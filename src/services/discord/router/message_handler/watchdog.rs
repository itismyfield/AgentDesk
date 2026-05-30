use super::*;

pub(super) const WATCHDOG_DEADLOCK_PREALERT_MS: i64 = 5 * 60 * 1000;
pub(super) const WATCHDOG_DEADLOCK_PREALERT_BOT: &str = "announce";
pub(super) const WATCHDOG_TIMEOUT_REASON: &str = "watchdog timeout";
pub(super) const WATCHDOG_TIMEOUT_CANCEL_SOURCE: &str = "watchdog_timeout";
pub(super) fn watchdog_deadlock_prealert_bot_name() -> &'static str {
    WATCHDOG_DEADLOCK_PREALERT_BOT
}

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
            super::super::super::tmux_watcher_now_ms(),
        ));
        let mailbox_finalize_owed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.clone(),
            output_path: output_path.clone(),
            paused: paused.clone(),
            resume_offset: resume_offset.clone(),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: turn_delivered.clone(),
            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
            mailbox_finalize_owed: mailbox_finalize_owed.clone(),
        };
        let claim = super::super::super::tmux::claim_or_reuse_watcher(
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
                    mailbox_finalize_owed,
                ),
            );
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
