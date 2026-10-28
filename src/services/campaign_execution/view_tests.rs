use super::*;
use crate::services::campaign_execution::{
    control::tests::{binding, envelope, input, now, prepared, running, step, stop, with_action},
    definition::validate_definition,
};
use serde_json::json;

fn rejection(state: &CampaignState, command: PublicCommand) -> Option<RejectionCode> {
    view(state, now())
        .commands
        .into_iter()
        .find(|c| c.command == command)
        .unwrap()
        .rejection
}

#[test]
fn server_view_serialization_has_explicit_versions_modes_nulls_and_public_command_names() {
    let state = prepared();
    let encoded = serde_json::to_value(view(&state, now())).unwrap();
    let object = encoded.as_object().unwrap();
    let mut keys: Vec<_> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = vec![
        "execution_mode",
        "campaign_id",
        "as_of",
        "definition_revision",
        "run_id",
        "control_version",
        "last_event_sequence",
        "last_event_at",
        "control",
        "stop_mode",
        "stop_confirmed",
        "display",
        "execution_readiness",
        "unknown",
        "failed_count",
        "blocked_count",
        "reasons",
        "commands",
        "nodes",
        "sessions",
        "unreconciled_actions",
    ];
    expected.sort_unstable();
    assert_eq!(keys, expected);
    assert_eq!(encoded["execution_mode"], "server");
    assert_eq!(encoded["as_of"], "2026-10-28T06:00:00Z");
    assert_eq!(encoded["definition_revision"], 1);
    assert_eq!(encoded["run_id"], json!(null));
    assert_eq!(encoded["control_version"], json!(null));
    assert_eq!(encoded["stop_mode"], json!(null));
    assert_eq!(encoded["control"], "prepared");
    assert_eq!(encoded["display"], "prepared");
    assert_eq!(encoded["execution_readiness"], "unavailable");
    assert_eq!(
        encoded["commands"],
        json!([
            {"command": "edit_definition", "rejection": null},
            {"command": "start", "rejection": "execution_unavailable"},
            {"command": "stop_immediate", "rejection": "invalid_state"},
            {"command": "stop_finish_stage", "rejection": "invalid_state"},
            {"command": "resume", "rejection": "execution_unavailable"},
            {"command": "prepare_retry", "rejection": "execution_unavailable"}
        ])
    );
    assert_eq!(
        encoded["nodes"][0],
        json!({
            "id": "a", "runtime": {
                "stage": "requirements", "disposition": "pending", "series": 1,
                "quality_failures": 0, "repair_cycles": 0, "review_rounds": 0,
                "current_attempt": null, "main_integrated": false, "acceptance_complete": false,
                "evidence_current": false, "wait_reason": null
            }, "direct_successors": ["b"], "ready_after_main": ["b"]
        })
    );
}

#[test]
fn failed_blocked_running_and_review_resource_wait_remain_distinct() {
    let mut state = running();
    state.nodes.get_mut("a").unwrap().disposition = NodeDisposition::Failed;
    state.nodes.get_mut("b").unwrap().disposition = NodeDisposition::Blocked;
    state.nodes.get_mut("c").unwrap().disposition = NodeDisposition::Blocked;
    let blocked = view(&state, now());
    assert_eq!(blocked.display, DisplayState::UnableToProgress);
    assert_eq!(blocked.failed_count, 1);
    assert_eq!(blocked.blocked_count, 2);
    assert!(
        blocked
            .reasons
            .iter()
            .any(|r| r.code == ReasonCode::PredecessorFailed && r.target == "b")
    );
    state.nodes.get_mut("c").unwrap().disposition = NodeDisposition::Running;
    assert_eq!(view(&state, now()).display, DisplayState::Running);
    let review = state.nodes.get_mut("c").unwrap();
    review.stage = Stage::Review;
    review.wait_reason = Some(ReasonCode::ReviewResources);
    let waiting = view(&state, now());
    assert_eq!(waiting.display, DisplayState::Waiting);
    assert!(
        waiting
            .reasons
            .iter()
            .any(|r| r.code == ReasonCode::ReviewResources && r.target == "c")
    );
}

#[test]
fn planned_or_claimed_work_is_not_displayed_as_actual_execution() {
    for phase in [ActionState::Planned, ActionState::Claimed] {
        let mut state = with_action(phase, false);
        state.nodes.get_mut("a").unwrap().disposition = NodeDisposition::Assigned;
        state.nodes.get_mut("a").unwrap().wait_reason = Some(ReasonCode::HostResources);
        assert_eq!(view(&state, now()).display, DisplayState::Waiting);
    }
    let state = with_action(ActionState::StartPending, false);
    let projected = view(&state, now());
    assert_eq!(projected.display, DisplayState::Running);
    assert!(projected.unknown);
}

