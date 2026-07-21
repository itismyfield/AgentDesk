use super::*;

pub(super) const WATCHDOG_DEADLOCK_PREALERT_MS: i64 = 5 * 60 * 1000;
pub(super) const WATCHDOG_DEADLOCK_PREALERT_BOT: &str =
    crate::services::discord::bot_role::UtilityBotRole::Announce.alias();
pub(super) const WATCHDOG_TIMEOUT_REASON: &str = "watchdog timeout";
pub(super) const WATCHDOG_TIMEOUT_CANCEL_SOURCE: &str = "watchdog_timeout";
#[cfg(not(test))]
const PAUSED_WATCHER_COLD_START_RETRY_ATTEMPTS: u32 = 180;
#[cfg(test)]
const PAUSED_WATCHER_COLD_START_RETRY_ATTEMPTS: u32 = 20;
#[cfg(not(test))]
const PAUSED_WATCHER_COLD_START_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_secs(1);
#[cfg(test)]
const PAUSED_WATCHER_COLD_START_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_millis(10);

pub(super) fn parse_watchdog_alert_channel_id(raw: &str) -> Option<serenity::ChannelId> {
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

pub(super) fn configured_watchdog_alert_channel_id() -> Option<serenity::ChannelId> {
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

pub(super) fn should_send_watchdog_deadlock_prealert(
    now_ms: i64,
    deadline_ms: i64,
    last_notified_deadline_ms: Option<i64>,
) -> bool {
    now_ms < deadline_ms
        && now_ms >= deadline_ms - WATCHDOG_DEADLOCK_PREALERT_MS
        && last_notified_deadline_ms != Some(deadline_ms)
}

pub(super) fn apply_watchdog_deadline_extension(
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

pub(super) fn build_watchdog_deadlock_prealert_message(
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

pub(super) fn build_watchdog_timeout_notice_message(elapsed_mins: i64, has_queued: bool) -> String {
    if has_queued {
        format!(
            "⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다. 대기 중인 메시지로 다음 턴을 시작합니다.",
        )
    } else {
        format!("⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다.",)
    }
}

pub(super) async fn send_watchdog_timeout_notice(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    http: &Arc<serenity::http::Http>,
    elapsed_mins: i64,
) {
    let has_queued =
        super::super::super::mailbox_has_pending_soft_queue(shared, provider, channel_id)
            .await
            .has_pending;
    let message = build_watchdog_timeout_notice_message(elapsed_mins, has_queued);
    if let Err(error) = channel_id.say(http, message).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⏰ WATCHDOG: failed timeout notice for channel {}: {}",
            channel_id,
            error
        );
    }
}

fn headless_inflight_has_watchdog_visible_surface(inflight: &InflightTurnState) -> bool {
    if inflight.rebind_origin || inflight.relay_ownership_only {
        return false;
    }
    inflight.long_running_placeholder_active
        || inflight.task_notification_kind.is_some()
        || (inflight.current_msg_id != 0
            && !super::super::super::is_synthetic_headless_message_id_raw(inflight.current_msg_id))
}

pub(super) fn headless_watchdog_timeout_notice_visible_from_surfaces(
    inflight: Option<&InflightTurnState>,
    footer_has_unfinished_entries: bool,
) -> bool {
    footer_has_unfinished_entries
        || inflight.is_some_and(headless_inflight_has_watchdog_visible_surface)
}

pub(super) fn headless_watchdog_timeout_notice_visible(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> bool {
    let inflight = super::super::super::inflight::load_inflight_state(provider, channel_id.get());
    let footer_has_unfinished_entries = shared.ui.status_panel_v2_enabled
        && shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, provider, "⠸")
            .has_unfinished_entries;
    headless_watchdog_timeout_notice_visible_from_surfaces(
        inflight.as_ref(),
        footer_has_unfinished_entries,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn maybe_send_headless_watchdog_timeout_notice(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    http: &Arc<serenity::http::Http>,
    timeout: std::time::Duration,
    current_deadline: i64,
    now: i64,
    visible: bool,
) {
    let elapsed_mins = (now - (current_deadline - timeout.as_millis() as i64)) / 1000 / 60;
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⏰ Headless watchdog timeout reconciled via cancel path for channel {}",
        channel_id
    );
    if visible {
        send_watchdog_timeout_notice(shared, provider, channel_id, http, elapsed_mins).await;
    }
}

pub(super) fn spawn_headless_turn_watchdog(
    cancel_token: &Arc<CancelToken>,
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    provider_label: &str,
) {
    let watchdog_token = cancel_token.clone();
    let watchdog_shared = shared.clone();
    let watchdog_http = http.clone();
    let timeout = super::super::super::turn_watchdog_timeout();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let turn_started_ms = now_ms;
    let ceiling_deadline_ms =
        super::super::super::turn_hard_ceiling_deadline_ms(turn_started_ms, provider);
    let proposed_initial_dl = now_ms + timeout.as_millis() as i64;
    let deadline_ms = std::cmp::min(proposed_initial_dl, ceiling_deadline_ms);
    let max_deadline_ms = deadline_ms;
    if proposed_initial_dl > ceiling_deadline_ms {
        let ts = chrono::Local::now().format("%H:%M:%S");
        let ceiling_min = (ceiling_deadline_ms - now_ms) / 1000 / 60;
        tracing::warn!(
            "  [{ts}] ⛔ WATCHDOG: hard ceiling ({ceiling_min}m) caps initial deadline for headless channel {} (provider={}) — turn will be reconciled at the ceiling",
            channel_id.get(),
            provider_label
        );
    }
    watchdog_token.mark_async_managed();
    watchdog_token
        .watchdog_deadline_ms
        .store(deadline_ms, std::sync::atomic::Ordering::Relaxed);
    watchdog_token
        .watchdog_max_deadline_ms
        .store(max_deadline_ms, std::sync::atomic::Ordering::Relaxed);

    let watchdog_channel_id_num = channel_id.get();
    let watchdog_provider = provider.clone();
    super::super::super::task_supervisor::spawn_observed("headless_turn_watchdog", async move {
        const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
        let mut last_deadlock_prealert_deadline_ms: Option<i64> = None;

        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;
            if watchdog_token
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                super::super::super::clear_watchdog_deadline_override(watchdog_channel_id_num)
                    .await;
                return;
            }
            if let Some(extension) =
                super::super::super::take_watchdog_deadline_override(watchdog_channel_id_num).await
            {
                apply_watchdog_deadline_extension(&watchdog_token, extension);
                last_deadlock_prealert_deadline_ms = None;
            }
            {
                let current_dl = watchdog_token
                    .watchdog_deadline_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                let now_ms_check = chrono::Utc::now().timestamp_millis();
                if now_ms_check > current_dl - 120_000
                    && let Some(inflight) = super::super::super::inflight::load_inflight_state(
                        &watchdog_provider,
                        watchdog_channel_id_num,
                    )
                    && let Ok(updated) = chrono::NaiveDateTime::parse_from_str(
                        &inflight.updated_at,
                        "%Y-%m-%d %H:%M:%S",
                    )
                {
                    let updated_ms = updated.and_utc().timestamp_millis();
                    let age_ms = now_ms_check - updated_ms;
                    if age_ms < 300_000 {
                        let ceiling_ms = super::super::super::turn_hard_ceiling_deadline_ms(
                            turn_started_ms,
                            &watchdog_provider,
                        );
                        let proposed_dl = now_ms_check + timeout.as_millis() as i64;
                        let (new_dl, clamped) = super::super::super::clamp_auto_extend_deadline_ms(
                            proposed_dl,
                            ceiling_ms,
                        );
                        if clamped && current_dl < ceiling_ms {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⛔ WATCHDOG: hard ceiling reached for headless channel {} — auto-extend clamped, turn will be reconciled at deadline",
                                watchdog_channel_id_num
                            );
                        }
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
                    super::super::super::mailbox_cancel_token(&watchdog_shared, channel_id)
                        .await
                        .is_some_and(|current| Arc::ptr_eq(&watchdog_token, &current));
                if !is_current_token {
                    super::super::super::clear_watchdog_deadline_override(watchdog_channel_id_num)
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
                super::super::super::take_watchdog_deadline_override(watchdog_channel_id_num).await
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

            // Must be computed BEFORE reconcile_watchdog_timeout: reconcile
            // clears the inflight row, and the visibility surfaces live on it.
            let should_emit_timeout_notice = headless_watchdog_timeout_notice_visible(
                watchdog_shared.as_ref(),
                &watchdog_provider,
                channel_id,
            );
            let disposition = reconcile_watchdog_timeout(
                &watchdog_shared,
                &watchdog_provider,
                channel_id,
                &watchdog_token,
            )
            .await;
            if disposition == WatchdogTimeoutCancelDisposition::Cancelled {
                maybe_send_headless_watchdog_timeout_notice(
                    &watchdog_shared,
                    &watchdog_provider,
                    channel_id,
                    &watchdog_http,
                    timeout,
                    current_deadline,
                    now,
                    should_emit_timeout_notice,
                )
                .await;
            }
            return;
        }
    });
}

pub(super) async fn maybe_send_watchdog_deadlock_prealert(
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
    let alert_http = match super::super::super::health::resolve_bot_http(
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
    let inflight = super::super::super::inflight::load_inflight_state(provider, channel_id.get());
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
pub(super) enum WatchdogTimeoutCancelDisposition {
    Cancelled,
    AlreadyStopping,
    StaleToken,
}

pub(super) fn watchdog_timeout_turn_id(inflight: &InflightTurnState) -> Option<String> {
    (inflight.user_msg_id != 0)
        .then(|| format!("discord:{}:{}", inflight.channel_id, inflight.user_msg_id))
}

pub(super) fn watchdog_timeout_cancel_request(
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

pub(super) async fn reconcile_watchdog_timeout(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    watchdog_token: &Arc<CancelToken>,
) -> WatchdogTimeoutCancelDisposition {
    let inflight = super::super::super::inflight::load_inflight_state(provider, channel_id.get());
    let result = super::super::super::mailbox_cancel_active_turn_if_current_with_reason(
        shared,
        channel_id,
        watchdog_token.clone(),
        WATCHDOG_TIMEOUT_CANCEL_SOURCE,
    )
    .await;
    super::super::super::clear_watchdog_deadline_override(channel_id.get()).await;

    let Some(token) = result.token else {
        return WatchdogTimeoutCancelDisposition::StaleToken;
    };
    if result.already_stopping {
        return WatchdogTimeoutCancelDisposition::AlreadyStopping;
    }

    super::super::super::ensure_cancel_token_bound_from_inflight(
        provider,
        channel_id,
        &token,
        "watchdog timeout mailbox cancel",
    );
    let termination_recorded = super::super::super::turn_bridge::stop_active_turn(
        provider,
        &token,
        super::super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession,
        WATCHDOG_TIMEOUT_REASON,
    )
    .await;
    let queue_depth = super::super::super::mailbox_snapshot(shared, channel_id)
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

#[cfg(unix)]
#[derive(Clone)]
struct PausedTurnWatcherAttachRequest {
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: String,
    output_path: String,
    initial_offset: u64,
    source: &'static str,
}

#[cfg(unix)]
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct PendingPausedWatcherAttachKey {
    channel_id: u64,
    tmux_session_name: String,
}

#[cfg(unix)]
impl PendingPausedWatcherAttachKey {
    fn new(channel_id: serenity::ChannelId, tmux_session_name: &str) -> Self {
        Self {
            channel_id: channel_id.get(),
            tmux_session_name: tmux_session_name.to_string(),
        }
    }
}

#[cfg(unix)]
static PENDING_PAUSED_WATCHER_ATTACHES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<PendingPausedWatcherAttachKey>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static TEST_PAUSED_WATCHER_TMUX_LIVE_OVERRIDE: std::sync::OnceLock<
    std::sync::Mutex<Option<std::collections::HashSet<String>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
static TEST_SUPPRESS_PAUSED_WATCHER_TASK_SPAWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn set_test_paused_watcher_tmux_live_override(names: Option<&[&str]>) {
    let lock = TEST_PAUSED_WATCHER_TMUX_LIVE_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = lock
        .lock()
        .expect("paused watcher tmux-live override lock poisoned");
    *guard = names.map(|slice| slice.iter().map(|name| (*name).to_string()).collect());
}

#[cfg(test)]
fn set_test_suppress_paused_watcher_task_spawn(suppress: bool) {
    TEST_SUPPRESS_PAUSED_WATCHER_TASK_SPAWN.store(suppress, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(unix)]
fn paused_watcher_tmux_session_has_live_pane(tmux_session_name: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(lock) = TEST_PAUSED_WATCHER_TMUX_LIVE_OVERRIDE.get()
            && let Ok(guard) = lock.lock()
            && let Some(names) = guard.as_ref()
        {
            return names.contains(tmux_session_name);
        }
    }

    crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name)
}

#[cfg(unix)]
fn active_watcher_owner_for_tmux(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> Option<serenity::ChannelId> {
    let owner_channel_id = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)?;
    let handle = shared.tmux_watchers.get(&owner_channel_id)?;
    (!handle.cancel.load(std::sync::atomic::Ordering::Relaxed)).then_some(owner_channel_id)
}

#[cfg(all(unix, test))]
fn pending_paused_watcher_attach_count_for_tests() -> usize {
    PENDING_PAUSED_WATCHER_ATTACHES
        .lock()
        .expect("pending paused watcher attach lock poisoned")
        .len()
}

#[cfg(all(unix, test))]
fn clear_pending_paused_watcher_attaches_for_tests() {
    PENDING_PAUSED_WATCHER_ATTACHES
        .lock()
        .expect("pending paused watcher attach lock poisoned")
        .clear();
}

#[cfg(unix)]
fn remove_pending_paused_watcher_attach(key: &PendingPausedWatcherAttachKey) {
    let mut guard = PENDING_PAUSED_WATCHER_ATTACHES
        .lock()
        .expect("pending paused watcher attach lock poisoned");
    guard.remove(key);
}

#[cfg(unix)]
fn schedule_pending_paused_turn_watcher_attach(request: PausedTurnWatcherAttachRequest) {
    let key = PendingPausedWatcherAttachKey::new(request.channel_id, &request.tmux_session_name);
    {
        let mut guard = PENDING_PAUSED_WATCHER_ATTACHES
            .lock()
            .expect("pending paused watcher attach lock poisoned");
        if !guard.insert(key.clone()) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::debug!(
                "  [{ts}] ↻ Pending paused tmux watcher attach already scheduled for channel {} — tmux {}",
                request.channel_id,
                request.tmux_session_name
            );
            return;
        }
    }

    if tokio::runtime::Handle::try_current().is_err() {
        remove_pending_paused_watcher_attach(&key);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ↻ Unable to schedule paused tmux watcher retry for channel {} — no Tokio runtime",
            request.channel_id
        );
        return;
    }

    super::super::super::task_supervisor::spawn_observed(
        "pending_paused_turn_watcher_attach",
        async move {
            for attempt in 1..=PAUSED_WATCHER_COLD_START_RETRY_ATTEMPTS {
                tokio::time::sleep(PAUSED_WATCHER_COLD_START_RETRY_DELAY).await;
                if let Some(owner) =
                    active_watcher_owner_for_tmux(&request.shared, &request.tmux_session_name)
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ↻ Skipping stale paused tmux watcher cold-start retry for channel {} via attempt {attempt}; tmux {} is already owned by {}",
                        request.channel_id,
                        request.tmux_session_name,
                        owner
                    );
                    remove_pending_paused_watcher_attach(&key);
                    return;
                }

                if paused_watcher_tmux_session_has_live_pane(&request.tmux_session_name) {
                    let owner = attach_paused_turn_watcher_inner(request.clone(), false);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ↻ Re-attached paused tmux watcher for channel {} via cold-start retry attempt {attempt}; owner={}",
                        request.channel_id,
                        owner
                    );
                    remove_pending_paused_watcher_attach(&key);
                    return;
                }
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ↻ Giving up paused tmux watcher retry for channel {} after {} attempts — tmux {} never became live",
                request.channel_id,
                PAUSED_WATCHER_COLD_START_RETRY_ATTEMPTS,
                request.tmux_session_name
            );
            remove_pending_paused_watcher_attach(&key);
        },
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn attach_paused_turn_watcher(
    shared: &Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<String>,
    output_path: Option<String>,
    initial_offset: u64,
    source: &'static str,
) -> serenity::ChannelId {
    #[cfg(unix)]
    if let (Some(tmux_session_name), Some(output_path)) = (tmux_session_name, output_path) {
        return attach_paused_turn_watcher_inner(
            PausedTurnWatcherAttachRequest {
                shared: shared.clone(),
                http,
                provider: provider.clone(),
                channel_id,
                tmux_session_name,
                output_path,
                initial_offset,
                source,
            },
            true,
        );
    }

    #[cfg(not(unix))]
    {
        let _ = (
            shared,
            http,
            provider,
            tmux_session_name,
            output_path,
            initial_offset,
            source,
        );
    }

    channel_id
}

#[allow(clippy::too_many_arguments)]
pub(super) fn attach_paused_turn_watcher_for_inflight(
    shared: &Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: Option<String>,
    output_path: Option<String>,
    initial_offset: u64,
    source: &'static str,
    inflight_state: &mut InflightTurnState,
) -> serenity::ChannelId {
    let owner_channel_id = attach_paused_turn_watcher(
        shared,
        http,
        provider,
        channel_id,
        tmux_session_name,
        output_path,
        initial_offset,
        source,
    );
    if inflight_state.set_watcher_owner_channel_id(owner_channel_id.get())
        && let Err(error) = save_inflight_state(inflight_state)
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}]   ⚠ inflight owner-channel save failed: {error}");
    }
    owner_channel_id
}

#[cfg(unix)]
fn attach_paused_turn_watcher_inner(
    request: PausedTurnWatcherAttachRequest,
    allow_cold_start_retry: bool,
) -> serenity::ChannelId {
    let PausedTurnWatcherAttachRequest {
        shared,
        http,
        provider,
        channel_id,
        tmux_session_name,
        output_path,
        initial_offset,
        source,
    } = request;
    let mut watcher_owner_channel_id = channel_id;

    {
        let existing_owner_for_tmux =
            active_watcher_owner_for_tmux(&shared, &tmux_session_name).is_some();
        let tmux_live = paused_watcher_tmux_session_has_live_pane(&tmux_session_name);
        if !tmux_live && !existing_owner_for_tmux {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Deferring paused tmux watcher attach for channel {} ({source}) — tmux {} is not live yet",
                channel_id,
                tmux_session_name
            );
            if allow_cold_start_retry {
                schedule_pending_paused_turn_watcher_attach(PausedTurnWatcherAttachRequest {
                    shared,
                    http,
                    provider,
                    channel_id,
                    tmux_session_name,
                    output_path,
                    initial_offset,
                    source,
                });
            }
            return watcher_owner_channel_id;
        }

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
            super::super::super::tmux_watcher_now_ms(),
        ));
        let handle = TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.clone(),
            output_path: output_path.clone(),
            paused: paused.clone(),
            resume_offset: resume_offset.clone(),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: turn_delivered.clone(),
            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
        };
        let claim = super::super::super::tmux::claim_or_reuse_watcher(
            &shared.tmux_watchers,
            channel_id,
            handle,
            &provider,
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
            #[cfg(test)]
            let suppress_spawn =
                TEST_SUPPRESS_PAUSED_WATCHER_TASK_SPAWN.load(std::sync::atomic::Ordering::Relaxed);
            #[cfg(not(test))]
            let suppress_spawn = false;
            if !suppress_spawn {
                if tokio::runtime::Handle::try_current().is_ok() {
                    super::super::super::task_supervisor::spawn_observed_tmux_watcher(
                        "router_tmux_output_watcher",
                        shared.clone(),
                        tmux_session_name.clone(),
                        cancel.clone(),
                        super::super::super::tmux::tmux_output_watcher(
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
                        ),
                    );
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ↻ Unable to spawn tmux watcher for channel {} — no Tokio runtime",
                        channel_id
                    );
                }
            }
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

