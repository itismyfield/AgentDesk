//! TUI relay HTTP primitives — single-shot `claude_tui_send` and event-driven
//! `claude_tui_wait` to replace the `write_stdin` + 1Hz polling pattern that
//! audit chunk-04 flagged (952 `write_stdin` calls across 29 sessions).
//!
//! Exposed by dcserver and registered as MCP tools downstream (see
//! `services::mcp_config` + the runtime config docs). The endpoints are
//! intentionally minimal so they can also be reached directly by other
//! AgentDesk processes (e.g. the runtime-ops skill).
//!
//! Endpoints (mounted under the dcserver root by `mod.rs`):
//! - `POST /tui/send`  — write a complete message to a tmux-hosted TUI in
//!   one call instead of streaming raw key bytes.
//! - `POST /tui/wait`  — block until a matching provider hook event arrives
//!   (Stop / SubagentStop / token-in-transcript), with caller-supplied
//!   timeout. No 1Hz polling.
//!
//! Notes:
//! - The send path uses tmux `load-buffer` + `paste-buffer -p -r` so that
//!   multi-line text, unicode, and special characters are preserved without
//!   tmux interpreting them as key names. A separate `Enter` follows when
//!   `submit=true` (default), matching how the Discord turn-bridge already
//!   submits prompts.
//! - The wait path subscribes to the existing in-process hook broadcast
//!   (`hook_server::subscribe_hook_events`) so we observe Stop events the
//!   moment they land instead of polling the transcript every second.

use std::time::Duration;

