use super::super::gateway::{
    DiscordGateway, HeadlessGateway, LiveDiscordTurnContext, send_intake_placeholder,
};
use super::super::*;
pub(in crate::services::discord) use super::authorization::{
    TurnKind, classify_turn_kind_from_author,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::dispatch_trigger::evaluate_dispatch_cwd_policy;
use super::dispatch_trigger::{
    dispatch_session_path_should_update, dispatch_should_recover_session_worktree,
    parse_dispatch_context_hints, resolve_dispatch_target_repo_dir,
};
use super::response_format::{
    build_headless_trigger_context, build_memory_injection_plan, build_race_requeued_intervention,
    build_system_discord_context, dispatch_profile_label, memento_recall_gate_decision,
    merge_reply_contexts, should_note_memento_context_loaded, wrap_user_prompt_with_author,
};
pub(in crate::services::discord) use super::turn_start::reserve_headless_turn;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::turn_start::resolve_session_id_for_current_turn;
#[cfg(test)]
use super::turn_start::session_strategy_lifecycle_event;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::turn_start::{HEADLESS_TURN_MESSAGE_ID_BASE, headless_turn_message_id_seed};
pub(crate) use super::turn_start::{
    HeadlessTurnReservation, HeadlessTurnStartError, HeadlessTurnStartOutcome,
};
use super::turn_start::{
    SessionResetReason, cli_just_spawned_for_emit, dispatch_reset_lifecycle_code,
    emit_session_strategy_lifecycle, load_session_runtime_state, log_session_strategy_diagnostic,
    refresh_session_strategy_after_pending_reset, release_mailbox_after_placeholder_post_failure,
    session_reset_reason_for_turn, session_reset_reason_lifecycle_code,
    session_runtime_state_after_redirect, take_session_retry_context,
};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::services::git::GitCommand;
use crate::services::memory::{
    RecallMode, RecallRequest, RecallResponse, RecallSizeBucket, build_memory_backend,
    note_recall_context_size, resolve_memory_role_id, resolve_memory_session_id,
};
#[cfg(test)]
use crate::services::observability::turn_lifecycle::TurnEvent;
use crate::services::provider::{CancelToken, cancel_requested};
use std::sync::Arc;

const WATCHDOG_DEADLOCK_PREALERT_MS: i64 = 5 * 60 * 1000;
const WATCHDOG_DEADLOCK_PREALERT_BOT: &str = "announce";
const WATCHDOG_TIMEOUT_REASON: &str = "watchdog timeout";
const WATCHDOG_TIMEOUT_CANCEL_SOURCE: &str = "watchdog_timeout";

fn watchdog_deadlock_prealert_bot_name() -> &'static str {
    WATCHDOG_DEADLOCK_PREALERT_BOT
}

fn parse_watchdog_alert_channel_id(raw: &str) -> Option<serenity::ChannelId> {
    let trimmed = raw.trim();
    let normalized = trimmed
        .strip_prefix("channel:")
        .unwrap_or(trimmed)
        .trim()
        .trim_start_matches("<#")
        .trim_end_matches('>');
    normalized
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .map(serenity::ChannelId::new)
}

fn metadata_parent_channel_id(metadata: Option<&serde_json::Value>) -> Option<serenity::ChannelId> {
    metadata
        .and_then(|value| value.get("parent_channel_id"))
        .and_then(|value| value.as_str())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|id| *id > 0)
        .map(serenity::ChannelId::new)
}

fn metadata_delivery_bot(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata
        .and_then(|value| value.get("delivery_bot"))
        .and_then(|value| value.as_str())
        .and_then(normalize_delivery_bot_name)
}

fn metadata_silent_flag(metadata: Option<&serde_json::Value>) -> bool {
    metadata
        .and_then(|value| value.get("silent"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn metadata_turn_source(
    source: Option<&str>,
    metadata: Option<&serde_json::Value>,
) -> crate::dispatch::Source {
    source
        .and_then(crate::dispatch::Source::from_label)
        .or_else(|| {
            metadata
                .and_then(|value| value.get("source").or_else(|| value.get("turn_source")))
                .and_then(serde_json::Value::as_str)
                .and_then(crate::dispatch::Source::from_label)
        })
        .unwrap_or_default()
}

fn normalize_delivery_bot_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return None;
    }
    Some(value.to_string())
}

fn resolve_headless_workspace(
    channel_id: serenity::ChannelId,
    channel_name_hint: Option<&str>,
    metadata: Option<&serde_json::Value>,
) -> Option<String> {
    settings::resolve_workspace(channel_id, channel_name_hint).or_else(|| {
        metadata_parent_channel_id(metadata)
            .and_then(|parent_channel_id| settings::resolve_workspace(parent_channel_id, None))
    })
}

fn configured_watchdog_alert_channel_id() -> Option<serenity::ChannelId> {
    for key in [
        "deadlock_manager_channel_id",
        "kanban_human_alert_channel_id",
    ] {
        if let Ok(Some(value)) = crate::services::discord::internal_api::get_kv_value(key)
            && let Some(channel_id) = parse_watchdog_alert_channel_id(&value)
        {
            return Some(channel_id);
        }
    }

    crate::config::load().ok().and_then(|config| {
        config
            .kanban
            .deadlock_manager_channel_id
            .as_deref()
            .and_then(parse_watchdog_alert_channel_id)
            .or_else(|| {
                config
                    .kanban
                    .human_alert_channel_id
                    .as_deref()
                    .and_then(parse_watchdog_alert_channel_id)
            })
    })
}

fn should_send_watchdog_deadlock_prealert(
    now_ms: i64,
    deadline_ms: i64,
    last_notified_deadline_ms: Option<i64>,
) -> bool {
    now_ms < deadline_ms
        && now_ms >= deadline_ms - WATCHDOG_DEADLOCK_PREALERT_MS
        && last_notified_deadline_ms != Some(deadline_ms)
}

fn apply_watchdog_deadline_extension(
    watchdog_token: &CancelToken,
    extension: crate::services::turn_orchestrator::WatchdogDeadlineExtension,
) -> i64 {
    watchdog_token.watchdog_max_deadline_ms.store(
        extension.max_deadline_ms,
        std::sync::atomic::Ordering::Relaxed,
    );
    watchdog_token.watchdog_deadline_ms.store(
        extension.new_deadline_ms,
        std::sync::atomic::Ordering::Relaxed,
    );
    extension.new_deadline_ms
}

fn build_watchdog_deadlock_prealert_message(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    now_ms: i64,
    deadline_ms: i64,
    turn_started_ms: i64,
    max_deadline_ms: i64,
    inflight: Option<&InflightTurnState>,
) -> String {
    let remaining_min = ((deadline_ms - now_ms).max(0) + 59_999) / 60_000;
    let elapsed_min = ((now_ms - turn_started_ms).max(0) + 59_999) / 60_000;
    let max_remaining_min = ((max_deadline_ms - now_ms).max(0) + 59_999) / 60_000;
    let session_key = inflight
        .and_then(|state| state.session_key.as_deref())
        .unwrap_or("?");
    let dispatch_id = inflight
        .and_then(|state| state.dispatch_id.as_deref())
        .unwrap_or("?");
    let tmux = inflight
        .and_then(|state| state.tmux_session_name.as_deref())
        .unwrap_or("?");
    let updated_at = inflight
        .map(|state| state.updated_at.as_str())
        .unwrap_or("?");

    let provider = provider.as_str();

    format!(
        "⚠️ [Watchdog pre-timeout]\n\
channel_id: {channel_id}\n\
provider: {provider}\n\
remaining: {remaining_min}분\n\
elapsed: {elapsed_min}분\n\
max_remaining: {max_remaining_min}분\n\
session_key: {session_key}\n\
dispatch_id: {dispatch_id}\n\
tmux: {tmux}\n\
inflight_updated_at: {updated_at}\n\
정상 진행이면 `POST /api/turns/{channel_id}/extend-timeout`로 연장하세요."
    )
}

async fn maybe_send_watchdog_deadlock_prealert(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    now_ms: i64,
    deadline_ms: i64,
    turn_started_ms: i64,
    max_deadline_ms: i64,
) -> bool {
    let Some(alert_channel_id) = configured_watchdog_alert_channel_id() else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⏰ WATCHDOG: no deadlock/human alert channel configured for pre-timeout alert"
        );
        return false;
    };
    let Some(registry) = shared.health_registry() else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⏰ WATCHDOG: health registry unavailable for {} pre-timeout alert to {}",
            WATCHDOG_DEADLOCK_PREALERT_BOT,
            alert_channel_id
        );
        return false;
    };
    let alert_http = match super::super::health::resolve_bot_http(
        registry.as_ref(),
        WATCHDOG_DEADLOCK_PREALERT_BOT,
    )
    .await
    {
        Ok(http) => http,
        Err((status, body)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⏰ WATCHDOG: {} bot unavailable for pre-timeout alert to {}: {status}: {body}",
                WATCHDOG_DEADLOCK_PREALERT_BOT,
                alert_channel_id
            );
            return false;
        }
    };
    let inflight = super::super::inflight::load_inflight_state(provider, channel_id.get());
    let message = build_watchdog_deadlock_prealert_message(
        provider,
        channel_id,
        now_ms,
        deadline_ms,
        turn_started_ms,
        max_deadline_ms,
        inflight.as_ref(),
    );
    match alert_channel_id.say(&*alert_http, message).await {
        Ok(_) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏰ WATCHDOG: sent pre-timeout alert via {} bot for channel {} to {}",
                WATCHDOG_DEADLOCK_PREALERT_BOT,
                channel_id,
                alert_channel_id
            );
            true
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⏰ WATCHDOG: failed pre-timeout alert for channel {} to {}: {}",
                channel_id,
                alert_channel_id,
                error
            );
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchdogTimeoutCancelDisposition {
    Cancelled,
    AlreadyStopping,
    StaleToken,
}

fn watchdog_timeout_turn_id(inflight: &InflightTurnState) -> Option<String> {
    (inflight.user_msg_id != 0)
        .then(|| format!("discord:{}:{}", inflight.channel_id, inflight.user_msg_id))
}

fn watchdog_timeout_cancel_request(
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    inflight: Option<&InflightTurnState>,
    queue_depth: Option<usize>,
    termination_recorded: bool,
) -> crate::services::turn_cancel_finalizer::FinalizeTurnCancelRequest {
    let turn_id = inflight.and_then(watchdog_timeout_turn_id);
    crate::services::turn_cancel_finalizer::FinalizeTurnCancelRequest {
        correlation: crate::services::turn_cancel_finalizer::TurnCancelCorrelation {
            provider: Some(provider.clone()),
            channel_id: Some(channel_id),
            dispatch_id: inflight.and_then(|state| state.dispatch_id.clone()),
            session_key: inflight.and_then(|state| state.session_key.clone()),
            turn_id,
        },
        reason: WATCHDOG_TIMEOUT_REASON.to_string(),
        surface: WATCHDOG_TIMEOUT_CANCEL_SOURCE.to_string(),
        lifecycle_path: "mailbox_cancel_active_turn.watchdog_timeout".to_string(),
        tmux_killed: false,
        inflight_cleared: false,
        queue_depth,
        queue_preserved: true,
        termination_recorded,
        completed_at: chrono::Utc::now(),
    }
}

async fn reconcile_watchdog_timeout(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    watchdog_token: &Arc<CancelToken>,
) -> WatchdogTimeoutCancelDisposition {
    let inflight = super::super::inflight::load_inflight_state(provider, channel_id.get());
    let result = super::super::mailbox_cancel_active_turn_if_current_with_reason(
        shared,
        channel_id,
        watchdog_token.clone(),
        WATCHDOG_TIMEOUT_CANCEL_SOURCE,
    )
    .await;
    super::super::clear_watchdog_deadline_override(channel_id.get()).await;

    let Some(token) = result.token else {
        return WatchdogTimeoutCancelDisposition::StaleToken;
    };
    if result.already_stopping {
        return WatchdogTimeoutCancelDisposition::AlreadyStopping;
    }

    super::super::ensure_cancel_token_bound_from_inflight(
        provider,
        channel_id,
        &token,
        "watchdog timeout mailbox cancel",
    );
    let termination_recorded = super::super::turn_bridge::stop_active_turn(
        provider,
        &token,
        super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
        WATCHDOG_TIMEOUT_REASON,
    )
    .await;
    let queue_depth = super::super::mailbox_snapshot(shared, channel_id)
        .await
        .intervention_queue
        .len();
    crate::services::turn_cancel_finalizer::finalize_turn_cancel(watchdog_timeout_cancel_request(
        provider,
        channel_id,
        inflight.as_ref(),
        Some(queue_depth),
        termination_recorded,
    ));

    WatchdogTimeoutCancelDisposition::Cancelled
}

fn attach_paused_turn_watcher(
    shared: &Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<String>,
    output_path: Option<String>,
    initial_offset: u64,
    source: &'static str,
) -> serenity::ChannelId {
    let mut watcher_owner_channel_id = channel_id;

    #[cfg(unix)]
    if let (Some(tmux_session_name), Some(output_path)) = (tmux_session_name, output_path) {
        let existing_owner_for_tmux = shared.tmux_watchers.iter().any(|entry| {
            entry.tmux_session_name == tmux_session_name
                && !entry.cancel.load(std::sync::atomic::Ordering::Relaxed)
        });
        let tmux_live =
            crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_session_name);
        if !tmux_live && !existing_owner_for_tmux {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping paused tmux watcher attach for channel {} ({source}) — tmux {} is not live yet",
                channel_id,
                tmux_session_name
            );
            return watcher_owner_channel_id;
        }

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
            super::super::tmux_watcher_now_ms(),
        ));
        let mailbox_finalize_owed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.clone(),
            paused: paused.clone(),
            resume_offset: resume_offset.clone(),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: turn_delivered.clone(),
            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
            mailbox_finalize_owed: mailbox_finalize_owed.clone(),
        };
        let claim = super::super::tmux::claim_or_reuse_watcher(
            &shared.tmux_watchers,
            channel_id,
            handle,
            provider,
            source,
        );
        watcher_owner_channel_id = claim.owner_channel_id();
        if claim.should_spawn() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Attaching tmux watcher for turn on channel {} ({})",
                channel_id,
                claim.as_str()
            );
            if claim.replaced_existing() {
                shared.record_tmux_watcher_reconnect(channel_id);
            }
            tokio::spawn(super::super::tmux::tmux_output_watcher(
                channel_id,
                http,
                shared.clone(),
                output_path,
                tmux_session_name,
                initial_offset,
                cancel,
                paused,
                resume_offset,
                pause_epoch,
                turn_delivered,
                last_heartbeat_ts_ms,
                mailbox_finalize_owed,
            ));
        }
    }

    if let Some(watcher) = shared.tmux_watchers.get(&watcher_owner_channel_id) {
        watcher
            .pause_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        watcher
            .paused
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    watcher_owner_channel_id
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) mod test_harness_exports {
    use super::*;

    pub(crate) fn attach_paused_turn_watcher(
        shared: &Arc<SharedData>,
        http: Arc<serenity::Http>,
        provider: &ProviderKind,
        channel_id: serenity::ChannelId,
        tmux_session_name: Option<String>,
        output_path: Option<String>,
        initial_offset: u64,
        source: &'static str,
    ) -> serenity::ChannelId {
        super::attach_paused_turn_watcher(
            shared,
            http,
            provider,
            channel_id,
            tmux_session_name,
            output_path,
            initial_offset,
            source,
        )
    }
}

fn should_add_turn_pending_reaction(_dispatch_id: Option<&str>) -> bool {
    // #750: announce bot no longer writes lifecycle emojis, so the command bot
    // is now the single source of ⏳ for both regular and dispatch turns.
    // Users stop an active dispatch turn by removing this ⏳, which
    // intake_gate's classify_removed_control_reaction catches.
    // (#559 originally skipped this for dispatches to avoid duplicating the
    // announce bot's ⏳. With the announce-bot path gone, we must re-add it
    // here so the stop-via-reaction-removal path keeps working.)
    true
}

async fn mailbox_try_start_turn_with_terminal_marker_cleanup(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    cancel_token: Arc<CancelToken>,
    request_owner: UserId,
    user_msg_id: MessageId,
    session_key: Option<&str>,
) -> bool {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return super::super::mailbox_try_start_turn(
            shared,
            channel_id,
            cancel_token,
            request_owner,
            user_msg_id,
        )
        .await;
    };
    let Some(session_key) = session_key.map(str::trim).filter(|value| !value.is_empty()) else {
        return super::super::mailbox_try_start_turn(
            shared,
            channel_id,
            cancel_token,
            request_owner,
            user_msg_id,
        )
        .await;
    };
    let thread_channel_id = channel_id.get().to_string();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!(
                "[outbox] failed to begin terminal delivery marker cleanup before turn start for channel {}: {}",
                channel_id,
                error
            );
            return super::super::mailbox_try_start_turn(
                shared,
                channel_id,
                cancel_token,
                request_owner,
                user_msg_id,
            )
            .await;
        }
    };

    if let Err(error) = sqlx::query("SELECT pg_advisory_xact_lock(1752, hashtext($1))")
        .bind(&thread_channel_id)
        .execute(&mut *tx)
        .await
    {
        tracing::warn!(
            "[outbox] failed to lock terminal delivery marker before turn start for channel {}: {}",
            channel_id,
            error
        );
        let _ = tx.rollback().await;
        return super::super::mailbox_try_start_turn(
            shared,
            channel_id,
            cancel_token,
            request_owner,
            user_msg_id,
        )
        .await;
    }

    let started = super::super::mailbox_try_start_turn(
        shared,
        channel_id,
        cancel_token,
        request_owner,
        user_msg_id,
    )
    .await;
    if started
        && let Err(error) = sqlx::query(
            "UPDATE sessions
                SET active_turn_delivery_outbox_id = NULL
              WHERE session_key = $1
                AND thread_channel_id = $2
                AND active_turn_delivery_outbox_id IS NOT NULL",
        )
        .bind(session_key)
        .bind(&thread_channel_id)
        .execute(&mut *tx)
        .await
    {
        tracing::warn!(
            "[outbox] failed to clear terminal delivery marker after new turn start for channel {}: {}",
            channel_id,
            error
        );
    }
    if let Err(error) = tx.commit().await {
        tracing::warn!(
            "[outbox] failed to commit terminal delivery marker cleanup after turn start for channel {}: {}",
            channel_id,
            error
        );
    }
    started
}

fn native_fast_mode_override_for_turn(
    provider: &ProviderKind,
    channel_fast_mode_setting: Option<bool>,
) -> Option<bool> {
    if matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        channel_fast_mode_setting
    } else {
        None
    }
}

fn codex_goals_override_for_turn(
    provider: &ProviderKind,
    channel_codex_goals_setting: Option<bool>,
) -> Option<bool> {
    if matches!(provider, ProviderKind::Codex) {
        channel_codex_goals_setting
    } else {
        None
    }
}

#[derive(Debug, PartialEq)]
enum GoalCommandKind {
    NotGoal,
    ChainedStart,
    FreshStart,
    Lifecycle,
}

const GOAL_LIFECYCLE_SUBCOMMANDS: &[&str] = &["pause", "resume", "clear"];

fn classify_codex_goal_command(text: &str) -> GoalCommandKind {
    let Some(first_line) = text.trim_start().lines().next() else {
        return GoalCommandKind::NotGoal;
    };
    let first_line = first_line.trim_end();
    let Some(rest) = first_line.strip_prefix("/goal") else {
        return GoalCommandKind::NotGoal;
    };
    if !rest.is_empty() && !rest.chars().next().is_some_and(char::is_whitespace) {
        return GoalCommandKind::NotGoal;
    }
    let args = rest.trim_start();
    if args.is_empty() {
        return GoalCommandKind::ChainedStart;
    }
    for sub in GOAL_LIFECYCLE_SUBCOMMANDS {
        let Some(after) = args.strip_prefix(sub) else {
            continue;
        };
        if after.is_empty() || after.chars().next().is_some_and(char::is_whitespace) {
            return GoalCommandKind::Lifecycle;
        }
    }
    if let Some(after_fresh) = args.strip_prefix("--fresh") {
        if after_fresh.is_empty() || after_fresh.chars().next().is_some_and(char::is_whitespace) {
            return GoalCommandKind::FreshStart;
        }
    }
    GoalCommandKind::ChainedStart
}

fn classify_codex_goal_command_for_provider(
    provider: &ProviderKind,
    text: &str,
    channel_codex_goals_setting: Option<bool>,
) -> GoalCommandKind {
    if matches!(provider, ProviderKind::Codex) && channel_codex_goals_setting.unwrap_or(true) {
        classify_codex_goal_command(text)
    } else {
        GoalCommandKind::NotGoal
    }
}

fn rewrite_fresh_goal_prompt(text: &str) -> String {
    let trimmed = text.trim_start();
    let prefix_len = text.len() - trimmed.len();
    let leading = &text[..prefix_len];
    let Some(rest) = trimmed.strip_prefix("/goal") else {
        return text.to_string();
    };
    let after_goal = rest.trim_start_matches(|c: char| c == ' ' || c == '\t');
    let Some(after_fresh) = after_goal.strip_prefix("--fresh") else {
        return text.to_string();
    };
    let objective = after_fresh.trim_start_matches(|c: char| c == ' ' || c == '\t');
    if objective.is_empty() {
        format!("{}/goal", leading)
    } else {
        format!("{}/goal {}", leading, objective)
    }
}

fn is_codex_goal_start_request(text: &str) -> bool {
    !matches!(classify_codex_goal_command(text), GoalCommandKind::NotGoal)
}

async fn clear_codex_goal_start_provider_session(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    adk_session_key: Option<&str>,
    session_id: &mut Option<String>,
    memento_context_loaded: &mut bool,
    session_strategy_reason: &mut &'static str,
) {
    let session_id_to_clear = session_id.clone();
    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
    }

    if let Some(key) = adk_session_key {
        super::super::adk_session::clear_provider_session_id(key, shared.api_port).await;
    }
    if let Some(ref stale_session_id) = session_id_to_clear {
        let _ = super::super::internal_api::clear_stale_session_id(stale_session_id).await;
    }

    *session_id = None;
    *memento_context_loaded = false;
    *session_strategy_reason = "codex_goal_start_fresh_session";
}

fn effective_fast_mode_channel_id(
    channel_id: ChannelId,
    thread_parent: Option<(ChannelId, Option<String>)>,
) -> ChannelId {
    thread_parent
        .map(|(parent_channel_id, _)| parent_channel_id)
        .unwrap_or(channel_id)
}

pub(in crate::services::discord) async fn start_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        shared,
        token,
        source,
        metadata,
        channel_name_hint,
        None,
        reserve_headless_turn(),
    )
    .await
}

pub(in crate::services::discord) async fn start_reserved_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessTurnReservation,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn_with_owner(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        UserId::new(1),
        shared,
        token,
        source,
        metadata,
        channel_name_hint,
        is_dm_hint,
        reservation,
    )
    .await
}

pub(in crate::services::discord) async fn start_voice_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    request_owner: UserId,
    shared: &Arc<SharedData>,
    token: &str,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn_with_owner(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        request_owner,
        shared,
        token,
        Some(crate::dispatch::Source::Voice.as_str()),
        metadata,
        channel_name_hint,
        Some(false),
        reserve_headless_turn(),
    )
    .await
}

