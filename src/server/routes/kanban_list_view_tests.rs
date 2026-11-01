use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "kanban-list-test-token";

fn app(pool: sqlx::PgPool) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.policies.dir = dir.path().join("policies");
    config.policies.hot_reload = false;
    config.data.dir = dir.path().join("data");
    config.server.auth_token = Some(TOKEN.into());
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let state = AppState {
        engine: crate::engine::PolicyEngine::new_with_pg(&config, None).unwrap(),
        config: Arc::new(config),
        pg_pool: Some(pool),
        broadcast_tx: crate::eventbus::new_broadcast(),
        batch_buffer: Default::default(),
        health_registry: None,
        cluster_instance_id: None,
    };
    (
        super::super::domains::kanban::router(state.clone()).with_state(state),
        dir,
    )
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "192.0.2.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn summary_list_omits_bodies_and_done_cards_while_detail_stays_full_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (id, status) in [("card-ready", "ready"), ("card-done", "done")] {
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, description, metadata)
             VALUES ($1, $1, $2, 'long body', '{\"k\":\"v\"}'::jsonb)",
        )
        .bind(id)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }
    let (app, _dir) = app(pool.clone());

    let (status, full) = get(&app, "/kanban-cards").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(full["cards"].as_array().unwrap().len(), 2);
    let full_ready = full["cards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|card| card["id"] == "card-ready")
        .unwrap()
        .clone();
    assert_eq!(full_ready["description"], "long body");
    assert_eq!(full_ready["metadata"], json!({"k": "v"}));

    let (status, summary) = get(&app, "/kanban-cards?view=summary&exclude_done=true").await;
    assert_eq!(status, StatusCode::OK);
    let mut expected = full_ready.clone();
    expected.as_object_mut().unwrap().remove("description");
    expected.as_object_mut().unwrap().remove("metadata");
    assert_eq!(summary, json!({"cards": [expected]}));
    assert_eq!(
        summary["cards"][0]["metadata_json"],
        full_ready["metadata_json"]
    );

    let (status, done_only) = get(&app, "/kanban-cards?exclude_done=true&status=done").await;
    assert_eq!((status, done_only), (StatusCode::OK, json!({"cards": []})));

    let (status, detail) = get(&app, "/kanban-cards/card-ready").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["card"]["description"], "long body");

    for path in [
        "/kanban-cards?view=compact",
        "/kanban-cards?exclude_done=yes",
    ] {
        assert_eq!(get(&app, path).await.0, StatusCode::BAD_REQUEST, "{path}");
    }

    pool.close().await;
    db.drop().await;
}