use axum::{Json, Router, routing::post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::services::claude_tui::hook_server::{
    HookEvent, HookEventKind, subscribe_hook_events,
};
use crate::services::platform::tmux;

/// Hard ceiling on `claude_tui_wait` timeout so a misbehaving caller cannot
/// pin a dcserver task indefinitely.
const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Default timeout if the caller omits one.
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Buffer name used when pasting send-text. Each request gets its own buffer
/// so concurrent sends to different sessions cannot stomp each other.
fn allocate_buffer_name() -> String {
    format!("adk-tui-send-{}", uuid::Uuid::new_v4().simple())
}

#[derive(Debug, Deserialize)]
pub struct SendRequest {
    /// tmux session name (without the leading `=`).
    pub session_name: String,
    /// Full message text. May contain newlines, unicode, etc.
    pub text: String,
    /// When true (default), follow the paste with a single `Enter` to
    /// submit. Set to false to leave the text in the prompt as a draft.
    #[serde(default = "default_true")]
    pub submit: bool,
}

#[derive(Debug, Serialize)]
pub struct SendResponse {
    pub ok: bool,
    pub session_name: String,
    pub bytes: usize,
    pub submitted: bool,
}

#[derive(Debug, Deserialize)]
pub struct WaitRequest {
    /// tmux session name (without the leading `=`). Reserved for caller
    /// telemetry; current implementation filters on provider-scoped
    /// session_id reported by the hook receiver.
    pub session_name: String,
    /// Provider hook session_id to match (typically the Claude session UUID
    /// or the Codex rollout id). When unset, *any* session_id is accepted —
    /// useful when the caller only knows the tmux name.
    #[serde(default)]
    pub session_id: Option<String>,
    /// One of `stop` (default) or `token`. When `token`, `token` must be
    /// supplied and we search the most recent hook payload for a substring.
    #[serde(default)]
    pub until: Option<String>,
    /// Substring to look for in hook event payloads (used with
    /// `until = "token"`).
    #[serde(default)]
    pub token: Option<String>,
    /// Timeout in milliseconds. Capped at 15 minutes. Defaults to 120s.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Optional provider filter (e.g. `"claude"` or `"codex"`). When unset,
    /// any provider matches.
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WaitResponse {
    pub ok: bool,
    pub matched: bool,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<Value>,
    pub waited_ms: u128,
}

fn default_true() -> bool {
    true
}

pub fn router() -> Router {
    Router::new()
        .route("/tui/send", post(handle_send))
        .route("/tui/wait", post(handle_wait))
}

async fn handle_send(Json(req): Json<SendRequest>) -> Json<Value> {
    let session_name = req.session_name.trim().to_string();
    if session_name.is_empty() {
        return Json(error_json("session_name is required"));
    }
    if !tmux::has_session(&session_name) {
        return Json(error_json(&format!(
            "tmux session not found: {session_name}"
        )));
    }
    if req.text.is_empty() {
        return Json(error_json("text must not be empty"));
    }

    let buffer_name = allocate_buffer_name();
    if let Err(error) = tmux::load_buffer(&buffer_name, &req.text) {
        return Json(error_json(&format!("tmux load-buffer failed: {error}")));
    }

    // `paste-buffer -p -r -d` pastes in bracketed-paste mode (so the TUI
    // recognises it as a paste, not many tiny keystrokes), preserves LF (so
    // it does NOT get auto-Enter'd by tmux), and deletes the buffer after
    // pasting.
    let paste_result = tmux::paste_buffer(&session_name, &buffer_name, true);
    if let Err(error) = paste_result {
        return Json(error_json(&format!("tmux paste-buffer failed: {error}")));
    }

    let mut submitted = false;
    if req.submit {
        match tmux::send_keys(&session_name, &["Enter"]) {
            Ok(_) => submitted = true,
            Err(error) => {
                return Json(error_json(&format!("tmux send-keys Enter failed: {error}")));
            }
        }
    }

    let bytes = req.text.len();
    let resp = SendResponse {
        ok: true,
        session_name,
        bytes,
        submitted,
    };
    Json(serde_json::to_value(resp).unwrap_or_else(|_| json!({ "ok": true })))
}

async fn handle_wait(Json(req): Json<WaitRequest>) -> Json<Value> {
    let session_name = req.session_name.trim().to_string();
    if session_name.is_empty() {
        return Json(error_json("session_name is required"));
    }

    let timeout = req
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_WAIT_TIMEOUT)
        .min(MAX_WAIT_TIMEOUT);

    let until_mode = req
        .until
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "stop".to_string());

    let token = req.token.as_deref().map(str::trim).map(str::to_string);
    if until_mode == "token" && token.as_deref().map(str::is_empty).unwrap_or(true) {
        return Json(error_json("until=token requires a non-empty token"));
    }

    let started = Instant::now();
    let mut rx = subscribe_hook_events();
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                return Json(serde_json::to_value(WaitResponse {
                    ok: true,
                    matched: false,
                    reason: "timeout".to_string(),
                    event: None,
                    waited_ms: started.elapsed().as_millis(),
                }).unwrap_or_else(|_| json!({ "ok": true, "matched": false, "reason": "timeout" })));
            }
            event = rx.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Lost events because the broadcast channel ran ahead.
                        // Resubscribe so we don't get stuck — the caller can
                        // re-poll if needed.
                        rx = subscribe_hook_events();
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Json(error_json("hook event channel closed"));
                    }
                };

                if !event_matches(
                    &event,
                    req.provider.as_deref(),
                    req.session_id.as_deref(),
                    &until_mode,
                    token.as_deref(),
                ) {
                    continue;
                }

                let summary = json!({
                    "provider": event.provider,
                    "session_id": event.session_id,
                    "kind": event.kind.as_str(),
                    "received_at": event.received_at,
                });
                return Json(serde_json::to_value(WaitResponse {
                    ok: true,
                    matched: true,
                    reason: format!("matched:{}", until_mode),
                    event: Some(summary),
                    waited_ms: started.elapsed().as_millis(),
                }).unwrap_or_else(|_| json!({ "ok": true, "matched": true })));
            }
        }
    }
}

