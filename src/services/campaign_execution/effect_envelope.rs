use super::control::validate_new_start;
use super::host_gate::{
    EffectAuthority, EffectEnvelope, HostNodeFence, HostStop, PhaseBinding, RunFence,
};
use super::model::{
    ActionKind, ActionState, AttemptBinding, CampaignState, Rejection, RejectionCode, Run,
};
use super::stage_fsm::validate_verification_start;
use super::verification_run::{BoundInput, CurrentFence, Digest, PhaseAdmission, digest_of};

fn reject<T>(code: RejectionCode) -> Result<T, Rejection> {
    Err(Rejection::new(code))
}

fn current_run(state: &CampaignState) -> Result<&Run, Rejection> {
    state
        .run
        .as_ref()
        .ok_or_else(|| Rejection::new(RejectionCode::StaleRun))
}

fn node_epoch(run: &Run, node_id: &str) -> u64 {
    run.fences.get(node_id).map_or(0, |f| f.epoch)
}

// Every request for one effect carries the same nonce, so a lost response is answered by the
// original start rather than a second one.
pub fn execution_nonce(action_id: &str, payload_digest: &str) -> String {
    let key = format!("{action_id}\n{payload_digest}");
    format!("n-{}", blake3::hash(key.as_bytes()).to_hex())
}

// An approved phase effect and the exact input it was approved for, built once from server state
// so a gate never pairs the envelope with other launch arguments.
pub struct VerifiedPhaseLaunch {
    pub(super) envelope: EffectEnvelope,
    pub(super) input: BoundInput,
}

impl VerifiedPhaseLaunch {
    pub fn envelope(&self) -> &EffectEnvelope {
        &self.envelope
    }
}

pub fn phase_envelope(
    state: &CampaignState,
    execution_id: &str,
    phase_id: &str,
    consumer: &AttemptBinding,
) -> Result<EffectEnvelope, Rejection> {
    phase_launch(state, execution_id, phase_id, consumer).map(|launch| launch.envelope)
}

// A StartPending phase's effect: identity and payload from the V's origin and bound input, the
// authority from whichever of Work, Review prerequisite or FinishPlan holds it now.
pub fn phase_launch(
    state: &CampaignState,
    execution_id: &str,
    phase_id: &str,
    consumer: &AttemptBinding,
) -> Result<VerifiedPhaseLaunch, Rejection> {
    let run = current_run(state)?;
    let v = run
        .verifications
        .get(execution_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    let authority = validate_verification_start(state, v, consumer)?;
    let phase = v
        .phase(phase_id)
        .map_err(|_| Rejection::new(RejectionCode::StaleBinding))?;
    let action = run
        .actions
        .get(&phase.action_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    let fence = CurrentFence {
        run_control_version: run.control_version,
        node_epoch: node_epoch(run, &v.origin.node_id),
    };
    let payload_digest = match v.admit_phase_start(phase_id, action, &authority, fence) {
        Ok(PhaseAdmission::Start { payload_digest, .. })
            if action.state == ActionState::StartPending =>
        {
            payload_digest
        }
        _ => return reject(RejectionCode::InvalidState),
    };
    let input = phase
        .input
        .bound()
        .cloned()
        .ok_or_else(|| Rejection::new(RejectionCode::InvalidState))?;
    let envelope = EffectEnvelope {
        execution_nonce: execution_nonce(&action.id, &payload_digest),
        action_id: action.id.clone(),
        run_id: run.id.clone(),
        node_id: v.origin.node_id.clone(),
        origin_attempt_id: v.origin.owner_attempt.clone(),
        origin_binding_digest: v.origin.binding_digest.clone(),
        target_host_gate: action.target.clone(),
        payload_digest,
        kind: ActionKind::VerificationPhase,
        phase: Some(PhaseBinding {
            execution_id: v.origin.execution_id.clone(),
            phase_id: phase.id.clone(),
            kind: phase.kind,
            check_id: v.origin.check_id.clone(),
            input_digest: input.digest.clone(),
        }),
        authority: EffectAuthority::Verification(authority),
    };
    Ok(VerifiedPhaseLaunch { envelope, input })
}

// A general action's effect under the current attempt; the payload digest is the caller's
// launch, input or request content, fixed before the action was planned.
pub fn action_envelope(
    state: &CampaignState,
    action_id: &str,
    binding: &AttemptBinding,
    payload_digest: Digest,
) -> Result<EffectEnvelope, Rejection> {
    validate_new_start(state, binding)?;
    let run = current_run(state)?;
    let action = run
        .actions
        .get(action_id)
        .filter(|a| a.binding == *binding && a.kind != ActionKind::VerificationPhase)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleBinding))?;
    if action.state != ActionState::StartPending {
        return reject(RejectionCode::InvalidState);
    }
    let binding_digest =
        digest_of(binding).map_err(|_| Rejection::new(RejectionCode::InvalidState))?;
    Ok(EffectEnvelope {
        execution_nonce: execution_nonce(action_id, &payload_digest),
        action_id: action_id.into(),
        run_id: run.id.clone(),
        node_id: binding.node_id.clone(),
        origin_attempt_id: binding.attempt_id.clone(),
        origin_binding_digest: binding_digest.clone(),
        target_host_gate: action.target.clone(),
        payload_digest,
        kind: action.kind,
        phase: None,
        authority: EffectAuthority::CurrentAttempt {
            attempt_id: binding.attempt_id.clone(),
            binding_digest,
            node_epoch: node_epoch(run, &binding.node_id),
            run_control_version: run.control_version,
        },
    })
}

// The run and node fences a host applies before it admits this node's effects or acknowledges a stop.
pub fn host_fences(
    state: &CampaignState,
    node_id: &str,
) -> Result<(RunFence, HostNodeFence), Rejection> {
    let run = current_run(state)?;
    if !state.nodes.contains_key(node_id) {
        return reject(RejectionCode::StaleBinding);
    }
    let stop = run.stop.as_ref().map(|stop| HostStop {
        mode: stop.mode,
        finish_actions: stop
            .finish_grants
            .iter()
            .flat_map(|g| g.phase_actions.iter().cloned())
            .collect(),
    });
    let fence = run.fences.get(node_id).cloned().unwrap_or_default();
    Ok((
        RunFence {
            run_id: run.id.clone(),
            control_version: run.control_version,
            stop,
        },
        HostNodeFence {
            run_id: run.id.clone(),
            node_id: node_id.into(),
            epoch: fence.epoch,
            interrupted: fence.interruption.is_some(),
        },
    ))
}
