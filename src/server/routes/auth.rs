use axum::{Json, extract::State};
use serde_json::json;
use std::net::SocketAddr;

use super::AppState;

/// GET /api/auth/session
/// Returns session status. If auth_token is configured, validates the request.
/// The actual auth check is done by the middleware — if this handler runs, the request is authenticated.
pub async fn get_session(State(state): State<AppState>) -> Json<serde_json::Value> {
    let auth_enabled = state.config.server.auth_token.is_some();
    Json(json!({
        "ok": true,
        "auth_enabled": auth_enabled,
        "csrf_token": "",
    }))
}

/// Internal / in-process webhook paths that are invoked by this same dcserver
/// over the loopback interface (policy engine, internal_api client).
///
/// Issue #2047 — Finding 1 & 2:
/// These paths must NOT be exposed to the LAN. Previously the middleware did
/// a blanket `path.starts_with("/internal/") || path.starts_with("/hook/")`
/// bypass which made every protected handler under those prefixes callable
/// without any Bearer / loopback proof. We now require either:
///   - the peer address is loopback (127.0.0.1 / ::1), OR
///   - a valid `Authorization: Bearer <auth_token>` is presented.
///
/// Same-origin (Origin/Referer) bypass is intentionally NOT honoured for these
/// paths because headers are trivially forgeable by a LAN attacker.
fn is_internal_loopback_path(path: &str) -> bool {
    path.starts_with("/hook/")
        || path == "/dispatched-sessions/webhook"
        || path.starts_with("/internal/")
}

fn extract_bearer<'a>(headers: &'a axum::http::HeaderMap) -> Option<&'a str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

fn peer_addr_from_request(req: &axum::extract::Request) -> Option<SocketAddr> {
    req.extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|info| info.0)
}

fn is_loopback_peer(peer: Option<SocketAddr>) -> bool {
    peer.is_some_and(|addr| addr.ip().is_loopback())
}

fn is_websocket_upgrade(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

fn unauthorized_response() -> axum::response::Response {
    axum::response::Response::builder()
        .status(401)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            r#"{"error":"unauthorized","message":"Bearer token required"}"#,
        ))
        .unwrap_or_default()
}