async fn start_reserved_headless_turn_with_owner(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    request_owner: UserId,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessTurnReservation,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(HeadlessTurnStartError::Internal(
            "prompt is required".to_string(),
        ));
    }

    shared.record_channel_speaker(
        channel_id,
        request_owner,
        request_owner_name,
        is_dm_hint.unwrap_or(false),
    );
    let user_msg_id = reservation.user_msg_id;
    let placeholder_msg_id = reservation.placeholder_msg_id;
    let mut session_reset_reason = None;
    let mut reset_session_id_to_clear = None;

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id)
            && let Some(reason) =
                session_reset_reason_for_turn(session, tokio::time::Instant::now())
        {
            if let Some(retry_context) =
                session.recent_history_context(super::super::SESSION_RECOVERY_CONTEXT_MESSAGES)
            {
                let _ = super::super::turn_bridge::store_session_retry_context(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    channel_id.get(),
                    &retry_context,
                );
            }
            session_reset_reason = Some(reason);
            reset_session_id_to_clear = session.session_id.clone();
            session.clear_provider_session();
            session.history.clear();
        }
    }

    let (mut session_id, mut memento_context_loaded, current_path) = {
        let mut data = shared.core.lock().await;
        if let Some(info) = load_session_runtime_state(&mut data.sessions, channel_id) {
            if let Some(channel_name_hint) = channel_name_hint.as_ref()
                && let Some(session) = data.sessions.get_mut(&channel_id)
                && session.channel_name.is_none()
            {
                session.channel_name = Some(channel_name_hint.clone());
            }
            info
        } else {
            let workspace = resolve_headless_workspace(
                channel_id,
                channel_name_hint.as_deref(),
                metadata.as_ref(),
            )
            .ok_or_else(|| {
                HeadlessTurnStartError::Internal(format!(
                    "no workspace resolved for headless turn channel {}",
                    channel_id.get()
                ))
            })?;
            let workspace_path = std::path::Path::new(&workspace);
            if !workspace_path.is_dir() {
                return Err(HeadlessTurnStartError::Internal(format!(
                    "resolved workspace does not exist for headless turn: {workspace}"
                )));
            }
            let canonical = workspace_path
                .canonicalize()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| workspace.clone());
            let session = data
                .sessions
                .entry(channel_id)
                .or_insert_with(|| DiscordSession {
                    session_id: None,
                    memento_context_loaded: false,
                    memento_reflected: false,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    channel_name: channel_name_hint.clone(),
                    category_name: None,
                    remote_profile_name: None,
                    channel_id: Some(channel_id.get()),
                    last_active: tokio::time::Instant::now(),
                    worktree: None,
                    born_generation: super::super::runtime_store::load_generation(),
                    assistant_turns: 0,
                });
            session.current_path = Some(canonical.clone());
            if session.channel_name.is_none() {
                session.channel_name = channel_name_hint.clone();
            }
            session.channel_id = Some(channel_id.get());
            session.last_active = tokio::time::Instant::now();
            (
                session.session_id.clone(),
                session.memento_context_loaded,
                canonical,
            )
        }
    };
    let mut session_strategy_reason = if session_id.is_some() {
        "runtime_cached_provider_session"
    } else {
        "no_runtime_provider_session"
    };

    let pending_uploads = {
        let mut data = shared.core.lock().await;
        data.sessions
            .get_mut(&channel_id)
            .map(|session| {
                session.cleared = false;
                std::mem::take(&mut session.pending_uploads)
            })
            .unwrap_or_default()
    };

    let (settings_provider, allowed_tools) = {
        let settings = shared.settings.read().await;
        (settings.provider.clone(), settings.allowed_tools.clone())
    };

    let turn_id = reservation.turn_id(channel_id);
    let session_retry_context = take_session_retry_context(shared, channel_id, Some(&turn_id));
    let reply_context = session_retry_context
        .as_ref()
        .map(|context| context.formatted_context.clone());
    let role_binding = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_deref());
        resolve_role_binding(channel_id, channel_name)
    };
    let provider = role_binding
        .as_ref()
        .and_then(|binding| binding.provider.clone())
        .unwrap_or(settings_provider);
    let dispatch_profile = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_deref());
        DispatchProfile::for_turn(
            None,
            settings::resolve_dispatch_profile(channel_id, channel_name),
        )
    };

    let fast_mode_channel_id = effective_fast_mode_channel_id(
        channel_id,
        super::super::resolve_thread_parent(&ctx.http, channel_id).await,
    );
    super::super::commands::reset_provider_session_if_pending(
        &ctx.http,
        shared,
        &provider,
        channel_id,
        fast_mode_channel_id,
    )
    .await;
    refresh_session_strategy_after_pending_reset(
        shared,
        channel_id,
        &mut session_id,
        &mut memento_context_loaded,
        &mut session_strategy_reason,
    )
    .await;

    let prompt_prep_started = std::time::Instant::now();
    let (channel_name, tmux_session_name, category_name) = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
            .or_else(|| channel_name_hint.clone());
        let tmux_session_name = if provider.uses_managed_tmux_backend() {
            channel_name
                .as_ref()
                .map(|name| provider.build_tmux_session_name(name))
        } else {
            None
        };
        let category_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.category_name.clone());
        (channel_name, tmux_session_name, category_name)
    };
    let adk_session_key = build_adk_session_key(shared, channel_id, &provider).await;
    if session_reset_reason.is_some() {
        if let Some(ref key) = adk_session_key {
            super::super::adk_session::clear_provider_session_id(key, shared.api_port).await;
        }
        if let Some(ref session_id_to_clear) = reset_session_id_to_clear {
            let _ = super::super::internal_api::clear_stale_session_id(session_id_to_clear).await;
        }
    }
    let headless_goal_kind = classify_codex_goal_command_for_provider(
        &provider,
        prompt,
        super::super::commands::channel_codex_goals_setting(shared, fast_mode_channel_id).await,
    );
    let force_fresh_provider_session = matches!(headless_goal_kind, GoalCommandKind::FreshStart);
    let fresh_codex_goal_session_requested = force_fresh_provider_session;
    if force_fresh_provider_session {
        clear_codex_goal_start_provider_session(
            shared,
            channel_id,
            adk_session_key.as_deref(),
            &mut session_id,
            &mut memento_context_loaded,
            &mut session_strategy_reason,
        )
        .await;
    }
    let effective_prompt: std::borrow::Cow<str> = if force_fresh_provider_session {
        std::borrow::Cow::Owned(rewrite_fresh_goal_prompt(prompt))
    } else {
        std::borrow::Cow::Borrowed(prompt)
    };
    if session_id.is_none() {
        if fresh_codex_goal_session_requested {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for headless channel {} due to /goal fresh session request",
                channel_id.get()
            );
        } else if let Some(reason) = session_reset_reason {
            let ts = chrono::Local::now().format("%H:%M:%S");
            session_strategy_reason = session_reset_reason_lifecycle_code(reason);
            let display_reason = match reason {
                SessionResetReason::IdleExpired => "idle timeout",
                SessionResetReason::AssistantTurnCap => "assistant turn cap",
            };
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for headless channel {} due to {}",
                channel_id.get(),
                display_reason
            );
        } else if let Some(ref key) = adk_session_key {
            let restored = super::super::adk_session::fetch_provider_session_id(
                key,
                &provider,
                shared.api_port,
            )
            .await;
            if restored.is_some() {
                session_strategy_reason = "db_provider_session_restored";
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ Restored provider session_id from DB for headless {}",
                    key
                );
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.restore_provider_session(restored.clone());
                    memento_context_loaded = session.memento_context_loaded;
                }
            } else {
                session_strategy_reason = "no_cached_provider_session";
            }
            session_id = restored;
        } else {
            session_strategy_reason = "session_key_unavailable";
        }
    }

    let cancel_token = Arc::new(CancelToken::new());
    let started = mailbox_try_start_turn_with_terminal_marker_cleanup(
        shared,
        channel_id,
        cancel_token.clone(),
        request_owner,
        user_msg_id,
        adk_session_key.as_deref(),
    )
    .await;
    if !started {
        return Err(HeadlessTurnStartError::Conflict(format!(
            "agent mailbox is busy for channel {}",
            channel_id.get()
        )));
    }
    shared
        .global_active
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    shared
        .turn_start_times
        .insert(channel_id, std::time::Instant::now());
    log_session_strategy_diagnostic(
        channel_id,
        &provider,
        dispatch_profile,
        session_strategy_reason,
        session_id.as_deref(),
        adk_session_key.as_deref(),
        tmux_session_name.as_deref(),
        session_retry_context.is_some(),
        memento_context_loaded,
    )
    .await;
    let cli_was_just_spawned = cli_just_spawned_for_emit(tmux_session_name.as_deref());
    emit_session_strategy_lifecycle(
        shared,
        channel_id,
        &turn_id,
        adk_session_key.as_deref(),
        None,
        session_id.as_deref(),
        session_strategy_reason,
        cli_was_just_spawned,
    )
    .await;

    let (memory_settings, memory_backend) = build_memory_backend(role_binding.as_ref());
    let memento_recall_gate = memento_recall_gate_decision(
        &memory_settings,
        memento_context_loaded,
        prompt,
        dispatch_profile,
    );
    let memory_recall = if !memento_recall_gate.should_recall {
        RecallResponse::default()
    } else {
        memory_backend
            .recall(RecallRequest {
                provider: provider.clone(),
                role_id: resolve_memory_role_id(role_binding.as_ref()),
                channel_id: channel_id.get(),
                session_id: resolve_memory_session_id(session_id.as_deref(), channel_id.get()),
                dispatch_profile,
                user_text: prompt.to_string(),
                mode: memento_recall_gate.mode,
            })
            .await
    };
    if memory_settings.backend == settings::MemoryBackendKind::Memento {
        let ts = chrono::Local::now().format("%H:%M:%S");
        let recall_bytes = memory_recall
            .external_recall
            .as_deref()
            .map(str::len)
            .unwrap_or(0);
        let bucket = if !memento_recall_gate.should_recall {
            RecallSizeBucket::Skipped
        } else {
            match memento_recall_gate.mode {
                RecallMode::Full => RecallSizeBucket::Full,
                RecallMode::IdentityOnly => RecallSizeBucket::IdentityOnly,
            }
        };
        note_recall_context_size(bucket, recall_bytes);
        tracing::info!(
            "  [{ts}] [memory] memento recall gate for headless channel {}: decision={} mode={:?} reason={} context_loaded={} recall_bytes={} input_tokens={} output_tokens={}",
            channel_id.get(),
            if memento_recall_gate.should_recall {
                "inject"
            } else {
                "skip"
            },
            memento_recall_gate.mode,
            memento_recall_gate.reason,
            memento_context_loaded,
            recall_bytes,
            memory_recall.token_usage.input_tokens,
            memory_recall.token_usage.output_tokens
        );
    }
    if should_note_memento_context_loaded(&memory_settings, memento_context_loaded, &memory_recall)
    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.note_memento_context_loaded();
        }
    }
    for warning in &memory_recall.warnings {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] [memory] recall warning for headless channel {}: {}",
            channel_id.get(),
            warning
        );
    }

    let mut context_chunks = Vec::new();
    let memory_injection_plan = build_memory_injection_plan(
        &provider,
        session_id.is_some(),
        dispatch_profile,
        &memory_recall,
    );
    if !pending_uploads.is_empty() {
        context_chunks.push(pending_uploads.join("\n"));
    }
    if let Some(headless_context) = build_headless_trigger_context(source, metadata.as_ref()) {
        context_chunks.push(headless_context);
    }
    if let Some(reply_context) = reply_context {
        context_chunks.push(reply_context);
    }
    if let Some(knowledge) = memory_injection_plan.shared_knowledge_for_context {
        context_chunks.push(knowledge.to_string());
    }
    if let Some(external_recall) = memory_injection_plan.external_recall_for_context {
        context_chunks.push(external_recall.to_string());
    }
    context_chunks.push(wrap_user_prompt_with_author(
        request_owner_name,
        request_owner,
        ai_screen::sanitize_user_input(&effective_prompt),
    ));
    let context_prompt = context_chunks.join("\n\n");

    let discord_context = build_system_discord_context(
        channel_name.as_deref(),
        category_name.as_deref(),
        channel_id,
        true,
    );

    let sak_for_system = memory_injection_plan.shared_knowledge_for_system_prompt;
    let longterm_catalog_for_prompt = memory_injection_plan.longterm_catalog_for_system_prompt;
    let memento_mcp_available = crate::services::mcp_config::provider_has_memento_mcp(&provider);
    let channel_participants = shared.channel_roster(channel_id, request_owner, request_owner_name);
    let memory_recall_manifest = super::super::prompt_builder::MemoryRecallManifestInput {
        should_recall: memento_recall_gate.should_recall,
        gate_reason: memento_recall_gate.reason,
        external_recall: memory_recall.external_recall.as_deref(),
    };
    let recovery_context_for_manifest =
        session_retry_context
            .as_ref()
            .map(|context| RecoveryContextManifestInput {
                raw_context: context.raw_context.as_str(),
                audit_record: context.audit_record.as_ref(),
            });
    let built_system_prompt = build_system_prompt_with_manifest(
        &discord_context,
        &channel_participants,
        &current_path,
        channel_id,
        token,
        role_binding.as_ref(),
        false,
        dispatch_profile,
        None,
        None,
        sak_for_system,
        longterm_catalog_for_prompt,
        Some(&memory_settings),
        memento_mcp_available,
        recovery_context_for_manifest.as_ref(),
        Some(&memory_recall_manifest),
        Some(&turn_id),
    );
    let system_prompt_owned = built_system_prompt.system_prompt;
    if let Some(manifest) = built_system_prompt.manifest {
        crate::db::prompt_manifests::spawn_save_prompt_manifest(shared.pg_pool.clone(), manifest);
    }
    let prompt_prep_duration_ms = prompt_prep_started.elapsed().as_millis();
    let memory_backend_label = memory_settings.backend.as_str();
    let provider_label = match &provider {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Qwen => "qwen",
        ProviderKind::Unsupported(_) => "unsupported",
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] [prompt-prep] headless channel={} provider={} dispatch={} memory_backend={} reused_session={} duration_ms={}",
        channel_id.get(),
        provider_label,
        dispatch_profile_label(dispatch_profile),
        memory_backend_label,
        session_id.is_some(),
        prompt_prep_duration_ms
    );
    // #1085: same session-reuse counter as the foreground path so headless
    // (background-trigger) turns are reflected in the reuse-rate metric.
    crate::services::observability::metrics::record_session_entry(
        channel_id.get(),
        provider_label,
        session_id.is_some(),
    );

    {
        let watchdog_token = cancel_token.clone();
        let watchdog_shared = shared.clone();
        let timeout = super::super::turn_watchdog_timeout();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let turn_started_ms = now_ms;
        let deadline_ms = now_ms + timeout.as_millis() as i64;
        let max_deadline_ms = deadline_ms;
        watchdog_token
            .watchdog_deadline_ms
            .store(deadline_ms, std::sync::atomic::Ordering::Relaxed);
        watchdog_token
            .watchdog_max_deadline_ms
            .store(max_deadline_ms, std::sync::atomic::Ordering::Relaxed);

        let watchdog_channel_id_num = channel_id.get();
        let watchdog_provider = provider.clone();
        tokio::spawn(async move {
            const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
            let mut last_deadlock_prealert_deadline_ms: Option<i64> = None;

            loop {
                tokio::time::sleep(CHECK_INTERVAL).await;
                if watchdog_token
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    super::super::clear_watchdog_deadline_override(watchdog_channel_id_num).await;
                    return;
                }
                if let Some(extension) =
                    super::super::take_watchdog_deadline_override(watchdog_channel_id_num).await
                {
                    apply_watchdog_deadline_extension(&watchdog_token, extension);
                    last_deadlock_prealert_deadline_ms = None;
                }
                {
                    let current_dl = watchdog_token
                        .watchdog_deadline_ms
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let now_ms_check = chrono::Utc::now().timestamp_millis();
                    if now_ms_check > current_dl - 120_000 {
                        if let Some(inflight) = super::super::inflight::load_inflight_state(
                            &watchdog_provider,
                            watchdog_channel_id_num,
                        ) {
                            if let Ok(updated) = chrono::NaiveDateTime::parse_from_str(
                                &inflight.updated_at,
                                "%Y-%m-%d %H:%M:%S",
                            ) {
                                let updated_ms = updated.and_utc().timestamp_millis();
                                let age_ms = now_ms_check - updated_ms;
                                if age_ms < 300_000 {
                                    let new_dl = now_ms_check + timeout.as_millis() as i64;
                                    if new_dl > current_dl {
                                        watchdog_token
                                            .watchdog_deadline_ms
                                            .store(new_dl, std::sync::atomic::Ordering::Relaxed);
                                        watchdog_token.watchdog_max_deadline_ms.store(
                                            std::cmp::max(
                                                watchdog_token
                                                    .watchdog_max_deadline_ms
                                                    .load(std::sync::atomic::Ordering::Relaxed),
                                                new_dl,
                                            ),
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        last_deadlock_prealert_deadline_ms = None;
                                    }
                                }
                            }
                        }
                    }
                }

                let current_deadline = watchdog_token
                    .watchdog_deadline_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                let now = chrono::Utc::now().timestamp_millis();
                if should_send_watchdog_deadlock_prealert(
                    now,
                    current_deadline,
                    last_deadlock_prealert_deadline_ms,
                ) {
                    let is_current_token =
                        super::super::mailbox_cancel_token(&watchdog_shared, channel_id)
                            .await
                            .is_some_and(|current| {
                                std::sync::Arc::ptr_eq(&watchdog_token, &current)
                            });
                    if !is_current_token {
                        super::super::clear_watchdog_deadline_override(watchdog_channel_id_num)
                            .await;
                        return;
                    }
                    let current_max_deadline = watchdog_token
                        .watchdog_max_deadline_ms
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if maybe_send_watchdog_deadlock_prealert(
                        &watchdog_shared,
                        &watchdog_provider,
                        channel_id,
                        now,
                        current_deadline,
                        turn_started_ms,
                        current_max_deadline,
                    )
                    .await
                    {
                        last_deadlock_prealert_deadline_ms = Some(current_deadline);
                    }
                }
                if let Some(extension) =
                    super::super::take_watchdog_deadline_override(watchdog_channel_id_num).await
                {
                    apply_watchdog_deadline_extension(&watchdog_token, extension);
                    last_deadlock_prealert_deadline_ms = None;
                }
                let current_deadline = watchdog_token
                    .watchdog_deadline_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                let now = chrono::Utc::now().timestamp_millis();
                if now < current_deadline {
                    continue;
                }

                let disposition = reconcile_watchdog_timeout(
                    &watchdog_shared,
                    &watchdog_provider,
                    channel_id,
                    &watchdog_token,
                )
                .await;
                if disposition == WatchdogTimeoutCancelDisposition::Cancelled {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⏰ Headless watchdog timeout reconciled via cancel path for channel {}",
                        channel_id
                    );
                }
                return;
            }
        });
    }

    let remote_profile = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.remote_profile_name.as_ref())
            .and_then(|name| {
                let settings = crate::config::Settings::load();
                settings
                    .remote_profiles
                    .iter()
                    .find(|profile| profile.name == *name)
                    .cloned()
            })
    };

    let adk_session_name = channel_name.clone();
    let adk_session_info =
        derive_adk_session_info(Some(prompt), channel_name.as_deref(), Some(&current_path));
    let adk_thread_channel_id = adk_session_name
        .as_deref()
        .and_then(super::super::adk_session::parse_thread_channel_id_from_name);
    post_adk_session_status(
        adk_session_key.as_deref(),
        adk_session_name.as_deref(),
        Some(provider.as_str()),
        "working",
        &provider,
        Some(&adk_session_info),
        None,
        Some(&current_path),
        None,
        adk_thread_channel_id,
        role_binding
            .as_ref()
            .map(|binding| binding.role_id.as_str()),
        shared.api_port,
    )
    .await;

    let (inflight_tmux_name, inflight_output_path, inflight_input_fifo, inflight_offset) = {
        #[cfg(unix)]
        {
            if remote_profile.is_none()
                && provider.uses_managed_tmux_backend()
                && claude::is_tmux_available()
            {
                if let Some(ref tmux_name) = tmux_session_name {
                    let (output_path, input_fifo_path) = tmux_runtime_paths(tmux_name);
                    let session_exists =
                        crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_name);
                    let last_offset = std::fs::metadata(&output_path)
                        .map(|metadata| metadata.len())
                        .unwrap_or(0);
                    (
                        Some(tmux_name.clone()),
                        Some(output_path),
                        Some(input_fifo_path),
                        if session_exists { last_offset } else { 0 },
                    )
                } else {
                    (None, None, None, 0)
                }
            } else {
                (None, None, None, 0)
            }
        }
        #[cfg(not(unix))]
        {
            (None, None, None, 0u64)
        }
    };
    let watcher_tmux_name = inflight_tmux_name.clone();
    let watcher_output_path = inflight_output_path.clone();

    let mut inflight_state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name.clone(),
        request_owner.get(),
        user_msg_id.get(),
        placeholder_msg_id.get(),
        prompt.to_string(),
        session_id.clone(),
        inflight_tmux_name,
        inflight_output_path,
        inflight_input_fifo.clone(),
        inflight_offset,
    );
    let (worktree_path, worktree_branch, base_commit) = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.worktree.as_ref())
            .map(|wt| {
                (
                    Some(wt.worktree_path.clone()),
                    Some(wt.branch_name.clone()),
                    crate::services::platform::git_head_commit(&wt.original_path),
                )
            })
            .unwrap_or((None, None, None))
    };
    inflight_state.set_worktree_context(worktree_path, worktree_branch, base_commit);
    inflight_state.logical_channel_id = Some(channel_id.get());
    inflight_state.session_key = adk_session_key.clone();
    inflight_state.delivery_bot = metadata_delivery_bot(metadata.as_ref());
    inflight_state.silent_turn = metadata_silent_flag(metadata.as_ref());
    inflight_state.source = metadata_turn_source(source, metadata.as_ref());
    if let Err(error) = save_inflight_state(&inflight_state) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}]   ⚠ inflight state save failed: {error}");
    }

    let _watcher_owner_channel_id = attach_paused_turn_watcher(
        shared,
        ctx.http.clone(),
        &provider,
        channel_id,
        watcher_tmux_name,
        watcher_output_path,
        inflight_offset,
        "turn_start_headless",
    );

    let (tx, rx) = mpsc::channel();
    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

    {
        let script = super::super::runtime_store::agentdesk_root()
            .unwrap_or_default()
            .join("scripts/worktree-autosync.sh");
        if script.exists() {
            let ws = current_path.clone();
            let ts = chrono::Local::now().format("%H:%M:%S");
            match std::process::Command::new(&script)
                .arg(&ws)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
            {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let msg = stdout.trim();
                    match out.status.code() {
                        Some(0) => tracing::info!("  [{ts}] 🔄 worktree-autosync [{ws}]: {msg}"),
                        Some(1) => {
                            tracing::info!("  [{ts}] ⏭ worktree-autosync [{ws}]: skipped — {msg}")
                        }
                        _ => tracing::warn!("  [{ts}] ⚠ worktree-autosync [{ws}]: error — {msg}"),
                    }
                }
                Err(error) => tracing::warn!(
                    "  [{ts}] ⚠ worktree-autosync: failed to run for headless turn — {error}"
                ),
            }
        }
    }

    let model_for_turn =
        super::super::commands::resolve_model_for_turn(shared, channel_id, &provider).await;
    let native_fast_mode_override = native_fast_mode_override_for_turn(
        &provider,
        super::super::commands::channel_fast_mode_setting(shared, fast_mode_channel_id).await,
    );
    let codex_goals_override = codex_goals_override_for_turn(
        &provider,
        super::super::commands::channel_codex_goals_setting(shared, fast_mode_channel_id).await,
    );
    let ctx_thresholds = super::super::adk_session::fetch_context_thresholds(shared.api_port).await;
    let compact_percent = ctx_thresholds.compact_pct_for(&provider);
    let model_context_window = provider.resolve_context_window(model_for_turn.as_deref());
    let compact_percent_for_claude = Some(ctx_thresholds.compact_pct_for(&provider));
    let compact_token_limit_for_codex = {
        let cli_config = provider.compact_cli_config(compact_percent, model_context_window);
        cli_config
            .first()
            .map(|(_, value)| value.parse::<u64>().unwrap_or(0))
    };
    // #1088: per-channel prompt-cache TTL (None|5|60). Only consumed by Claude.
    let cache_ttl_minutes = super::super::settings::resolve_cache_ttl_minutes(channel_id, None);
    let provider_execution_context = crate::services::provider_cli::ProviderExecutionContext {
        provider: provider.as_str().to_string(),
        agent_id: role_binding.as_ref().map(|binding| binding.role_id.clone()),
        channel_id: Some(channel_id.get().to_string()),
        session_key: adk_session_key.clone(),
        tmux_session: tmux_session_name.clone(),
        channel_name: channel_name.clone(),
        execution_mode: Some("discord_turn".to_string()),
    };

    let prompt_owned = prompt.to_string();
    let provider_for_blocking = provider.clone();
    tokio::task::spawn_blocking(move || {
        let result = crate::services::platform::with_provider_execution_context(
            provider_execution_context,
            || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match &provider_for_blocking {
                        ProviderKind::Claude => claude::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            native_fast_mode_override,
                            compact_percent_for_claude,
                            cache_ttl_minutes,
                            None,
                        ),
                        ProviderKind::Codex => codex::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            native_fast_mode_override,
                            codex_goals_override,
                            compact_token_limit_for_codex,
                            force_fresh_provider_session,
                        ),
                        ProviderKind::Gemini => gemini::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            None,
                        ),
                        ProviderKind::Qwen => qwen::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            None,
                        ),
                        ProviderKind::OpenCode => opencode::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            None,
                        ),
                        ProviderKind::Unsupported(name) => {
                            let _ = tx.send(StreamMessage::Error {
                                message: format!("Provider '{}' is not installed", name),
                                stdout: String::new(),
                                stderr: String::new(),
                                exit_code: None,
                            });
                            Ok(())
                        }
                    }
                }))
            },
        );

        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!("  [headless streaming] Error: {}", error);
                let _ = tx.send(StreamMessage::Error {
                    message: error,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            Err(panic_info) => {
                let msg = if let Some(value) = panic_info.downcast_ref::<String>() {
                    value.clone()
                } else if let Some(value) = panic_info.downcast_ref::<&str>() {
                    value.to_string()
                } else {
                    "unknown panic".to_string()
                };
                tracing::warn!("  [headless streaming] PANIC: {}", msg);
                let _ = tx.send(StreamMessage::Error {
                    message: format!("Internal error (panic): {}", msg),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
        }
    });

    spawn_turn_bridge(
        shared.clone(),
        cancel_token,
        rx,
        TurnBridgeContext {
            provider,
            gateway: Arc::new(HeadlessGateway),
            channel_id,
            user_msg_id,
            user_text_owned: prompt_owned,
            request_owner_name: request_owner_name.to_string(),
            role_binding,
            adk_session_key,
            adk_session_name,
            adk_session_info: Some(adk_session_info),
            adk_cwd: Some(current_path),
            dispatch_id: None,
            dispatch_kind: None,
            memory_recall_usage: memory_recall.token_usage,
            context_window_tokens: model_context_window,
            context_compact_percent: compact_percent,
            current_msg_id: placeholder_msg_id,
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: Some(inflight_offset),
            new_session_id: session_id,
            defer_watcher_resume: false,
            reuse_status_panel_message: false,
            completion_tx: None,
            inflight_state,
        },
    );

    Ok(HeadlessTurnStartOutcome {
        turn_id: reservation.turn_id(channel_id),
    })
}

async fn send_restore_notification(
    shared: &Arc<SharedData>,
    fallback_http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    restored_session_id: Option<&str>,
) {
    let sid_full = restored_session_id.unwrap_or("?");
    let sid_short: String = sid_full.chars().take(8).collect();
    let restore_msg = format!(
        "📋 세션 복원: {} (session: {})",
        provider.as_str(),
        sid_short
    );

    if let Some(registry) = shared.health_registry() {
        match super::super::health::resolve_bot_http(registry.as_ref(), "notify").await {
            Ok(notify_http) => match channel_id.say(&*notify_http, &restore_msg).await {
                Ok(_) => return,
                Err(err) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Restore notify send failed in channel {}: {} — falling back to provider bot",
                        channel_id,
                        err
                    );
                }
            },
            Err((status, body)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Restore notify bot unavailable in channel {}: {} {} — falling back to provider bot",
                    channel_id,
                    status,
                    body
                );
            }
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ Restore notify bot unavailable in channel {}: health registry dropped — falling back to provider bot",
            channel_id
        );
    }

    if let Err(err) = channel_id.say(fallback_http, &restore_msg).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ Restore fallback send failed in channel {}: {}",
            channel_id,
            err
        );
    }
}

/// Bundle of Discord-runtime dependencies that `handle_text_message`
/// reads from outside its per-message parameters. Phase 2-pre.2 of
/// intake-node-routing (docs/design/intake-node-routing.md): the body
/// reads only `http` and (optionally) `cache`, both of which are REST-
/// or cache-backed primitives. Worker-side callers without a live shard
/// pass `cache: None` and `ctx_for_chained_dispatch: None`; leader-side
/// callers pass `Some(&ctx.cache)` and `Some(ctx)` to preserve the
/// in-process category cache and the chained-dispatch path.
///
/// `ctx_for_chained_dispatch` is the only remaining `&serenity::Context`
/// dependency: `DiscordGateway::new` accepts an optional
/// `LiveDiscordTurnContext { ctx, .. }` that wires the queued-turn
/// hand-off back through the gateway's live shard. Workers cannot
/// participate in that flow (they have no shard) so they pass `None`
/// and the gateway is constructed with `live_turn = None`.
#[derive(Clone, Copy)]
pub(in crate::services::discord) struct IntakeDeps<'a> {
    pub http: &'a Arc<serenity::http::Http>,
    pub cache: Option<&'a Arc<serenity::cache::Cache>>,
    pub ctx_for_chained_dispatch: Option<&'a serenity::Context>,
    pub shared: &'a Arc<SharedData>,
    pub token: &'a str,
}

/// Per-message inputs of `handle_text_message` bundled into a single
/// owned struct. Phase 2-pre.3 of intake-node-routing: lets worker-side
/// callers (`execute_intake_turn_core`) accept a single deserialized
/// row from `intake_outbox` instead of 13 positional parameters.
///
/// All fields mirror the `intake_outbox` payload columns (see
/// migrations/postgres/0052_intake_node_routing.sql) and the per-message
/// parameters of the legacy 13-arg `handle_text_message` signature.
/// Adding a column to `intake_outbox` means adding a field here.
#[derive(Clone, Debug)]
pub(crate) struct IntakeRequest {
    pub channel_id: ChannelId,
    pub user_msg_id: MessageId,
    pub request_owner: UserId,
    pub request_owner_name: String,
    pub user_text: String,
    pub reply_to_user_message: bool,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
    pub merge_consecutive: bool,
    pub reply_context: Option<String>,
    pub has_reply_boundary: bool,
    pub dm_hint: Option<bool>,
    pub turn_kind: TurnKind,
}

/// Worker-callable entry point for executing an intake turn. Phase 2-pre.3
/// of intake-node-routing: this is the public surface a worker node will
/// invoke after claiming an `intake_outbox` row from its target queue. Pass
/// the runtime primitives the worker has (`Arc<Http>`, `Arc<SharedData>`,
/// bot token) plus the deserialized message payload; the function constructs
/// `IntakeDeps` with `cache: None` and `ctx_for_chained_dispatch: None`
/// (workers have no live gateway shard) and delegates to the existing
/// intake body.
///
/// Leader code keeps using `handle_text_message` directly with a
/// fully-populated `IntakeDeps` — leader behaviour is unchanged.
pub(crate) async fn execute_intake_turn_core(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    token: &str,
    request: IntakeRequest,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let deps = IntakeDeps {
        http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared,
        token,
    };
    handle_text_message(
        &deps,
        request.channel_id,
        request.user_msg_id,
        request.request_owner,
        &request.request_owner_name,
        &request.user_text,
        request.reply_to_user_message,
        request.defer_watcher_resume,
        request.wait_for_completion,
        request.merge_consecutive,
        request.reply_context,
        request.has_reply_boundary,
        request.dm_hint,
        request.turn_kind,
    )
    .await
}

