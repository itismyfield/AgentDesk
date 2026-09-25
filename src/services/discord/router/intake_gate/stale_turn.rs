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
    /// The row the stale verdict was computed from.
    inflight: crate::services::discord::inflight::InflightTurnState,
    /// Taken before every evidence read so a successor started after it fails the finish guard.
    observed_before: std::time::Instant,
}

/// The one episode the THREAD-GUARD force-clean may finish: the stale row plus the
/// mailbox anchor it was matched against.
pub(super) struct ThreadGuardForceCleanProof {
    user_msg_id: Option<u64>,
    turn_nonce: Option<String>,
    observed_before: std::time::Instant,
    inflight: crate::services::discord::inflight::InflightTurnState,
}

/// Parents routed to the thread while the proven episode still held it.
fn snapshot_thread_guard_parents(
    shared: &std::sync::Arc<SharedData>,
    thread_id: serenity::ChannelId,
) -> Vec<serenity::ChannelId> {
    let parents = shared.dispatch.thread_parents.iter();
    parents
        .filter(|entry| *entry.value() == thread_id)
        .map(|entry| *entry.key())
        .collect()
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
        inflight: inflight.clone(),
        observed_before,
    }
}

async fn classify_channel_stale_active_turn_proof(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    now_unix_secs: i64,
) -> Option<StaleActiveTurnProof> {
    let observed_before = std::time::Instant::now();
    let inflight =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())?;
    let registry = shared.health_registry.upgrade()?;
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
    thread_guard_force_clean_proof(proof)
}

/// The stale row and the mailbox anchor must name one episode: an id-only
/// match can be a successor that reused the anchor id.
fn thread_guard_force_clean_proof(
    proof: StaleActiveTurnProof,
) -> Option<ThreadGuardForceCleanProof> {
    let snapshot = proof.snapshot;
    if snapshot.mailbox_active_user_msg_id.is_some_and(|id| {
        id != proof.inflight.user_msg_id
            || snapshot.mailbox_active_turn_nonce != proof.inflight.turn_nonce
    }) {
        return None;
    }
    Some(ThreadGuardForceCleanProof {
        user_msg_id: snapshot.mailbox_active_user_msg_id,
        turn_nonce: snapshot.mailbox_active_turn_nonce,
        observed_before: proof.observed_before,
        inflight: proof.inflight,
    })
}

/// Finishes the proof's episode without completion, cleans up only what it still owns, then
/// publishes queue-eligible; `false` (caller keeps queueing) once the proof no longer holds.
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
    // Parents to kick once the thread is released; their mappings stay for the
    // intake idle check to clear.
    let parents = snapshot_thread_guard_parents(shared, thread_id);
    let Ok(finish) = thread_guard_release_proven_anchor(shared, provider, thread_id, &proof).await
    else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔀 THREAD-GUARD: thread {} episode changed since the stale proof; keeping it",
            thread_id
        );
        return false;
    };
    thread_guard_cleanup_released_episode(shared, provider, thread_id, &proof, finish, parents)
}

/// Releases the proof's anchor without a completion event. `Err` means a
/// successor owns the thread; `Ok(None)` means the proof names no anchor.
async fn thread_guard_release_proven_anchor(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    thread_id: serenity::ChannelId,
    proof: &ThreadGuardForceCleanProof,
) -> Result<Option<crate::services::turn_orchestrator::FinishTurnResult>, ()> {
    // An absent anchor is no authority to finish whatever the thread holds now.
    let Some(user_msg_id) = proof.user_msg_id.filter(|id| *id != 0) else {
        return Ok(None);
    };
    let finish =
        crate::services::discord::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_without_completion(
            shared,
            provider,
            thread_id,
            serenity::MessageId::new(user_msg_id),
            proof.turn_nonce.clone(),
            proof.observed_before,
        )
        .await;
    if finish.removed_token.is_none() {
        return Err(());
    }
    Ok(Some(finish))
}

