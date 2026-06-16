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

use std::{sync::Arc, time::Duration};

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind, subscribe_hook_events};
use crate::services::claude_tui::input::validate_prompt_text;
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

pub(crate) trait SendBackend: Send + Sync {
    fn has_session(&self, session_name: &str) -> bool;
    fn load_buffer(&self, buffer_name: &str, text: &str) -> Result<(), String>;
    fn paste_buffer(
        &self,
        session_name: &str,
        buffer_name: &str,
        delete: bool,
    ) -> Result<(), String>;
    fn send_enter(&self, session_name: &str) -> Result<(), String>;
}

struct TmuxSendBackend;

impl SendBackend for TmuxSendBackend {
    fn has_session(&self, session_name: &str) -> bool {
        tmux::has_session(session_name)
    }

    fn load_buffer(&self, buffer_name: &str, text: &str) -> Result<(), String> {
        tmux::load_buffer(buffer_name, text).map(|_| ())
    }

    fn paste_buffer(
        &self,
        session_name: &str,
        buffer_name: &str,
        delete: bool,
    ) -> Result<(), String> {
        tmux::paste_buffer(session_name, buffer_name, delete).map(|_| ())
    }

    fn send_enter(&self, session_name: &str) -> Result<(), String> {
        tmux::send_keys(session_name, &["Enter"]).map(|_| ())
    }
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
    router_with_send_backend(Arc::new(TmuxSendBackend))
}

pub(crate) fn router_with_send_backend(backend: Arc<dyn SendBackend>) -> Router {
    Router::new()
        .route("/tui/send", post(handle_send))
        .route("/tui/wait", post(handle_wait))
        .with_state(backend)
}

async fn handle_send(
    State(backend): State<Arc<dyn SendBackend>>,
    Json(req): Json<SendRequest>,
) -> (StatusCode, Json<Value>) {
    handle_send_with_backend(req, backend.as_ref())
}

fn handle_send_with_backend(
    req: SendRequest,
    backend: &dyn SendBackend,
) -> (StatusCode, Json<Value>) {
    let session_name = req.session_name.trim().to_string();
    if session_name.is_empty() {
        return bad_request_json("session_name is required");
    }
    if let Err(error) = validate_prompt_text(&req.text) {
        return bad_request_json(&error);
    }
    if !backend.has_session(&session_name) {
        return ok_error_json(&format!("tmux session not found: {session_name}"));
    }
    if req.text.is_empty() && !req.submit {
        // Empty text + no submit would be a no-op. Surface this as a 400 so
        // upstream bugs do not silently swallow attempted relays.
        return bad_request_json(
            "text must not be empty when submit=false (call would be a no-op)",
        );
    }

    let bytes = req.text.len();
    if !req.text.is_empty() {
        let buffer_name = allocate_buffer_name();
        if let Err(error) = backend.load_buffer(&buffer_name, &req.text) {
            return ok_error_json(&format!("tmux load-buffer failed: {error}"));
        }

        // `paste-buffer -p -r -d` pastes in bracketed-paste mode (so the TUI
        // recognises it as a paste, not many tiny keystrokes), preserves LF
        // (so it does NOT get auto-Enter'd by tmux), and deletes the buffer
        // after pasting.
        if let Err(error) = backend.paste_buffer(&session_name, &buffer_name, true) {
            // Best-effort cleanup of the orphan buffer is fine to skip:
            // tmux removes buffers when the server exits and we never reuse
            // the UUID-suffixed name.
            return ok_error_json(&format!("tmux paste-buffer failed: {error}"));
        }
    }

    let mut submitted = false;
    if req.submit {
        match backend.send_enter(&session_name) {
            Ok(()) => submitted = true,
            Err(error) => {
                return ok_error_json(&format!("tmux send-keys Enter failed: {error}"));
            }
        }
    }

    let resp = SendResponse {
        ok: true,
        session_name,
        bytes,
        submitted,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(resp).unwrap_or_else(|_| json!({ "ok": true }))),
    )
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

    if until_mode != "stop" && until_mode != "token" {
        return Json(error_json(&format!(
            "unknown until mode: {until_mode} (expected 'stop' or 'token')"
        )));
    }

    let token = req.token.as_deref().map(str::trim).map(str::to_string);
    if until_mode == "token" && token.as_deref().map(str::is_empty).unwrap_or(true) {
        return Json(error_json("until=token requires a non-empty token"));
    }

    let started = Instant::now();

    // #tui-hook-ttl-buffer (REQ-002/REQ-005/REQ-006): claim the registry key
    // BEFORE subscribing to the broadcast so a Stop that landed in the early
    // window (after the previous turn drained the channel but before this wait
    // subscribed) is replayed exactly once instead of being lost. This is the
    // additive event source the PRD migrates `/tui/wait` onto; the broadcast
    // subscription below remains the live path and the fallback. Claiming also
    // cancels any unclaimed-Stop diagnostic timer for the key. When the
    // registry is disabled via the rollback flag, this block is a no-op and the
    // behaviour reverts to the pure broadcast wait.
    if let Some(early) = try_claim_registry_match(
        req.provider.as_deref(),
        req.session_id.as_deref(),
        &until_mode,
        token.as_deref(),
    ) {
        let summary = json!({
            "provider": early.provider,
            "session_id": early.session_id,
            "kind": early.kind.as_str(),
            "received_at": early.received_at,
        });
        return Json(
            serde_json::to_value(WaitResponse {
                ok: true,
                matched: true,
                reason: format!("matched:{until_mode}:registry"),
                event: Some(summary),
                waited_ms: started.elapsed().as_millis(),
            })
            .unwrap_or_else(|_| json!({ "ok": true, "matched": true })),
        );
    }

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

