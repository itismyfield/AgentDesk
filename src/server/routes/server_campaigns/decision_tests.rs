use serde_json::{Value, json};

use super::dto::CommandRequest;

fn approval_request() -> Value {
    json!({
        "idempotency_key": "approval-one",
        "command": {
            "command": "approve", "expected_definition_revision": 1, "expected_run": "run",
            "node_id": "a", "gate": "review",
            "target": {"kind": "candidate", "base_sha": "a".repeat(40), "head_sha": "b".repeat(40)},
            "policy_hash": "c".repeat(64),
            "evidence": {
                "decision_ref": "i7438-m1-approval",
                "response_ref": "https://github.com/itismyfield/AgentDesk/discussions/7438#discussioncomment-1",
                "response_at": "2026-10-28T06:00:00Z"
            }
        }
    })
}

#[test]
fn public_approve_maps_every_flat_binding_field_to_the_domain_command() {
    let wire = approval_request();
    let request: CommandRequest = serde_json::from_value(wire.clone()).unwrap();
    let command = serde_json::to_value(request.command.into_domain().unwrap()).unwrap();
    assert_eq!(
        command,
        json!({
            "command": "approve_gate",
            "binding": {
                "run_id": "run", "node_id": "a", "gate": "review", "definition_revision": 1,
                "target": wire["command"]["target"], "policy_hash": "c".repeat(64)
            },
            "evidence": wire["command"]["evidence"]
        })
    );
}

#[test]
fn public_approval_keeps_internal_open_and_forged_source_fields_out_of_the_dto() {
    let mut internal = approval_request();
    internal["command"]["command"] = json!("open_approval_gate");
    assert!(serde_json::from_value::<CommandRequest>(internal).is_err());

    let mut extra_command_field = approval_request();
    extra_command_field["command"]["actor"] = json!("forum-user");
    assert!(serde_json::from_value::<CommandRequest>(extra_command_field).is_err());

    let mut forged_source = approval_request();
    forged_source["command"]["evidence"]["source"] = json!("server_authenticated_forum");
    assert!(serde_json::from_value::<CommandRequest>(forged_source).is_err());

    let mut mixed_target = approval_request();
    mixed_target["command"]["target"]["number"] = json!(7438);
    assert!(serde_json::from_value::<CommandRequest>(mixed_target).is_err());
}
