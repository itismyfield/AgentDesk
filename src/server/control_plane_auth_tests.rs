use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header::AUTHORIZATION};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::routes::AppState;
use super::routes::auth::auth_middleware;
use crate::services::claude_tui::hook_server::{HookServerState, hook_receiver_router_with_state};
use crate::services::claude_tui::tui_relay::{FakeSendBackend, router_with_send_backend};

fn test_app_state(auth_token: Option<&str>) -> AppState {
    let mut config = crate::config::Config::default();
    // Non-loopback host so the middleware cannot fall back to a host-based
    // shortcut; the boundary must be proven by peer addr / Bearer alone.
    config.server.host = "0.0.0.0".to_string();
    config.server.auth_token = auth_token.map(str::to_string);
    let tx = crate::eventbus::new_broadcast();
    let buf = crate::eventbus::spawn_batch_flusher(tx.clone());
    AppState {
        pg_pool: None,
        engine: crate::engine::PolicyEngine::new(&config).expect("test policy engine"),
        config: Arc::new(config),
        broadcast_tx: tx,
        batch_buffer: buf,
        health_registry: None,
        cluster_instance_id: None,
    }
}

fn protected_tui_router(auth_token: Option<&str>) -> Router {
    router_with_send_backend(Arc::new(FakeSendBackend)).layer(axum::middleware::from_fn_with_state(
        test_app_state(auth_token),
        auth_middleware,
    ))
}

fn protected_hook_router(auth_token: Option<&str>) -> Router {
    hook_receiver_router_with_state(HookServerState::new()).layer(
        axum::middleware::from_fn_with_state(test_app_state(auth_token), auth_middleware),
    )
}

fn tui_send_request(peer: &str, token: Option<&str>, text: &str) -> Request<Body> {
    let body = json!({
        "session_name": "test-session",
        "text": text,
        "submit": true,
    });
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/tui/send")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    request.extensions_mut().insert(ConnectInfo(
        peer.parse::<SocketAddr>().expect("valid socket addr"),
    ));
    if let Some(token) = token {
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {token}").parse().expect("valid bearer"),
        );
    }
    request
}

fn hook_request(peer: &str, token: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/hooks/claude/Stop?session_id=sess-auth")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"hook_event_name":"Stop"}"#))
        .expect("build request");
    request.extensions_mut().insert(ConnectInfo(
        peer.parse::<SocketAddr>().expect("valid socket addr"),
    ));
    if let Some(token) = token {
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {token}").parse().expect("valid bearer"),
        );
    }
    request
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body");
    serde_json::from_slice(&bytes).expect("json response")
}

#[tokio::test]
async fn tui_send_rejects_unauthenticated_non_loopback_when_protected() {
    let app = protected_tui_router(Some("secret"));

    let response = app
        .oneshot(tui_send_request("10.0.0.5:8791", None, "hello"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn tui_send_rejects_unauthenticated_non_loopback_when_unconfigured() {
    // Defense in depth: even with no auth_token configured ("local-only
    // mode"), a non-loopback caller must NOT reach the control plane.
    let app = protected_tui_router(None);

    let response = app
        .oneshot(tui_send_request("10.0.0.5:8791", None, "hello"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn tui_send_valid_input_with_valid_auth_returns_success() {
    let app = protected_tui_router(Some("secret"));

    let response = app
        .oneshot(tui_send_request(
            "10.0.0.5:8791",
            Some("secret"),
            "hello\nworld",
        ))
        .await
        .expect("response");
    let status = response.status();
    let body = response_json(response).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["session_name"], "test-session");
    assert_eq!(body["bytes"].as_u64(), Some("hello\nworld".len() as u64));
    assert_eq!(body["submitted"], true);
}

#[tokio::test]
async fn tui_send_allows_loopback_without_bearer_when_protected() {
    let app = protected_tui_router(Some("secret"));

    let response = app
        .oneshot(tui_send_request("127.0.0.1:8791", None, "hello"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn hook_receiver_rejects_unauthenticated_non_loopback_when_protected() {
    let app = protected_hook_router(Some("secret"));

    let response = app
        .oneshot(hook_request("10.0.0.5:8791", None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn hook_receiver_rejects_unauthenticated_non_loopback_when_unconfigured() {
    let app = protected_hook_router(None);

    let response = app
        .oneshot(hook_request("10.0.0.5:8791", None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn hook_receiver_allows_loopback_without_bearer_when_protected() {
    let app = protected_hook_router(Some("secret"));

    let response = app
        .oneshot(hook_request("127.0.0.1:8791", None))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::ACCEPTED);
}
