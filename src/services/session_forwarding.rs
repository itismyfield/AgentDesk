use std::sync::OnceLock;
use std::time::Duration;

use axum::http::HeaderMap;
use axum::{Json, http::StatusCode};
use reqwest::RequestBuilder;
use reqwest::header::HeaderValue;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::app_state::AppState;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

const FORWARDED_BY_HEADER: &str = "x-agentdesk-forwarded-by";
const SESSION_OWNER_HEADER: &str = "x-agentdesk-session-owner";
const FORWARD_TIMEOUT_SECS: u64 = 10;

static SESSION_FORWARD_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForwardTarget {
    pub(crate) owner_instance_id: String,
    pub(crate) base_url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ForwardResolution {
    Local,
    Forward(ForwardTarget),
    Unavailable { status: StatusCode, body: Value },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CancelTurnSessionInfo {
    pub(crate) session_key: String,
    pub(crate) dispatch_id: Option<String>,
    pub(crate) provider_name: Option<String>,
    pub(crate) agent_id: Option<String>,
    pub(crate) requested_provider: Option<String>,
    pub(crate) owner_instance_id: Option<String>,
    pub(crate) match_rank: i64,
}

/// Narrow state needed by session-control callers outside HTTP route handlers.
#[derive(Clone)]
pub(crate) struct ForwardCallerContext {
    pub(crate) pg_pool: Option<PgPool>,
    pub(crate) config: std::sync::Arc<crate::config::Config>,
    pub(crate) cluster_instance_id: Option<String>,
}

impl From<&AppState> for ForwardCallerContext {
    fn from(state: &AppState) -> Self {
        Self {
            pg_pool: state.pg_pool.clone(),
            config: state.config.clone(),
            cluster_instance_id: state.cluster_instance_id.clone(),
        }
    }
}

impl ForwardCallerContext {
    pub(crate) fn from_live_globals(pg_pool: Option<PgPool>) -> Self {
        Self {
            pg_pool,
            config: crate::config_live_reload::current()
                .unwrap_or_else(|| std::sync::Arc::new(crate::config::Config::default())),
            cluster_instance_id: Some(
                crate::services::cluster::node_registry::resolve_self_instance_id_without_config(),
            ),
        }
    }

    pub(crate) fn pg_pool_ref(&self) -> Option<&PgPool> {
        self.pg_pool.as_ref()
    }
}

pub(crate) fn is_forwarded_request(headers: &HeaderMap) -> bool {
    headers.contains_key(FORWARDED_BY_HEADER)
}

fn client() -> &'static reqwest::Client {
    SESSION_FORWARD_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(FORWARD_TIMEOUT_SECS))
            .build()
            .expect("session forwarding HTTP client")
    })
}

pub(crate) fn resolve_forward_target_from_nodes(
    owner_instance_id: Option<&str>,
    local_instance_id: Option<&str>,
    worker_nodes: &[Value],
) -> ForwardResolution {
    let owner_instance_id = owner_instance_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let local_instance_id = local_instance_id
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let Some(owner) = owner_instance_id else {
        return ForwardResolution::Local;
    };
    let Some(local) = local_instance_id else {
        return ForwardResolution::Local;
    };
    if !valid_instance_id(owner) {
        return ForwardResolution::Unavailable {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: json!({
                "error": "session owner instance id is invalid",
                "code": "session_owner_instance_id_invalid",
                "owner_instance_id": owner,
            }),
        };
    }
    if !valid_instance_id(local) {
        return ForwardResolution::Unavailable {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: json!({
                "error": "local cluster instance id is invalid",
                "code": "session_local_instance_id_invalid",
                "local_instance_id": local,
            }),
        };
    }
    if owner == local {
        return ForwardResolution::Local;
    }

    let routing = crate::services::cluster::session_routing::session_owner_routing_status(
        Some(owner),
        Some(local),
        worker_nodes,
    );
    if routing["routable"].as_bool() == Some(true) {
        if let Some(base_url) = routing["api_base_url"].as_str()
            && valid_api_base_url(base_url)
        {
            return ForwardResolution::Forward(ForwardTarget {
                owner_instance_id: owner.to_string(),
                base_url: base_url.to_string(),
            });
        }
        return ForwardResolution::Unavailable {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: json!({
                "error": "session owner API base URL is invalid",
                "code": "worker_api_base_url_invalid",
                "owner": routing,
            }),
        };
    }

    ForwardResolution::Unavailable {
        status: StatusCode::SERVICE_UNAVAILABLE,
        body: json!({
            "error": "session owner is not routable",
            "code": "session_owner_unroutable",
            "owner": routing,
        }),
    }
}