fn event_matches(
    event: &HookEvent,
    want_provider: Option<&str>,
    want_session_id: Option<&str>,
    until_mode: &str,
    token: Option<&str>,
) -> bool {
    if let Some(provider) = want_provider {
        if !provider.trim().is_empty()
            && !event.provider.eq_ignore_ascii_case(provider.trim())
        {
            return false;
        }
    }
    if let Some(sid) = want_session_id {
        if !sid.trim().is_empty() && event.session_id != sid.trim() {
            return false;
        }
    }
    match until_mode {
        "stop" => matches!(
            event.kind,
            HookEventKind::Stop | HookEventKind::SubagentStop
        ),
        "token" => {
            let needle = match token {
                Some(value) if !value.is_empty() => value,
                _ => return false,
            };
            payload_contains(&event.payload, needle)
        }
        _ => false,
    }
}

fn payload_contains(payload: &Value, needle: &str) -> bool {
    match payload {
        Value::String(text) => text.contains(needle),
        Value::Array(items) => items.iter().any(|item| payload_contains(item, needle)),
        Value::Object(map) => map.values().any(|value| payload_contains(value, needle)),
        _ => false,
    }
}

fn error_json(message: &str) -> Value {
    json!({ "ok": false, "error": message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn make_event(provider: &str, sid: &str, kind: HookEventKind, payload: Value) -> HookEvent {
        HookEvent {
            provider: provider.to_string(),
            session_id: sid.to_string(),
            kind,
            received_at: Utc::now(),
            payload,
        }
    }

    #[test]
    fn stop_event_matches_default() {
        let event = make_event("claude", "abc", HookEventKind::Stop, json!({}));
        assert!(event_matches(&event, None, None, "stop", None));
    }

    #[test]
    fn subagent_stop_also_matches_stop_mode() {
        let event = make_event("claude", "abc", HookEventKind::SubagentStop, json!({}));
        assert!(event_matches(&event, None, None, "stop", None));
    }

    #[test]
    fn unrelated_event_does_not_match_stop() {
        let event = make_event("claude", "abc", HookEventKind::UserPromptSubmit, json!({}));
        assert!(!event_matches(&event, None, None, "stop", None));
    }

    #[test]
    fn provider_filter_rejects_mismatch() {
        let event = make_event("codex", "abc", HookEventKind::Stop, json!({}));
        assert!(!event_matches(&event, Some("claude"), None, "stop", None));
        assert!(event_matches(&event, Some("codex"), None, "stop", None));
    }

    #[test]
    fn session_id_filter_rejects_mismatch() {
        let event = make_event("claude", "abc", HookEventKind::Stop, json!({}));
        assert!(!event_matches(&event, None, Some("xyz"), "stop", None));
        assert!(event_matches(&event, None, Some("abc"), "stop", None));
    }

    #[test]
    fn token_mode_matches_nested_payload() {
        let event = make_event(
            "claude",
            "abc",
            HookEventKind::UserPromptSubmit,
            json!({ "transcript": { "tail": "ready: marker-XYZ here" } }),
        );
        assert!(event_matches(&event, None, None, "token", Some("marker-XYZ")));
        assert!(!event_matches(&event, None, None, "token", Some("absent")));
    }

    #[test]
    fn token_mode_requires_non_empty_token() {
        let event = make_event("claude", "abc", HookEventKind::UserPromptSubmit, json!({}));
        assert!(!event_matches(&event, None, None, "token", None));
        assert!(!event_matches(&event, None, None, "token", Some("")));
    }

    #[test]
    fn payload_contains_searches_arrays_and_objects() {
        let payload = json!({
            "a": [
                { "b": "alpha" },
                { "c": [ "beta", "gamma" ] },
            ],
            "d": "delta"
        });
        assert!(payload_contains(&payload, "gamma"));
        assert!(payload_contains(&payload, "delta"));
        assert!(!payload_contains(&payload, "epsilon"));
    }
}
