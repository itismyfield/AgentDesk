use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::super::SharedData;
use super::super::inflight::{DEAD_WATCHER_PROVEN_DEAD_SECS, InflightTurnState};
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
    NoKillMarkerOnly,
}

pub(super) fn abandoned_tmux_cleanup_decision(
    has_usable_session_name: bool,
    pane: PaneLiveness,
    activity: RuntimeActivityEvidence,
) -> AbandonedTmuxCleanupDecision {
    if has_usable_session_name
        && pane == PaneLiveness::DeadOrAbsent
        && activity == RuntimeActivityEvidence::Inactive
    {
        AbandonedTmuxCleanupDecision::Kill
    } else {
        AbandonedTmuxCleanupDecision::NoKillMarkerOnly
    }
}

fn runtime_activity_evidence(session_name: &str) -> RuntimeActivityEvidence {
    let latest_nanos =
        crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos(session_name);
    if latest_nanos <= 0 {
        return RuntimeActivityEvidence::Unknown;
    }
    let latest_secs = latest_nanos / 1_000_000_000;
    let now_secs = chrono::Utc::now().timestamp();
    let age_secs = now_secs.saturating_sub(latest_secs).max(0) as u64;
    if age_secs < DEAD_WATCHER_PROVEN_DEAD_SECS {
        RuntimeActivityEvidence::Recent
    } else {
        RuntimeActivityEvidence::Inactive
    }
}

pub(super) fn abandoned_tmux_cleanup_decision_for(
    state: &InflightTurnState,
) -> AbandonedTmuxCleanupDecision {
    if state.user_msg_id == 0 {
        return AbandonedTmuxCleanupDecision::NoKillMarkerOnly;
    }
    let Some(session_name) = state.tmux_session_name.as_deref() else {
        return AbandonedTmuxCleanupDecision::NoKillMarkerOnly;
    };
    let session_name = session_name.trim();
    if session_name.is_empty() {
        return AbandonedTmuxCleanupDecision::NoKillMarkerOnly;
    }
    abandoned_tmux_cleanup_decision(
        true,
        crate::services::tmux_diagnostics::tmux_session_pane_liveness(session_name),
        runtime_activity_evidence(session_name),
    )
}

/// Finalize and cancel an abandoned turn only after independent tmux and
/// runtime-activity evidence proves its owner is gone. Every other outcome is
/// marker-only so a live or indeterminate session remains untouched.
pub(super) async fn finalize_abandoned_mailbox_if_proven_dead(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
) -> bool {
    if abandoned_tmux_cleanup_decision_for(state) != AbandonedTmuxCleanupDecision::Kill {
        return false;
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
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AbandonedTmuxCleanupDecision, RuntimeActivityEvidence, abandoned_tmux_cleanup_decision,
        abandoned_tmux_cleanup_decision_for,
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
    fn no_death_evidence_keeps_the_tmux_session() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                true,
                PaneLiveness::DeadOrAbsent,
                RuntimeActivityEvidence::Unknown,
            ),
            AbandonedTmuxCleanupDecision::NoKillMarkerOnly,
        );
    }

    #[test]
    fn missing_or_blank_tmux_name_is_marker_only_without_a_probe() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                false,
                PaneLiveness::DeadOrAbsent,
                RuntimeActivityEvidence::Inactive,
            ),
            AbandonedTmuxCleanupDecision::NoKillMarkerOnly,
        );
    }

    #[test]
    fn panel_only_state_is_marker_only() {
        let mut state = sweep_state();
        state.user_msg_id = 0;

        assert_eq!(
            abandoned_tmux_cleanup_decision_for(&state),
            AbandonedTmuxCleanupDecision::NoKillMarkerOnly,
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
    fn live_pane_keeps_the_tmux_session_even_when_activity_is_not_recent() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                true,
                PaneLiveness::Live,
                RuntimeActivityEvidence::Inactive,
            ),
            AbandonedTmuxCleanupDecision::NoKillMarkerOnly,
        );
    }

    #[test]
    fn ambiguous_pane_and_activity_evidence_is_marker_only() {
        for (pane, activity) in [
            (PaneLiveness::ProbeError, RuntimeActivityEvidence::Inactive),
            (PaneLiveness::DeadOrAbsent, RuntimeActivityEvidence::Recent),
            (PaneLiveness::Live, RuntimeActivityEvidence::Unknown),
        ] {
            assert_eq!(
                abandoned_tmux_cleanup_decision(true, pane, activity),
                AbandonedTmuxCleanupDecision::NoKillMarkerOnly,
            );
        }
    }
}
