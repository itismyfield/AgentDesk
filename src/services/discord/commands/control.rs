use poise::serenity_prelude as serenity;
use serenity::{CreateAttachment, MessageId};
use std::path::Path;
use std::sync::Arc;

use crate::db::session_transcripts;
use crate::services::provider::ProviderKind;

use super::super::catch_up::retry_state::clear_channel_discarding_catch_up_backlog;
use super::super::formatting::{send_long_message_ctx, truncate_str};
use super::super::queue_io::mailbox_cancel_queued_primary_message;
use super::super::settings::cleanup_channel_uploads;
use super::super::settings::save_bot_settings;
use super::super::turn_bridge::stop_active_turn;
use super::super::{
    Context, Error, SharedData, check_auth, mailbox_cancel_active_turn,
    saturating_decrement_global_active,
};
use super::config::{
    clear_codex_goals_reset_pending_for_channel, clear_fast_mode_reset_pending_for_channel,
    clear_fast_mode_reset_pending_for_provider, fast_mode_reset_pending_for_provider,
    fast_mode_reset_pending_key, sync_session_reset_pending,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedSessionClearBehavior {
    ResetManagedProcess,
    Noop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum SoftClearNotifyMode {
    Enqueue,
    Suppress,
}

impl SoftClearNotifyMode {
    fn should_enqueue(self) -> bool {
        matches!(self, Self::Enqueue)
    }
}

const SOFT_CLEAR_REASON_CODE: &str = "lifecycle.soft_clear";

fn soft_clear_lifecycle_notify_row(
    clear_source: &str,
    notify_mode: SoftClearNotifyMode,
) -> Option<(&'static str, String)> {
    notify_mode.should_enqueue().then(|| {
        (
            SOFT_CLEAR_REASON_CODE,
            format!("🧹 세션 클리어 ({clear_source})"),
        )
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedSessionResetBehavior {
    ResetManagedProcess,
    Noop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingSessionResetPlan {
    reset_source: &'static str,
    recreate_tmux: bool,
}

fn managed_session_clear_behavior(provider: &ProviderKind) -> ManagedSessionClearBehavior {
    match provider {
        // Claude/Codex/Qwen keep reusable local wrapper state; `/clear` must
        // drop that process/tmux state instead of sending provider-native keys.
        ProviderKind::Claude | ProviderKind::Codex | ProviderKind::Qwen => {
            ManagedSessionClearBehavior::ResetManagedProcess
        }
        ProviderKind::Gemini
        | ProviderKind::Grok
        | ProviderKind::Antigravity
        | ProviderKind::OpenCode
        | ProviderKind::Unsupported(_) => ManagedSessionClearBehavior::Noop,
    }
}

fn managed_session_reset_behavior(provider: &ProviderKind) -> ManagedSessionResetBehavior {
    match provider {
        ProviderKind::Claude => ManagedSessionResetBehavior::ResetManagedProcess,
        ProviderKind::Codex | ProviderKind::Qwen => {
            ManagedSessionResetBehavior::ResetManagedProcess
        }
        ProviderKind::Gemini
        | ProviderKind::Grok
        | ProviderKind::Antigravity
        | ProviderKind::OpenCode
        | ProviderKind::Unsupported(_) => ManagedSessionResetBehavior::Noop,
    }
}

fn pending_session_reset_plan(
    provider: &ProviderKind,
    fast_mode_reset_pending: bool,
    codex_goals_reset_pending: bool,
    model_reset_pending: bool,
) -> Option<PendingSessionResetPlan> {
    if fast_mode_reset_pending {
        return Some(PendingSessionResetPlan {
            reset_source: "fast mode reset pending",
            recreate_tmux: matches!(provider, ProviderKind::Claude | ProviderKind::Codex),
        });
    }
    if codex_goals_reset_pending {
        return Some(PendingSessionResetPlan {
            reset_source: "codex goals reset pending",
            recreate_tmux: matches!(provider, ProviderKind::Codex),
        });
    }
    if model_reset_pending {
        return Some(PendingSessionResetPlan {
            reset_source: "model session reset pending",
            recreate_tmux: false,
        });
    }
    None
}

pub(in crate::services::discord) fn reset_managed_process_session(session_name: &str) -> bool {
    let mut reset = false;
    let lingering_pid =
        crate::services::session_backend::process_session_pid(session_name).map(|pid| pid as i32);
    if let Some(handle) = crate::services::session_backend::remove_process_session(session_name) {
        crate::services::session_backend::terminate_process_handle(handle);
        reset = true;
    } else if let Some(pid) = lingering_pid {
        if let Ok(pid) = u32::try_from(pid) {
            crate::services::process::kill_pid_tree(pid);
            reset = true;
        }
    }

    #[cfg(unix)]
    if crate::services::platform::tmux::has_session(session_name) {
        crate::services::tmux_diagnostics::record_tmux_exit_reason(
            session_name,
            "managed session reset",
        );
        if crate::services::platform::tmux::kill_session(session_name, "managed session reset") {
            crate::services::tmux_common::cleanup_session_temp_files(session_name);
            reset = true;
        }
    }

    reset
}

#[cfg(unix)]
fn recreate_tmux_session(session_name: &str, reset_source: &str) -> bool {
    if !crate::services::platform::tmux::has_session(session_name) {
        return false;
    }
    crate::services::tmux_diagnostics::record_tmux_exit_reason(
        session_name,
        &format!("hard reset via {reset_source}"),
    );
    let killed = crate::services::platform::tmux::kill_session(
        session_name,
        &format!("hard reset via {reset_source}"),
    );
    if killed {
        // #892: delete persistent + legacy session temp files so the next
        // turn starts from a clean slate in the canonical location.
        crate::services::tmux_common::cleanup_session_temp_files(session_name);
    }
    killed
}

#[cfg(not(unix))]
fn recreate_tmux_session(_session_name: &str, _reset_source: &str) -> bool {
    false
}

async fn resolve_session_key_for_clear(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    if let Some(key) =
        super::super::adk_session::build_adk_session_key(shared, channel_id, provider, None).await
    {
        return Some(key);
    }

    let live_channel_name =
        channel_id
            .to_channel(http)
            .await
            .ok()
            .and_then(|channel| match channel {
                serenity::Channel::Guild(guild_channel) => Some(guild_channel.name),
                _ => None,
            });
    let channel_name = fallback_channel_name_for_clear(
        live_channel_name.as_deref(),
        super::super::resolve_thread_parent(http, channel_id).await,
        channel_id,
    )?;
    Some(build_fallback_session_key_for_clear(
        &shared.token_hash,
        provider,
        &channel_name,
    ))
}

fn fallback_channel_name_for_clear(
    live_channel_name: Option<&str>,
    thread_parent: Option<(serenity::ChannelId, Option<String>)>,
    channel_id: serenity::ChannelId,
) -> Option<String> {
    if let Some((parent_id, parent_name)) = thread_parent {
        let parent_name = parent_name.unwrap_or_else(|| parent_id.get().to_string());
        return Some(super::super::synthetic_thread_channel_name(
            &parent_name,
            channel_id,
        ));
    }

    live_channel_name.map(ToOwned::to_owned)
}

fn build_fallback_session_key_for_clear(
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
) -> String {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    super::super::adk_session::build_namespaced_session_key(token_hash, provider, &tmux_name)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn reset_channel_provider_state(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    reset_source: &str,
    reset_provider_state: bool,
    clear_history: bool,
    recreate_tmux: bool,
) -> Option<String> {
    let tmux_name = {
        let mut data = shared.core.lock().await;
        data.sessions.get_mut(&channel_id).and_then(|session| {
            if reset_provider_state {
                session.session_id = None;
                session.clear_provider_session();
            }
            if clear_history {
                session.history.clear();
            }
            session
                .channel_name
                .as_ref()
                .map(|channel_name| provider.build_tmux_session_name(channel_name))
        })
    };

    if reset_provider_state
        && let Some(session_key) =
            resolve_session_key_for_clear(http, shared, channel_id, provider).await
    {
        super::super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;
    }

    if let Some(name) = tmux_name.as_deref() {
        if reset_provider_state {
            match managed_session_reset_behavior(provider) {
                ManagedSessionResetBehavior::ResetManagedProcess => {
                    reset_managed_process_session(name);
                }
                ManagedSessionResetBehavior::Noop => {}
            }
        }
        if recreate_tmux {
            recreate_tmux_session(name, reset_source);
        }
    }

    tmux_name
}

pub(in crate::services::discord) async fn reset_provider_session_if_pending(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    fast_mode_channel_id: serenity::ChannelId,
) {
    let fast_mode_reset_pending =
        fast_mode_reset_pending_for_provider(shared, fast_mode_channel_id, provider);
    let codex_goals_reset_pending = matches!(provider, ProviderKind::Codex)
        && shared
            .overrides
            .codex_goals_session_reset_pending
            .contains(&fast_mode_channel_id);
    let model_reset_pending = shared
        .overrides
        .model_session_reset_pending
        .contains(&channel_id);

    let Some(plan) = pending_session_reset_plan(
        provider,
        fast_mode_reset_pending,
        codex_goals_reset_pending,
        model_reset_pending,
    ) else {
        sync_session_reset_pending(shared, channel_id);
        if fast_mode_channel_id != channel_id {
            sync_session_reset_pending(shared, fast_mode_channel_id);
        }
        return;
    };

    let _ = reset_channel_provider_state(
        http,
        shared,
        provider,
        channel_id,
        plan.reset_source,
        true,
        false,
        plan.recreate_tmux,
    )
    .await;

    if fast_mode_reset_pending {
        clear_fast_mode_reset_pending_for_provider(shared, fast_mode_channel_id, provider);
        persist_fast_mode_reset_marker(shared, fast_mode_channel_id, provider, false).await;
    }
    if codex_goals_reset_pending {
        clear_codex_goals_reset_pending_for_channel(shared, fast_mode_channel_id);
        persist_codex_goals_reset_marker(shared, fast_mode_channel_id, false).await;
    }
    if model_reset_pending {
        shared
            .overrides
            .model_session_reset_pending
            .remove(&channel_id);
    }
    sync_session_reset_pending(shared, channel_id);
    if fast_mode_channel_id != channel_id {
        sync_session_reset_pending(shared, fast_mode_channel_id);
    }
}

fn choose_clear_session_key(
    explicit_session_key: Option<&str>,
    resolved_session_key: Option<String>,
) -> Option<String> {
    explicit_session_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .or(resolved_session_key)
}

pub(in crate::services::discord) async fn clear_channel_session_state(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    clear_source: &str,
    notify_mode: SoftClearNotifyMode,
) -> anyhow::Result<()> {
    clear_channel_session_state_with_session_key(
        http,
        shared,
        provider,
        channel_id,
        clear_source,
        notify_mode,
        None,
    )
    .await
}

pub(in crate::services::discord) async fn clear_channel_session_state_with_session_key(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    clear_source: &str,
    notify_mode: SoftClearNotifyMode,
    explicit_session_key: Option<&str>,
) -> anyhow::Result<()> {
    if shared.pg_pool.is_none() {
        anyhow::bail!("postgres pool is required to persist a channel clear boundary");
    }
    clear_channel_session_state_fenced(
        http,
        shared,
        provider,
        channel_id,
        clear_source,
        notify_mode,
        explicit_session_key,
    )
    .await
}

/// Without a pool (tests) this skips only the transcript clear boundary.
async fn clear_channel_session_state_fenced(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    clear_source: &str,
    notify_mode: SoftClearNotifyMode,
    explicit_session_key: Option<&str>,
) -> anyhow::Result<()> {
    let boundary = match shared.pg_pool.as_ref() {
        Some(pool) => Some(session_transcripts::begin_channel_clear_boundary_tx(pool).await?),
        None => None,
    };
    // Intake and kickoff stay off the mailbox until a failed clear has stopped
    // its released turn and re-armed the restored backlog.
    let transition_guard = shared
        .acquire_session_transition(channel_id)
        .await
        .map_err(|_| anyhow::anyhow!("세션 전환 중이라 초기화하지 못했어요"))?;
    let tmux_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.as_ref())
            .map(|ch_name| provider.build_tmux_session_name(ch_name))
    };

    let cleared = clear_channel_discarding_catch_up_backlog(shared, provider, channel_id).await;
    if cleared.removed_token.is_some() {
        saturating_decrement_global_active(shared);
    }
    // A failed persist restored the backlog: keep its session and transcript,
    // stop the released turn and re-arm the backlog instead of reporting a clear.
    if let Some(error) = cleared.persistence_error {
        drop(boundary); // rolls the uncommitted boundary back
        if let Some(token) = cleared.removed_token {
            stop_active_turn(
                provider,
                &token,
                super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                clear_source,
            )
            .await;
        }
        super::super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            "clear_persist_failed",
        );
        anyhow::bail!(
            "세션을 초기화하지 못했어요: 대기열 저장에 실패해 세션과 대기열을 유지했어요 ({error})"
        );
    }
    let channel_key = channel_id.get().to_string();
    let boundary_committed = match boundary {
        Some(tx) => session_transcripts::finish_channel_clear_boundary_tx(tx, &channel_key).await,
        None => Ok(()),
    };
    drop(transition_guard);

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            cleanup_channel_uploads(channel_id);
            session.clear_provider_session();
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
    }

    shared.dispatch.role_overrides.remove(&channel_id);

    clear_fast_mode_reset_pending_for_channel(shared, channel_id);
    clear_codex_goals_reset_pending_for_channel(shared, channel_id);
    shared
        .overrides
        .model_session_reset_pending
        .remove(&channel_id);
    shared.overrides.session_reset_pending.remove(&channel_id);
    clear_all_fast_mode_reset_markers(shared, channel_id).await;
    persist_codex_goals_reset_marker(shared, channel_id, false).await;

    if let Some(token) = cleared.removed_token {
        // #1218: keep all stop sites converging on `stop_active_turn` so the
        // abort-key-then-SIGKILL ordering can never regress to the legacy
        // pair-by-hand pattern.
        stop_active_turn(
            provider,
            &token,
            super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
            clear_source,
        )
        .await;
    }

    let resolved_session_key =
        resolve_session_key_for_clear(http, shared, channel_id, provider).await;
    let session_key = choose_clear_session_key(explicit_session_key, resolved_session_key);
    if let Some(session_key) = session_key.as_deref() {
        super::super::adk_session::clear_provider_session_id(session_key, shared.api_port).await;
        super::super::adk_session::post_adk_session_status(
            Some(session_key),
            None,
            None,
            "idle",
            provider,
            None,
            Some(0),
            None,
            None,
            None,
            Some(channel_id),
            None,
            shared.api_port,
        )
        .await;
    }

    match managed_session_clear_behavior(provider) {
        ManagedSessionClearBehavior::ResetManagedProcess => {
            if let Some(name) = tmux_name {
                reset_managed_process_session(&name);
            }
        }
        ManagedSessionClearBehavior::Noop => {}
    }

    if let Some((reason_code, content)) = soft_clear_lifecycle_notify_row(clear_source, notify_mode)
    {
        // Notify bot message for clear paths that have no direct provider reply.
        crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
            shared.pg_pool.as_ref(),
            &format!("channel:{}", channel_id.get()),
            session_key.as_deref(),
            reason_code,
            &content,
        );
    }

    boundary_committed
}

/// /stop — Cancel in-progress AI request
///
/// #441: flows through mailbox_cancel_active_turn → cancel_active_token
/// → token.cancelled triggers turn_bridge loop exit → mailbox_finish_turn canonical cleanup
#[poise::command(slash_command, rename = "stop")]
pub(in crate::services::discord) async fn cmd_stop(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Issue #1005: runtime-control tier — owner-only regardless of
    // `allow_all_users`. Mirrors the text-surface gate in `handle_text_command`.
    if !super::enforce_slash_command_policy(&ctx, "/stop").await? {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/stop");

    let channel_id = ctx.channel_id();
    let forward_context =
        crate::services::session_forwarding::ForwardCallerContext::from_live_globals(
            ctx.data().shared.pg_pool.clone(),
        );
    match crate::services::session_forwarding::forward_remote_cancel_if_needed(
        &forward_context,
        &axum::http::HeaderMap::new(),
        &channel_id.get().to_string(),
        false,
    )
    .await
    {
        Ok(Some(_)) => {
            ctx.say(super::STOPPING_RESPONSE).await?;
            log_info_event!(
                "discord_cancel_acknowledged",
                channel_id = channel_id.get(),
                provider = ctx.data().provider.as_str(),
                status = "acknowledged",
            );
            return Ok(());
        }
        Ok(None) => {}
        Err(error) if error.status() == axum::http::StatusCode::NOT_FOUND => {
            ctx.say(super::NO_ACTIVE_TURN_RESPONSE).await?;
            return Ok(());
        }
        Err(error) => {
            tracing::error!(channel_id = channel_id.get(), error = %error, "/stop remote cancel failed closed");
            ctx.say("중지 요청을 owner에 전달하지 못했어요. 잠시 후 다시 시도해 주세요.")
                .await?;
            return Ok(());
        }
    }

    if let Err(error) = crate::services::session_forwarding::revalidate_local_cancel_owner(
        &forward_context,
        &channel_id.get().to_string(),
        None,
    )
    .await
    {
        tracing::error!(channel_id = channel_id.get(), error = %error, "/stop owner moved before local mutation");
        ctx.say("중지 요청 중 owner가 변경됐어요. 잠시 후 다시 시도해 주세요.")
            .await?;
        return Ok(());
    }

    let result = mailbox_cancel_active_turn(&ctx.data().shared, channel_id).await;

    match result.token {
        Some(token) => {
            if result.already_stopping {
                ctx.say(super::ALREADY_STOPPING_RESPONSE).await?;
                return Ok(());
            }

            ctx.say(super::STOPPING_RESPONSE).await?;

            // #1218: stop_active_turn keeps the abort-key-then-SIGKILL order
            // identical across every stop entrypoint.
            stop_active_turn(
                &ctx.data().provider,
                &token,
                super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
                "/stop",
            )
            .await;
            // #5176 — "the interrupt was sent (or deliberately skipped)" is not
            // cancel success. When the runtime this turn belonged to is already
            // gone, the stop delivery layer decides `skip_pre_generation` and
            // there is nobody left to run the turn-bridge exit that would
            // normally release the mailbox. Without this the channel keeps a
            // foreground anchor forever and every queued user message stays
            // locked behind it. The guard inside
            // `release_zombie_foreground_turn` is what keeps a live turn safe.
            let release = super::super::zombie_foreground_release::release_zombie_foreground_turn(
                &ctx.data().shared,
                &ctx.data().provider,
                channel_id,
                "/stop",
            )
            .await;
            log_info_event!(
                "discord_cancel_signal_sent",
                channel_id = channel_id.get(),
                provider = ctx.data().provider.as_str(),
                status = if release.released { "released" } else { "sent" },
                mailbox_foreground_released = release.released,
                zombie_verdict = release.verdict_str(),
                queue_depth_after = release.queue_depth_after,
                queue_kickoff_scheduled = release.queue_kickoff_scheduled,
            );
        }
        None => {
            ctx.say(super::NO_ACTIVE_TURN_RESPONSE).await?;
        }
    }
    Ok(())
}

pub(super) fn parse_queued_message_id(raw: &str) -> Option<MessageId> {
    raw.trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .map(MessageId::new)
}

/// /cancel-queued — Remove one queued message without affecting active work.
///
/// The target is an exact Discord message id. The mailbox actor serializes the
/// removal against dispatch, so a queued-to-active race is reported as stale
/// instead of cancelling the newly active turn.
#[poise::command(slash_command, rename = "cancel-queued")]
pub(in crate::services::discord) async fn cmd_cancel_queued(
    ctx: Context<'_>,
    #[description = "Queued Discord message ID"] message_id: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    if !super::enforce_slash_command_policy(&ctx, "/cancel-queued").await? {
        return Ok(());
    }

    let Some(message_id) = parse_queued_message_id(&message_id) else {
        ctx.say("유효한 큐 메시지 ID를 입력해 주세요.").await?;
        return Ok(());
    };

    let removed = mailbox_cancel_queued_primary_message(
        &ctx.data().shared,
        &ctx.data().provider,
        ctx.channel_id(),
        message_id,
    )
    .await;
    if removed.is_some() {
        ctx.say(format!("큐 메시지 `{}`를 취소했어요.", message_id.get()))
            .await?;
    } else {
        ctx.say(format!(
            "큐 메시지 `{}`는 이미 처리됐거나 현재 채널의 대기열에 없어요.",
            message_id.get()
        ))
        .await?;
    }
    Ok(())
}

/// /clear — Clear AI conversation history
#[poise::command(slash_command, rename = "clear")]
pub(in crate::services::discord) async fn cmd_clear(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Issue #1005: runtime-control tier — owner-only.
    if !super::enforce_slash_command_policy(&ctx, "/clear").await? {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/clear");

    let http = ctx.serenity_context().http.clone();
    clear_channel_session_state(
        &http,
        &ctx.data().shared,
        &ctx.data().provider,
        ctx.channel_id(),
        "/clear",
        SoftClearNotifyMode::Suppress,
    )
    .await?;

    ctx.say(super::SESSION_CLEARED_RESPONSE).await?;
    log_info_event!(
        "discord_session_cleared",
        channel_id = ctx.channel_id().get(),
        provider = ctx.data().provider.as_str(),
        user_name = %user_name,
        status = "cleared",
    );
    Ok(())
}

#[cfg(test)]
mod soft_clear_notify_tests {
    use poise::serenity_prelude::MessageId;

    use super::{
        SOFT_CLEAR_REASON_CODE, SoftClearNotifyMode, choose_clear_session_key,
        parse_queued_message_id, soft_clear_lifecycle_notify_row,
    };

    #[test]
    fn slash_and_text_clear_suppress_soft_clear_notify_row() {
        assert_eq!(
            soft_clear_lifecycle_notify_row("/clear", SoftClearNotifyMode::Suppress),
            None,
            "`/clear` and `!clear` already reply with the shared clear response and must not enqueue a duplicate `lifecycle.soft_clear` notify row"
        );
        assert_eq!(
            soft_clear_lifecycle_notify_row("!clear", SoftClearNotifyMode::Suppress),
            None,
            "`!clear` should leave the provider reply as the single user-visible completion surface"
        );
    }

    #[test]
    fn queued_cancel_parser_accepts_exact_nonzero_ids_only() {
        assert_eq!(parse_queued_message_id(" 42 "), Some(MessageId::new(42)));
        assert_eq!(parse_queued_message_id("0"), None);
        assert_eq!(parse_queued_message_id("stale"), None);
    }

    #[test]
    fn idle_recap_clear_keeps_soft_clear_notify_row() {
        assert_eq!(
            soft_clear_lifecycle_notify_row("idle_recap_clear", SoftClearNotifyMode::Enqueue),
            Some((
                SOFT_CLEAR_REASON_CODE,
                "🧹 세션 클리어 (idle_recap_clear)".to_string(),
            )),
            "idle recap clear has no provider reply, so it must keep the single user-visible `lifecycle.soft_clear` notify row"
        );
    }

    #[test]
    fn explicit_recap_session_key_wins_over_recomputed_channel_key() {
        assert_eq!(
            choose_clear_session_key(
                Some("claude/token/host:AgentDesk-claude-old-channel"),
                Some("claude/token/host:AgentDesk-claude-renamed-channel".to_string()),
            )
            .as_deref(),
            Some("claude/token/host:AgentDesk-claude-old-channel"),
            "idle recap clear must drop the exact session row that owns the recap card, even when channel-name drift changes the recomputed key"
        );
    }

    #[test]
    fn blank_explicit_session_key_falls_back_to_recomputed_key() {
        assert_eq!(
            choose_clear_session_key(
                Some("  "),
                Some("claude/token/host:AgentDesk-claude-channel".to_string()),
            )
            .as_deref(),
            Some("claude/token/host:AgentDesk-claude-channel")
        );
    }
}

/// An intentional clear whose queue persist fails keeps the session, the
/// transcript and the restored backlog, and never reports a clear.
#[cfg(test)]
mod clear_persist_failure_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use poise::serenity_prelude as serenity;
    use serenity::{ChannelId, MessageId, UserId};

    use super::{
        SoftClearNotifyMode, clear_channel_session_state, clear_channel_session_state_fenced,
    };
    use crate::services::discord::{DiscordSession, SharedData, make_shared_data_for_tests};
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::turn_orchestrator::{
        Intervention, InterventionMode, QueuePersistenceContext,
    };

    const SESSION_ID: &str = "provider-session-6233";
    const CHANNEL_NAME: &str = "adk-6233-clear-persist";

    fn queued(message_id: u64) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "restored backlog".to_string(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    async fn seed_session(shared: &Arc<SharedData>, channel_id: ChannelId) {
        shared.core.lock().await.sessions.insert(
            channel_id,
            DiscordSession {
                session_id: Some(SESSION_ID.to_string()),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),
                // A resolvable channel name keeps session-key lookup off Discord.
                channel_name: Some(CHANNEL_NAME.to_string()),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: shared.restart.current_generation,
            },
        );
    }

    async fn seed_backlog(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) {
        let persistence = QueuePersistenceContext::new(provider, &shared.token_hash, None);
        let enqueued = shared
            .mailbox(channel_id)
            .enqueue(queued(channel_id.get() + 1), persistence)
            .await;
        assert!(enqueued.enqueued, "the backlog is queued and persisted");
    }

    fn queue_file(
        root: &Path,
        shared: &SharedData,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> PathBuf {
        root.join("runtime/discord_pending_queue")
            .join(provider.as_str())
            .join(&shared.token_hash)
            .join(format!("{}.json", channel_id.get()))
    }

    /// An emptied queue is persisted by removing its file, so a directory in
    /// that file's place makes the clear's persist fail.
    fn break_queue_persist(path: &Path) {
        std::fs::remove_file(path).expect("persisted queue file");
        std::fs::create_dir(path).expect("directory in the queue file's place");
    }

    fn paused_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("test runtime")
    }

    async fn clear(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> anyhow::Result<()> {
        let http = Arc::new(serenity::Http::new(""));
        clear_channel_session_state_fenced(
            &http,
            shared,
            provider,
            channel_id,
            "/clear",
            SoftClearNotifyMode::Suppress,
            None,
        )
        .await
    }

    async fn session_state(shared: &SharedData, channel_id: ChannelId) -> (Option<String>, bool) {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id).expect("session kept");
        (session.session_id.clone(), session.cleared)
    }

    async fn queue_len(shared: &SharedData, channel_id: ChannelId) -> usize {
        shared
            .mailbox(channel_id)
            .snapshot()
            .await
            .intervention_queue
            .len()
    }

    fn global_active(shared: &SharedData) -> usize {
        shared.restart.global_active.load(Ordering::SeqCst)
    }

    #[test]
    fn failed_clear_persist_is_not_reported_as_cleared_and_keeps_the_session() {
        let root = tempfile::tempdir().expect("scratch runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let provider = ProviderKind::Gemini;
        let channel_id = ChannelId::new(6_233_101);
        paused_runtime().block_on(async {
            let shared = make_shared_data_for_tests();
            seed_session(&shared, channel_id).await;
            seed_backlog(&shared, &provider, channel_id).await;
            break_queue_persist(&queue_file(root.path(), &shared, &provider, channel_id));

            let result = clear(&shared, &provider, channel_id).await;

            assert!(
                result.is_err(),
                "a clear whose queue persist failed must not return the success the caller replies with"
            );
            assert_eq!(queue_len(&shared, channel_id).await, 1, "the failed persist restores the backlog");
            assert_eq!(
                session_state(&shared, channel_id).await,
                (Some(SESSION_ID.to_string()), false),
                "the restored backlog must not resume in a provider session this clear reset"
            );
            assert!(
                shared.restart.deferred_hook_channels.contains_key(&channel_id),
                "the restored backlog is re-armed through the idle-queue kickoff"
            );
        });
    }

    #[test]
    fn failed_clear_holds_the_transition_through_the_stop_and_one_restored_turn_runs() {
        let root = tempfile::tempdir().expect("scratch runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let provider = ProviderKind::Gemini;
        let channel_id = ChannelId::new(6_233_103);
        paused_runtime().block_on(async {
            let shared = make_shared_data_for_tests();
            seed_session(&shared, channel_id).await;
            // A tmux-bound token parks the stop in its hard-stop grace; the name
            // is absent, so nothing is signalled or killed.
            let token = Arc::new(CancelToken::new());
            token.bind_unmanaged_session_name(&format!("adk-6233-absent-{}", std::process::id()));
            assert!(
                crate::services::discord::mailbox_try_start_turn(
                    &shared,
                    channel_id,
                    token.clone(),
                    UserId::new(1),
                    MessageId::new(6_233_100),
                )
                .await,
                "the turn being cleared holds the mailbox"
            );
            // Another channel's turn keeps one more count, so a missing or a
            // doubled decrement is visible.
            crate::services::discord::increment_global_active(&shared, "other channel turn");
            crate::services::discord::increment_global_active(&shared, "cleared turn");
            seed_backlog(&shared, &provider, channel_id).await;
            let queue_path = queue_file(root.path(), &shared, &provider, channel_id);
            break_queue_persist(&queue_path);

            let starts = Arc::new(AtomicUsize::new(0));
            let hook_starts = starts.clone();
            let _hook = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests(
                Arc::new(move |shared, provider, channel, _reason| {
                    let hook_starts = hook_starts.clone();
                    Box::pin(async move {
                        if channel != channel_id {
                            return None;
                        }
                        let taken = crate::services::discord::idle_queue_take_next_soft_if_ready(
                            &shared, &provider, channel,
                        )
                        .await;
                        let started = taken.intervention.is_some();
                        if started {
                            hook_starts.fetch_add(1, Ordering::SeqCst);
                        }
                        Some(crate::services::discord::IdleQueueKickoffChannelOutcome { started })
                    })
                }),
            );

            let clear_task = tokio::spawn({
                let shared = shared.clone();
                let provider = provider.clone();
                async move { clear(&shared, &provider, channel_id).await }
            });
            let mut spins = 0;
            while !token.cancelled.load(Ordering::SeqCst) {
                assert!(
                    spins < 100_000,
                    "the failed clear reaches the released turn's stop"
                );
                spins += 1;
                tokio::task::yield_now().await;
            }
            assert!(
                !clear_task.is_finished(),
                "the stop is parked in its hard-stop grace"
            );
            assert!(
                shared
                    .session_transition_lock(channel_id)
                    .try_lock_owned()
                    .is_err(),
                "kickoff must not claim the mailbox while the failed clear stops its released turn"
            );
            assert!(
                crate::services::discord::try_intake_runtime_transition_after_redirect(
                    &shared,
                    channel_id,
                    (None, false, String::new()),
                )
                .await
                .is_err(),
                "intake must defer while the failed clear stops its released turn"
            );

            let result = clear_task.await.expect("clear task");
            assert!(
                result.is_err(),
                "the failed clear is not reported as cleared"
            );
            assert_eq!(
                global_active(&shared),
                1,
                "the released turn is counted down exactly once"
            );

            // Storage recovers; the released turn's late finalizer finds no anchor
            // and its re-kick coalesces with the one the clear armed.
            std::fs::remove_dir(&queue_path).expect("storage recovers");
            let finish =
                crate::services::discord::mailbox_finish_turn(&shared, &provider, channel_id).await;
            assert!(
                finish.removed_token.is_none(),
                "the clear already released the anchor"
            );
            crate::services::discord::schedule_deferred_idle_queue_kickoff(
                shared.clone(),
                provider.clone(),
                channel_id,
                "released_turn_finalizer",
            );
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;

            assert_eq!(
                starts.load(Ordering::SeqCst),
                1,
                "exactly one restored turn runs"
            );
            assert_eq!(
                queue_len(&shared, channel_id).await,
                0,
                "the restored backlog is drained"
            );
            assert_eq!(
                session_state(&shared, channel_id).await,
                (Some(SESSION_ID.to_string()), false),
                "the restored turn runs on the preserved session"
            );
            assert_eq!(
                global_active(&shared),
                1,
                "the late finalizer does not count down again"
            );
        });
    }

    #[test]
    fn persisted_clear_still_resets_the_session_and_arms_no_kick() {
        let root = tempfile::tempdir().expect("scratch runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let provider = ProviderKind::Gemini;
        let channel_id = ChannelId::new(6_233_102);
        paused_runtime().block_on(async {
            let shared = make_shared_data_for_tests();
            seed_session(&shared, channel_id).await;
            seed_backlog(&shared, &provider, channel_id).await;

            assert!(
                clear(&shared, &provider, channel_id).await.is_ok(),
                "a persisted clear still succeeds"
            );
            assert_eq!(
                queue_len(&shared, channel_id).await,
                0,
                "the backlog is discarded"
            );
            assert_eq!(
                session_state(&shared, channel_id).await,
                (None, true),
                "the provider session is reset"
            );
            assert!(
                !shared
                    .restart
                    .deferred_hook_channels
                    .contains_key(&channel_id),
                "an empty cleared queue arms no kickoff"
            );
            assert!(
                shared
                    .session_transition_lock(channel_id)
                    .try_lock_owned()
                    .is_ok(),
                "a persisted clear releases the transition"
            );
        });
    }

    #[test]
    fn persisted_clear_still_resets_the_managed_process() {
        let root = tempfile::tempdir().expect("scratch runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(6_233_104);
        let session_name = provider.build_tmux_session_name(CHANNEL_NAME);
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        crate::services::session_backend::insert_process_session(
            session_name.clone(),
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 6_233_104,
                alive: alive.clone(),
            },
        );
        paused_runtime().block_on(async {
            let shared = make_shared_data_for_tests();
            seed_session(&shared, channel_id).await;
            seed_backlog(&shared, &provider, channel_id).await;

            assert!(
                clear(&shared, &provider, channel_id).await.is_ok(),
                "a persisted clear still succeeds"
            );
            assert_eq!(
                session_state(&shared, channel_id).await,
                (None, true),
                "the provider session is reset"
            );
        });
        assert!(
            !alive.load(Ordering::SeqCst),
            "the managed process is terminated"
        );
        assert!(
            crate::services::session_backend::remove_process_session(&session_name).is_none(),
            "the managed process session is removed"
        );
    }

    /// Each caller propagates a failed clear before it replies or deletes the
    /// recap card, so no caller reports a clear that did not happen.
    #[test]
    fn clear_callers_propagate_a_failed_clear_before_their_success_effect() {
        let callers = [
            (
                include_str!("control.rs"),
                "async fn cmd_clear(",
                "clear_channel_session_state(",
                "SESSION_CLEARED_RESPONSE",
            ),
            (
                include_str!("text_commands.rs"),
                "TextCommandId::Clear =>",
                "clear_channel_session_state(",
                "SESSION_CLEARED_RESPONSE",
            ),
            (
                include_str!("../idle_recap_interaction.rs"),
                "async fn handle_idle_recap_clear_interaction(",
                "clear_channel_session_state_with_session_key(",
                "delete_previous_card(",
            ),
        ];
        for (source, handler, call, success) in callers {
            let body = &source[source.find(handler).expect(handler)..];
            let call_at = body.find(call).expect(call);
            let success_at = call_at + body[call_at..].find(success).expect(success);
            let between = &body[call_at..success_at];
            let awaited = between.find(".await").expect("the clear is awaited");
            assert!(
                between[awaited..].starts_with(".await?;"),
                "{handler} must propagate the clear's error before {success}"
            );
        }
    }

    async fn boundary_rows(pool: &sqlx::PgPool, channel_id: ChannelId) -> Vec<i64> {
        sqlx::query_scalar(
            "SELECT clear_generation::BIGINT FROM channel_session_clear_boundaries WHERE channel_id = $1",
        )
        .bind(channel_id.get().to_string())
        .fetch_all(pool)
        .await
        .expect("read clear boundaries")
    }

    #[test]
    fn clear_commits_its_transcript_boundary_only_after_the_queue_persists_pg() {
        let root = tempfile::tempdir().expect("scratch runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let provider = ProviderKind::Gemini;
        let channel_id = ChannelId::new(6_233_105);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
                "agentdesk_clear_boundary_6233",
                "clear transcript boundary",
            )
            .await;
            let pool = db.connect_and_migrate_with_max_connections(4).await;
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            seed_session(&shared, channel_id).await;
            seed_backlog(&shared, &provider, channel_id).await;
            let queue_path = queue_file(root.path(), &shared, &provider, channel_id);
            break_queue_persist(&queue_path);
            let http = Arc::new(serenity::Http::new(""));

            // `/clear` and `!clear` both call this entry.
            let failed = clear_channel_session_state(
                &http,
                &shared,
                &provider,
                channel_id,
                "/clear",
                SoftClearNotifyMode::Suppress,
            )
            .await;
            assert!(
                failed.is_err(),
                "the failed clear is propagated to its caller"
            );
            assert!(
                boundary_rows(&pool, channel_id).await.is_empty(),
                "a failed clear leaves the transcript history of the kept session"
            );

            std::fs::remove_dir(&queue_path).expect("storage recovers");
            let persisted = clear_channel_session_state(
                &http,
                &shared,
                &provider,
                channel_id,
                "/clear",
                SoftClearNotifyMode::Suppress,
            )
            .await;
            assert!(
                persisted.is_ok(),
                "a persisted clear succeeds: {persisted:?}"
            );
            assert_eq!(
                boundary_rows(&pool, channel_id).await,
                [1],
                "a persisted clear commits its boundary"
            );

            pool.close().await;
            db.drop().await;
        });
    }
}

/// /down <file> — Download file from server
#[poise::command(slash_command, rename = "down")]
pub(in crate::services::discord) async fn cmd_down(
    ctx: Context<'_>,
    #[description = "File path to download"] file: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/down", path = %file);

    let file_path = file.trim();
    if file_path.is_empty() {
        ctx.say("Usage: `/down <filepath>`\nExample: `/down /home/user/file.txt`")
            .await?;
        return Ok(());
    }

    // Resolve relative path
    let resolved_path = if Path::new(file_path).is_absolute() {
        file_path.to_string()
    } else {
        let current_path = {
            let mut data = ctx.data().shared.core.lock().await;
            data.sessions
                .get_mut(&ctx.channel_id())
                .and_then(|s| s.validated_path(ctx.channel_id()))
        };
        match current_path {
            Some(base) => format!("{}/{}", base.trim_end_matches('/'), file_path),
            None => {
                ctx.say("No active session or session path is stale. Use absolute path or `/start <path>` first.")
                    .await?;
                return Ok(());
            }
        }
    };

    let path = Path::new(&resolved_path);
    if !path.exists() {
        ctx.say(format!("File not found: {}", resolved_path))
            .await?;
        return Ok(());
    }
    if !path.is_file() {
        ctx.say(format!("Not a file: {}", resolved_path)).await?;
        return Ok(());
    }

    // Send file as attachment
    let attachment = CreateAttachment::path(path).await?;
    ctx.send(poise::CreateReply::default().attachment(attachment))
        .await?;

    Ok(())
}

/// /shell <command> — Run shell command directly
#[poise::command(slash_command, rename = "shell")]
pub(in crate::services::discord) async fn cmd_shell(
    ctx: Context<'_>,
    #[description = "Shell command to execute"] command: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Issue #1005: shell/tool-grant tier — owner-only AND default-disabled.
    // Even `allow_all_users=true` must NOT unlock RCE.
    if !super::enforce_slash_command_policy(&ctx, "/shell").await? {
        return Ok(());
    }

    let preview = truncate_str(&command, 60);
    log_command_received!(ctx.channel_id().get(), user_name, "/shell", command_preview = %preview);

    // Defer for potentially long-running commands
    ctx.defer().await?;

    let working_dir = {
        let mut data = ctx.data().shared.core.lock().await;
        data.sessions
            .get_mut(&ctx.channel_id())
            .and_then(|s| s.validated_path(ctx.channel_id()))
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| "/".to_string())
            })
    };

    let cmd_owned = command.clone();
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
                parts.push(super::owner_error_response(
                    "셸 명령이 오류 출력을 반환했어요.",
                    stderr.trim_end(),
                ));
            }
            if parts.is_empty() {
                parts.push(format!("(종료 코드: {})", exit_code));
            } else if exit_code != 0 {
                parts.push(format!("(종료 코드: {})", exit_code));
            }
            parts.join("\n")
        }
        Ok(Err(e)) => super::owner_error_response("셸 명령을 실행하지 못했어요.", &e.to_string()),
        Err(e) => {
            super::owner_error_response("셸 명령을 처리하는 중 오류가 발생했어요.", &e.to_string())
        }
    };

    send_long_message_ctx(ctx, &response).await?;
    log_info_event!(
        "discord_shell_command_completed",
        channel_id = ctx.channel_id().get(),
        user_name = %user_name,
        status = "completed",
    );
    Ok(())
}