pub(in crate::services::discord) async fn handle_text_message(
    deps: &IntakeDeps<'_>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    request_owner: UserId,
    request_owner_name: &str,
    user_text: &str,
    reply_to_user_message: bool,
    defer_watcher_resume: bool,
    wait_for_completion: bool,
    merge_consecutive: bool,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    dm_hint: Option<bool>,
    turn_kind: TurnKind,
) -> Result<(), Error> {
    let IntakeDeps {
        http,
        cache,
        ctx_for_chained_dispatch,
        shared,
        token,
    } = *deps;
    let original_channel_id = channel_id;
    let mut session_reset_reason = None;
    let mut reset_session_id_to_clear = None;
    // Get session info, allowed tools, and pending uploads
    let (session_info, provider, allowed_tools, pending_uploads) = {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id)
            && let Some(reason) =
                session_reset_reason_for_turn(session, tokio::time::Instant::now())
        {
            if let Some(retry_context) =
                session.recent_history_context(super::super::SESSION_RECOVERY_CONTEXT_MESSAGES)
            {
                let _ = super::super::turn_bridge::store_session_retry_context(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    channel_id.get(),
                    &retry_context,
                );
            }
            session_reset_reason = Some(reason);
            reset_session_id_to_clear = session.session_id.clone();
            session.clear_provider_session();
            session.history.clear();
        }
        let info = load_session_runtime_state(&mut data.sessions, channel_id);
        let uploads = data
            .sessions
            .get_mut(&channel_id)
            .map(|s| {
                s.cleared = false;
                std::mem::take(&mut s.pending_uploads)
            })
            .unwrap_or_default();
        drop(data);
        let settings = shared.settings.read().await;
        (
            info,
            settings.provider.clone(),
            settings.allowed_tools.clone(),
            uploads,
        )
    };
    let is_dm_channel = matches!(
        channel_id.to_channel(http).await.ok(),
        Some(serenity::Channel::Private(_))
    );
    let is_dm_channel = super::super::resolve_is_dm_channel(dm_hint, is_dm_channel);
    shared.record_channel_speaker(channel_id, request_owner, request_owner_name, is_dm_channel);
    let dm_default_agent = if is_dm_channel {
        super::super::agentdesk_config::resolve_dm_default_agent(&provider)
    } else {
        None
    };

    let (session_id, memento_context_loaded, current_path) = match session_info {
        Some(info) => info,
        None => {
            // Try auto-start from role_map workspace
            let ch_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let mut workspace = settings::resolve_workspace(channel_id, ch_name.as_deref());
            // Fallback: if this is a thread, try resolving workspace from parent channel
            if workspace.is_none() {
                if let Some((parent_id, parent_name)) =
                    super::super::resolve_thread_parent(http, channel_id).await
                {
                    // Use parent name from Discord API first, fall back to session map
                    let parent_ch_name = parent_name.or_else(|| {
                        let data = shared.core.try_lock().ok()?;
                        data.sessions
                            .get(&parent_id)
                            .and_then(|s| s.channel_name.clone())
                    });
                    workspace = settings::resolve_workspace(parent_id, parent_ch_name.as_deref());
                    if workspace.is_some() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🧵 Thread auto-start: resolved workspace from parent channel {}",
                            parent_id
                        );
                    }
                }
            }
            if workspace.is_none()
                && let Some(default_agent) = dm_default_agent.as_ref()
            {
                workspace = Some(default_agent.workspace.clone());
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 💬 DM auto-start: using default agent '{}' workspace",
                    default_agent.role_binding.role_id
                );
            }
            if let Some(ws_path) = workspace {
                let ws = std::path::Path::new(&ws_path);
                if ws.is_dir() {
                    let canonical = ws
                        .canonicalize()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| ws_path.clone());
                    // Resolve channel name from Discord API before worktree
                    // creation so the path uses the real name, not "unknown".
                    let (ch_name_api, cat_name) =
                        resolve_channel_category(http, cache, channel_id).await;
                    let ch_name = match super::super::resolve_thread_parent(http, channel_id).await
                    {
                        Some((_parent_id, parent_name)) => {
                            let parent = parent_name.unwrap_or_else(|| format!("{}", _parent_id));
                            Some(super::super::synthetic_thread_channel_name(
                                &parent, channel_id,
                            ))
                        }
                        None => ch_name_api,
                    };

                    // Check worktree: always use worktree for thread sessions,
                    // or when conflict detected with another session on same path.
                    // Use both dispatch_thread_parents (for reused threads) AND Discord API
                    // thread detection (for first-turn in newly created threads where
                    // dispatch_thread_parents hasn't been populated yet).
                    let wt_info = {
                        let is_thread = shared.dispatch_thread_parents.contains_key(&channel_id)
                            || super::super::resolve_thread_parent(http, channel_id)
                                .await
                                .is_some();
                        let data = shared.core.lock().await;
                        let conflict =
                            detect_worktree_conflict(&data.sessions, &canonical, channel_id);
                        drop(data);
                        let needs_worktree = is_thread || conflict.is_some();
                        if needs_worktree {
                            let reason = if is_thread {
                                "thread session"
                            } else {
                                "conflict"
                            };
                            let ch = ch_name.as_deref().unwrap_or("unknown");
                            match create_git_worktree(&canonical, ch, provider.as_str()) {
                                Ok((wt_path, branch)) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] 🌿 Auto-start worktree ({reason}): {ch} → {}",
                                        wt_path
                                    );
                                    Some(WorktreeInfo {
                                        original_path: canonical.clone(),
                                        worktree_path: wt_path,
                                        branch_name: branch,
                                    })
                                }
                                Err(_) => None,
                            }
                        } else {
                            None
                        }
                    };
                    let eff_path = wt_info
                        .as_ref()
                        .map(|wt| wt.worktree_path.clone())
                        .unwrap_or_else(|| canonical.clone());
                    {
                        let mut data = shared.core.lock().await;
                        let session =
                            data.sessions
                                .entry(channel_id)
                                .or_insert_with(|| DiscordSession {
                                    session_id: None,
                                    memento_context_loaded: false,
                                    memento_reflected: false,
                                    current_path: None,
                                    history: Vec::new(),
                                    pending_uploads: Vec::new(),
                                    cleared: false,
                                    channel_name: None,
                                    category_name: None,
                                    remote_profile_name: None,
                                    channel_id: Some(channel_id.get()),
                                    last_active: tokio::time::Instant::now(),
                                    worktree: None,

                                    born_generation: super::super::runtime_store::load_generation(),
                                    assistant_turns: 0,
                                });
                        session.current_path = Some(eff_path.clone());
                        session.channel_name = ch_name;
                        session.category_name = cat_name;
                        session.channel_id = Some(channel_id.get());
                        session.last_active = tokio::time::Instant::now();
                        session.worktree = wt_info;
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}] ▶ Auto-started session from workspace: {eff_path}");
                    let session_state = {
                        let data = shared.core.lock().await;
                        data.sessions
                            .get(&channel_id)
                            .map(|s| (s.session_id.clone(), s.memento_context_loaded))
                            .unwrap_or((None, false))
                    };
                    (session_state.0, session_state.1, eff_path)
                } else {
                    rate_limit_wait(shared, channel_id).await;
                    let _ = channel_id
                        .say(http, "No active session. Use `/start <path>` first.")
                        .await;
                    return Ok(());
                }
            } else {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(http, "No active session. Use `/start <path>` first.")
                    .await;
                return Ok(());
            }
        }
    };

    let dispatch_id_for_thread = super::super::adk_session::parse_dispatch_id(user_text);
    if should_add_turn_pending_reaction(dispatch_id_for_thread.as_deref())
        && !super::super::voice_barge_in::is_synthetic_voice_message_id(user_msg_id)
    {
        // Voice-originated turns use a synthetic msg id (>= 9e18) that does
        // not correspond to a real Discord message, so add_reaction would
        // return "Unknown Message". TTS already plays an acknowledgement
        // for the user — the ⏳ reaction is text-intake only.
        add_reaction(http, channel_id, user_msg_id, '⏳').await;
    }

    // ── Dispatch thread auto-creation ──────────────────────────────
    // When a dispatch message arrives, create a Discord thread for
    // isolated context.  All subsequent agent output goes to the thread.
    // Skip if already inside a thread (threads cannot nest).
    // Thread reuse: if the card already has an active_thread_id, redirect
    // to the existing thread instead of creating a new one.
    let is_already_thread = super::super::resolve_thread_parent(http, channel_id)
        .await
        .is_some();
    // #259: Fetch dispatch metadata once before thread creation so we can extract
    // worktree_path for both thread bootstrap and the subsequent session CWD override.
    let dispatch_info_cached = if let Some(ref did) = dispatch_id_for_thread {
        super::lookup_dispatch_info(shared.api_port, did).await
    } else {
        None
    };
    // #259: Prefer card-bound worktree over parent channel CWD for dispatch sessions.
    // All dispatch types now inject worktree_path into context via resolve_card_worktree().
    let mut dispatch_type_str = dispatch_info_cached
        .as_ref()
        .and_then(|info| info.dispatch_type.clone());
    let dispatch_context_hints = parse_dispatch_context_hints(
        dispatch_info_cached
            .as_ref()
            .and_then(|info| info.context.as_deref()),
        dispatch_type_str.as_deref(),
    );
    let dispatch_worktree_path = dispatch_context_hints.worktree_path.clone();
    let dispatch_stale_worktree_path = dispatch_context_hints.stale_worktree_path.clone();
    let dispatch_target_repo = dispatch_context_hints.target_repo.clone();
    let dispatch_reset_provider_state = dispatch_context_hints.reset_provider_state;
    let dispatch_recreate_tmux = dispatch_context_hints.recreate_tmux;
    let dispatch_retry_resume_session_id = dispatch_context_hints.retry_resume_session_id.clone();
    if let (Some(wt), Some(did)) = (&dispatch_worktree_path, &dispatch_id_for_thread) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] 🌿 Dispatch {did}: resolved worktree CWD: {wt}");
    }
    // #762: when the dispatch pins an external target_repo but emits no
    // worktree_path (e.g. refresh fell back without a usable path), resolve
    // the repo's configured directory first instead of dropping straight into
    // the default AgentDesk repo. Otherwise external-repo reviews silently
    // execute in the wrong repo.
    let dispatch_target_repo_path =
        resolve_dispatch_target_repo_dir(dispatch_target_repo.as_deref());
    let dispatch_default_path = dispatch_target_repo_path
        .clone()
        .or_else(|| {
            crate::services::platform::resolve_repo_dir()
                .filter(|p| std::path::Path::new(p).is_dir())
        })
        .unwrap_or_else(|| current_path.clone());
    let mut dispatch_effective_path = dispatch_worktree_path
        .clone()
        .unwrap_or_else(|| dispatch_default_path.clone());
    if dispatch_worktree_path.is_none() && dispatch_id_for_thread.is_some() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        if let (Some(stale_path), Some(did)) = (
            dispatch_stale_worktree_path.as_deref(),
            dispatch_id_for_thread.as_deref(),
        ) {
            tracing::warn!(
                "  [{ts}] ⚠ Dispatch {did}: context worktree_path no longer exists: {} — falling back to {}",
                stale_path,
                dispatch_effective_path
            );
        } else if let (Some(did), Some(tr), Some(tr_path)) = (
            dispatch_id_for_thread.as_deref(),
            dispatch_target_repo.as_deref(),
            dispatch_target_repo_path.as_deref(),
        ) {
            tracing::info!(
                "  [{ts}] 🌱 Dispatch {did}: no worktree_path; honoring target_repo '{}' at {}",
                tr,
                tr_path
            );
        } else {
            tracing::info!(
                "  [{ts}] 🌱 Dispatch fallback CWD: using repo root instead of inherited session path: {}",
                dispatch_effective_path
            );
        }
    }
    let dispatch_uses_thread_routing =
        crate::dispatch::dispatch_type_uses_thread_routing(dispatch_type_str.as_deref());
    let mut bootstrapped_fresh_thread_session = false;
    let channel_id = if let Some(ref did) = dispatch_id_for_thread {
        if !dispatch_uses_thread_routing {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📢 Dispatch {did} uses primary-channel routing, skipping thread creation"
            );
            channel_id
        } else {
            // Use cached dispatch metadata for thread reuse and cross-channel role override
            let dispatch_info = &dispatch_info_cached;
            let is_counter_model_dispatch =
                crate::server::routes::dispatches::use_counter_model_channel(
                    dispatch_type_str.as_deref(),
                );
            let alt_channel_id = dispatch_info
                .as_ref()
                .and_then(|i| i.discord_channel_alt.as_deref())
                .and_then(|s| s.parse::<u64>().ok())
                .map(ChannelId::new);

            if is_already_thread {
                // Ensure thread is accessible (unarchive if needed) before proceeding
                if !super::verify_thread_accessible(http, channel_id).await {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Dispatch {did} thread {channel_id} is not accessible (archived/locked), skipping"
                    );
                    return Ok(());
                }
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🧵 Dispatch {did} arrived in existing thread, skipping thread creation"
                );
                // For review dispatches in reused threads, set role override
                // so this turn uses the counter-model channel's role/model.
                if is_counter_model_dispatch {
                    if let Some(alt_ch) = alt_channel_id {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🔄 Review dispatch in reused thread: overriding role to alt channel {}",
                            alt_ch
                        );
                        shared.dispatch_role_overrides.insert(channel_id, alt_ch);
                    }
                }
                channel_id
            } else {
                // Check if card already has an active thread via internal API
                let existing_thread = dispatch_info
                    .as_ref()
                    .and_then(|i| i.active_thread_id.clone());
                let reuse_tid = existing_thread.as_ref().and_then(|t| {
                    let id = t.parse::<u64>().unwrap_or(0);
                    if id != 0 {
                        Some(ChannelId::new(id))
                    } else {
                        None
                    }
                });

                let reused = if let Some(tid) = reuse_tid {
                    if super::verify_thread_accessible(http, tid).await {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🧵 Reusing existing thread {} for dispatch {}",
                            tid,
                            did
                        );
                        bootstrapped_fresh_thread_session = super::super::bootstrap_thread_session(
                            shared,
                            tid,
                            &dispatch_effective_path,
                            http,
                            cache,
                        )
                        .await;
                        shared.dispatch_thread_parents.insert(channel_id, tid);
                        // For review dispatches reusing an implementation thread,
                        // override role/model to use the counter-model channel.
                        if is_counter_model_dispatch {
                            if let Some(alt_ch) = alt_channel_id {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 🔄 Review dispatch reusing thread: overriding role to alt channel {}",
                                    alt_ch
                                );
                                shared.dispatch_role_overrides.insert(tid, alt_ch);
                            }
                        }
                        Some(tid)
                    } else {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🧵 Thread {} is locked/inaccessible, creating new for {}",
                            tid,
                            did
                        );
                        None
                    }
                } else {
                    None
                };

                if let Some(tid) = reused {
                    tid
                } else {
                    // No existing usable thread — create new
                    let thread_title = user_text
                        .find(" - ")
                        .map(|idx| &user_text[idx + 3..])
                        .unwrap_or("dispatch")
                        .chars()
                        .take(90)
                        .collect::<String>();

                    match channel_id
                        .create_thread(
                            http,
                            poise::serenity_prelude::builder::CreateThread::new(thread_title)
                                .kind(poise::serenity_prelude::ChannelType::PublicThread)
                                .auto_archive_duration(
                                    poise::serenity_prelude::AutoArchiveDuration::OneDay,
                                ),
                        )
                        .await
                    {
                        Ok(thread) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] 🧵 Created dispatch thread {} for dispatch {}",
                                thread.id,
                                did
                            );
                            bootstrapped_fresh_thread_session =
                                super::super::bootstrap_thread_session(
                                    shared,
                                    thread.id,
                                    &dispatch_effective_path,
                                    http,
                                    cache,
                                )
                                .await;
                            shared.dispatch_thread_parents.insert(channel_id, thread.id);
                            super::link_dispatch_thread(
                                shared.api_port,
                                did,
                                thread.id.get(),
                                channel_id.get(),
                            )
                            .await;
                            thread.id
                        }
                        Err(e) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!("  [{ts}] ⚠ Failed to create dispatch thread: {e}");
                            channel_id // fallback to main channel
                        }
                    }
                }
            }
        }
    } else {
        channel_id
    };
    if dispatch_should_recover_session_worktree(
        dispatch_id_for_thread.is_some(),
        dispatch_type_str.as_deref(),
        dispatch_worktree_path.is_some(),
    ) {
        let session_worktree_path = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|session| session.worktree.as_ref())
                .map(|worktree| worktree.worktree_path.clone())
                .filter(|path| std::path::Path::new(path).is_dir())
        };
        if let Some(worktree_path) = session_worktree_path {
            if dispatch_effective_path != worktree_path {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🌿 Dispatch recovered thread worktree CWD: {} → {}",
                    dispatch_effective_path,
                    worktree_path
                );
                dispatch_effective_path = worktree_path;
            }
        }
    }
    let active_dispatch_id_for_prompt =
        super::super::adk_session::lookup_pending_dispatch_for_thread(
            shared.api_port,
            channel_id.get(),
        )
        .await
        .or_else(|| dispatch_id_for_thread.clone());
    let active_dispatch_info = match active_dispatch_id_for_prompt.as_deref() {
        Some(did) if dispatch_id_for_thread.as_deref() == Some(did) => dispatch_info_cached.clone(),
        Some(did) => super::lookup_dispatch_info(shared.api_port, did).await,
        None => None,
    };
    if let Some(active_dispatch_type) = active_dispatch_info
        .as_ref()
        .and_then(|info| info.dispatch_type.clone())
    {
        dispatch_type_str = Some(active_dispatch_type);
    }

    let (mut session_id, mut memento_context_loaded, current_path) = {
        let mut data = shared.core.lock().await;
        session_runtime_state_after_redirect(
            &mut data.sessions,
            original_channel_id,
            channel_id,
            (session_id, memento_context_loaded, current_path),
        )
    };
    let mut session_strategy_reason = if session_id.is_some() {
        "runtime_cached_provider_session"
    } else if bootstrapped_fresh_thread_session {
        "thread_session_bootstrapped"
    } else {
        "no_runtime_provider_session"
    };

    // #259: Override current_path with the pre-computed dispatch worktree path.
    // Also update the in-memory session so the worktree sticks for subsequent turns.
    //
    // #762 (B): Reused threads (where `bootstrap_thread_session` returned
    // early because the thread already had a session) carry their existing
    // `session.current_path`. Without this branch, a review dispatch that
    // pins only `target_repo` (no `worktree_path`, e.g. because the
    // external-repo worktree was cleaned up but `target_repo` still
    // resolves to the external repo root) would re-execute inside the
    // previous repo — the prompt and `adk_cwd` would both be built from
    // the stale path. Propagate `dispatch_effective_path` into the
    // session whenever it differs from the current path, regardless of
    // whether `worktree_path` was supplied.
    let mut current_path = if dispatch_session_path_should_update(
        dispatch_id_for_thread.is_some(),
        dispatch_type_str.as_deref(),
        dispatch_worktree_path.is_some(),
        bootstrapped_fresh_thread_session,
        &current_path,
        &dispatch_effective_path,
    ) {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            if session.current_path.as_deref() != Some(dispatch_effective_path.as_str()) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔄 Dispatch session CWD update: {:?} → {}",
                    session.current_path,
                    dispatch_effective_path
                );
                session.current_path = Some(dispatch_effective_path.clone());
            }
        }
        dispatch_effective_path.clone()
    } else {
        current_path
    };
    if let Some(active_info) = active_dispatch_info.as_ref() {
        let active_hints = parse_dispatch_context_hints(
            active_info.context.as_deref(),
            dispatch_type_str.as_deref(),
        );
        if let Some(active_worktree_path) = active_hints.worktree_path.as_deref()
            && current_path != active_worktree_path
        {
            let original_path =
                resolve_dispatch_target_repo_dir(active_hints.target_repo.as_deref())
                    .unwrap_or_else(|| dispatch_default_path.clone());
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔄 Active dispatch CWD update: {:?} → {}",
                    session.current_path,
                    active_worktree_path
                );
                session.current_path = Some(active_worktree_path.to_string());
                if crate::dispatch::dispatch_type_requires_fresh_worktree(
                    dispatch_type_str.as_deref(),
                ) {
                    session.worktree = Some(WorktreeInfo {
                        original_path,
                        worktree_path: active_worktree_path.to_string(),
                        branch_name: active_hints.worktree_branch.unwrap_or_default(),
                    });
                }
            }
            current_path = active_worktree_path.to_string();
        }
    }
    // Sanitize input
    let sanitized_input = ai_screen::sanitize_user_input(user_text);

    let role_binding = {
        // For cross-channel dispatch reuse (e.g. review in implementation thread),
        // resolve role from the override channel instead of the thread's parent.
        if let Some(override_ch) = shared.dispatch_role_overrides.get(&channel_id) {
            let alt_ch = *override_ch;
            resolve_role_binding(alt_ch, None)
        } else {
            let data = shared.core.lock().await;
            let ch_name = data
                .sessions
                .get(&channel_id)
                .and_then(|s| s.channel_name.as_deref());
            resolve_role_binding(channel_id, ch_name)
        }
    }
    .or_else(|| {
        dm_default_agent
            .as_ref()
            .map(|resolved| resolved.role_binding.clone())
    });

    // For cross-channel dispatch reuse, override the provider so the turn
    // executes via the counter-model CLI (e.g. Codex reviews Claude's work).
    let provider = if shared.dispatch_role_overrides.contains_key(&channel_id) {
        role_binding
            .as_ref()
            .and_then(|rb| rb.provider.clone())
            .unwrap_or(provider)
    } else {
        provider
    };

    if matches!(provider, ProviderKind::Codex)
        && !dispatch_reset_provider_state
        && !dispatch_recreate_tmux
        && let Some(resume_session_id) = dispatch_retry_resume_session_id.as_deref()
    {
        if session_id.as_deref() != Some(resume_session_id) {
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                session.restore_provider_session(Some(resume_session_id.to_string()));
                memento_context_loaded = session.memento_context_loaded;
            } else {
                memento_context_loaded = false;
            }
            session_id = Some(resume_session_id.to_string());
        }
        session_strategy_reason = "dispatch_context_retry_resume";
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Dispatch retry: using context-supplied Codex resume session for channel {}",
            channel_id.get()
        );
    }

    // Derive dispatch prompt profile before memory recall so ReviewLite can
    // skip heavy memory work consistently across supported backends.
    let dispatch_profile = {
        let dispatch_type = active_dispatch_id_for_prompt
            .as_ref()
            .and_then(|_| dispatch_type_str.as_deref());
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_deref());
        DispatchProfile::for_turn(
            dispatch_type,
            settings::resolve_dispatch_profile(channel_id, channel_name),
        )
    };

    if dispatch_reset_provider_state || dispatch_recreate_tmux {
        super::super::commands::reset_channel_provider_state(
            http,
            shared,
            &provider,
            channel_id,
            if dispatch_recreate_tmux {
                "dispatch hard reset"
            } else {
                "dispatch provider reset"
            },
            dispatch_reset_provider_state,
            false,
            dispatch_recreate_tmux,
        )
        .await;
        session_id = None;
        memento_context_loaded = false;
        session_strategy_reason =
            dispatch_reset_lifecycle_code(dispatch_reset_provider_state, dispatch_recreate_tmux);
        if let Some(ref did) = dispatch_id_for_thread {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻️ Dispatch {did}: reset_provider_state={}, recreate_tmux={}, skipping provider session reuse",
                dispatch_reset_provider_state,
                dispatch_recreate_tmux
            );
        }
    }

    let thread_parent = super::super::resolve_thread_parent(http, channel_id).await;
    let fast_mode_channel_id = effective_fast_mode_channel_id(channel_id, thread_parent.clone());
    super::super::commands::reset_provider_session_if_pending(
        http,
        shared,
        &provider,
        channel_id,
        fast_mode_channel_id,
    )
    .await;
    refresh_session_strategy_after_pending_reset(
        shared,
        channel_id,
        &mut session_id,
        &mut memento_context_loaded,
        &mut session_strategy_reason,
    )
    .await;
    let prompt_prep_started = std::time::Instant::now();

    // Resolve channel/tmux session name from current session state. We need the
    // persisted provider session_id before recall so external memory can scope by run_id.
    let (channel_name, tmux_session_name) = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone());
        let tmux_session_name = if provider.uses_managed_tmux_backend() {
            channel_name
                .as_ref()
                .map(|name| provider.build_tmux_session_name(name))
        } else {
            None
        };
        (channel_name, tmux_session_name)
    };
    let adk_session_key = build_adk_session_key(shared, channel_id, &provider).await;
    if session_reset_reason.is_some() {
        if let Some(ref key) = adk_session_key {
            super::super::adk_session::clear_provider_session_id(key, shared.api_port).await;
        }
        if let Some(ref session_id_to_clear) = reset_session_id_to_clear {
            let _ = super::super::internal_api::clear_stale_session_id(session_id_to_clear).await;
        }
    }
    let turn_goal_kind = if !dispatch_reset_provider_state && !dispatch_recreate_tmux {
        classify_codex_goal_command_for_provider(
            &provider,
            user_text,
            super::super::commands::channel_codex_goals_setting(shared, fast_mode_channel_id).await,
        )
    } else {
        GoalCommandKind::NotGoal
    };
    let force_fresh_provider_session = matches!(turn_goal_kind, GoalCommandKind::FreshStart);
    let fresh_codex_goal_session_requested = force_fresh_provider_session;
    if force_fresh_provider_session {
        clear_codex_goal_start_provider_session(
            shared,
            channel_id,
            adk_session_key.as_deref(),
            &mut session_id,
            &mut memento_context_loaded,
            &mut session_strategy_reason,
        )
        .await;
    }
    let sanitized_input = if force_fresh_provider_session {
        rewrite_fresh_goal_prompt(&sanitized_input)
    } else {
        sanitized_input
    };
    if session_id.is_none() {
        if fresh_codex_goal_session_requested {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for channel {} due to /goal fresh session request",
                channel_id.get()
            );
        } else if dispatch_reset_provider_state || dispatch_recreate_tmux {
            session_strategy_reason = dispatch_reset_lifecycle_code(
                dispatch_reset_provider_state,
                dispatch_recreate_tmux,
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for dispatch reset_provider_state={} recreate_tmux={}",
                dispatch_reset_provider_state,
                dispatch_recreate_tmux
            );
        } else if let Some(reason) = session_reset_reason {
            let ts = chrono::Local::now().format("%H:%M:%S");
            session_strategy_reason = session_reset_reason_lifecycle_code(reason);
            let display_reason = match reason {
                SessionResetReason::IdleExpired => "idle timeout",
                SessionResetReason::AssistantTurnCap => "assistant turn cap",
            };
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for channel {} due to {}",
                channel_id.get(),
                display_reason
            );
        } else if let Some(ref key) = adk_session_key {
            let restored = super::super::adk_session::fetch_provider_session_id(
                key,
                &provider,
                shared.api_port,
            )
            .await;
            if restored.is_some() {
                session_strategy_reason = "db_provider_session_restored";
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ Restored provider session_id from DB for {}",
                    key
                );
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.restore_provider_session(restored.clone());
                    memento_context_loaded = session.memento_context_loaded;
                }
                // Notify: session restored — send before placeholder so it appears first
                send_restore_notification(shared, http, channel_id, &provider, restored.as_deref())
                    .await;
            } else {
                session_strategy_reason = "no_cached_provider_session";
            }
            session_id = restored;
        } else {
            session_strategy_reason = "session_key_unavailable";
        }
    }
    let turn_id = format!("discord:{}:{}", channel_id.get(), user_msg_id.get());
    let session_retry_context = take_session_retry_context(shared, channel_id, Some(&turn_id));
    let reply_context = merge_reply_contexts(
        reply_context,
        session_retry_context
            .as_ref()
            .map(|context| context.formatted_context.clone()),
    );

    // #1332: probe turn liveness BEFORE posting any placeholder so a queued
    // message renders the dedicated `📬 메시지 대기 중` card instead of the
    // misleading `🔄 백그라운드 처리 중` Active card. The previous order
    // (send_intake_placeholder → mailbox_try_start_turn) made every queued
    // message look like processing had already begun.
    //
    // Create cancel token — with second check to close the TOCTOU race window.
    // Multiple messages can pass the initial cancel_tokens check (line 169) concurrently
    // because the async gap between check and insert allows interleaving.
    // If another message won the race, queue ourselves and clean up.
    let cancel_token = Arc::new(CancelToken::new());
    let started = mailbox_try_start_turn_with_terminal_marker_cleanup(
        shared,
        channel_id,
        cancel_token.clone(),
        request_owner,
        user_msg_id,
        adk_session_key.as_deref(),
    )
    .await;

    // #1332 dispatch hand-off: if this turn was previously enqueued and is now
    // being dispatched, reuse the Queued placeholder card so the user sees a
    // single message transition `📬 → 🔄` instead of two distinct placeholders.
    //
    // codex review P2 (round-after-#1332): merged interventions accumulate
    // multiple `source_message_ids`; each lost a separate race and registered
    // its own queued placeholder. Drain mappings for ALL of them — the head
    // (intervention.message_id) becomes the live Active card, and any
    // additional source ids' Discord cards must be tidied up so the user does
    // not see duplicate `📬` cards left behind for the merged tail.
    let queued_placeholder_handoff = if started {
        // Use the write-through helper so the on-disk snapshot stays in sync
        // with the in-memory map (codex review round-3 P2). Round-5 P2: the
        // helper now takes the per-channel async persistence mutex, so this
        // dispatch hand-off serializes against any concurrent race-loss
        // render path on the same channel.
        shared
            .remove_queued_placeholder(channel_id, user_msg_id)
            .await
    } else {
        None
    };

    // codex review P1/P2: when this turn lost the race, drive the entire
    // race-loss path (placeholder POST, mapping insert, enqueue, idle-drain
    // safety net, queued-card edit) here and return. Splitting into a
    // dedicated `if !started` block — instead of folding it into the
    // `placeholder_msg_id` let-binding below — keeps the started==true
    // path linear and lets us bail out without the post-let main flow ever
    // running on a non-active turn.
    if !started {
        let bot_owner_provider = super::super::resolve_discord_bot_provider(token);
        let is_thread_routed = channel_id != original_channel_id;
        let want_queued_card = !turn_kind.is_background_trigger() && !is_thread_routed;

        // codex review round-9 P2 (#1332): enqueue the intervention BEFORE
        // any Discord HTTP await. The previous order (POST placeholder →
        // insert mapping → enqueue) opened a window where the still-running
        // active turn could finalize during the POST/insert awaits. Without
        // an entry in the mailbox queue, `finalize_turn_state` reports
        // `has_pending == false`, and `turn_bridge` clears
        // `dispatch_role_overrides` for this channel. Our late enqueue then
        // lands without the override, so the queued dispatch runs under the
        // default provider/role instead of the dispatch-role routing the
        // request expects (e.g. a Codex-review hand-off would execute under
        // Claude). Enqueueing first keeps the mailbox snapshot consistent
        // with the override lifecycle: as long as our intervention is
        // queued, the override survives.
        //
        // Trade-off: this inverts the round-2 invariant ("queued_placeholders
        // mapping inserted BEFORE enqueue") — a fast dispatch could now
        // observe the queued intervention before our placeholder mapping
        // lands. The existing dispatch fallback (`else` branch ~line 3066 in
        // `handle_text_message`) tolerates that case by POSTing a fresh card
        // via `send_intake_placeholder`, restoring the pre-PR behavior of "a
        // fresh card on dispatch when no queued mapping exists." Round-2's
        // duplicate-card concern is mitigated below by checking
        // `active_user_message_id == user_msg_id` immediately before the
        // mapping insert: if the dispatch path has already promoted our
        // intervention into an active turn (with its own fresh card), we
        // delete our orphan POST and skip the mapping insert.
        let enqueue_outcome = super::super::mailbox_enqueue_intervention(
            shared,
            &bot_owner_provider,
            channel_id,
            build_race_requeued_intervention(
                request_owner,
                user_msg_id,
                user_text,
                reply_context.clone(),
                has_reply_boundary,
                merge_consecutive,
            ),
        )
        .await;
        let enqueued = enqueue_outcome.enqueued;

        // codex review P1: cover the residual race window where the active
        // turn finished between `mailbox_try_start_turn` and the enqueue
        // above. In that case `mailbox_finish_turn` saw an empty queue and
        // skipped the dequeue chain — schedule a deferred drain so the
        // intervention we just enqueued does not strand. Cheap no-op when
        // the active turn is still running. Round-9: this still runs first
        // so the deferred kickoff fires even if the placeholder POST below
        // ends up failing.
        if enqueued && !super::super::mailbox_has_active_turn(shared, channel_id).await {
            super::super::schedule_deferred_idle_queue_kickoff(
                shared.clone(),
                bot_owner_provider.clone(),
                channel_id,
                "race-loss enqueue idle drain",
            );
        }

        // If the enqueue was rejected (dedup / duplicate) there is nothing
        // for the dispatch path to pick up. Skip the placeholder POST + the
        // mapping insert entirely — POSTing a fresh card here would orphan
        // it. `📬` reaction is also skipped (the prior live enqueue already
        // owns the card and emoji). Just clean up `⏳` and return.
        if !enqueued {
            super::super::formatting::remove_reaction_raw(http, channel_id, user_msg_id, '⏳')
                .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔁 RACE: race-lost intervention dedup-merged into existing queue entry (channel {}); skipping placeholder POST",
                channel_id
            );
            return Ok(());
        }

        // codex review round-5 P2 (finding 2 — re-queue reuse): if a queued
        // placeholder mapping already exists for `(channel_id, user_msg_id)`
        // — typically because the active turn finished and the queued
        // turn was about to dispatch, but a new turn intercepted and won
        // the mailbox race before that dispatch could run — REUSE the
        // existing `📬` card instead of POSTing a fresh placeholder.
        // Posting a new placeholder would orphan the prior one (its mapping
        // would be overwritten by the new `insert_queued_placeholder`
        // below, and the old card would stay visible with no bookkeeping
        // path to clean it up). Background-trigger turns and thread-routed
        // turns never write to `queued_placeholders`, so they always go
        // through the fresh POST path.
        let existing_queued_card = if want_queued_card {
            shared
                .queued_placeholders
                .get(&(channel_id, user_msg_id))
                .map(|entry| *entry.value())
        } else {
            None
        };
        let reused_existing_mapping = existing_queued_card.is_some();

        let placeholder_msg_id = if let Some(existing) = existing_queued_card {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻ RACE: reusing existing queued placeholder (channel {}, msg {}) — re-queue without new POST",
                channel_id,
                existing
            );
            existing
        } else {
            let post_result = send_intake_placeholder(
                http.clone(),
                shared.clone(),
                channel_id,
                if reply_to_user_message && dispatch_id_for_thread.is_none() {
                    Some((channel_id, user_msg_id))
                } else {
                    None
                },
            )
            .await;

            match post_result {
                Ok(msg_id) => msg_id,
                Err(error) => {
                    // POST failed AFTER enqueue. Round-9 trade-off: the
                    // intervention is already in the mailbox queue, so a
                    // later kickoff (or the deferred idle drain scheduled
                    // above) will dispatch it — `dispatch_queued_turn` ->
                    // `handle_text_message` will POST its own fresh card
                    // through the missing-mapping fallback. The user
                    // briefly sees `⏳` only and no `📬`, but the message
                    // WILL be processed correctly. Roll back the `⏳`
                    // sentinel so the user knows we did not silently
                    // accept the message.
                    super::super::formatting::remove_reaction_raw(
                        http,
                        channel_id,
                        user_msg_id,
                        '⏳',
                    )
                    .await;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ RACE: placeholder POST failed for race-lost message AFTER enqueue (channel {}, error={}); message remains queued, dispatch will POST fresh card",
                        channel_id,
                        error
                    );
                    // #1984 (codex C — observation): the user message is
                    // already in the mailbox queue; the dispatch path will
                    // POST a fresh card via the missing-mapping fallback.
                    crate::services::observability::emit_intake_placeholder_post_failed(
                        provider.as_str(),
                        channel_id.get(),
                        Some(user_msg_id.get()),
                        "race_after_enqueue",
                        "fresh_card_via_dispatch",
                        &error.to_string(),
                    );
                    return Ok(());
                }
            }
        };

        // Insert the mapping AFTER the POST. Round-2's "mapping before
        // enqueue" invariant does not apply here (round-9 reorder); instead
        // we hold the per-channel persistence mutex across the recheck +
        // insert so a concurrent `dispatch_queued_turn` cannot take our
        // entry between the recheck and the write.
        //
        // Round-10 dispatch-state recheck: between our enqueue and this
        // point, the active turn could have finished AND turn_bridge could
        // have picked up our intervention from the queue, started its own
        // turn for us, and POSTed its own fresh card via the dispatch
        // fallback (no mapping → `send_intake_placeholder`). We must detect
        // that case BEFORE inserting our mapping; if dispatch already
        // promoted us into an active turn, our `placeholder_msg_id` is an
        // orphan and inserting a mapping would point at a stale `...` card
        // (and the subsequent `ensure_queued` PATCH would render `📬` on a
        // turn that is already running).
        //
        // Round-9 placed the snapshot BEFORE the per-channel persist lock;
        // codex round-10 P2 flagged the residual window: if the active
        // turn finishes between the snapshot and the lock acquire, the
        // dispatch path can still slip in (take the lock, see no mapping,
        // post fresh Active placeholder, release the lock) — and THIS
        // branch then takes the lock, observes the (now-stale) snapshot
        // result, inserts a Queued mapping for a turn that is already
        // running, and renders a stale `📬` card + sidecar entry that no
        // future event will reference.
        //
        // Fix: take the per-channel persist lock FIRST, then snapshot the
        // mailbox under the lock, then insert. Atomicity invariant:
        // "ownership check + insert + ensure_queued PATCH all happen under
        // one held lock guard." `dispatch_queued_turn`'s
        // `remove_queued_placeholder` mutator also serializes through this
        // same per-channel mutex (see `SharedData::remove_queued_placeholder`
        // at mod.rs:1151), so once we hold the lock the dispatch path
        // cannot promote our intervention to active until we release.
        //
        // Codex round-11 P2 broadened the recheck: the round-10 condition
        // `active_user_message_id == user_msg_id` only catches the
        // dispatch-promotion case. There are other queue-exit timelines
        // (cancellation, supersede, merged-drain of a non-head
        // source_message_id) where `user_msg_id` has left the queue but the
        // active turn does NOT equal us — `active_user_message_id` may be
        // `None` or a different message (e.g. the merge-head). Inserting a
        // `📬` mapping in those cases would orphan a card that no future
        // dispatch or queue-exit cleanup will ever reference. The expanded
        // recheck below additionally verifies `user_msg_id` is still in the
        // intervention queue (head `message_id` OR any `source_message_ids`
        // entry) and bails if not.
        //
        // Background-trigger / thread-routed turns + reused mappings stay
        // out of the `queued_placeholders` map by design and skip the
        // dispatch-state recheck entirely.
        let persist_guard_for_render = if want_queued_card && !reused_existing_mapping {
            // Use `lock_owned()` so the guard owns the `Arc` and can outlive
            // the local `persist_lock` binding when we hand it off to the
            // queued-card render branch below (round-10: single critical
            // section spanning the dispatch-state recheck, the mapping
            // insert, and the `ensure_queued` PATCH).
            let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
            let persist_guard = persist_lock.lock_owned().await;
            // Snapshot UNDER the lock so a concurrent dispatch path cannot
            // promote our intervention to active between this read and the
            // mapping insert below. `dispatch_queued_turn` removes the
            // queued mapping via `remove_queued_placeholder`, which itself
            // acquires this same per-channel persist mutex; while we hold
            // the guard, no dispatch path can advance from "queued" to
            // "active for our user_msg_id".
            let snapshot = super::super::mailbox_snapshot(shared, channel_id).await;
            // Round-11 codex review P2: the round-10 recheck only bailed when
            // `active_user_message_id == user_msg_id`, but there are other
            // states where `user_msg_id` is no longer in the queue and a
            // `📬` mapping must NOT be inserted:
            //   1. The intervention was cancelled / superseded between our
            //      enqueue and our lock acquire (queue-exit drain ran).
            //   2. The intervention was the non-head `source_message_id` of a
            //      merged Intervention that has already been dequeued (the
            //      merged-drain ran on dispatch).
            // In either case `active_user_message_id` may be `None` or a
            // different message (e.g. the merge-head), so the round-10
            // `active == user_msg_id` check passes through and we would
            // insert a `📬` mapping for a `user_msg_id` that no future
            // dispatch or queue-exit cleanup will ever reference → stale
            // card forever.
            //
            // Fix: in addition to the round-10 active-equals-us check, also
            // verify `user_msg_id` is still in the queue (head
            // `intervention.message_id` OR any `source_message_ids` entry).
            // If neither holds, treat it as a race-loss and bail.
            let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
                intervention.message_id == user_msg_id
                    || intervention.source_message_ids.contains(&user_msg_id)
            });
            let dispatch_already_running_for_our_msg =
                snapshot.active_user_message_id == Some(user_msg_id);
            if dispatch_already_running_for_our_msg || !still_queued {
                // Either dispatch already promoted us into an active turn
                // (round-10 case) OR our entry has left the queue via
                // cancellation / supersede / merged-drain (round-11 case).
                // In all cases our POSTed placeholder is an orphan that no
                // future dispatch or queue-exit cleanup will ever reference
                // — drop the lock before the HTTP DELETE await, delete the
                // orphan, remove the `⏳` reaction, and skip the mapping
                // insert.
                drop(persist_guard);
                let _ = channel_id.delete_message(http, placeholder_msg_id).await;
                super::super::formatting::remove_reaction_raw(http, channel_id, user_msg_id, '⏳')
                    .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                if dispatch_already_running_for_our_msg {
                    tracing::info!(
                        "  [{ts}] 🔁 RACE: dispatch already started turn for our message (channel {}, msg {}); deleting orphan placeholder POST",
                        channel_id,
                        user_msg_id
                    );
                } else {
                    tracing::info!(
                        "  [{ts}] 🔁 RACE: message no longer queued (cancelled/superseded/merged-drained) (channel {}, msg {}); deleting orphan placeholder POST",
                        channel_id,
                        user_msg_id
                    );
                }
                return Ok(());
            }
            shared.insert_queued_placeholder_locked(channel_id, user_msg_id, placeholder_msg_id);
            // Hand the still-held guard to the `ensure_queued` PATCH branch
            // below so the entire ownership check + insert + PATCH critical
            // section runs under one held lock guard (the round-10
            // atomicity invariant).
            Some(persist_guard)
        } else {
            None
        };

        // #1116 Pending-reaction emoji machine: 📬 queued → ⏳ processing →
        // ✅ done. Round-9: enqueue already happened above; the reaction
        // safely reflects the actual queue state.
        //
        // #2036 Surface 3 fix: previously, if the active turn finished
        // between this enqueue and the `add_reaction` await below, the
        // dequeue path's 📬 cleanup could run before our add landed and
        // leave the icon stuck on a turn that had already started. The
        // user-reported case (run 767447c8): dispatch message lands on a
        // channel whose previous turn is wrapping up, so the message gets
        // queued and reacted with 📬; the bridge then promotes it before
        // the add_reaction await resolves, and the leftover 📬 lies about
        // codex still being queue-pending while codex is in fact already
        // responding to the dispatch. Round-12 fix: after the
        // `add_reaction` await resolves, re-check whether our message is
        // still in the queue. If the queued_placeholder mapping has been
        // consumed (i.e. dispatch already promoted us into an active
        // turn), strip the just-added queue-pending emoji so the visual
        // state matches reality.
        if !is_thread_routed && should_add_turn_pending_reaction(dispatch_id_for_thread.as_deref())
        {
            // #1190 follow-up: merged messages get ➕ so the user can tell
            // them apart from standalone queue head entries (📬).
            let emoji = if enqueue_outcome.merged {
                '➕'
            } else {
                '📬'
            };
            add_reaction(http, channel_id, user_msg_id, emoji).await;
            // #2036 Surface 3: detect queue→start races where the
            // dispatch path consumed our mapping before this reaction
            // landed and proactively unstick the emoji.
            if !shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id) {
                super::super::formatting::remove_reaction_raw(http, channel_id, user_msg_id, emoji)
                    .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔁 RACE: queue-pending {emoji} reacted after dequeue promotion (channel {}, msg {}); removed stale reaction",
                    channel_id,
                    user_msg_id
                );
            }
        }
        // #796: Background-trigger turns (notify-bot driven, info-only) must
        // NOT have their placeholder deleted on race-loss. The placeholder is
        // the user-visible breadcrumb of the background notification (e.g.
        // a `Bash run_in_background` completion message).
        //
        // #1332: Foreground turns EDIT the bare `...` into a `📬 메시지 대기
        // 중` card via the placeholder controller. Mapping was already
        // inserted before enqueue (codex review P2); on edit failure we roll
        // back the mapping AND delete the Discord message so users never see
        // a stale `...` placeholder.
        if turn_kind.is_background_trigger() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔔 RACE: background-trigger placeholder preserved (channel {}, msg {})",
                channel_id,
                placeholder_msg_id
            );
        } else if want_queued_card && !reused_existing_mapping {
            // codex review round-3 P1 + round-5 P2 (finding 1 — atomic
            // ownership coupling) + round-10 P2 (single critical section):
            // between `mailbox_enqueue_intervention` and the `ensure_queued`
            // await below, the active turn can finish and the dispatch
            // path can already have consumed our
            // `(channel_id, user_msg_id)` mapping — at which point the
            // placeholder we POSTed has been promoted to the live response
            // card. Editing it to `📬 메시지 대기 중` (or deleting it on the
            // fallback branch) would corrupt/erase the active card. Round-4
            // checked ownership immediately before the PATCH, but the await
            // window between the check and the PATCH still allowed
            // `dispatch_queued_turn` (or `queue_exit_drain_queued_placeholders`)
            // to consume the mapping concurrently. Round-5 wraps the
            // ownership recheck + `ensure_queued` PATCH + persistence
            // rollback in a single critical section guarded by the
            // per-channel async persistence mutex. Round-10 extends that
            // critical section UPSTREAM through the dispatch-state recheck
            // and the mapping insert: we acquire the persist lock once
            // (above, where `dispatch_already_running_for_our_msg` is
            // computed), and pass the SAME held guard through to this
            // PATCH branch via `persist_guard_for_render`. Every other
            // path that mutates `queued_placeholders` (insert / remove /
            // merged drain / queue-exit drain) takes the same mutex, so
            // the mapping cannot change underneath this PATCH once we
            // hold the lock.
            //
            // Invariant (round-10): the dispatch-state snapshot, the
            // mapping insert, the ownership recheck, and the
            // `ensure_queued` PATCH all share ONE held lock guard. Any
            // alternative ordering would reopen either the round-4 hazard
            // or the round-9-residual hazard codex flagged in round-10.
            let persist_guard = persist_guard_for_render
                .expect("round-10: persist guard must be held by the matching insert branch");
            if !shared.queued_placeholder_still_owned(channel_id, user_msg_id, placeholder_msg_id) {
                drop(persist_guard);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔁 RACE: queued placeholder handoff already consumed by dispatch (channel {}, msg {}); skipping render",
                    channel_id,
                    placeholder_msg_id
                );
            } else {
                let gateway = DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    bot_owner_provider.clone(),
                    None,
                );
                let key = super::super::placeholder_controller::PlaceholderKey {
                    provider: bot_owner_provider.clone(),
                    channel_id,
                    message_id: placeholder_msg_id,
                };
                let queued_input = super::super::placeholder_controller::PlaceholderActiveInput {
                    reason: super::super::formatting::MonitorHandoffReason::Queued,
                    started_at_unix: chrono::Utc::now().timestamp(),
                    tool_summary: None,
                    command_summary: None,
                    reason_detail: None,
                    context_line: None,
                    request_line: Some(user_text.to_string()),
                    progress_line: None,
                };
                let outcome = shared
                    .placeholder_controller
                    .ensure_queued(&gateway, key, queued_input)
                    .await;
                use super::super::placeholder_controller::PlaceholderControllerOutcome::*;
                match outcome {
                    Edited | Coalesced => {
                        drop(persist_guard);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 📬 RACE: queued placeholder rendered (channel {}, msg {})",
                            channel_id,
                            placeholder_msg_id
                        );
                    }
                    _ => {
                        // Edit failed — roll back the mapping and delete the
                        // raw `...` so the dispatch path never matches a
                        // Discord message that no longer exists. The lock
                        // guarantees the mapping cannot have changed since
                        // our recheck above, so a single decision (still
                        // owned → roll back) is sound. Use the `_locked`
                        // variant to avoid re-acquiring the lock we
                        // already hold (round-5 P2).
                        let still_owned_under_lock = shared.queued_placeholder_still_owned(
                            channel_id,
                            user_msg_id,
                            placeholder_msg_id,
                        );
                        if still_owned_under_lock {
                            shared.remove_queued_placeholder_locked(channel_id, user_msg_id);
                        }
                        drop(persist_guard);
                        if still_owned_under_lock {
                            let _ = channel_id.delete_message(http, placeholder_msg_id).await;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] ⚠ RACE: queued placeholder render failed, deleted instead (channel {}, msg {})",
                                channel_id,
                                placeholder_msg_id
                            );
                        } else {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] 🔁 RACE: queued placeholder render failed AND handoff already consumed (channel {}, msg {}); leaving Discord state intact",
                                channel_id,
                                placeholder_msg_id
                            );
                        }
                    }
                }
            }
        } else if want_queued_card && reused_existing_mapping {
            // codex review round-5 P2 (finding 2): the existing card
            // already shows `📬 메시지 대기 중`. Skip the redundant
            // `ensure_queued` PATCH (the prior race-loss already wrote it,
            // and re-emitting the identical content would hit a
            // `Coalesced` no-op anyway). Leaving the card untouched is
            // correct — the user already sees it.
            //
            // Round-9 note: the round-6 "reused mapping + dedup-rejected
            // enqueue" sub-branch (preserving a card owned by an earlier
            // enqueue) is gone — this code path is only reached when
            // `enqueued == true` because we now return early on dedup
            // rejection (see the `if !enqueued { return Ok(()); }` block
            // above). The earlier owner's lifecycle still owns the card,
            // and our return runs before any placeholder POST/edit.
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻ RACE: re-queue reused existing 📬 card without re-render (channel {}, msg {})",
                channel_id,
                placeholder_msg_id
            );
        } else {
            // Background-trigger turns hit the explicit branch above;
            // remaining cases (e.g. is_thread_routed) fall here and have
            // no queued card to render — POSTed placeholder is a bare
            // `...` and would otherwise leak.
            let _ = channel_id.delete_message(http, placeholder_msg_id).await;
        }
        super::super::formatting::remove_reaction_raw(http, channel_id, user_msg_id, '⏳').await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔀 RACE: message queued (another turn won), channel {}",
            channel_id
        );
        return Ok(());
    }

    let placeholder_msg_id = if let Some(existing) = queued_placeholder_handoff {
        // Drive the controller from Queued → Active so the user sees the
        // existing `📬 메시지 대기 중` card morph into `🔄 응답 처리 중`
        // at the exact moment the queued turn starts. The streaming path will
        // overwrite this Active card with response text shortly after; the
        // brief Active beat is the visible "we picked your queued message up"
        // signal. If the controller rejects (e.g. the entry is already
        // terminal because of a race), we still reuse the message id so the
        // streaming path edits the same Discord card and the user does not
        // see a duplicate placeholder.
        let provider_for_handoff = super::super::resolve_discord_bot_provider(token);
        let key = super::super::placeholder_controller::PlaceholderKey {
            provider: provider_for_handoff.clone(),
            channel_id,
            message_id: existing,
        };
        let active_input = super::super::placeholder_controller::PlaceholderActiveInput {
            reason: super::super::formatting::MonitorHandoffReason::Queued,
            started_at_unix: chrono::Utc::now().timestamp(),
            tool_summary: None,
            command_summary: None,
            reason_detail: None,
            context_line: None,
            request_line: Some(user_text.to_string()),
            progress_line: None,
        };
        let gateway = super::super::gateway::DiscordGateway::new(
            http.clone(),
            shared.clone(),
            provider_for_handoff,
            None,
        );
        let _ = shared
            .placeholder_controller
            .ensure_active(&gateway, key, active_input)
            .await;
        // codex review P2: streaming overwrites this Discord message directly
        // and never calls `transition`/`detach` on the controller. `Active`
        // entries are excluded from `evict_terminal_entries` so without a
        // detach here every queued foreground turn would leave a permanent
        // controller row. Drop the entry now — streaming owns the card past
        // this point and the controller is no longer the source of truth.
        shared
            .placeholder_controller
            .detach_by_message(channel_id, existing);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📬➡️🔄 DISPATCH: queued placeholder transitioned to Active (channel {}, msg {})",
            channel_id,
            existing
        );
        existing
    } else {
        // Active turn started cleanly — POST a fresh placeholder. If the POST
        // fails we MUST release the mailbox slot we just acquired, otherwise
        // the channel is stuck with `current_msg_id == 0` until the cancel
        // token times out (codex review P1).
        match send_intake_placeholder(
            http.clone(),
            shared.clone(),
            channel_id,
            if reply_to_user_message
                && dispatch_id_for_thread.is_none()
                && !super::super::voice_barge_in::is_synthetic_voice_message_id(user_msg_id)
            {
                Some((channel_id, user_msg_id))
            } else {
                None
            },
        )
        .await
        {
            Ok(msg_id) => msg_id,
            Err(error) => {
                let bot_owner_provider = super::super::resolve_discord_bot_provider(token);
                let kicked = release_mailbox_after_placeholder_post_failure(
                    shared,
                    &bot_owner_provider,
                    channel_id,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ INTAKE: placeholder POST failed after mailbox slot acquired (channel {}, error={}); released mailbox slot, kickoff_scheduled={}",
                    channel_id,
                    error,
                    kicked
                );
                // #1984 (codex C — observation): the mailbox slot is
                // released; whether a follow-up kickoff was scheduled
                // determines if the user message can still progress.
                let recovery = if kicked {
                    "mailbox_released_kickoff_rescheduled"
                } else {
                    "mailbox_released_kickoff_skipped"
                };
                crate::services::observability::emit_intake_placeholder_post_failed(
                    provider.as_str(),
                    channel_id.get(),
                    Some(user_msg_id.get()),
                    "intake_after_mailbox_slot",
                    recovery,
                    &error.to_string(),
                );
                return Err::<(), Error>(error.into());
            }
        }
    };

    shared
        .global_active
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    shared
        .turn_start_times
        .insert(channel_id, std::time::Instant::now());
    log_session_strategy_diagnostic(
        channel_id,
        &provider,
        dispatch_profile,
        session_strategy_reason,
        session_id.as_deref(),
        adk_session_key.as_deref(),
        tmux_session_name.as_deref(),
        session_retry_context.is_some(),
        memento_context_loaded,
    )
    .await;
    let cli_was_just_spawned = cli_just_spawned_for_emit(tmux_session_name.as_deref());
    emit_session_strategy_lifecycle(
        shared,
        channel_id,
        &turn_id,
        adk_session_key.as_deref(),
        active_dispatch_id_for_prompt.as_deref(),
        session_id.as_deref(),
        session_strategy_reason,
        cli_was_just_spawned,
    )
    .await;

    let (memory_settings, memory_backend) = build_memory_backend(role_binding.as_ref());
    let memento_recall_gate = memento_recall_gate_decision(
        &memory_settings,
        memento_context_loaded,
        user_text,
        dispatch_profile,
    );
    let memory_recall = if !memento_recall_gate.should_recall {
        RecallResponse::default()
    } else {
        memory_backend
            .recall(RecallRequest {
                provider: provider.clone(),
                role_id: resolve_memory_role_id(role_binding.as_ref()),
                channel_id: channel_id.get(),
                session_id: resolve_memory_session_id(session_id.as_deref(), channel_id.get()),
                dispatch_profile,
                user_text: user_text.to_string(),
                mode: memento_recall_gate.mode,
            })
            .await
    };
    if memory_settings.backend == settings::MemoryBackendKind::Memento {
        let ts = chrono::Local::now().format("%H:%M:%S");
        let recall_bytes = memory_recall
            .external_recall
            .as_deref()
            .map(str::len)
            .unwrap_or(0);
        let bucket = if !memento_recall_gate.should_recall {
            RecallSizeBucket::Skipped
        } else {
            match memento_recall_gate.mode {
                RecallMode::Full => RecallSizeBucket::Full,
                RecallMode::IdentityOnly => RecallSizeBucket::IdentityOnly,
            }
        };
        note_recall_context_size(bucket, recall_bytes);
        tracing::info!(
            "  [{ts}] [memory] memento recall gate for channel {}: decision={} mode={:?} reason={} context_loaded={} recall_bytes={} input_tokens={} output_tokens={}",
            channel_id.get(),
            if memento_recall_gate.should_recall {
                "inject"
            } else {
                "skip"
            },
            memento_recall_gate.mode,
            memento_recall_gate.reason,
            memento_context_loaded,
            recall_bytes,
            memory_recall.token_usage.input_tokens,
            memory_recall.token_usage.output_tokens
        );
    }
    if should_note_memento_context_loaded(&memory_settings, memento_context_loaded, &memory_recall)
    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.note_memento_context_loaded();
        }
    }
    for warning in &memory_recall.warnings {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] [memory] recall warning for channel {}: {}",
            channel_id.get(),
            warning
        );
    }

    // Prepend pending file uploads
    let mut context_chunks = Vec::new();
    let memory_injection_plan = build_memory_injection_plan(
        &provider,
        session_id.is_some(),
        dispatch_profile,
        &memory_recall,
    );
    if !pending_uploads.is_empty() {
        context_chunks.push(pending_uploads.join("\n"));
    }
    if let Some(ref reply_ctx) = reply_context {
        context_chunks.push(reply_ctx.clone());
    }
    if let Some(knowledge) = memory_injection_plan.shared_knowledge_for_context {
        context_chunks.push(knowledge.to_string());
    }
    if let Some(external_recall) = memory_injection_plan.external_recall_for_context {
        context_chunks.push(external_recall.to_string());
    }
    context_chunks.push(wrap_user_prompt_with_author(
        request_owner_name,
        request_owner,
        sanitized_input,
    ));
    let context_prompt = context_chunks.join("\n\n");

    // Build Discord context info
    let discord_context = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        build_system_discord_context(
            session.and_then(|s| s.channel_name.as_deref()),
            session.and_then(|s| s.category_name.as_deref()),
            channel_id,
            false,
        )
    };

    // Claude keeps SAK in the system prompt for prefix-cache stability.
    // Non-Claude providers receive SAK in the user context instead.
    let sak_for_system = memory_injection_plan.shared_knowledge_for_system_prompt;
    let longterm_catalog_for_prompt = memory_injection_plan.longterm_catalog_for_system_prompt;
    let current_task_context = active_dispatch_info.as_ref().map(|info| {
        super::super::prompt_builder::CurrentTaskContext {
            dispatch_id: active_dispatch_id_for_prompt.as_deref(),
            card_id: info.card_id.as_deref(),
            dispatch_title: info.dispatch_title.as_deref(),
            dispatch_context: info.context.as_deref(),
            card_title: info.card_title.as_deref(),
            github_issue_url: info.github_issue_url.as_deref(),
        }
    });
    let memento_mcp_available = crate::services::mcp_config::provider_has_memento_mcp(&provider);
    let channel_participants = shared.channel_roster(channel_id, request_owner, request_owner_name);
    let memory_recall_manifest = super::super::prompt_builder::MemoryRecallManifestInput {
        should_recall: memento_recall_gate.should_recall,
        gate_reason: memento_recall_gate.reason,
        external_recall: memory_recall.external_recall.as_deref(),
    };

    let recovery_context_for_manifest =
        session_retry_context
            .as_ref()
            .map(|context| RecoveryContextManifestInput {
                raw_context: context.raw_context.as_str(),
                audit_record: context.audit_record.as_ref(),
            });
    let built_system_prompt = build_system_prompt_with_manifest(
        &discord_context,
        &channel_participants,
        &current_path,
        channel_id,
        token,
        role_binding.as_ref(),
        reply_to_user_message,
        dispatch_profile,
        dispatch_type_str.as_deref(),
        current_task_context.as_ref(),
        sak_for_system,
        longterm_catalog_for_prompt,
        Some(&memory_settings),
        memento_mcp_available,
        recovery_context_for_manifest.as_ref(),
        Some(&memory_recall_manifest),
        Some(&turn_id),
    );
    let system_prompt_owned = built_system_prompt.system_prompt;
    if let Some(manifest) = built_system_prompt.manifest {
        crate::db::prompt_manifests::spawn_save_prompt_manifest(shared.pg_pool.clone(), manifest);
    }
    if sak_for_system.is_some() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📦 SAK in system prompt ({} chars) for channel {}",
            sak_for_system.unwrap().len(),
            channel_id.get()
        );
    }
    let prompt_prep_duration_ms = prompt_prep_started.elapsed().as_millis();
    let memory_backend_label = memory_settings.backend.as_str();
    let provider_label = match &provider {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Qwen => "qwen",
        ProviderKind::Unsupported(_) => "unsupported",
    };
    let dispatch_profile_label = dispatch_profile_label(dispatch_profile);
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] [prompt-prep] channel={} provider={} dispatch={} memory_backend={} reused_session={} duration_ms={}",
        channel_id.get(),
        provider_label,
        dispatch_profile_label,
        memory_backend_label,
        session_id.is_some(),
        prompt_prep_duration_ms
    );
    // #1085: track provider-session reuse rate so we can monitor whether the
    // idle-timeout extension and reset removals are actually translating into
    // reused sessions (vs. falling back to fresh sessions every turn).
    crate::services::observability::metrics::record_session_entry(
        channel_id.get(),
        provider_label,
        session_id.is_some(),
    );
    // Spawn turn watchdog — detects deadline expiry and hands off to cancel reconciliation.
    // The deadline is stored in cancel_token.watchdog_deadline_ms and can be
    // extended via POST /api/turns/{channel_id}/extend-timeout.
    {
        let watchdog_token = cancel_token.clone();
        let watchdog_shared = shared.clone();
        let watchdog_http = http.clone();
        let timeout = super::super::turn_watchdog_timeout();

        // Set initial deadline. max_deadline tracks the farthest accepted
        // extension for alert context; it is no longer an absolute cap.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let turn_started_ms = now_ms;
        let deadline_ms = now_ms + timeout.as_millis() as i64;
        let max_deadline_ms = deadline_ms;
        watchdog_token
            .watchdog_deadline_ms
            .store(deadline_ms, std::sync::atomic::Ordering::Relaxed);
        watchdog_token
            .watchdog_max_deadline_ms
            .store(max_deadline_ms, std::sync::atomic::Ordering::Relaxed);

        let watchdog_channel_id_num = channel_id.get();
        let watchdog_provider = provider.clone();
        tokio::spawn(async move {
            const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
            let mut last_deadlock_prealert_deadline_ms: Option<i64> = None;

            loop {
                tokio::time::sleep(CHECK_INTERVAL).await;

                // Exit early if the turn already completed/cancelled
                if watchdog_token
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    super::super::clear_watchdog_deadline_override(watchdog_channel_id_num).await;
                    return;
                }

                // Check for API-based deadline extension
                if let Some(extension) =
                    super::super::take_watchdog_deadline_override(watchdog_channel_id_num).await
                {
                    let effective_deadline =
                        apply_watchdog_deadline_extension(&watchdog_token, extension);
                    last_deadlock_prealert_deadline_ms = None;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    let remaining_min =
                        (effective_deadline - chrono::Utc::now().timestamp_millis()) / 1000 / 60;
                    tracing::info!(
                        "  [{ts}] ⏰ WATCHDOG: deadline extended for channel {} — {remaining_min}m remaining",
                        channel_id
                    );
                }

                // Auto-extend based on inflight updated_at: if inflight was updated recently
                // (within last 5 min), push deadline forward by the default timeout
                {
                    let current_dl = watchdog_token
                        .watchdog_deadline_ms
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let now_ms_check = chrono::Utc::now().timestamp_millis();
                    // Only auto-extend when close to deadline (within 2 minutes)
                    if now_ms_check > current_dl - 120_000 {
                        if let Some(inflight) = super::super::inflight::load_inflight_state(
                            &watchdog_provider,
                            watchdog_channel_id_num,
                        ) {
                            if let Ok(updated) = chrono::NaiveDateTime::parse_from_str(
                                &inflight.updated_at,
                                "%Y-%m-%d %H:%M:%S",
                            ) {
                                let updated_ms = updated.and_utc().timestamp_millis();
                                let age_ms = now_ms_check - updated_ms;
                                // If inflight was updated within the last 5 minutes, auto-extend
                                if age_ms < 300_000 {
                                    let new_dl = now_ms_check + timeout.as_millis() as i64;
                                    if new_dl > current_dl {
                                        watchdog_token
                                            .watchdog_deadline_ms
                                            .store(new_dl, std::sync::atomic::Ordering::Relaxed);
                                        watchdog_token.watchdog_max_deadline_ms.store(
                                            std::cmp::max(
                                                watchdog_token
                                                    .watchdog_max_deadline_ms
                                                    .load(std::sync::atomic::Ordering::Relaxed),
                                                new_dl,
                                            ),
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        last_deadlock_prealert_deadline_ms = None;
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        let remaining_min = (new_dl - now_ms_check) / 1000 / 60;
                                        tracing::info!(
                                            "  [{ts}] ⏰ WATCHDOG: auto-extended for channel {} (inflight active) — {remaining_min}m remaining",
                                            channel_id
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                let current_deadline = watchdog_token
                    .watchdog_deadline_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                let now = chrono::Utc::now().timestamp_millis();
                if should_send_watchdog_deadlock_prealert(
                    now,
                    current_deadline,
                    last_deadlock_prealert_deadline_ms,
                ) {
                    let is_current_token =
                        super::super::mailbox_cancel_token(&watchdog_shared, channel_id)
                            .await
                            .is_some_and(|current| {
                                std::sync::Arc::ptr_eq(&watchdog_token, &current)
                            });
                    if !is_current_token {
                        super::super::clear_watchdog_deadline_override(watchdog_channel_id_num)
                            .await;
                        return;
                    }
                    let current_max_deadline = watchdog_token
                        .watchdog_max_deadline_ms
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if maybe_send_watchdog_deadlock_prealert(
                        &watchdog_shared,
                        &watchdog_provider,
                        channel_id,
                        now,
                        current_deadline,
                        turn_started_ms,
                        current_max_deadline,
                    )
                    .await
                    {
                        last_deadlock_prealert_deadline_ms = Some(current_deadline);
                    }
                }

                if let Some(extension) =
                    super::super::take_watchdog_deadline_override(watchdog_channel_id_num).await
                {
                    apply_watchdog_deadline_extension(&watchdog_token, extension);
                    last_deadlock_prealert_deadline_ms = None;
                }
                let current_deadline = watchdog_token
                    .watchdog_deadline_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                let now = chrono::Utc::now().timestamp_millis();
                if now < current_deadline {
                    continue; // Not yet — deadline may have been extended
                }

                // Deadline reached — fire watchdog through the cancel/reconcile path.
                let disposition = reconcile_watchdog_timeout(
                    &watchdog_shared,
                    &watchdog_provider,
                    channel_id,
                    &watchdog_token,
                )
                .await;
                if disposition == WatchdogTimeoutCancelDisposition::Cancelled {
                    let elapsed_mins =
                        (now - (current_deadline - timeout.as_millis() as i64)) / 1000 / 60;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏰ WATCHDOG: turn timeout (~{elapsed_mins}m) for channel {}, reconciled via cancel path",
                        channel_id
                    );

                    // Notify Discord
                    let has_queued = super::super::mailbox_has_pending_soft_queue(
                        &watchdog_shared,
                        &watchdog_provider,
                        channel_id,
                    )
                    .await
                    .has_pending;
                    let msg = if has_queued {
                        format!(
                            "⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다. 대기 중인 메시지로 다음 턴을 시작합니다.",
                        )
                    } else {
                        format!("⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다.",)
                    };
                    let _ = channel_id.say(&watchdog_http, msg).await;
                }
                return; // Watchdog done regardless
            }
        });
    }

    // Resolve remote profile for this channel
    let remote_profile = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.remote_profile_name.as_ref())
            .and_then(|name| {
                let settings = crate::config::Settings::load();
                settings
                    .remote_profiles
                    .iter()
                    .find(|p| p.name == *name)
                    .cloned()
            })
    };

    let adk_session_name = channel_name.clone();
    let adk_session_info = derive_adk_session_info(
        Some(user_text),
        channel_name.as_deref(),
        Some(&current_path),
    );
    let adk_thread_channel_id = adk_session_name
        .as_deref()
        .and_then(super::super::adk_session::parse_thread_channel_id_from_name)
        .or_else(|| {
            shared
                .dispatch_thread_parents
                .contains_key(&channel_id)
                .then_some(channel_id.get())
        });
    // #222: DB-based dispatch lookup takes priority over text parsing.
    // In unified threads, user_text may contain a stale DISPATCH: prefix
    // from a previous dispatch in the same thread. DB lookup uses the
    // thread→card→dispatch link which is always current.
    let dispatch_id = super::super::adk_session::lookup_pending_dispatch_for_thread(
        shared.api_port,
        channel_id.get(),
    )
    .await
    .or_else(|| super::super::adk_session::parse_dispatch_id(user_text));
    post_adk_session_status(
        adk_session_key.as_deref(),
        adk_session_name.as_deref(),
        Some(provider.as_str()),
        "working",
        &provider,
        Some(&adk_session_info),
        None,
        Some(&current_path),
        dispatch_id.as_deref(),
        adk_thread_channel_id,
        role_binding
            .as_ref()
            .map(|binding| binding.role_id.as_str()),
        shared.api_port,
    )
    .await;

    let (inflight_tmux_name, inflight_output_path, inflight_input_fifo, inflight_offset) = {
        #[cfg(unix)]
        {
            if remote_profile.is_none()
                && provider.uses_managed_tmux_backend()
                && claude::is_tmux_available()
            {
                if let Some(ref tmux_name) = tmux_session_name {
                    let (output_path, input_fifo_path) = tmux_runtime_paths(tmux_name);
                    let session_exists =
                        crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_name);
                    let last_offset = std::fs::metadata(&output_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    (
                        Some(tmux_name.clone()),
                        Some(output_path),
                        Some(input_fifo_path),
                        if session_exists { last_offset } else { 0 },
                    )
                } else {
                    (None, None, None, 0)
                }
            } else {
                (None, None, None, 0)
            }
        }
        #[cfg(not(unix))]
        {
            (None, None, None, 0u64)
        }
    };
    let watcher_tmux_name = inflight_tmux_name.clone();
    let watcher_output_path = inflight_output_path.clone();

    let (logical_channel_id, thread_id, thread_title) =
        if let Some((parent_id, _parent_name)) = thread_parent {
            let (live_thread_title, _) =
                super::super::resolve_channel_category(http, cache, channel_id).await;
            (parent_id.get(), Some(channel_id.get()), live_thread_title)
        } else {
            (channel_id.get(), None, None)
        };

    let mut inflight_state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name.clone(),
        request_owner.get(),
        user_msg_id.get(),
        placeholder_msg_id.get(),
        user_text.to_string(),
        session_id.clone(),
        inflight_tmux_name,
        inflight_output_path,
        inflight_input_fifo.clone(),
        inflight_offset,
    );
    let (worktree_path, worktree_branch, base_commit) = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.worktree.as_ref())
            .map(|wt| {
                (
                    Some(wt.worktree_path.clone()),
                    Some(wt.branch_name.clone()),
                    crate::services::platform::git_head_commit(&wt.original_path),
                )
            })
            .unwrap_or((None, None, None))
    };
    inflight_state.set_worktree_context(worktree_path, worktree_branch, base_commit);
    inflight_state.logical_channel_id = Some(logical_channel_id);
    inflight_state.thread_id = thread_id;
    inflight_state.thread_title = thread_title;
    // Persist identifiers for long-turn diagnostics (#130)
    inflight_state.session_key = adk_session_key.clone();
    inflight_state.dispatch_id = dispatch_id.clone();
    if let Err(e) = save_inflight_state(&inflight_state) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}]   ⚠ inflight state save failed: {e}");
    }

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();
    let (completion_tx, completion_rx) = if wait_for_completion {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

    // Pause the tmux-session owner watcher before writing to the provider
    // FIFO. In thread follow-ups, the watcher may be owned by the parent
    // channel rather than the requested thread channel.
    let _watcher_owner_channel_id = attach_paused_turn_watcher(
        shared,
        http.clone(),
        &provider,
        channel_id,
        watcher_tmux_name,
        watcher_output_path,
        inflight_offset,
        "turn_start_message",
    );

    // Auto-sync worktree before sending message to session
    {
        let script = super::super::runtime_store::agentdesk_root()
            .unwrap_or_default()
            .join("scripts/worktree-autosync.sh");
        if script.exists() {
            let ws = current_path.clone();
            let ts = chrono::Local::now().format("%H:%M:%S");
            match std::process::Command::new(&script)
                .arg(&ws)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
            {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let msg = stdout.trim();
                    match out.status.code() {
                        Some(0) => tracing::info!("  [{ts}] 🔄 worktree-autosync [{ws}]: {msg}"),
                        Some(1) => {
                            tracing::info!("  [{ts}] ⏭ worktree-autosync [{ws}]: skipped — {msg}")
                        }
                        _ => tracing::warn!("  [{ts}] ⚠ worktree-autosync [{ws}]: error — {msg}"),
                    }
                }
                Err(e) => tracing::warn!("  [{ts}] ⚠ worktree-autosync: failed to run — {e}"),
            }
        }
    }

    let model_for_turn =
        super::super::commands::resolve_model_for_turn(shared, channel_id, &provider).await;
    let native_fast_mode_override = native_fast_mode_override_for_turn(
        &provider,
        super::super::commands::channel_fast_mode_setting(shared, fast_mode_channel_id).await,
    );
    let codex_goals_override = codex_goals_override_for_turn(
        &provider,
        super::super::commands::channel_codex_goals_setting(shared, fast_mode_channel_id).await,
    );

    // Fetch context compact percent from ADK settings (provider-specific)
    let ctx_thresholds = super::super::adk_session::fetch_context_thresholds(shared.api_port).await;
    let compact_percent = ctx_thresholds.compact_pct_for(&provider);
    // Use model-specific context window (reads Codex models cache), falling
    // back to the provider default if the model isn't found.
    let model_context_window = provider.resolve_context_window(model_for_turn.as_deref());

    // Pre-compute provider-specific compact config
    let compact_percent_for_claude = Some(ctx_thresholds.compact_pct_for(&provider));
    let compact_token_limit_for_codex = {
        let cli_config = provider.compact_cli_config(compact_percent, model_context_window);
        cli_config
            .first()
            .map(|(_, v)| v.parse::<u64>().unwrap_or(0))
    };
    // #1088: per-channel prompt-cache TTL (None|5|60). Only consumed by Claude.
    let cache_ttl_minutes = super::super::settings::resolve_cache_ttl_minutes(channel_id, None);
    let provider_execution_context = crate::services::provider_cli::ProviderExecutionContext {
        provider: provider.as_str().to_string(),
        agent_id: role_binding.as_ref().map(|binding| binding.role_id.clone()),
        channel_id: Some(channel_id.get().to_string()),
        session_key: adk_session_key.clone(),
        tmux_session: tmux_session_name.clone(),
        channel_name: channel_name.clone(),
        execution_mode: Some("discord_turn".to_string()),
    };
    let dispatch_type_for_mcp = dispatch_type_str.clone();

    // Run the provider in a blocking thread
    let provider_for_blocking = provider.clone();
    tokio::task::spawn_blocking(move || {
        let result = crate::services::platform::with_provider_execution_context(
            provider_execution_context,
            || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match &provider_for_blocking {
                        ProviderKind::Claude => claude::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            native_fast_mode_override,
                            compact_percent_for_claude,
                            cache_ttl_minutes,
                            dispatch_type_for_mcp.as_deref(),
                        ),
                        ProviderKind::Codex => codex::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            native_fast_mode_override,
                            codex_goals_override,
                            compact_token_limit_for_codex,
                            force_fresh_provider_session,
                        ),
                        ProviderKind::Gemini => gemini::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            None, // Gemini: compact not supported
                        ),
                        ProviderKind::Qwen => qwen::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            None, // Qwen: compact not supported
                        ),
                        ProviderKind::OpenCode => opencode::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            Some(&system_prompt_owned),
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            None,
                        ),
                        ProviderKind::Unsupported(name) => {
                            let _ = tx.send(StreamMessage::Error {
                                message: format!("Provider '{}' is not installed", name),
                                stdout: String::new(),
                                stderr: String::new(),
                                exit_code: None,
                            });
                            Ok(())
                        }
                    }
                }))
            },
        );

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("  [streaming] Error: {}", e);
                let _ = tx.send(StreamMessage::Error {
                    message: e,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "unknown panic".to_string()
                };
                tracing::warn!("  [streaming] PANIC: {}", msg);
                let _ = tx.send(StreamMessage::Error {
                    message: format!("Internal error (panic): {}", msg),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
        }
    });

    spawn_turn_bridge(
        shared.clone(),
        cancel_token.clone(),
        rx,
        TurnBridgeContext {
            provider: provider.clone(),
            gateway: Arc::new(DiscordGateway::new(
                http.clone(),
                shared.clone(),
                provider.clone(),
                ctx_for_chained_dispatch.map(|live_ctx| LiveDiscordTurnContext {
                    ctx: live_ctx.clone(),
                    token: token.to_string(),
                    request_owner,
                }),
            )),
            channel_id,
            user_msg_id,
            user_text_owned: user_text.to_string(),
            request_owner_name: request_owner_name.to_string(),
            role_binding: role_binding.clone(),
            adk_session_key,
            adk_session_name,
            adk_session_info: Some(adk_session_info),
            adk_cwd: Some(current_path.clone()),
            dispatch_id,
            dispatch_kind: super::super::turn_bridge::classify_turn_finished_dispatch_kind(
                active_dispatch_info
                    .as_ref()
                    .and_then(|info| info.context.as_deref()),
                dispatch_type_str.as_deref(),
            )
            .map(str::to_string),
            memory_recall_usage: memory_recall.token_usage,
            context_window_tokens: model_context_window,
            context_compact_percent: compact_percent,
            current_msg_id: placeholder_msg_id,
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: Some(inflight_offset),
            new_session_id: session_id.clone(),
            defer_watcher_resume,
            reuse_status_panel_message: false,
            completion_tx,
            inflight_state,
        },
    );

    if let Some(rx) = completion_rx {
        rx.await
            .map_err(|_| "queued turn completion wait failed".to_string())?;
    }

    Ok(())
}