/// #tui-hook-ttl-buffer: claim the registry key for this wait and return the
/// newest buffered event that already satisfies the wait filter, if any. The
/// claim drains the key's fresh buffer exactly once (single-consumption) and
/// cancels any unclaimed-Stop diagnostic timer, so a stale event cannot be
/// replayed into a later turn.
///
/// A key is only formed when BOTH a provider and a session_id are supplied —
/// the registry key requires a concrete session id (provider session id or the
/// tmux fallback), and `/tui/wait` callers that omit `session_id` intentionally
/// accept "any session", which the registry (per-key) cannot represent. Those
/// callers fall through to the broadcast wait unchanged. Returns `None` when
/// the registry is disabled, no key can be formed, or no buffered event matches.
fn try_claim_registry_match(
    want_provider: Option<&str>,
    want_session_id: Option<&str>,
    until_mode: &str,
    token: Option<&str>,
) -> Option<HookEvent> {
    use crate::services::claude_tui::hook_registry;
    if !hook_registry::registry_enabled() {
        return None;
    }
    let provider = want_provider.map(str::trim).filter(|p| !p.is_empty())?;
    let session_id = want_session_id.map(str::trim).filter(|s| !s.is_empty())?;
    let key = hook_registry::RegistryKey::new(provider, Some(session_id), None)?;
    // claim_once drains the fresh buffer AND drops the key so the next turn
    // re-buffers early Stops instead of seeing them delivered "live" and lost.
    let replayed = hook_registry::global().claim_once(key);
    // Return the newest matching event (registry preserves arrival order, so the
    // last matching entry is the freshest Stop / token hit).
    replayed
        .into_iter()
        .rev()
        .find(|event| event_matches(event, want_provider, want_session_id, until_mode, token))
}

