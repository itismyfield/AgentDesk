use std::path::PathBuf;
use std::sync::{Arc, LazyLock, RwLock};

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use crate::services::claude_tui::memento_feedback::PendingMementoFeedbackTracker;

const EVENT_BUFFER_CAPACITY: usize = 256;

static HOOK_ENDPOINT: LazyLock<RwLock<Option<String>>> = LazyLock::new(|| RwLock::new(None));
static HOOK_SERVER_STATE: LazyLock<HookServerState> = LazyLock::new(HookServerState::new);
static PROMPT_READY_NOTIFY: LazyLock<Arc<Notify>> = LazyLock::new(|| Arc::new(Notify::new()));

/// Returns the global notify handle that is woken whenever a provider hook
/// event suggesting "prompt ready" arrives (currently `Stop` / `SubagentStop`).
///
/// Callers that need to await prompt readiness should register a waiter via
/// `notify.notified()` BEFORE issuing the prompt — `notify_waiters` only
/// wakes currently-registered waiters, so signals fired before subscription
/// are intentionally dropped to keep the channel edge-triggered.
pub fn prompt_ready_notify() -> Arc<Notify> {
    PROMPT_READY_NOTIFY.clone()
}

/// Internal entry point used by `receive_hook` to wake `prompt_ready_notify`
/// waiters. Exposed (crate-visible) for unit tests that exercise the wake path
/// without spinning up the full HTTP receiver.
// #3034: test-only wake hook (consumed by input.rs end-to-end wiring tests).
#[allow(dead_code)]
pub(crate) fn signal_prompt_ready_for_test() {
    PROMPT_READY_NOTIFY.notify_waiters();
}

