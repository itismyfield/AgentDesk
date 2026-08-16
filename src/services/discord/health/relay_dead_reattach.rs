use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::snapshot::WatcherStateSnapshot;
use super::{HealthRegistry, recovery, stall_liveness};
use crate::services::discord::relay_health::RelayStallState;
use crate::services::discord::{self, SharedData};
use crate::services::provider::ProviderKind;

#[cfg(test)]
#[derive(Clone, Copy)]
enum ReattachApplyHookPoint {
    /// Fires once this tick has decided the channel is a reattach candidate,
    /// immediately before the recovery call. #5071 T3-A1 removed the sibling
    /// `BeforeFenceRead` point together with the #5067 read it existed to race.
    CandidateAccepted,
}
#[cfg(test)]
type ReattachApplyHook = Arc<dyn Fn(ReattachApplyHookPoint) + Send + Sync + 'static>;
#[cfg(test)]
static REATTACH_APPLY_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<ReattachApplyHook>>> =
    std::sync::OnceLock::new();
#[cfg(test)]
fn run_reattach_apply_hook_for_tests(point: ReattachApplyHookPoint) {
    let hook = REATTACH_APPLY_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(point);
    }
}

/// #5071 T3-A1 retired the #5067 `relay_emission_in_flight` conjunct that used
/// to lead this predicate, together with its sibling read in
/// `relay_recovery/apply.rs` — the same origin commit, the same call chain. The
/// reattach this lane requests is itself non-destructive, and the one branch
/// downstream that destroys under the retired fence's old cover — the
/// dead-frontier watcher cancel in `relay_recovery::apply` — carries the
/// registry identity CAS in its place. That CAS is not an emission lease, so the
/// same-incarnation emission race stays a declared non-guarantee.
///
/// That replacement covers the dead-frontier branch and nothing else in this
/// lane. When the reattach instead rebinds an existing claim,
/// `watchers::lifecycle::claims` removes through `remove_tmux_session_locked`,
/// which carries no identity conjunct and is left to T5.
fn should_reattach_relay_dead_watcher(
    snapshot: &WatcherStateSnapshot,
    channel_id: ChannelId,
    latest_runtime_activity_unix_nanos: i64,
    now_unix_secs: i64,
    boot_unix_secs: i64,
) -> bool {
    if snapshot.relay_stall_state != RelayStallState::TmuxAliveRelayDead
        || !snapshot.attached
        || snapshot.watcher_owner_channel_id != Some(channel_id.get())
        || snapshot.tmux_session_alive != Some(true)
    {
        return false;
    }
    if !recovery::stall_watchdog_should_force_clean(
        snapshot.attached,
        true,
        false,
        snapshot.inflight_terminal_delivery_committed,
        snapshot.inflight_started_at.as_deref(),
        now_unix_secs,
        recovery::STALL_WATCHDOG_THRESHOLD_SECS,
        boot_unix_secs,
    ) {
        return false;
    }
    // Fresh runtime activity is a blocker for destructive cleanup, but for a
    // relay frontier that never moved it is evidence that a non-destructive
    // watcher reattach can recover live output.
    if snapshot
        .relay_health
        .relay_frontier_never_advanced_with_unread_tail()
    {
        return true;
    }
    !stall_liveness::stall_watchdog_jsonl_liveness_defers_force_clean(
        latest_runtime_activity_unix_nanos,
        now_unix_secs,
        recovery::STALL_WATCHDOG_LIVENESS_FRESHNESS_SECS,
    )
}

/// What the relay-dead reattach lane did with one stall-watchdog tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReattachLaneOutcome {
    /// This channel is not the lane's candidate, or the recovery call reported
    /// nothing the lane can stand behind. The tick walks on into its remaining
    /// branches.
    Untouched,
    /// The reattach reused a live incumbent. The lane still owns this tick, but
    /// nothing was repaired.
    HandledWithoutRepair,
    /// The reattach reported a real transition.
    Repaired,
}

impl ReattachLaneOutcome {
    /// Whether the tick stops here. The stall-watchdog pass skips its remaining
    /// branches — several of them destructive — for anything but `Untouched`.
    pub(super) fn handled_tick(self) -> bool {
        !matches!(self, Self::Untouched)
    }

