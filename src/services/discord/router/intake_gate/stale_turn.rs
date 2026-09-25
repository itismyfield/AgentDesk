use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StaleActiveTurnProofClassification {
    LiveOrUnclear,
    RelayStalled,
    QueueBlockedOrphan,
    ExplicitBackgroundStatus,
}

struct StaleActiveTurnProof {
    classification: StaleActiveTurnProofClassification,
    snapshot: crate::services::discord::health::WatcherStateSnapshot,
    /// Taken before the snapshot so a successor started after it fails the finish guard.
    observed_before: std::time::Instant,
}

/// #6199 — the snapshot episode the THREAD-GUARD force-clean may finish.
pub(super) struct ThreadGuardForceCleanProof {
    user_msg_id: Option<u64>,
    turn_nonce: Option<String>,
    observed_before: std::time::Instant,
}

/// Non-unix builds have no tmux reachability evidence source: every warrant
/// operand is absent, so the warrant abstains and the structural
/// classification alone decides (the pre-warrant behavior of both stale-turn
/// release paths).
#[cfg(not(unix))]
fn stale_turn_axis_b_warrants(provider: &ProviderKind, proof: &StaleActiveTurnProof) -> bool {
    let _ = provider;
    matches!(
        proof.classification,
        StaleActiveTurnProofClassification::RelayStalled
            | StaleActiveTurnProofClassification::QueueBlockedOrphan
    )
}

