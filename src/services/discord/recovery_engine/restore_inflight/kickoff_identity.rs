use super::*;
use crate::services::platform::tmux::PaneLiveness;
use std::num::NonZeroU64;

pub(super) struct RecoveryKickoffIdentity {
    pub(super) request_owner: UserId,
    pub(super) user_message_id: Option<MessageId>,
}

/// `None` when the row has no Discord request owner (watcher-reacquired rows).
pub(super) fn recovery_kickoff_identity(
    state: &inflight::InflightTurnState,
) -> Option<RecoveryKickoffIdentity> {
    Some(RecoveryKickoffIdentity {
        request_owner: UserId::new(NonZeroU64::new(state.request_owner_user_id)?.get()),
        user_message_id: optional_message_id(state.user_msg_id),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct OwnerlessDeadPanePlan {
    pub(super) stop_source: &'static str,
    pub(super) branch: &'static str,
    pub(super) tmux_alive: bool,
    pub(super) best_response: String,
    pub(super) notice_text: String,
}

/// Plans the interrupted notice for an ownerless row whose pane is not live;
/// `None` for a live pane, which keeps its row for the watcher.
pub(super) fn plan_ownerless_dead_pane_row(
    state: &inflight::InflightTurnState,
    liveness: PaneLiveness,
    output_path: &str,
) -> Option<OwnerlessDeadPanePlan> {
    let tmux_alive = match liveness {
        PaneLiveness::Live => return None,
        PaneLiveness::DeadOrAbsent => false,
        // A failed probe is not proof of death, so budget exhaustion must not clear the row.
        PaneLiveness::ProbeError => true,
    };
    // Read from this turn's start so a reacquired row does not replay earlier turns.
    let extracted = extract_response_from_output(output_path, state.turn_start_offset.unwrap_or(0));
    let best_response = if extracted.trim().is_empty() {
        state.full_response.clone()
    } else {
        extracted
    };
    Some(OwnerlessDeadPanePlan {
        stop_source: "recovery_ownerless_dead_pane",
        branch: "ownerless_dead_pane",
        tmux_alive,
        notice_text: interrupted_recovery_message(state, &best_response),
        best_response,
    })
}

/// Ownerless rows get no mailbox turn: notify and dispose a dead pane, keep a live one.
pub(super) async fn dispose_ownerless_row(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    tmux_session_name: &str,
    output_path: &str,
) {
    let liveness =
        crate::services::tmux_diagnostics::probe_tmux_session_pane_liveness(tmux_session_name)
            .await;
    let Some(plan) = plan_ownerless_dead_pane_row(state, liveness, output_path) else {
        return;
    };
    let outcome =
        relay_recovery_terminal_notice(http, shared, provider, state, &plan.notice_text).await;
    apply_ownerless_dead_pane_outcome(shared, provider, state, &plan, outcome).await;
}

pub(super) async fn apply_ownerless_dead_pane_outcome(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    plan: &OwnerlessDeadPanePlan,
    outcome: RecoveryRelayOutcome,
) {
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = state.channel_id,
        tmux_alive = plan.tmux_alive,
        ?outcome,
        "recovery: ownerless inflight row has no kickoff owner; disposing without a mailbox turn"
    );
    dispose_recovery_relay_outcome(
        shared,
        provider,
        state,
        outcome,
        plan.tmux_alive,
        plan.stop_source,
        plan.branch,
        &plan.best_response,
        false,
    )
    .await;
}
