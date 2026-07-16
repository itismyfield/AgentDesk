use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::super::SharedData;
use super::super::inflight::{
    DEAD_WATCHER_PROVEN_DEAD_SECS, GuardedClearOutcome, InflightTurnIdentity, InflightTurnState,
    clear_inflight_state_if_matches_identity_generation,
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

/// Map the Discord probe to the only evidence that may finalize a mailbox.
/// Keeping this mapping in the production path prevents call sites from swapping
/// terminal delivery and owner death policies independently of the tested table.
pub(super) fn abandoned_cleanup_evidence_for_probe(
    probe: super::PlaceholderProbe,
) -> Option<AbandonedCleanupEvidence> {
    match probe {
        super::PlaceholderProbe::AlreadyDelivered => {
            Some(AbandonedCleanupEvidence::TerminalDelivered)
        }
        super::PlaceholderProbe::MessageGone => Some(AbandonedCleanupEvidence::OwnerDeath),
        super::PlaceholderProbe::StillPlaceholder | super::PlaceholderProbe::ProbeFailed => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbandonedCleanupPlan {
    decision: AbandonedTmuxCleanupDecision,
    cleanup_policy: super::super::TmuxCleanupPolicy,
    finish_mailbox: bool,
    allows_state_delete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbandonedMailboxFinishActions {
    cancel_removed_token: bool,
    schedule_queue_kickoff: bool,
}

fn abandoned_mailbox_finish_actions(
    removed_token_present: bool,
    has_pending: bool,
) -> AbandonedMailboxFinishActions {
    AbandonedMailboxFinishActions {
        cancel_removed_token: removed_token_present,
        schedule_queue_kickoff: has_pending,
    }
}

/// Pure plan consumed directly by [`finalize_abandoned_mailbox`]. Owner-death
/// evidence authorizes bounded eviction once the final probe says Kill, even if
/// the matching mailbox token is already absent. PreserveRetry always keeps the
/// durable row, including the revived-during-edit race.
fn abandoned_cleanup_plan(
    state: &InflightTurnState,
    evidence: AbandonedCleanupEvidence,
    owner_decision: AbandonedTmuxCleanupDecision,
) -> AbandonedCleanupPlan {
    let decision = if evidence.terminal_delivered() {
        AbandonedTmuxCleanupDecision::Kill
    } else {
        owner_decision
    };
    let cleanup_policy = if evidence.terminal_delivered() {
        super::super::TmuxCleanupPolicy::PreserveSession
    } else {
        super::super::TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some("placeholder_sweeper_abandon"),
        }
    };
    AbandonedCleanupPlan {
        decision,
        cleanup_policy,
        finish_mailbox: state.user_msg_id != 0 && decision == AbandonedTmuxCleanupDecision::Kill,
        allows_state_delete: decision != AbandonedTmuxCleanupDecision::PreserveRetry,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AbandonedTmuxCleanupOutcome {
    pub(super) decision: AbandonedTmuxCleanupDecision,
    state_delete_authorized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevivedVisibleRepair {
    None,
    InvalidateRenderCache,
}

fn revived_visible_repair(
    repair_enabled: bool,
    same_turn: bool,
    decision: AbandonedTmuxCleanupDecision,
) -> RevivedVisibleRepair {
    if repair_enabled && same_turn && decision == AbandonedTmuxCleanupDecision::PreserveRetry {
        RevivedVisibleRepair::InvalidateRenderCache
    } else {
        RevivedVisibleRepair::None
    }
}

impl AbandonedTmuxCleanupOutcome {
    fn from_plan(plan: AbandonedCleanupPlan) -> Self {
        Self {
            decision: plan.decision,
            state_delete_authorized: plan.allows_state_delete,
        }
    }

    fn allows_state_delete(self) -> bool {
        self.state_delete_authorized
    }

    pub(super) fn delete_state_if_allowed(
        self,
        provider: &ProviderKind,
        state: &InflightTurnState,
    ) -> bool {
        self.allows_state_delete()
            && clear_inflight_state_if_matches_identity_generation(
                provider,
                state.channel_id,
                &InflightTurnIdentity::from_state(state),
                state.effective_finalizer_turn_id(),
                &state.updated_at,
                state.save_generation,
            ) == GuardedClearOutcome::Cleared
    }
}

/// Finalize an abandoned mailbox from one explicit evidence source. Terminal
/// delivery may skip owner probing and preserves the reusable tmux session;
/// owner-death cleanup re-probes and keeps the destructive cleanup policy.
pub(super) async fn finalize_abandoned_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    evidence: AbandonedCleanupEvidence,
) -> AbandonedTmuxCleanupOutcome {
    let owner_decision = if evidence.terminal_delivered() {
        AbandonedTmuxCleanupDecision::PreserveRetry
    } else {
        abandoned_tmux_cleanup_decision_for(state).await
    };
    let plan = abandoned_cleanup_plan(state, evidence, owner_decision);
    if !plan.finish_mailbox {
        return AbandonedTmuxCleanupOutcome::from_plan(plan);
    }

    let channel = serenity::ChannelId::new(state.channel_id);
    let finish = super::super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        channel,
        serenity::MessageId::new(state.user_msg_id),
    )
    .await;
    let actions =
        abandoned_mailbox_finish_actions(finish.removed_token.is_some(), finish.has_pending);
    if actions.cancel_removed_token
        && let Some(removed_token) = finish.removed_token
    {
        super::super::turn_bridge::cancel_active_token(
            &removed_token,
            plan.cleanup_policy,
            "placeholder_sweeper abandoned",
        );
        super::super::saturating_decrement_global_active(shared);
    }
    // A restarted/tokenless mailbox can still own durable soft-queued work.
    // Schedule from the actor's authoritative backlog flag independently of
    // token removal so deleting the orphan inflight cannot strand that queue.
    if actions.schedule_queue_kickoff {
        super::super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel,
            "placeholder_sweeper_abandon",
        );
    }
    AbandonedTmuxCleanupOutcome::from_plan(plan)
}

fn abandoned_placeholder_key(
    state: &InflightTurnState,
) -> Option<super::super::placeholder_controller::PlaceholderKey> {
    let provider = ProviderKind::from_str(&state.provider)?;
    (state.current_msg_id != 0).then(|| super::super::placeholder_controller::PlaceholderKey {
        provider,
        channel_id: serenity::ChannelId::new(state.channel_id),
        message_id: serenity::MessageId::new(state.current_msg_id),
    })
}

fn detach_abandoned_placeholder_controller(shared: &Arc<SharedData>, state: &InflightTurnState) {
    if let Some(key) = abandoned_placeholder_key(state) {
        shared.ui.placeholder_controller.detach(&key);
    }
}

async fn invalidate_abandoned_placeholder_render_cache(
    shared: &Arc<SharedData>,
    state: &InflightTurnState,
) -> bool {
    let Some(key) = abandoned_placeholder_key(state) else {
        return false;
    };
    shared
        .ui
        .placeholder_controller
        .invalidate_render_cache(&key)
        .await
}

fn should_detach_after_cleanup(same_turn: bool, state_deleted: bool) -> bool {
    same_turn && state_deleted
}

async fn finalize_cleanup_if_same_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    evidence: AbandonedCleanupEvidence,
) -> bool {
    if !super::inflight_state_still_same_turn(provider, state, age_secs) {
        return false;
    }
    let cleanup = finalize_abandoned_mailbox(shared, provider, state, evidence).await;
    let same_turn = super::inflight_state_still_same_turn(provider, state, age_secs);
    let deleted = same_turn && cleanup.delete_state_if_allowed(provider, state);
    if should_detach_after_cleanup(same_turn, deleted) {
        detach_abandoned_placeholder_controller(shared, state);
    }
    deleted
}

pub(super) async fn finalize_probe_cleanup_if_same_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    probe: super::PlaceholderProbe,
) -> bool {
    let Some(evidence) = abandoned_cleanup_evidence_for_probe(probe) else {
        return false;
    };
    finalize_cleanup_if_same_turn(shared, provider, state, age_secs, evidence).await
}

pub(super) async fn finalize_owner_dead_cleanup_if_same_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    repair_visible_on_revival: bool,
) -> bool {
    if !super::inflight_state_still_same_turn(provider, state, age_secs) {
        return false;
    }
    let cleanup = finalize_abandoned_mailbox(
        shared,
        provider,
        state,
        AbandonedCleanupEvidence::OwnerDeath,
    )
    .await;
    let same_turn = super::inflight_state_still_same_turn(provider, state, age_secs);
    if revived_visible_repair(repair_visible_on_revival, same_turn, cleanup.decision)
        == RevivedVisibleRepair::InvalidateRenderCache
    {
        invalidate_abandoned_placeholder_render_cache(shared, state).await;
        return false;
    }
    let deleted = same_turn && cleanup.delete_state_if_allowed(provider, state);
    if should_detach_after_cleanup(same_turn, deleted) {
        detach_abandoned_placeholder_controller(shared, state);
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::{
        AbandonedCleanupEvidence, AbandonedTmuxCleanupDecision, RevivedVisibleRepair,
        RuntimeActivityEvidence, abandoned_cleanup_evidence_for_probe, abandoned_cleanup_plan,
        abandoned_mailbox_finish_actions, abandoned_tmux_cleanup_decision,
        abandoned_tmux_cleanup_decision_for, revived_visible_repair, run_blocking_cleanup_probe,
        runtime_activity_evidence_from, should_detach_after_cleanup,
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
    fn discord_probe_maps_to_the_only_valid_cleanup_evidence() {
        assert_eq!(
            abandoned_cleanup_evidence_for_probe(super::super::PlaceholderProbe::AlreadyDelivered),
            Some(AbandonedCleanupEvidence::TerminalDelivered),
        );
        assert_eq!(
            abandoned_cleanup_evidence_for_probe(super::super::PlaceholderProbe::MessageGone),
            Some(AbandonedCleanupEvidence::OwnerDeath),
        );
        assert_eq!(
            abandoned_cleanup_evidence_for_probe(super::super::PlaceholderProbe::StillPlaceholder),
            None,
        );
        assert_eq!(
            abandoned_cleanup_evidence_for_probe(super::super::PlaceholderProbe::ProbeFailed),
            None,
        );
    }

    #[test]
    fn production_cleanup_plan_pins_evidence_probe_policy_and_delete_gate() {
        let state = sweep_state();
        let delivered = abandoned_cleanup_plan(
            &state,
            AbandonedCleanupEvidence::TerminalDelivered,
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
        assert_eq!(delivered.decision, AbandonedTmuxCleanupDecision::Kill);
        assert_eq!(
            delivered.cleanup_policy,
            crate::services::discord::TmuxCleanupPolicy::PreserveSession,
        );
        assert!(delivered.finish_mailbox);
        assert!(delivered.allows_state_delete);

        let revived = abandoned_cleanup_plan(
            &state,
            AbandonedCleanupEvidence::OwnerDeath,
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
        assert_eq!(
            revived.decision,
            AbandonedTmuxCleanupDecision::PreserveRetry
        );
        assert!(matches!(
            revived.cleanup_policy,
            crate::services::discord::TmuxCleanupPolicy::CleanupSession { .. }
        ));
        assert!(!revived.finish_mailbox);
        assert!(!revived.allows_state_delete);

        let dead = abandoned_cleanup_plan(
            &state,
            AbandonedCleanupEvidence::OwnerDeath,
            AbandonedTmuxCleanupDecision::Kill,
        );
        assert_eq!(dead.decision, AbandonedTmuxCleanupDecision::Kill);
        assert!(matches!(
            dead.cleanup_policy,
            crate::services::discord::TmuxCleanupPolicy::CleanupSession { .. }
        ));
        assert!(dead.finish_mailbox);
        assert!(dead.allows_state_delete);
    }

    #[test]
    fn terminal_marker_owner_death_evicts_without_constructing_message_id_zero() {
        let mut state = sweep_state();
        state.user_msg_id = 0;

        let plan = abandoned_cleanup_plan(
            &state,
            AbandonedCleanupEvidence::OwnerDeath,
            AbandonedTmuxCleanupDecision::TerminalMarkerOnly,
        );

        assert_eq!(
            plan.decision,
            AbandonedTmuxCleanupDecision::TerminalMarkerOnly
        );
        assert!(!plan.finish_mailbox);
        assert!(plan.allows_state_delete);
    }

    #[test]
    fn owner_death_allows_tokenless_bounded_eviction_but_revival_preserves_row() {
        let state = sweep_state();
        let dead = abandoned_cleanup_plan(
            &state,
            AbandonedCleanupEvidence::OwnerDeath,
            AbandonedTmuxCleanupDecision::Kill,
        );
        let revived = abandoned_cleanup_plan(
            &state,
            AbandonedCleanupEvidence::OwnerDeath,
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );

        assert!(dead.allows_state_delete);
        assert!(!revived.allows_state_delete);
    }

    #[test]
    fn tokenless_finalize_with_pending_soft_queue_still_schedules_kickoff() {
        let tokenless_pending = abandoned_mailbox_finish_actions(false, true);
        let tokenless_idle = abandoned_mailbox_finish_actions(false, false);

        assert!(!tokenless_pending.cancel_removed_token);
        assert!(tokenless_pending.schedule_queue_kickoff);
        assert!(!tokenless_idle.schedule_queue_kickoff);
    }

    #[test]
    fn controller_detach_is_gated_by_identity_and_committed_state_delete() {
        assert!(should_detach_after_cleanup(true, true));
        assert!(!should_detach_after_cleanup(false, true));
        assert!(!should_detach_after_cleanup(true, false));
    }

    #[test]
    fn revived_after_abandoned_edit_preserves_row_and_requires_visible_repair() {
        let revived = abandoned_cleanup_plan(
            &sweep_state(),
            AbandonedCleanupEvidence::OwnerDeath,
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );

        assert!(!revived.allows_state_delete);
        assert_eq!(
            revived_visible_repair(true, true, revived.decision),
            RevivedVisibleRepair::InvalidateRenderCache
        );
        assert_eq!(
            revived_visible_repair(true, true, AbandonedTmuxCleanupDecision::Kill),
            RevivedVisibleRepair::None
        );
        assert_eq!(
            revived_visible_repair(true, false, revived.decision),
            RevivedVisibleRepair::None
        );
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