fn valid_api_base_url(base_url: &str) -> bool {
    let lower = base_url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn valid_instance_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(
            |byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.'),
        )
}

pub(crate) async fn resolve_forward_target(
    state: &ForwardCallerContext,
    owner_instance_id: Option<&str>,
    pool: &PgPool,
) -> ForwardResolution {
    let owner_instance_id = owner_instance_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let local_instance_id = state
        .cluster_instance_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if owner_instance_id.is_none()
        || local_instance_id.is_none()
        || owner_instance_id == local_instance_id
    {
        return ForwardResolution::Local;
    }

    let worker_nodes = match crate::services::cluster::node_registry::list_worker_nodes(
        pool,
        state.config.cluster.lease_ttl_secs,
    )
    .await
    {
        Ok(nodes) => nodes,
        Err(error) => {
            return ForwardResolution::Unavailable {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: json!({
                    "error": format!("failed to load worker nodes for session forwarding: {error}"),
                    "code": "worker_nodes_unavailable",
                    "owner_instance_id": owner_instance_id,
                }),
            };
        }
    };

    resolve_forward_target_from_nodes(owner_instance_id, local_instance_id, &worker_nodes)
}

pub(crate) async fn forward_tmux_output(
    state: &ForwardCallerContext,
    target: &ForwardTarget,
    session_id: i64,
    lines: i32,
) -> (StatusCode, Json<Value>) {
    let url = format!(
        "{}/api/sessions/{}/tmux-output",
        target.base_url, session_id
    );
    let request = apply_node_headers(state, target, client().get(url).query(&[("lines", lines)]));
    forward_json_response(request, "tmux-output", target).await
}

pub(crate) async fn forward_force_kill(
    state: &ForwardCallerContext,
    target: &ForwardTarget,
    session_key: &str,
    retry: bool,
    reason: &str,
) -> (StatusCode, Json<Value>) {
    let url = format!(
        "{}/api/sessions/{}/force-kill",
        target.base_url,
        encode_path_segment(session_key)
    );
    let request = apply_node_headers(
        state,
        target,
        client()
            .post(url)
            .json(&json!({ "retry": retry, "reason": reason })),
    );
    forward_json_response(request, "force-kill", target).await
}

pub(crate) async fn forward_kill_tmux(
    state: &ForwardCallerContext,
    target: &ForwardTarget,
    session_key: &str,
    reason: &str,
    minimum_idle_minutes: Option<u64>,
) -> (StatusCode, Json<Value>) {
    let url = format!(
        "{}/api/sessions/{}/kill-tmux",
        target.base_url,
        encode_path_segment(session_key)
    );
    let request = apply_node_headers(
        state,
        target,
        client()
            .post(url)
            .json(&json!({ "reason": reason, "minimum_idle_minutes": minimum_idle_minutes })),
    );
    forward_json_response(request, "kill-tmux", target).await
}

