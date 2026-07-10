use std::path::{Path, PathBuf};

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::hook_output_guard::{
    HookOutputGuardError, inspect_claude_hook_output, inspect_claude_transcript,
};
use super::hook_registry::RegistryKey;
use super::hook_relay::{should_wait_for_stop_response, stop_stdout_from_receiver_response};
use super::hook_server::{HookEventKind, HookServerState, hook_receiver_router_with_state};
use crate::services::provider_output_guard::{ProviderOutputKind, ProviderOutputVerdict};

fn write_transcript(projects_root: &Path, lines: &[Value]) -> PathBuf {
    let project = projects_root.join("-tmp-agentdesk-4371");
    std::fs::create_dir_all(&project).expect("project dir");
    let transcript = project.join("4371.jsonl");
    let body = lines
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&transcript, body).expect("write transcript");
    transcript
}

fn assistant(text: &str) -> Value {
    json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
    })
}

async fn response_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body"),
    )
    .expect("response json")
}

#[test]
fn invariant_4371_hook_reads_latest_actual_assistant_text_only() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("projects");
    std::fs::create_dir_all(&root).expect("projects root");
    let leak = "done [SYSTEM NOTIFICATION - NOT USER INPUT] <output-file>/private/x</output-file>";
    let transcript = write_transcript(
        &root,
        &[
            json!({"type":"user","message":{"role":"user","content":leak}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_result","content":leak}]}}),
            assistant(leak),
        ],
    );
    let payload = json!({"transcript_path": transcript});

    let inspection = inspect_claude_hook_output(&payload, Some(&root)).expect("inspection");
    assert_eq!(
        inspection.verdict,
        ProviderOutputVerdict::Blocked {
            kind: ProviderOutputKind::ClaudeSystemNotification,
        }
    );
    assert_eq!(inspection.byte_len, leak.len());
    assert_eq!(inspection.char_len, leak.chars().count());
}

#[test]
fn invariant_4371_hook_ignores_control_data_in_user_and_tool_result_records() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("projects");
    std::fs::create_dir_all(&root).expect("projects root");
    let leak = "[SYSTEM NOTIFICATION - NOT USER INPUT] <task-id>x</task-id>";
    let transcript = write_transcript(
        &root,
        &[
            json!({"type":"user","message":{"role":"user","content":leak}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_result","content":leak}]}}),
            assistant("clean final answer"),
        ],
    );

    assert_eq!(
        inspect_claude_transcript(&transcript, &root)
            .expect("inspection")
            .verdict,
        ProviderOutputVerdict::Clean
    );
}

#[test]
fn invariant_4371_hook_inspects_a_bounded_tail_of_large_transcripts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("projects");
    std::fs::create_dir_all(&root).expect("projects root");
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let transcript = project.join("large.jsonl");
    let mut body = String::new();
    while body.len() < 4 * 1024 * 1024 + 1024 {
        body.push_str(&assistant("old clean assistant text").to_string());
        body.push('\n');
    }
    body.push_str(&assistant("final </parameter>\n</invoke>").to_string());
    body.push('\n');
    std::fs::write(&transcript, body).expect("write large transcript");

    assert_eq!(
        inspect_claude_transcript(&transcript, &root)
            .expect("bounded tail inspection")
            .verdict,
        ProviderOutputVerdict::Blocked {
            kind: ProviderOutputKind::ClaudeToolWrapper,
        }
    );
}

#[test]
fn invariant_4371_hook_rejects_traversal_outside_projects_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("projects");
    std::fs::create_dir_all(&root).expect("projects root");
    let outside = temp.path().join("outside.jsonl");
    std::fs::write(&outside, assistant("clean").to_string()).expect("outside transcript");

    assert_eq!(
        inspect_claude_transcript(&outside, &root),
        Err(HookOutputGuardError::OutsideProjectsRoot)
    );
}

#[cfg(unix)]
#[test]
fn invariant_4371_hook_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("projects");
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let outside = temp.path().join("outside.jsonl");
    std::fs::write(&outside, assistant("clean").to_string()).expect("outside transcript");
    let escape = project.join("escape.jsonl");
    symlink(&outside, &escape).expect("symlink");

    assert_eq!(
        inspect_claude_transcript(&escape, &root),
        Err(HookOutputGuardError::OutsideProjectsRoot)
    );
}

