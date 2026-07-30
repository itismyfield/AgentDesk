use std::sync::Arc;

use super::auto_heal_attempts::{
    auto_heal_key, cancel_unapplied_auto_heal_attempt_if_current,
    commit_auto_heal_attempt_if_current, record_auto_heal_confirm_failure_if_current,
    refund_auto_heal_attempt_if_current, remaining_auto_heal_attempts, reserve_auto_heal_attempt,
};
use super::auto_heal_confirm::{ReattachConfirmation, classify_reattach_confirmation};
use super::*;

pub(super) async fn apply_relay_recovery_plan(
    registry: &HealthRegistry,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    decision: RelayRecoveryDecision,
    now_ms: i64,
    source: RelayRecoveryApplySource,
) -> RelayRecoveryResponse {
    apply_relay_recovery_plan_with_seams(
        registry,
        shared,
        provider,
        decision,
        now_ms,
        source,
        &circuit_breaker::PgCircuitAlertEnqueue,
        &ImmediateApplyBoundary,
    )
    .await
}

#[async_trait::async_trait]
pub(super) trait ReservedEpisodeApplyBoundary: Sync {
    async fn after_reserve(&self, episode: &circuit_breaker::RelayReattachEpisode);
}

struct ImmediateApplyBoundary;

#[async_trait::async_trait]
impl ReservedEpisodeApplyBoundary for ImmediateApplyBoundary {
    async fn after_reserve(&self, _episode: &circuit_breaker::RelayReattachEpisode) {}
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_relay_recovery_plan_with_seams(
    registry: &HealthRegistry,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    mut decision: RelayRecoveryDecision,
    now_ms: i64,
    source: RelayRecoveryApplySource,
    alert_enqueue: &dyn circuit_breaker::CircuitAlertEnqueue,
    apply_boundary: &dyn ReservedEpisodeApplyBoundary,
) -> RelayRecoveryResponse {
    if !decision.auto_heal.eligible {
        trace_relay_recovery_skipped(&decision, decision.auto_heal.skipped_reason);
        return RelayRecoveryResponse {
            ok: false,
            mode: "apply",
            applied: false,
            skipped: true,
            decision,
            apply_result: None,
        };
    }

    let key = auto_heal_key(
        &decision.provider,
        decision.channel_id,
        decision.action,
        source,
    );
    decision.auto_heal.remaining_attempts =
        remaining_auto_heal_attempts(&key, now_ms, decision.auto_heal.max_attempts_per_window);
    let attempt_generation =
        match reserve_auto_heal_attempt(&key, now_ms, decision.auto_heal.max_attempts_per_window) {
            Ok((remaining, generation)) => {
                decision.auto_heal.remaining_attempts = remaining;
                generation
            }
            Err(reason) => {
                decision.auto_heal.remaining_attempts = 0;
                decision.auto_heal.skipped_reason = Some(reason);
                trace_relay_recovery_skipped(&decision, Some(reason));
                return RelayRecoveryResponse {
                    ok: false,
                    mode: "apply",
                    applied: false,
                    skipped: true,
                    decision,
                    apply_result: None,
                };
            }
        };

    let mut reserved_episode = None;
    if circuit_breaker::should_use_durable_circuit(decision.action, source) {
        reserved_episode = match circuit_breaker::inspect_current_episode(provider, &decision) {
            Ok(episode) => Some(episode),
            Err(reservation) => {
                return skipped_durable_reservation_response(
                    shared,
                    provider,
                    decision,
                    &key,
                    attempt_generation,
                    reservation,
                    alert_enqueue,
                )
                .await;
            }
        };
    }

    if let Some(episode) = reserved_episode.as_mut() {
        match circuit_breaker::reserve_inspected_episode(
            provider,
            &decision,
            episode,
            decision.auto_heal.max_attempts_per_window,
        ) {
            circuit_breaker::CircuitReservation::Reserved {
                attempt,
                episode: reserved,
                orphaned_staged_alert_ids,
            } => {
                *episode = reserved;
                for staged_alert_id in orphaned_staged_alert_ids {
                    match alert_enqueue
                        .cancel(shared.pg_pool.as_ref(), staged_alert_id)
                        .await
                    {
                        Ok(()) => circuit_breaker::acknowledge_orphaned_staged_alert_cleanup(
                            provider,
                            decision.channel_id,
                            staged_alert_id,
                        ),
                        Err(error) => tracing::warn!(
                            target: "agentdesk::discord::relay_recovery",
                            provider = provider.as_str(),
                            channel_id = decision.channel_id,
                            staged_alert_id,
                            error = %error,
                            "relay reattach circuit orphaned held alert cleanup will retry"
                        ),
                    }
                }
                tracing::info!(
                    target: "agentdesk::discord::relay_recovery",
                    provider = provider.as_str(),
                    channel_id = decision.channel_id,
                    attempt,
                    max_attempts = decision.auto_heal.max_attempts_per_window,
                    episode = episode.short_key(),
                    "reserved durable relay reattach episode attempt"
                );
            }
            reservation => {
                return skipped_durable_reservation_response(
                    shared,
                    provider,
                    decision,
                    &key,
                    attempt_generation,
                    reservation,
                    alert_enqueue,
                )
                .await;
            }
        }
        apply_boundary.after_reserve(episode).await;
    }

    let mut apply_result = apply_relay_recovery_decision(
        registry,
        shared,
        provider,
        &decision,
        reserved_episode.as_ref(),
        source,
    )
    .await;
    if let Some(episode) = reserved_episode.as_ref()
        && matches!(
            apply_result.status,
            "rebind_failed" | "provider_unavailable" | "reattach_episode_changed"
        )
        && !circuit_breaker::refund_unhanded_episode_attempt(provider, decision.channel_id, episode)
    {
        tracing::warn!(
            target: "agentdesk::discord::relay_recovery",
            provider = provider.as_str(),
            channel_id = decision.channel_id,
            episode = episode.short_key(),
            "failed to refund durable relay reattach attempt before watcher handoff"
        );
    }
    let confirmation = classify_reattach_confirmation(
        shared,
        &decision,
        &apply_result,
        registry.started_at_unix(),
        chrono::Utc::now().timestamp(),
    )
    .await;
    settle_auto_heal_confirmation(
        &mut apply_result,
        confirmation,
        &key,
        attempt_generation,
        now_ms,
    );
    let skipped = apply_result.status == "reattach_episode_changed";
    if skipped {
        decision.auto_heal.skipped_reason = Some("durable_reattach_confirmation_episode_changed");
    }
    decision.auto_heal.remaining_attempts =
        remaining_auto_heal_attempts(&key, now_ms, decision.auto_heal.max_attempts_per_window);
    let applied = relay_recovery_status_counts_as_applied(apply_result.status);
    tracing::info!(
        target: "agentdesk::discord::relay_recovery",
        provider = decision.provider.as_str(),
        channel_id = decision.channel_id,
        action = decision.action.as_str(),
        source = source.as_str(),
        status = apply_result.status,
        confirmation = ?confirmation,
        remaining_attempts = decision.auto_heal.remaining_attempts,
        removed_thread_proofs = apply_result.removed_thread_proofs,
        removed_mailbox_token = apply_result.removed_mailbox_token,
        "relay recovery auto-heal attempt completed"
    );
    RelayRecoveryResponse {
        ok: applied,
        mode: "apply",
        applied,
        skipped,
        decision,
        apply_result: Some(apply_result),
    }
}

async fn skipped_durable_reservation_response(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    mut decision: RelayRecoveryDecision,
    key: &str,
    attempt_generation: u64,
    reservation: circuit_breaker::CircuitReservation,
    alert_enqueue: &dyn circuit_breaker::CircuitAlertEnqueue,
) -> RelayRecoveryResponse {
    cancel_unapplied_auto_heal_attempt_if_current(key, attempt_generation);
    decision.auto_heal.skipped_reason = Some(match reservation {
        circuit_breaker::CircuitReservation::Open {
            episode,
            open,
            alert_needed,
            staged_alert_id,
        } => {
            decision.auto_heal.remaining_attempts = 0;
            if alert_needed || staged_alert_id.is_some() {
                circuit_breaker::queue_or_resume_open_alert_with_enqueue(
                    shared,
                    provider,
                    poise::serenity_prelude::ChannelId::new(decision.channel_id),
                    &episode,
                    &open,
                    decision.auto_heal.max_attempts_per_window,
                    staged_alert_id,
                    alert_enqueue,
                )
                .await;
            }
            "durable_reattach_circuit_open"
        }
        circuit_breaker::CircuitReservation::StaleIdentity => "durable_reattach_stale_identity",
        circuit_breaker::CircuitReservation::MissingInflight => "durable_reattach_missing_inflight",
        circuit_breaker::CircuitReservation::IoError => "durable_reattach_store_unavailable",
        circuit_breaker::CircuitReservation::Reserved { .. } => {
            "durable_reattach_unexpected_reservation"
        }
    });
    trace_relay_recovery_skipped(&decision, decision.auto_heal.skipped_reason);
    RelayRecoveryResponse {
        ok: false,
        mode: "apply",
        applied: false,
        skipped: true,
        decision,
        apply_result: None,
    }
}

fn settle_auto_heal_confirmation(
    apply_result: &mut RelayRecoveryApplyResult,
    confirmation: ReattachConfirmation,
    key: &str,
    attempt_generation: u64,
    now_ms: i64,
) {
    match confirmation {
        ReattachConfirmation::StartupGrace => {
            // Startup grace is inconclusive, but rebind already handed a watcher
            // into the registry. Retrying immediately could replace that live
            // writer and duplicate its range, so the reservation remains spent.
            // Do not call `commit`: a failed probe is not a healed round and must
            // not clear the existing consecutive-failure/backoff history.
            apply_result.status = "reattach_confirm_startup_grace";
        }
        ReattachConfirmation::RelayEmissionInFlight => {
            // The exact spawned watcher still owns an active emission. Retrying
            // reattach concurrently could replace that writer and duplicate the
            // range, so the reservation remains spent. Active emission is still
            // not delivery confirmation and therefore must not clear failure
            // history through the healed-round `commit` path.
            apply_result.status = "reattach_confirm_emission_in_flight";
        }
        ReattachConfirmation::Failed => {
            apply_result.status = "reattach_confirm_failed";
            apply_result.reattach_error = Some(
                "spawned watcher did not confirm heartbeat or relay-frontier progress".to_string(),
            );
            record_auto_heal_confirm_failure_if_current(key, attempt_generation, now_ms);
        }
        ReattachConfirmation::NotRequired | ReattachConfirmation::Confirmed => {
            // #5021: `NotRequired` is "confirmation was skipped", not
            // "confirmation passed" — `classify_reattach_confirmation` returns it
            // whenever no watcher was spawned, so a no-op reuse never proves that
            // delivery recovered. Settling a round that repaired nothing as a
            // healed round would clear the consecutive-refund counter and the
            // retry window, leaving the escalating backoff unreachable forever.
            if matches!(
                apply_result.status,
                "rebind_failed" | "provider_unavailable" | "reattach_episode_changed"
            ) || relay_recovery_status_repaired_nothing(apply_result.status)
            {
                refund_auto_heal_attempt_if_current(key, attempt_generation, now_ms);
            } else {
                commit_auto_heal_attempt_if_current(key, attempt_generation);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use poise::serenity_prelude::{ChannelId, Http, MessageId, UserId};

    use super::super::auto_heal_attempts::{
        AUTO_HEAL_REFUND_BACKOFF_THRESHOLD, auto_heal_attempt_generation, auto_heal_attempt_state,
        auto_heal_key, auto_heal_test_lock, clear_auto_heal_attempts_for_tests,
        reserve_auto_heal_attempt,
    };
    use super::*;
    use crate::services::provider::CancelToken;

    struct NeverAlert;

    struct CountingAlert {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl circuit_breaker::CircuitAlertEnqueue for CountingAlert {
        async fn enqueue(
            &self,
            _pool: Option<&sqlx::PgPool>,
            _request: &circuit_breaker::CircuitAlertRequest,
        ) -> Result<i64, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(5023)
        }

        async fn activate(&self, _pool: Option<&sqlx::PgPool>, id: i64) -> Result<bool, String> {
            assert_eq!(id, 5023);
            Ok(true)
        }

        async fn cancel(&self, _pool: Option<&sqlx::PgPool>, _id: i64) -> Result<(), String> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl circuit_breaker::CircuitAlertEnqueue for NeverAlert {
        async fn enqueue(
            &self,
            _pool: Option<&sqlx::PgPool>,
            _request: &circuit_breaker::CircuitAlertRequest,
        ) -> Result<i64, String> {
            panic!("reserved-episode race must not reach the circuit-open alert path")
        }

        async fn activate(&self, _pool: Option<&sqlx::PgPool>, _id: i64) -> Result<bool, String> {
            panic!("reserved-episode race must not activate an alert")
        }

        async fn cancel(&self, _pool: Option<&sqlx::PgPool>, _id: i64) -> Result<(), String> {
            panic!("reserved-episode race must not cancel an alert")
        }
    }

    struct BarrierApplyBoundary {
        reserved: Arc<tokio::sync::Barrier>,
        resume: Arc<tokio::sync::Barrier>,
    }

    #[async_trait::async_trait]
    impl ReservedEpisodeApplyBoundary for BarrierApplyBoundary {
        async fn after_reserve(&self, _episode: &circuit_breaker::RelayReattachEpisode) {
            self.reserved.wait().await;
            self.resume.wait().await;
        }
    }

    async fn start_turn(
        shared: &Arc<SharedData>,
        channel: ChannelId,
        message: u64,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        assert!(
            super::super::super::mailbox_try_start_turn(
                shared,
                channel,
                token.clone(),
                UserId::new(1),
                MessageId::new(message),
            )
            .await
        );
        token
    }

    #[tokio::test]
    async fn relay_recovery_emission_in_flight_consumes_budget_until_frontier_progress() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = auto_heal_key(
            "codex",
            4_423_302,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::ProbeAutoHeal,
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000, 1).map(|(remaining, _)| remaining),
            Ok(0)
        );
        let mut apply_result = RelayRecoveryApplyResult {
            status: "reattached_watcher",
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            reattach_watcher_spawned: Some(true),
            reattach_watcher_replaced: Some(false),
            reattach_initial_offset: Some(0),
            reattach_error: None,
        };

        settle_auto_heal_confirmation(
            &mut apply_result,
            ReattachConfirmation::RelayEmissionInFlight,
            &key,
            auto_heal_attempt_generation(&key).expect("reservation generation"),
            2_000,
        );

        assert_eq!(apply_result.status, "reattach_confirm_emission_in_flight");
        assert!(apply_result.reattach_error.is_none());
        assert!(relay_recovery_status_counts_as_applied(apply_result.status));
        assert_eq!(
            reserve_auto_heal_attempt(&key, 3_000, 1).map(|(remaining, _)| remaining),
            Err("auto_heal_rate_limited"),
            "in-flight relay must not refund an automatic reattach reservation before confirmed frontier progress"
        );
    }

    /// A window budget deliberately larger than `AUTO_HEAL_REFUND_BACKOFF_THRESHOLD`
    /// so the plain per-window rate limiter cannot mask the escalating backoff
    /// this test is about.
    const NOOP_REUSE_WINDOW_BUDGET: u32 = 8;

    fn noop_reuse_apply_result() -> RelayRecoveryApplyResult {
        RelayRecoveryApplyResult {
            status: "reuse_existing_live_watcher",
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            // The rebind reported no authoritative state mutation.
            reattach_watcher_spawned: Some(false),
            reattach_watcher_replaced: Some(false),
            reattach_initial_offset: None,
            reattach_error: None,
        }
    }

    /// #5021 regression: `reuse_existing_live_watcher` repairs nothing, so it must
    /// not settle as a healed round.
    ///
    /// If such a round commits, `commit_auto_heal_attempt` clears
    /// `consecutive_refunds` and `retry_not_before_ms` on every cycle, so the
    /// escalating failure backoff can never engage. A stall classification that
    /// keeps re-firing (for example a false-positive `tmux_alive_relay_dead`) then
    /// drives an unbounded reattach/redrive loop that never converges, never
    /// escalates, and never surfaces as a failure — observed live on channel
    /// 1479671298497183835 as 340 recovery decisions with zero convergence.
    #[tokio::test]
    async fn repeated_noop_reuse_rounds_escalate_into_failure_backoff() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = auto_heal_key(
            "claude",
            1_479_671_298_497_183_835,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::StallWatchdog,
        );

        let mut now_ms = 1_000_i64;
        for _ in 0..AUTO_HEAL_REFUND_BACKOFF_THRESHOLD {
            let _ = reserve_auto_heal_attempt(&key, now_ms, NOOP_REUSE_WINDOW_BUDGET)
                .map(|(remaining, _)| remaining);
            let mut apply_result = noop_reuse_apply_result();
            settle_auto_heal_confirmation(
                &mut apply_result,
                ReattachConfirmation::NotRequired,
                &key,
                auto_heal_attempt_generation(&key).expect("reservation generation"),
                now_ms,
            );
            assert_eq!(apply_result.status, "reuse_existing_live_watcher");
            now_ms += 1_000;
        }

        assert_eq!(
            remaining_auto_heal_attempts(&key, now_ms, NOOP_REUSE_WINDOW_BUDGET),
            0,
            "consecutive rounds that repaired nothing must open the escalating retry window"
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, now_ms, NOOP_REUSE_WINDOW_BUDGET)
                .map(|(remaining, _)| remaining),
            Err("auto_heal_failure_backoff"),
            "a reattach that repaired nothing must not keep resetting the failure backoff; \
             otherwise a re-firing stall classification loops reattach/redrive forever"
        );
    }

    /// Guards the opposite over-correction: a genuinely spawned and confirmed
    /// reattach really did heal, so it must still commit and leave the retry
    /// window closed.
    #[tokio::test]
    async fn confirmed_spawned_reattach_still_commits_without_opening_backoff() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = auto_heal_key(
            "claude",
            1_479_671_298_497_183_836,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::StallWatchdog,
        );

        let mut now_ms = 1_000_i64;
        for _ in 0..AUTO_HEAL_REFUND_BACKOFF_THRESHOLD {
            let _ = reserve_auto_heal_attempt(&key, now_ms, NOOP_REUSE_WINDOW_BUDGET)
                .map(|(remaining, _)| remaining);
            let mut apply_result = noop_reuse_apply_result();
            apply_result.status = "reattached_watcher";
            apply_result.reattach_watcher_spawned = Some(true);
            settle_auto_heal_confirmation(
                &mut apply_result,
                ReattachConfirmation::Confirmed,
                &key,
                auto_heal_attempt_generation(&key).expect("reservation generation"),
                now_ms,
            );
            now_ms += 1_000;
        }

        assert!(
            reserve_auto_heal_attempt(&key, now_ms, NOOP_REUSE_WINDOW_BUDGET).is_ok(),
            "a confirmed reattach genuinely healed the relay, so it must not be \
             penalised with the failure backoff"
        );
    }

