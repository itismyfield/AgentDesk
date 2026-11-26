use serde_json::{Value, json};

use crate::domain::campaign::definition;
use crate::domain::campaign::model::{
    CampaignState, ChangeReason, Command, Decision, RejectionCode,
};
use crate::services::campaign_execution::control::{self, tests as domain};

fn options(required: bool) -> Value {
    json!({
        "start": required, "implementation": required, "review": required,
        "landing": required, "merge": required, "deploy": required
    })
}

fn configured(mut state: CampaignState) -> CampaignState {
    let mut input = serde_json::to_value(state.definition.input()).unwrap();
    input["workflow_options"] = options(true);
    state.definition =
        definition::validate_definition(serde_json::from_value(input).unwrap()).unwrap();
    state
}

fn target(gate: &str, head: char) -> Value {
    let sha = head.to_string().repeat(40);
    match gate {
        "merge" => json!({"kind": "pull_request", "number": 7438, "head_sha": sha}),
        "deploy" => json!({"kind": "main", "sha": sha}),
        _ => json!({"kind": "candidate", "base_sha": "a".repeat(40), "head_sha": sha}),
    }
}

fn binding(state: &CampaignState, gate: &str) -> Value {
    json!({
        "run_id": state.run.as_ref().unwrap().id,
        "node_id": "a", "gate": gate,
        "definition_revision": state.definition_revision,
        "target": target(gate, 'b'), "policy_hash": "c".repeat(64)
    })
}

fn evidence() -> Value {
    json!({
        "decision_ref": "i7438-m1-approval",
        "response_ref": "https://github.com/itismyfield/AgentDesk/discussions/7438#discussioncomment-1",
        "response_at": domain::now()
    })
}

fn open(binding: &Value) -> Command {
    serde_json::from_value(json!({"command": "open_approval_gate", "binding": binding})).unwrap()
}

fn approve(binding: &Value, evidence: &Value) -> Command {
    serde_json::from_value(json!({
        "command": "approve_gate", "binding": binding, "evidence": evidence
    }))
    .unwrap()
}

fn apply(state: &CampaignState, command: Command, key: &str) -> Decision {
    let mut envelope = domain::envelope(command);
    envelope.idempotency_key = key.into();
    control::decide(state, &envelope).unwrap()
}

fn book(state: &CampaignState) -> Value {
    serde_json::to_value(state.run.as_ref().unwrap()).unwrap()["approvals"].clone()
}

#[test]
fn six_slots_default_off_and_open_independently_when_required() {
    let mut default = domain::running();
    let mut legacy = serde_json::to_value(default.definition.input()).unwrap();
    legacy.as_object_mut().unwrap().remove("workflow_options");
    default.definition =
        definition::validate_definition(serde_json::from_value(legacy).unwrap()).unwrap();
    let mut required = configured(domain::running());
    for gate in [
        "start",
        "implementation",
        "review",
        "landing",
        "merge",
        "deploy",
    ] {
        let requested = binding(&default, gate);
        assert!(control::decide(&default, &domain::envelope(open(&requested))).is_err());
        let opened = apply(&required, open(&requested), &format!("open-{gate}"));
        assert_eq!(opened.events.len(), 1, "{gate}");
        required = opened.state;
    }
    assert_eq!(book(&required)["current"].as_object().unwrap().len(), 6);
    assert_eq!(book(&required)["items"].as_object().unwrap().len(), 6);
}

#[test]
fn approve_requires_every_binding_coordinate_to_match_the_current_gate() {
    let state = configured(domain::running());
    let requested = binding(&state, "review");
    let opened = apply(&state, open(&requested), "open").state;
    let mut changed = Vec::new();
    for (field, value) in [
        ("run_id", json!("other-run")),
        ("node_id", json!("b")),
        ("gate", json!("implementation")),
        ("definition_revision", json!(state.definition_revision + 1)),
        ("policy_hash", json!("d".repeat(64))),
    ] {
        let mut other = requested.clone();
        other[field] = value;
        changed.push((field, other));
    }
    for (field, value) in [
        (
            "kind",
            json!({"kind": "pull_request", "number": 7438, "head_sha": "b".repeat(40)}),
        ),
        (
            "base_sha",
            json!({"kind": "candidate", "base_sha": "d".repeat(40), "head_sha": "b".repeat(40)}),
        ),
        ("head_sha", target("review", 'd')),
    ] {
        let mut other = requested.clone();
        other["target"] = value;
        changed.push((field, other));
    }
    assert_eq!(changed.len(), 8);
    for (field, other) in changed {
        assert!(
            control::decide(&opened, &domain::envelope(approve(&other, &evidence()))).is_err(),
            "accepted a changed {field}"
        );
    }
    assert_eq!(
        apply(&opened, approve(&requested, &evidence()), "approve")
            .events
            .len(),
        1
    );
}