pub(crate) async fn forward_cancel_turn(
    state: &ForwardCallerContext,
    target: &ForwardTarget,
    channel_id: &str,
    force: bool,
) -> (StatusCode, Json<Value>) {
    let url = format!(
        "{}/api/turns/{}/cancel",
        target.base_url,
        encode_path_segment(channel_id)
    );
    let request = apply_node_headers(
        state,
        target,
        client().post(url).json(&json!({ "force": force })),
    );
    forward_json_response(request, "cancel-turn", target).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CancelRetryDecision {
    Success,
    RetryOwner,
    Fail,
}

pub(crate) fn classify_cancel_forward_status(status: StatusCode) -> CancelRetryDecision {
    match status {
        StatusCode::OK | StatusCode::NOT_FOUND => CancelRetryDecision::Success,
        StatusCode::CONFLICT => CancelRetryDecision::RetryOwner,
        _ => CancelRetryDecision::Fail,
    }
}

pub(crate) async fn forward_cancel_with_owner_retry(
    state: &ForwardCallerContext,
    channel_id: &str,
    force: bool,
    first_target: ForwardTarget,
) -> ServiceResult<Option<Value>> {
    let (status, Json(body)) = forward_cancel_turn(state, &first_target, channel_id, force).await;
    match classify_cancel_forward_status(status) {
        CancelRetryDecision::Success => {
            return Ok(Some(cancel_success_body(status, body, channel_id)));
        }
        CancelRetryDecision::Fail => return Err(cancel_forward_error(status, body, channel_id)),
        CancelRetryDecision::RetryOwner => {}
    }

    let pool = state.pg_pool_ref().ok_or_else(|| {
        ServiceError::internal("postgres pool unavailable for cancel owner retry")
            .with_code(ErrorCode::Database)
    })?;
    let owner = load_cancel_owner(pool, channel_id).await?;
    let resolution = resolve_forward_target(state, owner.as_deref(), pool).await;
    let retry_target = match resolution {
        ForwardResolution::Forward(target) if target != first_target => target,
        ForwardResolution::Local => return Ok(None),
        ForwardResolution::Forward(_) => {
            return Err(ServiceError::conflict(
                "cancel owner rejected the request without changing",
            )
            .with_context("channel_id", channel_id));
        }
        ForwardResolution::Unavailable { status, body } => {
            return Err(cancel_forward_error(status, body, channel_id));
        }
    };

    let (status, Json(body)) = forward_cancel_turn(state, &retry_target, channel_id, force).await;
    match classify_cancel_forward_status(status) {
        CancelRetryDecision::Success => Ok(Some(cancel_success_body(status, body, channel_id))),
        CancelRetryDecision::RetryOwner | CancelRetryDecision::Fail => {
            Err(cancel_forward_error(status, body, channel_id))
        }
    }
}

pub(crate) async fn forward_remote_cancel_if_needed(
    state: &ForwardCallerContext,
    headers: &HeaderMap,
    channel_id: &str,
    force: bool,
) -> ServiceResult<Option<Value>> {
    let pool = state.pg_pool_ref().ok_or_else(|| {
        ServiceError::internal("postgres pool unavailable for cancel owner lookup")
            .with_code(ErrorCode::Database)
    })?;
    let owner = load_cancel_owner(pool, channel_id).await?;
    let resolution = resolve_forward_target(state, owner.as_deref(), pool).await;

    if is_forwarded_request(headers) {
        return match resolution {
            ForwardResolution::Local => Ok(None),
            _ => Err(ServiceError::conflict(
                "forwarded cancel reached a non-owner instance",
            )
            .with_context("channel_id", channel_id)
            .with_context("owner_instance_id", owner)),
        };
    }

    match resolution {
        ForwardResolution::Local => Ok(None),
        ForwardResolution::Forward(target) => {
            forward_cancel_with_owner_retry(state, channel_id, force, target).await
        }
        ForwardResolution::Unavailable { status, body } => Err(cancel_forward_error(
            status,
            body,
            channel_id,
        )),
    }
}

pub(crate) async fn load_cancel_turn_session(
    pool: &PgPool,
    channel_id: &str,
) -> ServiceResult<Option<CancelTurnSessionInfo>> {
    sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
        ),
    >(
        "WITH channel_agent AS (
           SELECT id AS agent_id,
                  CASE
                    WHEN discord_channel_cc = $1 OR discord_channel_id = $1 THEN 'claude'
                    WHEN discord_channel_cdx = $1 OR discord_channel_alt = $1 THEN 'codex'
                    ELSE NULL
                  END AS requested_provider
           FROM agents
           WHERE discord_channel_id = $1
              OR discord_channel_alt = $1
              OR discord_channel_cc = $1
              OR discord_channel_cdx = $1
           LIMIT 1
         )
         SELECT s.session_key,
                s.active_dispatch_id,
                s.provider,
                s.agent_id,
                ca.requested_provider,
                s.instance_id,
                CASE
                  WHEN COALESCE(s.thread_channel_id, '') = $1 THEN 0
                  WHEN s.session_key LIKE '%' || $1 || '%' THEN 1
                  WHEN ca.requested_provider IS NOT NULL
                       AND COALESCE(s.provider, '') = ca.requested_provider THEN 2
                  ELSE 3
                END::BIGINT AS match_rank
         FROM sessions s
         LEFT JOIN channel_agent ca ON s.agent_id = ca.agent_id
         WHERE s.status = 'turn_active'
           AND (
             COALESCE(s.thread_channel_id, '') = $1
             OR s.session_key LIKE '%' || $1 || '%'
             OR (
               ca.agent_id IS NOT NULL
               AND (
                 ca.requested_provider IS NULL
                 OR COALESCE(s.provider, '') = ca.requested_provider
               )
             )
           )
         ORDER BY match_rank ASC, s.last_heartbeat DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .map(|row| {
        row.map(
            |(
                session_key,
                dispatch_id,
                provider_name,
                agent_id,
                requested_provider,
                owner_instance_id,
                match_rank,
            )| CancelTurnSessionInfo {
                session_key,
                dispatch_id,
                provider_name,
                agent_id,
                requested_provider,
                owner_instance_id,
                match_rank,
            },
        )
    })
    .map_err(|error| {
        ServiceError::internal(format!("load postgres active turn: {error}"))
            .with_code(ErrorCode::Database)
            .with_operation("cancel_turn.query_active_session_pg")
            .with_context("channel_id", channel_id)
    })
}