/// Cleanup limited to what the proven episode still owns. The queue-eligible edge
/// goes last, though other admission paths may already be starting a successor.
fn thread_guard_cleanup_released_episode(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    thread_id: serenity::ChannelId,
    proof: &ThreadGuardForceCleanProof,
    finish: Option<crate::services::turn_orchestrator::FinishTurnResult>,
    parents: Vec<serenity::ChannelId>,
) -> bool {
    // Without a finished anchor nothing was released, so the row and the guard stay.
    let Some(finish) = finish else {
        return false;
    };
    let _ = crate::services::discord::inflight::clear_inflight_state_for_snapshot(
        provider,
        &proof.inflight,
    );
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🔓 THREAD-GUARD: stale inflight detected for thread {}, cleaning up and proceeding",
        thread_id
    );
    // Releases `cancelled` and `global_active` without killing tmux by name, since a successor
    // may reuse the session; a live wedged session is left running and not reclaimed automatically.
    crate::services::discord::stall_recovery::finalize_orphaned_clear_preserve_session(
        shared,
        thread_id,
        finish.removed_token.clone(),
        "1446_thread_guard_stale_inflight",
    );
    crate::services::discord::turn_finalizer::cleanup::kickoff_thread_parents_after_finalize(
        shared, provider, parents,
    );
    crate::services::discord::turn_completion_events::publish_mailbox_release_completion_event(
        shared,
        thread_id,
        proof.user_msg_id,
        &finish,
    );
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
        seed_inflight_row(provider, channel_id, updated_at, 8_001, turn_nonce);
    }

    /// Writes one episode's inflight row and returns it as persisted.
    fn seed_inflight_row(
        provider: &ProviderKind,
        channel_id: u64,
        updated_at: &str,
        user_msg_id: u64,
        turn_nonce: Option<String>,
    ) -> crate::services::discord::inflight::InflightTurnState {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("test-thread-guard".to_string()),
            42,
            user_msg_id,
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
        crate::services::discord::inflight::load_inflight_state(provider, channel_id)
            .expect("seeded inflight row must load")
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

    fn stale_updated_at() -> String {
        local_at_offset(
            chrono::Utc::now().timestamp(),
            -(crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 5,
        )
    }

    async fn shared_with_registry(
        provider: &ProviderKind,
    ) -> (
        Arc<crate::services::discord::health::HealthRegistry>,
        Arc<SharedData>,
    ) {
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .expect("fresh shared data should be uniquely owned before registry install")
            .health_registry = Arc::downgrade(&registry);
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        (registry, shared)
    }

    /// Stale dead-dispatch thread with an active anchor and `queued` follow-ups.
    async fn seed_stale_thread_with_queue(
        provider: &ProviderKind,
        thread_id: ChannelId,
        user_msg_id: MessageId,
        queued: u64,
    ) -> (
        Arc<crate::services::discord::health::HealthRegistry>,
        Arc<SharedData>,
        Arc<CancelToken>,
    ) {
        let cancel_token = Arc::new(CancelToken::new());
        seed_inflight_row(
            provider,
            thread_id.get(),
            &stale_updated_at(),
            user_msg_id.get(),
            cancel_token.turn_nonce().map(str::to_string),
        );
        let (registry, shared) = shared_with_registry(provider).await;
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
        for id in (9_001..).take(queued as usize) {
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

    /// The THREAD-GUARD force-clean releases the dead dispatch's anchor
    /// but must keep the thread's queued follow-ups (no Superseded drain).
    #[tokio::test]
    async fn thread_guard_force_clean_keeps_the_thread_queue() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let parent_id = ChannelId::new(900_000_000_000_920);
        let thread_id = ChannelId::new(900_000_000_000_921);
        let (_registry, shared, token) =
            seed_stale_thread_with_queue(&provider, thread_id, MessageId::new(8_001), 3).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        let mut rx =
            crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
                shared.as_ref(),
            );

        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;
        assert!(force_clean(&shared, thread_id, Some(proof)).await);

        let after = crate::services::discord::mailbox_snapshot(&shared, thread_id).await;
        assert_eq!(
            after.intervention_queue.len(),
            3,
            "force-clean must not supersede the thread's queued follow-ups"
        );
        assert!(after.cancel_token.is_none());
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(shared.dispatch.thread_parents.contains_key(&parent_id));
        assert_eq!(kicked_parents(&shared), vec![parent_id]);
        let event = rx
            .try_recv()
            .expect("the kept queue must be handed to the completion listener");
        assert_eq!(event.channel_id, thread_id);
    }

    /// ABA: a successor that reuses the anchor id after the stale proof
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
            seed_stale_thread_with_queue(&provider, thread_id, anchor, 3).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;

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

        assert!(!force_clean(&shared, thread_id, Some(proof)).await);

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

    const ID_BASE: u64 = 900_000_000_000_000;

    fn channel(offset: u64) -> ChannelId {
        ChannelId::new(ID_BASE + offset)
    }

    async fn force_clean_proof_for(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        thread_id: ChannelId,
    ) -> super::ThreadGuardForceCleanProof {
        let now = chrono::Utc::now().timestamp();
        super::thread_guard_should_force_clean_stale_thread(shared, provider, thread_id, now)
            .await
            .expect("a dead dispatch thread must warrant force-clean")
    }

    async fn force_clean(
        shared: &Arc<SharedData>,
        thread_id: ChannelId,
        proof: Option<super::ThreadGuardForceCleanProof>,
    ) -> bool {
        let (provider, parent_id) = (ProviderKind::Codex, ChannelId::new(1));
        super::thread_guard_force_clean_stale_thread(shared, &provider, parent_id, thread_id, proof)
            .await
    }

    /// Runs the finish step alone, as the force-clean does before its cleanup.
    async fn release_proven_anchor(
        shared: &Arc<SharedData>,
        thread_id: ChannelId,
        proof: &super::ThreadGuardForceCleanProof,
    ) -> (
        crate::services::turn_orchestrator::FinishTurnResult,
        Vec<ChannelId>,
    ) {
        let parents = super::snapshot_thread_guard_parents(shared, thread_id);
        let finish = super::thread_guard_release_proven_anchor(
            shared,
            &ProviderKind::Codex,
            thread_id,
            proof,
        )
        .await;
        (
            finish.expect("the proof owns the anchor").expect("anchor"),
            parents,
        )
    }

    /// Starts `token`'s episode on the thread and saves its row.
    async fn start_episode(
        shared: &Arc<SharedData>,
        thread_id: ChannelId,
        token: &Arc<CancelToken>,
        user_msg_id: u64,
        updated_at: &str,
    ) {
        let (user, anchor) = (UserId::new(7), MessageId::new(user_msg_id));
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                shared,
                thread_id,
                token.clone(),
                user,
                anchor
            )
            .await
        );
        let nonce = token.turn_nonce().map(str::to_string);
        seed_inflight_row(
            &ProviderKind::Codex,
            thread_id.get(),
            updated_at,
            user_msg_id,
            nonce,
        );
    }

    fn fresh_updated_at() -> String {
        local_at_offset(chrono::Utc::now().timestamp(), 0)
    }

    /// The `(parent -> thread, thread -> alt)` routing entries.
    fn routing(
        shared: &SharedData,
        parent_id: ChannelId,
        thread_id: ChannelId,
    ) -> (Option<ChannelId>, Option<ChannelId>) {
        let dispatch = &shared.dispatch;
        (
            dispatch.thread_parents.get(&parent_id).map(|e| *e.value()),
            dispatch.role_overrides.get(&thread_id).map(|e| *e.value()),
        )
    }

    async fn assert_episode_untouched(
        shared: &Arc<SharedData>,
        thread_id: ChannelId,
        token: &Arc<CancelToken>,
        user_msg_id: u64,
    ) {
        let active = crate::services::discord::mailbox_snapshot(shared, thread_id)
            .await
            .cancel_token;
        assert!(active.is_some_and(|current| Arc::ptr_eq(&current, token)));
        assert!(!token.cancelled.load(Ordering::Relaxed));
        let row = crate::services::discord::inflight::load_inflight_state(
            &ProviderKind::Codex,
            thread_id.get(),
        )
        .expect("the live episode's row must survive");
        assert_eq!(row.user_msg_id, user_msg_id);
        assert_eq!(row.turn_nonce.as_deref(), token.turn_nonce());
    }

    /// A proof taken while the thread had no anchor must not finish the episode
    /// that started afterwards.
    #[tokio::test]
    async fn thread_guard_force_clean_without_anchor_defers_to_a_new_episode() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let (parent_id, thread_id) = (channel(940), channel(941));
        let alt_id = channel(942);
        let nonce = Some("nonce-a".to_string());
        let stale_row =
            seed_inflight_row(&provider, ID_BASE + 941, &stale_updated_at(), 8_001, nonce);
        let (_registry, shared) = shared_with_registry(&provider).await;
        let proof = super::ThreadGuardForceCleanProof {
            user_msg_id: None,
            turn_nonce: None,
            observed_before: std::time::Instant::now(),
            inflight: stale_row,
        };

        let successor = Arc::new(CancelToken::new());
        start_episode(&shared, thread_id, &successor, 8_002, &fresh_updated_at()).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        shared.dispatch.role_overrides.insert(thread_id, alt_id);
        shared.restart.global_active.store(1, Ordering::Relaxed);

        assert!(!force_clean(&shared, thread_id, Some(proof)).await);
        assert_episode_untouched(&shared, thread_id, &successor, 8_002).await;
        let routing_after = routing(&shared, parent_id, thread_id);
        assert_eq!(routing_after, (Some(thread_id), Some(alt_id)));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    /// A token held without an anchor keeps the stale row as its only durable
    /// evidence, along with the token and the parent guard.
    #[tokio::test]
    async fn thread_guard_force_clean_without_anchor_keeps_the_row_of_a_held_token() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let (parent_id, thread_id) = (channel(1_060), channel(1_061));
        let nonce = Some("nonce-a".to_string());
        seed_inflight_row(
            &provider,
            ID_BASE + 1_061,
            &stale_updated_at(),
            8_001,
            nonce,
        );
        let (_registry, shared) = shared_with_registry(&provider).await;
        let token = Arc::new(CancelToken::new());
        shared
            .mailbox(thread_id)
            .recovery_kickoff(token.clone(), UserId::new(7), None)
            .await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;

        assert!(!force_clean(&shared, thread_id, Some(proof)).await);
        let row =
            crate::services::discord::inflight::load_inflight_state(&provider, ID_BASE + 1_061);
        assert!(
            row.is_some(),
            "the held token's only durable evidence stays"
        );
        let after = crate::services::discord::mailbox_snapshot(&shared, thread_id).await;
        assert!(
            after
                .cancel_token
                .is_some_and(|current| Arc::ptr_eq(&current, &token))
        );
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert!(shared.dispatch.thread_parents.contains_key(&parent_id));
    }

    /// A stale verdict from the old row must not authorize finishing the
    /// mailbox's newer episode that shares only the anchor id.
    #[tokio::test]
    async fn thread_guard_proof_rejects_a_row_from_another_episode() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let thread_id = channel(951);
        let (registry, shared) = shared_with_registry(&provider).await;
        let observed_before = std::time::Instant::now();
        let mut old_row = inflight_with_updated_at(&provider, ID_BASE + 951, &stale_updated_at());
        old_row.turn_nonce = Some("nonce-a".to_string());

        // B=(8001, b) claims the thread and saves its row right after A's row was read.
        let successor = Arc::new(CancelToken::new());
        start_episode(&shared, thread_id, &successor, 8_001, &stale_updated_at()).await;
        let mailbox = shared.mailbox(thread_id);
        mailbox
            .age_active_turn_for_test(std::time::Duration::from_secs(60))
            .await;
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let snapshot = registry
            .snapshot_watcher_state_for_provider(&provider, ID_BASE + 951)
            .await
            .expect("registered provider must snapshot");
        let now = chrono::Utc::now().timestamp();
        let mixed = super::bind_stale_active_turn_proof(&old_row, snapshot, now, observed_before);
        assert!(
            super::stale_turn_axis_b_warrants(&provider, &mixed),
            "the mixed evidence alone would authorize a force-clean"
        );

        let proof = super::thread_guard_force_clean_proof(mixed);
        assert!(proof.is_none());
        assert!(!force_clean(&shared, thread_id, proof).await);
        assert_episode_untouched(&shared, thread_id, &successor, 8_001).await;
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    /// After A's finish, a replacement B keeps its row, mapping and override while
    /// A's parent is kicked; the queue-eligible edge follows the cleanup.
    #[tokio::test]
    async fn thread_guard_cleanup_after_finish_spares_a_replacement_episode() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let (parent_id, thread_id) = (channel(960), channel(961));
        let (alt_id, next_thread) = (channel(962), channel(963));
        let next_alt = channel(964);
        let (_registry, shared, _stale_token) =
            seed_stale_thread_with_queue(&provider, thread_id, MessageId::new(8_001), 0).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        shared.dispatch.role_overrides.insert(thread_id, alt_id);
        let mut rx =
            crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
                shared.as_ref(),
            );
        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;
        let (finish, owned) = release_proven_anchor(&shared, thread_id, &proof).await;
        assert!(
            rx.try_recv().is_err(),
            "no queue-eligible edge before cleanup"
        );

        let replacement = Arc::new(CancelToken::new());
        start_episode(&shared, thread_id, &replacement, 9_001, &fresh_updated_at()).await;
        shared
            .dispatch
            .thread_parents
            .insert(parent_id, next_thread);
        shared.dispatch.role_overrides.insert(thread_id, next_alt);

        assert!(!finish.has_pending);
        let finish = Some(finish);
        assert!(super::thread_guard_cleanup_released_episode(
            &shared, &provider, thread_id, &proof, finish, owned,
        ));
        assert_episode_untouched(&shared, thread_id, &replacement, 9_001).await;
        let routing_after = routing(&shared, parent_id, thread_id);
        assert_eq!(routing_after, (Some(next_thread), Some(next_alt)));
        assert_eq!(kicked_parents(&shared), vec![parent_id]);
        let event = rx
            .try_recv()
            .expect("the queue-eligible edge must follow the cleanup");
        assert_eq!(event.channel_id, thread_id);
    }

    /// A successor that reuses A's id and nonce but started after the proof was
    /// observed is rejected by the started-before cutoff alone.
    #[tokio::test]
    async fn thread_guard_force_clean_rejects_an_episode_started_after_the_proof() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let (thread_id, anchor) = (channel(971), MessageId::new(8_001));
        let (_registry, shared, stale_token) =
            seed_stale_thread_with_queue(&provider, thread_id, anchor, 0).await;
        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;

        crate::services::discord::mailbox_finish_turn(&shared, &provider, thread_id).await;
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let nonce = stale_token.turn_nonce().map(str::to_string);
        let successor = Arc::new(CancelToken::from_persisted_turn_nonce(nonce));
        start_episode(&shared, thread_id, &successor, 8_001, &stale_updated_at()).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);

        assert!(!force_clean(&shared, thread_id, Some(proof)).await);
        assert_episode_untouched(&shared, thread_id, &successor, 8_001).await;
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    /// B re-registers the same `parent -> thread` after A's finish released the
    /// thread; A's cleanup keeps that guard and only kicks the parent queue.
    #[tokio::test]
    async fn thread_guard_force_clean_spares_a_same_value_parent_reregistered_after_release() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let (parent_id, thread_id) = (channel(980), channel(981));
        let (_registry, shared, _stale_token) =
            seed_stale_thread_with_queue(&provider, thread_id, MessageId::new(8_001), 0).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;
        let (finish, owned) = release_proven_anchor(&shared, thread_id, &proof).await;

        let successor = Arc::new(CancelToken::new());
        start_episode(&shared, thread_id, &successor, 9_001, &fresh_updated_at()).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        let finish = Some(finish);
        assert!(super::thread_guard_cleanup_released_episode(
            &shared, &provider, thread_id, &proof, finish, owned,
        ));
        assert_episode_untouched(&shared, thread_id, &successor, 9_001).await;
        let parent = shared.dispatch.thread_parents.get(&parent_id);
        assert_eq!(parent.map(|e| *e.value()), Some(thread_id));
        assert_eq!(kicked_parents(&shared), vec![parent_id]);
    }

    /// A token restored without rewriting the row after an anchorless, tokenless
    /// proof keeps the row, the token and the parent guard.
    #[tokio::test]
    async fn thread_guard_force_clean_without_anchor_keeps_a_row_restored_after_the_proof() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let (parent_id, thread_id) = (channel(1_070), channel(1_071));
        let nonce = Some("nonce-a".to_string());
        seed_inflight_row(
            &provider,
            ID_BASE + 1_071,
            &stale_updated_at(),
            8_001,
            nonce,
        );
        let (_registry, shared) = shared_with_registry(&provider).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;

        let token = Arc::new(CancelToken::new());
        let mailbox = shared.mailbox(thread_id);
        mailbox
            .recovery_kickoff(token.clone(), UserId::new(7), None)
            .await;

        assert!(!force_clean(&shared, thread_id, Some(proof)).await);
        let row =
            crate::services::discord::inflight::load_inflight_state(&provider, ID_BASE + 1_071);
        assert!(row.is_some(), "the restored token's row stays");
        let after = crate::services::discord::mailbox_snapshot(&shared, thread_id).await;
        let current = after.cancel_token.expect("the restored token stays");
        assert!(Arc::ptr_eq(&current, &token));
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert!(shared.dispatch.thread_parents.contains_key(&parent_id));
    }

    /// The force-clean clears the row and kicks the parent but leaves both routing
    /// entries, with or without queued follow-ups.
    #[tokio::test]
    async fn thread_guard_force_clean_leaves_the_role_override_untouched() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        for queued in [0, 1] {
            let (parent_id, thread_id) = (channel(990 + 3 * queued), channel(991 + 3 * queued));
            let (_registry, shared, _stale_token) =
                seed_stale_thread_with_queue(&provider, thread_id, MessageId::new(8_001), queued)
                    .await;
            let alt_id = channel(992 + 3 * queued);
            shared.dispatch.thread_parents.insert(parent_id, thread_id);
            shared.dispatch.role_overrides.insert(thread_id, alt_id);
            let proof = force_clean_proof_for(&shared, &provider, thread_id).await;

            assert!(force_clean(&shared, thread_id, Some(proof)).await);
            let routing_after = routing(&shared, parent_id, thread_id);
            assert_eq!(
                routing_after,
                (Some(thread_id), Some(alt_id)),
                "queued={queued}"
            );
            assert_eq!(kicked_parents(&shared), vec![parent_id]);
            let row =
                crate::services::discord::inflight::load_inflight_state(&provider, thread_id.get());
            assert!(row.is_none());
        }
    }

    /// A successor that registered its own override before the force-clean
    /// keeps it: the cleanup cannot tell that entry from the dead episode's.
    #[tokio::test]
    async fn thread_guard_force_clean_keeps_a_successor_preregistered_role_override() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let (parent_id, thread_id) = (channel(1_050), channel(1_051));
        let (alt_id, successor_parent, successor_alt) =
            (channel(1_052), channel(1_053), channel(1_054));
        let (_registry, shared, _stale_token) =
            seed_stale_thread_with_queue(&provider, thread_id, MessageId::new(8_001), 0).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        shared.dispatch.role_overrides.insert(thread_id, alt_id);
        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;

        // B reuses the thread from another parent and registers before claiming it.
        let dispatch = &shared.dispatch;
        dispatch.thread_parents.insert(successor_parent, thread_id);
        dispatch.role_overrides.insert(thread_id, successor_alt);

        assert!(force_clean(&shared, thread_id, Some(proof)).await);
        assert!(dispatch.thread_parents.contains_key(&parent_id));
        let role_override = dispatch.role_overrides.get(&thread_id).map(|e| *e.value());
        assert_eq!(role_override, Some(successor_alt));
    }

    /// Parents whose deferred idle-queue kick was scheduled, read before any yield.
    fn kicked_parents(shared: &SharedData) -> Vec<ChannelId> {
        let channels = &shared.restart.deferred_hook_channels;
        channels.iter().map(|entry| *entry.key()).collect()
    }

    /// Releasing the dead episode must not kill tmux by name: a successor may
    /// already run in a session with the same non-unified-thread name.
    #[test]
    fn thread_guard_force_clean_does_not_kill_successor_shared_tmux_name() {
        use crate::services::provider::cancel_token_cleanup::executor;
        executor::with_executor_dispatch_seam(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime");
            runtime.block_on(async {
                let temp = tempfile::tempdir().expect("create temp runtime root");
                let _guard = EnvRootGuard::set(temp.path());
                let provider = ProviderKind::Codex;
                let (thread_id, anchor) = (channel(1_001), MessageId::new(8_001));
                let (_registry, shared, stale_token) =
                    seed_stale_thread_with_queue(&provider, thread_id, anchor, 1).await;
                let session_name = "AgentDesk-codex-thread-guard-shared-session";
                stale_token.bind_unmanaged_session_name(session_name);
                stale_token.store_child_pid(std::process::id());
                let successor = CancelToken::new();
                successor.bind_unmanaged_session_name(session_name);
                let proof = force_clean_proof_for(&shared, &provider, thread_id).await;

                assert!(force_clean(&shared, thread_id, Some(proof)).await);
                assert!(stale_token.cancelled.load(Ordering::Relaxed));
                assert_eq!(executor::tmux_kill_dispatches_for_test(), 0);
                assert_eq!(executor::pid_kill_dispatches_for_test(), 0);
                let binding = stale_token.tmux_session_name();
                assert_eq!(binding.as_deref(), Some(session_name));
                assert!(!successor.cancelled.load(Ordering::Relaxed));
            });
        });
    }

    /// A registration for another thread must not stop the force-clean from
    /// kicking the proven thread's own parent; every mapping stays.
    #[tokio::test]
    async fn thread_guard_force_clean_kicks_target_parent_despite_unrelated_registration() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let (parent_id, thread_id) = (channel(1_010), channel(1_011));
        let (other_parent, other_thread) = (channel(1_013), channel(1_014));
        let (alt_id, other_alt) = (channel(1_012), channel(1_015));
        let (_registry, shared, _stale_token) =
            seed_stale_thread_with_queue(&provider, thread_id, MessageId::new(8_001), 0).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        shared.dispatch.role_overrides.insert(thread_id, alt_id);
        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;
        shared
            .dispatch
            .thread_parents
            .insert(other_parent, other_thread);
        shared
            .dispatch
            .role_overrides
            .insert(other_thread, other_alt);

        assert!(force_clean(&shared, thread_id, Some(proof)).await);
        assert_eq!(kicked_parents(&shared), vec![parent_id]);
        let routing_after = routing(&shared, parent_id, thread_id);
        assert_eq!(routing_after, (Some(thread_id), Some(alt_id)));
        let other_routing = routing(&shared, other_parent, other_thread);
        assert_eq!(other_routing, (Some(other_thread), Some(other_alt)));
    }

    /// Records the finalize log and each completion publish in order, noting
    /// at each publish whether the parent was kicked and the inflight row gone.
    struct CleanupOrder {
        shared: Arc<SharedData>,
        parent_id: ChannelId,
        thread_id: ChannelId,
        seen: Arc<std::sync::Mutex<Vec<(&'static str, bool)>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CleanupOrder {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let target = event.metadata().target();
            let label = if target.ends_with("stall_recovery") {
                "finalize"
            } else if target == "agentdesk::discord::turn_completion_events" {
                "completion"
            } else {
                return;
            };
            let kicked = kicked_parents(&self.shared).contains(&self.parent_id);
            let row = crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Codex,
                self.thread_id.get(),
            );
            let cleaned = kicked && row.is_none();
            self.seen.lock().unwrap().push((label, cleaned));
        }
    }

    /// The queue-eligible edge comes after every cleanup step and hands the
    /// kept follow-up to a real dequeue.
    #[tokio::test]
    async fn thread_guard_force_clean_completion_event_drives_real_dequeue() {
        use tracing_subscriber::layer::SubscriberExt;
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());
        let provider = ProviderKind::Codex;
        let (parent_id, thread_id) = (channel(1_030), channel(1_031));
        let (_registry, shared, _stale_token) =
            seed_stale_thread_with_queue(&provider, thread_id, MessageId::new(8_001), 1).await;
        shared.dispatch.thread_parents.insert(parent_id, thread_id);
        let mut rx =
            crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
                shared.as_ref(),
            );
        let recovery_done = shared.mailboxes.recovery_done(thread_id);
        let proof = force_clean_proof_for(&shared, &provider, thread_id).await;
        let seen = Arc::default();
        let order = CleanupOrder {
            shared: shared.clone(),
            parent_id,
            thread_id,
            seen: Arc::clone(&seen),
        };
        crate::logging::test_capture::pin_callsite_interest();
        let capture = tracing::subscriber::set_default(tracing_subscriber::registry().with(order));

        assert!(force_clean(&shared, thread_id, Some(proof)).await);
        drop(capture);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
            ["finalize", "completion"]
        );
        assert!(
            seen[1].1,
            "cleanup must finish before the queue-eligible edge"
        );
        let event = rx
            .try_recv()
            .expect("the kept queue must reach the listener");
        assert!(event.queue_is_eligible());
        let taken = crate::services::discord::mailbox_take_next_soft_intervention(
            &shared, &provider, thread_id,
        )
        .await;
        let head = taken.intervention.expect("the kept follow-up is dequeued");
        assert_eq!(head.message_id, MessageId::new(9_001));
        assert!(!taken.has_more);
        let after = crate::services::discord::mailbox_snapshot(&shared, thread_id).await;
        assert!(after.intervention_queue.is_empty());
        let wait =
            tokio::time::timeout(std::time::Duration::from_millis(100), recovery_done.wait());
        assert!(wait.await.is_ok(), "the finish must wake recovery waiters");
    }

    fn block_on_under_executor_seam(test: impl std::future::Future<Output = ()>) {
        use crate::services::provider::cancel_token_cleanup::executor;
        executor::with_executor_dispatch_seam(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime");
            runtime.block_on(test);
        });
    }

    /// A stale row whose nonce differs from the live anchor's is not a proof:
    /// the live episode keeps its queue, row, routing and processes.
    #[test]
    fn thread_guard_force_clean_rejects_mismatched_episode() {
        use crate::services::provider::cancel_token_cleanup::executor;
        block_on_under_executor_seam(async {
            let temp = tempfile::tempdir().expect("create temp runtime root");
            let _guard = EnvRootGuard::set(temp.path());
            let provider = ProviderKind::Codex;
            let (parent_id, thread_id) = (channel(1_040), channel(1_041));
            let alt_id = channel(1_042);
            let nonce = Some("nonce-a".to_string());
            seed_inflight_row(
                &provider,
                ID_BASE + 1_041,
                &stale_updated_at(),
                8_001,
                nonce,
            );
            let (_registry, shared) = shared_with_registry(&provider).await;
            let live = Arc::new(CancelToken::new());
            let (user, anchor) = (UserId::new(7), MessageId::new(8_001));
            assert!(
                crate::services::discord::mailbox_try_start_turn(
                    &shared,
                    thread_id,
                    live.clone(),
                    user,
                    anchor,
                )
                .await
            );
            let mailbox = shared.mailbox(thread_id);
            mailbox
                .age_active_turn_for_test(std::time::Duration::from_secs(60))
                .await;
            crate::services::discord::mailbox_enqueue_intervention(
                &shared,
                &provider,
                thread_id,
                queued_thread_followup(9_001),
            )
            .await;
            shared.dispatch.thread_parents.insert(parent_id, thread_id);
            shared.dispatch.role_overrides.insert(thread_id, alt_id);
            shared.restart.global_active.store(1, Ordering::Relaxed);

            let proof = super::thread_guard_should_force_clean_stale_thread(
                &shared,
                &provider,
                thread_id,
                chrono::Utc::now().timestamp(),
            )
            .await;
            assert!(proof.is_none());
            assert!(!force_clean(&shared, thread_id, proof).await);

            let after = crate::services::discord::mailbox_snapshot(&shared, thread_id).await;
            let current = after
                .cancel_token
                .expect("the live episode keeps the thread");
            assert!(Arc::ptr_eq(&current, &live));
            assert!(!live.cancelled.load(Ordering::Relaxed));
            assert_eq!(after.intervention_queue.len(), 1);
            let row =
                crate::services::discord::inflight::load_inflight_state(&provider, thread_id.get())
                    .expect("the row stays");
            assert_eq!(row.turn_nonce.as_deref(), Some("nonce-a"));
            let routing_after = routing(&shared, parent_id, thread_id);
            assert_eq!(routing_after, (Some(thread_id), Some(alt_id)));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
            assert_eq!(executor::tmux_kill_dispatches_for_test(), 0);
            assert_eq!(executor::pid_kill_dispatches_for_test(), 0);
        });
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