#[test]
fn main_integration_does_not_imply_final_acceptance_or_campaign_success() {
    let mut state = running();
    let node = state.nodes.get_mut("a").unwrap();
    node.main_integrated = true;
    node.stage = Stage::FinalVerification;
    node.disposition = NodeDisposition::Verifying;
    let projected = view(&state, now());
    let a = projected.nodes.iter().find(|n| n.id == "a").unwrap();
    assert!(a.runtime.main_integrated);
    assert!(!a.runtime.acceptance_complete);
    assert_eq!(projected.display, DisplayState::Running);
    state.control = CampaignControl::Succeeded;
    assert_eq!(view(&state, now()).display, DisplayState::NeedsInspection);
    for node in state.nodes.values_mut() {
        node.main_integrated = true;
        node.acceptance_complete = true;
        node.disposition = NodeDisposition::Complete;
        node.stage = Stage::Complete;
    }
    assert_eq!(view(&state, now()).display, DisplayState::Succeeded);
    state.control = CampaignControl::Running;
    assert_ne!(view(&state, now()).display, DisplayState::Succeeded);
}

#[test]
fn stop_requested_confirmed_and_stopped_serialize_separately_with_unknown_lock() {
    let requested = stop(
        &with_action(ActionState::Started, true),
        StopMode::Immediate,
    );
    let projected = view(&requested, now());
    assert_eq!(projected.display, DisplayState::Stopping);
    assert_eq!(projected.stop_mode, Some(StopMode::Immediate));
    assert!(!projected.stop_confirmed);
    let target = requested.run.as_ref().unwrap().actions["action"]
        .target
        .clone();
    let acknowledged = step(
        &requested,
        Command::AcknowledgeFence {
            target,
            control_version: 2,
        },
    );
    let confirmed = step(&acknowledged, Command::ConfirmStopFence);
    let projected = view(&confirmed, now());
    assert!(projected.stop_confirmed);
    assert_eq!(projected.control, CampaignControl::Stopping);
    assert_eq!(
        rejection(&confirmed, PublicCommand::EditDefinition),
        Some(RejectionCode::InvalidState)
    );
    let reconciled = step(
        &confirmed,
        Command::ObserveAction {
            action_id: "action".into(),
            binding: Box::new(binding(&confirmed)),
            observation: ActionObservation::Reconciled {
                evidence: "actual effect".into(),
            },
        },
    );
    let stopped = step(&reconciled, Command::ConfirmStopped);
    assert_eq!(view(&stopped, now()).display, DisplayState::Stopped);
    assert_eq!(rejection(&stopped, PublicCommand::EditDefinition), None);
    let mut missing = stopped.clone();
    missing.run.as_mut().unwrap().sessions.clear();
    assert!(view(&missing, now()).unknown);
    assert_eq!(
        rejection(&missing, PublicCommand::EditDefinition),
        Some(RejectionCode::SessionsNotGone)
    );
    let mut unknown = stopped.clone();
    unknown.run.as_mut().unwrap().sessions.push(SessionRecord {
        identity: binding(&stopped).session,
        attempt_id: "attempt".into(),
        observation: SessionObservation::Unknown {
            reason: "host unreachable".into(),
            last_observed_at: None,
        },
    });
    let projected = view(&unknown, now());
    assert!(projected.unknown);
    assert_eq!(
        rejection(&unknown, PublicCommand::EditDefinition),
        Some(RejectionCode::SessionsNotGone)
    );
    assert!(
        projected
            .reasons
            .iter()
            .any(|r| r.code == ReasonCode::ObservationUnknown)
    );
    assert_eq!(
        serde_json::to_value(projected).unwrap()["sessions"][0]["observation"]["state"],
        "unknown"
    );
}

#[test]
fn successors_distinguish_edges_from_dependency_and_resource_eligibility() {
    let mut state = prepared();
    let mut definition = input();
    definition.nodes[1].dependencies.push("c".into());
    state.definition = validate_definition(definition).unwrap();
    let projected = view(&state, now());
    let a = projected.nodes.iter().find(|n| n.id == "a").unwrap();
    assert_eq!(a.direct_successors, ["b"]);
    assert!(a.ready_after_main.is_empty());
    state.nodes.get_mut("c").unwrap().main_integrated = true;
    assert_eq!(view(&state, now()).nodes[0].ready_after_main, ["b"]);
    state.nodes.get_mut("b").unwrap().wait_reason = Some(ReasonCode::HostResources);
    assert!(view(&state, now()).nodes[0].ready_after_main.is_empty());
    assert_eq!(view(&state, now()).nodes[0].direct_successors, ["b"]);
}

#[test]
fn event_growth_keeps_definition_and_stop_availability_stable() {
    let before = running();
    let mut after = before.clone();
    after.event_sequence += 20;
    after.last_event_at = Some(now() + chrono::Duration::seconds(1));
    let before_view = view(&before, now());
    let after_view = view(&after, now());
    assert_eq!(
        before_view.definition_revision,
        after_view.definition_revision
    );
    assert_eq!(before_view.run_id, after_view.run_id);
    assert_eq!(before_view.control_version, after_view.control_version);
    assert_eq!(before_view.commands, after_view.commands);
    assert_ne!(
        before_view.last_event_sequence,
        after_view.last_event_sequence
    );
    for command in [
        PublicCommand::Start,
        PublicCommand::Resume,
        PublicCommand::PrepareRetry,
    ] {
        assert_eq!(
            rejection(&after, command),
            Some(RejectionCode::ExecutionUnavailable)
        );
    }
    let result = control::decide(
        &after,
        &envelope(Command::RequestStop {
            expected_revision: 1,
            expected_run: "run".into(),
            mode: StopMode::FinishStage,
        }),
    )
    .unwrap();
    assert_eq!(result.state.control, CampaignControl::Stopping);
}