#[cfg(all(test, unix))]
mod relay_state_contract_refs {
    //! #4268 — relay-state contract symbol anchor for the `pause_epoch` producer
    //! (compiler-checked existence). `attach_paused_turn_watcher_inner` is the
    //! sole production writer of `TmuxWatcherHandle::pause_epoch` (invariant I5),
    //! and it is a private fn nameable only from within `watchdog`, so its anchor
    //! lives here rather than in the central blocks.
    //!
    //! Gated `all(test, unix)` (not plain `test`): `attach_paused_turn_watcher_inner`
    //! is itself `#[cfg(unix)]`, so on windows test builds it is compiled out and
    //! a plain `#[cfg(test)]` anchor referencing it fails to compile (E0432,
    //! #4268 r3 / #4394). Matching its platform gate makes the anchor and the
    //! symbol appear/disappear together. `unix` is true on the required ubuntu
    //! `check_fast` compile, so that required job still compiles this block and
    //! proves the symbol exists. `#[cfg(all(test, unix))]` is one of the two
    //! byte-exact cfg spellings the checker whitelists (the other is
    //! `#[cfg(test)]`); a windows-only gate is rejected because no required job
    //! compiles it.
    //!
    //! See the header on `inflight::store::relay_state_contract_refs` for the
    //! contract, the CI wiring, and why there are no `// sym:` labels.
    #[test]
    fn contract_symbols_exist() {
        use super::attach_paused_turn_watcher_inner as _;
    }
}