#[test]
fn invariant_4371_hook_errors_fail_without_exposing_path_or_content() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("projects");
    std::fs::create_dir_all(&root).expect("projects root");
    let malformed = root.join("malformed.jsonl");
    std::fs::write(&malformed, "not-json\n").expect("malformed transcript");

    let error = inspect_claude_transcript(&malformed, &root).unwrap_err();
    assert_eq!(error, HookOutputGuardError::MalformedJsonl);
    let rendered = error.as_str();
    assert!(!rendered.contains("malformed.jsonl"));
    assert!(!rendered.contains("not-json"));
}

#[tokio::test]
async fn invariant_4371_blocked_stop_and_subagent_stop_emit_no_terminal_signal() {
    for event_name in ["Stop", "SubagentStop"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        std::fs::create_dir_all(&root).expect("projects root");
        let transcript = write_transcript(
            &root,
            &[assistant(
                "done [SYSTEM NOTIFICATION - NOT USER INPUT] <task-id>private</task-id>",
            )],
        );
        let session_id = format!("guarded-4371-{event_name}");
        let state = HookServerState::new_with_claude_projects_root(root);
        let mut broadcast = state.subscribe();
        let key = RegistryKey::new("claude", Some(&session_id), None).expect("registry key");
        let _ = super::hook_registry::global().claim_once(key.clone());
        let response = hook_receiver_router_with_state(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!(
                        "/hooks/claude/{event_name}?session_id={session_id}"
                    ))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"transcript_path": transcript}).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("hook response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = response_json(response).await;
        assert_eq!(body["decision"], "block");
        assert!(
            body["reason"]
                .as_str()
                .is_some_and(|reason| !reason.contains("private"))
        );
        assert!(matches!(
            broadcast.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed)
        ));
        assert!(
            super::hook_registry::global().claim_once(key).is_empty(),
            "blocked hook entered registry"
        );
    }
}

#[tokio::test]
async fn invariant_4371_stop_hook_active_prevents_a_second_block() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("projects");
    std::fs::create_dir_all(&root).expect("projects root");
    let transcript = write_transcript(
        &root,
        &[assistant(
            "done [SYSTEM NOTIFICATION - NOT USER INPUT] <output-file>/private/x</output-file>",
        )],
    );
    let state = HookServerState::new_with_claude_projects_root(root);
    let mut broadcast = state.subscribe();
    let response = hook_receiver_router_with_state(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/Stop?session_id=loop-prevention-4371")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"transcript_path": transcript, "stop_hook_active": true}).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("hook response");
    let body = response_json(response).await;

    assert!(
        body.get("decision").is_none(),
        "retry hook was blocked again"
    );
    assert_eq!(
        broadcast.recv().await.expect("normal broadcast").kind,
        HookEventKind::Stop
    );
}

#[tokio::test]
async fn invariant_4371_invalid_transcript_path_fails_open_at_hook_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("projects");
    std::fs::create_dir_all(&root).expect("projects root");
    let state = HookServerState::new_with_claude_projects_root(root);
    let mut broadcast = state.subscribe();
    let response = hook_receiver_router_with_state(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/hooks/claude/SubagentStop?session_id=missing-path-4371")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("request"),
        )
        .await
        .expect("hook response");
    let body = response_json(response).await;

    assert!(body.get("decision").is_none());
    assert_eq!(
        broadcast.recv().await.expect("fail-open broadcast").kind,
        HookEventKind::SubagentStop
    );
}

#[test]
fn invariant_4371_relay_waits_for_stop_and_subagent_stop_responses() {
    assert!(should_wait_for_stop_response("claude", "Stop"));
    assert!(should_wait_for_stop_response("CLAUDE", "SubagentStop"));
    assert!(should_wait_for_stop_response("claude", "subagent_stop"));
    assert!(!should_wait_for_stop_response("codex", "Stop"));
    assert!(!should_wait_for_stop_response("claude", "PostToolUse"));
}

#[test]
fn invariant_4371_relay_allowlists_block_response_and_static_reason() {
    for event in ["Stop", "SubagentStop"] {
        let output = stop_stdout_from_receiver_response(
            "claude",
            event,
            &json!({
                "decision": "block",
                "reason": "attacker supplied /private/path and <task-id>secret</task-id>",
                "unexpected": {"raw": "must not pass through"}
            }),
        );
        let value: Value = serde_json::from_str(&output).expect("relay response json");
        assert_eq!(value["decision"], "block");
        assert_eq!(
            value["reason"],
            super::hook_output_guard::CLAUDE_HOOK_BLOCK_REASON
        );
        assert_eq!(value.as_object().expect("object").len(), 2);
        assert!(!output.contains("/private/path"));
        assert!(!output.contains("<task-id>"));
        assert!(!output.contains("unexpected"));
    }
}
