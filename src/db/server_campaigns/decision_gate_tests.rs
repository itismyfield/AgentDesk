use crate::db::server_campaigns::*;
use crate::domain::campaign::definition::DefinitionInput;
use crate::domain::campaign::model::*;
use crate::services::campaign_execution::control::{self, test_support as domain};
use crate::services::krx_db_tests::server_campaigns_storage::{
    Harness, applied, envelope, execute, render, send,
};
use serde_json::{Value, json};

fn options(implementation: bool) -> Value {
    json!({
        "start": false, "implementation": implementation, "review": false,
        "landing": false, "merge": false, "deploy": false
    })
}

fn definition_with_options(implementation: bool) -> DefinitionInput {
    let mut input = serde_json::to_value(domain::input()).unwrap();
    input["workflow_options"] = options(implementation);
    serde_json::from_value(input).unwrap()
}

fn command(value: Value) -> Command {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn concurrent_distinct_keys_accept_one_approval_and_restore_book_pg() {
    let mut h = Harness::new().await;
    let mut state = domain::running();
    state.definition =
        crate::domain::campaign::definition::validate_definition(definition_with_options(true))
            .unwrap();
    h.seed(state).await;
    let binding = json!({
        "run_id": "run", "node_id": "a", "gate": "implementation",
        "definition_revision": 1,
        "target": {"kind": "candidate", "base_sha": "a".repeat(40), "head_sha": "b".repeat(40)},
        "policy_hash": "c".repeat(64)
    });
    applied(
        send(
            &h.pool,
            "open",
            command(json!({
                "command": "open_approval_gate", "binding": binding
            })),
        )
        .await,
    );
    let evidence = json!({
        "decision_ref": "i7438-minimal-approve-1118",
        "response_ref": "https://example.com/forum/responses/approval",
        "response_at": domain::now()
    });
    let approve = command(json!({
        "command": "approve_gate", "binding": binding, "evidence": evidence
    }));
    let first = envelope("approve-a", approve.clone());
    let mut second = envelope("approve-b", approve);
    second.observed_at += chrono::Duration::seconds(1);
    let (a, b) = tokio::join!(
        execute(&h.pool, "campaign", &first),
        execute(&h.pool, "campaign", &second)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(!a.replayed && !b.replayed);
    assert_eq!(a.outcome, b.outcome);
    assert_eq!(a.response, b.response);
    applied(a.outcome.clone());
    let stored = get(&h.pool, "campaign").await.unwrap().unwrap();
    let stored_run = serde_json::to_value(stored.run.as_ref().unwrap()).unwrap();
    let items = stored_run["approvals"]["items"].as_object().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        stored_run["approvals"]["current"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    let item = items.values().next().unwrap();
    assert_eq!(item["binding"], binding);
    assert_eq!(item["approval"]["source"], "coordinator_recorded_forum");
    assert_eq!(item["approval"]["evidence"], evidence);
    let saved_events = events(&h.pool, "campaign", None, 0, 100).await.unwrap();
    let accepted: Vec<_> = saved_events
        .iter()
        .filter(|event| {
            serde_json::to_value(&event.kind).unwrap()["event"] == "approval_gate_approved"
        })
        .collect();
    assert_eq!(saved_events.len(), 3);
    assert_eq!(accepted.len(), 1);
    assert_eq!(
        item["approval"]["recorded_at"],
        json!(accepted[0].observed_at)
    );
    h.reconnect().await;
    assert_eq!(get(&h.pool, "campaign").await.unwrap(), Some(stored));

    let mut changed = binding;
    changed["target"]["head_sha"] = json!("d".repeat(40));
    applied(
        send(
            &h.pool,
            "new-candidate",
            command(json!({
                "command": "open_approval_gate", "binding": changed
            })),
        )
        .await,
    );
    let retry = execute(&h.pool, "campaign", &first).await.unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.outcome, a.outcome);
    assert_eq!(retry.response, a.response);
    assert_eq!(
        events(&h.pool, "campaign", None, 0, 100)
            .await
            .unwrap()
            .len(),
        4
    );
    h.close().await;
}

#[tokio::test]
async fn legacy_edit_success_replays_before_missing_options_guard_pg() {
    use crate::db::server_campaigns::command_transaction::{Preparation, StoreRequest};
    use crate::domain::campaign::definition::validate_definition;

    let h = Harness::new().await;
    let state = domain::prepared();
    h.seed(state.clone()).await;
    let mut legacy = serde_json::to_value(domain::input()).unwrap();
    legacy.as_object_mut().unwrap().remove("workflow_options");
    legacy["title"] = json!("Previously accepted legacy edit");
    let legacy_input: DefinitionInput = serde_json::from_value(legacy.clone()).unwrap();
    let old_request = envelope(
        "legacy-success",
        Command::EditDefinition {
            expected_revision: 1,
            definition: legacy_input.clone(),
            change: None,
        },
    );
    let mut explicit = legacy;
    explicit["workflow_options"] = options(false);
    let mut historical = control::decide(
        &state,
        &envelope(
            "legacy-success",
            Command::EditDefinition {
                expected_revision: 1,
                definition: serde_json::from_value(explicit).unwrap(),
                change: None,
            },
        ),
    )
    .unwrap();
    // Restore the successful edit and receipt that the old decoder persisted without the new field.
    historical.state.definition = validate_definition(legacy_input.clone()).unwrap();
    let prepared = command_transaction::prepare(
        h.pool.begin().await.unwrap(),
        "campaign",
        &old_request.idempotency_key,
        old_request.observed_at,
        StoreRequest::Command(old_request.command.clone()),
    )
    .await
    .unwrap();
    let Preparation::Fresh(prepared) = prepared else {
        panic!("expected empty receipt fixture")
    };
    let staged = prepared.stage(Some(Ok(historical))).await.unwrap();
    let response = render(staged.outcome()).unwrap();
    let (tx, original) = staged.record_receipt(response).await.unwrap();
    tx.commit().await.unwrap();
    applied(original.outcome.clone());

    let mut current_input = definition_with_options(true);
    current_input.title = "Current enabled gate".into();
    applied(
        send(
            &h.pool,
            "current-edit",
            Command::EditDefinition {
                expected_revision: 2,
                definition: current_input,
                change: None,
            },
        )
        .await,
    );
    let current = get(&h.pool, "campaign").await.unwrap().unwrap();
    assert_eq!(current.definition_revision, 3);
    let fresh = execute(
        &h.pool,
        "campaign",
        &envelope(
            "fresh-legacy",
            Command::EditDefinition {
                expected_revision: 3,
                definition: legacy_input,
                change: None,
            },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(
        fresh.outcome,
        CommandOutcome::Decided(Err(Rejection {
            code: RejectionCode::InvalidDefinition,
            ..
        }))
    ));
    assert!(!fresh.replayed);
    let retry = execute(&h.pool, "campaign", &old_request).await.unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.outcome, original.outcome);
    assert_eq!(retry.response, original.response);
    assert_eq!(retry.response.status, 200);
    assert_eq!(get(&h.pool, "campaign").await.unwrap(), Some(current));
    assert_eq!(
        events(&h.pool, "campaign", None, 0, 100)
            .await
            .unwrap()
            .len(),
        3
    );
    h.close().await;
}
