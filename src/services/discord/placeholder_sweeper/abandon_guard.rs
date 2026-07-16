use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::super::SharedData;
use super::super::inflight::{
    DEAD_WATCHER_PROVEN_DEAD_SECS, GuardedClearOutcome, InflightTurnIdentity, InflightTurnState,
    clear_inflight_state_if_matches_identity_turn_nonce,
};
use crate::services::platform::tmux::PaneLiveness;
use crate::services::provider::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeActivityEvidence {
    Recent,
    Inactive,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AbandonedTmuxCleanupDecision {
    Kill,
    PreserveRetry,
    TerminalMarkerOnly,
}

impl AbandonedTmuxCleanupDecision {
    pub(super) fn allows_discord_cleanup(self) -> bool {
        self != Self::PreserveRetry
    }
}

pub(super) fn abandoned_tmux_cleanup_decision(
    has_usable_session_name: bool,
    pane: PaneLiveness,
    activity: RuntimeActivityEvidence,
) -> AbandonedTmuxCleanupDecision {
    if !has_usable_session_name {
        return AbandonedTmuxCleanupDecision::PreserveRetry;
    }
    if pane == PaneLiveness::DeadOrAbsent
        && matches!(
            activity,
            RuntimeActivityEvidence::Inactive | RuntimeActivityEvidence::Unknown
        )
    {
        AbandonedTmuxCleanupDecision::Kill
    } else {
        AbandonedTmuxCleanupDecision::PreserveRetry
    }
}

fn runtime_activity_evidence_from(latest_nanos: i64, now_secs: i64) -> RuntimeActivityEvidence {
    if latest_nanos <= 0 {
        return RuntimeActivityEvidence::Unknown;
    }
    let latest_secs = latest_nanos / 1_000_000_000;
    let age_secs = now_secs.saturating_sub(latest_secs).max(0) as u64;
    if age_secs <= DEAD_WATCHER_PROVEN_DEAD_SECS {
        RuntimeActivityEvidence::Recent
    } else {
        RuntimeActivityEvidence::Inactive
    }
}

fn runtime_activity_evidence(session_name: &str) -> RuntimeActivityEvidence {
    let latest_nanos =
        crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos(session_name);
    runtime_activity_evidence_from(latest_nanos, chrono::Utc::now().timestamp())
}

async fn run_blocking_cleanup_probe<F>(probe: F) -> AbandonedTmuxCleanupDecision
where
    F: FnOnce() -> AbandonedTmuxCleanupDecision + Send + 'static,
{
    match tokio::task::spawn_blocking(probe).await {
        Ok(decision) => decision,
        Err(err) => {
            tracing::warn!(
                "[placeholder_sweeper] abandoned tmux evidence probe failed to join; preserving state for retry: {err}"
            );
            AbandonedTmuxCleanupDecision::PreserveRetry
        }
    }
}

pub(super) async fn abandoned_tmux_cleanup_decision_for(
    state: &InflightTurnState,
) -> AbandonedTmuxCleanupDecision {
    if state.user_msg_id == 0 {
        return AbandonedTmuxCleanupDecision::TerminalMarkerOnly;
    }
    let Some(session_name) = state.tmux_session_name.as_deref() else {
        return AbandonedTmuxCleanupDecision::PreserveRetry;
    };
    let session_name = session_name.trim();
    if session_name.is_empty() {
        return AbandonedTmuxCleanupDecision::PreserveRetry;
    }
    let session_name = session_name.to_string();
    run_blocking_cleanup_probe(move || {
        abandoned_tmux_cleanup_decision(
            true,
            crate::services::tmux_diagnostics::tmux_session_pane_liveness(&session_name),
            runtime_activity_evidence(&session_name),
        )
    })
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AbandonedCleanupEvidence {
    OwnerDeath,
    TerminalDelivered,
}

impl AbandonedCleanupEvidence {
    fn terminal_delivered(self) -> bool {
        self == Self::TerminalDelivered
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AbandonedTmuxCleanupOutcome {
    pub(super) decision: AbandonedTmuxCleanupDecision,
    cleanup_killed: bool,
    discord_cleanup_committed: bool,
}

impl AbandonedTmuxCleanupOutcome {
    fn allows_state_delete(self) -> bool {
        self.discord_cleanup_committed
            || self.decision == AbandonedTmuxCleanupDecision::TerminalMarkerOnly
            || (self.decision == AbandonedTmuxCleanupDecision::Kill && self.cleanup_killed)
    }

    pub(super) fn delete_state_if_allowed(
        self,
        provider: &ProviderKind,
        state: &InflightTurnState,
    ) -> bool {
        self.allows_state_delete()
            && clear_inflight_state_if_matches_identity_turn_nonce(
                provider,
                state.channel_id,
                &InflightTurnIdentity::from_state(state),
                state.turn_nonce.as_deref(),
            ) == GuardedClearOutcome::Cleared
    }
}

fn cleanup_policy_for(evidence: AbandonedCleanupEvidence) -> super::super::TmuxCleanupPolicy {
    if evidence.terminal_delivered() {
        super::super::TmuxCleanupPolicy::PreserveSession
    } else {
        super::super::TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some("placeholder_sweeper_abandon"),
        }
    }
}

fn should_finish_mailbox(
    state: &InflightTurnState,
    decision: AbandonedTmuxCleanupDecision,
) -> bool {
    state.user_msg_id != 0 && decision == AbandonedTmuxCleanupDecision::Kill
}

#[cfg(test)]
fn outcome_from_evidence_for_test(
    state: &InflightTurnState,
    evidence: AbandonedCleanupEvidence,
    owner_decision: AbandonedTmuxCleanupDecision,
    discord_cleanup_committed: bool,
    cleanup_killed: bool,
) -> (AbandonedTmuxCleanupOutcome, super::super::TmuxCleanupPolicy, bool) {
    let decision = if evidence.terminal_delivered() {
        AbandonedTmuxCleanupDecision::Kill
    } else {
        owner_decision
    };
    (
        AbandonedTmuxCleanupOutcome {
            decision,
            cleanup_killed,
            discord_cleanup_committed,
        },
        cleanup_policy_for(evidence),
        should_finish_mailbox(state, decision),
    )
}

#[cfg(test)]
pub(super) fn cleanup_decision_allows_state_delete(
    decision: AbandonedTmuxCleanupDecision,
    cleanup_killed: bool,
    discord_cleanup_committed: bool,
) -> bool {
    AbandonedTmuxCleanupOutcome {
        decision,
        cleanup_killed,
        discord_cleanup_committed,
    }
    .allows_state_delete()
}

/// Finalize an abandoned mailbox from one explicit evidence source. Terminal
/// delivery may skip owner probing and preserves the reusable tmux session;
/// owner-death cleanup re-probes and keeps the destructive cleanup policy.
async fn finalize_abandoned_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    evidence: AbandonedCleanupEvidence,
    discord_cleanup_committed: bool,
) -> AbandonedTmuxCleanupOutcome {
    let terminal_delivered = evidence.terminal_delivered();
    let decision = if terminal_delivered {
        AbandonedTmuxCleanupDecision::Kill
    } else {
        abandoned_tmux_cleanup_decision_for(state).await
    };
    if !should_finish_mailbox(state, decision) {
        return AbandonedTmuxCleanupOutcome {
            decision,
            cleanup_killed: false,
            discord_cleanup_committed,
        };
    }

    let channel = serenity::ChannelId::new(state.channel_id);
    let finish = super::super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        channel,
        serenity::MessageId::new(state.user_msg_id),
    )
    .await;
    if let Some(removed_token) = finish.removed_token {
        super::super::turn_bridge::cancel_active_token(
            &removed_token,
            cleanup_policy_for(evidence),
            "placeholder_sweeper abandoned",
        );
        super::super::saturating_decrement_global_active(shared);
        if finish.has_pending {
            super::super::schedule_deferred_idle_queue_kickoff(
                shared.clone(),
                provider.clone(),
                channel,
                "placeholder_sweeper_abandon",
            );
        }
        AbandonedTmuxCleanupOutcome {
            decision,
            cleanup_killed: true,
            discord_cleanup_committed,
        }
    } else {
        AbandonedTmuxCleanupOutcome {
            decision,
            cleanup_killed: false,
            discord_cleanup_committed,
        }
    }
}

pub(super) async fn finalize_terminal_delivered_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
) -> AbandonedTmuxCleanupOutcome {
    finalize_abandoned_mailbox(
        shared,
        provider,
        state,
        AbandonedCleanupEvidence::TerminalDelivered,
        true,
    )
    .await
}