    /// Whether the pass may add this channel to the `cleaned` total it logs.
    /// Owning the tick is not the same as having repaired something.
    pub(super) fn counts_as_cleaned(self) -> bool {
        matches!(self, Self::Repaired)
    }
}

pub(super) async fn try_apply(
    registry: &HealthRegistry,
    shared: Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    now_unix_secs: i64,
) -> ReattachLaneOutcome {
    let Some(latest_activity_unix_nanos) = snapshot
        .tmux_session
        .as_deref()
        .map(crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos)
    else {
        return ReattachLaneOutcome::Untouched;
    };
    if !should_reattach_relay_dead_watcher(
        snapshot,
        channel_id,
        latest_activity_unix_nanos,
        now_unix_secs,
        registry.started_at_unix(),
    ) {
        return ReattachLaneOutcome::Untouched;
    }
    #[cfg(test)]
    run_reattach_apply_hook_for_tests(ReattachApplyHookPoint::CandidateAccepted);
    match discord::relay_recovery::auto_apply_relay_recovery_for_shared(
        registry,
        shared,
        provider,
        channel_id.get(),
        discord::relay_recovery::RelayRecoveryActionKind::ReattachWatcher,
        discord::relay_recovery::RelayRecoveryApplySource::StallWatchdog,
    )
    .await
    {
        Ok(response) => reattach_lane_outcome(
            response.applied,
            response.apply_result.as_ref().map(|result| result.status),
        ),
        Err(error) => {
            tracing::warn!(
                target: "agentdesk::discord::relay_recovery",
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                status = error.status_str(),
                body = %error.body(),
                "relay-dead watcher reattach skipped"
            );
            ReattachLaneOutcome::Untouched
        }
    }
}

