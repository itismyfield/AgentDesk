use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::hook_server::{HookServerState, hook_receiver_router_with_state};

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body");
    serde_json::from_slice(&bytes).expect("json response")
}

#[tokio::test]
async fn memento_search_then_tool_feedback_clears_pending_stop_flush() {
    let state = HookServerState::new();
    let app = hook_receiver_router_with_state(state);
    let search_payload = json!({
        "tool_name": "mcp__memento__recall",
        "tool_response": [{"type":"text","text":"{\"_meta\":{\"searchEventId\":\"3332\"}}"}]
    });
    let feedback_payload = json!({
        "tool_name": "mcp__memento__tool_feedback",
        "tool_input": {"search_event_id": 3332, "relevant": true, "sufficient": true}
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/PostToolUse?session_id=sess-feedback")
                .header("content-type", "application/json")
                .body(Body::from(search_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/PostToolUse?session_id=sess-feedback")
                .header("content-type", "application/json")
                .body(Body::from(feedback_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=sess-feedback")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(response).await;
    assert!(body.get("memento_tool_feedback_flush").is_none());
}

#[tokio::test]
async fn stop_without_pending_memento_search_stays_observational() {
    let app = hook_receiver_router_with_state(HookServerState::new());

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=sess-no-pending")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(response).await;

    assert_eq!(body["ok"], true);
    assert!(body.get("memento_tool_feedback_flush").is_none());
}

#[tokio::test]
async fn stop_with_pending_memento_search_flushes_once_and_clears() {
    let state = HookServerState::new();
    let app = hook_receiver_router_with_state(state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/PostToolUse?session_id=sess-pending")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "tool_name": "mcp__memento__context",
                        "tool_response": {"_meta":{"searchEventId":"44"}}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=sess-pending")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(response).await;
    let flush = &body["memento_tool_feedback_flush"];
    assert_eq!(flush["search_event_ids"], json!(["44"]));
    assert!(
        flush["additional_context"]
            .as_str()
            .unwrap()
            .contains("[44]")
    );

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=sess-pending")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(response).await;
    assert!(body.get("memento_tool_feedback_flush").is_none());
}

#[tokio::test]
async fn stop_hook_active_suppresses_memento_flush() {
    let state = HookServerState::new();
    let app = hook_receiver_router_with_state(state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/PostToolUse?session_id=sess-active")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "tool_name": "mcp__memento__recall",
                        "tool_response": {"_meta":{"searchEventId":"45"}}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=sess-active")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"stop_hook_active":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(response).await;

    assert!(body.get("memento_tool_feedback_flush").is_none());
}

#[tokio::test]
async fn codex_stop_clears_pending_memento_search_without_flush() {
    let state = HookServerState::new();
    let app = hook_receiver_router_with_state(state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/codex/PostToolUse?session_id=sess-codex")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "tool_name": "mcp__memento__recall",
                        "tool_response": {"_meta":{"searchEventId":"46"}}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/codex/Stop?session_id=sess-codex")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(response).await;
    assert!(body.get("memento_tool_feedback_flush").is_none());

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=sess-codex")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(response).await;
    assert!(body.get("memento_tool_feedback_flush").is_none());
}