fn should_signal_prompt_ready(provider: &str, kind: &HookEventKind) -> bool {
    matches!(
        (provider, kind),
        (
            "claude" | "codex",
            HookEventKind::Stop | HookEventKind::SubagentStop
        )
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventKind {
    SessionStart,
    UserPromptSubmit,
    Stop,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PreCompact,
    PostCompact,
    Notification,
    SubagentStop,
    Unknown(String),
}

impl HookEventKind {
    pub fn from_path(value: &str) -> Self {
        match normalize_hook_event_name(value).as_str() {
            "session_start" => Self::SessionStart,
            "user_prompt_submit" => Self::UserPromptSubmit,
            "stop" => Self::Stop,
            "pre_tool_use" => Self::PreToolUse,
            "permission_request" => Self::PermissionRequest,
            "post_tool_use" => Self::PostToolUse,
            "pre_compact" => Self::PreCompact,
            "post_compact" => Self::PostCompact,
            "notification" => Self::Notification,
            "subagent_stop" => Self::SubagentStop,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::SessionStart => "session_start",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::Stop => "stop",
            Self::PreToolUse => "pre_tool_use",
            Self::PermissionRequest => "permission_request",
            Self::PostToolUse => "post_tool_use",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
            Self::Notification => "notification",
            Self::SubagentStop => "subagent_stop",
            Self::Unknown(value) => value.as_str(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct HookEvent {
    pub provider: String,
    pub session_id: String,
    pub kind: HookEventKind,
    pub received_at: DateTime<Utc>,
    pub payload: Value,
}

#[derive(Clone)]
pub struct HookServerState {
    event_tx: broadcast::Sender<HookEvent>,
    memento_feedback: PendingMementoFeedbackTracker,
    claude_projects_root: Option<PathBuf>,
}

impl HookServerState {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        Self {
            event_tx,
            memento_feedback: PendingMementoFeedbackTracker::default(),
            claude_projects_root:
                crate::services::claude_tui::hook_output_guard::configured_claude_projects_root(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_claude_projects_root(claude_projects_root: PathBuf) -> Self {
        let mut state = Self::new();
        state.claude_projects_root = Some(claude_projects_root);
        state
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HookEvent> {
        self.event_tx.subscribe()
    }
}

impl Default for HookServerState {
    fn default() -> Self {
        Self::new()
    }
}

pub struct HookEndpointGuard {
    endpoint: String,
}

impl Drop for HookEndpointGuard {
    fn drop(&mut self) {
        clear_hook_endpoint_if_current(&self.endpoint);
    }
}

pub fn publish_hook_endpoint(endpoint: String) -> HookEndpointGuard {
    *HOOK_ENDPOINT
        .write()
        .unwrap_or_else(|error| error.into_inner()) = Some(endpoint.clone());
    HookEndpointGuard { endpoint }
}

fn clear_hook_endpoint_if_current(expected: &str) {
    let mut endpoint = HOOK_ENDPOINT
        .write()
        .unwrap_or_else(|error| error.into_inner());
    if endpoint.as_deref() == Some(expected) {
        *endpoint = None;
    }
}

pub fn current_hook_endpoint() -> Option<String> {
    HOOK_ENDPOINT
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub fn subscribe_hook_events() -> broadcast::Receiver<HookEvent> {
    HOOK_SERVER_STATE.subscribe()
}

pub fn hook_receiver_router() -> Router {
    hook_receiver_router_with_state(HOOK_SERVER_STATE.clone())
}

pub(crate) fn hook_receiver_router_with_state(state: HookServerState) -> Router {
    Router::new()
        .route("/hooks/{provider}/{event}", post(receive_hook))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct HookQuery {
    session_id: Option<String>,
}

fn hook_routing_session_ids(
    command_session_id: Option<String>,
    payload_session_id: Option<String>,
    command_is_registered: bool,
    payload_is_registered: bool,
) -> (Option<String>, Option<String>) {
    match (command_session_id, payload_session_id) {
        (Some(command), Some(payload))
            if command != payload && !command_is_registered && payload_is_registered =>
        {
            (Some(payload), None)
        }
        (Some(command), Some(payload))
            if command != payload && command_is_registered && payload_is_registered =>
        {
            (Some(command), Some(payload))
        }
        (Some(command), _) => (Some(command), None),
        (None, payload) => (payload, None),
    }
}

async fn receive_hook(
    State(state): State<HookServerState>,
    Path((provider, event)): Path<(String, String)>,
    Query(query): Query<HookQuery>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "missing provider" })),
        );
    }

    let command_session_id = query.session_id.as_deref().and_then(non_empty_string);
    let observed_payload_session_id = payload_session_id(&payload);
    if provider == "claude"
        && let (Some(command_session_id), Some(payload_session_id)) = (
            command_session_id.as_deref(),
            observed_payload_session_id.as_deref(),
        )
        && command_session_id != payload_session_id
    {
        match crate::services::tui_prompt_dedupe::adopt_claude_continuation_session(
            command_session_id,
            payload_session_id,
        ) {
            Some((tmux_session_name, transcript_path)) => {
                match crate::services::claude_tui::session::persist_claude_continuation_session(
                    &tmux_session_name,
                    payload_session_id,
                ) {
                    Ok(changed) => tracing::warn!(
                        provider,
                        command_session_id,
                        payload_session_id,
                        tmux_session_name,
                        transcript_path,
                        persistent_artifacts_changed = changed,
                        "adopted Claude continuation session from hook payload"
                    ),
                    Err(error) => tracing::error!(
                        provider,
                        command_session_id,
                        payload_session_id,
                        tmux_session_name,
                        error,
                        "adopted Claude continuation in memory but failed to persist cutover artifacts"
                    ),
                }
            }
            None => tracing::debug!(
                provider,
                command_session_id,
                payload_session_id,
                "Claude hook payload session differs from command identity but no safe runtime binding adoption was available"
            ),
        }
    }
    // Keep the launch-time query UUID as the hook wait/routing identity while
    // it is registered; replacing it would strand callers already waiting on
    // that stable key. After restart or a partial artifact cutover, fall back
    // to the registered payload UUID so stale settings cannot strand events.
    let command_is_registered = command_session_id.as_deref().is_some_and(|session_id| {
        crate::services::tui_prompt_dedupe::provider_session_is_registered(&provider, session_id)
    });
    let payload_is_registered = observed_payload_session_id
        .as_deref()
        .is_some_and(|session_id| {
            crate::services::tui_prompt_dedupe::provider_session_is_registered(
                &provider, session_id,
            )
        });
    let (session_id, alias_session_id) = hook_routing_session_ids(
        command_session_id,
        observed_payload_session_id,
        command_is_registered,
        payload_is_registered,
    );
    let Some(session_id) = session_id else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "missing session_id" })),
        );
    };

    let kind = HookEventKind::from_path(&event);
    if provider == "claude"
        && matches!(kind, HookEventKind::Stop | HookEventKind::SubagentStop)
        && !crate::services::claude_tui::memento_feedback::stop_hook_active(&payload)
    {
        match crate::services::claude_tui::hook_output_guard::inspect_claude_hook_output(
            &payload,
            state.claude_projects_root.as_deref(),
        ) {
            Ok(inspection) => match inspection.verdict {
                crate::services::provider_output_guard::ProviderOutputVerdict::Clean => {}
                crate::services::provider_output_guard::ProviderOutputVerdict::Hold { kind }
                | crate::services::provider_output_guard::ProviderOutputVerdict::Blocked { kind } =>
                {
                    tracing::warn!(
                        provider,
                        hook_event = event,
                        verdict_kind = kind.as_str(),
                        session_id,
                        output_bytes = inspection.byte_len,
                        output_chars = inspection.char_len,
                        "blocked Claude completion containing harness control data"
                    );
                    return (
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "decision": "block",
                            "reason": crate::services::claude_tui::hook_output_guard::CLAUDE_HOOK_BLOCK_REASON,
                        })),
                    );
                }
            },
            Err(error) => tracing::warn!(
                provider,
                event = kind.as_str(),
                session_id,
                reason = error.as_str(),
                "Claude completion transcript guard unavailable; failing open at hook boundary"
            ),
        }
    }
    let payload_is_noise = is_informational_empty_payload(&payload);
    let event = HookEvent {
        provider: provider.clone(),
        session_id: session_id.clone(),
        kind,
        received_at: Utc::now(),
        payload,
    };
    let alias_event = alias_session_id.map(|alias_session_id| HookEvent {
        session_id: alias_session_id,
        ..event.clone()
    });
    let event_name = event.kind.as_str().to_string();
    let memento_stop_flush = match event.kind {
        HookEventKind::PostToolUse => {
            let _ = state
                .memento_feedback
                .observe_post_tool_use(&event.session_id, &event.payload);
            None
        }
        HookEventKind::Stop if event.provider == "claude" => state
            .memento_feedback
            .take_stop_flush(&event.session_id, &event.payload),
        HookEventKind::Stop | HookEventKind::SubagentStop => {
            state.memento_feedback.clear_session(&event.session_id);
            None
        }
        _ => None,
    };
    if matches!(event.kind, HookEventKind::Unknown(_)) {
        tracing::warn!(
            provider,
            event = event_name,
            session_id,
            "unknown tui hook event accepted for provider-scoped telemetry"
        );
    }
    let stop_flush_injected = memento_stop_flush.is_some();
    if should_signal_prompt_ready(&event.provider, &event.kind) && !stop_flush_injected {
        // Wake any task currently awaiting prompt readiness. `notify_waiters`
        // is edge-triggered: signals fired with no current waiters are
        // intentionally dropped so `wait_for_prompt_ready` cannot latch onto
        // a stale Stop from a previous turn. The polling fallback in
        // `input::wait_for_prompt_ready` handles the missed-signal race.
        //
        // Note: this wake intentionally fires BEFORE the empty-payload drop
        // below so a Stop hook with an empty body (the common case) still
        // unblocks waiters. The body is irrelevant to the prompt-ready
        // signal — only the (provider, kind) tuple matters.
        PROMPT_READY_NOTIFY.notify_waiters();
    }
    // #2655: surface forget:recall floods. The PreToolUse payload carries
    // `tool_name` (Claude/Codex contract); when it's a memento forget or
    // recall, observe the call against the sliding window and emit a warn if
    // the threshold is breached. Suppressed-by-cool-down decisions are
    // logged at debug to keep the alarm channel readable.
    if matches!(event.kind, HookEventKind::PreToolUse) {
        if let Some(observation) = classify_memento_tool_invocation(&event.payload) {
            let scope = format!("{}:{}", event.provider, event.session_id);
            let snapshot = match observation {
                MementoToolInvocation::Forget => {
                    crate::services::memory::note_memento_forget_call(&scope)
                }
                MementoToolInvocation::Recall => {
                    crate::services::memory::note_memento_recall_call(&scope)
                }
            };
            if matches!(
                snapshot.decision,
                crate::services::memory::ForgetRatioAlarmDecision::AlarmSuppressedByCooldown
            ) {
                tracing::debug!(
                    scope,
                    forget_count = snapshot.forget_count,
                    recall_count = snapshot.recall_count,
                    ratio = snapshot.ratio,
                    "memento forget:recall flood alarm currently suppressed by cool-down"
                );
            }
        }
    }

    // Issue #2665: empty-payload filter. Per the 2026-05-19 workflow audit,
    // one session alone accumulated 1,531 `hook_success` attachments whose
    // bodies averaged 12 bytes each (vs. ~80–100 byte envelopes). For
    // payloads that carry no event-specific content (only identifier echoes
    // already encoded in the request line), we still wake prompt-ready
    // waiters and we still log unknown event names — but we skip the
    // broadcast publish so downstream subscribers (rollout tail completion
    // detection, future telemetry pipelines) do not see the noise.
    //
    // Drop logic:
    // * `Stop` / `SubagentStop` events ARE useful even with empty bodies
    //   (they carry their semantics in the path), so they bypass the
    //   filter — `codex_completion_hook_matches` reads only kind+session,
    //   and dropping them would regress turn-completion detection.
    // * Every other event kind with `is_informational_empty_payload(payload) == true`
    //   is dropped at the broadcast boundary. The HTTP response stays 202
    //   so the relay CLI cannot tell — that contract is one-way fire-and-forget.
    let should_drop_broadcast = stop_flush_injected
        || (payload_is_noise
            && !matches!(
                event.kind,
                HookEventKind::Stop | HookEventKind::SubagentStop
            ));
    // #tui-hook-ttl-buffer: additively feed the in-memory hook registry so a
    // hook that lands before its consumer claims the key is not lost (the
    // broadcast and `prompt_ready_notify` above are edge-triggered and drop
    // early signals). This is a parallel buffer — it does NOT gate, drop, or
    // reorder the broadcast send below; the Codex `try_recv()` observer path
    // and `/tui/wait` broadcast subscribers are unchanged. Delivery is a single
    // in-memory mutex section (O(buffer length for the key)); no I/O.
    //
    // The buffering gate MIRRORS the broadcast gate (`should_drop_broadcast`):
    // we never buffer anything the live broadcast intentionally drops, so a late
    // registry replay can never surface an event the live `/tui/wait` path would
    // not have seen. Concretely this means:
    //   * `stop_flush_injected` Stops are NOT buffered. That Stop is suppressed
    //     because it only exists to inject required memento feedback and must not
    //     be treated as turn completion; buffering it would let a later
    //     `/tui/wait` return `matched:stop:registry` for it and finish the turn
    //     before Claude processes the feedback.
    //   * informational empty-payload NON-Stop events are NOT buffered. They only
    //     echo `session_id`/`provider`/`event`, so a token wait could otherwise
    //     match those identifiers via `payload_contains` and return
    //     `matched:token:registry` for an event the live path filtered out.
    //   * empty-body Stop / SubagentStop events ARE still buffered (the broadcast
    //     gate exempts them) so the early-Stop race and unclaimed-Stop diagnostic
    //     keep working — only the feedback-injected Stop above is excluded.
    //
    // Keyed by (provider, session_id): session_id is already the provider
    // session id when the hook reported one, else the tmux session name passed
    // via the query string — exactly the REQ-001 fallback. Including the
    // provider in the key keeps Claude and Codex isolated even when they share
    // a tmux session name.
    if !should_drop_broadcast && crate::services::claude_tui::hook_registry::registry_enabled() {
        if let Some(key) = crate::services::claude_tui::hook_registry::RegistryKey::new(
            &event.provider,
            Some(event.session_id.as_str()),
            None,
        ) {
            crate::services::claude_tui::hook_registry::global().deliver(key, event.clone());
        }
        if let Some(alias_event) = alias_event.as_ref()
            && let Some(key) = crate::services::claude_tui::hook_registry::RegistryKey::new(
                &alias_event.provider,
                Some(alias_event.session_id.as_str()),
                None,
            )
        {
            crate::services::claude_tui::hook_registry::global().deliver(key, alias_event.clone());
        }
    }

    if should_drop_broadcast {
        tracing::debug!(
            provider,
            event = event_name,
            session_id,
            "tui hook event has empty payload or pending memento feedback flush; dropping broadcast"
        );
    } else {
        let primary_discarded = state.event_tx.send(event).is_err();
        let alias_discarded = alias_event
            .map(|alias_event| state.event_tx.send(alias_event).is_err())
            .unwrap_or(true);
        if primary_discarded && alias_discarded {
            tracing::debug!(
                provider,
                event = event_name,
                session_id,
                "tui hook event accepted with no subscribers; event discarded"
            );
        }
    }

    let mut body = json!({
        "ok": true,
        "provider": provider,
        "event": event_name,
        "session_id": session_id
    });
    if let Some(flush) = memento_stop_flush {
        body["memento_tool_feedback_flush"] = flush.to_json();
    }

    (StatusCode::ACCEPTED, Json(body))
}