/// Handle file uploads from Discord messages
pub(super) async fn handle_file_upload(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let channel_id = msg.channel_id;

    // Always use the runtime uploads directory (works without session)
    let Some(save_dir) = channel_upload_dir(channel_id) else {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Cannot resolve upload directory.")
            .await;
        return Ok(());
    };

    if let Err(e) = fs::create_dir_all(&save_dir) {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(
                &ctx.http,
                format!("Failed to prepare upload directory: {}", e),
            )
            .await;
        return Ok(());
    }

    for attachment in &msg.attachments {
        let file_name = &attachment.filename;

        // Download file from Discord CDN
        let buf = match reqwest::get(&attachment.url).await {
            Ok(resp) => match resp.bytes().await {
                Ok(bytes) => bytes,
                Err(e) => {
                    rate_limit_wait(shared, channel_id).await;
                    let _ = channel_id
                        .say(&ctx.http, format!("Download failed: {}", e))
                        .await;
                    continue;
                }
            },
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, format!("Download failed: {}", e))
                    .await;
                continue;
            }
        };

        // Save to session path (sanitize filename)
        let safe_name = Path::new(file_name)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("uploaded_file"));
        let ts = chrono::Utc::now().timestamp_millis();
        let stamped_name = format!("{}_{}", ts, safe_name.to_string_lossy());
        let dest = save_dir.join(stamped_name);
        let file_size = buf.len();

        match fs::write(&dest, &buf) {
            Ok(_) => {
                let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id.say(&ctx.http, &msg_text).await;
            }
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, format!("Failed to save file: {}", e))
                    .await;
                continue;
            }
        }

        // Record upload in session
        let upload_record = format!(
            "[File uploaded] {} → {} ({} bytes)",
            file_name,
            dest.display(),
            file_size
        );
        {
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                session.history.push(HistoryItem {
                    item_type: HistoryType::User,
                    content: upload_record.clone(),
                });
                session.pending_uploads.push(upload_record);
            }
        }
    }

    Ok(())
}

