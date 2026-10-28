use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::{
    definition::{self, DefinitionInput, WORKFLOW_VERSION},
    model::*,
};

type DecisionResult<T> = Result<T, Rejection>;
fn reject(code: RejectionCode) -> Rejection {
    Rejection::new(code)
}
fn run(state: &CampaignState) -> DecisionResult<&Run> {
    state
        .run
        .as_ref()
        .ok_or_else(|| reject(RejectionCode::StaleRun))
}
fn run_mut(state: &mut CampaignState) -> DecisionResult<&mut Run> {
    state
        .run
        .as_mut()
        .ok_or_else(|| reject(RejectionCode::StaleRun))
}
fn revision(state: &CampaignState, expected: u64) -> DecisionResult<()> {
    if state.definition_revision != expected {
        return Err(reject(RejectionCode::StaleDefinition));
    }
    Ok(())
}
fn target(state: &CampaignState, expected_revision: u64, expected_run: &str) -> DecisionResult<()> {
    revision(state, expected_revision)?;
    if run(state)?.id != expected_run {
        return Err(reject(RejectionCode::StaleRun));
    }
    Ok(())
}

pub fn execution_readiness() -> ExecutionReadiness {
    ExecutionReadiness::Unavailable
}

pub fn edit_guard(state: &CampaignState) -> DecisionResult<()> {
    if !matches!(
        state.control,
        CampaignControl::Prepared | CampaignControl::Stopped
    ) {
        return Err(reject(RejectionCode::InvalidState));
    }
    quiescent(state)
}

fn quiescent(state: &CampaignState) -> DecisionResult<()> {
    if let Some(run) = &state.run {
        if run
            .sessions
            .iter()
            .any(|s| !matches!(s.observation, SessionObservation::Gone { .. }))
        {
            return Err(reject(RejectionCode::SessionsNotGone));
        }
        if run.actions.values().any(|action| {
            matches!(action.kind, ActionKind::Launch | ActionKind::DeliverStage)
                && action.initiation_receipt.is_some()
                && !run.sessions.iter().any(|s| {
                    s.identity == action.binding.session
                        && matches!(s.observation, SessionObservation::Gone { .. })
                })
        }) {
            return Err(reject(RejectionCode::SessionsNotGone));
        }
        if run
            .actions
            .values()
            .any(|a| !matches!(a.state, ActionState::Reconciled | ActionState::Cancelled))
        {
            return Err(reject(RejectionCode::UnreconciledActions));
        }
        if run
            .attempts
            .values()
            .any(|a| a.started && a.outcome.is_none())
        {
            return Err(reject(RejectionCode::InvalidState));
        }
    }
    Ok(())
}

pub fn stop_guard(state: &CampaignState) -> DecisionResult<()> {
    if !matches!(
        state.control,
        CampaignControl::Running | CampaignControl::Stopping
    ) {
        return Err(reject(RejectionCode::InvalidState));
    }
    run(state)?;
    Ok(())
}