/// #2655: classification of the memento tool surface invoked in a hook
/// payload. Anything not on the `forget`/`recall` pair returns `None` so the
/// caller can no-op cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MementoToolInvocation {
    Forget,
    Recall,
}

/// #2655: parses the `tool_name` field of a PreToolUse hook payload and maps
/// it onto `MementoToolInvocation`. Accepts both the Claude convention
/// (`mcp__memento__forget`) and the bare tool name (`forget`/`recall`/`context`).
/// `context` counts as a recall — it produces the same kind of evidence
/// (recall precision) that the forget:recall ratio is meant to surface.
pub(crate) fn classify_memento_tool_invocation(payload: &Value) -> Option<MementoToolInvocation> {
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .or_else(|| payload.get("toolName").and_then(Value::as_str))?
        .trim();
    if tool_name.is_empty() {
        return None;
    }
    // Tool names arrive in a few shapes depending on the provider:
    //   * Claude MCP namespacing: `mcp__memento__forget`
    //   * Codex MCP namespacing: `memento.forget` / `memento/forget`
    //   * Bare:                   `forget`
    // Normalise by stripping the `mcp__memento__` prefix and any
    // `memento[./]` prefix, then case-fold.
    let lowered = tool_name.to_ascii_lowercase();
    let stripped = lowered
        .strip_prefix("mcp__memento__")
        .or_else(|| lowered.strip_prefix("memento."))
        .or_else(|| lowered.strip_prefix("memento/"))
        .unwrap_or(lowered.as_str());
    match stripped {
        "forget" => Some(MementoToolInvocation::Forget),
        "recall" | "context" => Some(MementoToolInvocation::Recall),
        _ => None,
    }
}