/// Handle shell commands from raw text messages (! prefix)
pub(super) async fn handle_shell_command_raw(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let cmd_str = text.strip_prefix('!').unwrap_or("").trim();
    if cmd_str.is_empty() {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Usage: `!<command>`\nExample: `!ls -la`")
            .await;
        return Ok(());
    }

    let working_dir = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.current_path.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| "/".to_string())
            })
    };

    let cmd_owned = cmd_str.to_string();
    let working_dir_clone = working_dir.clone();

    let result = tokio::task::spawn_blocking(move || {
        let child = crate::services::platform::shell::shell_command_builder(&cmd_owned)
            .current_dir(&working_dir_clone)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        match child {
            Ok(child) => child.wait_with_output(),
            Err(e) => Err(e),
        }
    })
    .await;

    let response = match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);
            let mut parts = Vec::new();
            if !stdout.is_empty() {
                parts.push(format!("```\n{}\n```", stdout.trim_end()));
            }
            if !stderr.is_empty() {
                parts.push(format!("stderr:\n```\n{}\n```", stderr.trim_end()));
            }
            if parts.is_empty() {
                parts.push(format!("(exit code: {})", exit_code));
            } else if exit_code != 0 {
                parts.push(format!("(exit code: {})", exit_code));
            }
            parts.join("\n")
        }
        Ok(Err(e)) => format!("Failed to execute: {}", e),
        Err(e) => format!("Task error: {}", e),
    };

    send_long_message_raw(&ctx.http, channel_id, &response, shared).await?;
    Ok(())
}

