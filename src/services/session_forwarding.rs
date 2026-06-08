use std::sync::OnceLock;
use std::time::Duration;

use axum::http::HeaderMap;
use axum::{Json, http::StatusCode};
use reqwest::RequestBuilder;
use reqwest::header::HeaderValue;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::app_state::AppState;

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
    state: &AppState,
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
    state: &AppState,
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
    state: &AppState,
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
    state: &AppState,
    target: &ForwardTarget,
    session_key: &str,
    reason: &str,
) -> (StatusCode, Json<Value>) {
    let url = format!(
        "{}/api/sessions/{}/kill-tmux",
        target.base_url,
        encode_path_segment(session_key)
    );
    let request = apply_node_headers(
        state,
        target,
        client().post(url).json(&json!({ "reason": reason })),
    );
    forward_json_response(request, "kill-tmux", target).await
}

fn apply_node_headers(
    state: &AppState,
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
        ForwardResolution, ForwardTarget, client, encode_path_segment, forward_json_response,
        is_forwarded_request, resolve_forward_target_from_nodes,
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
}