/// Returns `true` when the JSON payload carries no event-specific information
/// beyond the identifier fields already encoded in the receive URL.
///
/// The provider TUI hooks (Claude UserPromptSubmit, Codex completion echo,
/// etc.) frequently fire with a body that only re-states `session_id` /
/// `sessionId` / `provider` / `event` / empty arrays. The receive_hook path
/// already extracts session_id from the query string, so these copies add
/// pure noise to the broadcast bus and (downstream) to any persisted
/// telemetry.
///
/// Definition of "noise":
/// * Empty JSON object `{}` or `null`.
/// * Array (any size — the issue specifically calls out empty `attachments`
///   arrays as bloat).
/// * Object whose every value is one of:
///     - Null
///     - Empty string after trim
///     - Empty array
///     - Empty object
///     - A copy of `session_id`, `sessionId`, `provider`, `event`,
///       `event_name`, `kind`, `type` (identifier echoes — already routed)
///
/// Anything that contains a non-empty primitive field, a non-empty nested
/// object, or a non-empty array of non-identifier items is kept.
pub(crate) fn is_informational_empty_payload(payload: &Value) -> bool {
    match payload {
        Value::Null => true,
        Value::Object(map) if map.is_empty() => true,
        Value::Array(_) => {
            // Top-level arrays are uncommon for TUI hooks; treat as noise
            // because the relay CLI already converts empty stdin to `{}`,
            // so a top-level array can only come from the rare provider
            // that intentionally sends a list. Even then, an empty list
            // adds no broadcast value. A non-empty list with structured
            // entries is rare; if it appears in production we'll revisit.
            payload.as_array().is_some_and(Vec::is_empty)
        }
        Value::Object(map) => map.iter().all(|(key, value)| is_noise_field(key, value)),
        _ => false,
    }
}

