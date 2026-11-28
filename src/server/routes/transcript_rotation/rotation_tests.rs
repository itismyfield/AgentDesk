use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::Method,
};
use chrono::TimeDelta;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

use super::*;
use crate::{
    config::{Config, TestRuntimeRootGuard},
    db::auto_queue::test_support::TestPostgresDb,
    services::turn_orchestrator::{
        ChannelMailboxRegistry,
        queued_head::{
            v2::{actor, rotation::sha256},
            v2_types::rotation::{RotationIntent, Ticket},
        },
    },
};

fn app(soft_bytes: Option<u64>, pool: Option<sqlx::PgPool>) -> Router {
    let mut config = Config::default();
    config.runtime.transcript_rotation_soft_bytes = soft_bytes;
    let engine = crate::engine::PolicyEngine::new(&config).unwrap();
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    let state = AppState {
        pg_pool: pool,
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    };
    Router::new().nest(
        "/api",
        super::super::domains::runtime::router(state.clone()).with_state(state),
    )
}

async fn call(router: &Router, method: Method, key: &str, body: Body) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(format!("/api/sessions/{key}/rotation-handoff"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
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
async fn rotation_handoff_api_stages_without_effect_pg() {
    let root = TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = ChannelId::new(8_472_103);
    let key = "rotation-key";
    let row_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, channel_id, discord_token_hash)
         VALUES ($1, 'claude', $2, 'rotation-tests') RETURNING id",
    )
    .bind(key)
    .bind(channel.get().to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, "rotation-tests", None);
    let (path, namespace) = actor::v2_queue(&persistence, channel).unwrap();
    let mailbox = ChannelMailboxRegistry::default().handle(channel);
    mailbox
        .v2_adopt(path.clone(), namespace.clone())
        .await
        .unwrap();
    let handoff = Handoff {
        op_id: Uuid::new_v4(),
        native_session_id: Uuid::new_v4(),
        ticket: Uuid::new_v4(),
        body: "Continue from the verified checkpoint.\n".into(),
    };
    let now = chrono::Utc::now();
    let intent = RotationIntent {
        op_id: handoff.op_id,
        native_session_id: handoff.native_session_id,
        session_row_id: row_id,
        requested_at: now,
        ticket: Ticket {
            digest: sha256(handoff.ticket.as_bytes()),
            expires_at: now + TimeDelta::minutes(5),
            revoked: false,
        },
        submission: None,
    };
    mailbox
        .v2_request(move |slot| {
            slot.as_mut().unwrap().snapshot.rotation = Some(intent);
            Ok(())
        })
        .await
        .unwrap();
    let before = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    let router = app(Some(1), Some(pool.clone()));
    let (status, pending) = call(&router, Method::GET, key, Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pending["rotation"]["state"], "requested");
    assert!(pending["rotation"].get("ticket").is_none());
    assert!(!path.exists());

    let post_body = || Body::from(serde_json::to_vec(&handoff).unwrap());
    let (first, second) = tokio::join!(
        call(&router, Method::POST, key, post_body()),
        call(&router, Method::POST, key, post_body())
    );
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first, second);
    assert_eq!(first.1["op_id"], handoff.op_id.to_string());
    assert_eq!(first.1["digest"], sha256(handoff.body.as_bytes()));
    let after = std::fs::read(&path).unwrap();
    let mut expected = before;
    expected["rotation"]["submission"] = json!({ "receipt": first.1, "body": handoff.body });
    assert_eq!(serde_json::from_slice::<Value>(&after).unwrap(), expected);
    mailbox.v2_adopt(path.clone(), namespace).await.unwrap();
    let (status, staged) = call(&router, Method::GET, key, Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(staged["rotation"]["state"], "staged");
    assert_eq!(staged["rotation"]["body"], handoff.body);
    assert_eq!(staged["rotation"]["receipt"], first.1);
    assert!(staged["rotation"].get("ticket").is_none());
    assert_eq!(std::fs::read(&path).unwrap(), after);
    let mut removed_field = serde_json::to_value(&handoff).unwrap();
    removed_field["scheduled_prompts"] = json!([]);
    assert_eq!(
        call(
            &router,
            Method::POST,
            key,
            Body::from(removed_field.to_string())
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM transcript_rotation_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "staging changed the request latch");
    drop(root);
}

#[tokio::test]
async fn rotation_http_off_skips_body_owner_and_postgres() {
    let pg = sqlx::PgPool::connect_lazy("postgres://postgres@127.0.0.1:1/rotation-off").unwrap();
    pg.close().await;
    let router = app(None, Some(pg));
    let unreadable = futures::stream::poll_fn(
        |_| -> std::task::Poll<Option<Result<axum::body::Bytes, std::io::Error>>> {
            panic!("disabled rotation polled its request body")
        },
    );
    for (method, body) in [
        (Method::POST, Body::from_stream(unreadable)),
        (Method::GET, Body::empty()),
    ] {
        let (status, json) = call(&router, method, "disabled", body).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"], "transcript rotation disabled");
    }
}