pub(super) async fn finalize_owner_dead_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    discord_cleanup_committed: bool,
) -> AbandonedTmuxCleanupOutcome {
    finalize_abandoned_mailbox(
        shared,
        provider,
        state,
        AbandonedCleanupEvidence::OwnerDeath,
        discord_cleanup_committed,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        AbandonedCleanupEvidence, AbandonedTmuxCleanupDecision, RuntimeActivityEvidence,
        abandoned_tmux_cleanup_decision, abandoned_tmux_cleanup_decision_for,
        cleanup_decision_allows_state_delete, cleanup_policy_for, outcome_from_evidence_for_test,
        run_blocking_cleanup_probe, runtime_activity_evidence_from, should_finish_mailbox,
    };
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::platform::tmux::PaneLiveness;
    use crate::services::provider::ProviderKind;

    fn sweep_state() -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            4242,
            None,
            7,
            9101,
            9102,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk".to_string()),
            Some("/tmp/recovery.jsonl".to_string()),
            None,
            0,
        )
    }

    #[test]
    fn dead_pane_without_runtime_files_converges_to_cleanup() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                true,
                PaneLiveness::DeadOrAbsent,
                RuntimeActivityEvidence::Unknown,
            ),
            AbandonedTmuxCleanupDecision::Kill,
        );
    }

    #[test]
    fn missing_tmux_name_preserves_mailbox_state() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                false,
                PaneLiveness::DeadOrAbsent,
                RuntimeActivityEvidence::Inactive,
            ),
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
    }

    #[tokio::test]
    async fn panel_only_state_is_terminal_marker_only() {
        let mut state = sweep_state();
        state.user_msg_id = 0;

        assert_eq!(
            abandoned_tmux_cleanup_decision_for(&state).await,
            AbandonedTmuxCleanupDecision::TerminalMarkerOnly,
        );
    }

    #[tokio::test]
    async fn real_turn_without_a_tmux_name_preserves_mailbox_state() {
        let mut state = sweep_state();
        state.tmux_session_name = None;

        assert_eq!(
            abandoned_tmux_cleanup_decision_for(&state).await,
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
    }

    #[test]
    fn confirmed_dead_pane_with_confirmed_inactivity_allows_cleanup() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                true,
                PaneLiveness::DeadOrAbsent,
                RuntimeActivityEvidence::Inactive,
            ),
            AbandonedTmuxCleanupDecision::Kill,
        );
    }

    #[test]
    fn live_pane_preserves_the_tmux_session_even_when_activity_is_not_recent() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                true,
                PaneLiveness::Live,
                RuntimeActivityEvidence::Inactive,
            ),
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
    }

    #[test]
    fn uncertain_or_live_evidence_preserves_retry() {
        for (pane, activity) in [
            (PaneLiveness::ProbeError, RuntimeActivityEvidence::Inactive),
            (PaneLiveness::DeadOrAbsent, RuntimeActivityEvidence::Recent),
            (PaneLiveness::Live, RuntimeActivityEvidence::Unknown),
        ] {
            assert_eq!(
                abandoned_tmux_cleanup_decision(true, pane, activity),
                AbandonedTmuxCleanupDecision::PreserveRetry,
            );
        }
    }

    #[test]
    fn evidence_wires_finalize_policy_probe_and_state_convergence() {
        let state = sweep_state();
        let (delivered, delivered_policy, delivered_finish) = outcome_from_evidence_for_test(
            &state,
            AbandonedCleanupEvidence::TerminalDelivered,
            AbandonedTmuxCleanupDecision::PreserveRetry,
            true,
            false,
        );
        assert_eq!(delivered.decision, AbandonedTmuxCleanupDecision::Kill);
        assert_eq!(
            delivered_policy,
            crate::services::discord::TmuxCleanupPolicy::PreserveSession,
        );
        assert!(delivered_finish);
        assert!(delivered.allows_state_delete());

        let (revived, revived_policy, revived_finish) = outcome_from_evidence_for_test(
            &state,
            AbandonedCleanupEvidence::OwnerDeath,
            AbandonedTmuxCleanupDecision::PreserveRetry,
            true,
            false,
        );
        assert_eq!(revived.decision, AbandonedTmuxCleanupDecision::PreserveRetry);
        assert!(matches!(
            revived_policy,
            crate::services::discord::TmuxCleanupPolicy::CleanupSession { .. }
        ));
        assert!(!revived_finish);

        let (dead, dead_policy, dead_finish) = outcome_from_evidence_for_test(
            &state,
            AbandonedCleanupEvidence::OwnerDeath,
            AbandonedTmuxCleanupDecision::Kill,
            true,
            false,
        );
        assert_eq!(dead.decision, AbandonedTmuxCleanupDecision::Kill);
        assert!(matches!(
            dead_policy,
            crate::services::discord::TmuxCleanupPolicy::CleanupSession { .. }
        ));
        assert!(dead_finish);
        assert!(dead.allows_state_delete());
    }

    #[test]
    fn terminal_delivery_never_constructs_a_zero_message_id() {
        let mut state = sweep_state();
        state.user_msg_id = 0;
        assert!(!should_finish_mailbox(
            &state,
            AbandonedTmuxCleanupDecision::Kill,
        ));
    }

    #[test]
    fn terminal_delivery_allows_state_delete_without_a_mailbox_token() {
        assert!(cleanup_decision_allows_state_delete(
            AbandonedTmuxCleanupDecision::Kill,
            false,
            true,
        ));
    }


    #[test]
    fn runtime_activity_zero_and_negative_are_unknown() {
        assert_eq!(
            runtime_activity_evidence_from(0, 10_000),
            RuntimeActivityEvidence::Unknown,
        );
        assert_eq!(
            runtime_activity_evidence_from(-1, 10_000),
            RuntimeActivityEvidence::Unknown,
        );
    }

    #[test]
    fn runtime_activity_exact_boundary_is_recent_and_next_second_is_inactive() {
        let now_secs = 10_000;
        let boundary_secs = now_secs - super::DEAD_WATCHER_PROVEN_DEAD_SECS as i64;
        assert_eq!(
            runtime_activity_evidence_from(boundary_secs * 1_000_000_000, now_secs),
            RuntimeActivityEvidence::Recent,
        );
        assert_eq!(
            runtime_activity_evidence_from((boundary_secs - 1) * 1_000_000_000, now_secs),
            RuntimeActivityEvidence::Inactive,
        );
    }

    #[tokio::test]
    async fn blocking_probe_join_failure_preserves_retry() {
        let decision = run_blocking_cleanup_probe(|| panic!("synthetic probe panic")).await;
        assert_eq!(decision, AbandonedTmuxCleanupDecision::PreserveRetry);
    }
}
