use super::*;
use crate::services::campaign_execution::definition::{WORKFLOW_VERSION, validate_definition};
use serde_json::json;

#[test]
fn model_preserves_nonempty_fence_ack_and_action_target_for_canonical_json_storage() {
    let definition = validate_definition(serde_json::from_value(json!({
        "repository": "adk", "title": "Persistent stop", "goal": "Keep target identity",
        "profile_version": "v1", "nodes": [{
            "id": "a", "title": "Work", "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
            "acceptance": [{"id": "stop", "description": "Reconcile exact host gate", "verification": "test"}],
            "dependencies": []
        }]
    })).unwrap()).unwrap();
    let now = Utc::now();
    let target = HostGate {
        host: "remote".into(),
        gate: "launch".into(),
        incarnation: "boot-2".into(),
    };
    let session = SessionIdentity {
        gate: target.clone(),
        session: "worker".into(),
        incarnation: "process-7".into(),
    };
    let binding = AttemptBinding {
        campaign_id: "campaign".into(),
        run_id: "run".into(),
        node_id: "a".into(),
        series: 2,
        stage: Stage::Implementation,
        attempt_id: "attempt".into(),
        role: Role::Implementation,
        session: session.clone(),
        repository: Repository::Adk,
        hashes: definition.hashes()["a"].clone(),
        workflow_version: WORKFLOW_VERSION.into(),
        base_sha: "base".into(),
        candidate_sha: "candidate".into(),
        control_version: 4,
    };
    let action = Action {
        id: "launch-1".into(),
        binding: binding.clone(),
        kind: ActionKind::Launch,
        target: target.clone(),
        state: ActionState::Unknown,
        initiation_receipt: None,
        reconciliation: None,
    };
    let run = Run {
        id: "run".into(),
        control_version: 5,
        stop: Some(StopIntent {
            mode: StopMode::FinishStage,
            requested_at: now,
            targets: [target.clone()].into_iter().collect(),
            acknowledgements: vec![FenceAcknowledgement {
                target: target.clone(),
                control_version: 5,
            }],
            finish_attempts: ["attempt".into()].into_iter().collect(),
            confirmed_event: None,
        }),
        attempts: [(
            "attempt".into(),
            Attempt {
                binding: binding.clone(),
                started: true,
                outcome: None,
            },
        )]
        .into_iter()
        .collect(),
        actions: [(action.id.clone(), action)].into_iter().collect(),
        sessions: vec![],
    };
    let state = CampaignState {
        id: "campaign".into(),
        definition_revision: 3,
        definition,
        control: CampaignControl::Stopping,
        run: Some(run),
        nodes: [(
            "a".into(),
            NodeRuntime {
                stage: Stage::Implementation,
                current_attempt: Some("attempt".into()),
                main_integrated: true,
                ..NodeRuntime::default()
            },
        )]
        .into_iter()
        .collect(),
        event_sequence: 9,
        last_event_at: Some(now),
    };
    let result = CommandResult {
        definition_revision: 3,
        run_id: Some("run".into()),
        control_version: Some(5),
        event_sequence: 9,
        control: CampaignControl::Stopping,
    };
    let decision = Decision {
        state,
        result: result.clone(),
        events: vec![Event {
            sequence: 9,
            run_id: Some("run".into()),
            command_id: "stop-key".into(),
            observed_at: now,
            kind: EventKind::StopRequested {
                mode: StopMode::FinishStage,
            },
        }],
    };
    let encoded =
        serde_json::to_vec(&decision).expect("nonempty stop ACKs must be JSON persistable");
    let restored: Decision = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(restored, decision);
    let run = restored.state.run.unwrap();
    assert_eq!(
        run.stop.as_ref().unwrap().acknowledgements[0].target,
        target
    );
    assert_eq!(
        run.stop.as_ref().unwrap().acknowledgements[0].control_version,
        5
    );
    assert!(run.sessions.is_empty());
    assert_eq!(run.actions["launch-1"].target, target);
    assert_eq!(run.attempts["attempt"].binding.control_version, 4);
    assert_eq!(run.control_version, 5);
    assert!(restored.state.nodes["a"].main_integrated);
    assert!(!restored.state.nodes["a"].acceptance_complete);
    let session_record = SessionRecord {
        identity: session,
        attempt_id: binding.attempt_id.clone(),
        observation: SessionObservation::Unknown {
            reason: "host offline".into(),
            last_observed_at: Some(now),
        },
    };
    let envelope = CommandEnvelope {
        idempotency_key: "observe-key".into(),
        observed_at: now,
        command: Command::ObserveAction {
            action_id: "launch-1".into(),
            binding: Box::new(binding),
            observation: ActionObservation::Unknown {
                reason: "receipt lost".into(),
            },
        },
    };
    assert!(serde_json::to_vec(&(session_record, envelope)).is_ok());
    let receipt = Receipt {
        fingerprint: "normalized-command".into(),
        result: Ok(result),
    };
    assert_eq!(
        serde_json::from_slice::<Receipt>(&serde_json::to_vec(&receipt).unwrap()).unwrap(),
        receipt
    );
    assert_eq!(
        Rejection::new(RejectionCode::ExecutionUnavailable).code,
        RejectionCode::ExecutionUnavailable
    );
    assert_eq!(
        serde_json::to_value(ExecutionReadiness::Unavailable).unwrap(),
        "unavailable"
    );
}

#[test]
fn model_command_input_rejects_self_reported_completion_and_stop_event_cas() {
    for name in [
        "complete_node",
        "complete_run",
        "accept_stage_result",
        "set_status",
    ] {
        assert!(
            serde_json::from_value::<Command>(json!({"command": name, "result": "PASS"})).is_err()
        );
    }
    let command = json!({"command": "request_stop", "expected_revision": 3, "expected_run": "run", "mode": "immediate"});
    assert!(serde_json::from_value::<Command>(command.clone()).is_ok());
    let mut event_cas = command;
    event_cas["expected_event_sequence"] = json!(8);
    assert!(serde_json::from_value::<Command>(event_cas).is_err());
}

#[test]
fn model_fence_ack_requires_run_identity_even_when_gate_and_version_are_known() {
    let mut ack = json!({
        "command": "acknowledge_fence",
        "target": { "host": "remote", "gate": "launch", "incarnation": "gate-1" },
        "control_version": 5
    });
    assert!(serde_json::from_value::<Command>(ack.clone()).is_err());
    ack["expected_run"] = json!("run-1");
    assert!(serde_json::from_value::<Command>(ack).is_ok());
}