/// Auth middleware: checks Bearer token against config.server.auth_token.
/// If auth_token is not set, all requests pass through (local-only mode).
pub async fn auth_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(expected_token) = state.config.server.auth_token.as_deref() else {
        // No auth configured — pass through
        return next.run(req).await;
    };

    if expected_token.is_empty() {
        return next.run(req).await;
    }

    // Note: path is relative to the /api nest, so "/health" not "/api/health"
    let path = req.uri().path();

    // Truly-public endpoints. These return no privileged data and are safe to
    // expose without authentication on any interface.
    if path == "/health" || path == "/auth/session" {
        return next.run(req).await;
    }

    let headers = req.headers().clone();
    let peer = peer_addr_from_request(&req);

    // Internal in-process paths (escalation emit, dispatch↔thread link,
    // hook/* state mutations, dispatched-sessions webhook). Require strict
    // loopback peer OR matching Bearer token — never accept Origin/Referer
    // alone since those are forgeable from the LAN.
    if is_internal_loopback_path(path) {
        if is_loopback_peer(peer) {
            return next.run(req).await;
        }
        if let Some(token) = extract_bearer(&headers) {
            if token == expected_token {
                return next.run(req).await;
            }
        }
        return unauthorized_response();
    }

    // Same-origin bypass (dashboard SPA served from this server). #2047
    // Finding 3 — require the peer address itself to be loopback before
    // trusting the (forgeable) Origin/Referer header.
    if is_loopback_peer(peer) {
        let is_same_origin = headers
            .get(axum::http::header::ORIGIN)
            .or_else(|| headers.get(axum::http::header::REFERER))
            .and_then(|v| v.to_str().ok())
            .map(|v| crate::utils::loopback_url::is_loopback_url(v, Some(state.config.server.port)))
            .unwrap_or(false);
        if is_same_origin {
            return next.run(req).await;
        }
    }

    // Check Authorization header
    if let Some(token) = extract_bearer(&headers) {
        if token == expected_token {
            return next.run(req).await;
        }
    }

    // Query-param token fallback (Finding 4): restricted to WebSocket upgrade
    // handshakes since the browser WebSocket API cannot attach an
    // Authorization header. Plain GET / POST requests must use the
    // Authorization header instead so the secret never leaks into access logs
    // or the Referer of downstream navigations.
    if is_websocket_upgrade(&headers) {
        if let Some(query) = req.uri().query() {
            for pair in query.split('&') {
                if let Some(token) = pair.strip_prefix("token=") {
                    if token == expected_token {
                        return next.run(req).await;
                    }
                }
            }
        }
    }

    unauthorized_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{HeaderValue, Method, Request, header};

    fn make_req(path: &str, peer: Option<&str>) -> axum::extract::Request {
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .body(Body::empty())
            .expect("build request");
        let mut request = request;
        if let Some(peer) = peer {
            request.extensions_mut().insert(ConnectInfo(
                peer.parse::<SocketAddr>().expect("valid socket addr"),
            ));
        }
        request
    }

    #[test]
    fn is_internal_loopback_path_matches_known_prefixes() {
        assert!(is_internal_loopback_path("/internal/escalation/emit"));
        assert!(is_internal_loopback_path("/internal/link-dispatch-thread"));
        assert!(is_internal_loopback_path("/internal/card-thread"));
        assert!(is_internal_loopback_path(
            "/internal/pending-dispatch-for-thread"
        ));
        assert!(is_internal_loopback_path("/hook/reset-status"));
        assert!(is_internal_loopback_path("/hook/skill-usage"));
        assert!(is_internal_loopback_path("/hook/session/abc"));
        assert!(is_internal_loopback_path("/dispatched-sessions/webhook"));
    }

    #[test]
    fn is_internal_loopback_path_rejects_unrelated_paths() {
        assert!(!is_internal_loopback_path("/health"));
        assert!(!is_internal_loopback_path("/auth/session"));
        assert!(!is_internal_loopback_path("/discord/send"));
        assert!(!is_internal_loopback_path("/dispatches"));
    }

    #[test]
    fn loopback_peer_detection() {
        assert!(is_loopback_peer(Some("127.0.0.1:54321".parse().unwrap())));
        assert!(is_loopback_peer(Some("[::1]:54321".parse().unwrap())));
        assert!(!is_loopback_peer(Some("10.0.0.5:54321".parse().unwrap())));
        assert!(!is_loopback_peer(Some(
            "192.168.1.10:54321".parse().unwrap()
        )));
        assert!(!is_loopback_peer(None));
    }

    #[test]
    fn extract_bearer_strips_prefix_or_returns_none() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(extract_bearer(&headers).is_none());

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret-token"),
        );
        assert_eq!(extract_bearer(&headers), Some("secret-token"));

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic user:pass"),
        );
        assert_eq!(extract_bearer(&headers), None);
    }

    #[test]
    fn is_websocket_upgrade_detects_handshake_header() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(!is_websocket_upgrade(&headers));

        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(is_websocket_upgrade(&headers));

        headers.insert(header::UPGRADE, HeaderValue::from_static("WebSocket"));
        assert!(is_websocket_upgrade(&headers));

        headers.insert(header::UPGRADE, HeaderValue::from_static("h2c"));
        assert!(!is_websocket_upgrade(&headers));
    }

    #[test]
    fn peer_addr_from_request_reads_connect_info() {
        let request = make_req("/internal/escalation/emit", Some("127.0.0.1:54321"));
        assert_eq!(
            peer_addr_from_request(&request),
            Some("127.0.0.1:54321".parse().unwrap())
        );

        let request_no_peer = make_req("/internal/escalation/emit", None);
        assert_eq!(peer_addr_from_request(&request_no_peer), None);
    }
}
