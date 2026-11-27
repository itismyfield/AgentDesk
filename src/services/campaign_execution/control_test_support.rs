use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::json;

use super::{create_definition, decide};
use crate::domain::campaign::{
    definition::{DefinitionInput, WORKFLOW_VERSION},
    model::{
        Action, ActionKind, ActionState, Attempt, AttemptBinding, CampaignControl, CampaignState,
        Command, CommandEnvelope, HostGate, NodeDisposition, NodeRuntime, Repository, Role, Run,
        SessionIdentity, SessionObservation, SessionRecord, Stage, StopMode,
    },
};

pub(crate) fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-28T06:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

pub(crate) fn input() -> DefinitionInput {
    serde_json::from_value(json!({
        "repository": "adk", "title": "Campaign", "goal": "Verified work",
        "workflow_options": {"start":false,"implementation":false,"review":false,"landing":false,"merge":false,"deploy":false},
        "profile_version": "v1", "nodes": [
            {"id": "a", "title": "A", "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
             "acceptance": [{"id": "ac-a", "description": "Prove A", "verification": "test"}], "dependencies": []},
            {"id": "b", "title": "B", "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
             "acceptance": [{"id": "ac-b", "description": "Prove B", "verification": "test"}], "dependencies": ["a"]},
            {"id": "c", "title": "C", "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
             "acceptance": [{"id": "ac-c", "description": "Prove C", "verification": "operational"}], "dependencies": []}
        ]
    })).unwrap()
}

pub(crate) fn envelope(command: Command) -> CommandEnvelope {
    CommandEnvelope {
        idempotency_key: "key".into(),
        observed_at: now(),
        command,
    }
}

pub(crate) fn prepared() -> CampaignState {
    create_definition("campaign".into(), input(), "create-key", now())
        .unwrap()
        .state
}

pub(crate) fn running() -> CampaignState {
    let mut state = prepared();
    state.control = CampaignControl::Running;
    let gate = HostGate {
        host: "remote".into(),
        gate: "launch".into(),
        incarnation: "boot-2".into(),
    };
    let binding = AttemptBinding {
        campaign_id: state.id.clone(),
        run_id: "run".into(),
        node_id: "a".into(),
        series: 1,
        stage: Stage::Implementation,
        attempt_id: "attempt".into(),
        role: Role::Implementation,
        session: SessionIdentity {
            gate,
            session: "worker".into(),
            incarnation: "process-7".into(),
        },
        repository: Repository::Adk,
        hashes: state.definition.hashes()["a"].clone(),
        workflow_version: WORKFLOW_VERSION.into(),
        base_sha: "base".into(),
        candidate_sha: "candidate".into(),
        control_version: 1,
    };
    state.nodes.insert(
        "a".into(),
        NodeRuntime {
            stage: Stage::Implementation,
            disposition: NodeDisposition::Running,
            current_attempt: Some(binding.attempt_id.clone()),
            ..NodeRuntime::default()
        },
    );
    state.run = Some(Run {
        approvals: Default::default(),
        id: "run".into(),
        control_version: 1,
        stop: None,
        attempts: [(
            binding.attempt_id.clone(),
            Attempt {
                binding,
                started: true,
                outcome: None,
                execution: None,
            },
        )]
        .into_iter()
        .collect(),
        actions: BTreeMap::new(),
        sessions: Vec::new(),
        verifications: BTreeMap::new(),
        prerequisites: BTreeMap::new(),
        reviews: BTreeMap::new(),
        landings: BTreeMap::new(),
        merges: BTreeMap::new(),
        deployments: BTreeMap::new(),
        acceptance_checks: BTreeMap::new(),
        fences: BTreeMap::new(),
        seals: BTreeMap::new(),
        environments: BTreeMap::new(),
    });
    state
}

pub(crate) fn binding(state: &CampaignState) -> AttemptBinding {
    state.run.as_ref().unwrap().attempts["attempt"]
        .binding
        .clone()
}

pub(crate) fn with_action(phase: ActionState, started: bool) -> CampaignState {
    let mut state = running();
    let binding = binding(&state);
    let run = state.run.as_mut().unwrap();
    run.attempts.get_mut("attempt").unwrap().started = started;
    if phase == ActionState::Started {
        run.sessions.push(SessionRecord {
            identity: binding.session.clone(),
            attempt_id: binding.attempt_id.clone(),
            observation: SessionObservation::Gone {
                observed_at: now(),
                evidence: "reachable host: exact process absent".into(),
            },
        });
    }
    run.actions.insert(
        "action".into(),
        Action {
            id: "action".into(),
            target: binding.session.gate.clone(),
            binding,
            kind: ActionKind::Launch,
            state: phase,
            initiation_receipt: (phase == ActionState::Started).then(|| "effect-1".into()),
            reconciliation: None,
        },
    );
    state
}

pub(crate) fn step(state: &CampaignState, command: Command) -> CampaignState {
    let mut command = envelope(command);
    command.idempotency_key = format!("key-{}", state.event_sequence);
    decide(state, &command).unwrap().state
}

pub(crate) fn stop(state: &CampaignState, mode: StopMode) -> CampaignState {
    step(
        state,
        Command::RequestStop {
            expected_revision: state.definition_revision,
            expected_run: state.run.as_ref().unwrap().id.clone(),
            mode,
        },
    )
}
