use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::super::SharedData;
use super::super::inflight::{
    DEAD_WATCHER_PROVEN_DEAD_SECS, InflightTurnState, delete_inflight_state_file,
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

pub(super) fn abandoned_tmux_cleanup_decision(
    has_usable_session_name: bool,
    pane: PaneLiveness,
    activity: RuntimeActivityEvidence,
) -> AbandonedTmuxCleanupDecision {
    if !has_usable_session_name {
        return AbandonedTmuxCleanupDecision::TerminalMarkerOnly;
    }
    if pane == PaneLiveness::DeadOrAbsent && activity == RuntimeActivityEvidence::Inactive {
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
        return AbandonedTmuxCleanupDecision::TerminalMarkerOnly;
    };
    let session_name = session_name.trim();
    if session_name.is_empty() {
        return AbandonedTmuxCleanupDecision::TerminalMarkerOnly;
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
pub(super) struct AbandonedTmuxCleanupOutcome {
    pub(super) decision: AbandonedTmuxCleanupDecision,
    pub(super) cleanup_killed: bool,
}

impl AbandonedTmuxCleanupOutcome {
    fn allows_state_delete(self) -> bool {
        self.decision == AbandonedTmuxCleanupDecision::TerminalMarkerOnly
            || (self.decision == AbandonedTmuxCleanupDecision::Kill && self.cleanup_killed)
    }

    pub(super) fn delete_state_if_allowed(
        self,
        provider: &ProviderKind,
        state: &InflightTurnState,
    ) -> bool {
        self.allows_state_delete() && delete_inflight_state_file(provider, state.channel_id)
    }
}

#[cfg(test)]
pub(super) fn cleanup_decision_allows_state_delete(
    decision: AbandonedTmuxCleanupDecision,
    cleanup_killed: bool,
) -> bool {
    AbandonedTmuxCleanupOutcome {
        decision,
        cleanup_killed,
    }
    .allows_state_delete()
}

/// Finalize and cancel an abandoned turn only after independent tmux and
/// runtime-activity evidence proves its owner is gone. Live, recent, and
/// indeterminate evidence preserves the row for a later retry.
pub(super) async fn finalize_abandoned_mailbox_if_proven_dead(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
) -> AbandonedTmuxCleanupOutcome {
    let decision = abandoned_tmux_cleanup_decision_for(state).await;
    if decision != AbandonedTmuxCleanupDecision::Kill {
        return AbandonedTmuxCleanupOutcome {
            decision,
            cleanup_killed: false,
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
            super::super::TmuxCleanupPolicy::CleanupSession {
                termination_reason_code: Some("placeholder_sweeper_abandon"),
            },
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
        }
    } else {
        AbandonedTmuxCleanupOutcome {
            decision,
            cleanup_killed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AbandonedTmuxCleanupDecision, RuntimeActivityEvidence, abandoned_tmux_cleanup_decision,
        abandoned_tmux_cleanup_decision_for, run_blocking_cleanup_probe,
        runtime_activity_evidence_from,
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
    fn no_death_evidence_preserves_the_tmux_session_for_retry() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                true,
                PaneLiveness::DeadOrAbsent,
                RuntimeActivityEvidence::Unknown,
            ),
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
    }

    #[test]
    fn missing_or_blank_tmux_name_is_terminal_marker_only_without_a_probe() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                false,
                PaneLiveness::DeadOrAbsent,
                RuntimeActivityEvidence::Inactive,
            ),
            AbandonedTmuxCleanupDecision::TerminalMarkerOnly,
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
    fn uncertain_or_recent_evidence_preserves_retry() {
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