#[cfg(test)]
mod timeout_notice_tests {
    use super::*;

    fn headless_inflight(
        current_msg_id: u64,
    ) -> crate::services::discord::inflight::InflightTurnState {
        crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            9_000_000_000_411_900,
            Some("headless-watchdog-test".to_string()),
            123,
            456,
            current_msg_id,
            "run the routine".to_string(),
            None,
            None,
            None,
            None,
            0,
        )
    }

    #[test]
    fn watchdog_timeout_notice_message_matches_foreground_copy() {
        assert_eq!(
            build_watchdog_timeout_notice_message(17, false),
            "⚠️ 턴이 17분 타임아웃으로 자동 중단되었습니다."
        );
        assert_eq!(
            build_watchdog_timeout_notice_message(17, true),
            "⚠️ 턴이 17분 타임아웃으로 자동 중단되었습니다. 대기 중인 메시지로 다음 턴을 시작합니다."
        );
    }

    #[test]
    fn headless_watchdog_timeout_notice_visibility_requires_visible_surface() {
        assert!(
            !headless_watchdog_timeout_notice_visible_from_surfaces(None, false),
            "no inflight and no footer slots means fully background/log-only"
        );

        let synthetic_id = crate::services::discord::SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR + 10;
        let invisible = headless_inflight(synthetic_id);
        assert!(
            !headless_watchdog_timeout_notice_visible_from_surfaces(Some(&invisible), false),
            "synthetic headless id alone is not a user-visible placeholder"
        );

        assert!(
            headless_watchdog_timeout_notice_visible_from_surfaces(Some(&invisible), true),
            "unfinished footer slots are a visible headless surface"
        );

        let mut long_running_placeholder = headless_inflight(synthetic_id);
        long_running_placeholder.long_running_placeholder_active = true;
        assert!(
            headless_watchdog_timeout_notice_visible_from_surfaces(
                Some(&long_running_placeholder),
                false
            ),
            "explicit long-running placeholder is a visible status surface"
        );

        let mut task_notification = headless_inflight(synthetic_id);
        task_notification.task_notification_kind =
            Some(crate::services::agent_protocol::TaskNotificationKind::Background);
        assert!(
            headless_watchdog_timeout_notice_visible_from_surfaces(Some(&task_notification), false),
            "task-notification status is the same explicit background surface used by #4100"
        );

        let real_placeholder = headless_inflight(123_456_789);
        assert!(
            headless_watchdog_timeout_notice_visible_from_surfaces(Some(&real_placeholder), false),
            "a real Discord placeholder id is visible"
        );

        let mut relay_only = headless_inflight(123_456_789);
        relay_only.relay_ownership_only = true;
        assert!(
            !headless_watchdog_timeout_notice_visible_from_surfaces(Some(&relay_only), false),
            "internal relay-ownership rows must not create user-facing timeout noise"
        );

        let mut rebind_origin = headless_inflight(123_456_789);
        rebind_origin.rebind_origin = true;
        assert!(
            !headless_watchdog_timeout_notice_visible_from_surfaces(Some(&rebind_origin), false),
            "rebind-origin rows must not create user-facing timeout noise even with a real placeholder id"
        );
    }
}