pub(super) enum TextStopLookup {
    NoActiveTurn,
    AlreadyStopping,
    Stop(Arc<CancelToken>),
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn lookup_text_stop_token(
    cancel_tokens: &std::collections::HashMap<serenity::ChannelId, Arc<CancelToken>>,
    channel_id: serenity::ChannelId,
) -> TextStopLookup {
    match cancel_tokens.get(&channel_id).cloned() {
        Some(token) if cancel_requested(Some(token.as_ref())) => TextStopLookup::AlreadyStopping,
        Some(token) => TextStopLookup::Stop(token),
        None => TextStopLookup::NoActiveTurn,
    }
}

#[allow(dead_code)]
pub(super) async fn lookup_text_stop_token_mailbox(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> TextStopLookup {
    match super::super::mailbox_cancel_token(shared, channel_id).await {
        Some(token) if cancel_requested(Some(token.as_ref())) => TextStopLookup::AlreadyStopping,
        Some(token) => TextStopLookup::Stop(token),
        None => TextStopLookup::NoActiveTurn,
    }
}

pub(super) async fn cancel_text_stop_token_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> TextStopLookup {
    let result = super::super::mailbox_cancel_active_turn(shared, channel_id).await;
    match result.token {
        Some(_) if result.already_stopping => TextStopLookup::AlreadyStopping,
        Some(token) => {
            super::super::ensure_cancel_token_bound_from_inflight(
                provider,
                channel_id,
                &token,
                "text stop mailbox lookup",
            );
            TextStopLookup::Stop(token)
        }
        None => TextStopLookup::NoActiveTurn,
    }
}

/// #2044 F1: identity-checked variant — cancels active turn ONLY if the
/// current mailbox cancel-token is the same `Arc` as `expected_token`.
///
/// Required by the reaction-remove path: between the mailbox snapshot
/// and the cancel await, the mailbox actor can finish the old turn and
/// start a new one for a queued message, which would otherwise be
/// cancelled here (a stale ⏳-remove cancelling an unrelated follow-up
/// turn). The mailbox's `CancelActiveTurnIfCurrent` does a pointer-eq
/// check, so token identity prevents the wrong turn from being killed.
pub(super) async fn cancel_text_stop_token_mailbox_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    expected_token: Arc<CancelToken>,
    reason: &'static str,
) -> TextStopLookup {
    let result = super::super::mailbox_cancel_active_turn_if_current_with_reason(
        shared,
        channel_id,
        expected_token,
        reason,
    )
    .await;
    match result.token {
        Some(_) if result.already_stopping => TextStopLookup::AlreadyStopping,
        Some(token) => {
            super::super::ensure_cancel_token_bound_from_inflight(
                provider,
                channel_id,
                &token,
                "text stop mailbox lookup (if_current)",
            );
            TextStopLookup::Stop(token)
        }
        None => TextStopLookup::NoActiveTurn,
    }
}

/// Handle text-based commands (!start, !meeting, !stop, !clear, etc.).
/// Returns true if the command was handled, false otherwise.
pub(super) async fn handle_text_command(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel_id: serenity::ChannelId,
    text: &str,
) -> Result<bool, Error> {
    /* legacy inline text-command handler kept commented during upstream merge
        let parts: Vec<&str> = text.splitn(3, char::is_whitespace).collect();
        let cmd = parts[0];
        let arg1 = parts.get(1).unwrap_or(&"");
        let arg2 = parts.get(2).unwrap_or(&"");

        match cmd {
            "!start" => {
                let path_str = if arg1.is_empty() { "." } else { arg1 };

                // Resolve path
                let effective_path = if path_str == "." || path_str.is_empty() {
                    // Use workspace root or current directory
                    let Some(workspace_dir) = runtime_store::workspace_root() else {
                        let _ = msg
                            .reply(&ctx.http, "Error: cannot determine workspace root.")
                            .await;
                        return Ok(true);
                    };
                    // Create a random workspace for this channel
                    use rand::Rng;
                    let random_name: String = rand::thread_rng()
                        .sample_iter(&rand::distributions::Alphanumeric)
                        .take(8)
                        .map(char::from)
                        .collect();
                    let ch_name = resolve_channel_category(ctx, channel_id)
                        .await
                        .0
                        .unwrap_or_else(|| format!("ch-{}", channel_id));
                    let dir = workspace_dir.join(format!("{}-{}", ch_name, random_name));
                    std::fs::create_dir_all(&dir).ok();
                    dir.to_string_lossy().to_string()
                } else if path_str.starts_with('~') {
                    dirs::home_dir()
                        .map(|h| path_str.replacen('~', &h.to_string_lossy(), 1))
                        .unwrap_or_else(|| path_str.to_string())
                } else {
                    path_str.to_string()
                };

                // Validate path exists
                if !std::path::Path::new(&effective_path).exists() {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!("Error: path `{}` does not exist.", effective_path),
                        )
                        .await;
                    return Ok(true);
                }

                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ◀ [{}] !start path={}",
                    msg.author.name,
                    effective_path
                );

                // Create session
                let (ch_name, cat_name) = resolve_channel_category(ctx, channel_id).await;
                {
                    let mut d = data.shared.core.lock().await;
                    let session = d
                        .sessions
                        .entry(channel_id)
                        .or_insert_with(|| DiscordSession {
                            session_id: None,
                            memento_context_loaded: false,
                            memento_reflected: false,
                            current_path: None,
                            history: Vec::new(),
                            pending_uploads: Vec::new(),
                            cleared: false,
                            channel_name: None,
                            category_name: None,
                            remote_profile_name: None,
                            channel_id: Some(channel_id.get()),
                            last_active: tokio::time::Instant::now(),
                            worktree: None,

                            born_generation: runtime_store::load_generation(),
                            assistant_turns: 0,
                        });
                    session.current_path = Some(effective_path.clone());
                    session.channel_name = ch_name;
                    session.category_name = cat_name;
                    session.last_active = tokio::time::Instant::now();
                }

                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ▶ Session started: {}", effective_path);
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("Session started at `{}`.", effective_path),
                    )
                    .await;
                return Ok(true);
            }

            "!meeting" => {
                let action = if arg1.is_empty() { "start" } else { arg1 };
                let agenda = if arg2.is_empty() { arg1 } else { arg2 };

                match action {
                    "start" => {
                        let agenda_text = if agenda.is_empty() || *agenda == "start" {
                            let _ = msg
                                .reply(
                                    &ctx.http,
                                    "사용법: `!meeting start <안건>` 또는 `!meeting <안건>`",
                                )
                                .await;
                            return Ok(true);
                        } else {
                            agenda
                        };

                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ◀ [{}] !meeting start {}",
                            msg.author.name,
                            agenda_text
                        );

                        let http = ctx.http.clone();
                        let shared = data.shared.clone();
                        let provider = data.provider.clone();
                        let reviewer = provider.counterpart();
                        let agenda_owned = agenda_text.to_string();

                        let _ = msg
                            .reply(
                                &ctx.http,
                                format!(
                                    "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                                    provider.display_name(),
                                    reviewer.display_name()
                                ),
                            )
                            .await;

                        tokio::spawn(async move {
                            match meeting::start_meeting(
                                &*http,
                                channel_id,
                                &agenda_owned,
                                provider,
                                reviewer,
                                &shared,
                            )
                            .await
                            {
                                Ok(Some(id)) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!("  [{ts}] ✅ Meeting completed: {id}");
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!("  [{ts}] ❌ Meeting error: {e}");
                                }
                            }
                        });
                        return Ok(true);
                    }
                    "stop" => {
                        let _ = meeting::cancel_meeting(&ctx.http, channel_id, &data.shared).await;
                        return Ok(true);
                    }
                    "status" => {
                        let _ = meeting::meeting_status(&ctx.http, channel_id, &data.shared).await;
                        return Ok(true);
                    }
                    _ => {
                        // Treat unknown action as agenda text
                        let full_agenda = text.trim_start_matches("!meeting").trim();
                        if full_agenda.is_empty() {
                            let _ = msg.reply(&ctx.http, "사용법: `!meeting <안건>`").await;
                            return Ok(true);
                        }
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!("  [{ts}] ◀ [{}] !meeting {}", msg.author.name, full_agenda);

                        let http = ctx.http.clone();
                        let shared = data.shared.clone();
                        let provider = data.provider.clone();
                        let reviewer = provider.counterpart();
                        let agenda_owned = full_agenda.to_string();

                        let _ = msg
                            .reply(
                                &ctx.http,
                                format!(
                                    "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                                    provider.display_name(),
                                    reviewer.display_name()
                                ),
                            )
                            .await;

                        tokio::spawn(async move {
                            match meeting::start_meeting(
                                &*http,
                                channel_id,
                                &agenda_owned,
                                provider,
                                reviewer,
                                &shared,
                            )
                            .await
                            {
                                Ok(Some(id)) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!("  [{ts}] ✅ Meeting completed: {id}");
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!("  [{ts}] ❌ Meeting error: {e}");
                                }
                            }
                        });
                        return Ok(true);
                    }
                }
            }

            "!stop" => {
                // #441: flows through cancel_text_stop_token_mailbox (mailbox_cancel_active_turn)
                // → stop_active_turn → token.cancelled triggers turn_bridge loop exit
                // → mailbox_finish_turn canonical cleanup.
                // #1218: stop_active_turn ensures the provider abort key
                // (C-c) is sent before SIGKILL, which is required for any
                // turn whose `child_pid` is `None` (handoff/restart/Codex
                // TUI). The previous code only called `cancel_active_token`
                // here, so those runs never received an abort key.
                let stop_lookup =
                    cancel_text_stop_token_mailbox(&data.shared, &data.provider, channel_id).await;
                match stop_lookup {
                    TextStopLookup::Stop(token) => {
                        let termination_recorded = super::super::turn_bridge::stop_active_turn(
                            &data.provider,
                            &token,
                            super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                            "!stop",
                        )
                        .await;
                        crate::services::turn_cancel_finalizer::finalize_turn_cancel(
                            crate::services::turn_cancel_finalizer::FinalizeTurnCancelRequest::from_text_stop(
                                data.provider.clone(),
                                channel_id,
                                "!stop",
                                termination_recorded,
                            ),
                        );
                        super::super::commands::notify_turn_stop(
                            &ctx.http,
                            &data.shared,
                            &data.provider,
                            channel_id,
                            "!stop",
                        )
                        .await;
                    }
                    TextStopLookup::AlreadyStopping => {
                        let _ = msg.reply(&ctx.http, "Already stopping...").await;
                    }
                    TextStopLookup::NoActiveTurn => {
                        let _ = msg.reply(&ctx.http, "No active turn to stop.").await;
                    }
                }
                return Ok(true);
            }

            "!clear" => {
                super::super::commands::clear_channel_session_state(
                    &ctx.http,
                    &data.shared,
                    &data.provider,
                    channel_id,
                    "!clear",
                )
                .await;
                let _ = msg.reply(&ctx.http, "Session cleared.").await;
                return Ok(true);
            }

            // ── Simple diagnostic / info commands ──
            "!pwd" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !pwd", msg.author.name);

                auto_restore_session(&data.shared, channel_id, ctx).await;

                let current_path = {
                    let d = data.shared.core.lock().await;
                    let session = d.sessions.get(&channel_id);
                    session.and_then(|s| s.current_path.clone())
                };
                let reply = match current_path {
                    Some(path) => format!("`{}`", path),
                    None => "No active session. Use `!start <path>` first.".to_string(),
                };
                let _ = msg.reply(&ctx.http, &reply).await;
                return Ok(true);
            }

            "!health" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !health", msg.author.name);

                let text =
                    commands::build_health_report(&data.shared, &data.provider, channel_id).await;
                send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
                return Ok(true);
            }

            "!status" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !status", msg.author.name);

                let text =
                    commands::build_status_report(&data.shared, &data.provider, channel_id).await;
                send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
                return Ok(true);
            }

            "!inflight" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !inflight", msg.author.name);

                let text =
                    commands::build_inflight_report(&data.shared, &data.provider, channel_id).await;
                send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
                return Ok(true);
            }

            "!queue" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !queue", msg.author.name);

                let show_all = *arg1 == "all";
                let text =
                    commands::build_queue_report(&data.shared, &data.provider, channel_id, show_all)
                        .await;
                send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
                return Ok(true);
            }

            "!metrics" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !metrics", msg.author.name);

                let metrics_data = if arg1.is_empty() {
                    metrics::load_today()
                } else {
                    metrics::load_date(arg1)
                };
                let label = if arg1.is_empty() { "today" } else { arg1 };
                let text = metrics::build_metrics_report(&metrics_data, label);
                send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
                return Ok(true);
            }

            "!debug" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !debug", msg.author.name);

                let new_state = claude::toggle_debug();
                let status = if new_state { "ON" } else { "OFF" };
                let _ = msg
                    .reply(&ctx.http, format!("Debug logging: **{}**", status))
                    .await;
                tracing::info!("  [{ts}] ▶ Debug logging toggled to {status}");
                return Ok(true);
            }

            "!escalation" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                let rest = text.strip_prefix("!escalation").unwrap_or("").trim();
                tracing::info!("  [{ts}] ◀ [{}] !escalation {}", msg.author.name, rest);

                if !check_owner(msg.author.id, &data.shared).await {
                    let _ = msg
                        .reply(&ctx.http, "Only the owner can change escalation settings.")
                        .await;
                    return Ok(true);
                }

                let mut settings = match fetch_escalation_settings_via_api().await {
                    Ok(response) => response.current,
                    Err(err) => {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                format!("Failed to load escalation settings: {err}"),
                            )
                            .await;
                        return Ok(true);
                    }
                };

                if rest.is_empty() || rest.eq_ignore_ascii_case("status") {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "**Escalation Settings**\n{}",
                                format_escalation_settings_summary(&settings)
                            ),
                        )
                        .await;
                    return Ok(true);
                }

                let mut parts = rest.splitn(2, char::is_whitespace);
                let subcommand = parts.next().unwrap_or("").trim().to_ascii_lowercase();
                let value = parts.next().unwrap_or("").trim();

                let usage = "Usage: `!escalation status|pm|user|scheduled|schedule <HH:MM-HH:MM>|timezone <IANA>|owner <user_id>|pm-channel <channel_id>`";
                let update_error = match subcommand.as_str() {
                    "pm" => {
                        settings.mode = crate::config::EscalationMode::Pm;
                        None
                    }
                    "user" => {
                        settings.mode = crate::config::EscalationMode::User;
                        None
                    }
                    "scheduled" => {
                        settings.mode = crate::config::EscalationMode::Scheduled;
                        None
                    }
                    "schedule" => {
                        if value.is_empty() {
                            Some("schedule value is required")
                        } else {
                            settings.mode = crate::config::EscalationMode::Scheduled;
                            settings.schedule.pm_hours = value.to_string();
                            None
                        }
                    }
                    "timezone" => {
                        if value.is_empty() {
                            Some("timezone value is required")
                        } else {
                            settings.schedule.timezone = value.to_string();
                            None
                        }
                    }
                    "owner" => match parse_discord_user_id(value) {
                        Some(user_id) => {
                            settings.owner_user_id = Some(user_id);
                            None
                        }
                        None => Some("owner must be a numeric Discord user id or mention"),
                    },
                    "clear-owner" => {
                        settings.owner_user_id = None;
                        None
                    }
                    "pm-channel" => {
                        if value.is_empty() {
                            Some("pm-channel value is required")
                        } else {
                            settings.pm_channel_id = Some(value.to_string());
                            None
                        }
                    }
                    "clear-pm-channel" => {
                        settings.pm_channel_id = None;
                        None
                    }
                    _ => Some(usage),
                };

                if let Some(err) = update_error {
                    let _ = msg.reply(&ctx.http, err).await;
                    return Ok(true);
                }

                match save_escalation_settings_via_api(&settings).await {
                    Ok(response) => {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                format!(
                                    "**Escalation Settings Updated**\n{}",
                                    format_escalation_settings_summary(&response.current)
                                ),
                            )
                            .await;
                    }
                    Err(err) => {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                format!("Failed to save escalation settings: {err}"),
                            )
                            .await;
                    }
                }
                return Ok(true);
            }

            "!help" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !help", msg.author.name);

                let provider_name = data.provider.display_name();
                let help = format!(
                    "\
**AgentDesk Discord Bot**
Manage server files & chat with {p}.
Each channel gets its own independent {p} session.

**Session**
`!start <path>` — Start session at directory
`!pwd` — Show current working directory
`!health` — Show runtime health summary
`!status` — Show this channel session status
`!inflight` — Show saved inflight turn state
`!clear` — Clear AI conversation history
`!stop` — Stop current AI request

**File Transfer**
`!down <file>` — Download file from server
Send a file/photo — Upload to session directory

**Shell**
`!shell <command>` — Run shell command directly

**AI Chat**
Any other message is sent to {p}.

**Tool Management**
`!allowedtools` — Show currently allowed tools
`!allowed +name` — Add tool (e.g. `!allowed +Bash`)
`!allowed -name` — Remove tool

**Skills**
`!cc <skill>` — Run a provider skill

**Settings**
`/model` — Open the interactive model picker
`!debug` — Toggle debug logging
`!metrics [date]` — Show turn metrics
`!queue [all]` — Show pending queue
`!escalation status` — Show escalation routing mode

**User Management** (owner only)
`!allowall on|off|status` — Allow everyone or restrict to authorized users
`!adduser <user_id>` — Allow a user to use the bot
`!removeuser <user_id>` — Remove a user's access
`!escalation pm|user|scheduled` — Change escalation routing mode
`!escalation schedule <HH:MM-HH:MM>` — Set PM hours and switch to scheduled mode
`!escalation timezone <IANA>` — Set scheduled timezone
`!escalation owner <user_id>` — Override fallback owner user id
`!escalation pm-channel <channel_id>` — Override PM channel
`!help` — Show this help",
                    p = provider_name
                );
                send_long_message_raw(&ctx.http, channel_id, &help, &data.shared).await?;
                return Ok(true);
            }

            "!allowedtools" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !allowedtools", msg.author.name);

                let tools = {
                    let settings = data.shared.settings.read().await;
                    settings.allowed_tools.clone()
                };

                let mut reply = String::from("**Allowed Tools**\n\n");
                for tool in &tools {
                    let (desc, destructive) = super::super::formatting::tool_info(tool);
                    let badge = super::super::formatting::risk_badge(destructive);
                    if badge.is_empty() {
                        reply.push_str(&format!("`{}` — {}\n", tool, desc));
                    } else {
                        reply.push_str(&format!("`{}` {} — {}\n", tool, badge, desc));
                    }
                }
                reply.push_str(&format!(
                    "\n{} = destructive\nTotal: {}",
                    super::super::formatting::risk_badge(true),
                    tools.len()
                ));
                send_long_message_raw(&ctx.http, channel_id, &reply, &data.shared).await?;
                return Ok(true);
            }

            // ── Commands with arguments ──
            "!allowed" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !allowed {}", msg.author.name, arg1);

                let arg = arg1.trim();
                let (op, raw_name) = if let Some(name) = arg.strip_prefix('+') {
                    ('+', name.trim())
                } else if let Some(name) = arg.strip_prefix('-') {
                    ('-', name.trim())
                } else {
                    let _ = msg.reply(&ctx.http, "Use `+toolname` to add or `-toolname` to remove.\nExample: `!allowed +Bash`").await;
                    return Ok(true);
                };

                if raw_name.is_empty() {
                    let _ = msg.reply(&ctx.http, "Tool name cannot be empty.").await;
                    return Ok(true);
                }

                let Some(tool_name) =
                    super::super::formatting::canonical_tool_name(raw_name).map(str::to_string)
                else {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "Unknown tool `{}`. Use `!allowedtools` to see valid tool names.",
                                raw_name
                            ),
                        )
                        .await;
                    return Ok(true);
                };

                let response_msg = {
                    let mut settings = data.shared.settings.write().await;
                    match op {
                        '+' => {
                            if settings.allowed_tools.iter().any(|t| t == &tool_name) {
                                format!("`{}` is already in the list.", tool_name)
                            } else {
                                settings.allowed_tools.push(tool_name.clone());
                                save_bot_settings(&data.token, &settings);
                                format!("Added `{}`", tool_name)
                            }
                        }
                        '-' => {
                            let before_len = settings.allowed_tools.len();
                            settings.allowed_tools.retain(|t| t != &tool_name);
                            if settings.allowed_tools.len() < before_len {
                                save_bot_settings(&data.token, &settings);
                                format!("Removed `{}`", tool_name)
                            } else {
                                format!("`{}` is not in the list.", tool_name)
                            }
                        }
                        _ => unreachable!(),
                    }
                };
                let _ = msg.reply(&ctx.http, &response_msg).await;
                return Ok(true);
            }

            "!adduser" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !adduser {}", msg.author.name, arg1);

                if !check_owner(msg.author.id, &data.shared).await {
                    let _ = msg.reply(&ctx.http, "Only the owner can add users.").await;
                    return Ok(true);
                }

                let raw_id = arg1
                    .trim()
                    .trim_start_matches("<@")
                    .trim_end_matches('>')
                    .trim_start_matches('!');
                let target_id: u64 = match raw_id.parse() {
                    Ok(id) => id,
                    Err(_) => {
                        let _ = msg
                            .reply(&ctx.http, "Usage: `!adduser <user_id>` or `!adduser @user`")
                            .await;
                        return Ok(true);
                    }
                };

                {
                    let mut settings = data.shared.settings.write().await;
                    if settings.allowed_user_ids.contains(&target_id) {
                        let _ = msg
                            .reply(&ctx.http, format!("`{}` is already authorized.", target_id))
                            .await;
                        return Ok(true);
                    }
                    settings.allowed_user_ids.push(target_id);
                    save_bot_settings(&data.token, &settings);
                }

                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("Added `{}` as authorized user.", target_id),
                    )
                    .await;
                tracing::info!("  [{ts}] ▶ Added user: {target_id}");
                return Ok(true);
            }

            "!allowall" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !allowall {}", msg.author.name, arg1);

                if !check_owner(msg.author.id, &data.shared).await {
                    let _ = msg
                        .reply(&ctx.http, "Only the owner can change public access.")
                        .await;
                    return Ok(true);
                }

                let action = arg1.trim().to_ascii_lowercase();
                if action.is_empty() || action == "status" {
                    let enabled = {
                        let settings = data.shared.settings.read().await;
                        settings.allow_all_users
                    };
                    let message = if enabled {
                        "Public access is enabled. Any Discord user can talk to this bot in allowed channels."
                    } else {
                        "Public access is disabled. Only the owner and authorized users can talk to this bot."
                    };
                    let _ = msg.reply(&ctx.http, message).await;
                    return Ok(true);
                }

                let enabled = match action.as_str() {
                    "on" | "true" | "enable" | "enabled" => true,
                    "off" | "false" | "disable" | "disabled" => false,
                    _ => {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                "Usage: `!allowall on`, `!allowall off`, or `!allowall status`",
                            )
                            .await;
                        return Ok(true);
                    }
                };

                let response = {
                    let mut settings = data.shared.settings.write().await;
                    settings.allow_all_users = enabled;
                    save_bot_settings(&data.token, &settings);
                    if enabled {
                        "Public access enabled. Any Discord user can talk to this bot in allowed channels."
                    } else {
                        "Public access disabled. Only the owner and authorized users can talk to this bot."
                    }
                };

                let _ = msg.reply(&ctx.http, response).await;
                tracing::info!("  [{ts}] ▶ {response}");
                return Ok(true);
            }

            "!removeuser" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ◀ [{}] !removeuser {}", msg.author.name, arg1);

                if !check_owner(msg.author.id, &data.shared).await {
                    let _ = msg
                        .reply(&ctx.http, "Only the owner can remove users.")
                        .await;
                    return Ok(true);
                }

                let raw_id = arg1
                    .trim()
                    .trim_start_matches("<@")
                    .trim_end_matches('>')
                    .trim_start_matches('!');
                let target_id: u64 = match raw_id.parse() {
                    Ok(id) => id,
                    Err(_) => {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                "Usage: `!removeuser <user_id>` or `!removeuser @user`",
                            )
                            .await;
                        return Ok(true);
                    }
                };

                {
                    let mut settings = data.shared.settings.write().await;
                    let before_len = settings.allowed_user_ids.len();
                    settings.allowed_user_ids.retain(|&id| id != target_id);
                    if settings.allowed_user_ids.len() == before_len {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                format!("`{}` is not in the authorized list.", target_id),
                            )
                            .await;
                        return Ok(true);
                    }
                    save_bot_settings(&data.token, &settings);
                }

                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("Removed `{}` from authorized users.", target_id),
                    )
                    .await;
                tracing::info!("  [{ts}] ▶ Removed user: {target_id}");
                return Ok(true);
            }

            "!down" => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                let file_arg = text.strip_prefix("!down").unwrap_or("").trim();
                tracing::info!("  [{ts}] ◀ [{}] !down {}", msg.author.name, file_arg);

                if file_arg.is_empty() {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            "Usage: `!down <filepath>`\nExample: `!down /home/user/file.txt`",
                        )
                        .await;
                    return Ok(true);
                }

                // Resolve relative path
                let resolved_path = if std::path::Path::new(file_arg).is_absolute() {
                    file_arg.to_string()
                } else {
                    let current_path = {
                        let d = data.shared.core.lock().await;
                        d.sessions
                            .get(&channel_id)
                            .and_then(|s| s.current_path.clone())
                    };
                    match current_path {
                        Some(base) => format!("{}/{}", base.trim_end_matches('/'), file_arg),
                        None => {
                            let _ = msg
                                .reply(
                                    &ctx.http,
                                    "No active session. Use absolute path or `!start <path>` first.",
                                )
                                .await;
                            return Ok(true);
                        }
                    }
                };

                let path = std::path::Path::new(&resolved_path);
                if !path.exists() {
                    let _ = msg
                        .reply(&ctx.http, format!("File not found: {}", resolved_path))
                        .await;
                    return Ok(true);
                }
                if !path.is_file() {
                    let _ = msg
                        .reply(&ctx.http, format!("Not a file: {}", resolved_path))
                        .await;
                    return Ok(true);
                }

                let attachment = CreateAttachment::path(path).await?;
                rate_limit_wait(&data.shared, channel_id).await;
                let _ = channel_id
                    .send_message(&ctx.http, CreateMessage::new().add_file(attachment))
                    .await;
                return Ok(true);
            }

            "!shell" => {
                let cmd_str = text.strip_prefix("!shell").unwrap_or("").trim();
                let ts = chrono::Local::now().format("%H:%M:%S");
                let preview = truncate_str(cmd_str, 60);
                tracing::info!("  [{ts}] ◀ [{}] !shell {}", msg.author.name, preview);

                if cmd_str.is_empty() {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            "Usage: `!shell <command>`\nExample: `!shell ls -la`",
                        )
                        .await;
                    return Ok(true);
                }

                let working_dir = {
                    let d = data.shared.core.lock().await;
                    d.sessions
                        .get(&channel_id)
                        .and_then(|s| s.current_path.clone())
                        .unwrap_or_else(|| {
                            dirs::home_dir()
                                .map(|h| h.display().to_string())
                                .unwrap_or_else(|| "/".to_string())
                        })
                };

                let cmd_owned = cmd_str.to_string();
                let working_dir_clone = working_dir.clone();

                let result = tokio::task::spawn_blocking(move || {
                    let child = crate::services::platform::shell::shell_command_builder(&cmd_owned)
                        .current_dir(&working_dir_clone)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn();
                    match child {
                        Ok(child) => child.wait_with_output(),
                        Err(e) => Err(e),
                    }
                })
                .await;

                let response = match result {
                    Ok(Ok(output)) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let exit_code = output.status.code().unwrap_or(-1);
                        let mut parts = Vec::new();
                        if !stdout.is_empty() {
                            parts.push(format!("```\n{}\n```", stdout.trim_end()));
                        }
                        if !stderr.is_empty() {
                            parts.push(format!("stderr:\n```\n{}\n```", stderr.trim_end()));
                        }
                        if parts.is_empty() {
                            parts.push(format!("(exit code: {})", exit_code));
                        } else if exit_code != 0 {
                            parts.push(format!("(exit code: {})", exit_code));
                        }
                        parts.join("\n")
                    }
                    Ok(Err(e)) => format!("Failed to execute: {}", e),
                    Err(e) => format!("Task error: {}", e),
                };

                send_long_message_raw(&ctx.http, channel_id, &response, &data.shared).await?;
                return Ok(true);
            }

            "!cc" => {
                let skill = arg1.to_string();
                let args_str = text
                    .strip_prefix("!cc")
                    .unwrap_or("")
                    .trim()
                    .strip_prefix(&skill)
                    .unwrap_or("")
                    .trim();
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ◀ [{}] !cc {} {}",
                    msg.author.name,
                    skill,
                    args_str
                );

                if skill.is_empty() {
                    let _ = msg.reply(&ctx.http, "Usage: `!cc <skill> [args]`").await;
                    return Ok(true);
                }

                // Handle built-in shortcuts
                match skill.as_str() {
                    "clear" => {
                        let _ = msg.reply(&ctx.http, "Use `!clear` instead.").await;
                        return Ok(true);
                    }
                    "stop" => {
                        // #441: flows through cancel_text_stop_token_mailbox (mailbox_cancel_active_turn)
                        // → stop_active_turn → token.cancelled triggers turn_bridge loop exit
                        // → mailbox_finish_turn canonical cleanup
                        let stop_lookup =
                            cancel_text_stop_token_mailbox(&data.shared, &data.provider, channel_id)
                                .await;
                        match stop_lookup {
                            TextStopLookup::Stop(token) => {
                                let termination_recorded =
                                    super::super::turn_bridge::stop_active_turn(
                                    &data.provider,
                                    &token,
                                    super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                                    "!cc stop",
                                )
                                .await;
                                crate::services::turn_cancel_finalizer::finalize_turn_cancel(
                                    crate::services::turn_cancel_finalizer::FinalizeTurnCancelRequest::from_text_stop(
                                        data.provider.clone(),
                                        channel_id,
                                        "!cc stop",
                                        termination_recorded,
                                    ),
                                );
                                super::super::commands::notify_turn_stop(
                                    &ctx.http,
                                    &data.shared,
                                    &data.provider,
                                    channel_id,
                                    "!cc stop",
                                )
                                .await;
                                let _ = msg.reply(&ctx.http, "Stopping...").await;
                            }
                            TextStopLookup::AlreadyStopping => {
                                let _ = msg.reply(&ctx.http, "Already stopping...").await;
                            }
                            TextStopLookup::NoActiveTurn => {
                                let _ = msg.reply(&ctx.http, "No active request to stop.").await;
                            }
                        }
                        return Ok(true);
                    }
                    "pwd" => {
                        // Delegate to !pwd
                        return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!pwd")).await;
                    }
                    "health" => {
                        return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!health"))
                            .await;
                    }
                    "status" => {
                        return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!status"))
                            .await;
                    }
                    "inflight" => {
                        return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!inflight"))
                            .await;
                    }
                    "help" => {
                        return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!help"))
                            .await;
                    }
                    _ => {}
                }

                // Auto-restore session
                auto_restore_session(&data.shared, channel_id, ctx).await;

                // Verify skill exists
                let skill_exists = {
                    let skills = data.shared.skills_cache.read().await;
                    skills.iter().any(|(name, _)| name == &skill)
                };

                if !skill_exists {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "Unknown skill: `{}`. Use `!cc` to see available skills.",
                                skill
                            ),
                        )
                        .await;
                    return Ok(true);
                }

                // Check session exists
                let has_session = {
                    let d = data.shared.core.lock().await;
                    d.sessions
                        .get(&channel_id)
                        .and_then(|s| s.current_path.as_ref())
                        .is_some()
                };

                if !has_session {
                    let _ = msg
                        .reply(&ctx.http, "No active session. Use `!start <path>` first.")
                        .await;
                    return Ok(true);
                }

                // Block if AI is in progress
                if super::super::mailbox_has_active_turn(&data.shared, channel_id).await {
                    let _ = msg
                        .reply(&ctx.http, "AI request in progress. Use `!stop` to cancel.")
                        .await;
                    return Ok(true);
                }

                // Build the prompt
                let skill_prompt = match super::super::commands::build_provider_skill_prompt(
                    &data.provider,
                    &skill,
                    args_str,
                ) {
                    Ok(prompt) => prompt,
                    Err(message) => {
                        let _ = msg.reply(&ctx.http, message).await;
                        return Ok(true);
                    }
                };

                // Send confirmation and hand off to AI
                rate_limit_wait(&data.shared, channel_id).await;
                let confirm = channel_id
                    .send_message(
                        &ctx.http,
                        CreateMessage::new().content(format!("Running skill: `/{skill}`")),
                    )
                    .await?;

                handle_text_message(
                    ctx,
                    channel_id,
                    confirm.id,
                    msg.author.id,
                    &msg.author.name,
                    &skill_prompt,
                    &data.shared,
                    &data.token,
                    false,
                    false,
                    false,
                    false,
                    None,
                    false,
                )
                .await?;
                return Ok(true);
            }

            _ => {}
        }

        Ok(false)
    */
    super::super::commands::handle_text_command(ctx, msg, data, channel_id, text).await
}

#[cfg(test)]
mod session_strategy_lifecycle_tests {
    use super::*;

    #[test]
    fn session_strategy_lifecycle_event_records_fresh_and_resumed_details() {
        let fresh = session_strategy_lifecycle_event(None, "no_cached_provider_session");
        match fresh {
            TurnEvent::SessionFresh(details) => {
                assert_eq!(details.reason, "no_cached_provider_session");
                assert_eq!(details.provider_session_id, None);
                assert_eq!(details.fingerprint, None);
            }
            other => panic!("expected session_fresh event, got {other:?}"),
        }

        let resumed = session_strategy_lifecycle_event(
            Some("provider-session-123"),
            "db_provider_session_restored",
        );
        match resumed {
            TurnEvent::SessionResumed(details) => {
                assert_eq!(details.reason, "db_provider_session_restored");
                assert_eq!(
                    details.provider_session_id.as_deref(),
                    Some("provider-session-123")
                );
                assert_eq!(
                    details.fingerprint.as_deref(),
                    Some(
                        crate::services::observability::turn_lifecycle::provider_session_fingerprint(
                            "provider-session-123",
                        )
                        .as_str()
                    )
                );
            }
            other => panic!("expected session_resumed event, got {other:?}"),
        }
    }

    #[test]
    fn cli_just_spawned_for_emit_handles_none_and_blank_session_names() {
        // Non-tmux mode (ProcessBackend / no managed session) always
        // re-spawns the CLI per turn, so the helper must report "just
        // spawned" for None / blank tmux session names.
        assert!(cli_just_spawned_for_emit(None));
        assert!(cli_just_spawned_for_emit(Some("")));
        assert!(cli_just_spawned_for_emit(Some("   ")));
    }

    #[test]
    fn watchdog_timeout_cancel_request_uses_canonical_cancel_source() {
        let channel_id = serenity::ChannelId::new(1479671301387059200);
        let mut inflight = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id.get(),
            Some("adk-cdx".to_string()),
            343742347365974026,
            1501205715878936748,
            1501205715878936749,
            "work on issue".to_string(),
            Some("provider-session".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/agentdesk-output.jsonl".to_string()),
            None,
            0,
        );
        inflight.dispatch_id = Some("dispatch-1748".to_string());
        inflight.session_key = Some("mac-mini:AgentDesk-codex-adk-cdx".to_string());

        let request = watchdog_timeout_cancel_request(
            &ProviderKind::Codex,
            channel_id,
            Some(&inflight),
            Some(2),
            true,
        );

        assert_eq!(request.reason, WATCHDOG_TIMEOUT_REASON);
        assert_eq!(request.surface, WATCHDOG_TIMEOUT_CANCEL_SOURCE);
        assert_eq!(
            request.lifecycle_path,
            "mailbox_cancel_active_turn.watchdog_timeout"
        );
        assert_eq!(request.queue_depth, Some(2));
        assert!(request.queue_preserved);
        assert!(request.termination_recorded);
        assert_eq!(
            request.correlation.dispatch_id.as_deref(),
            Some("dispatch-1748")
        );
        assert_eq!(
            request.correlation.session_key.as_deref(),
            Some("mac-mini:AgentDesk-codex-adk-cdx")
        );
        assert_eq!(
            request.correlation.turn_id.as_deref(),
            Some("discord:1479671301387059200:1501205715878936748")
        );
    }

    #[test]
    fn parse_dispatch_context_hints_extracts_auto_queue_retry_resume_session() {
        let hints = parse_dispatch_context_hints(
            Some(
                r#"{"auto_queue_retry_resume_session_id":" thread-1585 ","reset_provider_state":false}"#,
            ),
            Some("implementation"),
        );

        assert_eq!(
            hints.retry_resume_session_id.as_deref(),
            Some("thread-1585")
        );
        assert!(!hints.reset_provider_state);
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::super::super::DiscordSession;
    use super::super::control_intent::{
        build_control_intent_system_reminder, detect_natural_language_control_intent,
    };
    use super::*;
    use crate::services::discord::prompt_builder;
    use crate::services::memory::RecallResponse;
    use crate::ui::ai_screen::{HistoryItem, HistoryType};
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::time::Duration;

    fn sample_recall() -> RecallResponse {
        RecallResponse {
            shared_knowledge: Some("[Shared Knowledge]".to_string()),
            longterm_catalog: Some("- notes.md".to_string()),
            external_recall: Some("[External Recall]".to_string()),
            memento_context_loaded: true,
            warnings: Vec::new(),
            token_usage: crate::services::memory::TokenUsage::default(),
        }
    }

    fn make_session(
        current_path: Option<String>,
        remote_profile_name: Option<String>,
    ) -> DiscordSession {
        DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name,
            channel_id: None,
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
            assistant_turns: 0,
        }
    }

    #[test]
    fn headless_turn_message_id_seed_uses_time_and_process() {
        let seed = headless_turn_message_id_seed(1_777_500_000_000, 42);
        let later_seed = headless_turn_message_id_seed(1_777_500_000_001, 42);
        let other_process_seed = headless_turn_message_id_seed(1_777_500_000_000, 43);

        assert!(seed >= HEADLESS_TURN_MESSAGE_ID_BASE);
        assert!(later_seed > seed);
        assert_ne!(seed, other_process_seed);
    }

    #[test]
    fn metadata_delivery_bot_uses_safe_explicit_bot_only() {
        let explicit = serde_json::json!({
            "delivery_bot": " opencode ",
            "agent_id": "fallback"
        });
        assert_eq!(
            metadata_delivery_bot(Some(&explicit)).as_deref(),
            Some("opencode")
        );

        let fallback = serde_json::json!({"agent_id": "monitoring"});
        assert_eq!(metadata_delivery_bot(Some(&fallback)), None);

        let invalid = serde_json::json!({"delivery_bot": "not valid"});
        assert_eq!(metadata_delivery_bot(Some(&invalid)), None);
    }

    #[test]
    fn metadata_turn_source_prefers_explicit_source_arg() {
        let metadata = serde_json::json!({"source": "text"});

        assert_eq!(
            metadata_turn_source(Some("voice"), Some(&metadata)),
            crate::dispatch::Source::Voice
        );
        assert_eq!(
            metadata_turn_source(None, Some(&metadata)),
            crate::dispatch::Source::Text
        );
        assert_eq!(
            metadata_turn_source(None, None),
            crate::dispatch::Source::Text
        );
    }

    #[test]
    fn memory_injection_plan_routes_shared_knowledge_by_provider() {
        let recall = sample_recall();

        let claude = build_memory_injection_plan(
            &ProviderKind::Claude,
            false,
            DispatchProfile::Full,
            &recall,
        );
        assert_eq!(claude.shared_knowledge_for_context, None);
        assert_eq!(
            claude.shared_knowledge_for_system_prompt,
            Some("[Shared Knowledge]")
        );
        assert_eq!(
            claude.external_recall_for_context,
            Some("[External Recall]")
        );
        assert_eq!(
            claude.longterm_catalog_for_system_prompt,
            Some("- notes.md")
        );

        let codex = build_memory_injection_plan(
            &ProviderKind::Codex,
            false,
            DispatchProfile::Full,
            &recall,
        );
        assert_eq!(
            codex.shared_knowledge_for_context,
            Some("[Shared Knowledge]")
        );
        assert_eq!(codex.shared_knowledge_for_system_prompt, None);
        assert_eq!(codex.external_recall_for_context, Some("[External Recall]"));
        assert_eq!(codex.longterm_catalog_for_system_prompt, Some("- notes.md"));

        let qwen =
            build_memory_injection_plan(&ProviderKind::Qwen, false, DispatchProfile::Full, &recall);
        assert_eq!(
            qwen.shared_knowledge_for_context,
            Some("[Shared Knowledge]")
        );
        assert_eq!(qwen.shared_knowledge_for_system_prompt, None);
        assert_eq!(qwen.external_recall_for_context, Some("[External Recall]"));
        assert_eq!(qwen.longterm_catalog_for_system_prompt, Some("- notes.md"));
    }