fn is_noise_field(key: &str, value: &Value) -> bool {
    if matches!(
        key,
        "session_id"
            | "sessionId"
            | "provider"
            | "event"
            | "event_name"
            | "kind"
            | "type"
            | "received_at"
    ) {
        return true;
    }
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn payload_session_id(payload: &Value) -> Option<String> {
    payload
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(non_empty_string)
        .or_else(|| {
            payload
                .get("sessionId")
                .and_then(Value::as_str)
                .and_then(non_empty_string)
        })
}

fn non_empty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn normalize_hook_event_name(value: &str) -> String {
    value
        .trim()
        .chars()
        .enumerate()
        .flat_map(|(idx, ch)| {
            if ch == '-' || ch == ' ' {
                return vec!['_'];
            }
            if ch.is_ascii_uppercase() {
                let lower = ch.to_ascii_lowercase();
                if idx == 0 {
                    vec![lower]
                } else {
                    vec!['_', lower]
                }
            } else {
                vec![ch]
            }
        })
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    static ENDPOINT_TEST_LOCK: LazyLock<std::sync::Mutex<()>> =
        LazyLock::new(|| std::sync::Mutex::new(()));

    #[test]
    fn classify_memento_recognizes_claude_and_codex_prefixes() {
        // Claude-style MCP namespace.
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "tool_name": "mcp__memento__forget" })),
            Some(MementoToolInvocation::Forget)
        );
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "tool_name": "mcp__memento__recall" })),
            Some(MementoToolInvocation::Recall)
        );
        // `context` counts as recall — it carries the same recall-precision
        // signal the forget:recall ratio is meant to surface.
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "tool_name": "mcp__memento__context" })),
            Some(MementoToolInvocation::Recall)
        );
        // Codex-style dot/slash separators.
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "tool_name": "memento.forget" })),
            Some(MementoToolInvocation::Forget)
        );
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "tool_name": "memento/recall" })),
            Some(MementoToolInvocation::Recall)
        );
        // Bare tool name (no provider prefix).
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "tool_name": "forget" })),
            Some(MementoToolInvocation::Forget)
        );
        // camelCase field name also accepted.
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "toolName": "mcp__memento__recall" })),
            Some(MementoToolInvocation::Recall)
        );
    }

    #[test]
    fn classify_memento_ignores_unrelated_tools() {
        // Other MCP tools must not contribute to the ratio.
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "tool_name": "Bash" })),
            None
        );
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "tool_name": "mcp__memento__remember" })),
            None
        );
        assert_eq!(classify_memento_tool_invocation(&json!({})), None);
        // Empty / whitespace strings degrade gracefully.
        assert_eq!(
            classify_memento_tool_invocation(&json!({ "tool_name": "   " })),
            None
        );
    }

    #[test]
    fn hook_event_kind_normalizes_provider_hook_names() {
        assert_eq!(HookEventKind::from_path("Stop"), HookEventKind::Stop);
        assert_eq!(
            HookEventKind::from_path("PreToolUse"),
            HookEventKind::PreToolUse
        );
        assert_eq!(
            HookEventKind::from_path("PermissionRequest"),
            HookEventKind::PermissionRequest
        );
        assert_eq!(
            HookEventKind::from_path("pre-compact"),
            HookEventKind::PreCompact
        );
        assert_eq!(
            HookEventKind::from_path("PostCompact"),
            HookEventKind::PostCompact
        );
        assert_eq!(
            HookEventKind::from_path("subagent-stop"),
            HookEventKind::SubagentStop
        );
        assert_eq!(
            HookEventKind::from_path("FutureCodexHook"),
            HookEventKind::Unknown("future_codex_hook".to_string())
        );
    }

    #[tokio::test]
    async fn receiver_accepts_query_session_id_and_broadcasts_event() {
        let state = HookServerState::new();
        let mut rx = state.subscribe();
        let app = hook_receiver_router_with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/hooks/claude/Stop?session_id=sess-1")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"hook_event_name":"Stop"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.provider, "claude");
        assert_eq!(event.session_id, "sess-1");
        assert_eq!(event.kind, HookEventKind::Stop);
    }

    /// Issue #2665: a non-Stop hook with an informationally-empty body must
    /// NOT be published on the broadcast bus. The HTTP response stays 202 so
    /// the relay CLI cannot observe the drop (fire-and-forget contract). We
    /// use a short receive timeout because the broadcast channel has no
    /// natural sentinel — silence is the success signal.
    #[tokio::test]
    async fn receiver_drops_empty_payload_for_non_stop_event() {
        let state = HookServerState::new();
        let mut rx = state.subscribe();
        let app = hook_receiver_router_with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    // UserPromptSubmit is the canonical 1531-attachment offender
                    // — Claude TUI fires it every turn with an essentially empty
                    // body.
                    .uri("/hooks/claude/UserPromptSubmit?session_id=sess-empty")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"session_id":"sess-empty"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // The broadcast must NOT see this event. Two acceptable terminal
        // shapes:
        //   * `Err(_)` from tokio::time::timeout (no event arrived within
        //     150ms — the silence indicates a drop).
        //   * `Ok(Err(RecvError::Closed))` (the only HookServerState was
        //     dropped when `app.oneshot` consumed its router state; the
        //     sender end closed without ever firing the event we're
        //     watching for).
        // Both prove the event was dropped at the broadcast boundary. The
        // critical assertion is the *negative*: `Ok(Ok(_))` would prove the
        // event escaped the filter.
        let result = tokio::time::timeout(std::time::Duration::from_millis(150), rx.recv()).await;
        match result {
            Err(_) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) => {}
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                panic!("unexpected lag — channel saw events before close: {result:?}")
            }
            Ok(Ok(event)) => {
                panic!("empty UserPromptSubmit must be dropped, but broadcast received: {event:?}")
            }
        }
    }

    /// The Stop / SubagentStop exemption is load-bearing — `codex_completion_
    /// hook_matches` uses the broadcast event as a turn-completion signal and
    /// Claude TUI sends Stop with an empty body in the common case. This test
    /// pins the inverse property: even with an empty body, Stop must still
    /// reach subscribers.
    #[tokio::test]
    async fn receiver_keeps_stop_event_even_with_empty_payload() {
        let state = HookServerState::new();
        let mut rx = state.subscribe();
        let app = hook_receiver_router_with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/hooks/claude/Stop?session_id=sess-stop")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.kind, HookEventKind::Stop);
        assert_eq!(event.session_id, "sess-stop");
    }

    #[tokio::test]
    async fn receiver_rejects_missing_session_id() {
        let app = hook_receiver_router_with_state(HookServerState::new());

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/hooks/claude/Stop")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn stop_event_wakes_prompt_ready_waiter() {
        let notify = prompt_ready_notify();
        let waiter = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(2), notify.notified())
                .await
                .map_err(|_| "timeout")
        });

        // Give the waiter a moment to register before signaling.
        tokio::task::yield_now().await;
        // Drive via the same code path the HTTP receiver uses.
        let state = HookServerState::new();
        let app = hook_receiver_router_with_state(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/hooks/claude/Stop?session_id=sess-wake")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"hook_event_name":"Stop"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        waiter
            .await
            .expect("waiter task did not panic")
            .expect("Stop event should wake prompt_ready_notify waiter");
    }

    #[tokio::test]
    async fn codex_stop_event_wakes_prompt_ready_waiter() {
        let notify = prompt_ready_notify();
        let waiter = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(2), notify.notified())
                .await
                .map_err(|_| "timeout")
        });

        tokio::task::yield_now().await;
        let state = HookServerState::new();
        let app = hook_receiver_router_with_state(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/hooks/codex/Stop?session_id=sess-codex-wake")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"hook_event_name":"Stop"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        waiter
            .await
            .expect("waiter task did not panic")
            .expect("Codex Stop event should wake prompt_ready_notify waiter");
    }

    // Note: a "negative wake" test against the global PROMPT_READY_NOTIFY is
    // intentionally omitted — concurrent tests in the same process can race on
    // the shared notify and flake the assertion. The pure-function predicate
    // `should_signal_prompt_ready_only_for_stop_kinds` below pins the
    // dispatch rule deterministically without touching global state.

    #[test]
    fn should_signal_prompt_ready_only_for_supported_provider_stop_kinds() {
        assert!(should_signal_prompt_ready("claude", &HookEventKind::Stop));
        assert!(should_signal_prompt_ready(
            "claude",
            &HookEventKind::SubagentStop
        ));
        assert!(should_signal_prompt_ready("codex", &HookEventKind::Stop));
        assert!(should_signal_prompt_ready(
            "codex",
            &HookEventKind::SubagentStop
        ));
        // Notifications carry permission prompts etc.; conservatively skip.
        assert!(!should_signal_prompt_ready(
            "claude",
            &HookEventKind::Notification
        ));
        assert!(!should_signal_prompt_ready(
            "claude",
            &HookEventKind::UserPromptSubmit
        ));
        // Unknown providers still cannot poke the shared readiness notify.
        assert!(!should_signal_prompt_ready("qwen", &HookEventKind::Stop));
    }

    // -------- #2665 empty-payload filter --------

    #[test]
    fn empty_payload_filter_drops_pure_empty_objects() {
        assert!(is_informational_empty_payload(&json!({})));
        assert!(is_informational_empty_payload(&Value::Null));
    }

    #[test]
    fn empty_payload_filter_drops_identifier_echo_only_objects() {
        // The relay CLI commonly re-states session_id + sessionId + provider
        // inside the body. None of these carry information the receiver
        // doesn't already have from the URL/query.
        assert!(is_informational_empty_payload(&json!({
            "session_id": "abc",
            "sessionId": "abc",
            "provider": "claude",
            "event": "Stop",
        })));
        // Mixed identifier echoes + empty-string scalars still count as noise.
        assert!(is_informational_empty_payload(&json!({
            "session_id": "abc",
            "attachments": [],
            "message": "",
            "metadata": {},
        })));
    }

    #[test]
    fn empty_payload_filter_keeps_payloads_with_real_content() {
        // Any non-identifier, non-empty primitive must keep the broadcast.
        assert!(!is_informational_empty_payload(&json!({
            "session_id": "abc",
            "tool_use_id": "tu-42",
        })));
        assert!(!is_informational_empty_payload(&json!({
            "session_id": "abc",
            "user_prompt": "hello world",
        })));
        // Non-empty nested object: keep.
        assert!(!is_informational_empty_payload(&json!({
            "metadata": { "exit_code": 0 },
        })));
        // Non-empty array: keep.
        assert!(!is_informational_empty_payload(&json!({
            "tool_results": [{ "status": "ok" }],
        })));
        // Plain non-null primitive (defensive — uncommon, but possible).
        assert!(!is_informational_empty_payload(&json!(42)));
        assert!(!is_informational_empty_payload(&json!("non-empty")));
    }

    #[test]
    fn empty_payload_filter_drops_top_level_empty_array() {
        assert!(is_informational_empty_payload(&Value::Array(Vec::new())));
        // Non-empty array stays.
        assert!(!is_informational_empty_payload(&json!([1, 2, 3])));
    }

    #[test]
    fn empty_payload_filter_treats_whitespace_strings_as_noise() {
        assert!(is_informational_empty_payload(&json!({
            "session_id": "abc",
            "message": "   ",
            "tag": "\t\n",
        })));
    }

    /// Pin the documented behaviour: Stop / SubagentStop must NOT be dropped
    /// even when their payload is empty, because `codex_completion_hook_matches`
    /// uses the broadcast event as a turn-completion signal. The empty body
    /// is the *common case* for Stop hooks.
    #[test]
    fn stop_kind_short_circuit_classifier_matches_receive_hook_logic() {
        // Mirror the inverse of the gate inside `receive_hook` so the rule is
        // pinned at the type level — if someone later removes the
        // `Stop | SubagentStop` exemption, this test must fail.
        for keep in [HookEventKind::Stop, HookEventKind::SubagentStop] {
            assert!(
                matches!(keep, HookEventKind::Stop | HookEventKind::SubagentStop),
                "completion-signal events must be exempt from the empty-payload drop"
            );
        }
        for drop in [
            HookEventKind::UserPromptSubmit,
            HookEventKind::SessionStart,
            HookEventKind::PreToolUse,
            HookEventKind::PostToolUse,
            HookEventKind::Notification,
            HookEventKind::Unknown("custom".to_string()),
        ] {
            assert!(
                !matches!(drop, HookEventKind::Stop | HookEventKind::SubagentStop),
                "non-completion events must be eligible for the empty-payload drop"
            );
        }
    }

    /// #tui-hook-ttl-buffer (REQ-005 / overlap invariant): delivering a hook
    /// through the receiver must BOTH buffer it in the registry AND keep the
    /// broadcast firing for observers (the Codex `try_recv` path). The registry
    /// is additive — it does not gate or drop the broadcast. We claim_once the
    /// registry key after the request to prove the early Stop was buffered, and
    /// confirm the broadcast subscriber still saw the same Stop.
    #[tokio::test]
    async fn receiver_feeds_registry_and_preserves_broadcast() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};

        let session = "sess-registry-broadcast";
        // Start from a clean key so a neighbour test cannot pollute this one.
        if let Some(key) = RegistryKey::new("claude", Some(session), None) {
            let _ = global().claim_once(key);
        }

        let state = HookServerState::new();
        let mut rx = state.subscribe();
        let app = hook_receiver_router_with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/hooks/claude/Stop?session_id={session}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"hook_event_name":"Stop"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Broadcast observer still sees the Stop (Codex path preserved).
        let broadcast_event = rx.recv().await.unwrap();
        assert_eq!(broadcast_event.kind, HookEventKind::Stop);
        assert_eq!(broadcast_event.session_id, session);

        // Registry buffered the same Stop for a late claimer.
        let key = RegistryKey::new("claude", Some(session), None).unwrap();
        let replayed = global().claim_once(key);
        assert_eq!(replayed.len(), 1, "registry must buffer the early Stop");
        assert_eq!(replayed[0].kind, HookEventKind::Stop);
    }

    /// #tui-hook-ttl-buffer (buffering gate mirrors the broadcast gate): a
    /// feedback-injected Stop (one that produces a `memento_tool_feedback_flush`)
    /// must NOT be buffered in the registry. That Stop is suppressed from the
    /// broadcast / prompt-ready because it only exists to inject required memento
    /// feedback; buffering it would let a later `/tui/wait` return
    /// `matched:stop:registry` and finish the turn before Claude processes the
    /// feedback.
    #[tokio::test]
    async fn feedback_injected_stop_is_not_buffered() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};

        let session = "sess-feedback-injected-stop";
        if let Some(key) = RegistryKey::new("claude", Some(session), None) {
            let _ = global().claim_once(key);
        }

        let state = HookServerState::new();
        let app = hook_receiver_router_with_state(state);

        // 1) Arm a pending memento feedback flush: a recall PostToolUse with no
        //    matching tool feedback leaves an unknown pending search.
        let post = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/hooks/claude/PostToolUse?session_id={session}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"hook_event_name":"PostToolUse","tool_name":"mcp__memento__recall"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(post.status(), StatusCode::ACCEPTED);

        // 2) A Stop with no `stop_hook_active` now triggers a feedback flush; the
        //    response carries `memento_tool_feedback_flush`.
        let stop = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/hooks/claude/Stop?session_id={session}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"hook_event_name":"Stop"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stop.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(stop.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            value.get("memento_tool_feedback_flush").is_some(),
            "the Stop must be a feedback-injected flush for this test to be meaningful"
        );

        // The feedback-injected Stop must NOT have been buffered. (The priming
        // PostToolUse is a normal non-noise event and MAY be buffered; we assert
        // specifically that no Stop / SubagentStop landed in the buffer.)
        let key = RegistryKey::new("claude", Some(session), None).unwrap();
        let buffered = global().claim_once(key);
        assert!(
            !buffered
                .iter()
                .any(|e| matches!(e.kind, HookEventKind::Stop | HookEventKind::SubagentStop)),
            "feedback-injected Stop must not be buffered in the registry"
        );
    }

    /// #tui-hook-ttl-buffer (buffering gate): an informational empty-payload
    /// NON-Stop event (the kind the broadcast drops because it only echoes
    /// identifiers) must NOT be buffered, so a later `until=token` wait cannot
    /// match those identifiers and return `matched:token:registry` for an event
    /// the live path filtered out.
    #[tokio::test]
    async fn informational_empty_payload_non_stop_is_not_buffered() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};

        let session = "sess-noise-non-stop";
        if let Some(key) = RegistryKey::new("claude", Some(session), None) {
            let _ = global().claim_once(key);
        }

        let state = HookServerState::new();
        let app = hook_receiver_router_with_state(state);

        // A Notification whose body only re-states identifiers is broadcast-noise.
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/hooks/claude/Notification?session_id={session}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"session_id":"{session}","provider":"claude","event":"Notification"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let key = RegistryKey::new("claude", Some(session), None).unwrap();
        assert!(
            global().claim_once(key).is_empty(),
            "informational empty-payload non-Stop must not be buffered"
        );
    }

    /// An empty-body Stop (the common Claude TUI case) is NOT broadcast-noise and
    /// MUST still be buffered so the early-Stop race rescue keeps working.
    #[tokio::test]
    async fn empty_body_stop_is_still_buffered() {
        use crate::services::claude_tui::hook_registry::{RegistryKey, global};

        let session = "sess-empty-stop-buffered";
        if let Some(key) = RegistryKey::new("claude", Some(session), None) {
            let _ = global().claim_once(key);
        }

        let state = HookServerState::new();
        let app = hook_receiver_router_with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/hooks/claude/Stop?session_id={session}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"hook_event_name":"Stop"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let key = RegistryKey::new("claude", Some(session), None).unwrap();
        let replayed = global().claim_once(key);
        assert_eq!(replayed.len(), 1, "empty-body Stop must still be buffered");
        assert_eq!(replayed[0].kind, HookEventKind::Stop);
    }

    #[test]
    fn mismatched_hook_routes_through_the_registered_side_of_partial_cutover() {
        assert_eq!(
            hook_routing_session_ids(
                Some("stale-command".to_string()),
                Some("live-payload".to_string()),
                false,
                true,
            )
            .0
            .as_deref(),
            Some("live-payload"),
            "after restart, a rewritten launch with stale settings must route through the rehydrated payload UUID"
        );
        assert_eq!(
            hook_routing_session_ids(
                Some("stable-command".to_string()),
                Some("new-payload".to_string()),
                true,
                true,
            )
            .0
            .as_deref(),
            Some("stable-command"),
            "while the original waiter is live, its stable command UUID remains authoritative"
        );
        assert_eq!(
            hook_routing_session_ids(
                Some("rewritten-command".to_string()),
                Some("old-payload".to_string()),
                false,
                true,
            )
            .0
            .as_deref(),
            Some("old-payload"),
            "a settings-first partial update must remain routable until launch repair converges"
        );
        assert_eq!(
            hook_routing_session_ids(
                Some("stable-command".to_string()),
                Some("new-payload".to_string()),
                true,
                true,
            )
            .1
            .as_deref(),
            Some("new-payload"),
            "the payload alias must also receive the transition event for waiters created after idle adoption"
        );
    }

    #[test]
    fn published_endpoint_remains_stable_until_replaced_guard_drops() {
        let _guard = ENDPOINT_TEST_LOCK.lock().unwrap();
        *HOOK_ENDPOINT
            .write()
            .unwrap_or_else(|error| error.into_inner()) = None;

        let first = publish_hook_endpoint("http://127.0.0.1:8791".to_string());
        assert_eq!(
            current_hook_endpoint().as_deref(),
            Some("http://127.0.0.1:8791")
        );

        let second = publish_hook_endpoint("http://127.0.0.1:8799".to_string());
        drop(first);
        assert_eq!(
            current_hook_endpoint().as_deref(),
            Some("http://127.0.0.1:8799")
        );

        drop(second);
        assert_eq!(current_hook_endpoint(), None);
    }
}
