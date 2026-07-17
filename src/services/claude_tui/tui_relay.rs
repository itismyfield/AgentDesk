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

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use chrono::{DateTime, Utc};
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

static SEND_NOT_BEFORE: OnceLock<Mutex<HashMap<String, DateTime<Utc>>>> = OnceLock::new();

/// Buffer name used when pasting send-text. Each request gets its own buffer
/// so concurrent sends to different sessions cannot stomp each other.
fn allocate_buffer_name() -> String {
    format!("adk-tui-send-{}", uuid::Uuid::new_v4().simple())
}

fn send_not_before_map() -> &'static Mutex<HashMap<String, DateTime<Utc>>> {
    SEND_NOT_BEFORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_send_not_before(session_name: &str, not_before: DateTime<Utc>) {
    if let Ok(mut map) = send_not_before_map().lock() {
        map.insert(session_name.to_string(), not_before);
    }
}

fn last_send_not_before(session_name: &str) -> Option<DateTime<Utc>> {
    send_not_before_map()
        .lock()
        .ok()
        .and_then(|map| map.get(session_name).copied())
}

fn resolve_wait_not_before(
    session_name: &str,
    explicit: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    explicit.or_else(|| last_send_not_before(session_name))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<Utc>>,
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
    /// Optional turn-boundary lower bound (RFC3339 / ISO-8601 wall-clock).
    ///
    /// #tui-hook-ttl-buffer: a `/tui/wait` MUST NOT be satisfied by a Stop (or
    /// token) hook that arrived *before* the caller's turn began, otherwise a
    /// previous turn's still-buffered (un-claimed, within-TTL) Stop can wake a
    /// brand-new wait. `WaitRequest` itself carries no turn id, so the caller —
    /// which is the only party that knows when it submitted the turn — passes the
    /// turn-start wall clock here. Any buffered/broadcast event with
    /// `received_at < not_before` is then rejected by `event_matches`, applied
    /// uniformly to both the registry-replay claim and the live broadcast loop.
    ///
    /// Optional and additive: callers that omit it keep the prior behaviour
    /// (any in-TTL event matches), so this is backward compatible.
    #[serde(default)]
    pub not_before: Option<DateTime<Utc>>,
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
    // P3 #4616: `handle_send_with_backend` acquires the per-pane composer mutation
    // lock (a `std::sync::Mutex`) and holds it across the blocking tmux paste +
    // Enter, which can wait several seconds behind a busy-pane auto `/compact`.
    // Blocking a tokio worker thread on a std Mutex starves the async runtime, so
    // run the whole blocking section on a blocking thread — matching the other
    // composer holders, which already offload via `spawn_blocking`
    // (`claude_stop_delivery`, `claude_compact_trigger`).
    let outcome =
        tokio::task::spawn_blocking(move || handle_send_with_backend(req, backend.as_ref())).await;
    match outcome {
        Ok(response) => response,
        Err(join_error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(error_json(&format!(
                "tui send worker join error: {join_error}"
            ))),
        ),
    }
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
    // F1: hold the SAME per-pane composer mutation lock `/compact` steering uses
    // across the paste + Enter, so a `/tui/send` and a busy-pane auto `/compact`
    // cannot interleave their key sends — otherwise the two could co-mingle into
    // a single corrupt `/compact<user text>` submission (or `/compact` could be
    // Enter-submitted mid-paste). This HTTP handler holds no other lock, so the
    // composer lock is the outermost acquisition here (one-directional order, no
    // deadlock). The hold is narrowed to just the paste + Enter mutation.
    let mutation = crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
        &session_name,
        || -> Result<(bool, Option<DateTime<Utc>>), (StatusCode, Json<Value>)> {
            if !req.text.is_empty() {
                let buffer_name = allocate_buffer_name();
                backend
                    .load_buffer(&buffer_name, &req.text)
                    .map_err(|error| ok_error_json(&format!("tmux load-buffer failed: {error}")))?;

                // `paste-buffer -p -r -d` pastes in bracketed-paste mode (so the
                // TUI recognises it as a paste, not many tiny keystrokes),
                // preserves LF (so it does NOT get auto-Enter'd by tmux), and
                // deletes the buffer after pasting. A failed paste skips
                // best-effort orphan-buffer cleanup: tmux removes buffers when the
                // server exits and we never reuse the UUID-suffixed name.
                backend
                    .paste_buffer(&session_name, &buffer_name, true)
                    .map_err(|error| {
                        ok_error_json(&format!("tmux paste-buffer failed: {error}"))
                    })?;
            }

            let mut submitted = false;
            let mut not_before = None;
            if req.submit {
                let turn_boundary = Utc::now();
                backend.send_enter(&session_name).map_err(|error| {
                    ok_error_json(&format!("tmux send-keys Enter failed: {error}"))
                })?;
                submitted = true;
                not_before = Some(turn_boundary);
                remember_send_not_before(&session_name, turn_boundary);
            }
            Ok((submitted, not_before))
        },
    );
    let (submitted, not_before) = match mutation {
        Ok(values) => values,
        Err(response) => return response,
    };

    let resp = SendResponse {
        ok: true,
        session_name,
        bytes,
        submitted,
        not_before,
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

    let wait_not_before = resolve_wait_not_before(&session_name, req.not_before);
    let started = Instant::now();

    // #tui-hook-ttl-buffer (REQ-002/REQ-005/REQ-006): SUBSCRIBE to the broadcast
    // BEFORE attempting the registry claim. The claim only sees events buffered
    // up to the moment it runs; a Stop that arrives AFTER the claim but BEFORE we
    // subscribe would otherwise be fed to the registry (for a later request) with
    // no live subscriber here, leaving this wait to time out. Subscribing first
    // guarantees any post-claim event is delivered to the broadcast loop below,
    // so the only events the claim must rescue are the ones already buffered when
    // it runs — closing the missed-event window in both directions.
    let mut rx = subscribe_hook_events();

    // Now claim the registry key for events that landed in the early window
    // (after the previous turn drained the channel but before this subscriber
    // existed). `claim_matching_once` consumes ONLY the matching event and leaves
    // any non-matching buffered events for a later waiter. When the registry is
    // disabled via the rollback flag, this is a no-op and the behaviour reverts
    // to the pure broadcast wait.
    if let Some(early) = try_claim_registry_match(
        req.provider.as_deref(),
        req.session_id.as_deref(),
        &until_mode,
        token.as_deref(),
        wait_not_before,
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
                    wait_not_before,
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
    not_before: Option<DateTime<Utc>>,
) -> Option<HookEvent> {
    use crate::services::claude_tui::hook_registry;
    if !hook_registry::registry_enabled() {
        return None;
    }
    // Registry replay must be tied to a concrete turn lower-bound. Without it,
    // a previous turn's still-in-TTL Stop/token payload for the same provider
    // session can satisfy the next wait before the live broadcast path sees any
    // new assistant output.
    let not_before = not_before?;
    let provider = want_provider.map(str::trim).filter(|p| !p.is_empty())?;
    let session_id = want_session_id.map(str::trim).filter(|s| !s.is_empty())?;
    let key = hook_registry::RegistryKey::new(provider, Some(session_id), None)?;
    // `claim_matching_once` consumes ONLY the freshest event satisfying THIS
    // wait's filter and re-buffers every other fresh event, so a buffered Stop or
    // a different `until=token` payload is not discarded for a later waiter on the
    // same session. (The old `claim_once` drained the whole buffer up front and
    // dropped unmatched events.) The closure is the same predicate the live
    // broadcast loop applies, keeping registry replay and live waits consistent.
    let until_mode = until_mode.to_string();
    let token = token.map(str::to_string);
    hook_registry::global().claim_matching_once(key, |event| {
        event_matches_registry_replay(
            event,
            want_provider,
            want_session_id,
            &until_mode,
            token.as_deref(),
            Some(not_before),
        )
    })
}

fn event_matches_registry_replay(
    event: &HookEvent,
    want_provider: Option<&str>,
    want_session_id: Option<&str>,
    until_mode: &str,
    token: Option<&str>,
    not_before: Option<DateTime<Utc>>,
) -> bool {
    if until_mode == "token" && matches!(event.kind, HookEventKind::UserPromptSubmit) {
        return false;
    }
    event_matches(
        event,
        want_provider,
        want_session_id,
        until_mode,
        token,
        not_before,
    )
}

fn event_matches(
    event: &HookEvent,
    want_provider: Option<&str>,
    want_session_id: Option<&str>,
    until_mode: &str,
    token: Option<&str>,
    not_before: Option<DateTime<Utc>>,
) -> bool {
    // #tui-hook-ttl-buffer (turn-boundary gate): reject any event that arrived
    // before the caller's turn started. This is the lower bound that stops a
    // previous turn's still-buffered Stop from satisfying a new wait. Applied
    // first so it gates BOTH the registry-replay claim and the live broadcast
    // loop (both funnel through this predicate). Callers that omit `not_before`
    // keep the prior any-in-TTL-event behaviour.
    if let Some(min) = not_before {
        if event.received_at < min {
            return false;
        }
    }
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
        assert!(body.0["not_before"].as_str().is_some());
    }

    #[test]
    fn handle_send_records_not_before_for_followup_waits() {
        let mut req = send_request("hello", true);
        req.session_name = "not-before-session".to_string();
        let (status, body) = handle_send_with_backend(req, &FakeSendBackend);

        assert_eq!(status, StatusCode::OK);
        let recorded = body.0["not_before"]
            .as_str()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .expect("send response should include not_before");
        assert_eq!(
            resolve_wait_not_before("not-before-session", None),
            Some(recorded)
        );

        let explicit = recorded + chrono::Duration::seconds(5);
        assert_eq!(
            resolve_wait_not_before("not-before-session", Some(explicit)),
            Some(explicit),
            "caller-supplied not_before must override the remembered send boundary"
        );
    }

    /// F1 mutation guard: `/tui/send` must hold the per-pane composer mutation
    /// lock across its paste + Enter, so it cannot interleave key sends with a
    /// busy-pane auto `/compact`. While a simulated `/compact` holds the composer
    /// lock for the session, the handler must NOT reach its paste stage; it
    /// proceeds only after the lock releases. Reverting the composer-lock wrapping
    /// in `handle_send_with_backend` lets the paste run immediately even while the
    /// lock is held, failing the `recv_timeout(..).is_err()` assertion.
    #[cfg(unix)]
    #[test]
    fn tui_send_holds_composer_lock_across_paste_and_enter() {
        use std::sync::mpsc;
        use std::time::Duration;

        struct SignalingBackend {
            paste_reached: mpsc::Sender<()>,
        }
        impl SendBackend for SignalingBackend {
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
                self.paste_reached.send(()).expect("signal paste reached");
                Ok(())
            }
            fn send_enter(&self, _session_name: &str) -> Result<(), String> {
                Ok(())
            }
        }

        let session = format!("adk-tui-send-lock-{}", uuid::Uuid::new_v4());
        let (holding_tx, holding_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (paste_tx, paste_rx) = mpsc::channel();

        // Simulate `/compact` steering holding the composer lock for this session.
        let lock_session = session.clone();
        std::thread::spawn(move || {
            crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                &lock_session,
                || {
                    holding_tx.send(()).expect("signal compact holding lock");
                    release_rx.recv().expect("await release");
                },
            );
        });
        holding_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("compact must acquire the composer lock first");

        // Fire `/tui/send` on another thread; it must block on the composer lock.
        let handler_session = session.clone();
        std::thread::spawn(move || {
            let req = SendRequest {
                session_name: handler_session,
                text: "hello".to_string(),
                submit: true,
            };
            let backend = SignalingBackend {
                paste_reached: paste_tx,
            };
            let _ = handle_send_with_backend(req, &backend);
        });

        assert!(
            paste_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "/tui/send paste must wait behind the /compact composer lock"
        );
        release_tx.send(()).expect("release compact");
        paste_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("/tui/send paste proceeds once the composer lock releases");
    }

    /// P3 #4616 regression: the async `handle_send` must offload its blocking
    /// composer-lock wait (`handle_send_with_backend` holds a `std::sync::Mutex`
    /// across the tmux paste + Enter) onto a blocking thread via `spawn_blocking`,
    /// so it never blocks a tokio runtime worker. On a current-thread runtime, a
    /// concurrently-spawned canary task must still make progress WHILE the handler
    /// is parked on a composer lock held elsewhere.
    ///
    /// Guard removal: reverting `handle_send` to call `handle_send_with_backend`
    /// directly (no `spawn_blocking`) blocks the single runtime worker on the std
    /// Mutex for the whole composer hold, so the canary cannot be polled until the
    /// lock releases — the observer samples `false` and this test fails. The
    /// composer hold is time-bounded on an independent std thread, so the pre-fix
    /// behaviour fails cleanly rather than hanging.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn handle_send_offloads_blocking_composer_wait_off_the_runtime_worker() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;

        const HOLD_MS: u64 = 200;

        let session = format!("adk-tui-send-p3-{}", uuid::Uuid::new_v4());

        // A busy-pane `/compact` holds the composer lock for `HOLD_MS`, released on
        // an independent std timer so the pre-fix path fails cleanly (never hangs).
        let (holding_tx, holding_rx) = mpsc::channel();
        let lock_session = session.clone();
        std::thread::spawn(move || {
            crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                &lock_session,
                || {
                    holding_tx
                        .send(())
                        .expect("signal compact holding composer");
                    std::thread::sleep(Duration::from_millis(HOLD_MS));
                },
            );
        });
        holding_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("compact must acquire the composer lock first");

        // Canary task: flips the flag the moment the runtime worker polls it.
        let canary = std::sync::Arc::new(AtomicBool::new(false));
        let canary_task = std::sync::Arc::clone(&canary);
        tokio::spawn(async move {
            canary_task.store(true, Ordering::SeqCst);
        });

        // Observer runs OFF the runtime: it samples the canary mid-hold-window, so
        // it can witness worker starvation the starved runtime could not report.
        let observer_canary = std::sync::Arc::clone(&canary);
        let (observed_tx, observed_rx) = mpsc::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(HOLD_MS / 2));
            observed_tx
                .send(observer_canary.load(Ordering::SeqCst))
                .expect("report mid-window canary sample");
        });

        // Drive the async handler; awaiting it must free the worker (fix) so the
        // runtime can poll the canary while the blocking paste waits off-worker.
        let backend: Arc<dyn SendBackend> = Arc::new(FakeSendBackend);
        let req = SendRequest {
            session_name: session.clone(),
            text: "hello".to_string(),
            submit: true,
        };
        let (status, body) = handle_send(State(backend), Json(req)).await;

        let observed_mid_window = observed_rx
            .recv()
            .expect("observer must report the mid-window canary sample");
        assert!(
            observed_mid_window,
            "the current-thread runtime worker must stay free to poll other tasks while /tui/send is parked on the composer lock"
        );
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["ok"], true);
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

    fn replay_lower_bound() -> Option<DateTime<Utc>> {
        Some(Utc::now() - chrono::Duration::seconds(1))
    }

    #[test]
    fn stop_event_matches_default() {
        let event = make_event("claude", "abc", HookEventKind::Stop, json!({}));
        assert!(event_matches(&event, None, None, "stop", None, None));
    }

    #[test]
    fn subagent_stop_also_matches_stop_mode() {
        let event = make_event("claude", "abc", HookEventKind::SubagentStop, json!({}));
        assert!(event_matches(&event, None, None, "stop", None, None));
    }

    #[test]
    fn unrelated_event_does_not_match_stop() {
        let event = make_event("claude", "abc", HookEventKind::UserPromptSubmit, json!({}));
        assert!(!event_matches(&event, None, None, "stop", None, None));
    }

    #[test]
    fn provider_filter_rejects_mismatch() {
        let event = make_event("codex", "abc", HookEventKind::Stop, json!({}));
        assert!(!event_matches(
            &event,
            Some("claude"),
            None,
            "stop",
            None,
            None
        ));
        assert!(event_matches(
            &event,
            Some("codex"),
            None,
            "stop",
            None,
            None
        ));
    }

    #[test]
    fn session_id_filter_rejects_mismatch() {
        let event = make_event("claude", "abc", HookEventKind::Stop, json!({}));
        assert!(!event_matches(
            &event,
            None,
            Some("xyz"),
            "stop",
            None,
            None
        ));
        assert!(event_matches(&event, None, Some("abc"), "stop", None, None));
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
            Some("marker-XYZ"),
            None,
        ));
        assert!(!event_matches(
            &event,
            None,
            None,
            "token",
            Some("absent"),
            None
        ));
    }

    #[test]
    fn token_mode_requires_non_empty_token() {
        let event = make_event("claude", "abc", HookEventKind::UserPromptSubmit, json!({}));
        assert!(!event_matches(&event, None, None, "token", None, None));
        assert!(!event_matches(&event, None, None, "token", Some(""), None));
    }

    // -------- #tui-hook-ttl-buffer: turn-boundary lower bound (`not_before`) ----

    /// A Stop that arrived BEFORE the caller's turn began (`received_at <
    /// not_before`) must NOT satisfy the wait — this is the turn-boundary gate
    /// that stops a previous turn's still-buffered, un-claimed, within-TTL Stop
    /// from waking a brand-new `/tui/wait`. The same predicate gates both the
    /// registry-replay claim and the live broadcast loop, so closing it here
    /// closes the stale-Stop replay race deferred from the earlier fix pass.
    #[test]
    fn stale_stop_before_turn_start_is_rejected_by_not_before() {
        let turn_start = Utc::now();
        // Stop from the *previous* turn: stamped one second before the boundary.
        let mut stale = make_event("claude", "abc", HookEventKind::Stop, json!({}));
        stale.received_at = turn_start - chrono::Duration::seconds(1);

        // Without a lower bound the stale Stop matches (legacy behaviour).
        assert!(event_matches(&stale, None, None, "stop", None, None));
        // With the turn boundary it is rejected: it predates this turn.
        assert!(!event_matches(
            &stale,
            None,
            None,
            "stop",
            None,
            Some(turn_start),
        ));
    }

    /// A Stop that arrives at or after the turn boundary still satisfies the
    /// wait — `not_before` only rejects strictly-older events, so a fresh Stop
    /// (including one stamped exactly at the boundary) is honoured.
    #[test]
    fn fresh_stop_at_or_after_turn_start_still_matches_with_not_before() {
        let turn_start = Utc::now();

        // Exactly at the boundary: inclusive, must match.
        let mut at_boundary = make_event("claude", "abc", HookEventKind::Stop, json!({}));
        at_boundary.received_at = turn_start;
        assert!(event_matches(
            &at_boundary,
            None,
            None,
            "stop",
            None,
            Some(turn_start),
        ));

        // After the boundary: must match.
        let mut after = make_event("claude", "abc", HookEventKind::Stop, json!({}));
        after.received_at = turn_start + chrono::Duration::seconds(1);
        assert!(event_matches(
            &after,
            None,
            None,
            "stop",
            None,
            Some(turn_start),
        ));
    }

    /// `WaitRequest::not_before` is optional and additive: a request body that
    /// omits it deserializes to `None`, preserving the prior any-in-TTL-event
    /// behaviour for existing callers.
    #[test]
    fn wait_request_not_before_defaults_to_none() {
        let req: WaitRequest =
            serde_json::from_value(json!({ "session_name": "s" })).expect("deserialize");
        assert!(req.not_before.is_none());

        let req: WaitRequest = serde_json::from_value(
            json!({ "session_name": "s", "not_before": "2026-06-16T00:00:00Z" }),
        )
        .expect("deserialize with not_before");
        assert!(req.not_before.is_some());
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

        let matched = try_claim_registry_match(
            Some(provider),
            Some(session),
            "stop",
            None,
            replay_lower_bound(),
        );
        assert!(matched.is_some(), "buffered early Stop must be claimed");
        assert_eq!(matched.unwrap().kind, HookEventKind::Stop);

        // Single-consumption: a second claim finds nothing.
        let second = try_claim_registry_match(
            Some(provider),
            Some(session),
            "stop",
            None,
            replay_lower_bound(),
        );
        assert!(
            second.is_none(),
            "claimed Stop must not replay to a later wait"
        );
    }

    #[test]
    fn try_claim_registry_match_requires_turn_lower_bound() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};
        let provider = "claude";
        let session = "wait-requires-bound";
        if let Some(k) = RegistryKey::new(provider, Some(session), None) {
            let _ = global().claim_once(k);
        }
        let key = RegistryKey::new(provider, Some(session), None).unwrap();
        global().deliver(
            key,
            make_event(provider, session, HookEventKind::Stop, json!({})),
        );

        assert!(
            try_claim_registry_match(Some(provider), Some(session), "stop", None, None).is_none()
        );
        assert!(
            try_claim_registry_match(
                Some(provider),
                Some(session),
                "stop",
                None,
                replay_lower_bound(),
            )
            .is_some()
        );
    }

    /// REQ-005: a `/tui/wait` caller that omits `session_id` accepts "any
    /// session", which the per-key registry cannot represent — so it falls
    /// through to the broadcast wait (registry returns `None`).
    #[test]
    fn try_claim_registry_match_skips_when_no_session_id() {
        assert!(
            try_claim_registry_match(Some("claude"), None, "stop", None, replay_lower_bound())
                .is_none()
        );
        assert!(
            try_claim_registry_match(None, Some("sid"), "stop", None, replay_lower_bound())
                .is_none()
        );
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
        assert!(
            try_claim_registry_match(
                Some("claude"),
                Some(session),
                "stop",
                None,
                replay_lower_bound(),
            )
            .is_none()
        );
        // The codex wait does see it.
        assert!(
            try_claim_registry_match(
                Some("codex"),
                Some(session),
                "stop",
                None,
                replay_lower_bound(),
            )
            .is_some()
        );
    }

    /// Do-not-discard-unmatched-events: a `until=token` wait whose token does not
    /// match the buffered events must NOT drain/discard them — a subsequent Stop
    /// wait for the same session must still be able to replay the buffered Stop.
    #[test]
    fn try_claim_registry_match_does_not_discard_unmatched_events() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};
        let provider = "claude";
        let session = "wait-no-discard";
        if let Some(k) = RegistryKey::new(provider, Some(session), None) {
            let _ = global().claim_once(k);
        }
        let key = RegistryKey::new(provider, Some(session), None).unwrap();
        // Buffer a Stop with an empty body (no token).
        global().deliver(
            key,
            make_event(provider, session, HookEventKind::Stop, json!({})),
        );

        // A token wait with a non-matching token finds nothing.
        let token_miss = try_claim_registry_match(
            Some(provider),
            Some(session),
            "token",
            Some("absent-token"),
            replay_lower_bound(),
        );
        assert!(
            token_miss.is_none(),
            "non-matching token wait matches nothing"
        );

        // The buffered Stop must still be claimable by a later Stop wait — the
        // failed token wait must not have discarded it.
        let stop_hit = try_claim_registry_match(
            Some(provider),
            Some(session),
            "stop",
            None,
            replay_lower_bound(),
        );
        assert!(
            stop_hit.is_some(),
            "unmatched token wait must not discard the buffered Stop"
        );
    }

    /// A token wait consumes only the matching token payload and leaves a buffered
    /// Stop for a subsequent Stop wait on the same session.
    #[test]
    fn try_claim_registry_match_token_leaves_stop_for_later_wait() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};
        let provider = "claude";
        let session = "wait-token-then-stop";
        if let Some(k) = RegistryKey::new(provider, Some(session), None) {
            let _ = global().claim_once(k);
        }
        let key = RegistryKey::new(provider, Some(session), None).unwrap();
        global().deliver(
            key.clone(),
            make_event(
                provider,
                session,
                HookEventKind::Notification,
                json!({ "text": "needle-42 present" }),
            ),
        );
        global().deliver(
            key,
            make_event(provider, session, HookEventKind::Stop, json!({})),
        );

        // Token wait consumes only the token payload.
        let token_hit = try_claim_registry_match(
            Some(provider),
            Some(session),
            "token",
            Some("needle-42"),
            replay_lower_bound(),
        );
        assert!(token_hit.is_some(), "token payload must be claimable");

        // The Stop is still buffered and claimable.
        let stop_hit = try_claim_registry_match(
            Some(provider),
            Some(session),
            "stop",
            None,
            replay_lower_bound(),
        );
        assert!(
            stop_hit.is_some(),
            "the Stop must survive a token-mode claim on the same session"
        );
    }

    #[test]
    fn try_claim_registry_match_token_ignores_prompt_submit_payload() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};
        let provider = "claude";
        let session = "wait-token-prompt-submit";
        if let Some(k) = RegistryKey::new(provider, Some(session), None) {
            let _ = global().claim_once(k);
        }
        let key = RegistryKey::new(provider, Some(session), None).unwrap();
        global().deliver(
            key.clone(),
            make_event(
                provider,
                session,
                HookEventKind::UserPromptSubmit,
                json!({ "prompt": "marker-from-user-prompt" }),
            ),
        );
        global().deliver(
            key,
            make_event(
                provider,
                session,
                HookEventKind::Notification,
                json!({ "text": "marker-from-assistant" }),
            ),
        );

        let prompt_submit = try_claim_registry_match(
            Some(provider),
            Some(session),
            "token",
            Some("marker-from-user-prompt"),
            replay_lower_bound(),
        );
        assert!(
            prompt_submit.is_none(),
            "registry replay must not satisfy token waits from prompt-submit echo"
        );

        let assistant_token = try_claim_registry_match(
            Some(provider),
            Some(session),
            "token",
            Some("marker-from-assistant"),
            replay_lower_bound(),
        );
        assert!(
            assistant_token.is_some(),
            "non-prompt-submit token payload remains claimable"
        );
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