#[cfg(unix)]
fn stale_turn_axis_b_warrants(provider: &ProviderKind, proof: &StaleActiveTurnProof) -> bool {
    let structural_candidate_apply =
        crate::services::discord::relay_recovery::structural_candidate_apply(matches!(
            proof.classification,
            StaleActiveTurnProofClassification::RelayStalled
                | StaleActiveTurnProofClassification::QueueBlockedOrphan
        ));
    let action =
        crate::services::discord::relay_recovery::RelayRecoveryActionKind::ClearStaleThreadProof;
    let destructive_warrant_bind =
        crate::services::discord::relay_recovery::destructive_warrant_bind(
            structural_candidate_apply,
            action,
            provider,
            Some(&proof.snapshot),
            false,
        );
    destructive_warrant_bind.eligible
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

fn bind_stale_active_turn_proof(
    inflight: &crate::services::discord::inflight::InflightTurnState,
    snapshot: crate::services::discord::health::WatcherStateSnapshot,
    now_unix_secs: i64,
    observed_before: std::time::Instant,
) -> StaleActiveTurnProof {
    StaleActiveTurnProof {
        classification: classify_stale_active_turn_proof(inflight, &snapshot, now_unix_secs),
        snapshot,
        observed_before,
    }
}

async fn classify_channel_stale_active_turn_proof(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> Option<StaleActiveTurnProof> {
    let inflight =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())?;
    let registry = shared.health_registry.upgrade()?;
    let observed_before = std::time::Instant::now();
    let snapshot = registry
        .snapshot_watcher_state_for_provider(provider, channel_id.get())
        .await?;
    Some(bind_stale_active_turn_proof(
        &inflight,
        snapshot,
        now_unix_secs,
        observed_before,
    ))
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
) -> Option<ThreadGuardForceCleanProof> {
    let proof =
        classify_channel_stale_active_turn_proof(shared, provider, thread_id, now_unix_secs)
            .await?;
    if !stale_turn_axis_b_warrants(provider, &proof) {
        return None;
    }
    Some(ThreadGuardForceCleanProof {
        user_msg_id: proof.snapshot.mailbox_active_user_msg_id,
        turn_nonce: proof.snapshot.mailbox_active_turn_nonce,
        observed_before: proof.observed_before,
    })
}

/// #1446 Layer 2 — perform the THREAD-GUARD's stale-thread cleanup:
///   1. drop the parent → thread mapping so subsequent intakes do not re-
///      trigger the guard,
///   2. delete the thread's inflight state file (releases the durable lock
///      whose presence convinced `mailbox_has_active_turn` the dispatch is
///      still live),
///   3. **finish** the thread's snapshot episode (cancel token + active turn
///      anchor). `cancel_active_turn` alone is insufficient here — for a
///      dead-dispatch case there is no live turn task to observe the cancel
///      signal and call `finish_turn`, so `has_active_turn()` would stay
///      `true` forever and the next bot message would re-enter the
///      THREAD-GUARD's queueing branch. #6199: the finish releases the anchor
///      synchronously but keeps the queued follow-ups for the completion
///      listener; a Clear would supersede them.
///   4. complete the bookkeeping that the missing `finish_turn` would
///      otherwise have done: cancel the orphaned token (kill any leftover
///      child / tmux session) and decrement `global_active`. Mirrors the
///      `placeholder_sweeper::finalize_abandoned_mailbox` cleanup
///      pattern so health and deferred-restart counters do not leak.
///
/// We never touch the parent channel's own mailbox — only the thread's.
/// This preserves the `watcher_owns_live_relay` invariant by leaving
/// parent-side relay state untouched.
///
/// Returns `false` (and changes nothing) without a proof or when a successor
/// episode now owns the thread, so the caller keeps queueing behind it.
pub(super) async fn thread_guard_force_clean_stale_thread(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    _parent_channel_id: serenity::ChannelId,
    thread_id: serenity::ChannelId,
    proof: Option<ThreadGuardForceCleanProof>,
) -> bool {
    let Some(proof) = proof else {
        return false;
    };
    // #4198: snapshot before the finish await so a follow-up's fresh override survives.
    let owned_role_override =
        crate::services::discord::turn_finalizer::cleanup::snapshot_role_override(
            shared, thread_id,
        );
    // The guarded finish goes first: a miss means a successor owns the thread.
    let (finish, guarded) = match proof.user_msg_id {
        Some(user_msg_id) if user_msg_id != 0 => (
            crate::services::discord::mailbox_finish_turn_if_matches_episode_started_before(
                shared,
                provider,
                thread_id,
                serenity::MessageId::new(user_msg_id),
                proof.turn_nonce,
                proof.observed_before,
            )
            .await,
            true,
        ),
        // No anchor id to compare: plain finish, never a queue-draining Clear.
        _ => (
            mailbox_finish_turn(shared, provider, thread_id).await,
            false,
        ),
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    if guarded && finish.removed_token.is_none() {
        tracing::info!(
            "  [{ts}] 🔀 THREAD-GUARD: thread {} episode changed since the stale proof; keeping it",
            thread_id
        );
        return false;
    }
    tracing::info!(
        "  [{ts}] 🔓 THREAD-GUARD: stale inflight detected for thread {}, cleaning up and proceeding",
        thread_id
    );
    crate::services::discord::inflight::delete_inflight_state_file(provider, thread_id.get());
    // #2044 F7: `finalize_orphaned_clear` owns `cancelled` and the `global_active` decrement.
    crate::services::discord::stall_recovery::finalize_orphaned_clear(
        shared,
        thread_id,
        finish.removed_token,
        "1446_thread_guard_stale_inflight",
    );
    let thread_parent_kickoffs =
        crate::services::discord::turn_finalizer::cleanup::collect_and_clear_thread_parents(
            shared, thread_id,
        );
    crate::services::discord::turn_finalizer::cleanup::kickoff_thread_parents_after_finalize(
        shared,
        provider,
        thread_parent_kickoffs,
    );
    if !finish.has_pending {
        crate::services::discord::turn_finalizer::cleanup::remove_owned_role_override(
            shared,
            thread_id,
            owned_role_override,
        );
    }
    true
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
    let Some(proof) =
        classify_channel_stale_active_turn_proof(shared, provider, channel_id, now_unix_secs).await
    else {
        return false;
    };
    if proof.classification != StaleActiveTurnProofClassification::QueueBlockedOrphan {
        return false;
    }

    if !stale_turn_axis_b_warrants(provider, &proof) {
        return false;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] 🔓 QUEUE-GUARD: stale active-turn proof for channel {} has no live owner; releasing mailbox and proceeding",
        channel_id
    );
    // #4198: snapshot before the yielding watchdog/finish awaits so the remove
    // below cannot clobber a same-channel follow-up's freshly inserted override.
    let owned_role_override =
        crate::services::discord::turn_finalizer::cleanup::snapshot_role_override(
            shared, channel_id,
        );
    crate::services::discord::inflight::delete_inflight_state_file(provider, channel_id.get());
    let finish = mailbox_finish_turn(shared, provider, channel_id).await;
    // #2044 F7: `finalize_orphaned_clear` owns both `cancelled.store(true)`
    // and the saturating `global_active` decrement — do not duplicate them here.
    crate::services::discord::stall_recovery::finalize_orphaned_clear(
        shared,
        channel_id,
        finish.removed_token,
        "1456_queue_blocked_stale_proof",
    );
    let thread_parent_kickoffs =
        crate::services::discord::turn_finalizer::cleanup::collect_and_clear_thread_parents(
            shared, channel_id,
        );
    crate::services::discord::turn_finalizer::cleanup::kickoff_thread_parents_after_finalize(
        shared,
        provider,
        thread_parent_kickoffs,
    );
    if !finish.has_pending {
        crate::services::discord::turn_finalizer::cleanup::remove_owned_role_override(
            shared,
            channel_id,
            owned_role_override,
        );
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

/// #1446 Layer 2 — these cases read inflight files via the runtime root
/// override and build `SharedData` fixtures without a health harness.
#[cfg(test)]
mod thread_guard_stale_pure_tests {
    use super::*;
    use chrono::TimeZone;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::{Arc, atomic::Ordering};

    use crate::services::provider::CancelToken;

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
        seed_inflight_with_nonce(provider, channel_id, updated_at, None);
    }

    fn seed_inflight_with_nonce(
        provider: &ProviderKind,
        channel_id: u64,
        updated_at: &str,
        turn_nonce: Option<String>,
    ) {
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
        state.turn_nonce = turn_nonce;
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
            mailbox_turn_started_at_ms: None,
            mailbox_turn_age_secs: None,
            queue_depth: 0,
            pending_discord_callback_msg_id: Some(8_002),
            pending_thread_proof: false,
            parent_channel_id: None,
            thread_channel_id: None,
            last_relay_ts_ms: None,
            last_relay_age_secs: None,
            last_outbound_activity_ms: None,
            last_capture_offset: None,
            last_relay_offset: 0,
            unread_bytes: None,
            desynced,
            stale_thread_proof: false,
            unpaired_active_token_reconfirmed: false,
        };
        let relay_stall_state =
            crate::services::discord::relay_health::RelayStallClassifier::classify(&relay_health);
        crate::services::discord::health::WatcherStateSnapshot {
            provider: provider.as_str().to_string(),
            attached,
            tmux_session: Some("stale-proof-tmux".to_string()),
            watcher_owner_channel_id: attached.then_some(channel_id),
            last_relay_offset: 0,
            durable_frontier:
                crate::services::discord::relay_health::DurableFrontierObservation::RowAbsent,
            inflight_state_present: true,
            last_relay_ts_ms: 0,
            last_capture_offset: None,
            capture_coordinate:
                crate::services::discord::health::liveness_authority::CaptureCoordinateObservation {
                    offset: None,
                    path_hash: 0,
                    file_id: None,
                    status: crate::services::discord::health::liveness_authority::CoordinateStatus::Missing,
                },
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
            mailbox_active_turn_nonce: None,
            bound_output_path: None,
            bound_session_id: None,
            transcript_binding_stall: "none",
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: Some("/tmp/stale-proof-tmux.jsonl".to_string()),
            #[cfg(unix)]
            reachability_observation: None,
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
        let mut snapshot = watcher_snapshot(
            &provider,
            channel_id,
            inflight.user_msg_id,
            false,
            Some(false),
            false,
        );
        #[cfg(unix)]
        let observation = (
            crate::services::discord::health::reachability::verdict::ReachabilityVerdict::Reachable,
            54_641,
        );
        #[cfg(unix)]
        {
            snapshot.reachability_observation = Some(observation.clone());
        }
        let proof = super::bind_stale_active_turn_proof(
            &inflight,
            snapshot,
            now_unix,
            std::time::Instant::now(),
        );

        assert_eq!(
            proof.classification,
            super::StaleActiveTurnProofClassification::QueueBlockedOrphan
        );
        #[cfg(unix)]
        assert_eq!(
            proof.snapshot.reachability_observation,
            Some(observation),
            "the proof must retain the exact snapshot that authorized cleanup"
        );
    }

    #[tokio::test]
    async fn queue_blocked_stale_active_turn_release_publishes_completion_event() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());

        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(900_000_000_000_912);
        let user_msg_id = MessageId::new(8_001);
        let now_unix = chrono::Utc::now().timestamp();
        let stale_at = local_at_offset(
            now_unix,
            -(crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 5,
        );
        let cancel_token = Arc::new(CancelToken::new());
        seed_inflight_with_nonce(
            &provider,
            channel_id.get(),
            &stale_at,
            cancel_token.turn_nonce().map(str::to_string),
        );

        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .expect("fresh shared data should be uniquely owned before registry install")
            .health_registry = Arc::downgrade(&registry);
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel_id,
                cancel_token,
                UserId::new(7),
                user_msg_id,
            )
            .await,
            "seed stale active mailbox owner"
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let mut rx =
            crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
                shared.as_ref(),
            );
        assert!(
            super::release_queue_blocked_stale_active_turn(
                &shared, &provider, channel_id, now_unix,
            )
            .await,
            "queue-blocked stale active proof should release the mailbox"
        );

        let event = rx
            .try_recv()
            .expect("stale active-turn release must publish a completion event");
        assert_eq!(event.channel_id, channel_id);
        assert_eq!(
            shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
            0,
            "release primitive publishes only; the listener owns immediate drain/backstop policy"
        );
    }

    fn queued_thread_followup(message_id: u64) -> crate::services::turn_orchestrator::Intervention {
        crate::services::turn_orchestrator::Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::load_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: format!("thread follow-up {message_id}"),
            mode: crate::services::turn_orchestrator::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    /// Stale dead-dispatch thread with an active anchor and three queued follow-ups.
    async fn seed_stale_thread_with_queue(
        provider: &ProviderKind,
        thread_id: ChannelId,
        user_msg_id: MessageId,
    ) -> (
        Arc<crate::services::discord::health::HealthRegistry>,
        Arc<SharedData>,
        Arc<CancelToken>,
    ) {
        let now_unix = chrono::Utc::now().timestamp();
        let stale_at = local_at_offset(
            now_unix,
            -(crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 5,
        );
        let cancel_token = Arc::new(CancelToken::new());
        seed_inflight_with_nonce(
            provider,
            thread_id.get(),
            &stale_at,
            cancel_token.turn_nonce().map(str::to_string),
        );
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .expect("fresh shared data should be uniquely owned before registry install")
            .health_registry = Arc::downgrade(&registry);
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                thread_id,
                cancel_token.clone(),
                UserId::new(7),
                user_msg_id,
            )
            .await
        );
        for id in 9_001..=9_003 {
            crate::services::discord::mailbox_enqueue_intervention(
                &shared,
                provider,
                thread_id,
                queued_thread_followup(id),
            )
            .await;
        }
        shared.restart.global_active.store(1, Ordering::Relaxed);
        (registry, shared, cancel_token)
    }

    /// #6199: the THREAD-GUARD force-clean releases the dead dispatch's anchor
    /// but must keep the thread's queued follow-ups (no Superseded drain).
    #[tokio::test]
    async fn thread_guard_force_clean_keeps_the_thread_queue() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let parent_id = ChannelId::new(900_000_000_000_920);
        let thread_id = ChannelId::new(900_000_000_000_921);
        let (_registry, shared, token) =
            seed_stale_thread_with_queue(&provider, thread_id, MessageId::new(8_001)).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        let mut rx =
            crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
                shared.as_ref(),
            );

        let proof = super::thread_guard_should_force_clean_stale_thread(
            &shared,
            &provider,
            thread_id,
            chrono::Utc::now().timestamp(),
        )
        .await
        .expect("a dead dispatch thread must warrant force-clean");
        assert!(
            super::thread_guard_force_clean_stale_thread(
                &shared,
                &provider,
                parent_id,
                thread_id,
                Some(proof),
            )
            .await
        );

        let after = crate::services::discord::mailbox_snapshot(&shared, thread_id).await;
        assert_eq!(
            after.intervention_queue.len(),
            3,
            "force-clean must not supersede the thread's queued follow-ups"
        );
        assert!(after.cancel_token.is_none());
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(!shared.dispatch.thread_parents.contains_key(&parent_id));
        let event = rx
            .try_recv()
            .expect("the kept queue must be handed to the completion listener");
        assert_eq!(event.channel_id, thread_id);
    }

    /// #6199 ABA: a successor that reuses the anchor id after the stale proof
    /// keeps its token, queue, inflight file, and parent guard.
    #[tokio::test]
    async fn thread_guard_force_clean_leaves_a_successor_episode_untouched() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let parent_id = ChannelId::new(900_000_000_000_930);
        let thread_id = ChannelId::new(900_000_000_000_931);
        let anchor = MessageId::new(8_001);
        let (_registry, shared, _stale_token) =
            seed_stale_thread_with_queue(&provider, thread_id, anchor).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        let proof = super::thread_guard_should_force_clean_stale_thread(
            &shared,
            &provider,
            thread_id,
            chrono::Utc::now().timestamp(),
        )
        .await
        .expect("a dead dispatch thread must warrant force-clean");

        // A->B under the same message id; B is backdated so only its nonce differs.
        crate::services::discord::mailbox_finish_turn(&shared, &provider, thread_id).await;
        let successor = Arc::new(CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                thread_id,
                successor.clone(),
                UserId::new(7),
                anchor,
            )
            .await
        );
        shared
            .mailbox(thread_id)
            .age_active_turn_for_test(std::time::Duration::from_secs(60))
            .await;
        shared.restart.global_active.store(1, Ordering::Relaxed);

        assert!(
            !super::thread_guard_force_clean_stale_thread(
                &shared,
                &provider,
                parent_id,
                thread_id,
                Some(proof),
            )
            .await
        );

        let after = crate::services::discord::mailbox_snapshot(&shared, thread_id).await;
        assert!(
            after
                .cancel_token
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &successor))
        );
        assert_eq!(after.intervention_queue.len(), 3);
        assert!(!successor.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(shared.dispatch.thread_parents.contains_key(&parent_id));
        assert!(
            crate::services::discord::inflight::load_inflight_state(&provider, thread_id.get())
                .is_some()
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
