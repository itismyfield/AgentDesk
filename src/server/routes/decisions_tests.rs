//! `POST /api/decisions` stores an ask under the hash of its requester's capability, and with no
//! live provider bot posts nothing; `GET /api/decisions/{id}` reads it back without the capability.
use super::*;
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;

const OWNER: &str = "x-agentdesk-decision-owner";
const CHANNEL: &str = "8211300001";

fn app(root: &std::path::Path, pool: sqlx::PgPool) -> Router {
    let mut config = crate::config::Config::default();
    config.server.host = "127.0.0.1".into();
    config.policies.hot_reload = false;
    config.policies.dir = root.join("policies");
    config.data.dir = root.join("data");
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let state = AppState {
        engine: PolicyEngine::new_with_pg(&config, None).unwrap(),
        config: Arc::new(config),
        pg_pool: Some(pool),
        broadcast_tx: crate::eventbus::new_broadcast(),
        batch_buffer: Default::default(),
        health_registry: None,
        cluster_instance_id: None,
    };
    let access = crate::server::dashboard_auth::DashboardAccess::new(&state.config);
    Router::new().nest("/api", api_router_with_dashboard_access(state, access))
}

/// One request from a local process, with the requester capability when given.
async fn call(
    app: &Router,
    (method, path): (&str, &str),
    capability: Option<&str>,
    body: &Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(capability) = capability {
        request = request.header(OWNER, capability);
    }
    let mut request = request.body(Body::from(body.to_string())).unwrap();
    let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn ask_body(title: &str) -> Value {
    json!({"agent_id": "decision-asker", "target": {"channel_id": CHANNEL},
    "ref": "r1", "idempotency_key": "k1", "title": title, "context": "배경",
    "options": [
        {"label": "진행", "detail": "지금 한다", "recommendation_percent": 70},
        {"label": "축소", "detail": "작게 한다", "recommendation_percent": 30},
    ]})
}

async fn count(pool: &sqlx::PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

const REQUESTS: &str = "SELECT COUNT(*) FROM decision_requests";

/// An ask is stored once per idempotency key, only the capability's hash is kept, another
/// capability or payload cannot take the key, and without a live provider bot the stored ask
/// answers 503 and nothing goes to Discord.
#[tokio::test]
async fn decision_ask_stores_one_request_under_its_capability_hash_pg() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('decision-asker', 'decision-asker', 'claude', $1)",
    )
    .bind(CHANNEL)
    .execute(&pool)
    .await
    .unwrap();
    let app = app(root.path(), pool.clone());
    let (mine, other) = ("ab".repeat(32), "cd".repeat(32));
    let post = ("POST", "/api/decisions");

    let (status, first) = call(&app, post, Some(&mine), &ask_body("질문")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{first}");
    assert_eq!(first["error"], "decision_bot_unavailable");
    assert_eq!(first["created"], true);
    let id = first["request_id"].as_str().unwrap().to_string();

    // Only the hash of the capability is stored.
    let stored: String = sqlx::query_scalar("SELECT capability_sha256 FROM decision_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, hex::encode(Sha256::digest(mine.as_bytes())));
    let leaked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM decision_requests r WHERE row_to_json(r)::text LIKE '%' || $1 || '%'",
    )
    .bind(&mine)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(leaked, 0, "the capability itself is stored");
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM decision_options").await,
        2
    );

    // The same key, payload and capability is the same request.
    let (status, again) = call(&app, post, Some(&mine), &ask_body("질문")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{again}");
    assert_eq!(
        (again["request_id"].as_str(), &again["created"]),
        (Some(id.as_str()), &json!(false))
    );
    // The same channel spelled another way is still the same request.
    let mut spelled = ask_body("질문");
    spelled["target"]["channel_id"] = json!(format!("00{CHANNEL}"));
    let (status, again) = call(&app, post, Some(&mine), &spelled).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{again}");
    assert_eq!(again["request_id"].as_str(), Some(id.as_str()));
    // Another capability, or another payload, cannot take the key.
    for (capability, title) in [(&other, "질문"), (&mine, "다른 질문")] {
        let (status, refused) = call(&app, post, Some(capability), &ask_body(title)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        assert_eq!(refused["error"], "idempotency_conflict");
    }
    assert_eq!(count(&pool, REQUESTS).await, 1);

    // A request without a capability, with one option or for an unknown agent stores nothing.
    let (status, _) = call(&app, post, None, &ask_body("질문")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let mut one_option = ask_body("질문");
    one_option["idempotency_key"] = json!("k2");
    one_option["options"].as_array_mut().unwrap().truncate(1);
    let (status, _) = call(&app, post, Some(&mine), &one_option).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let mut stranger = ask_body("질문");
    stranger["agent_id"] = json!("no-such-agent");
    let (status, _) = call(&app, post, Some(&mine), &stranger).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(count(&pool, REQUESTS).await, 1);

    // Only the requester reads its request: the capability that asked it, and nothing of the
    // capability or its hash comes back.
    let path = format!("/api/decisions/{id}");
    let read = ("GET", path.as_str());
    let (status, shown) = call(&app, read, Some(&mine), &json!(null)).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(
        (&shown["title"], &shown["publication"]),
        (&json!("질문"), &json!("stored"))
    );
    assert_eq!(shown["options"].as_array().map(Vec::len), Some(2));
    let text = shown.to_string();
    assert!(!text.contains(&mine) && !text.contains(&stored), "{text}");
    // Another requester's own request, asked under its own capability.
    let mut theirs = ask_body("남의 질문");
    theirs["idempotency_key"] = json!("k3");
    let (status, _) = call(&app, post, Some(&other), &theirs).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    for (who, capability) in [
        ("no capability", None),
        ("another requester's", Some(&other)),
    ] {
        let (status, refused) =
            call(&app, read, capability.map(String::as_str), &json!(null)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who}: {refused}");
        let text = refused.to_string();
        assert!(
            !text.contains("질문") && !text.contains("진행"),
            "{who}: {text}"
        );
    }
    // An id nobody asked reads the same as a request that is not the caller's.
    let missing = format!("/api/decisions/{}", uuid::Uuid::new_v4());
    let (status, _) = call(&app, ("GET", missing.as_str()), Some(&mine), &json!(null)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM message_outbox").await,
        0,
        "an ask posts nothing"
    );
    pool.close().await;
    db.drop().await;
}