#[cfg(all(test, unix))]
mod cold_start_retry_tests {
    use super::*;
    use crate::services::discord::{tmux, tmux_watcher_now_ms};
    use std::sync::{LazyLock, Mutex, MutexGuard};
    use tokio::time::{Duration, sleep, timeout};

    static RETRY_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct RetryTestGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl RetryTestGuard {
        fn new() -> Self {
            let lock = RETRY_TEST_MUTEX
                .lock()
                .expect("paused watcher retry test lock poisoned");
            clear_pending_paused_watcher_attaches_for_tests();
            set_test_paused_watcher_tmux_live_override(Some(&[]));
            set_test_suppress_paused_watcher_task_spawn(true);
            Self { _lock: lock }
        }
    }

    impl Drop for RetryTestGuard {
        fn drop(&mut self) {
            set_test_paused_watcher_tmux_live_override(None);
            set_test_suppress_paused_watcher_task_spawn(false);
            clear_pending_paused_watcher_attaches_for_tests();
        }
    }

    fn test_watcher_handle(
        tmux_session_name: &str,
        output_path: &str,
        paused: bool,
    ) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(paused)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(
                std::sync::atomic::AtomicI64::new(tmux_watcher_now_ms()),
            ),
        }
    }

    #[tokio::test]
    async fn deferred_paused_watcher_attach_retries_when_tmux_goes_live() {
        let _guard = RetryTestGuard::new();
        let shared = super::super::super::super::make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(1485506232256168199);
        let tmux_name = format!(
            "AgentDesk-codex-cold-start-retry-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );

        let owner = attach_paused_turn_watcher(
            &shared,
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
            &ProviderKind::Codex,
            channel,
            Some(tmux_name.clone()),
            Some("/tmp/agentdesk-cold-start-retry-output.jsonl".to_string()),
            42,
            "unit-test-cold-start-restore",
        );

        assert_eq!(owner, channel);
        assert!(
            !shared.tmux_watchers.contains_key(&channel),
            "cold-start attach must not create a dead-pane watcher immediately"
        );
        assert_eq!(
            pending_paused_watcher_attach_count_for_tests(),
            1,
            "dead tmux attach should leave a bounded retry registered"
        );

        set_test_paused_watcher_tmux_live_override(Some(&[tmux_name.as_str()]));

        timeout(Duration::from_secs(1), async {
            loop {
                if shared.tmux_watchers.contains_key(&channel)
                    && pending_paused_watcher_attach_count_for_tests() == 0
                {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("retry should attach once tmux becomes live");

        let watcher = shared
            .tmux_watchers
            .get(&channel)
            .expect("retry should install a watcher slot");
        assert_eq!(watcher.tmux_session_name, tmux_name);
        assert_eq!(
            watcher.output_path,
            "/tmp/agentdesk-cold-start-retry-output.jsonl"
        );
        assert!(
            watcher.paused.load(std::sync::atomic::Ordering::Relaxed),
            "reattached restored-turn watcher must stay paused until turn bridge hands off"
        );
    }

    #[tokio::test]
    async fn cold_start_retry_does_not_repause_existing_live_handoff_watcher() {
        let _guard = RetryTestGuard::new();
        let shared = super::super::super::super::make_shared_data_for_tests();
        let channel = serenity::ChannelId::new(1485506232256168201);
        let tmux_name = format!(
            "AgentDesk-claude-cold-start-active-owner-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let output_path = "/tmp/agentdesk-cold-start-active-owner-output.jsonl";

        let owner = attach_paused_turn_watcher(
            &shared,
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
            &ProviderKind::Claude,
            channel,
            Some(tmux_name.clone()),
            Some(output_path.to_string()),
            0,
            "turn_start_headless",
        );

        assert_eq!(owner, channel);
        assert!(
            !shared.tmux_watchers.contains_key(&channel),
            "cold-start attach must not create a dead-pane watcher immediately"
        );
        assert_eq!(
            pending_paused_watcher_attach_count_for_tests(),
            1,
            "dead tmux attach should leave a bounded retry registered"
        );

        let active = test_watcher_handle(&tmux_name, output_path, false);
        let paused_flag = active.paused.clone();
        let claim = tmux::claim_or_reuse_watcher(
            &shared.tmux_watchers,
            channel,
            active,
            &ProviderKind::Claude,
            "unit-test-tmux-ready-handoff",
        );
        assert_eq!(claim.owner_channel_id(), channel);
        assert!(!paused_flag.load(std::sync::atomic::Ordering::Relaxed));

        timeout(Duration::from_secs(1), async {
            loop {
                if pending_paused_watcher_attach_count_for_tests() == 0 {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("retry should retire itself when a handoff watcher already owns the tmux");

        assert!(
            !paused_flag.load(std::sync::atomic::Ordering::Relaxed),
            "stale cold-start retry must not pause a watcher already unpaused by turn bridge handoff"
        );
    }
}