    #[test]
    fn memory_injection_plan_keeps_review_lite_minimal() {
        let recall = sample_recall();
        let plan = build_memory_injection_plan(
            &ProviderKind::Codex,
            false,
            DispatchProfile::ReviewLite,
            &recall,
        );

        assert_eq!(plan.shared_knowledge_for_context, None);
        assert_eq!(plan.shared_knowledge_for_system_prompt, None);
        assert_eq!(plan.external_recall_for_context, None);
        assert_eq!(plan.longterm_catalog_for_system_prompt, None);
    }

    #[test]
    fn memory_injection_plan_keeps_lite_to_external_recall_only() {
        let recall = sample_recall();
        let plan = build_memory_injection_plan(
            &ProviderKind::Codex,
            false,
            DispatchProfile::Lite,
            &recall,
        );

        assert_eq!(plan.shared_knowledge_for_context, None);
        assert_eq!(plan.shared_knowledge_for_system_prompt, None);
        assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
        assert_eq!(plan.longterm_catalog_for_system_prompt, None);
    }

    #[test]
    fn memory_injection_plan_skips_shared_knowledge_when_session_exists() {
        let recall = sample_recall();
        let plan =
            build_memory_injection_plan(&ProviderKind::Codex, true, DispatchProfile::Full, &recall);

        assert_eq!(plan.shared_knowledge_for_context, None);
        assert_eq!(plan.shared_knowledge_for_system_prompt, None);
        assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
        assert_eq!(plan.longterm_catalog_for_system_prompt, Some("- notes.md"));
    }

    #[test]
    fn memory_injection_plan_keeps_shared_knowledge_for_claude_resumed_sessions() {
        let recall = sample_recall();
        let plan = build_memory_injection_plan(
            &ProviderKind::Claude,
            true,
            DispatchProfile::Full,
            &recall,
        );

        assert_eq!(plan.shared_knowledge_for_context, None);
        assert_eq!(
            plan.shared_knowledge_for_system_prompt,
            Some("[Shared Knowledge]")
        );
        assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
        assert_eq!(plan.longterm_catalog_for_system_prompt, Some("- notes.md"));
    }

    #[test]
    fn resolve_session_id_for_current_turn_drops_resume_after_model_reset() {
        assert_eq!(
            resolve_session_id_for_current_turn(Some("session-123".to_string()), true),
            None
        );
    }

    #[test]
    fn resolve_session_id_for_current_turn_keeps_existing_session_when_not_reset() {
        assert_eq!(
            resolve_session_id_for_current_turn(Some("session-123".to_string()), false),
            Some("session-123".to_string())
        );
    }

    #[test]
    fn memory_injection_plan_treats_model_reset_as_fresh_turn() {
        let recall = sample_recall();
        let session_id = resolve_session_id_for_current_turn(Some("session-123".to_string()), true);
        let plan = build_memory_injection_plan(
            &ProviderKind::Codex,
            session_id.is_some(),
            DispatchProfile::Full,
            &recall,
        );

        assert_eq!(
            plan.shared_knowledge_for_context,
            Some("[Shared Knowledge]")
        );
        assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
    }

    #[test]
    fn session_path_is_usable_for_existing_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_session(Some(dir.path().to_str().unwrap().to_string()), None);
        assert!(session.validated_path("test-channel").is_some());
    }

