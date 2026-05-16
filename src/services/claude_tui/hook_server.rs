use std::net::SocketAddr;
use std::sync::{LazyLock, RwLock};

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

const EVENT_BUFFER_CAPACITY: usize = 256;

static HOOK_ENDPOINT: LazyLock<RwLock<Option<String>>> = LazyLock::new(|| RwLock::new(None));
static HOOK_SERVER_STATE: LazyLock<HookServerState> = LazyLock::new(HookServerState::new);

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
}

impl HookServerState {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        Self { event_tx }
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

pub struct HookServerHandle {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

pub struct HookEndpointGuard {
    endpoint: String,
}

impl HookServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for HookServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        clear_hook_endpoint_if_current(&self.base_url());
    }
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

pub async fn spawn_hook_server() -> Result<HookServerHandle, String> {
    spawn_hook_server_with_state(HOOK_SERVER_STATE.clone()).await
}

pub async fn spawn_hook_server_with_state(
    state: HookServerState,
) -> Result<HookServerHandle, String> {
    let app = hook_standalone_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("bind hook server: {error}"))?;
    let addr = listener
        .local_addr()
        .map_err(|error| format!("hook server local_addr: {error}"))?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
        if let Err(error) = result {
            tracing::warn!("tui hook server stopped with error: {error}");
        }
    });

    let handle = HookServerHandle {
        addr,
        shutdown_tx: Some(shutdown_tx),
        task,
    };
    *HOOK_ENDPOINT
        .write()
        .unwrap_or_else(|error| error.into_inner()) = Some(handle.base_url());
    tracing::info!(endpoint = handle.base_url(), "tui hook server started");
    Ok(handle)
}

pub fn hook_receiver_router() -> Router {
    hook_receiver_router_with_state(HOOK_SERVER_STATE.clone())
}

fn hook_standalone_router(state: HookServerState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/hooks/{provider}/{event}", post(receive_hook))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(state)
}

fn hook_receiver_router_with_state(state: HookServerState) -> Router {
    Router::new()
        .route("/hooks/{provider}/{event}", post(receive_hook))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

#[derive(Debug, Deserialize)]
struct HookQuery {
    session_id: Option<String>,
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

    let session_id = query
        .session_id
        .as_deref()
        .and_then(non_empty_string)
        .or_else(|| payload_session_id(&payload));
    let Some(session_id) = session_id else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "missing session_id" })),
        );
    };

    let kind = HookEventKind::from_path(&event);
    let event = HookEvent {
        provider: provider.clone(),
        session_id: session_id.clone(),
        kind,
        received_at: Utc::now(),
        payload,
    };
    let event_name = event.kind.as_str().to_string();
    if matches!(event.kind, HookEventKind::Unknown(_)) {
        tracing::warn!(
            provider,
            event = event_name,
            session_id,
            "unknown tui hook event accepted for provider-scoped telemetry"
        );
    }
    if state.event_tx.send(event).is_err() {
        tracing::debug!(
            provider,
            event = event_name,
            session_id,
            "tui hook event accepted with no subscribers; event discarded"
        );
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "provider": provider,
            "event": event_name,
            "session_id": session_id
        })),
    )
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