async fn persist_fast_mode_reset_marker(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    pending: bool,
) {
    let Some(token) = shared.http.cached_bot_token.get() else {
        return;
    };

    let channel_key = channel_id.get().to_string();
    let provider_key = fast_mode_reset_pending_key(channel_id, provider);
    let mut settings = shared.settings.write().await;
    if pending {
        settings
            .channel_fast_mode_reset_pending
            .remove(&channel_key);
        settings
            .channel_fast_mode_reset_pending
            .insert(provider_key);
    } else {
        settings
            .channel_fast_mode_reset_pending
            .remove(&channel_key);
        settings
            .channel_fast_mode_reset_pending
            .remove(&provider_key);
    }
    save_bot_settings(token, &settings);
}

async fn persist_codex_goals_reset_marker(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    pending: bool,
) {
    let Some(token) = shared.http.cached_bot_token.get() else {
        return;
    };

    let channel_key = channel_id.get().to_string();
    let mut settings = shared.settings.write().await;
    if pending {
        settings
            .channel_codex_goals_reset_pending
            .insert(channel_key);
    } else {
        settings
            .channel_codex_goals_reset_pending
            .remove(&channel_key);
    }
    save_bot_settings(token, &settings);
}

async fn clear_all_fast_mode_reset_markers(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) {
    let Some(token) = shared.http.cached_bot_token.get() else {
        return;
    };

    let channel_key = channel_id.get().to_string();
    let suffix = format!(":{channel_key}");
    let mut settings = shared.settings.write().await;
    settings
        .channel_fast_mode_reset_pending
        .retain(|entry| entry != &channel_key && !entry.ends_with(&suffix));
    save_bot_settings(token, &settings);
}