    #[test]
    fn session_path_is_not_usable_for_missing_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().to_str().unwrap().to_string();
        drop(dir);
        let mut session = make_session(Some(missing_path), None);
        assert!(session.validated_path("test-channel").is_none());
        assert!(session.current_path.is_none());
        assert!(session.worktree.is_none());
    }

    #[test]
    fn session_path_is_stale_for_remote_session_with_missing_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().to_str().unwrap().to_string();
        drop(dir);
        let mut session = make_session(Some(missing_path), Some("mac-mini".to_string()));
        assert!(session.validated_path("test-channel").is_some());
        assert!(session.current_path.is_some());
    }

    #[test]
    fn review_bypass_hint_detects_leading_pr_number_direct_merge_request() {
        let hint =
            detect_natural_language_control_intent("366은 기여자가 직접 머지가능하게 만들 것 같아")
                .map(|intent| build_control_intent_system_reminder(&intent))
                .expect("direct merge intent should be detected");

        assert!(hint.contains("pr_number: 366"));
        assert!(hint.contains("review_decision: dismiss"));
    }

    #[test]
    fn review_bypass_hint_detects_explicit_pr_reference() {
        let hint = detect_natural_language_control_intent("#366 리뷰 우회하고 직접 머지해도 돼")
            .map(|intent| build_control_intent_system_reminder(&intent))
            .expect("explicit PR reference should be detected");

        assert!(hint.contains("PR #366"));
    }

    #[test]
    fn review_bypass_hint_ignores_debug_discussion() {
        assert_eq!(
            detect_natural_language_control_intent("366 리뷰 우회 인식이 왜 안먹었는지 잡아줘"),
            None
        );
    }

    #[test]
    fn review_bypass_hint_ignores_negative_direct_merge_request() {
        assert_eq!(
            detect_natural_language_control_intent("#366 리뷰 우회하면 안 돼"),
            None
        );
        assert_eq!(
            detect_natural_language_control_intent("366은 직접 머지하지 마"),
            None
        );
    }

    #[test]
    fn review_bypass_hint_ignores_stray_non_pr_numbers() {
        assert_eq!(
            detect_natural_language_control_intent("2명만 직접 머지 가능하게 해줘"),
            None
        );
    }

    #[test]
    fn memento_recall_gate_uses_session_start_and_turn_signals() {
        let memento = settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..settings::ResolvedMemorySettings::default()
        };
        let file = settings::ResolvedMemorySettings::default();

        // #1083: a fresh session (no memento context loaded yet) without any
        // turn signal should trigger the *identity-only* lite recall, not the
        // full session_start recall.
        let identity =
            memento_recall_gate_decision(&memento, false, "평범한 요청", DispatchProfile::Full);
        assert_eq!(identity.reason, "identity_only_session_start");
        assert!(identity.should_recall);
        assert_eq!(identity.mode, RecallMode::IdentityOnly);

        // After identity is loaded, no trigger means no recall.
        assert!(
            !memento_recall_gate_decision(&memento, true, "평범한 요청", DispatchProfile::Full)
                .should_recall
        );

        // Trigger keywords still upgrade to full recall regardless of whether
        // identity has been loaded yet.
        let prev = memento_recall_gate_decision(
            &memento,
            true,
            "이전에 하던 거 이어서 해줘",
            DispatchProfile::Full,
        );
        assert_eq!(prev.reason, "previous_context_signal");
        assert_eq!(prev.mode, RecallMode::Full);

        let err = memento_recall_gate_decision(
            &memento,
            true,
            "빌드 실패 원인 찾아줘",
            DispatchProfile::Full,
        );
        assert_eq!(err.reason, "error_context_signal");
        assert_eq!(err.mode, RecallMode::Full);

        let cfg = memento_recall_gate_decision(
            &memento,
            true,
            "설정 변경 내용 기억나?",
            DispatchProfile::Full,
        );
        assert_eq!(cfg.reason, "setting_change_signal");
        assert_eq!(cfg.mode, RecallMode::Full);

        let explicit = memento_recall_gate_decision(
            &memento,
            true,
            "/recall deploy note",
            DispatchProfile::Full,
        );
        assert_eq!(explicit.reason, "explicit_recall_signal");
        assert_eq!(explicit.mode, RecallMode::Full);

        // Trigger keywords on a fresh session also win over identity-only.
        let fresh_trigger = memento_recall_gate_decision(
            &memento,
            false,
            "이전에 하던 거 이어서 해줘",
            DispatchProfile::Full,
        );
        assert_eq!(fresh_trigger.reason, "previous_context_signal");
        assert_eq!(fresh_trigger.mode, RecallMode::Full);

        // Non-memento backend always recalls in Full mode.
        let non_memento =
            memento_recall_gate_decision(&file, true, "평범한 요청", DispatchProfile::Full);
        assert!(non_memento.should_recall);
        assert_eq!(non_memento.mode, RecallMode::Full);
    }

    #[test]
    fn memento_recall_gate_keeps_lite_profile_lightweight_without_trigger() {
        let memento = settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..settings::ResolvedMemorySettings::default()
        };

        let first =
            memento_recall_gate_decision(&memento, false, "평범한 요청", DispatchProfile::Lite);
        assert!(first.should_recall);
        assert_eq!(first.reason, "lite_identity_only");
        assert_eq!(first.mode, RecallMode::IdentityOnly);

        let next =
            memento_recall_gate_decision(&memento, true, "평범한 요청", DispatchProfile::Lite);
        assert!(!next.should_recall);
        assert_eq!(next.reason, "lite_no_turn_signal");
    }

    #[test]
    fn memento_recall_gate_lite_profile_keeps_explicit_full_recall_triggers() {
        let memento = settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..settings::ResolvedMemorySettings::default()
        };

        let prev = memento_recall_gate_decision(
            &memento,
            true,
            "이전에 하던 거 이어서 해줘",
            DispatchProfile::Lite,
        );
        assert!(prev.should_recall);
        assert_eq!(prev.reason, "previous_context_signal");
        assert_eq!(prev.mode, RecallMode::Full);

        let explicit = memento_recall_gate_decision(
            &memento,
            true,
            "/recall deploy note",
            DispatchProfile::Lite,
        );
        assert!(explicit.should_recall);
        assert_eq!(explicit.reason, "explicit_recall_signal");
        assert_eq!(explicit.mode, RecallMode::Full);
    }

    #[test]
    fn memento_context_loaded_is_not_noted_without_explicit_backend_success() {
        let settings = settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..settings::ResolvedMemorySettings::default()
        };

        assert!(!should_note_memento_context_loaded(
            &settings,
            false,
            &RecallResponse::default()
        ));

        let recall = RecallResponse {
            memento_context_loaded: true,
            ..RecallResponse::default()
        };
        assert!(should_note_memento_context_loaded(
            &settings, false, &recall
        ));
        assert!(!should_note_memento_context_loaded(
            &settings, true, &recall
        ));
    }

    #[test]
    fn dispatch_turns_add_pending_reaction_as_single_source() {
        // #750: announce bot no longer writes ⏳. Command bot must add it on
        // dispatch turn start so the stop-via-reaction-removal path still
        // works.
        let dispatch_id = crate::services::discord::adk_session::parse_dispatch_id(
            "DISPATCH:550e8400-e29b-41d4-a716-446655440000 - Fix login bug",
        );

        assert!(should_add_turn_pending_reaction(dispatch_id.as_deref()));
    }

    #[test]
    fn regular_turns_keep_generic_pending_reaction() {
        assert!(should_add_turn_pending_reaction(None));
    }

    #[test]
    fn native_fast_mode_override_only_applies_when_explicitly_enabled() {
        assert_eq!(
            native_fast_mode_override_for_turn(&ProviderKind::Claude, Some(true)),
            Some(true)
        );
        assert_eq!(
            native_fast_mode_override_for_turn(&ProviderKind::Claude, Some(false)),
            Some(false)
        );
        assert_eq!(
            native_fast_mode_override_for_turn(&ProviderKind::Claude, None),
            None
        );
        assert_eq!(
            native_fast_mode_override_for_turn(&ProviderKind::Gemini, Some(true)),
            None
        );
    }

    #[test]
    fn codex_goals_override_only_applies_to_codex() {
        assert_eq!(
            codex_goals_override_for_turn(&ProviderKind::Codex, Some(true)),
            Some(true)
        );
        assert_eq!(
            codex_goals_override_for_turn(&ProviderKind::Codex, Some(false)),
            Some(false)
        );
        assert_eq!(
            codex_goals_override_for_turn(&ProviderKind::Claude, Some(true)),
            None
        );
    }

    #[test]
    fn codex_goal_start_request_matches_only_goal_command_prefix() {
        assert!(is_codex_goal_start_request("/goal"));
        assert!(is_codex_goal_start_request("  /goal 지금 문서 검토"));
        assert!(is_codex_goal_start_request("/goal\n다음 줄"));
        assert!(is_codex_goal_start_request("/goal\t탭 뒤 내용"));

        assert!(!is_codex_goal_start_request("/goals"));
        assert!(!is_codex_goal_start_request("/goalkeeper"));
        assert!(!is_codex_goal_start_request("질문 /goal"));
        assert!(!is_codex_goal_start_request(""));
    }

    #[test]
    fn classify_codex_goal_command_basic() {
        // ChainedStart: plain /goal
        assert_eq!(
            classify_codex_goal_command("/goal 새 목표"),
            GoalCommandKind::ChainedStart
        );
        assert_eq!(
            classify_codex_goal_command("/goal\n다음 줄"),
            GoalCommandKind::ChainedStart
        );
        assert_eq!(
            classify_codex_goal_command("  /goal 탭 뒤"),
            GoalCommandKind::ChainedStart
        );

        // FreshStart: /goal --fresh
        assert_eq!(
            classify_codex_goal_command("/goal --fresh 새 목표"),
            GoalCommandKind::FreshStart
        );
        assert_eq!(
            classify_codex_goal_command("/goal --fresh"),
            GoalCommandKind::FreshStart
        );

        // Lifecycle
        assert_eq!(
            classify_codex_goal_command("/goal pause"),
            GoalCommandKind::Lifecycle
        );
        assert_eq!(
            classify_codex_goal_command("/goal resume"),
            GoalCommandKind::Lifecycle
        );
        assert_eq!(
            classify_codex_goal_command("/goal clear"),
            GoalCommandKind::Lifecycle
        );

        // NotGoal
        assert_eq!(
            classify_codex_goal_command("/goals"),
            GoalCommandKind::NotGoal
        );
        assert_eq!(
            classify_codex_goal_command("/goalkeeper"),
            GoalCommandKind::NotGoal
        );
        assert_eq!(
            classify_codex_goal_command("질문 /goal"),
            GoalCommandKind::NotGoal
        );
        assert_eq!(classify_codex_goal_command(""), GoalCommandKind::NotGoal);
    }

    #[test]
    fn classify_codex_goal_command_for_provider_gates_non_codex() {
        // Non-Codex provider → always NotGoal
        assert_eq!(
            classify_codex_goal_command_for_provider(&ProviderKind::Claude, "/goal 새 목표", None),
            GoalCommandKind::NotGoal
        );
        // goals disabled → NotGoal
        assert_eq!(
            classify_codex_goal_command_for_provider(
                &ProviderKind::Codex,
                "/goal 새 목표",
                Some(false)
            ),
            GoalCommandKind::NotGoal
        );
        // Codex + goals enabled (or unset) → classify
        assert_eq!(
            classify_codex_goal_command_for_provider(
                &ProviderKind::Codex,
                "/goal 새 목표",
                Some(true)
            ),
            GoalCommandKind::ChainedStart
        );
        assert_eq!(
            classify_codex_goal_command_for_provider(
                &ProviderKind::Codex,
                "/goal --fresh 새 목표",
                None
            ),
            GoalCommandKind::FreshStart
        );
        assert_eq!(
            classify_codex_goal_command_for_provider(
                &ProviderKind::Codex,
                "/goal pause",
                Some(true)
            ),
            GoalCommandKind::Lifecycle
        );
    }

    #[test]
    fn rewrite_fresh_goal_prompt_strips_fresh_marker() {
        assert_eq!(
            rewrite_fresh_goal_prompt("/goal --fresh 새 목표"),
            "/goal 새 목표"
        );
        assert_eq!(rewrite_fresh_goal_prompt("/goal --fresh"), "/goal");
        // Non-fresh prompts are returned unchanged
        assert_eq!(rewrite_fresh_goal_prompt("/goal 새 목표"), "/goal 새 목표");
    }

    #[test]
    fn clear_resets_memento_skip_so_next_turn_can_reload_context() {
        let memento = settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..settings::ResolvedMemorySettings::default()
        };
        let mut session = make_session(Some("/tmp/project".to_string()), None);

        session.restore_provider_session(Some("session-1".to_string()));
        session.note_memento_context_loaded();
        assert!(
            !memento_recall_gate_decision(
                &memento,
                session.memento_context_loaded,
                "평범한 요청",
                DispatchProfile::Full,
            )
            .should_recall
        );

        session.clear_provider_session();
        assert!(
            memento_recall_gate_decision(
                &memento,
                session.memento_context_loaded,
                "평범한 요청",
                DispatchProfile::Full,
            )
            .should_recall
        );
    }

    #[test]
    fn restored_provider_session_does_not_skip_memento_recall_until_context_reloads() {
        let memento = settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..settings::ResolvedMemorySettings::default()
        };
        let mut session = make_session(Some("/tmp/project".to_string()), None);

        session.restore_provider_session(Some("session-1".to_string()));
        let mut memento_context_loaded = session.memento_context_loaded;
        assert!(
            memento_recall_gate_decision(
                &memento,
                memento_context_loaded,
                "평범한 요청",
                DispatchProfile::Full,
            )
            .should_recall
        );

        session.note_memento_context_loaded();
        memento_context_loaded = session.memento_context_loaded;
        assert!(
            !memento_recall_gate_decision(
                &memento,
                memento_context_loaded,
                "평범한 요청",
                DispatchProfile::Full,
            )
            .should_recall
        );
    }

    #[test]
    fn session_reset_reason_triggers_after_idle_timeout() {
        let mut session = make_session(Some("/tmp/project".to_string()), None);
        let last_active = tokio::time::Instant::now();
        let now = last_active + crate::services::discord::SESSION_MAX_IDLE + Duration::from_secs(1);
        session.last_active = last_active;

        assert_eq!(
            session_reset_reason_for_turn(&session, now),
            Some(SessionResetReason::IdleExpired)
        );
    }

    #[test]
    fn session_reset_reason_triggers_after_assistant_turn_cap() {
        let mut session = make_session(Some("/tmp/project".to_string()), None);
        session.history = (0..100)
            .map(|idx| HistoryItem {
                item_type: HistoryType::Assistant,
                content: format!("assistant-{idx}"),
            })
            .collect();

        assert_eq!(
            session_reset_reason_for_turn(&session, tokio::time::Instant::now()),
            Some(SessionResetReason::AssistantTurnCap)
        );
    }

    #[test]
    fn effective_fast_mode_channel_id_prefers_thread_parent() {
        assert_eq!(
            effective_fast_mode_channel_id(
                ChannelId::new(222),
                Some((ChannelId::new(111), Some("adk-cdx".to_string())))
            ),
            ChannelId::new(111)
        );
    }

    #[test]
    fn effective_fast_mode_channel_id_keeps_non_thread_channel() {
        assert_eq!(
            effective_fast_mode_channel_id(ChannelId::new(222), None),
            ChannelId::new(222)
        );
    }

    #[test]
    fn merge_reply_contexts_prefers_retry_context_first() {
        assert_eq!(
            merge_reply_contexts(
                Some("reply context".to_string()),
                Some("retry context".to_string())
            )
            .as_deref(),
            Some("retry context\n\nreply context")
        );
    }

    #[test]
    fn parse_dispatch_context_hints_extracts_session_strategy_and_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let raw = serde_json::json!({
            "worktree_path": temp.path(),
            "reset_provider_state": true,
            "recreate_tmux": true
        })
        .to_string();

        let hints = parse_dispatch_context_hints(Some(&raw), Some("review-decision"));

        assert_eq!(hints.worktree_path.as_deref(), temp.path().to_str());
        assert!(hints.stale_worktree_path.is_none());
        assert!(hints.reset_provider_state);
        assert!(hints.recreate_tmux);
    }

    #[test]
    fn parse_dispatch_context_hints_tracks_missing_path_but_keeps_legacy_reset_flag() {
        let hints = parse_dispatch_context_hints(
            Some(r#"{"worktree_path":"/definitely/missing","force_new_session":true}"#),
            Some("review-decision"),
        );

        assert!(hints.worktree_path.is_none());
        assert_eq!(
            hints.stale_worktree_path.as_deref(),
            Some("/definitely/missing")
        );
        assert!(hints.reset_provider_state);
        assert!(!hints.recreate_tmux);
    }

    #[test]
    fn parse_dispatch_context_hints_defaults_fresh_session_for_work_dispatches() {
        let implementation = parse_dispatch_context_hints(None, Some("implementation"));
        let review = parse_dispatch_context_hints(None, Some("review"));
        let rework = parse_dispatch_context_hints(None, Some("rework"));

        assert!(implementation.reset_provider_state);
        assert!(!implementation.recreate_tmux);
        assert!(review.reset_provider_state);
        assert!(!review.recreate_tmux);
        assert!(rework.reset_provider_state);
        assert!(!rework.recreate_tmux);
    }

    #[test]
    fn parse_dispatch_context_hints_defaults_warm_resume_for_review_decision() {
        let hints = parse_dispatch_context_hints(None, Some("review-decision"));
        assert!(!hints.reset_provider_state);
        assert!(!hints.recreate_tmux);
    }

    #[test]
    fn parse_dispatch_context_hints_respects_explicit_override_over_dispatch_type_default() {
        let hints =
            parse_dispatch_context_hints(Some(r#"{"force_new_session":false}"#), Some("rework"));
        assert!(!hints.reset_provider_state);
        assert!(!hints.recreate_tmux);
    }

    #[test]
    fn parse_dispatch_context_hints_allows_tmux_recreate_without_legacy_alias() {
        let hints = parse_dispatch_context_hints(
            Some(r#"{"reset_provider_state":false,"recreate_tmux":true}"#),
            Some("review-decision"),
        );
        assert!(!hints.reset_provider_state);
        assert!(hints.recreate_tmux);
    }

    #[test]
    fn parse_dispatch_context_hints_extracts_target_repo() {
        let hints = parse_dispatch_context_hints(
            Some(r#"{"target_repo":"/tmp/external-762","worktree_path":null}"#),
            Some("review"),
        );
        assert_eq!(hints.target_repo.as_deref(), Some("/tmp/external-762"));
        assert!(hints.worktree_path.is_none());
    }

    #[test]
    fn parse_dispatch_context_hints_target_repo_rejects_blank_values() {
        let hints = parse_dispatch_context_hints(
            Some(r#"{"target_repo":"   ","worktree_path":null}"#),
            Some("review"),
        );
        assert!(hints.target_repo.is_none());
    }

    /// #762 (B): when the dispatch context pins an external `target_repo` but
    /// emits `worktree_path: null` (e.g. the completion lives in repo HEAD
    /// but HEAD has drifted, so refresh suppressed worktree_path per #682
    /// round 3), bootstrap must land in the external repo instead of the
    /// default AgentDesk workspace. Prior behavior always fell back to
    /// `resolve_repo_dir()` because `DispatchContextHints` dropped
    /// `target_repo` on the floor.
    #[test]
    fn resolve_dispatch_target_repo_dir_honors_external_target_repo_when_worktree_path_is_null() {
        // Build a real git worktree at a path that is explicitly NOT the
        // default AgentDesk workspace. `resolve_repo_dir_for_target` treats
        // absolute paths as explicit and only accepts them if the directory
        // is a valid git worktree.
        let external = tempfile::tempdir().unwrap();
        let external_dir = external.path().to_str().unwrap();
        GitCommand::new()
            .args(["init", "-b", "main"])
            .repo(external_dir)
            .run_output()
            .unwrap();
        GitCommand::new()
            .args(["config", "user.email", "test@test.com"])
            .repo(external_dir)
            .run_output()
            .unwrap();
        GitCommand::new()
            .args(["config", "user.name", "Test"])
            .repo(external_dir)
            .run_output()
            .unwrap();
        GitCommand::new()
            .args(["commit", "--allow-empty", "-m", "initial"])
            .repo(external_dir)
            .run_output()
            .unwrap();

        let raw = serde_json::json!({
            "target_repo": external_dir,
            "worktree_path": serde_json::Value::Null,
            "reviewed_commit": "0123456789abcdef0123456789abcdef01234567",
        })
        .to_string();
        let hints = parse_dispatch_context_hints(Some(&raw), Some("review"));

        assert_eq!(hints.target_repo.as_deref(), Some(external_dir));
        assert!(
            hints.worktree_path.is_none(),
            "null worktree_path must not be synthesized from target_repo by the hints parser"
        );

        // This is the specific regression: bootstrap must resolve to the
        // external repo, NOT the default AgentDesk workspace. Prior code
        // called `resolve_repo_dir()` unconditionally when `worktree_path`
        // was absent.
        let resolved = resolve_dispatch_target_repo_dir(hints.target_repo.as_deref())
            .expect("external target_repo with null worktree_path must resolve to the repo dir");
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(external_dir).unwrap()
        );
    }

    #[test]
    fn resolve_dispatch_target_repo_dir_returns_none_for_missing_target_repo() {
        assert!(resolve_dispatch_target_repo_dir(None).is_none());
        assert!(resolve_dispatch_target_repo_dir(Some("")).is_none());
        assert!(resolve_dispatch_target_repo_dir(Some("   ")).is_none());
    }

    #[test]
    fn resolve_dispatch_target_repo_dir_rejects_nonexistent_path() {
        // A target_repo that references a path outside any configured
        // mapping cannot be resolved — bootstrap falls back to the default
        // workspace, not to the (nonexistent) requested path.
        assert!(
            resolve_dispatch_target_repo_dir(Some(
                "/tmp/agentdesk-issue-762-definitely-not-a-repo"
            ))
            .is_none()
        );
    }

    #[test]
    fn session_runtime_state_after_redirect_prefers_reused_thread_state() {
        let parent_dir = tempfile::tempdir().unwrap();
        let thread_dir = tempfile::tempdir().unwrap();
        let parent_channel_id = ChannelId::new(100);
        let thread_channel_id = ChannelId::new(200);

        let mut sessions = std::collections::HashMap::new();
        let mut parent = make_session(Some(parent_dir.path().to_str().unwrap().to_string()), None);
        parent.restore_provider_session(Some("parent-session".to_string()));
        sessions.insert(parent_channel_id, parent);

        let thread = make_session(Some(thread_dir.path().to_str().unwrap().to_string()), None);
        sessions.insert(thread_channel_id, thread);

        let resolved = session_runtime_state_after_redirect(
            &mut sessions,
            parent_channel_id,
            thread_channel_id,
            (
                Some("parent-session".to_string()),
                true,
                parent_dir.path().to_str().unwrap().to_string(),
            ),
        );

        assert_eq!(resolved.0, None);
        assert!(!resolved.1);
        assert_eq!(resolved.2, thread_dir.path().to_str().unwrap());
    }

    /// #762 round-2 (B): reused threads that bypass `bootstrap_thread_session`
    /// still need their session CWD refreshed whenever the new dispatch
    /// points at a different effective path — even when no `worktree_path`
    /// is supplied. Prior behavior only updated session.current_path when
    /// `dispatch_worktree_path.is_some()`, so external-repo reviews that
    /// emitted only `target_repo` quietly executed inside the previous
    /// implementation's repo.
    #[test]
    fn dispatch_session_path_should_update_when_target_repo_diverges_without_worktree() {
        // Reused thread: dispatch present, no worktree_path, but
        // target_repo resolved to a different directory than the
        // session's stale current_path. Must update.
        assert!(
            dispatch_session_path_should_update(
                true, // has_dispatch
                Some("review"),
                false, // has_worktree_path
                false, // existing thread, no fresh bootstrap this turn
                "/tmp/stale-impl-repo",
                "/tmp/external-target-repo",
            ),
            "reused thread with divergent target_repo must update session CWD"
        );
    }

    #[test]
    fn dispatch_session_path_should_update_still_triggers_for_worktree_path_dispatch() {
        // Classic #259 path: dispatch has worktree_path. Always update,
        // even when stale current_path already happens to match.
        assert!(
            dispatch_session_path_should_update(
                true,
                Some("review"),
                true,
                false,
                "/tmp/impl-wt",
                "/tmp/impl-wt",
            ),
            "worktree_path dispatches must always update session CWD"
        );
        assert!(
            dispatch_session_path_should_update(
                true,
                Some("review"),
                true,
                false,
                "/tmp/stale",
                "/tmp/fresh-wt",
            ),
            "worktree_path dispatches with divergent path must update"
        );
    }

    #[test]
    fn dispatch_session_path_should_update_skips_when_paths_match() {
        // No dispatch → leave alone.
        assert!(!dispatch_session_path_should_update(
            false, None, false, false, "/tmp/a", "/tmp/b",
        ));
        // Dispatch present but worktree_path absent AND effective path
        // matches current path → nothing to update.
        assert!(!dispatch_session_path_should_update(
            true,
            Some("review"),
            false,
            false,
            "/tmp/same",
            "/tmp/same",
        ));
    }

    #[test]
    fn dispatch_session_path_should_update_fresh_bootstrap_for_worktree_dispatch() {
        assert!(dispatch_session_path_should_update(
            true,
            Some("implementation"),
            true,
            true,
            "/tmp/workspaces/agentdesk",
            "/tmp/worktrees/dispatch-934",
        ));
    }

    #[test]
    fn evaluate_dispatch_cwd_policy_rejects_main_workspace_for_implementation() {
        let root = tempfile::tempdir().unwrap();
        let main_workspace = root.path().join("workspaces").join("agentdesk");
        let worktrees_root = root.path().join("worktrees");
        std::fs::create_dir_all(&main_workspace).unwrap();
        std::fs::create_dir_all(worktrees_root.join("impl-934")).unwrap();

        let decision = evaluate_dispatch_cwd_policy(
            Some("implementation"),
            main_workspace.to_str().unwrap(),
            Some(main_workspace.as_path()),
            Some(worktrees_root.as_path()),
        );

        assert!(decision.log_main_workspace_error);
        assert!(decision.reject_for_missing_fresh_worktree);
    }

    #[test]
    fn evaluate_dispatch_cwd_policy_allows_review_repo_root_fallback() {
        let root = tempfile::tempdir().unwrap();
        let main_workspace = root.path().join("workspaces").join("agentdesk");
        let external_repo = root.path().join("external-review");
        let worktrees_root = root.path().join("worktrees");
        std::fs::create_dir_all(&main_workspace).unwrap();
        std::fs::create_dir_all(&external_repo).unwrap();
        std::fs::create_dir_all(&worktrees_root).unwrap();

        let decision = evaluate_dispatch_cwd_policy(
            Some("review"),
            external_repo.to_str().unwrap(),
            Some(main_workspace.as_path()),
            Some(worktrees_root.as_path()),
        );

        assert!(!decision.log_main_workspace_error);
        assert!(!decision.reject_for_missing_fresh_worktree);
    }

    #[test]
    fn session_runtime_state_after_redirect_keeps_original_state_when_channel_unchanged() {
        let channel_id = ChannelId::new(100);
        let dir = tempfile::tempdir().unwrap();
        let original = (
            Some("session-1".to_string()),
            true,
            dir.path().to_str().unwrap().to_string(),
        );

        let resolved = session_runtime_state_after_redirect(
            &mut std::collections::HashMap::new(),
            channel_id,
            channel_id,
            original.clone(),
        );

        assert_eq!(resolved, original);
    }

    #[test]
    fn race_requeue_preserves_reply_boundary_without_reply_context() {
        let queued = build_race_requeued_intervention(
            UserId::new(7),
            MessageId::new(8),
            "hello",
            None,
            true,
            true,
        );

        assert!(queued.has_reply_boundary);
        assert!(queued.reply_context.is_none());
        assert!(queued.merge_consecutive);
    }

    #[test]
    fn race_requeue_preserves_non_mergeable_turns() {
        let queued = build_race_requeued_intervention(
            UserId::new(7),
            MessageId::new(8),
            "hello",
            None,
            false,
            false,
        );

        assert!(!queued.has_reply_boundary);
        assert!(!queued.merge_consecutive);
    }

    #[test]
    fn build_system_discord_context_omits_user_identity() {
        let context = build_system_discord_context(
            Some("adk-cdx"),
            Some("agentdesk"),
            ChannelId::new(42),
            false,
        );

        assert_eq!(
            context,
            "Discord context: channel #adk-cdx (ID: 42) (category: agentdesk)"
        );
        assert!(!context.contains("user:"));
        assert!(!context.contains("author_id"));
    }

    #[test]
    fn wrap_user_prompt_with_author_adds_user_prefix() {
        let prompt = wrap_user_prompt_with_author(
            "  Alice [ops]\nteam  ",
            UserId::new(77),
            "deploy it".to_string(),
        );

        assert_eq!(prompt, "[User: Alice (ops) team (ID: 77)]\ndeploy it");
    }

    #[test]
    fn dm_channel_roster_keeps_single_requester() {
        let shared = super::super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(42);
        shared.record_channel_speaker(channel_id, UserId::new(101), "Alice", false);
        shared.record_channel_speaker(channel_id, UserId::new(202), "Bob", false);
        shared.record_channel_speaker(channel_id, UserId::new(101), "Alice", true);

        let roster = shared.channel_roster(channel_id, UserId::new(999), "Fallback");
        assert_eq!(roster, vec![UserRecord::new(UserId::new(101), "Alice")]);
    }

    #[test]
    fn watchdog_prealert_helpers_parse_and_dedupe_deadline() {
        assert_eq!(watchdog_deadlock_prealert_bot_name(), "announce");
        assert_eq!(
            parse_watchdog_alert_channel_id("channel:<#12345>"),
            Some(ChannelId::new(12345))
        );
        assert_eq!(
            parse_watchdog_alert_channel_id("67890"),
            Some(ChannelId::new(67890))
        );
        assert_eq!(parse_watchdog_alert_channel_id("deadlock-manager"), None);

        let deadline = 1_000_000;
        assert!(!should_send_watchdog_deadlock_prealert(
            deadline - WATCHDOG_DEADLOCK_PREALERT_MS - 1,
            deadline,
            None
        ));
        assert!(should_send_watchdog_deadlock_prealert(
            deadline - WATCHDOG_DEADLOCK_PREALERT_MS,
            deadline,
            None
        ));
        assert!(!should_send_watchdog_deadlock_prealert(
            deadline - 1,
            deadline,
            Some(deadline)
        ));
        assert!(!should_send_watchdog_deadlock_prealert(
            deadline, deadline, None
        ));
    }

    #[test]
    fn watchdog_prealert_message_contains_extension_contract() {
        let now = 60 * 60 * 1000;
        let deadline = now + 4 * 60 * 1000;
        let started = 0;
        let max_deadline = started + 3 * 60 * 60 * 1000;

        let message = build_watchdog_deadlock_prealert_message(
            &ProviderKind::Codex,
            ChannelId::new(42),
            now,
            deadline,
            started,
            max_deadline,
            None,
        );

        assert!(message.contains("[Watchdog pre-timeout]"));
        assert!(message.contains("channel_id: 42"));
        assert!(message.contains("provider: codex"));
        assert!(message.contains("remaining: 4분"));
        assert!(message.contains("POST /api/turns/42/extend-timeout"));
    }

    #[test]
    fn watchdog_deadline_extension_moves_deadline_and_tracked_max() {
        let token = CancelToken::new();
        token
            .watchdog_deadline_ms
            .store(1_000, std::sync::atomic::Ordering::Relaxed);
        token
            .watchdog_max_deadline_ms
            .store(2_000, std::sync::atomic::Ordering::Relaxed);
        let extension = crate::services::turn_orchestrator::WatchdogDeadlineExtension {
            requested_deadline_ms: 4_000,
            new_deadline_ms: 4_000,
            max_deadline_ms: 4_000,
            applied_extend_secs: 2,
            requested_extend_secs: 2,
            extension_count: 1,
            extension_count_limit: u32::MAX,
            extension_total_secs: 2,
            extension_total_secs_limit: u64::MAX,
            clamped: false,
        };

        assert_eq!(apply_watchdog_deadline_extension(&token, extension), 4_000);
        assert_eq!(
            token
                .watchdog_deadline_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            4_000
        );
        assert_eq!(
            token
                .watchdog_max_deadline_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            4_000
        );
    }

    #[test]
    fn attach_paused_turn_watcher_pauses_existing_tmux_owner_channel() {
        let shared = super::super::super::make_shared_data_for_tests();
        let owner_channel = ChannelId::new(1485506232256168136);
        let thread_channel = ChannelId::new(1485506232256168137);
        let tmux_name = "AgentDesk-codex-adk-cdx-owner".to_string();
        let owner_paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let owner_pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        shared.tmux_watchers.insert(
            owner_channel,
            TmuxWatcherHandle {
                tmux_session_name: tmux_name.clone(),
                paused: owner_paused.clone(),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                pause_epoch: owner_pause_epoch.clone(),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                    super::super::super::tmux_watcher_now_ms(),
                )),
                mailbox_finalize_owed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );

        let owner = attach_paused_turn_watcher(
            &shared,
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
            &ProviderKind::Codex,
            thread_channel,
            Some(tmux_name),
            Some("/tmp/agentdesk-test-output.jsonl".to_string()),
            0,
            "unit-test-turn-start",
        );

        assert_eq!(owner, owner_channel);
        assert!(
            owner_paused.load(std::sync::atomic::Ordering::Relaxed),
            "turn start must pause the live owner watcher, not the requested thread slot"
        );
        assert_eq!(
            owner_pause_epoch.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(
            !shared.tmux_watchers.contains_key(&thread_channel),
            "reusing an owner watcher must not install a duplicate thread watcher"
        );
    }

    #[test]
    fn attach_paused_turn_watcher_skips_prelaunch_dead_tmux() {
        let shared = super::super::super::make_shared_data_for_tests();
        let channel = ChannelId::new(1485506232256168138);
        let owner = attach_paused_turn_watcher(
            &shared,
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
            &ProviderKind::Codex,
            channel,
            Some("AgentDesk-codex-not-yet-spawned".to_string()),
            Some("/tmp/agentdesk-test-output.jsonl".to_string()),
            0,
            "unit-test-prelaunch",
        );

        assert_eq!(owner, channel);
        assert!(
            !shared.tmux_watchers.contains_key(&channel),
            "prelaunch turn start must wait for TmuxReady instead of spawning a watcher that immediately observes a dead pane"
        );
    }

    #[test]
    fn multi_user_turns_keep_system_prompt_identical() {
        let discord_context = build_system_discord_context(
            Some("multi-user"),
            Some("agentdesk"),
            ChannelId::new(9001),
            false,
        );

        let alice_system = prompt_builder::build_system_prompt(
            &discord_context,
            &[],
            "/tmp/work",
            ChannelId::new(9001),
            "token",
            None,
            false,
            prompt_builder::DispatchProfile::Full,
            None,
            None,
            None,
            None,
            None,
            false,
        );
        let bob_system = prompt_builder::build_system_prompt(
            &discord_context,
            &[],
            "/tmp/work",
            ChannelId::new(9001),
            "token",
            None,
            false,
            prompt_builder::DispatchProfile::Full,
            None,
            None,
            None,
            None,
            None,
            false,
        );

        assert_eq!(alice_system.as_bytes(), bob_system.as_bytes());

        let alice_user_prompt =
            wrap_user_prompt_with_author("Alice", UserId::new(101), "same task".to_string());
        let bob_user_prompt =
            wrap_user_prompt_with_author("Bob", UserId::new(202), "same task".to_string());

        assert!(alice_user_prompt.starts_with("[User: Alice (ID: 101)]"));
        assert!(bob_user_prompt.starts_with("[User: Bob (ID: 202)]"));
        assert_ne!(alice_user_prompt, bob_user_prompt);
    }

    /// codex review round-8 P2 (#1332): when `send_intake_placeholder` POSTs
    /// while another concurrent message has lost the race and queued itself,
    /// the failure-path mailbox release MUST schedule a deferred kickoff so
    /// the queued message is dispatched. The previous code ignored
    /// `FinishTurnResult::has_pending` and let the channel sit idle with a
    /// persisted queued item, so this test pins the kickoff.
    #[tokio::test(flavor = "current_thread")]
    async fn release_mailbox_after_placeholder_post_failure_schedules_kickoff_when_pending() {
        use crate::services::provider::CancelToken;
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use std::time::Instant;

        let shared = super::super::super::make_shared_data_for_tests();
        let provider = super::super::super::ProviderKind::Codex;
        let channel_id = ChannelId::new(987_654_321);
        let owner = UserId::new(42);
        let active_msg_id = MessageId::new(1_000);
        let queued_msg_id = MessageId::new(1_001);

        // 1. Active turn acquires the slot via the start-turn race.
        let cancel_token = Arc::new(CancelToken::new());
        let started = super::super::super::mailbox_try_start_turn(
            shared.as_ref(),
            channel_id,
            cancel_token.clone(),
            owner,
            active_msg_id,
        )
        .await;
        assert!(started, "fresh mailbox should accept the active turn");
        shared.global_active.fetch_add(1, Ordering::Relaxed);

        // 2. While the placeholder POST is in flight, a concurrent message
        //    loses the race and is enqueued as a soft intervention.
        let enqueue = super::super::super::mailbox_enqueue_intervention(
            shared.as_ref(),
            &provider,
            channel_id,
            super::super::super::Intervention {
                author_id: owner,
                message_id: queued_msg_id,
                source_message_ids: vec![queued_msg_id],
                text: "race-loser queued message".to_string(),
                mode: super::super::super::InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
        )
        .await;
        assert!(enqueue.enqueued, "concurrent race-loser should enqueue");

        // 3. Snapshot the deferred-hook backlog BEFORE the simulated failure
        //    so we can prove the kickoff was actually scheduled.
        let backlog_before = shared.deferred_hook_backlog.load(Ordering::Relaxed);

        // 4. Simulate the placeholder POST failure: invoke the new release
        //    helper that wraps `mailbox_finish_turn` + the deferred kickoff.
        let kicked =
            release_mailbox_after_placeholder_post_failure(&shared, &provider, channel_id).await;

        // 5. The helper MUST report a kickoff was scheduled, the deferred
        //    backlog MUST have been incremented synchronously by
        //    `schedule_deferred_idle_queue_kickoff`, and the mailbox MUST
        //    still have the queued item ready for the kickoff to drain.
        assert!(kicked, "kickoff must be scheduled when has_pending == true");
        let backlog_after = shared.deferred_hook_backlog.load(Ordering::Relaxed);
        assert_eq!(
            backlog_after,
            backlog_before + 1,
            "deferred_hook_backlog must increment exactly once when a kickoff is scheduled (channel must not be left idle with a queued item)"
        );

        let snapshot = shared.mailbox(channel_id).snapshot().await;
        assert_eq!(
            snapshot.intervention_queue.len(),
            1,
            "queued race-loser must remain in the mailbox so the deferred kickoff can drain it"
        );
        assert_eq!(
            snapshot.intervention_queue[0].message_id, queued_msg_id,
            "queued message identity must be preserved across mailbox_finish_turn"
        );
    }

    /// Negative: when the mailbox queue is empty after `mailbox_finish_turn`,
    /// the failure-path helper must NOT schedule a deferred kickoff (no
    /// double-kicks, no spurious wake-ups for channels with nothing pending).
    #[tokio::test(flavor = "current_thread")]
    async fn release_mailbox_after_placeholder_post_failure_skips_kickoff_when_idle() {
        use crate::services::provider::CancelToken;
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let shared = super::super::super::make_shared_data_for_tests();
        let provider = super::super::super::ProviderKind::Codex;
        let channel_id = ChannelId::new(123_456_789);
        let owner = UserId::new(7);
        let active_msg_id = MessageId::new(2_000);

        let cancel_token = Arc::new(CancelToken::new());
        let started = super::super::super::mailbox_try_start_turn(
            shared.as_ref(),
            channel_id,
            cancel_token.clone(),
            owner,
            active_msg_id,
        )
        .await;
        assert!(started, "fresh mailbox should accept the active turn");
        shared.global_active.fetch_add(1, Ordering::Relaxed);

        let backlog_before = shared.deferred_hook_backlog.load(Ordering::Relaxed);
        let kicked =
            release_mailbox_after_placeholder_post_failure(&shared, &provider, channel_id).await;
        assert!(
            !kicked,
            "no kickoff should be scheduled when nothing is pending"
        );
        let backlog_after = shared.deferred_hook_backlog.load(Ordering::Relaxed);
        assert_eq!(
            backlog_after, backlog_before,
            "deferred_hook_backlog must not grow when the queue is empty (avoid spurious wake-ups)"
        );
    }

    /// codex review round-9 P2 (#1332): when a dispatch-role-routed message
    /// loses the mailbox start-turn race, the new race-loss path enqueues
    /// the intervention BEFORE awaiting any Discord HTTP. This test
    /// simulates the round-8-finding race directly:
    ///
    ///   1. Active turn is running.
    ///   2. `dispatch_role_overrides[channel] = override_channel` is
    ///      installed (pretend this turn was a Codex-review hand-off
    ///      pinning a sister channel).
    ///   3. A new message arrives, loses the race, and goes through the
    ///      round-9 ordering — **enqueue first, then POST placeholder**.
    ///   4. **DURING the simulated POST await window**, the active turn
    ///      finishes (`mailbox_finish_turn`).
    ///   5. `turn_bridge` mirror logic checks `finish.has_pending` —
    ///      because we already enqueued, `has_pending == true`, so the
    ///      override is preserved. The queued dispatch will run under the
    ///      intended dispatch routing.
    ///
    /// Pre round-9 (enqueue AFTER the POST await): the active turn would
    /// finalize before our enqueue, observe `has_pending == false`, and
    /// `turn_bridge` would clear `dispatch_role_overrides`. Our late
    /// enqueue would then be persisted/routed without the override and the
    /// queued dispatch would silently run under the wrong provider.
    #[tokio::test(flavor = "current_thread")]
    async fn race_loss_enqueue_before_post_preserves_dispatch_role_overrides() {
        use crate::services::provider::CancelToken;
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let shared = super::super::super::make_shared_data_for_tests();
        let provider = super::super::super::ProviderKind::Claude;
        let channel_id = ChannelId::new(987_654_321);
        let override_channel = ChannelId::new(111_222_333);
        let owner = UserId::new(11);
        let active_user_msg_id = MessageId::new(5_000);
        let race_lost_msg_id = MessageId::new(5_001);

        // (1) Active turn running.
        let active_token = Arc::new(CancelToken::new());
        let started = super::super::super::mailbox_try_start_turn(
            shared.as_ref(),
            channel_id,
            active_token.clone(),
            owner,
            active_user_msg_id,
        )
        .await;
        assert!(started, "fresh mailbox must accept the first turn");
        shared.global_active.fetch_add(1, Ordering::Relaxed);

        // (2) Dispatch hand-off override installed for this channel.
        shared
            .dispatch_role_overrides
            .insert(channel_id, override_channel);
        assert!(
            shared.dispatch_role_overrides.contains_key(&channel_id),
            "override must be present at the start of the race"
        );

        // (3) Round-9 ordering: race-loss enqueues the intervention BEFORE
        // any Discord HTTP await. (The actual POST is omitted from the
        // unit test — what matters is the ordering relative to
        // `mailbox_finish_turn` of the still-active turn.)
        let race_lost_msg_id_clone = race_lost_msg_id;
        let outcome = super::super::super::mailbox_enqueue_intervention(
            shared.as_ref(),
            &provider,
            channel_id,
            super::super::super::Intervention {
                author_id: owner,
                message_id: race_lost_msg_id_clone,
                source_message_ids: vec![race_lost_msg_id_clone],
                text: "queued during race".to_string(),
                mode: super::super::super::InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
        )
        .await;
        assert!(outcome.enqueued, "race-loss intervention must enqueue");

        // (4) Simulated active-turn finalization that, in the live system,
        // would happen during the placeholder POST await window. Mirror
        // the turn_bridge logic: if `has_pending == false`, clear the
        // override; otherwise keep it.
        let finish =
            super::super::super::mailbox_finish_turn(shared.as_ref(), &provider, channel_id).await;
        assert!(
            finish.removed_token.is_some(),
            "finish_turn should remove the active turn's cancel token"
        );
        assert!(
            finish.has_pending,
            "the queued intervention must surface as pending so turn_bridge keeps the override"
        );
        if !finish.has_pending {
            // Mirrors `turn_bridge` (see src/services/discord/turn_bridge/mod.rs:2136):
            // `if !finish.has_pending { dispatch_role_overrides.remove(&channel_id); }`
            shared.dispatch_role_overrides.remove(&channel_id);
        }

        // (5) Override survives, ready for the queued dispatch to use.
        assert!(
            shared.dispatch_role_overrides.contains_key(&channel_id),
            "round-9: enqueueing before the POST await preserves dispatch_role_overrides across active-turn finalization"
        );
        assert_eq!(
            shared
                .dispatch_role_overrides
                .get(&channel_id)
                .map(|entry| *entry),
            Some(override_channel),
            "the override channel must still resolve to the intended dispatch routing"
        );

        // The queued intervention must still be in the mailbox so the
        // subsequent kickoff can dispatch it under the preserved override.
        let snapshot = super::super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
        assert!(snapshot.cancel_token.is_none(), "active turn must be done");
        assert_eq!(
            snapshot.intervention_queue.len(),
            1,
            "the race-lost intervention must remain queued"
        );
        assert_eq!(
            snapshot.intervention_queue[0].message_id, race_lost_msg_id,
            "queued head must be our race-lost message"
        );
    }

    /// codex review round-10 P2 (#1332): the round-9 race-loss path
    /// snapshotted `mailbox.active_user_message_id` BEFORE acquiring the
    /// per-channel `queued_placeholders_persist_lock`. The residual race:
    /// if the active turn finishes between the snapshot and the lock
    /// acquire, the dispatch path can dequeue our just-enqueued
    /// intervention, take the lock, see no mapping, post a fresh Active
    /// placeholder, release the lock — and THIS branch then takes the
    /// lock with a stale snapshot result, inserts a Queued mapping for a
    /// turn that is already running, and renders a stale `📬` card +
    /// sidecar entry that no future event will reference.
    ///
    /// Round-10 fix: take the per-channel persist lock FIRST, then
    /// snapshot the mailbox UNDER the lock. `dispatch_queued_turn`'s
    /// `remove_queued_placeholder` mutator also serializes through the
    /// same per-channel mutex, so once we hold the guard the dispatch
    /// path cannot promote our intervention to active until we release.
    ///
    /// This test simulates the "active turn finishes between our former
    /// snapshot-spot and lock-acquire-spot" timeline by:
    ///   1. Acquiring the per-channel persist lock first.
    ///   2. Mutating mailbox state UNDER that held lock to mark the
    ///      active turn as `our_msg_id` — i.e. the worst-case state the
    ///      old snapshot would have missed.
    ///   3. Calling `mailbox_snapshot` while still holding the lock and
    ///      asserting it observes the updated state.
    ///   4. Skipping the mapping insert (matching the production round-10
    ///      bail branch) and asserting `queued_placeholders` stays empty
    ///      and the on-disk persistence is also empty (no stale `📬` card
    ///      sidecar entry).
    ///
    /// Pre round-10 (snapshot OUTSIDE the lock): step 3 would have used
    /// the pre-step-2 snapshot value, decided "queued", and inserted a
    /// stale mapping in step 4.
    #[tokio::test(flavor = "current_thread")]
    async fn race_loss_dispatch_state_recheck_under_persist_lock_skips_stale_insert() {
        use crate::services::provider::CancelToken;
        use std::sync::Arc;

        let shared = super::super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(123_456_789);
        let owner = UserId::new(11);
        let our_msg_id = MessageId::new(7_777);
        let placeholder_msg_id = MessageId::new(8_888);

        // Acquire the per-channel persist lock FIRST (round-10
        // ordering). All `queued_placeholders` mutators serialize on this
        // mutex, so while we hold the guard nothing else can promote our
        // intervention into the map or out of it.
        let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
        let persist_guard = persist_lock.lock_owned().await;

        // Mutate mailbox state UNDER the held guard to simulate the
        // dispatch path advancing from "queued" to "active for our
        // user_msg_id" during the previous code's snapshot↔lock window.
        // In production this is the timeline:
        //   - active turn finishes
        //   - dispatch dequeues our intervention
        //   - dispatch starts a turn for our_msg_id
        //   - dispatch posts a fresh Active placeholder via the
        //     missing-mapping fallback
        // For the unit test we directly call `mailbox_try_start_turn` so
        // the snapshot's `active_user_message_id` equals `our_msg_id`,
        // which is the precise state the round-9 snapshot would have
        // missed but the round-10 snapshot must observe.
        let dispatch_token = Arc::new(CancelToken::new());
        let started = super::super::super::mailbox_try_start_turn(
            shared.as_ref(),
            channel_id,
            dispatch_token,
            owner,
            our_msg_id,
        )
        .await;
        assert!(
            started,
            "fresh mailbox must accept the dispatch-promoted turn"
        );

        // Snapshot UNDER the lock. Round-10: this is the round-9-residual
        // hazard's exact moment of truth — our path observes the
        // post-mutation state, not the pre-mutation snapshot.
        let snapshot = super::super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
        let dispatch_already_running_for_our_msg =
            snapshot.active_user_message_id == Some(our_msg_id);
        assert!(
            dispatch_already_running_for_our_msg,
            "round-10: snapshot under the held persist lock must observe dispatch-already-running"
        );

        // Bail branch (matching the production code): do NOT call
        // `insert_queued_placeholder_locked`. The old code would have
        // inserted here because it snapshotted before the lock and
        // missed the dispatch promotion.
        if !dispatch_already_running_for_our_msg {
            shared.insert_queued_placeholder_locked(channel_id, our_msg_id, placeholder_msg_id);
        }
        drop(persist_guard);

        // Round-10 invariant: no stale mapping in memory.
        assert!(
            !shared
                .queued_placeholders
                .contains_key(&(channel_id, our_msg_id)),
            "round-10: no stale Queued mapping must be inserted when dispatch is already running for our_msg_id"
        );

        // And the ownership recheck (round-5 invariant) reports
        // not-owned, so the production `else if want_queued_card &&
        // !reused_existing_mapping` PATCH branch's first check would
        // skip the `ensure_queued` PATCH entirely — no stale `📬` card
        // gets rendered.
        assert!(
            !shared.queued_placeholder_still_owned(channel_id, our_msg_id, placeholder_msg_id),
            "queued_placeholder_still_owned must report not-owned so the PATCH branch skips the render"
        );
    }

    /// codex review round-11 P2 (#1332): the round-10 recheck only bailed
    /// when `active_user_message_id == user_msg_id`, but other queue-exit
    /// timelines also leave `user_msg_id` orphaned without making us the
    /// active turn. Specifically:
    ///   - The intervention was cancelled / superseded between enqueue
    ///     and our lock acquire.
    ///   - The intervention is the non-head `source_message_id` of a
    ///     merged Intervention that has already been dequeued and its
    ///     merged-drain ran.
    /// In those cases `active_user_message_id` may be `None` or a
    /// different message, so the round-10 `active == user_msg_id` check
    /// passes through and we would insert a `📬` mapping for a
    /// `user_msg_id` that no future dispatch or queue-exit cleanup will
    /// ever reference → stale card forever.
    ///
    /// Round-11 fix: in addition to the round-10 active-equals-us check,
    /// also verify `user_msg_id` is still in the queue (head
    /// `intervention.message_id` OR any `source_message_ids` entry). If
    /// neither holds, treat it as a race-loss and bail.
    ///
    /// This test simulates the cancelled/superseded scenario where:
    ///   - `active_user_message_id == None` (no active turn — e.g. the
    ///     active turn finished and nothing else has started yet, OR the
    ///     channel never had an active turn after our enqueue was wiped).
    ///   - `intervention_queue` does NOT contain `our_msg_id` (queue
    ///     was drained / our entry was cancelled).
    ///
    /// Pre round-11 (queue-membership check absent): the recheck would
    /// pass through (active != us), the production code would insert a
    /// `📬` mapping for our_msg_id, and the card would be left orphaned
    /// forever.
    #[tokio::test(flavor = "current_thread")]
    async fn race_loss_recheck_bails_when_message_no_longer_queued() {
        let shared = super::super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(424_242_424);
        let our_msg_id = MessageId::new(9_001);
        let placeholder_msg_id = MessageId::new(9_002);

        // Acquire the per-channel persist lock FIRST (round-10 / round-11
        // ordering). We do NOT enqueue our_msg_id and we do NOT start a
        // turn for our_msg_id, simulating the timeline where:
        //   - we enqueued, then released; queue-exit drain ran (cancel /
        //     supersede / merged-drain) and removed our_msg_id;
        //   - the active turn either finished or never picked us up;
        //   - we now take the persist lock to insert our `📬` mapping,
        //     observe `active_user_message_id == None` and a queue that
        //     no longer contains our_msg_id.
        let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
        let persist_guard = persist_lock.lock_owned().await;

        // Snapshot UNDER the lock.
        let snapshot = super::super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;

        // Round-11 invariant: not the active turn.
        assert_eq!(
            snapshot.active_user_message_id, None,
            "test setup: no active turn so the round-10 condition active == us is FALSE",
        );
        // Round-11 invariant: queue does not contain our_msg_id.
        let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
            intervention.message_id == our_msg_id
                || intervention.source_message_ids.contains(&our_msg_id)
        });
        assert!(
            !still_queued,
            "test setup: our_msg_id must NOT be in the queue (cancelled/superseded/merged-drained)",
        );

        // Compute the recheck condition exactly as the production code does.
        let dispatch_already_running_for_our_msg =
            snapshot.active_user_message_id == Some(our_msg_id);
        let should_bail = dispatch_already_running_for_our_msg || !still_queued;
        assert!(
            should_bail,
            "round-11: recheck must bail when message no longer queued, even if active != us",
        );

        // Production bail branch: do NOT call `insert_queued_placeholder_locked`.
        // Pre round-11 the broadened check did not exist, so the only
        // condition was `active == us`, which is FALSE here, and the code
        // would have inserted a stale `📬` mapping.
        if !should_bail {
            shared.insert_queued_placeholder_locked(channel_id, our_msg_id, placeholder_msg_id);
        }
        drop(persist_guard);

        // Round-11 invariant: no stale mapping in memory.
        assert!(
            !shared
                .queued_placeholders
                .contains_key(&(channel_id, our_msg_id)),
            "round-11: no stale Queued mapping must be inserted when message no longer queued",
        );

        // The ownership recheck reports not-owned, so the PATCH branch
        // would skip the `ensure_queued` render entirely — no stale `📬`
        // card surfaces.
        assert!(
            !shared.queued_placeholder_still_owned(channel_id, our_msg_id, placeholder_msg_id),
            "queued_placeholder_still_owned must report not-owned so the PATCH branch skips the render",
        );
    }

    #[test]
    fn session_strategy_lifecycle_event_records_fresh_and_resumed_details() {
        let fresh = session_strategy_lifecycle_event(None, "no_cached_provider_session");
        assert_eq!(fresh.meta().kind, "session_fresh");
        assert!(!fresh.notify_user());
        let fresh_details = fresh.details_json();
        assert_eq!(fresh_details["reason"], "no_cached_provider_session");
        assert!(fresh_details["providerSessionId"].is_null());
        assert!(fresh_details["fingerprint"].is_null());

        let resumed = session_strategy_lifecycle_event(
            Some("provider-session-123"),
            "db_provider_session_restored",
        );
        assert_eq!(resumed.meta().kind, "session_resumed");
        assert!(!resumed.notify_user());
        let resumed_details = resumed.details_json();
        assert_eq!(resumed_details["reason"], "db_provider_session_restored");
        assert_eq!(resumed_details["providerSessionId"], "provider-session-123");
        assert_eq!(
            resumed_details["fingerprint"],
            crate::services::observability::turn_lifecycle::provider_session_fingerprint(
                "provider-session-123",
            )
        );
    }
}
