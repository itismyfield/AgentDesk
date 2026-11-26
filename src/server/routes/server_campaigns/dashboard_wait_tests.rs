use super::*;
use crate::domain::campaign::decision_item::ApprovalBinding;

#[tokio::test]
async fn dashboard_v3_gate_projection_is_bound_to_the_view_sequence_pg() {
    let h = Harness::new().await;
    let mut state = domain::running();
    let mut definition = state.definition.input().clone();
    definition.workflow_options.as_mut().unwrap().start = true;
    state.definition =
        crate::domain::campaign::definition::validate_definition(definition).unwrap();
    h.seed_running_state(&state).await;
    let detail_path = "/server-campaigns/campaign";
    let node_path = "/campaign/view/nodes/2/runtime";
    let (status, detail) = request(&h.app, "GET", detail_path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail.pointer(node_path).unwrap()["pending_gate"],
        json!({"gate": "start", "binding": null})
    );
    assert_eq!(
        detail.pointer(node_path).unwrap()["human_status"],
        "decision_required"
    );
    assert_eq!(
        detail["campaign"]["view"]["nodes"][1]["runtime"]["human_status"],
        Value::Null
    );

    let mut binding = json!({
        "run_id": "run", "node_id": "c", "gate": "start", "definition_revision": 1,
        "target": {"kind": "candidate", "base_sha": "a".repeat(40), "head_sha": "b".repeat(40)},
        "policy_hash": "c".repeat(64)
    });
    let open = |label: &'static str, binding: Value, observed_at| {
        let binding: ApprovalBinding = serde_json::from_value(binding).unwrap();
        let envelope = CommandEnvelope {
            idempotency_key: label.into(),
            observed_at,
            command: Command::OpenApprovalGate {
                binding: Box::new(binding),
            },
        };
        let pool = &h.pool;
        async move {
            let reply = commands::execute(pool, "campaign", &envelope, protocol::commanded)
                .await
                .unwrap();
            assert_eq!(reply.response.status, 202, "{:?}", reply.response.body);
        }
    };
    let first_at = domain::now() + chrono::Duration::nanoseconds(123);
    open("first-open", binding.clone(), first_at).await;
    let saved = store::get(&h.pool, "campaign").await.unwrap().unwrap();

    let first_binding = binding.clone();
    binding["target"]["head_sha"] = json!("d".repeat(40));
    let other_at = domain::now() + chrono::Duration::seconds(2);
    open("other-open", binding.clone(), other_at).await;
    let (_, list) = request(&h.app, "GET", "/server-campaigns", None).await;
    let runtime = &list["campaigns"][0]["view"]["nodes"][2]["runtime"];
    assert_eq!(runtime["pending_gate"]["binding"], binding);
    assert_eq!(runtime["wait_started_at"], json!(other_at));

    let reopened_at = domain::now() + chrono::Duration::seconds(3);
    open("reopen-original", first_binding.clone(), reopened_at).await;
    let (_, detail) = request(&h.app, "GET", detail_path, None).await;
    let current = detail.pointer(node_path).unwrap();
    assert_eq!(current["human_status"], "awaiting_approval");
    assert_eq!(current["pending_gate"]["binding"], first_binding);
    assert_eq!(current["wait_started_at"], json!(reopened_at));
    assert_eq!(current.get("evidence_url"), Some(&Value::Null));
    assert_eq!(current.get("reminder_interval_seconds"), Some(&Value::Null));

    let mut old_view = view::view(&saved, Utc::now());
    fill_approval_opening_times(&h.pool, std::slice::from_mut(&mut old_view))
        .await
        .unwrap();
    assert_eq!(old_view.nodes[2].runtime.wait_started_at, Some(first_at));
    old_view.last_event_sequence -= 1;
    old_view.nodes[2].runtime.wait_started_at = None;
    fill_approval_opening_times(&h.pool, std::slice::from_mut(&mut old_view))
        .await
        .unwrap();
    assert_eq!(old_view.nodes[2].runtime.wait_started_at, None);

    let mut approval = json!({
        "command": "approve", "expected_definition_revision": 1, "expected_run": "run",
        "node_id": "c", "gate": "start", "target": first_binding["target"],
        "policy_hash": first_binding["policy_hash"], "evidence": {
            "decision_ref": "release-start",
            "response_ref": "https://forum.example/decisions/release-start#response-2",
            "response_at": domain::now()
        }
    });
    let approve_path = "/server-campaigns/campaign/commands";
    for (field, invalid) in [
        ("decision_ref", " "),
        ("response_ref", "http://forum.example/r/2"),
    ] {
        let mut rejected = approval.clone();
        rejected["evidence"][field] = json!(invalid);
        assert_eq!(
            request(
                &h.app,
                "POST",
                approve_path,
                Some(command_body(field, rejected))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut missing_time = approval.clone();
    missing_time["evidence"]
        .as_object_mut()
        .unwrap()
        .remove("response_at");
    assert_eq!(
        request(
            &h.app,
            "POST",
            approve_path,
            Some(command_body("missing-time", missing_time))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    approval["evidence"]["response_at"] = json!(reopened_at);
    assert_eq!(
        request(
            &h.app,
            "POST",
            approve_path,
            Some(command_body("approve", approval))
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    let (_, detail) = request(&h.app, "GET", detail_path, None).await;
    let after = detail.pointer(node_path).unwrap();
    assert_eq!(after["human_status"], "awaiting_start");
    assert_eq!(after["pending_gate"], Value::Null);
    assert_eq!(after["wait_started_at"], Value::Null);
    assert!(!detail.to_string().contains("forum.example"));
    h.close().await;
}