    /// Production-wiring regression: this crosses the real apply adapter and
    /// settlement call. The apply fails before mutation, so each round must refund
    /// through the exact key reserved by the orchestration layer.
    #[tokio::test]
    async fn production_apply_failure_rounds_open_failure_backoff() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let provider = ProviderKind::Codex;
        let registry = HealthRegistry::new();
        let shared = super::super::super::make_shared_data_for_tests();
        let channel_id = 5_021_001;
        let mut decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                provider: provider.as_str().to_string(),
                channel_id,
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: Some("AgentDesk-codex-5021-missing-runtime".to_string()),
                tmux_alive: Some(true),
                watcher_attached: false,
                watcher_attached_stale: false,
                watcher_owner_channel_id: None,
                watcher_owns_live_relay: false,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(5_021_201),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(5_021_101),
                mailbox_turn_started_at_ms: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: None,
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: None,
                last_outbound_activity_ms: None,
                last_capture_offset: Some(128),
                last_relay_offset: 0,
                unread_bytes: Some(128),
                desynced: true,
                stale_thread_proof: false,
            },
            RelayStallState::TmuxAliveRelayDead,
            1_000,
        );
        decision.auto_heal.max_attempts_per_window = NOOP_REUSE_WINDOW_BUDGET;
        let key = auto_heal_key(
            provider.as_str(),
            channel_id,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::Manual,
        );

        let mut now_ms = 1_000_i64;
        for _ in 0..AUTO_HEAL_REFUND_BACKOFF_THRESHOLD {
            let response = apply_relay_recovery_plan_with_seams(
                &registry,
                &shared,
                &provider,
                decision.clone(),
                now_ms,
                RelayRecoveryApplySource::Manual,
                &NeverAlert,
                &ImmediateApplyBoundary,
            )
            .await;
            assert_eq!(
                response.apply_result.as_ref().map(|result| result.status),
                Some("provider_unavailable")
            );
            now_ms += 1_000;
        }

        assert_eq!(
            remaining_auto_heal_attempts(&key, now_ms, NOOP_REUSE_WINDOW_BUDGET),
            0,
            "production orchestration must settle the exact reserved key"
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, now_ms, NOOP_REUSE_WINDOW_BUDGET)
                .map(|(remaining, _)| remaining),
            Err("auto_heal_failure_backoff")
        );
    }

    #[tokio::test]
    async fn production_internal_pre_handoff_failure_refunds_durable_attempt() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let root = tempfile::tempdir().expect("isolated AgentDesk root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Codex;
        let registry = HealthRegistry::new();
        let shared = super::super::super::make_shared_data_for_tests();
        let channel_id = 5_023_201;
        let tmux = "AgentDesk-codex-5023-pre-handoff";
        let mut state = super::super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            None,
            7,
            channel_id + 1,
            channel_id + 2,
            "pre-handoff failure".to_string(),
            Some("session-5023".to_string()),
            Some(tmux.to_string()),
            Some("/tmp/missing-5023.jsonl".to_string()),
            None,
            0,
        );
        state.finalizer_turn_id = state.user_msg_id;
        state.turn_nonce = Some("nonce-5023".to_string());
        super::super::super::inflight::save_inflight_state(&state).expect("seed inflight");
        let mut decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                provider: provider.as_str().to_string(),
                channel_id,
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: Some(tmux.to_string()),
                tmux_alive: Some(true),
                watcher_attached: false,
                watcher_attached_stale: false,
                watcher_owner_channel_id: None,
                watcher_owns_live_relay: false,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(state.current_msg_id),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(state.user_msg_id),
                mailbox_turn_started_at_ms: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: None,
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: None,
                last_outbound_activity_ms: None,
                last_capture_offset: Some(128),
                last_relay_offset: 0,
                unread_bytes: Some(128),
                desynced: true,
                stale_thread_proof: false,
            },
            RelayStallState::TmuxAliveRelayDead,
            1_000,
        );
        decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());
        decision.auto_heal.eligible = true;
        decision.auto_heal.skipped_reason = None;
        decision.auto_heal.max_attempts_per_window = 2;

        assert!(decision.auto_heal.eligible);
        let first = apply_relay_recovery_plan_with_seams(
            &registry,
            &shared,
            &provider,
            decision.clone(),
            1_000,
            RelayRecoveryApplySource::ProbeAutoHeal,
            &NeverAlert,
            &ImmediateApplyBoundary,
        )
        .await;
        assert_eq!(
            first.apply_result.as_ref().map(|result| result.status),
            Some("provider_unavailable"),
            "unexpected response: {first:?}"
        );
        let expected = circuit_breaker::inspect_current_episode(&provider, &decision)
            .expect("inspect refunded episode");
        assert!(matches!(
            circuit_breaker::reserve_inspected_episode(&provider, &decision, &expected, 2),
            circuit_breaker::CircuitReservation::Reserved { attempt: 1, .. }
        ));
    }

    #[tokio::test]
    async fn relay_recovery_startup_grace_spends_budget_without_committing_healed_round() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = auto_heal_key(
            "codex",
            4_423_303,
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::ProbeAutoHeal,
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000, 1).map(|(remaining, _)| remaining),
            Ok(0)
        );
        let mut apply_result = RelayRecoveryApplyResult {
            status: "reattached_watcher",
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            reattach_watcher_spawned: Some(true),
            reattach_watcher_replaced: Some(false),
            reattach_initial_offset: Some(0),
            reattach_error: None,
        };

        settle_auto_heal_confirmation(
            &mut apply_result,
            ReattachConfirmation::StartupGrace,
            &key,
            auto_heal_attempt_generation(&key).expect("reservation generation"),
            2_000,
        );

        assert_eq!(apply_result.status, "reattach_confirm_startup_grace");
        assert_eq!(
            auto_heal_attempt_state(&key)
                .map(|(_, refunds, retry_at, healed)| (refunds, retry_at, healed)),
            Some((0, None, 0)),
            "an inconclusive probe must neither commit healed state nor manufacture failure history"
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 3_000, 1).map(|(remaining, _)| remaining),
            Err("auto_heal_rate_limited"),
            "an already handed-off watcher must consume the attempt even when startup grace makes its probe inconclusive"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reserved_episode_replacement_at_apply_barrier_is_untouched() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let root = tempfile::tempdir().expect("isolated AgentDesk root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Codex;
        let registry = Arc::new(HealthRegistry::new());
        let shared = super::super::super::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        registry
            .register_http(
                provider.as_str().to_string(),
                Arc::new(Http::new("Bot test-token")),
            )
            .await;
        let channel = ChannelId::new(4_465_701);
        let tmux = "AgentDesk-codex-4465-apply-cas";
        let old_output = root.path().join("relay-4465-old.jsonl");
        let replacement_output = root.path().join("relay-4465-replacement.jsonl");
        let output_fixture = r#"{"type":"thread.started","thread_id":"t"}"#;
        std::fs::write(&old_output, output_fixture).expect("old output fixture");
        std::fs::write(&replacement_output, output_fixture).expect("replacement output fixture");
        let output_len = std::fs::metadata(&old_output)
            .expect("old output metadata")
            .len();
        let mut old = super::super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            343_742_347,
            4_465_711,
            4_465_721,
            "reserved episode".to_string(),
            Some("provider-session-old".to_string()),
            Some(tmux.to_string()),
            Some(old_output.display().to_string()),
            None,
            output_len,
        );
        old.finalizer_turn_id = old.user_msg_id;
        old.turn_nonce = Some("nonce-old".to_string());
        old.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
        old.set_relay_owner_kind(super::super::super::inflight::RelayOwnerKind::Watcher);
        super::super::super::inflight::save_inflight_state(&old).expect("seed old episode");
        let token = start_turn(&shared, channel, old.user_msg_id).await;
        shared
            .restart
            .global_active
            .store(1, std::sync::atomic::Ordering::Relaxed);
        let watcher_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        shared.tmux_watchers.insert(
            channel,
            super::super::super::TmuxWatcherHandle {
                tmux_session_name: tmux.to_string(),
                output_path: old.output_path.clone().expect("old output path"),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: watcher_cancel.clone(),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(1)),
            },
        );
        shared.turn_finalizer.register_start(
            super::super::super::turn_finalizer::TurnKey::new(
                channel,
                old.effective_finalizer_turn_id(),
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::super::inflight::RelayOwnerKind::Watcher,
            &shared,
        );
        let snapshot = RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id: channel.get(),
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some(tmux.to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_attached_stale: true,
            watcher_owner_channel_id: Some(channel.get()),
            watcher_owns_live_relay: true,
            bridge_inflight_present: true,
            bridge_current_msg_id: Some(old.current_msg_id),
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(old.user_msg_id),
            mailbox_turn_started_at_ms: None,
            queue_depth: 0,
            pending_discord_callback_msg_id: None,
            pending_thread_proof: false,
            parent_channel_id: None,
            thread_channel_id: None,
            last_relay_ts_ms: None,
            last_outbound_activity_ms: None,
            last_capture_offset: Some(128),
            last_relay_offset: 0,
            unread_bytes: Some(128),
            desynced: true,
            stale_thread_proof: false,
        };
        let mut decision = plan_relay_recovery(
            &snapshot,
            RelayStallState::TmuxAliveRelayDead,
            chrono::Utc::now().timestamp_millis(),
        );
        decision.affected.finalizer_turn_id = Some(old.effective_finalizer_turn_id());
        let reserved = Arc::new(tokio::sync::Barrier::new(2));
        let resume = Arc::new(tokio::sync::Barrier::new(2));
        let boundary = Arc::new(BarrierApplyBoundary {
            reserved: reserved.clone(),
            resume: resume.clone(),
        });
        let task = {
            let registry = registry.clone();
            let shared = shared.clone();
            let provider = provider.clone();
            let boundary = boundary.clone();
            tokio::spawn(async move {
                apply_relay_recovery_plan_with_seams(
                    &registry,
                    &shared,
                    &provider,
                    decision,
                    chrono::Utc::now().timestamp_millis(),
                    RelayRecoveryApplySource::ProbeAutoHeal,
                    &NeverAlert,
                    boundary.as_ref(),
                )
                .await
            })
        };

        reserved.wait().await;
        let mut replacement = old.clone();
        replacement.session_id = Some("provider-session-replacement".to_string());
        replacement.output_path = Some(replacement_output.display().to_string());
        replacement.turn_nonce = Some("nonce-replacement".to_string());
        super::super::super::inflight::save_inflight_state(&replacement)
            .expect("install replacement at apply barrier");
        let replacement_before = serde_json::to_value(
            super::super::super::inflight::load_inflight_state_read_only(&provider, channel.get())
                .expect("replacement before apply"),
        )
        .expect("serialize replacement before");
        resume.wait().await;

        let response = task.await.expect("apply task");
        assert_eq!(
            response.apply_result.as_ref().map(|result| result.status),
            Some("reattach_episode_changed")
        );
        assert!(!response.applied);
        assert!(response.skipped);
        assert_eq!(
            response.decision.auto_heal.skipped_reason,
            Some("durable_reattach_confirmation_episode_changed")
        );
        assert_eq!(
            serde_json::to_value(
                super::super::super::inflight::load_inflight_state_read_only(
                    &provider,
                    channel.get(),
                )
                .expect("replacement survives apply"),
            )
            .expect("serialize replacement after"),
            replacement_before,
            "stale reserved episode must not clear or adopt the replacement"
        );
        let watcher = shared
            .tmux_watchers
            .get(&channel)
            .expect("replacement keeps incumbent watcher");
        assert!(Arc::ptr_eq(&watcher.cancel, &watcher_cancel));
        assert!(!watcher_cancel.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let mailbox = super::super::super::mailbox_snapshot(&shared, channel).await;
        assert!(Arc::ptr_eq(
            mailbox
                .cancel_token
                .as_ref()
                .expect("replacement token survives"),
            &token,
        ));
    }

    #[test]
    fn relay_recovery_manual_apply_succeeds_after_probe_budget_is_exhausted() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        // Match the established suite-wide lock order: auto-heal budget first,
        // shared environment second. The inverse order deadlocks parallel tests.
        let _guard = runtime.block_on(auto_heal_test_lock().lock());
        let root = tempfile::tempdir().expect("isolated AgentDesk root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        runtime.block_on(async {
            clear_auto_heal_attempts_for_tests();
            let provider = ProviderKind::Codex;
            let registry = HealthRegistry::new();
            let shared = super::super::super::make_shared_data_for_tests();
            registry
                .register(provider.as_str().to_string(), shared.clone())
                .await;
            let channel = ChannelId::new(4_423_301);

            start_turn(&shared, channel, 4_423_311).await;
            shared
                .mailboxes
                .handle(channel)
                .age_active_turn_for_test(ORPHAN_PENDING_TOKEN_ADMISSION_GRACE)
                .await;
            let first = auto_apply_relay_recovery_for_shared(
                &registry,
                shared.clone(),
                &provider,
                channel.get(),
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                RelayRecoveryApplySource::ProbeAutoHeal,
            )
            .await
            .expect("first probe apply");
            assert!(first.applied);

            start_turn(&shared, channel, 4_423_312).await;
            shared
                .mailboxes
                .handle(channel)
                .age_active_turn_for_test(ORPHAN_PENDING_TOKEN_ADMISSION_GRACE)
                .await;
            let blocked_probe = auto_apply_relay_recovery_for_shared(
                &registry,
                shared.clone(),
                &provider,
                channel.get(),
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                RelayRecoveryApplySource::ProbeAutoHeal,
            )
            .await
            .expect("exhausted probe apply");
            assert!(blocked_probe.skipped);
            assert_eq!(
                blocked_probe.decision.auto_heal.skipped_reason,
                Some("auto_heal_rate_limited")
            );

            let manual = auto_apply_relay_recovery_for_shared(
                &registry,
                shared,
                &provider,
                channel.get(),
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                RelayRecoveryApplySource::Manual,
            )
            .await
            .expect("manual apply after probe exhaustion");
            assert!(
                manual.applied,
                "internal probe budget must never lock an operator"
            );
        });
    }
}