fn event_matches(
    event: &HookEvent,
    want_provider: Option<&str>,
    want_session_id: Option<&str>,
    until_mode: &str,
    token: Option<&str>,
) -> bool {
    if let Some(provider) = want_provider {
        if !provider.trim().is_empty() && !event.provider.eq_ignore_ascii_case(provider.trim()) {
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

fn ok_error_json(message: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(error_json(message)))
}

fn bad_request_json(message: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(error_json(message)))
}

/// Test-only `SendBackend` exposed to the crate so the server-layer
/// auth-boundary integration tests (which own the middleware wiring) can mount
/// `/tui/send` without touching tmux. Kept next to the trait it implements; the
/// auth-boundary assertions live under `crate::server` because middleware
/// composition is a server-layer responsibility (#3311).
#[cfg(test)]
pub(crate) struct FakeSendBackend;

#[cfg(test)]
impl SendBackend for FakeSendBackend {
    fn has_session(&self, _session_name: &str) -> bool {
        true
    }

    fn load_buffer(&self, _buffer_name: &str, _text: &str) -> Result<(), String> {
        Ok(())
    }

    fn paste_buffer(
        &self,
        _session_name: &str,
        _buffer_name: &str,
        _delete: bool,
    ) -> Result<(), String> {
        Ok(())
    }

    fn send_enter(&self, _session_name: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use chrono::Utc;
    use serde_json::json;

    fn send_request(text: &str, submit: bool) -> SendRequest {
        SendRequest {
            session_name: "test-session".to_string(),
            text: text.to_string(),
            submit,
        }
    }

    #[test]
    fn handle_send_rejects_control_characters_with_bad_request() {
        let (status, body) =
            handle_send_with_backend(send_request("hello\u{1b}[31m", true), &FakeSendBackend);

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0["error"],
            "prompt contains unsupported terminal control characters"
        );
    }

    #[test]
    fn handle_send_rejects_empty_session_name() {
        let mut req = send_request("hello", true);
        req.session_name = "   ".to_string();
        let (status, body) = handle_send_with_backend(req, &FakeSendBackend);

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0["error"], "session_name is required");
    }

    #[test]
    fn handle_send_valid_input_returns_success() {
        let (status, body) =
            handle_send_with_backend(send_request("hello\nworld", true), &FakeSendBackend);

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["ok"], true);
        assert_eq!(body.0["session_name"], "test-session");
        assert_eq!(body.0["bytes"].as_u64(), Some("hello\nworld".len() as u64));
        assert_eq!(body.0["submitted"], true);
    }

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
        assert!(event_matches(
            &event,
            None,
            None,
            "token",
            Some("marker-XYZ")
        ));
        assert!(!event_matches(&event, None, None, "token", Some("absent")));
    }

    #[test]
    fn token_mode_requires_non_empty_token() {
        let event = make_event("claude", "abc", HookEventKind::UserPromptSubmit, json!({}));
        assert!(!event_matches(&event, None, None, "token", None));
        assert!(!event_matches(&event, None, None, "token", Some("")));
    }

    // -------- #tui-hook-ttl-buffer: registry-backed early-Stop claim --------

    /// REQ-002/REQ-006: an early Stop buffered in the registry (before `/tui/wait`
    /// subscribed to the broadcast) is returned by `try_claim_registry_match`
    /// without waiting for a new broadcast event. The claim is single-shot —
    /// a second call returns nothing (stale Stop cannot wake a later turn).
    #[test]
    fn try_claim_registry_match_returns_buffered_early_stop_once() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};
        let provider = "claude";
        let session = "wait-early-stop";
        // Clean slate.
        if let Some(k) = RegistryKey::new(provider, Some(session), None) {
            let _ = global().claim_once(k);
        }
        let key = RegistryKey::new(provider, Some(session), None).unwrap();
        global().deliver(
            key,
            make_event(provider, session, HookEventKind::Stop, json!({})),
        );

        let matched = try_claim_registry_match(Some(provider), Some(session), "stop", None);
        assert!(matched.is_some(), "buffered early Stop must be claimed");
        assert_eq!(matched.unwrap().kind, HookEventKind::Stop);

        // Single-consumption: a second claim finds nothing.
        let second = try_claim_registry_match(Some(provider), Some(session), "stop", None);
        assert!(
            second.is_none(),
            "claimed Stop must not replay to a later wait"
        );
    }

    /// REQ-005: a `/tui/wait` caller that omits `session_id` accepts "any
    /// session", which the per-key registry cannot represent — so it falls
    /// through to the broadcast wait (registry returns `None`).
    #[test]
    fn try_claim_registry_match_skips_when_no_session_id() {
        assert!(try_claim_registry_match(Some("claude"), None, "stop", None).is_none());
        assert!(try_claim_registry_match(None, Some("sid"), "stop", None).is_none());
    }

    /// Cross-provider isolation at the wait seam: a Stop buffered for
    /// (codex, ABC) must not be claimable by a (claude, ABC) wait.
    #[test]
    fn try_claim_registry_match_isolates_providers() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};
        let session = "wait-iso-ABC";
        for p in ["claude", "codex"] {
            if let Some(k) = RegistryKey::new(p, Some(session), None) {
                let _ = global().claim_once(k);
            }
        }
        let codex_key = RegistryKey::new("codex", Some(session), None).unwrap();
        global().deliver(
            codex_key,
            make_event("codex", session, HookEventKind::Stop, json!({})),
        );
        // A claude wait for the same session id must not see the codex Stop.
        assert!(try_claim_registry_match(Some("claude"), Some(session), "stop", None).is_none());
        // The codex wait does see it.
        assert!(try_claim_registry_match(Some("codex"), Some(session), "stop", None).is_some());
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
