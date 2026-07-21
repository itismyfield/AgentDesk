use std::sync::Arc;

use super::super::SharedData;
use super::super::inflight::{
    DEAD_WATCHER_PROVEN_DEAD_SECS, GuardedClearOutcome, InflightTurnIdentity, InflightTurnState,
    clear_inflight_state_if_matches_identity_generation, opt_channel_id, opt_message_id,
};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::platform::tmux::PaneLiveness;
#[cfg(unix)]
use crate::services::process::ProcessIdentity;
use crate::services::process::ProcessIdentityProbe;
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

fn decision_for_user_identity(
    user_msg_id: u64,
    owner_decision: AbandonedTmuxCleanupDecision,
) -> AbandonedTmuxCleanupDecision {
    if user_msg_id == 0 && owner_decision == AbandonedTmuxCleanupDecision::Kill {
        AbandonedTmuxCleanupDecision::TerminalMarkerOnly
    } else {
        owner_decision
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

fn claude_e_process_probe_decision(probe: ProcessIdentityProbe) -> AbandonedTmuxCleanupDecision {
    match probe {
        ProcessIdentityProbe::GoneOrReused => AbandonedTmuxCleanupDecision::Kill,
        ProcessIdentityProbe::Same | ProcessIdentityProbe::ProbeError => {
            AbandonedTmuxCleanupDecision::PreserveRetry
        }
    }
}

// This closes only the row-present ClaudeE sweeper path. The Drop-time row-delete
// gap remains a separate follow-up because no durable identity survives that deletion.
#[cfg(unix)]
fn claude_e_process_cleanup_decision(
    runtime_kind: Option<RuntimeHandoffKind>,
    pid: Option<u32>,
    starttime: Option<u128>,
    macos_lstart_hash: Option<u128>,
) -> AbandonedTmuxCleanupDecision {
    if runtime_kind != Some(RuntimeHandoffKind::ClaudeEAdapter) {
        return AbandonedTmuxCleanupDecision::PreserveRetry;
    }
    let Some(pid) = pid.filter(|pid| *pid != 0) else {
        return AbandonedTmuxCleanupDecision::PreserveRetry;
    };
    if starttime.is_none() && macos_lstart_hash.is_none() {
        return AbandonedTmuxCleanupDecision::PreserveRetry;
    }
    let identity = ProcessIdentity::from_persisted(starttime, macos_lstart_hash);
    claude_e_process_probe_decision(identity.probe(pid))
}

#[cfg(not(unix))]
fn claude_e_process_cleanup_decision(
    _runtime_kind: Option<RuntimeHandoffKind>,
    _pid: Option<u32>,
    _starttime: Option<u128>,
    _macos_lstart_hash: Option<u128>,
) -> AbandonedTmuxCleanupDecision {
    AbandonedTmuxCleanupDecision::PreserveRetry
}

pub(super) async fn abandoned_tmux_cleanup_decision_for(
    state: &InflightTurnState,
) -> AbandonedTmuxCleanupDecision {
    let Some(session_name) = state.tmux_session_name.as_deref() else {
        let runtime_kind = state.runtime_kind;
        let pid = state.claude_e_pid;
        let starttime = state.claude_e_process_starttime;
        let macos_lstart_hash = state.claude_e_macos_lstart_hash;
        return run_blocking_cleanup_probe(move || {
            claude_e_process_cleanup_decision(runtime_kind, pid, starttime, macos_lstart_hash)
        })
        .await;
    };
    let session_name = session_name.trim();
    if session_name.is_empty() {
        return AbandonedTmuxCleanupDecision::PreserveRetry;
    }
    let session_name = session_name.to_string();
    let owner_decision = run_blocking_cleanup_probe(move || {
        abandoned_tmux_cleanup_decision(
            true,
            crate::services::tmux_diagnostics::tmux_session_pane_liveness(&session_name),
            runtime_activity_evidence(&session_name),
        )
    })
    .await;
    decision_for_user_identity(state.user_msg_id, owner_decision)
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
    publish_completion_after_cleanup: bool,
}

fn abandoned_mailbox_finish_actions(
    removed_token_present: bool,
    has_pending: bool,
) -> AbandonedMailboxFinishActions {
    AbandonedMailboxFinishActions {
        cancel_removed_token: removed_token_present,
        schedule_queue_kickoff: has_pending,
        publish_completion_after_cleanup: removed_token_present,
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
    sweep_started_before: std::time::Instant,
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

    let Some(channel) = opt_channel_id(state.channel_id) else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            "abandoned mailbox cleanup skipped because persisted channel id is zero"
        );
        return AbandonedTmuxCleanupOutcome::from_plan(plan);
    };
    let Some(user_msg_id) = opt_message_id(state.user_msg_id) else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel.get(),
            "abandoned mailbox cleanup skipped because persisted user message id is zero"
        );
        return AbandonedTmuxCleanupOutcome::from_plan(plan);
    };
    let finish = super::super::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_without_completion(
        shared,
        provider,
        channel,
        user_msg_id,
        state.turn_nonce.clone(),
        sweep_started_before,
    )
    .await;
    let actions =
        abandoned_mailbox_finish_actions(finish.removed_token.is_some(), finish.has_pending);
    if actions.cancel_removed_token
        && let Some(removed_token) = finish.removed_token.as_ref()
    {
        super::super::turn_bridge::cancel_active_token(
            removed_token,
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
    if actions.publish_completion_after_cleanup {
        super::super::turn_completion_events::publish_mailbox_release_completion_event(
            shared,
            channel,
            Some(user_msg_id.get()),
            &finish,
        );
    }
    AbandonedTmuxCleanupOutcome::from_plan(plan)
}

fn abandoned_placeholder_key(
    state: &InflightTurnState,
) -> Option<super::super::placeholder_controller::PlaceholderKey> {
    Some(super::super::placeholder_controller::PlaceholderKey {
        provider: ProviderKind::from_str(&state.provider)?,
        channel_id: opt_channel_id(state.channel_id)?,
        message_id: opt_message_id(state.current_msg_id)?,
    })
}

async fn detach_abandoned_placeholder_controller(
    shared: &Arc<SharedData>,
    state: &InflightTurnState,
) {
    if let Some(key) = abandoned_placeholder_key(state) {
        shared
            .ui
            .placeholder_controller
            .revoke_and_detach(&key)
            .await;
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
    sweep_started_before: std::time::Instant,
    evidence: AbandonedCleanupEvidence,
) -> bool {
    if !super::inflight_state_still_same_turn(provider, state, age_secs) {
        return false;
    }
    let cleanup =
        finalize_abandoned_mailbox(shared, provider, state, sweep_started_before, evidence).await;
    let same_turn = super::inflight_state_still_same_turn(provider, state, age_secs);
    let deleted = same_turn && cleanup.delete_state_if_allowed(provider, state);
    if should_detach_after_cleanup(same_turn, deleted) {
        detach_abandoned_placeholder_controller(shared, state).await;
    }
    deleted
}

pub(super) async fn finalize_probe_cleanup_if_same_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    sweep_started_before: std::time::Instant,
    probe: super::PlaceholderProbe,
) -> bool {
    let Some(evidence) = abandoned_cleanup_evidence_for_probe(probe) else {
        return false;
    };
    finalize_cleanup_if_same_turn(
        shared,
        provider,
        state,
        age_secs,
        sweep_started_before,
        evidence,
    )
    .await
}

pub(super) async fn finalize_owner_dead_cleanup_if_same_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    sweep_started_before: std::time::Instant,
    repair_visible_on_revival: bool,
) -> bool {
    if !super::inflight_state_still_same_turn(provider, state, age_secs) {
        return false;
    }
    let cleanup = finalize_abandoned_mailbox(
        shared,
        provider,
        state,
        sweep_started_before,
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
        detach_abandoned_placeholder_controller(shared, state).await;
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::{
        AbandonedCleanupEvidence, AbandonedTmuxCleanupDecision, RevivedVisibleRepair,
        RuntimeActivityEvidence, abandoned_cleanup_evidence_for_probe, abandoned_cleanup_plan,
        abandoned_mailbox_finish_actions, abandoned_tmux_cleanup_decision,
        abandoned_tmux_cleanup_decision_for, claude_e_process_cleanup_decision,
        claude_e_process_probe_decision, decision_for_user_identity, revived_visible_repair,
        run_blocking_cleanup_probe, runtime_activity_evidence_from, should_detach_after_cleanup,
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

    #[test]
    fn zero_id_rows_require_owner_probe_before_terminal_marker_cleanup() {
        assert_eq!(
            decision_for_user_identity(0, AbandonedTmuxCleanupDecision::PreserveRetry),
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
        assert_eq!(
            decision_for_user_identity(0, AbandonedTmuxCleanupDecision::Kill),
            AbandonedTmuxCleanupDecision::TerminalMarkerOnly,
        );
        assert_eq!(
            decision_for_user_identity(9101, AbandonedTmuxCleanupDecision::Kill),
            AbandonedTmuxCleanupDecision::Kill,
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

    #[tokio::test]
    async fn claude_e_missing_or_legacy_identity_preserves_retry() {
        let mut state = sweep_state();
        state.tmux_session_name = None;
        state.runtime_kind =
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeEAdapter);
        state.claude_e_pid = Some(std::process::id());

        assert_eq!(
            abandoned_tmux_cleanup_decision_for(&state).await,
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
    }

    #[tokio::test]
    async fn claude_e_live_process_preserves_retry() {
        let mut state = sweep_state();
        let identity = crate::services::process::ProcessIdentity::capture(std::process::id());
        state.tmux_session_name = None;
        state.runtime_kind =
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeEAdapter);
        state.claude_e_pid = Some(std::process::id());
        state.claude_e_process_starttime = identity.persisted_starttime();
        state.claude_e_macos_lstart_hash = identity.persisted_macos_lstart_hash();

        assert_eq!(
            abandoned_tmux_cleanup_decision_for(&state).await,
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
    }

    #[test]
    fn claude_e_probe_error_preserves_retry() {
        assert_eq!(
            claude_e_process_probe_decision(
                crate::services::process::ProcessIdentityProbe::ProbeError,
            ),
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_e_pid_reuse_evidence_allows_cleanup() {
        let identity = crate::services::process::ProcessIdentity::capture(std::process::id());
        let wrong_starttime = identity
            .persisted_starttime()
            .map(|value| value.wrapping_add(1));
        let wrong_macos_hash = identity
            .persisted_macos_lstart_hash()
            .map(|value| value.wrapping_add(1));
        assert_eq!(
            claude_e_process_cleanup_decision(
                Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeEAdapter),
                Some(std::process::id()),
                wrong_starttime,
                wrong_macos_hash,
            ),
            AbandonedTmuxCleanupDecision::Kill,
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn claude_e_process_cleanup_is_fail_closed_without_unix_probe() {
        assert_eq!(
            claude_e_process_cleanup_decision(
                Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeEAdapter),
                Some(42),
                Some(1),
                Some(1),
            ),
            AbandonedTmuxCleanupDecision::PreserveRetry,
        );
    }

    #[test]
    fn tmuxless_wrong_runtime_kind_preserves_retry() {
        assert_eq!(
            claude_e_process_cleanup_decision(
                Some(crate::services::agent_protocol::RuntimeHandoffKind::ProcessBackend),
                Some(u32::MAX),
                Some(1),
                Some(1),
            ),
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
    fn tmux_present_path_is_unchanged_by_claude_e_process_evidence() {
        assert_eq!(
            abandoned_tmux_cleanup_decision(
                true,
                PaneLiveness::DeadOrAbsent,
                RuntimeActivityEvidence::Inactive,
            ),
            AbandonedTmuxCleanupDecision::Kill,
        );
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
        assert!(!tokenless_pending.publish_completion_after_cleanup);
        assert!(!tokenless_idle.schedule_queue_kickoff);
    }

    #[test]
    fn destructive_finalize_publishes_only_after_cleanup_actions() {
        let actions = abandoned_mailbox_finish_actions(true, true);
        let mut order = Vec::new();
        if actions.cancel_removed_token {
            order.push("cancel");
        }
        if actions.schedule_queue_kickoff {
            order.push("kickoff");
        }
        if actions.publish_completion_after_cleanup {
            order.push("publish");
        }

        assert_eq!(order, ["cancel", "kickoff", "publish"]);
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
