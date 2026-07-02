use super::*;

/// #1446 Layer 2 — load the thread's persisted inflight state and report
/// whether its `updated_at` is older than `INFLIGHT_STALENESS_THRESHOLD_SECS`.
/// Returns `false` when no state file exists (nothing to clean) or when
/// `updated_at` cannot be parsed (never infer staleness from missing data).
///
/// **Pure-classification helper only.** A stale `updated_at` is necessary
/// but not sufficient to force-clean a live thread — `updated_at` only
/// advances when `save_inflight_state` runs, so a healthy long Bash /
/// large Read / slow LLM stream can legitimately go silent for minutes.
/// `thread_guard_should_force_clean_stale_thread` adds the required
/// secondary signal (watcher snapshot's `desynced == true`).
#[allow(dead_code)] // #3034: #1446 Layer-2 classifier pinned by the intake-gate unit tests.
pub(super) fn thread_guard_inflight_is_stale(
    provider: &ProviderKind,
    thread_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> bool {
    crate::services::discord::inflight::load_inflight_state(provider, thread_id.get())
        .map(|state| {
            crate::services::discord::inflight::inflight_state_is_stale(
                &state,
                now_unix_secs,
                crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
            )
        })
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StaleActiveTurnProofClassification {
    LiveOrUnclear,
    RelayStalled,
    QueueBlockedOrphan,
    ExplicitBackgroundStatus,
}

fn classify_stale_active_turn_proof(
    inflight: &crate::services::discord::inflight::InflightTurnState,
    snapshot: &crate::services::discord::health::WatcherStateSnapshot,
    now_unix_secs: i64,
) -> StaleActiveTurnProofClassification {
    if !crate::services::discord::inflight::inflight_state_is_stale(
        inflight,
        now_unix_secs,
        crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
    ) {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    }

    if inflight.long_running_placeholder_active {
        return StaleActiveTurnProofClassification::ExplicitBackgroundStatus;
    }

    if snapshot.desynced {
        return StaleActiveTurnProofClassification::RelayStalled;
    }

    if !snapshot.inflight_state_present {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    }

    if snapshot.mailbox_active_user_msg_id.is_some()
        && snapshot.mailbox_active_user_msg_id != Some(inflight.user_msg_id)
    {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    }

    if !snapshot.attached && snapshot.tmux_session_alive != Some(true) {
        return StaleActiveTurnProofClassification::QueueBlockedOrphan;
    }

    StaleActiveTurnProofClassification::LiveOrUnclear
}

async fn classify_channel_stale_active_turn_proof(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> StaleActiveTurnProofClassification {
    let Some(inflight) =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    };
    let Some(registry) = shared.health_registry.upgrade() else {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    };
    let Some(snapshot) = registry
        .snapshot_watcher_state_for_provider(provider, channel_id.get())
        .await
    else {
        return StaleActiveTurnProofClassification::LiveOrUnclear;
    };
    classify_stale_active_turn_proof(&inflight, &snapshot, now_unix_secs)
}

/// #1446 / #1456 — full force-clean predicate. Requires stale persisted
/// inflight state plus either:
///   1. the watcher-state snapshot for the thread reports `desynced == true`
///      (capture-lag, cross-owner mismatch, or live-tmux orphan with no
///      relay heartbeat — the same conjunction the stall-watchdog uses), or
///   2. the mailbox active-turn proof has no live owner (`attached == false`
///      and no live tmux session), which is the queue-blocked fail-open path.
///
/// Without the snapshot's desync corroboration we would force-clean a
/// healthy long-running turn whose `updated_at` simply has not advanced
/// because no chunk hit the bridge in the last 5 minutes. The no-owner path
/// is intentionally narrower: live tmux sessions and explicit background
/// placeholder status are preserved. Returning `false` when the registry is
/// unreachable is the conservative default — a missing registry happens
/// during startup before the stall-watchdog would also be running, so
/// deferring cleanup costs nothing.
pub(super) async fn thread_guard_should_force_clean_stale_thread(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    thread_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> bool {
    matches!(
        classify_channel_stale_active_turn_proof(shared, provider, thread_id, now_unix_secs).await,
        StaleActiveTurnProofClassification::RelayStalled
            | StaleActiveTurnProofClassification::QueueBlockedOrphan
    )
}

/// #1446 Layer 2 — perform the THREAD-GUARD's stale-thread cleanup:
///   1. drop the parent → thread mapping so subsequent intakes do not re-
///      trigger the guard,
///   2. delete the thread's inflight state file (releases the durable lock
///      whose presence convinced `mailbox_has_active_turn` the dispatch is
///      still live),
///   3. **clear** the thread's mailbox (cancel token + active turn anchor +
///      pending interventions). `cancel_active_turn` alone is insufficient
///      here — for a dead-dispatch case there is no live turn task to
///      observe the cancel signal and call `finish_turn`, so
///      `has_active_turn()` would stay `true` forever and the next bot
///      message would re-enter the THREAD-GUARD's queueing branch.
///      `mailbox_clear_channel` synchronously drops `active_request_owner`
///      / `active_user_message_id` and reports `has_active_turn() == false`
///      immediately on completion (see `ChannelMailboxMsg::Clear` handler
///      in `turn_orchestrator.rs`).
///   4. complete the bookkeeping that the missing `finish_turn` would
///      otherwise have done: cancel the orphaned token (kill any leftover
///      child / tmux session) and decrement `global_active`. Mirrors the
///      `placeholder_sweeper::finalize_abandoned_mailbox` cleanup
///      pattern so health and deferred-restart counters do not leak.
///
/// We never touch the parent channel's own mailbox — only the thread's.
/// This preserves the `watcher_owns_live_relay` invariant by leaving
/// parent-side relay state untouched.
pub(super) async fn thread_guard_force_clean_stale_thread(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    parent_channel_id: serenity::ChannelId,
    thread_id: serenity::ChannelId,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🔓 THREAD-GUARD: stale inflight detected for thread {}, cleaning up and proceeding",
        thread_id
    );
    shared.dispatch.thread_parents.remove(&parent_channel_id);
    crate::services::discord::inflight::delete_inflight_state_file(provider, thread_id.get());
    let cleared = mailbox_clear_channel(shared, provider, thread_id).await;
    crate::services::discord::stall_recovery::finalize_orphaned_clear(
        shared,
        thread_id,
        cleared.removed_token,
        "1446_thread_guard_stale_inflight",
    );
}

/// #2044 F7 (P3 — documentation): invariant note.
///
/// This recovery path delegates the `cancelled` flag + `global_active`
/// decrement to `stall_recovery::finalize_orphaned_clear`, which has
/// owned both side-effects since #1446 (see `stall_recovery.rs:65-89`):
///   1. it calls `turn_bridge::cancel_active_token` on the removed
///      token — that helper sets `token.cancelled = true` so any
///      watchdog/voice-barge-in holding an Arc to the same token sees
///      the cancellation;
///   2. it calls `saturating_decrement_global_active`, mirroring what
///      the normal `turn_bridge::mod.rs:3132-3141` and
///      `tmux.rs:2052-2061` cleanup sites do inline.
///
/// Therefore this site MUST NOT also poke `cancelled` / `global_active`
/// — doing so would double-decrement the counter (already saturating
/// in `finalize_orphaned_clear`, but the duplicate is still a smell)
/// and confuse audit logs. If a future change splits
/// `finalize_orphaned_clear` or makes either side-effect conditional,
/// this comment and the comments in the bridge/tmux peer sites must
/// move in lockstep.
async fn release_queue_blocked_stale_active_turn(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> bool {
    let classification =
        classify_channel_stale_active_turn_proof(shared, provider, channel_id, now_unix_secs).await;
    if classification != StaleActiveTurnProofClassification::QueueBlockedOrphan {
        return false;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] 🔓 QUEUE-GUARD: stale active-turn proof for channel {} has no live owner; releasing mailbox and proceeding",
        channel_id
    );
    crate::services::discord::inflight::delete_inflight_state_file(provider, channel_id.get());
    crate::services::discord::clear_watchdog_deadline_override(channel_id.get()).await;
    let finish = mailbox_finish_turn(shared, provider, channel_id).await;
    // #2044 F7: `finalize_orphaned_clear` owns both `cancelled.store(true)`
    // and the saturating `global_active` decrement — do not duplicate them here.
    crate::services::discord::stall_recovery::finalize_orphaned_clear(
        shared,
        channel_id,
        finish.removed_token,
        "1456_queue_blocked_stale_proof",
    );
    shared
        .dispatch
        .thread_parents
        .retain(|_, thread_id| *thread_id != channel_id);
    if !finish.has_pending {
        shared.dispatch.role_overrides.remove(&channel_id);
    }
    true
}

pub(super) async fn mailbox_has_live_active_turn_or_cleanup_stale_proof(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> bool {
    if !mailbox_has_active_turn(shared, channel_id).await {
        return false;
    }
    if release_queue_blocked_stale_active_turn(
        shared,
        provider,
        channel_id,
        chrono::Utc::now().timestamp(),
    )
    .await
    {
        return mailbox_has_active_turn(shared, channel_id).await;
    }
    true
}

/// #1446 Layer 2 — `thread_guard_inflight_is_stale` reads inflight files
/// via the runtime root override, so we keep the always-on slice that
/// only exercises the read+staleness classification (no `SharedData`
/// construction). The `thread_guard_force_clean_stale_thread` integration
/// test that drives mailbox cancel / dispatch_thread_parents removal is
/// still not in the default suite because it depends on `TestHealthHarness`.
#[cfg(test)]
mod thread_guard_stale_pure_tests {
    use super::*;
    use chrono::TimeZone;
    use poise::serenity_prelude::ChannelId;

    /// Anchor `now` and produce a stale `updated_at` literal using the
    /// production `now_string` encoding.
    fn local_at_offset(now_unix: i64, offset_secs: i64) -> String {
        chrono::Local
            .timestamp_opt(now_unix + offset_secs, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    fn seed_inflight_with_updated_at(provider: &ProviderKind, channel_id: u64, updated_at: &str) {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("test-thread-guard".to_string()),
            42,
            8_001,
            8_002,
            "test-input".to_string(),
            Some("test-session".to_string()),
            Some("test-tmux".to_string()),
            None,
            None,
            0,
        );
        state.updated_at = updated_at.to_string();
        state.started_at = updated_at.to_string();
        let root = crate::services::discord::inflight::inflight_runtime_root()
            .expect("inflight runtime root must be available under test override");
        let provider_dir = root.join(provider.as_str());
        std::fs::create_dir_all(&provider_dir).expect("create provider dir");
        let path = provider_dir.join(format!("{channel_id}.json"));
        let json = serde_json::to_string_pretty(&state).expect("serialize seeded inflight");
        std::fs::write(&path, json).expect("write seeded inflight");
    }

    fn inflight_with_updated_at(
        provider: &ProviderKind,
        channel_id: u64,
        updated_at: &str,
    ) -> crate::services::discord::inflight::InflightTurnState {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("test-thread-guard".to_string()),
            42,
            8_001,
            8_002,
            "test-input".to_string(),
            Some("test-session".to_string()),
            Some("stale-proof-tmux".to_string()),
            None,
            None,
            0,
        );
        state.updated_at = updated_at.to_string();
        state.started_at = updated_at.to_string();
        state
    }

    fn watcher_snapshot(
        provider: &ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
        attached: bool,
        tmux_session_alive: Option<bool>,
        desynced: bool,
    ) -> crate::services::discord::health::WatcherStateSnapshot {
        let relay_health = crate::services::discord::relay_health::RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id,
            active_turn: crate::services::discord::relay_health::RelayActiveTurn::Foreground,
            tmux_session: Some("stale-proof-tmux".to_string()),
            tmux_alive: tmux_session_alive,
            watcher_attached: attached,
            watcher_attached_stale: false,
            watcher_owner_channel_id: attached.then_some(channel_id),
            watcher_owns_live_relay: false,
            bridge_inflight_present: true,
            bridge_current_msg_id: Some(8_002),
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(user_msg_id),
            queue_depth: 0,
            pending_discord_callback_msg_id: Some(8_002),
            pending_thread_proof: false,
            parent_channel_id: None,
            thread_channel_id: None,
            last_relay_ts_ms: None,
            last_outbound_activity_ms: None,
            last_capture_offset: None,
            last_relay_offset: 0,
            unread_bytes: None,
            desynced,
            stale_thread_proof: false,
        };
        let relay_stall_state =
            crate::services::discord::relay_health::RelayStallClassifier::classify(&relay_health);
        crate::services::discord::health::WatcherStateSnapshot {
            provider: provider.as_str().to_string(),
            attached,
            tmux_session: Some("stale-proof-tmux".to_string()),
            watcher_owner_channel_id: attached.then_some(channel_id),
            last_relay_offset: 0,
            inflight_state_present: true,
            last_relay_ts_ms: 0,
            last_capture_offset: None,
            unread_bytes: None,
            desynced,
            reconnect_count: 0,
            inflight_started_at: None,
            inflight_updated_at: None,
            inflight_user_msg_id: Some(user_msg_id),
            inflight_current_msg_id: Some(8_002),
            tmux_session_alive,
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(user_msg_id),
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            relay_stall_state,
            relay_health,
        }
    }

    /// Scoped env-var override for `AGENTDESK_ROOT_DIR`. Restores the
    /// previous value (or removes the var) on drop. Used so the always-on
    /// test does not leak state into adjacent test runs that may also rely
    /// on the runtime root.
    ///
    /// #2444 follow-up: acquires `shared_test_env_lock()` so this writer
    /// serializes with every other AGENTDESK_ROOT_DIR mutator in the test
    /// binary (claude_tui::hook_relay, credential, integration tests etc),
    /// closing the cross-module env race that survived the wave-D fix.
    struct EnvRootGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl EnvRootGuard {
        fn set(path: &std::path::Path) -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self {
                previous,
                _lock: lock,
            }
        }
    }
    impl Drop for EnvRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    /// `thread_guard_inflight_is_stale` must:
    ///   1. report `true` for a stale on-disk inflight,
    ///   2. report `false` for a fresh on-disk inflight,
    ///   3. report `false` when the inflight file does not exist (nothing
    ///      to clean — never cleanup a thread we know nothing about).
    #[tokio::test]
    async fn thread_guard_inflight_is_stale_classifies_disk_state() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());

        let provider = ProviderKind::Codex;
        let now_unix = chrono::Utc::now().timestamp();

        // Missing inflight → not stale.
        assert!(
            !super::thread_guard_inflight_is_stale(
                &provider,
                ChannelId::new(900_000_000_000_900),
                now_unix
            ),
            "missing inflight must NOT be classified as stale"
        );

        // Stale inflight → stale.
        let stale_channel = 900_000_000_000_901u64;
        let stale_at = local_at_offset(
            now_unix,
            -(crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 5,
        );
        seed_inflight_with_updated_at(&provider, stale_channel, &stale_at);
        assert!(
            super::thread_guard_inflight_is_stale(
                &provider,
                ChannelId::new(stale_channel),
                now_unix
            ),
            "stale inflight (updated_at={stale_at}) must be classified as stale"
        );

        // Fresh inflight → not stale.
        let fresh_channel = 900_000_000_000_902u64;
        let fresh_at = local_at_offset(now_unix, -5);
        seed_inflight_with_updated_at(&provider, fresh_channel, &fresh_at);
        assert!(
            !super::thread_guard_inflight_is_stale(
                &provider,
                ChannelId::new(fresh_channel),
                now_unix
            ),
            "fresh inflight (updated_at={fresh_at}) must NOT be classified as stale"
        );
    }

    /// #1456: a stale active-turn proof with no attached watcher and no live
    /// tmux owner must be classified as queue-blocked orphan state. The intake
    /// gate uses this to release the mailbox before the new user message takes
    /// the normal streaming path instead of being queued forever.
    #[test]
    fn stale_active_turn_proof_classifies_no_owner_as_queue_blocked_orphan() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());

        let provider = ProviderKind::Codex;
        let channel_id = 900_000_000_000_910u64;
        let now_unix = chrono::Utc::now().timestamp();
        let stale_at = local_at_offset(
            now_unix,
            -(crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 5,
        );
        let inflight = inflight_with_updated_at(&provider, channel_id, &stale_at);
        let snapshot = watcher_snapshot(
            &provider,
            channel_id,
            inflight.user_msg_id,
            false,
            Some(false),
            false,
        );

        assert_eq!(
            super::classify_stale_active_turn_proof(&inflight, &snapshot, now_unix),
            super::StaleActiveTurnProofClassification::QueueBlockedOrphan
        );
    }

    /// #1456: explicit background placeholders are a visible status surface,
    /// not disposable stale proof. Even if their inflight timestamp is old,
    /// the fail-open classifier must preserve them instead of taking the
    /// cleanup path that would cancel the owning session.
    #[test]
    fn stale_active_turn_proof_preserves_explicit_background_status() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());

        let provider = ProviderKind::Codex;
        let channel_id = 900_000_000_000_911u64;
        let now_unix = chrono::Utc::now().timestamp();
        let stale_at = local_at_offset(
            now_unix,
            -(crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 5,
        );
        let mut inflight = inflight_with_updated_at(&provider, channel_id, &stale_at);
        inflight.long_running_placeholder_active = true;
        let snapshot = watcher_snapshot(
            &provider,
            channel_id,
            inflight.user_msg_id,
            false,
            Some(false),
            false,
        );

        assert_eq!(
            super::classify_stale_active_turn_proof(&inflight, &snapshot, now_unix),
            super::StaleActiveTurnProofClassification::ExplicitBackgroundStatus
        );
    }
}
