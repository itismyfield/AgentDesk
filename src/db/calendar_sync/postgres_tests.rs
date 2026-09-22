use super::*;
use serde_json::json;

async fn fixture() -> (
    crate::dispatch::test_support::DispatchPostgresTestDb,
    PgPool,
    Vec<Binding>,
) {
    let db =
        crate::dispatch::test_support::DispatchPostgresTestDb::create("calendar", "calendar sync")
            .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let a = bind_account(&pool, "default", 10, 100).await.unwrap();
    let b = bind_account(&pool, "friend", 10, 200).await.unwrap();
    (db, pool, vec![a, b])
}
fn content() -> Value {
    json!({"title":"meeting","time":{"startAt":"2026-09-30T10:00:00+09:00","endAt":"2026-09-30T11:00:00+09:00","timeZone":"Asia/Seoul"}})
}
fn accounts() -> Vec<String> {
    vec!["default".into(), "friend".into()]
}

#[tokio::test]
async fn managed_list_filters_all_accounts_and_preserves_status_pg() {
    let (db, pool, bindings) = fixture().await;
    let own = create(&pool, "own", "own", &content(), &bindings[..1])
        .await
        .unwrap();
    create(&pool, "both", "both", &content(), &bindings)
        .await
        .unwrap();
    create(&pool, "friend", "friend", &content(), &bindings[1..])
        .await
        .unwrap();
    let own_list = list(&pool, &["default".into()], None, 10).await.unwrap();
    assert_eq!(own_list, vec![get(&pool, own.event_id).await.unwrap()]);
    assert!(list(&pool, &[], None, 10).await.unwrap().is_empty());
    let all = list(&pool, &accounts(), None, 10).await.unwrap();
    assert_eq!(all.len(), 3);
    let first_page = list(&pool, &accounts(), None, 1).await.unwrap();
    let cursor = serde_json::from_value(first_page[0]["eventId"].clone()).unwrap();
    let rest = list(&pool, &accounts(), Some(cursor), 10).await.unwrap();
    assert_eq!([first_page, rest].concat(), all);
    for _ in 0..4 {
        let c = claim(&pool, &accounts()).await.unwrap().unwrap();
        assert!(dispatch(&pool, &c).await.unwrap());
        complete(&pool, &c, Some(&c.target_id.to_string()))
            .await
            .unwrap();
    }
    let mut changed = content();
    changed["title"] = json!("next revision");
    mutate(
        &pool,
        Mutation {
            event: own.event_id,
            key: "update",
            fingerprint: "update",
            expected_revision: 1,
            content: &changed,
            delete: false,
        },
    )
    .await
    .unwrap();
    let current = get(&pool, own.event_id).await.unwrap();
    assert_eq!(current["revision"], 2);
    assert_eq!(current["targets"][0]["appliedRevision"], 1);
    assert_eq!(current["status"], "accepted");
    assert_eq!(
        list(&pool, &["default".into()], None, 10).await.unwrap(),
        vec![current]
    );
    let checks = account_checks(&pool, &["default".into(), "unbound".into()])
        .await
        .unwrap();
    assert!(!checks[0]["lastCheckedAt"].is_null());
    assert!(checks[1]["lastCheckedAt"].is_null());
    assert_eq!(checks[0]["credentialVerified"], false);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn two_account_crud_reaches_mock_provider_and_does_not_recreate_success_pg() {
    use crate::services::{calendar_sync::execute_for_test, kakao::test_support};
    use axum::{
        Json, Router,
        extract::{Form, Query},
        routing::{delete, post},
    };
    use std::collections::HashMap;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let creates = Arc::new(AtomicUsize::new(0));
    let updates = Arc::new(AtomicUsize::new(0));
    let deletes = Arc::new(AtomicUsize::new(0));
    let create_count = creates.clone();
    let update_count = updates.clone();
    let delete_count = deletes.clone();
    let router = Router::new()
        .route(
            "/v2/api/calendar/create/event",
            post(move |Form(form): Form<HashMap<String, String>>| {
                let count = create_count.clone();
                async move {
                    assert_eq!(form["calendar_id"], "primary");
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"event_id":format!("remote-{n}")}))
                }
            }),
        )
        .route(
            "/v2/api/calendar/update/event/host",
            post(move |Form(form): Form<HashMap<String, String>>| {
                let count = update_count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"event_id":form["event_id"]}))
                }
            }),
        )
        .route(
            "/v2/api/calendar/delete/event",
            delete(move |Query(query): Query<HashMap<String, String>>| {
                let count = delete_count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"event_id":query["event_id"]}))
                }
            }),
        );
    let (origin, task) = test_support::server(router).await;
    let (db, pool, bindings) = fixture().await;
    let receipt = create(&pool, "create", "fp", &content(), &bindings)
        .await
        .unwrap();
    let failed = claim(&pool, &accounts()).await.unwrap().unwrap();
    fail(&pool, &failed, "blocked", "consent_required")
        .await
        .unwrap();
    {
        let c = claim(&pool, &accounts()).await.unwrap().unwrap();
        assert_ne!(c.target_id, failed.target_id);
        assert!(dispatch(&pool, &c).await.unwrap());
        execute_for_test(&pool, &c, &test_support::client(&origin, &c.account_id))
            .await
            .unwrap();
    }
    assert_eq!(
        get(&pool, receipt.event_id).await.unwrap()["status"],
        "partial_success"
    );
    assert!(claim(&pool, &accounts()).await.unwrap().is_none());
    recover(
        &pool,
        &failed,
        RecoveryResolution::Retry,
        None,
        "Consent repaired and verified",
    )
    .await
    .unwrap();
    let retried = claim(&pool, &accounts()).await.unwrap().unwrap();
    assert_eq!(retried.target_id, failed.target_id);
    assert!(dispatch(&pool, &retried).await.unwrap());
    execute_for_test(
        &pool,
        &retried,
        &test_support::client(&origin, &retried.account_id),
    )
    .await
    .unwrap();
    create(&pool, "create", "fp", &content(), &bindings)
        .await
        .unwrap();
    assert!(claim(&pool, &accounts()).await.unwrap().is_none());
    let mut changed = content();
    changed["title"] = json!("new title");
    mutate(
        &pool,
        Mutation {
            event: receipt.event_id,
            key: "update",
            fingerprint: "update",
            expected_revision: 1,
            content: &changed,
            delete: false,
        },
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let c = claim(&pool, &accounts()).await.unwrap().unwrap();
        assert!(dispatch(&pool, &c).await.unwrap());
        execute_for_test(&pool, &c, &test_support::client(&origin, &c.account_id))
            .await
            .unwrap();
    }
    mutate(
        &pool,
        Mutation {
            event: receipt.event_id,
            key: "delete",
            fingerprint: "delete",
            expected_revision: 2,
            content: &changed,
            delete: true,
        },
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let c = claim(&pool, &accounts()).await.unwrap().unwrap();
        assert!(dispatch(&pool, &c).await.unwrap());
        execute_for_test(&pool, &c, &test_support::client(&origin, &c.account_id))
            .await
            .unwrap();
    }
    assert_eq!(creates.load(Ordering::SeqCst), 2);
    assert_eq!(updates.load(Ordering::SeqCst), 2);
    assert_eq!(deletes.load(Ordering::SeqCst), 2);
    let event = get(&pool, receipt.event_id).await.unwrap();
    assert_eq!(event["status"], "success");
    assert_eq!(event["content"], json!({}));
    task.abort();
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn durable_replay_two_account_crud_and_tombstone_pg() {
    let (db, pool, bindings) = fixture().await;
    let initial = content();
    let (first, repeated) = tokio::join!(
        create(&pool, "create-1", "fp1", &initial, &bindings),
        create(&pool, "create-1", "fp1", &initial, &bindings)
    );
    let first = first.unwrap();
    let repeated = repeated.unwrap();
    assert_eq!(first.event_id, repeated.event_id);
    assert!(matches!(
        create(&pool, "create-1", "different", &content(), &bindings).await,
        Err(CalendarDbError::Conflict)
    ));
    for remote in ["remote-a", "remote-b"] {
        let c = claim(&pool, &accounts()).await.unwrap().unwrap();
        assert!(dispatch(&pool, &c).await.unwrap());
        assert!(complete(&pool, &c, Some(remote)).await.unwrap());
    }
    assert_eq!(
        get(&pool, first.event_id).await.unwrap()["status"],
        "success"
    );
    let mut changed = content();
    changed["title"] = json!("changed");
    let updated = mutate(
        &pool,
        Mutation {
            event: first.event_id,
            key: "update-1",
            fingerprint: "fp2",
            expected_revision: 1,
            content: &changed,
            delete: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(
        mutate(
            &pool,
            Mutation {
                event: first.event_id,
                key: "update-1",
                fingerprint: "fp2",
                expected_revision: 1,
                content: &changed,
                delete: false
            }
        )
        .await
        .unwrap()
        .revision,
        2
    );
    assert!(matches!(
        mutate(
            &pool,
            Mutation {
                event: first.event_id,
                key: "racing",
                fingerprint: "fp3",
                expected_revision: 1,
                content: &changed,
                delete: false
            }
        )
        .await,
        Err(CalendarDbError::Conflict)
    ));
    for _ in 0..2 {
        let c = claim(&pool, &accounts()).await.unwrap().unwrap();
        assert!(c.remote_id.is_some());
        assert!(dispatch(&pool, &c).await.unwrap());
        complete(&pool, &c, c.remote_id.as_deref()).await.unwrap();
    }
    mutate(
        &pool,
        Mutation {
            event: first.event_id,
            key: "delete-1",
            fingerprint: "fp4",
            expected_revision: 2,
            content: &changed,
            delete: true,
        },
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let c = claim(&pool, &accounts()).await.unwrap().unwrap();
        assert_eq!(c.action, "delete");
        assert!(dispatch(&pool, &c).await.unwrap());
        complete(&pool, &c, None).await.unwrap();
    }
    let event = get(&pool, first.event_id).await.unwrap();
    assert_eq!(event["deleted"], true);
    assert_eq!(event["status"], "success");
    assert!(claim(&pool, &accounts()).await.unwrap().is_none());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn unknown_create_blocks_only_its_target_and_accepts_late_evidence_pg() {
    let (db, pool, bindings) = fixture().await;
    let receipt = create(&pool, "create", "fp", &content(), &bindings)
        .await
        .unwrap();
    let c = claim(&pool, &accounts()).await.unwrap().unwrap();
    assert!(dispatch(&pool, &c).await.unwrap());
    sqlx::query("UPDATE kakao_calendar_operations SET lease_expires_at=NOW()-INTERVAL '1 second' WHERE id=$1").bind(c.id).execute(&pool).await.unwrap();
    // Credential unavailability must not hide an interrupted write or claim
    // the other account's queued operation.
    assert!(claim(&pool, &[]).await.unwrap().is_none());
    assert_eq!(
        get(&pool, receipt.event_id).await.unwrap()["status"],
        "unknown"
    );
    assert_eq!(
        recovery_target(&pool, receipt.event_id, c.id)
            .await
            .unwrap()
            .id,
        c.id
    );
    let other = claim(&pool, &accounts()).await.unwrap().unwrap();
    assert_ne!(c.target_id, other.target_id);
    let mut stale = recovery_target(&pool, receipt.event_id, c.id)
        .await
        .unwrap();
    stale.claim_token = Uuid::new_v4();
    assert!(matches!(
        recover(
            &pool,
            &stale,
            RecoveryResolution::ConfirmNotApplied,
            None,
            "stale recovery must not apply"
        )
        .await,
        Err(CalendarDbError::Conflict)
    ));
    assert!(matches!(
        recover(
            &pool,
            &c,
            RecoveryResolution::Retry,
            None,
            "unknown write cannot be retried"
        )
        .await,
        Err(CalendarDbError::Conflict)
    ));
    assert!(dispatch(&pool, &other).await.unwrap());
    complete(&pool, &other, Some("other")).await.unwrap();
    assert_eq!(
        get(&pool, receipt.event_id).await.unwrap()["status"],
        "unknown"
    );
    mutate(
        &pool,
        Mutation {
            event: receipt.event_id,
            key: "delete",
            fingerprint: "del",
            expected_revision: 1,
            content: &content(),
            delete: true,
        },
    )
    .await
    .unwrap();
    let delete = claim(&pool, &accounts()).await.unwrap().unwrap();
    assert_eq!(delete.target_id, other.target_id);
    assert!(dispatch(&pool, &delete).await.unwrap());
    complete(&pool, &delete, None).await.unwrap();
    assert!(claim(&pool, &accounts()).await.unwrap().is_none());
    assert!(complete(&pool, &c, Some("late-created")).await.unwrap());
    let final_delete = claim(&pool, &accounts()).await.unwrap().unwrap();
    assert_eq!(final_delete.remote_id.as_deref(), Some("late-created"));
    assert_eq!(final_delete.action, "delete");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn dispatch_checks_lease_revision_and_binding_and_enqueue_rolls_back_pg() {
    let (db, pool, bindings) = fixture().await;
    assert!(matches!(
        bind_account(&pool, "default", 10, 999).await,
        Err(CalendarDbError::Binding)
    ));
    assert!(matches!(
        bind_account(&pool, "alias", 10, 100).await,
        Err(CalendarDbError::Binding)
    ));
    let invalid = Binding {
        account_id: "bad".into(),
        binding_id: Uuid::new_v4(),
        app_id: 10,
        user_id: 999,
    };
    assert!(
        create(
            &pool,
            "rollback",
            "fp",
            &content(),
            &[bindings[0].clone(), invalid]
        )
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kakao_calendar_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let receipt = create(&pool, "create", "fp", &content(), &bindings)
        .await
        .unwrap();
    let c = claim(&pool, &accounts()).await.unwrap().unwrap();
    sqlx::query("UPDATE kakao_calendar_operations SET lease_expires_at=NOW()-INTERVAL '1 second' WHERE id=$1").bind(c.id).execute(&pool).await.unwrap();
    assert!(!dispatch(&pool, &c).await.unwrap());
    let current = claim(&pool, &accounts()).await.unwrap().unwrap();
    mutate(
        &pool,
        Mutation {
            event: receipt.event_id,
            key: "delete",
            fingerprint: "del",
            expected_revision: 1,
            content: &content(),
            delete: true,
        },
    )
    .await
    .unwrap();
    assert!(!dispatch(&pool, &current).await.unwrap());
    pool.close().await;
    db.drop().await;
}
