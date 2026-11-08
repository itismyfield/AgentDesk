use super::*;

fn document(auto_queue: bool) -> Value {
    json!({"id": "patch", "title": "Patch", "description": "Keep context", "status": "active",
    "round": 1, "auto_queue": auto_queue, "nodes": [
        {"id": "first", "title": "First", "status": "pending", "stage": "implement", "round": 1},
        {"id": "second", "title": "Second", "status": "pending", "stage": "review", "round": 2,
         "dependencies": ["first"], "group": "Backend", "assignee": "reviewer", "provider": "codex",
         "session_id": "historical", "summary": "gist", "benefit": "preserve context",
         "details": "durable details", "evidence": ["cargo test"], "findings": ["P2"],
         "evidence_records": [{"summary": "CI", "result": "Passed", "references": ["artifact"]}],
         "acceptance": ["reviewed"], "next_action": "inspect", "blocker": "waiting"}
    ]})
}

async fn patch(app: &Router, node: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!("/campaigns/patch/nodes/{node}"))
                .header(header::AUTHORIZATION, "Bearer campaign-test-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn postgres_campaign_node_patch_preserves_full_document_and_history_pg() {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let app = app(pool.clone());
    let (status, created) = request(
        &app,
        Method::POST,
        "/campaigns",
        Some(document(false)),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, summary) = request(&app, Method::GET, "/campaigns?view=summary", None, true).await;
    assert!(summary["campaigns"][0]["nodes"][1].get("details").is_none());
    let (status, saved) = patch(
        &app,
        "second",
        json!({"expected_revision": 1, "status": "running"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "status-only PATCH: {saved}");
    let mut expected = created["campaign"].clone();
    expected["revision"] = json!(2);
    expected["updated_at"] = saved["campaign"]["updated_at"].clone();
    expected["nodes"][1]["status"] = json!("running");
    expected["nodes"][1]["updated_at"] = saved["campaign"]["nodes"][1]["updated_at"].clone();
    assert_eq!(
        saved["campaign"], expected,
        "only status and server timestamps may change"
    );
    let (_, read) = request(&app, Method::GET, "/campaigns/patch", None, true).await;
    assert_eq!(read["campaign"], expected);
    let (_, history) = request(&app, Method::GET, "/campaigns/patch/history", None, true).await;
    assert_eq!(history["revisions"], json!([expected, created["campaign"]]));

    assert_eq!(
        patch(
            &app,
            "second",
            json!({"expected_revision": 1, "status": "blocked"})
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "the revision before the successful PATCH is stale"
    );
    let (status, cleared) = patch(
        &app,
        "second",
        json!({"expected_revision": 2,
        "details": null, "dependencies": null, "evidence": null, "findings": null,
        "evidence_records": null, "acceptance": null, "blocker": null, "group": null}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let node = &cleared["campaign"]["nodes"][1];
    assert_eq!(node["details"], "");
    for key in [
        "dependencies",
        "evidence",
        "findings",
        "evidence_records",
        "acceptance",
    ] {
        assert_eq!(node[key], json!([]), "null clears {key}");
    }
    assert_eq!(node["blocker"], Value::Null);
    assert_eq!(node["group"], Value::Null);
    assert_eq!(node["assignee"], "reviewer");
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_campaign_node_patch_rejects_conflicts_missing_nodes_and_invalid_dags_pg() {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let app = app(pool.clone());
    let (_, created) = request(
        &app,
        Method::POST,
        "/campaigns",
        Some(document(false)),
        true,
    )
    .await;
    assert_eq!(
        request(
            &app,
            Method::PATCH,
            "/campaigns/patch/nodes/second",
            Some(json!({"expected_revision": 1, "status": "running"})),
            false
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            Method::PATCH,
            "/campaigns/absent/nodes/second",
            Some(json!({"expected_revision": 1, "status": "running"})),
            true
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for (node, body, expected) in [
        (
            "second",
            json!({"expected_revision": 2, "status": "running"}),
            StatusCode::CONFLICT,
        ),
        (
            "absent",
            json!({"expected_revision": 1, "status": "running"}),
            StatusCode::NOT_FOUND,
        ),
        (
            "first",
            json!({"expected_revision": 1, "dependencies": ["second"]}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "second",
            json!({"expected_revision": 1, "dependencies": ["absent"]}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "second",
            json!({"expected_revision": 1, "dependencies": ["first", "first"]}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "second",
            json!({"expected_revision": 1, "title": null}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "second",
            json!({"expected_revision": 1, "round": 0}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "second",
            json!({"expected_revision": 1, "id": "renamed"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "second",
            json!({"expected_revision": 1, "updated_at": null}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "second",
            json!({"status": "running"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (status, response) = patch(&app, node, body.clone()).await;
        assert_eq!(
            status, expected,
            "node {node}, body {body}, response {response}"
        );
    }
    let (_, history) = request(&app, Method::GET, "/campaigns/patch/history", None, true).await;
    assert_eq!(history["revisions"], json!([created["campaign"]]));
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_campaign_node_patch_hands_off_active_auto_queue_pg() {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let app = app(pool.clone());
    assert_eq!(
        request(&app, Method::POST, "/campaigns", Some(document(true)), true)
            .await
            .0,
        StatusCode::CREATED
    );
    let (status, saved) = patch(
        &app,
        "second",
        json!({"expected_revision": 1, "status": "running"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        saved["handoff"],
        json!({"queued": [], "waiting": [{"node_id": "first", "reason": "no_issue_card"}]})
    );
    assert_eq!(saved["campaign"]["auto_queue"], true);
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_campaign_node_patch_races_put_on_the_same_revision_pg() {
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let app = app(pool.clone());
    let (_, created) = request(
        &app,
        Method::POST,
        "/campaigns",
        Some(document(false)),
        true,
    )
    .await;
    let mut replacement = created["campaign"].clone();
    replacement["expected_revision"] = json!(1);
    replacement["title"] = json!("PUT winner");
    let (put, patch) = tokio::join!(
        request(
            &app,
            Method::PUT,
            "/campaigns/patch",
            Some(replacement),
            true
        ),
        patch(
            &app,
            "second",
            json!({"expected_revision": 1, "status": "running"})
        )
    );
    assert!(
        (put.0 == StatusCode::OK && patch.0 == StatusCode::CONFLICT)
            || (put.0 == StatusCode::CONFLICT && patch.0 == StatusCode::OK),
        "PUT {put:?}, PATCH {patch:?}"
    );
    let winner = if put.0 == StatusCode::OK {
        put.1
    } else {
        patch.1
    };
    assert_eq!(winner["campaign"]["revision"], 2);
    let (_, history) = request(&app, Method::GET, "/campaigns/patch/history", None, true).await;
    assert_eq!(
        history["revisions"],
        json!([winner["campaign"], created["campaign"]])
    );
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_campaign_node_patch_rejects_future_revision_before_competing_put_pg() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::{
        sync::Notify,
        time::{Duration, timeout},
    };

    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let regular_app = app(pool.clone());
    let mut body = document(false);
    body["nodes"][0]["details"] = json!("old");
    let (status, created) =
        request(&regular_app, Method::POST, "/campaigns", Some(body), true).await;
    assert_eq!(status, StatusCode::CREATED);

    let read_finished = Arc::new(Notify::new());
    let put_finished = Arc::new(Notify::new());
    let patch_pool = sqlx::postgres::PgPoolOptions::new()
        .min_connections(0)
        .max_connections(1)
        .after_release({
            let read_finished = read_finished.clone();
            let put_finished = put_finished.clone();
            let first_release = AtomicBool::new(true);
            move |_, _| {
                let read_finished = read_finished.clone();
                let put_finished = put_finished.clone();
                let block = first_release.swap(false, Ordering::SeqCst);
                Box::pin(async move {
                    // Hold PATCH's GET connection until the competing PUT commits.
                    if block {
                        read_finished.notify_one();
                        put_finished.notified().await;
                    }
                    Ok(true)
                })
            }
        })
        .connect_lazy(&fixture.database_url)
        .unwrap();
    let patch_app = app(patch_pool.clone());
    let patch_task = tokio::spawn(async move {
        patch(
            &patch_app,
            "second",
            json!({"expected_revision": 2, "status": "running"}),
        )
        .await
    });
    timeout(Duration::from_secs(20), read_finished.notified())
        .await
        .expect("PATCH finished its revision-1 GET");

    let mut replacement = created["campaign"].clone();
    replacement["expected_revision"] = json!(1);
    replacement["nodes"][0]["details"] = json!("new");
    let (put_status, saved) = request(
        &regular_app,
        Method::PUT,
        "/campaigns/patch",
        Some(replacement.clone()),
        true,
    )
    .await;
    put_finished.notify_one();
    let (patch_status, patched) = timeout(Duration::from_secs(20), patch_task)
        .await
        .unwrap()
        .unwrap();
    let (_, current) = request(&regular_app, Method::GET, "/campaigns/patch", None, true).await;
    let (_, history) = request(
        &regular_app,
        Method::GET,
        "/campaigns/patch/history",
        None,
        true,
    )
    .await;
    let (stale_status, put_conflict) = request(
        &regular_app,
        Method::PUT,
        "/campaigns/patch",
        Some(replacement),
        true,
    )
    .await;
    patch_pool.close().await;
    pool.close().await;
    fixture.drop().await;

    assert_eq!(put_status, StatusCode::OK);
    assert_eq!(saved["campaign"]["revision"], 2);
    assert_eq!(
        (
            patch_status,
            current["campaign"]["revision"].clone(),
            current["campaign"]["nodes"][0]["details"].clone()
        ),
        (StatusCode::CONFLICT, json!(2), json!("new")),
        "a PATCH merged from revision 1 must not overwrite the competing PUT: {patched}"
    );
    assert_eq!(current["campaign"], saved["campaign"]);
    assert_eq!(
        history["revisions"],
        json!([saved["campaign"], created["campaign"]])
    );
    assert_eq!(stale_status, StatusCode::CONFLICT);
    assert_eq!(patched, put_conflict, "PATCH uses PUT's conflict envelope");
}

#[tokio::test]
async fn campaign_node_patch_is_advertised_by_api_docs() {
    let app = Router::new().route(
        "/api/docs",
        axum::routing::get(super::super::super::docs::api_docs),
    );
    let (status, docs) = request(&app, Method::GET, "/api/docs?format=flat", None, false).await;
    assert_eq!(status, StatusCode::OK);
    let endpoint = docs["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|endpoint| {
            endpoint["method"] == "PATCH"
                && endpoint["path"] == "/api/campaigns/{id}/nodes/{node_id}"
        });
    assert!(
        endpoint.is_some(),
        "GET /api/docs must advertise node PATCH"
    );
}