pub(crate) async fn load_cancel_owner(
    pool: &PgPool,
    channel_id: &str,
) -> ServiceResult<Option<String>> {
    Ok(load_cancel_turn_session(pool, channel_id)
        .await?
        .and_then(|session| session.owner_instance_id))
}

fn cancel_success_body(status: StatusCode, body: Value, channel_id: &str) -> Value {
    if status == StatusCode::NOT_FOUND {
        json!({
            "ok": true,
            "channel_id": channel_id,
            "already_absent": true,
            "remote_response": body,
        })
    } else {
        body
    }
}

fn cancel_forward_error(status: StatusCode, body: Value, channel_id: &str) -> ServiceError {
    ServiceError::new(
        status,
        if status == StatusCode::CONFLICT {
            ErrorCode::Conflict
        } else {
            ErrorCode::Queue
        },
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("remote cancel forwarding failed"),
    )
    .with_context("channel_id", channel_id)
    .with_context("remote_response", body)
}

fn apply_node_headers(
    state: &ForwardCallerContext,
    target: &ForwardTarget,
    mut request: RequestBuilder,
) -> RequestBuilder {
    if let Some(token) = state
        .config
        .server
        .auth_token
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        request = request.bearer_auth(token);
    }
    if let Some(local_instance_id) = state.cluster_instance_id.as_deref() {
        match HeaderValue::from_str(local_instance_id) {
            Ok(value) => {
                request = request.header(FORWARDED_BY_HEADER, value);
            }
            Err(error) => {
                tracing::error!(
                    "[session-forwarding] cluster_instance_id is not a valid header value: {error}"
                );
                request = request.header(
                    FORWARDED_BY_HEADER,
                    HeaderValue::from_static("invalid-local-instance-id"),
                );
            }
        }
    }
    if let Ok(value) = HeaderValue::from_str(&target.owner_instance_id) {
        request = request.header(SESSION_OWNER_HEADER, value);
    }
    request
}

async fn forward_json_response(
    request: RequestBuilder,
    operation: &str,
    target: &ForwardTarget,
) -> (StatusCode, Json<Value>) {
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": format!("session forwarding {operation} request failed: {error}"),
                    "code": "session_forward_failed",
                    "owner_instance_id": target.owner_instance_id,
                    "api_base_url": target.base_url,
                })),
            );
        }
    };

    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = response.json::<Value>().await.unwrap_or_else(|error| {
        json!({
            "error": format!("session forwarding {operation} returned non-JSON response: {error}"),
            "code": "session_forward_invalid_response",
            "owner_instance_id": target.owner_instance_id,
            "api_base_url": target.base_url,
        })
    });
    (status, Json(body))
}