pub fn validate_result_binding(
    state: &CampaignState,
    binding: &AttemptBinding,
) -> DecisionResult<()> {
    let run = run(state)?;
    let node = state
        .nodes
        .get(&binding.node_id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    let attempt = run
        .attempts
        .get(&binding.attempt_id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    if binding.campaign_id != state.id
        || binding.run_id != run.id
        || attempt.binding != *binding
        || node.current_attempt.as_ref() != Some(&binding.attempt_id)
        || node.series != binding.series
        || node.stage != binding.stage
        || binding.repository != state.definition.input().repository
        || binding.workflow_version != WORKFLOW_VERSION
        || attempt.outcome.is_some()
        || !attempt.started
        || state.definition.hashes().get(&binding.node_id) != Some(&binding.hashes)
    {
        return Err(reject(RejectionCode::StaleBinding));
    }
    match state.control {
        CampaignControl::Running => Ok(()),
        CampaignControl::Stopping
            if run.stop.as_ref().is_some_and(|stop| {
                stop.mode == StopMode::FinishStage
                    && stop.finish_attempts.contains(&binding.attempt_id)
            }) =>
        {
            Ok(())
        }
        _ => Err(reject(RejectionCode::InvalidState)),
    }
}

pub fn validate_new_start(state: &CampaignState, binding: &AttemptBinding) -> DecisionResult<()> {
    if state.control != CampaignControl::Running {
        return Err(reject(RejectionCode::InvalidState));
    }
    let run = run(state)?;
    let attempt = run
        .attempts
        .get(&binding.attempt_id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    let node = state
        .nodes
        .get(&binding.node_id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    if binding.control_version != run.control_version {
        return Err(reject(RejectionCode::StaleFence));
    }
    if binding.campaign_id != state.id
        || binding.run_id != run.id
        || attempt.binding != *binding
        || attempt.outcome.is_some()
        || node.current_attempt.as_ref() != Some(&binding.attempt_id)
        || node.series != binding.series
        || node.stage != binding.stage
        || binding.repository != state.definition.input().repository
        || binding.workflow_version != WORKFLOW_VERSION
        || state.definition.hashes().get(&binding.node_id) != Some(&binding.hashes)
    {
        return Err(reject(RejectionCode::StaleBinding));
    }
    if run
        .sessions
        .iter()
        .any(|session| matches!(session.observation, SessionObservation::Unknown { .. }))
    {
        return Err(reject(RejectionCode::SessionsNotGone));
    }
    Ok(())
}

// Receipt lookup precedes decide, including all stale-version checks.
pub fn replay_receipt(
    receipt: &Receipt,
    fingerprint: &str,
) -> DecisionResult<Result<CommandResult, Rejection>> {
    if receipt.fingerprint != fingerprint {
        return Err(reject(RejectionCode::IdempotencyConflict));
    }
    Ok(receipt.result.clone())
}

pub fn command_fingerprint(command: &Command) -> Result<String, serde_json::Error> {
    serde_json::to_vec(command).map(|bytes| blake3::hash(&bytes).to_hex().to_string())
}

// The store looks up this campaign/key receipt under the same transaction lock.
pub fn decide_with_receipt(
    state: &CampaignState,
    envelope: &CommandEnvelope,
    receipt: Option<&Receipt>,
) -> DecisionResult<Decision> {
    if let Some(receipt) = receipt {
        let fingerprint = command_fingerprint(&envelope.command)
            .map_err(|_| reject(RejectionCode::InvalidDefinition))?;
        let result = replay_receipt(receipt, &fingerprint)??;
        return Ok(Decision {
            state: state.clone(),
            events: Vec::new(),
            result,
        });
    }
    decide(state, envelope)
}

fn finish(
    mut state: CampaignState,
    command_id: &str,
    observed_at: DateTime<Utc>,
    kind: EventKind,
) -> Decision {
    state.event_sequence += 1;
    state.last_event_at = Some(observed_at);
    let event = Event {
        sequence: state.event_sequence,
        run_id: state.run.as_ref().map(|r| r.id.clone()),
        command_id: command_id.to_owned(),
        observed_at,
        kind,
    };
    let result = CommandResult {
        definition_revision: state.definition_revision,
        run_id: state.run.as_ref().map(|r| r.id.clone()),
        control_version: state.run.as_ref().map(|r| r.control_version),
        event_sequence: state.event_sequence,
        control: state.control,
    };
    Decision {
        state,
        events: vec![event],
        result,
    }
}

pub fn create_definition(
    id: String,
    input: DefinitionInput,
    idempotency_key: &str,
    observed_at: DateTime<Utc>,
) -> DecisionResult<Decision> {
    if !definition::valid_id(&id) || idempotency_key.trim().is_empty() {
        return Err(reject(RejectionCode::InvalidDefinition));
    }
    let definition = definition::validate_definition(input)
        .map_err(|_| reject(RejectionCode::InvalidDefinition))?;
    let nodes = definition
        .input()
        .nodes
        .iter()
        .map(|n| (n.id.clone(), NodeRuntime::default()))
        .collect();
    Ok(finish(
        CampaignState {
            id,
            definition_revision: 1,
            definition,
            control: CampaignControl::Prepared,
            run: None,
            nodes,
            event_sequence: 0,
            last_event_at: None,
        },
        idempotency_key,
        observed_at,
        EventKind::DefinitionCreated,
    ))
}

pub fn decide(state: &CampaignState, envelope: &CommandEnvelope) -> DecisionResult<Decision> {
    if envelope.idempotency_key.trim().is_empty() {
        return Err(reject(RejectionCode::InvalidDefinition));
    }
    let mut next = state.clone();
    let event = match &envelope.command {
        Command::EditDefinition {
            expected_revision,
            definition,
            change,
        } => {
            revision(state, *expected_revision)?;
            edit_guard(state)?;
            let validated = definition::validate_definition(definition.clone())
                .map_err(|_| reject(RejectionCode::InvalidDefinition))?;
            let superseded_nodes = state.definition.superseded_nodes(&validated);
            if !superseded_nodes.is_empty()
                && !change.as_ref().is_some_and(|c| {
                    url::Url::parse(&c.reference).is_ok() && !c.explanation.trim().is_empty()
                })
            {
                return Err(reject(RejectionCode::MissingChangeReason));
            }
            if superseded_nodes
                .iter()
                .any(|id| state.nodes.get(id).is_some_and(|n| n.acceptance_complete))
            {
                return Err(reject(RejectionCode::CompletedNodeChanged));
            }
            for id in &superseded_nodes {
                if let Some(node) = next.nodes.get_mut(id) {
                    node.evidence_current = false;
                }
            }
            let mut nodes = BTreeMap::new();
            for node in &validated.input().nodes {
                nodes.insert(
                    node.id.clone(),
                    next.nodes.get(&node.id).cloned().unwrap_or_default(),
                );
            }
            next.nodes = nodes;
            next.definition = validated;
            next.definition_revision += 1;
            EventKind::DefinitionEdited {
                superseded_nodes,
                change: change.clone(),
            }
        }
        Command::Start { expected_revision } => {
            revision(state, *expected_revision)?;
            return Err(reject(RejectionCode::ExecutionUnavailable));
        }
        Command::Resume {
            expected_revision,
            expected_run,
        }
        | Command::PrepareRetry {
            expected_revision,
            expected_run,
            ..
        } => {
            target(state, *expected_revision, expected_run)?;
            return Err(reject(RejectionCode::ExecutionUnavailable));
        }
        Command::RequestStop {
            expected_revision,
            expected_run,
            mode,
        } => {
            target(state, *expected_revision, expected_run)?;
            stop_guard(state)?;
            let run = next
                .run
                .as_mut()
                .ok_or_else(|| reject(RejectionCode::StaleRun))?;
            let escalating = run
                .stop
                .as_ref()
                .is_some_and(|s| s.mode == StopMode::FinishStage && *mode == StopMode::Immediate);
            if run.stop.is_some() && !escalating {
                return Ok(unchanged(state));
            }
            run.control_version += 1;
            let targets = run
                .actions
                .values()
                .map(|a| a.target.clone())
                .chain(
                    run.actions
                        .values()
                        .filter(|a| matches!(a.kind, ActionKind::Launch | ActionKind::DeliverStage))
                        .map(|a| a.binding.session.gate.clone()),
                )
                .chain(run.sessions.iter().map(|s| s.identity.gate.clone()))
                .chain(run.stop.iter().flat_map(|s| s.targets.iter().cloned()))
                .collect();
            let finish_attempts = run
                .attempts
                .iter()
                .filter(|(_, a)| *mode == StopMode::FinishStage && a.started && a.outcome.is_none())
                .map(|(id, _)| id.clone())
                .collect();
            run.stop = Some(StopIntent {
                mode: *mode,
                requested_at: envelope.observed_at,
                targets,
                acknowledgements: Vec::new(),
                finish_attempts,
                confirmed_event: None,
            });
            for action in run.actions.values_mut() {
                if matches!(action.state, ActionState::Planned | ActionState::Claimed) {
                    action.state = ActionState::Cancelled;
                }
            }
            // A gate may have started an in-flight action before applying the fence.
            let in_flight: std::collections::BTreeSet<_> = run
                .actions
                .values()
                .filter(|a| matches!(a.state, ActionState::StartPending | ActionState::Unknown))
                .map(|a| a.binding.attempt_id.clone())
                .collect();
            for attempt in run.attempts.values_mut() {
                if attempt.outcome.is_none()
                    && (*mode == StopMode::Immediate
                        || (!attempt.started && !in_flight.contains(&attempt.binding.attempt_id)))
                {
                    attempt.outcome = Some(AttemptOutcome::UserInterrupted);
                    if let Some(node) = next.nodes.get_mut(&attempt.binding.node_id)
                        && node.current_attempt.as_ref() == Some(&attempt.binding.attempt_id)
                    {
                        node.disposition = NodeDisposition::Interrupted;
                    }
                }
            }
            next.control = CampaignControl::Stopping;
            if escalating {
                EventKind::StopEscalated
            } else {
                EventKind::StopRequested { mode: *mode }
            }
        }
        Command::PlanAction { action } => {
            validate_new_start(state, &action.binding)?;
            if action.id.trim().is_empty()
                || action.target.host.is_empty()
                || action.target.gate.is_empty()
                || action.target.incarnation.is_empty()
                || action.state != ActionState::Planned
                || action.initiation_receipt.is_some()
                || action.reconciliation.is_some()
            {
                return Err(reject(RejectionCode::InvalidState));
            }
            if run(state)?.actions.contains_key(&action.id) {
                return Err(reject(RejectionCode::AlreadyExists));
            }
            run_mut(&mut next)?
                .actions
                .insert(action.id.clone(), *action.clone());
            EventKind::ActionChanged {
                action_id: action.id.clone(),
            }
        }
        Command::AdvanceAction {
            action_id,
            binding,
            state: phase,
        } => {
            validate_new_start(state, binding)?;
            let action = action_mut(&mut next, action_id, binding)?;
            if !matches!(
                (action.state, phase),
                (ActionState::Planned, ActionState::Claimed)
                    | (ActionState::Claimed, ActionState::StartPending)
            ) {
                return Err(reject(RejectionCode::InvalidState));
            }
            action.state = *phase;
            EventKind::ActionChanged {
                action_id: action_id.clone(),
            }
        }
        Command::ObserveAction {
            action_id,
            binding,
            observation,
        } => {
            observe_action(&mut next, action_id, binding, observation)?;
            EventKind::ActionChanged {
                action_id: action_id.clone(),
            }
        }
        Command::ObserveSession {
            identity,
            observation,
        } => {
            if matches!(observation, SessionObservation::Gone { evidence, .. } if evidence.trim().is_empty())
            {
                return Err(reject(RejectionCode::InvalidState));
            }
            let current = run_mut(&mut next)?;
            if let Some(session) = current
                .sessions
                .iter_mut()
                .find(|s| s.identity == *identity)
            {
                session.observation = observation.clone();
            } else {
                let action = current
                    .actions
                    .values()
                    .find(|a| {
                        matches!(a.kind, ActionKind::Launch | ActionKind::DeliverStage)
                            && a.initiation_receipt.is_some()
                            && a.binding.session == *identity
                    })
                    .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
                current.sessions.push(SessionRecord {
                    identity: identity.clone(),
                    attempt_id: action.binding.attempt_id.clone(),
                    observation: observation.clone(),
                });
            }
            EventKind::SessionObserved {
                identity: identity.clone(),
            }
        }
        Command::AcknowledgeFence {
            expected_run,
            target,
            control_version,
        } => {
            self::target(state, state.definition_revision, expected_run)?;
            if state.control != CampaignControl::Stopping {
                return Err(reject(RejectionCode::InvalidState));
            }
            let run = run_mut(&mut next)?;
            if *control_version != run.control_version {
                return Err(reject(RejectionCode::StaleFence));
            }
            let stop = run
                .stop
                .as_mut()
                .ok_or_else(|| reject(RejectionCode::InvalidState))?;
            if !stop.targets.contains(target) {
                return Err(reject(RejectionCode::StaleBinding));
            }
            if run.actions.values().any(|a| {
                a.target == *target
                    && matches!(a.state, ActionState::StartPending | ActionState::Unknown)
            }) {
                return Err(reject(RejectionCode::UnreconciledActions));
            }
            if !stop
                .acknowledgements
                .iter()
                .any(|a| a.target == *target && a.control_version == *control_version)
            {
                stop.acknowledgements.push(FenceAcknowledgement {
                    target: target.clone(),
                    control_version: *control_version,
                });
            }
            EventKind::FenceAcknowledged {
                target: target.clone(),
            }
        }
        Command::ConfirmStopFence => {
            stop_guard(state)?;
            let sequence = next.event_sequence + 1;
            let run = run_mut(&mut next)?;
            let stop = run
                .stop
                .as_mut()
                .ok_or_else(|| reject(RejectionCode::InvalidState))?;
            if !stop.targets.iter().all(|g| {
                stop.acknowledgements
                    .iter()
                    .any(|a| a.target == *g && a.control_version == run.control_version)
            }) {
                return Err(reject(RejectionCode::FenceIncomplete));
            }
            if run
                .actions
                .values()
                .any(|a| matches!(a.state, ActionState::StartPending | ActionState::Unknown))
            {
                return Err(reject(RejectionCode::UnreconciledActions));
            }
            if stop.confirmed_event.is_some() {
                return Ok(unchanged(state));
            }
            stop.confirmed_event = Some(sequence);
            EventKind::StopConfirmed
        }
        Command::ConfirmStopped => {
            if state.control != CampaignControl::Stopping
                || !run(state)?
                    .stop
                    .as_ref()
                    .is_some_and(|s| s.confirmed_event.is_some())
            {
                return Err(reject(RejectionCode::FenceIncomplete));
            }
            quiescent(state)?;
            next.control = CampaignControl::Stopped;
            EventKind::RunStopped
        }
    };
    Ok(finish(
        next,
        &envelope.idempotency_key,
        envelope.observed_at,
        event,
    ))
}

fn unchanged(state: &CampaignState) -> Decision {
    Decision {
        state: state.clone(),
        events: Vec::new(),
        result: CommandResult {
            definition_revision: state.definition_revision,
            run_id: state.run.as_ref().map(|r| r.id.clone()),
            control_version: state.run.as_ref().map(|r| r.control_version),
            event_sequence: state.event_sequence,
            control: state.control,
        },
    }
}
fn action_mut<'a>(
    state: &'a mut CampaignState,
    id: &str,
    binding: &AttemptBinding,
) -> DecisionResult<&'a mut Action> {
    if binding.campaign_id != state.id {
        return Err(reject(RejectionCode::StaleBinding));
    }
    let run = run_mut(state)?;
    if binding.run_id != run.id {
        return Err(reject(RejectionCode::StaleRun));
    }
    let action = run
        .actions
        .get_mut(id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    if action.binding != *binding {
        return Err(reject(RejectionCode::StaleBinding));
    }
    Ok(action)
}
fn observe_action(
    state: &mut CampaignState,
    id: &str,
    binding: &AttemptBinding,
    observation: &ActionObservation,
) -> DecisionResult<()> {
    let current = run(state)?;
    let recorded = current
        .actions
        .get(id)
        .ok_or_else(|| reject(RejectionCode::StaleBinding))?;
    let gate_fenced = current.stop.as_ref().is_some_and(|s| {
        s.confirmed_event.is_some()
            || s.acknowledgements.iter().any(|ack| {
                ack.target == recorded.target && ack.control_version == current.control_version
            })
    });
    let action = action_mut(state, id, binding)?;
    match observation {
        ActionObservation::Started { receipt }
            if !receipt.trim().is_empty()
                && (!gate_fenced || action.initiation_receipt.as_ref() == Some(receipt))
                && action
                    .initiation_receipt
                    .as_ref()
                    .is_none_or(|old| old == receipt)
                && matches!(
                    action.state,
                    ActionState::StartPending | ActionState::Unknown
                ) =>
        {
            action.state = ActionState::Started;
            action.initiation_receipt = Some(receipt.clone());
        }
        ActionObservation::NotStarted { evidence }
            if !evidence.trim().is_empty()
                && action.initiation_receipt.is_none()
                && matches!(
                    action.state,
                    ActionState::StartPending | ActionState::Unknown
                ) =>
        {
            action.state = ActionState::Cancelled;
            action.reconciliation = Some(evidence.clone());
        }
        ActionObservation::Reconciled { evidence }
            if !evidence.trim().is_empty()
                && action.initiation_receipt.is_some()
                && matches!(action.state, ActionState::Started | ActionState::Unknown) =>
        {
            action.state = ActionState::Reconciled;
            action.reconciliation = Some(evidence.clone());
        }
        ActionObservation::Unknown { reason }
            if !reason.trim().is_empty()
                && matches!(
                    action.state,
                    ActionState::StartPending | ActionState::Started | ActionState::Unknown
                ) =>
        {
            action.state = ActionState::Unknown
        }
        _ => return Err(reject(RejectionCode::InvalidState)),
    }
    let session_to_track = if matches!(observation, ActionObservation::Started { .. })
        && matches!(action.kind, ActionKind::Launch | ActionKind::DeliverStage)
    {
        Some(action.binding.session.clone())
    } else {
        None
    };
    if matches!(observation, ActionObservation::Started { .. }) {
        let run = run_mut(state)?;
        if let Some(identity) = session_to_track
            && !run.sessions.iter().any(|s| s.identity == identity)
        {
            run.sessions.push(SessionRecord {
                identity,
                attempt_id: binding.attempt_id.clone(),
                observation: SessionObservation::Unknown {
                    reason: "session initiation observed; liveness not yet observed".into(),
                    last_observed_at: None,
                },
            });
        }
        if let Some(attempt) = run.attempts.get_mut(&binding.attempt_id) {
            attempt.started = true;
        }
        if let Some(stop) = &mut run.stop {
            if stop.mode == StopMode::FinishStage
                && run
                    .attempts
                    .get(&binding.attempt_id)
                    .is_some_and(|a| a.outcome.is_none())
            {
                stop.finish_attempts.insert(binding.attempt_id.clone());
            }
        }
    }
    if matches!(observation, ActionObservation::NotStarted { .. }) {
        let current = run_mut(state)?;
        let mut interrupted = false;
        if current.stop.is_some()
            && !current.actions.values().any(|a| {
                a.binding.attempt_id == binding.attempt_id
                    && !matches!(a.state, ActionState::Cancelled | ActionState::Reconciled)
            })
            && let Some(attempt) = current.attempts.get_mut(&binding.attempt_id)
            && !attempt.started
            && attempt.outcome.is_none()
        {
            attempt.outcome = Some(AttemptOutcome::UserInterrupted);
            interrupted = true;
        }
        if interrupted
            && let Some(node) = state.nodes.get_mut(&binding.node_id)
            && node.current_attempt.as_ref() == Some(&binding.attempt_id)
        {
            node.disposition = NodeDisposition::Interrupted;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "control_tests.rs"]
pub(crate) mod tests;
