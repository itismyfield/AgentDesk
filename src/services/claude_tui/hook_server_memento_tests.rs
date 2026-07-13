use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::hook_server::relay_receipts::{
    RELAY_DEADLINE_HEADER, RELAY_PUBLISHED_AT_HEADER, RELAY_REQUEST_ID_HEADER,
};
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
                .uri("/hooks/claude/Stop?session_id=sess-feedback")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let stop_body = response_json(response).await;
    assert_eq!(
        stop_body["memento_tool_feedback_flush"]["search_event_ids"],
        json!(["3332"])
    );
    let stop_context = stop_body["memento_tool_feedback_flush"]["additional_context"]
        .as_str()
        .expect("Stop additional context");
    assert!(stop_context.contains("then stop"));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/UserPromptSubmit?session_id=sess-feedback")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prompt":"retry turn"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let retry_body = response_json(response).await;
    assert_eq!(
        retry_body["memento_tool_feedback_flush"]["search_event_ids"],
        json!(["3332"])
    );
    let retry_context = retry_body["memento_tool_feedback_flush"]["additional_context"]
        .as_str()
        .expect("retry additional context");
    assert!(retry_context.contains("continue with the submitted prompt"));
    assert!(!retry_context.contains("then stop"));

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
async fn first_stop_then_next_prompt_retries_once_and_records_terminal_drop() {
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
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/UserPromptSubmit?session_id=sess-pending")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prompt":"next turn"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(response).await;
    assert_eq!(
        body["memento_tool_feedback_flush"]["search_event_ids"],
        json!(["44"])
    );

    let before = crate::services::memory::memento_call_metrics_snapshot(24 * 7)
        ["searchObservability"]["feedback_counts_by_trigger_type"]
        ["unsubmitted_stop_flush"]
        .as_u64()
        .unwrap_or(0);
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
    let after = crate::services::memory::memento_call_metrics_snapshot(24 * 7)
        ["searchObservability"]["feedback_counts_by_trigger_type"]
        ["unsubmitted_stop_flush"]
        .as_u64()
        .unwrap_or(0);
    assert!(after >= before.saturating_add(1));
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
        .clone()
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

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=sess-active")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response_json(response).await;
    assert_eq!(
        body["memento_tool_feedback_flush"]["search_event_ids"],
        json!(["45"])
    );
}

#[tokio::test]
async fn pending_retry_is_cross_session_isolated_and_session_start_clears() {
    let app = hook_receiver_router_with_state(HookServerState::new());
    for (session_id, search_event_id) in [("sess-a", "51"), ("sess-b", "52")] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/hooks/claude/PostToolUse?session_id={session_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "tool_name": "mcp__memento__recall",
                            "tool_response": {"_meta":{"searchEventId":search_event_id}}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    let first_a = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=sess-a")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response_json(first_a).await["memento_tool_feedback_flush"]["search_event_ids"],
        json!(["51"])
    );

    let prompt_b = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/UserPromptSubmit?session_id=sess-b")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prompt":"unrelated"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response_json(prompt_b)
            .await
            .get("memento_tool_feedback_flush")
            .is_none()
    );

    let retry_a = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/UserPromptSubmit?session_id=sess-a")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"prompt":"same session"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response_json(retry_a).await["memento_tool_feedback_flush"]["search_event_ids"],
        json!(["51"])
    );

    let start_b = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/SessionStart?session_id=sess-b")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"source":"resume"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(start_b.status(), StatusCode::ACCEPTED);
    let stop_b = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=sess-b")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response_json(stop_b)
            .await
            .get("memento_tool_feedback_flush")
            .is_none()
    );
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

#[tokio::test]
async fn stale_relay_stop_emits_no_ephemeral_signal_and_does_not_advance_memento() {
    use crate::services::claude_tui::hook_registry::{RegistryKey, global};

    let session = "stale-relay-stop";
    let state = HookServerState::new();
    let mut events = state.subscribe();
    let app = hook_receiver_router_with_state(state);
    let key = RegistryKey::new("claude", Some(session), None).unwrap();
    let _ = global().claim_once(key.clone());

    let search = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/hooks/claude/PostToolUse?session_id={session}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "tool_name":"mcp__memento__recall",
                        "tool_response":{"_meta":{"searchEventId":"84308"}}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(search.status(), StatusCode::ACCEPTED);
    let primed = events.recv().await.unwrap();
    assert_eq!(primed.kind.as_str(), "post_tool_use");
    let _ = global().claim_once(key.clone());

    let now = chrono::Utc::now();
    let stale = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/hooks/claude/Stop?session_id={session}"))
                .header("content-type", "application/json")
                .header(RELAY_REQUEST_ID_HEADER, uuid::Uuid::new_v4().to_string())
                .header(
                    RELAY_PUBLISHED_AT_HEADER,
                    (now - chrono::Duration::hours(2)).to_rfc3339(),
                )
                .header(
                    RELAY_DEADLINE_HEADER,
                    (now - chrono::Duration::hours(1)).to_rfc3339(),
                )
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::GONE);
    assert!(
        events.try_recv().is_err(),
        "stale Stop must not reach the broadcast signal boundary"
    );
    assert!(
        global()
            .claim_once(key)
            .iter()
            .all(|event| event.kind.as_str() != "stop"),
        "stale Stop must not reach the registry signal boundary"
    );

    let fresh = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/hooks/claude/Stop?session_id={session}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let fresh = response_json(fresh).await;
    assert_eq!(
        fresh["memento_tool_feedback_flush"]["search_event_ids"],
        json!(["84308"]),
        "fresh legacy Stop must still observe the first unadvanced memento stage"
    );
    assert!(
        fresh["memento_tool_feedback_flush"]["additional_context"]
            .as_str()
            .unwrap()
            .contains("then stop")
    );
}