fn encode_path_segment(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{
        CancelRetryDecision, ForwardResolution, ForwardTarget, classify_cancel_forward_status,
        client, encode_path_segment, forward_json_response, is_forwarded_request,
        load_cancel_owner, load_cancel_turn_session, resolve_forward_target_from_nodes,
    };
    use axum::Json;
    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn resolve_forward_target_keeps_missing_and_local_sessions_local() {
        assert_eq!(
            resolve_forward_target_from_nodes(None, Some("leader"), &[]),
            ForwardResolution::Local
        );
        assert_eq!(
            resolve_forward_target_from_nodes(Some("leader"), Some("leader"), &[]),
            ForwardResolution::Local
        );
        assert_eq!(
            resolve_forward_target_from_nodes(Some("worker"), None, &[]),
            ForwardResolution::Local
        );
    }

    #[test]
    fn resolve_forward_target_returns_worker_api_for_routable_foreign_owner() {
        let nodes = vec![json!({
            "instance_id": "worker-a",
            "status": "online",
            "api_base_url": "http://worker-a.local:8791"
        })];

        let resolution =
            resolve_forward_target_from_nodes(Some("worker-a"), Some("leader"), &nodes);
        let ForwardResolution::Forward(target) = resolution else {
            panic!("expected forward target");
        };
        assert_eq!(target.owner_instance_id, "worker-a");
        assert_eq!(target.base_url, "http://worker-a.local:8791");
    }

    #[test]
    fn resolve_forward_target_reports_stale_owner_explicitly() {
        let nodes = vec![json!({
            "instance_id": "worker-a",
            "status": "offline",
            "api_base_url": "http://worker-a.local:8791"
        })];

        let resolution =
            resolve_forward_target_from_nodes(Some("worker-a"), Some("leader"), &nodes);
        let ForwardResolution::Unavailable { status, body } = resolution else {
            panic!("expected unavailable owner");
        };
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["owner"]["reason"].as_str(), Some("worker_node_stale"));
    }

    #[test]
    fn resolve_forward_target_rejects_invalid_worker_api_scheme() {
        let nodes = vec![json!({
            "instance_id": "worker-a",
            "status": "online",
            "api_base_url": "file:///tmp/agentdesk.sock"
        })];

        let resolution =
            resolve_forward_target_from_nodes(Some("worker-a"), Some("leader"), &nodes);
        let ForwardResolution::Unavailable { body, .. } = resolution else {
            panic!("expected unavailable owner");
        };
        assert_eq!(body["code"].as_str(), Some("worker_api_base_url_invalid"));
    }

    #[test]
    fn resolve_forward_target_rejects_invalid_owner_instance_id() {
        let nodes = vec![json!({
            "instance_id": "worker-a\r\nx-injected: true",
            "status": "online",
            "api_base_url": "http://worker-a.local:8791"
        })];

        let resolution = resolve_forward_target_from_nodes(
            Some("worker-a\r\nx-injected: true"),
            Some("leader"),
            &nodes,
        );
        let ForwardResolution::Unavailable { body, .. } = resolution else {
            panic!("expected unavailable owner");
        };
        assert_eq!(
            body["code"].as_str(),
            Some("session_owner_instance_id_invalid")
        );
    }

    #[test]
    fn resolve_forward_target_rejects_invalid_local_instance_id() {
        let nodes = vec![json!({
            "instance_id": "worker-a",
            "status": "online",
            "api_base_url": "http://worker-a.local:8791"
        })];

        let resolution = resolve_forward_target_from_nodes(
            Some("worker-a"),
            Some("leader\r\nx-injected: true"),
            &nodes,
        );
        let ForwardResolution::Unavailable { body, .. } = resolution else {
            panic!("expected unavailable owner");
        };
        assert_eq!(
            body["code"].as_str(),
            Some("session_local_instance_id_invalid")
        );
    }

    #[test]
    fn forwarded_header_is_detected() {
        let mut headers = HeaderMap::new();
        assert!(!is_forwarded_request(&headers));
        headers.insert(
            "x-agentdesk-forwarded-by",
            HeaderValue::from_static("leader"),
        );
        assert!(is_forwarded_request(&headers));
    }

    #[tokio::test]
    async fn forward_json_response_preserves_worker_auth_failure_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("test listener addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept test request");
            let mut buffer = [0_u8; 512];
            let _ = socket.read(&mut buffer).await.expect("read request");
            let body = r#"{"error":"unauthorized"}"#;
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let target = ForwardTarget {
            owner_instance_id: "worker-a".to_string(),
            base_url: format!("http://{addr}"),
        };
        let (status, Json(body)) = forward_json_response(
            client().get(format!("http://{addr}/probe")),
            "probe",
            &target,
        )
        .await;

        server.await.expect("test server task");
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"].as_str(), Some("unauthorized"));
    }

    #[test]
    fn encode_path_segment_escapes_session_key_separators() {
        assert_eq!(
            encode_path_segment("host:AgentDesk-codex/a b"),
            "host%3AAgentDesk-codex%2Fa%20b"
        );
    }

    #[test]
    fn cancel_retry_status_accepts_ack_and_idempotent_not_found() {
        assert_eq!(
            classify_cancel_forward_status(axum::http::StatusCode::OK),
            CancelRetryDecision::Success
        );
        assert_eq!(
            classify_cancel_forward_status(axum::http::StatusCode::NOT_FOUND),
            CancelRetryDecision::Success
        );
    }

    #[test]
    fn cancel_retry_status_reloads_owner_only_for_conflict() {
        assert_eq!(
            classify_cancel_forward_status(axum::http::StatusCode::CONFLICT),
            CancelRetryDecision::RetryOwner
        );
        for status in [
            axum::http::StatusCode::BAD_GATEWAY,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::http::StatusCode::UNAUTHORIZED,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert_eq!(
                classify_cancel_forward_status(status),
                CancelRetryDecision::Fail
            );
        }
    }

    // PostgreSQL cases below pin sessions.instance_id as cancel authority.
    async fn insert_cancel_owner_fixture(
        pool: &sqlx::PgPool,
        session_key: &str,
        channel_id: Option<&str>,
        thread_channel_id: Option<&str>,
        instance_id: Option<&str>,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions
             (session_key, status, channel_id, thread_channel_id, instance_id, last_heartbeat)
             VALUES ($1, $2, $3, $4, $5, NOW())",
        )
        .bind(session_key)
        .bind(status)
        .bind(channel_id)
        .bind(thread_channel_id)
        .bind(instance_id)
        .execute(pool)
        .await
        .expect("insert cancel owner fixture");
    }

    #[tokio::test]
    async fn cancel_owner_reads_sessions_instance_id_for_thread_channel() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        insert_cancel_owner_fixture(
            &pool,
            "cancel-owner-thread",
            None,
            Some("101"),
            Some("worker-a"),
            "turn_active",
        )
        .await;

        assert_eq!(
            load_cancel_owner(&pool, "101").await.expect("load owner"),
            Some("worker-a".to_string())
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_owner_reads_sessions_instance_id_for_runtime_channel() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        insert_cancel_owner_fixture(
            &pool,
            "cancel-owner-runtime",
            Some("102"),
            None,
            Some("worker-b"),
            "turn_active",
        )
        .await;

        assert_eq!(
            load_cancel_owner(&pool, "102").await.expect("load owner"),
            Some("worker-b".to_string())
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_owner_ignores_disconnected_sessions() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        insert_cancel_owner_fixture(
            &pool,
            "cancel-owner-disconnected",
            Some("103"),
            None,
            Some("worker-c"),
            "disconnected",
        )
        .await;

        assert_eq!(
            load_cancel_owner(&pool, "103").await.expect("load owner"),
            None
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_owner_preserves_null_instance_as_local_authority() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        insert_cancel_owner_fixture(
            &pool,
            "cancel-owner-null",
            Some("104"),
            None,
            None,
            "turn_active",
        )
        .await;

        assert_eq!(
            load_cancel_owner(&pool, "104").await.expect("load owner"),
            None
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_owner_prefers_latest_active_heartbeat() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        insert_cancel_owner_fixture(
            &pool,
            "cancel-owner-old",
            Some("105"),
            None,
            Some("worker-old"),
            "turn_active",
        )
        .await;
        sqlx::query("UPDATE sessions SET last_heartbeat = NOW() - INTERVAL '1 minute' WHERE session_key = $1")
            .bind("cancel-owner-old")
            .execute(&pool)
            .await
            .expect("age owner fixture");
        insert_cancel_owner_fixture(
            &pool,
            "cancel-owner-new",
            Some("105"),
            None,
            Some("worker-new"),
            "turn_active",
        )
        .await;

        assert_eq!(
            load_cancel_owner(&pool, "105").await.expect("load owner"),
            Some("worker-new".to_string())
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_owner_matches_exact_channel_not_session_key_text() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        insert_cancel_owner_fixture(
            &pool,
            "contains-106",
            Some("999"),
            None,
            Some("worker-wrong"),
            "turn_active",
        )
        .await;

        assert_eq!(
            load_cancel_owner(&pool, "106").await.expect("load owner"),
            None
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_owner_does_not_fall_back_to_agent_binding() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        insert_cancel_owner_fixture(
            &pool,
            "cancel-owner-unrelated",
            Some("107"),
            None,
            Some("worker-unrelated"),
            "turn_active",
        )
        .await;

        assert_eq!(
            load_cancel_owner(&pool, "108").await.expect("load owner"),
            None
        );
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_selection_session_key_fallback_preserves_remote_owner() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        insert_cancel_owner_fixture(
            &pool,
            "host:AgentDesk-claude-110",
            None,
            None,
            Some("worker-key"),
            "turn_active",
        )
        .await;

        let selected = load_cancel_turn_session(&pool, "110")
            .await
            .expect("load selected session")
            .expect("session-key fallback selection");
        assert_eq!(selected.session_key, "host:AgentDesk-claude-110");
        assert_eq!(selected.owner_instance_id.as_deref(), Some("worker-key"));
        assert_eq!(selected.match_rank, 1);
        let status: String = sqlx::query_scalar(
            "SELECT status FROM sessions WHERE session_key = 'host:AgentDesk-claude-110'",
        )
        .fetch_one(&pool)
        .await
        .expect("load unchanged status");
        assert_eq!(status, "turn_active");
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_selection_agent_provider_fallback_preserves_remote_owner() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_cc)
             VALUES ('cancel-agent-111', 'Cancel Agent', 'claude', '111')",
        )
        .execute(&pool)
        .await
        .expect("insert cancel agent fixture");
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, instance_id, last_heartbeat)
             VALUES ('owner-provider-fallback', 'cancel-agent-111', 'claude', 'turn_active', 'worker-provider', NOW())",
        )
        .execute(&pool)
        .await
        .expect("insert provider fallback session");

        let selected = load_cancel_turn_session(&pool, "111")
            .await
            .expect("load selected session")
            .expect("agent/provider fallback selection");
        assert_eq!(selected.session_key, "owner-provider-fallback");
        assert_eq!(
            selected.owner_instance_id.as_deref(),
            Some("worker-provider")
        );
        assert_eq!(selected.requested_provider.as_deref(), Some("claude"));
        assert_eq!(selected.match_rank, 2);
        let status: String = sqlx::query_scalar(
            "SELECT status FROM sessions WHERE session_key = 'owner-provider-fallback'",
        )
        .fetch_one(&pool)
        .await
        .expect("load unchanged status");
        assert_eq!(status, "turn_active");
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancel_owner_returns_none_when_no_session_exists() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        assert_eq!(
            load_cancel_owner(&pool, "109").await.expect("load owner"),
            None
        );
        pg_db.drop().await;
    }
}