#[test]
fn same_gate_repeated_approval_preserves_the_first_record_without_another_event() {
    let state = configured(domain::running());
    let requested = binding(&state, "review");
    let opened = apply(&state, open(&requested), "open").state;
    let approved = apply(&opened, approve(&requested, &evidence()), "approve-first");
    assert_eq!(approved.events.len(), 1);
    let stored = book(&approved.state);
    let item = stored["items"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(item["approval"]["source"], "coordinator_recorded_forum");
    assert_eq!(item["approval"]["evidence"], evidence());
    assert_eq!(item["approval"]["recorded_at"], json!(domain::now()));

    let reopened = apply(&approved.state, open(&requested), "open-again");
    assert!(reopened.events.is_empty());
    assert_eq!(reopened.state, approved.state);
    let mut retry = domain::envelope(approve(&requested, &evidence()));
    retry.idempotency_key = "approve-second-key".into();
    retry.observed_at += chrono::Duration::minutes(5);
    let repeated = control::decide(&approved.state, &retry).unwrap();
    assert!(repeated.events.is_empty());
    assert_eq!(repeated.state, approved.state);
}

#[test]
fn target_change_blocks_old_approval_and_reopening_a_keeps_its_full_key_history() {
    let state = configured(domain::running());
    let a = binding(&state, "review");
    let opened_a = apply(&state, open(&a), "open-a").state;
    let approved_a = apply(&opened_a, approve(&a, &evidence()), "approve-a").state;
    let old_book = book(&approved_a);
    let (a_id, a_item) = old_book["items"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let mut b = a.clone();
    b["target"] = target("review", 'd');
    let opened_b = apply(&approved_a, open(&b), "open-b").state;
    assert!(control::decide(&opened_b, &domain::envelope(approve(&a, &evidence()))).is_err());
    let approved_b = apply(&opened_b, approve(&b, &evidence()), "approve-b").state;
    let reopened_a = apply(&approved_b, open(&a), "reopen-a");
    assert_eq!(reopened_a.events.len(), 1);
    assert_eq!(
        book(&reopened_a.state)["current"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        book(&reopened_a.state)["items"].as_object().unwrap().len(),
        2
    );
    assert_eq!(&book(&reopened_a.state)["items"][a_id], a_item);
    let repeated = apply(
        &reopened_a.state,
        approve(&a, &evidence()),
        "approve-a-again",
    );
    assert!(repeated.events.is_empty());
    assert_eq!(repeated.state, reopened_a.state);
}

#[test]
fn a_changed_forum_response_conflicts_with_an_approved_gate() {
    let state = configured(domain::running());
    let requested = binding(&state, "review");
    let opened = apply(&state, open(&requested), "open").state;
    let approved = apply(&opened, approve(&requested, &evidence()), "approve").state;
    for (field, value) in [
        ("decision_ref", json!("different-decision")),
        (
            "response_ref",
            json!("https://github.com/itismyfield/AgentDesk/discussions/7438#discussioncomment-2"),
        ),
        (
            "response_at",
            json!(domain::now() + chrono::Duration::minutes(1)),
        ),
    ] {
        let mut changed = evidence();
        changed[field] = value;
        let refused = control::decide(&approved, &domain::envelope(approve(&requested, &changed)))
            .unwrap_err();
        assert_eq!(refused.code, RejectionCode::IdempotencyConflict, "{field}");
    }
}

#[test]
fn fresh_approval_rechecks_the_current_run_and_revision_even_after_approval() {
    let state = configured(domain::running());
    let requested = binding(&state, "review");
    let opened = apply(&state, open(&requested), "open").state;
    let approved = apply(&opened, approve(&requested, &evidence()), "approve").state;
    let mut edited = approved.clone();
    edited.definition_revision += 1;
    assert!(control::decide(&edited, &domain::envelope(approve(&requested, &evidence()))).is_err());
    let mut replaced = approved;
    replaced.run.as_mut().unwrap().id = "another-run".into();
    assert!(
        control::decide(
            &replaced,
            &domain::envelope(approve(&requested, &evidence()))
        )
        .is_err()
    );
}

#[test]
fn omitted_workflow_edit_cannot_switch_off_configured_gates_but_explicit_off_can() {
    let state = configured(domain::prepared());
    let change = Some(ChangeReason {
        reference: "https://github.com/itismyfield/AgentDesk/discussions/7438".into(),
        explanation: "Explicitly update the definition after user review".into(),
    });
    let mut legacy = serde_json::to_value(domain::input()).unwrap();
    legacy.as_object_mut().unwrap().remove("workflow_options");
    let omitted = Command::EditDefinition {
        expected_revision: state.definition_revision,
        definition: serde_json::from_value(legacy).unwrap(),
        change: change.clone(),
    };
    assert!(control::decide(&state, &domain::envelope(omitted)).is_err());
    let mut explicit = serde_json::to_value(domain::input()).unwrap();
    explicit["workflow_options"] = options(false);
    let edited = apply(
        &state,
        Command::EditDefinition {
            expected_revision: state.definition_revision,
            definition: serde_json::from_value(explicit).unwrap(),
            change,
        },
        "explicit-options",
    );
    assert_eq!(
        edited.state.definition_revision,
        state.definition_revision + 1
    );
    assert_eq!(
        serde_json::to_value(edited.state.definition.input()).unwrap()["workflow_options"],
        options(false)
    );
}