/// Split the recovery response into the two questions the stall-watchdog tick
/// asks separately.
///
/// #5021 stopped counting `reuse_existing_live_watcher` as an applied heal so
/// the auto-heal budget can back off on a repeating no-op; that accounting
/// correction must not, as a side effect, drop a live turn into the destructive
/// branches that follow this lane. So the reuse status keeps the short-circuit
/// it already had while its budget settles as a refund — but it is not a repair,
/// and #5396 stopped letting it inflate the pass's `cleaned` total, whose one
/// production consumer is the `stall-watchdog (provider): cleaned=N` operator
/// log line in `spawn_stall_watchdog`.
fn reattach_lane_outcome(applied: bool, apply_status: Option<&str>) -> ReattachLaneOutcome {
    if applied {
        ReattachLaneOutcome::Repaired
    } else if apply_status
        .is_some_and(discord::relay_recovery::relay_recovery_status_reused_live_watcher)
    {
        ReattachLaneOutcome::HandledWithoutRepair
    } else {
        ReattachLaneOutcome::Untouched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::relay_health::{RelayActiveTurn, RelayHealthSnapshot};
    use chrono::TimeZone;
    use std::sync::atomic::Ordering;

    struct ReattachHookGuard {
        previous: Option<ReattachApplyHook>,
    }

    impl Drop for ReattachHookGuard {
        fn drop(&mut self) {
            let mut slot = REATTACH_APPLY_HOOK
                .get_or_init(|| std::sync::Mutex::new(None))
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *slot = self.previous.take();
        }
    }

    fn set_reattach_hook(hook: ReattachApplyHook) -> ReattachHookGuard {
        let mut slot = REATTACH_APPLY_HOOK
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous = slot.replace(hook);
        ReattachHookGuard { previous }
    }

    fn local_string(unix: i64) -> String {
        chrono::Local
            .timestamp_opt(unix, 0)
            .single()
            .expect("valid local time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    fn snapshot(started_at: &str) -> WatcherStateSnapshot {
        WatcherStateSnapshot {
            provider: "codex".to_string(),
            attached: true,
            tmux_session: Some("AgentDesk-codex-test".to_string()),
            watcher_owner_channel_id: Some(42),
            last_relay_offset: 0,
            inflight_state_present: true,
            last_relay_ts_ms: 0,
            last_capture_offset: Some(128),
            capture_coordinate: crate::services::discord::health::liveness_authority::CaptureCoordinateObservation {
                offset: Some(128),
                path_hash: 0,
                file_id: None,
                status: crate::services::discord::health::liveness_authority::CoordinateStatus::Observed,
            },
            unread_bytes: Some(128),
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: Some(started_at.to_string()),
            inflight_updated_at: Some(started_at.to_string()),
            inflight_user_msg_id: Some(1),
            inflight_current_msg_id: Some(2),
            tmux_session_alive: Some(true),
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(1),
            mailbox_active_turn_nonce: None,
            bound_output_path: None,
            bound_session_id: None,
            transcript_binding_stall: "none",
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: Some("/tmp/AgentDesk-codex-test.jsonl".to_string()),
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: RelayHealthSnapshot {
                provider: "codex".to_string(),
                channel_id: 42,
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: Some("AgentDesk-codex-test".to_string()),
                tmux_alive: Some(true),
                watcher_attached: true,
                watcher_attached_stale: false,
                watcher_owner_channel_id: Some(42),
                watcher_owns_live_relay: true,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(2),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(1),
                mailbox_turn_started_at_ms: None,
                mailbox_turn_age_secs: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: Some(2),
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: None,
                last_relay_age_secs: None,
                last_outbound_activity_ms: None,
                last_capture_offset: Some(128),
                last_relay_offset: 0,
                unread_bytes: Some(128),
                desynced: true,
                stale_thread_proof: false,
                unpaired_active_token_reconfirmed: false,
            },
        }
    }

    /// #5021: the budget correction is accounting-only. The relay-dead lane must
    /// still report the reuse no-op as handled so this tick keeps skipping the
    /// destructive branches that follow the reattach call.
    ///
    /// #5396 item 5: owning the tick is where that stops. The reuse no-op
    /// repaired nothing, so it must not be added to the pass's `cleaned` total.
    /// Making `HandledWithoutRepair` count as cleaned fails this test.
    #[test]
    fn reuse_no_op_keeps_the_relay_dead_tick_short_circuit() {
        let reuse = reattach_lane_outcome(false, Some("reuse_existing_live_watcher"));
        assert_eq!(reuse, ReattachLaneOutcome::HandledWithoutRepair);
        assert!(reuse.handled_tick());
        assert!(
            !reuse.counts_as_cleaned(),
            "a reused live incumbent repaired nothing and must not be counted cleaned"
        );

        let repaired = reattach_lane_outcome(true, Some("reattached_watcher"));
        assert_eq!(repaired, ReattachLaneOutcome::Repaired);
        assert!(repaired.handled_tick());
        assert!(repaired.counts_as_cleaned());

        for untouched in [
            reattach_lane_outcome(false, Some("rebind_failed")),
            reattach_lane_outcome(false, None),
        ] {
            assert_eq!(untouched, ReattachLaneOutcome::Untouched);
            assert!(!untouched.handled_tick());
            assert!(!untouched.counts_as_cleaned());
        }
    }

    /// #5071 T3-A1 fixed mutation gate (c), fence site 1 of 2: the #5067
    /// in-flight emission read is GONE from this lane. Re-adding
    /// `shared.relay_emission_in_flight(..)` here — as a `try_apply` early
    /// return or as a `should_reattach_relay_dead_watcher` conjunct — makes this
    /// test fail, because the candidate must still be accepted while the relay
    /// slot is held. The sibling site is pinned by
    /// `relay_recovery::tests::post_gate_relay_emission_no_longer_blocks_dead_frontier_watcher_cancel`.
    #[tokio::test]
    async fn relay_dead_reattach_no_longer_fences_on_live_relay_emission() {
        let now = chrono::Utc::now().timestamp();
        let stale = local_string(now - (recovery::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1);
        let channel = ChannelId::new(50_220_004);
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared
            .tmux_relay_coord(channel)
            .relay_slot
            .store(1, Ordering::Release);
        assert!(shared.relay_emission_in_flight(channel));
        let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_accepted = Arc::clone(&accepted);
        let _hook = set_reattach_hook(Arc::new(move |point| match point {
            ReattachApplyHookPoint::CandidateAccepted => {
                hook_accepted.store(true, Ordering::Release)
            }
        }));
        let mut registry = HealthRegistry::new();
        registry.started_at_unix = now - (recovery::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 100;

        // The recovery call that follows resolves no snapshot for this
        // unregistered provider, so `try_apply` still returns false; the hook is
        // what proves the candidate got past the retired fence.
        let _ = try_apply(
            &registry,
            shared,
            &ProviderKind::Codex,
            channel,
            &WatcherStateSnapshot {
                watcher_owner_channel_id: Some(channel.get()),
                relay_health: RelayHealthSnapshot {
                    channel_id: channel.get(),
                    watcher_owner_channel_id: Some(channel.get()),
                    ..snapshot(&stale).relay_health
                },
                ..snapshot(&stale)
            },
            now,
        )
        .await;

        assert!(
            accepted.load(Ordering::Acquire),
            "an in-flight relay emission must no longer block the non-destructive reattach lane"
        );
    }

    #[test]
    fn relay_dead_watcher_reattach_handles_dead_frontier_liveness() {
        let now = chrono::Utc::now().timestamp();
        let stale = local_string(now - (recovery::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 1);
        let fresh = local_string(now - 5);
        let stale_activity = (now - (recovery::STALL_WATCHDOG_LIVENESS_FRESHNESS_SECS as i64) - 1)
            .saturating_mul(1_000_000_000);
        let fresh_activity = (now - 5).saturating_mul(1_000_000_000);

        let boot = now - (recovery::STALL_WATCHDOG_THRESHOLD_SECS as i64) - 100;
        assert!(should_reattach_relay_dead_watcher(
            &snapshot(&stale),
            ChannelId::new(42),
            stale_activity,
            now,
            boot,
        ));
        let mut wrong_state = snapshot(&stale);
        wrong_state.relay_stall_state = RelayStallState::ActiveForegroundStream;
        let mut wrong_owner = snapshot(&stale);
        wrong_owner.watcher_owner_channel_id = Some(99);
        let mut committed = snapshot(&stale);
        committed.inflight_terminal_delivery_committed = true;
        let mut fresh_outbound = snapshot(&stale);
        fresh_outbound.relay_health.last_outbound_activity_ms = Some((now - 5) * 1000);
        let mut advanced_frontier = snapshot(&stale);
        advanced_frontier.last_relay_ts_ms = (now - 30) * 1000;
        advanced_frontier.last_relay_offset = 64;
        advanced_frontier.last_capture_offset = Some(128);
        advanced_frontier.unread_bytes = Some(64);
        advanced_frontier.relay_health.last_relay_ts_ms = Some((now - 30) * 1000);
        advanced_frontier.relay_health.last_relay_offset = 64;
        advanced_frontier.relay_health.last_capture_offset = Some(128);
        advanced_frontier.relay_health.unread_bytes = Some(64);
        let recent_boot = now - 5;

        for (name, candidate, activity, boot_unix_secs) in [
            ("wrong stall state", wrong_state, stale_activity, boot),
            ("wrong owner", wrong_owner, stale_activity, boot),
            ("terminal committed", committed, stale_activity, boot),
            ("fresh inflight", snapshot(&fresh), stale_activity, boot),
            (
                "fresh advanced relay frontier",
                advanced_frontier.clone(),
                fresh_activity,
                boot,
            ),
            (
                "post-restart grace",
                snapshot(&stale),
                stale_activity,
                recent_boot,
            ),
        ] {
            assert!(
                !should_reattach_relay_dead_watcher(
                    &candidate,
                    ChannelId::new(42),
                    activity,
                    now,
                    boot_unix_secs,
                ),
                "{name}"
            );
        }
        assert!(
            should_reattach_relay_dead_watcher(
                &snapshot(&stale),
                ChannelId::new(42),
                fresh_activity,
                now,
                boot,
            ),
            "fresh runtime activity is positive liveness for the dead-frontier reattach signature"
        );
        assert!(
            should_reattach_relay_dead_watcher(
                &advanced_frontier,
                ChannelId::new(42),
                stale_activity,
                now,
                boot,
            ),
            "advanced relay frontiers still require stale runtime activity before reattach"
        );
        assert!(
            should_reattach_relay_dead_watcher(
                &fresh_outbound,
                ChannelId::new(42),
                stale_activity,
                now,
                boot,
            ),
            "recent outbound activity must not block non-destructive watcher reattach"
        );
    }
}
