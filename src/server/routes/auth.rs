use axum::{Json, extract::State, response::IntoResponse};
use serde_json::json;
use std::net::SocketAddr;

use super::AppState;
use crate::api_caller_observability::{AuthStrength, RequestPrincipal};

mod tailnet_peer;

/// GET /api/auth/session
/// Public authentication probe; never treats reaching this handler as proof.
pub async fn get_session(
    State(state): State<AppState>,
    req: axum::extract::Request,
) -> impl IntoResponse {
    let auth_enabled = state
        .config
        .server
        .auth_token
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    let authenticated =
        dashboard_auth_strength(&state.config, req.headers(), peer_addr_from_request(&req))
            .is_some();
    (
        [
            ("cache-control", "no-store"),
            ("vary", "Authorization, Origin, Referer"),
        ],
        Json(json!({
            "ok": true,
            "auth_enabled": auth_enabled,
            "authenticated": authenticated,
            "csrf_token": "",
        })),
    )
}

pub(crate) async fn issue_ws_ticket(
    axum::Extension(access): axum::Extension<crate::server::dashboard_auth::DashboardAccess>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    match access.issue(&headers) {
        Ok((ticket, expires_in)) => (
            [("cache-control", "no-store")],
            Json(json!({"ticket": ticket, "expires_in": expires_in})),
        )
            .into_response(),
        Err(status) => (
            status,
            Json(json!({"error": "websocket ticket unavailable for this origin"})),
        )
            .into_response(),
    }
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
        || path.starts_with("/hooks/")
        || path.starts_with("/tui/")
        || path == "/dispatched-sessions/webhook"
        || path.starts_with("/internal/")
}

pub(in crate::server) fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
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

fn dashboard_auth_strength(
    config: &crate::config::Config,
    headers: &axum::http::HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<AuthStrength> {
    let Some(expected) = config
        .server
        .auth_token
        .as_deref()
        .filter(|s| !s.is_empty())
    else {
        return Some(AuthStrength::None);
    };
    if extract_bearer(headers)
        .is_some_and(|token| crate::utils::auth::constant_time_token_eq(expected, token))
    {
        return Some(AuthStrength::ServerAdmin);
    }
    if is_loopback_peer(peer)
        && headers
            .get(axum::http::header::ORIGIN)
            .or_else(|| headers.get(axum::http::header::REFERER))
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                crate::utils::loopback_url::is_loopback_url(v, Some(config.server.port))
            })
    {
        return Some(AuthStrength::Loopback);
    }
    None
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

async fn run_with_request_principal(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
    auth_strength: AuthStrength,
) -> axum::response::Response {
    let principal = RequestPrincipal::from_headers(req.headers(), auth_strength);
    req.extensions_mut().insert(principal);
    next.run(req).await
}

/// Internal routes require socket or configured Bearer proof; only node-to-node
/// routes accept tokenless tailnet peers. Other routes keep the configured auth mode.
pub async fn auth_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Note: path is relative to the /api nest, so "/health" not "/api/health"
    let path = req.uri().path();
    let headers = req.headers().clone();
    let peer = peer_addr_from_request(&req);

    // Keep local control paths behind loopback/Bearer proof. Tokenless tailnet
    // trust below is restricted to the two node-to-node routes.
    if is_internal_loopback_path(path) {
        if is_loopback_peer(peer) {
            return run_with_request_principal(req, next, AuthStrength::Loopback).await;
        }
        if let Some(expected_token) = state.config.server.auth_token.as_deref() {
            if !expected_token.is_empty() {
                if let Some(token) = extract_bearer(&headers) {
                    if crate::utils::auth::constant_time_token_eq(expected_token, token) {
                        return run_with_request_principal(req, next, AuthStrength::ServerAdmin)
                            .await;
                    }
                }
            }
        }
        if state
            .config
            .server
            .auth_token
            .as_deref()
            .is_none_or(str::is_empty)
            && tailnet_peer::allows(&req, peer)
        {
            tracing::debug!(method = %req.method(), route = path, auth_strength = "TailnetPeer",
                "node request authenticated");
            return run_with_request_principal(req, next, AuthStrength::TailnetPeer).await;
        }
        return unauthorized_response();
    }

    let Some(expected_token) = state.config.server.auth_token.as_deref() else {
        // No auth configured — pass through
        return run_with_request_principal(req, next, AuthStrength::None).await;
    };

    if expected_token.is_empty() {
        return run_with_request_principal(req, next, AuthStrength::None).await;
    }

    // Truly-public endpoints. These return no privileged data and are safe to
    // expose without authentication on any interface.
    if path == "/health" || path == "/auth/session" {
        return run_with_request_principal(req, next, AuthStrength::None).await;
    }

    if let Some(strength) = dashboard_auth_strength(&state.config, &headers, peer) {
        return run_with_request_principal(req, next, strength).await;
    }

    unauthorized_response()
}

/// Who may speak for a human on the human-input route: a non-browser process on this host,
/// or a holder of the server token. A body author is honored only behind one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HumanInputCaller {
    LocalProcess,
    ServerToken,
}

fn human_input_caller(
    config: &crate::config::Config,
    headers: &axum::http::HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<HumanInputCaller> {
    let token = config
        .server
        .auth_token
        .as_deref()
        .filter(|token| !token.is_empty());
    if let (Some(expected), Some(supplied)) = (token, extract_bearer(headers))
        && crate::utils::auth::constant_time_token_eq(expected, supplied)
    {
        return Some(HumanInputCaller::ServerToken);
    }
    if !peer?.ip().to_canonical().is_loopback() {
        return None;
    }
    // A same-host proxy (e.g. Tailscale serve) arrives as loopback; its forwarded headers say so.
    if headers.contains_key(axum::http::header::FORWARDED)
        || headers.contains_key("x-forwarded-for")
    {
        return None;
    }
    let header = |name: &str| headers.get(name).map(|value| value.to_str().unwrap_or(""));
    if header("sec-fetch-site").is_some_and(|site| {
        !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none")
    }) {
        return None;
    }
    // Browsers send Origin on every POST, so a foreign page or a rebinding name shows up here.
    if header(axum::http::header::ORIGIN.as_str()).is_some_and(|origin| {
        !crate::utils::loopback_url::is_loopback_url(origin, Some(config.server.port))
    }) {
        return None;
    }
    Some(HumanInputCaller::LocalProcess)
}

impl axum::extract::FromRequestParts<AppState> for HumanInputCaller {
    type Rejection = axum::response::Response;

    /// Runs before the body extractor, so a refused caller's body is never read.
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|info| info.0);
        human_input_caller(&state.config, &parts.headers, peer)
            .ok_or_else(|| refuse_human_input_caller(&parts.headers, peer))
    }
}

fn refuse_human_input_caller(
    headers: &axum::http::HeaderMap,
    peer: Option<SocketAddr>,
) -> axum::response::Response {
    tracing::warn!(
        peer_ip = peer.map(|peer| peer.ip().to_string()).as_deref(),
        has_origin = headers.contains_key(axum::http::header::ORIGIN),
        has_fetch_site = headers.contains_key("sec-fetch-site"),
        has_forwarded = headers.contains_key(axum::http::header::FORWARDED)
            || headers.contains_key("x-forwarded-for"),
        "human input: caller is neither a local process nor a server token holder"
    );
    (
        axum::http::StatusCode::FORBIDDEN,
        Json(json!({"ok": false, "error": "caller_not_allowed"})),
    )
        .into_response()
}

/// Loopback socket peer for tests that drive the HTTP routes in-process.
#[cfg(test)]
pub(crate) fn loopback_peer() -> axum::extract::ConnectInfo<SocketAddr> {
    axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0)))
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
        assert!(is_internal_loopback_path("/hooks/claude/Stop"));
        assert!(is_internal_loopback_path("/tui/send"));
        assert!(is_internal_loopback_path("/tui/wait"));
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
    fn peer_addr_from_request_reads_connect_info() {
        let request = make_req("/internal/escalation/emit", Some("127.0.0.1:54321"));
        assert_eq!(
            peer_addr_from_request(&request),
            Some("127.0.0.1:54321".parse().unwrap())
        );

        let request_no_peer = make_req("/internal/escalation/emit", None);
        assert_eq!(peer_addr_from_request(&request_no_peer), None);
    }

    #[test]
    fn human_input_caller_is_a_native_local_process_or_a_server_token_holder() {
        use HumanInputCaller::{LocalProcess, ServerToken};
        let config = |token: Option<&str>| {
            let mut config = crate::config::Config::default();
            config.server.port = 8791;
            config.server.auth_token = token.map(str::to_string);
            config
        };
        let headers = |pairs: &[(&str, &str)]| {
            let mut headers = axum::http::HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                );
            }
            headers
        };
        type Case<'a> = (
            Option<&'a str>,
            &'a [(&'a str, &'a str)],
            Option<&'a str>,
            Option<HumanInputCaller>,
        );
        #[rustfmt::skip]
        let cases: &[Case] = &[
            (Some("127.0.0.1:1"), &[], None, Some(LocalProcess)),
            (Some("127.8.0.1:1"), &[], None, Some(LocalProcess)),
            (Some("[::1]:1"), &[], None, Some(LocalProcess)),
            (Some("[::ffff:127.0.0.1]:1"), &[], None, Some(LocalProcess)),
            (Some("127.0.0.1:1"), &[("origin", "http://127.0.0.1:8791")], None, Some(LocalProcess)),
            (Some("127.0.0.1:1"), &[("origin", "http://localhost:8791")], None, Some(LocalProcess)),
            (Some("127.0.0.1:1"), &[("sec-fetch-site", "Same-Origin")], None, Some(LocalProcess)),
            (Some("127.0.0.1:1"), &[("sec-fetch-site", "none")], None, Some(LocalProcess)),
            (Some("127.0.0.1:1"), &[("origin", "http://localhost:5173")], None, None),
            (Some("127.0.0.1:1"), &[("origin", "https://evil.example")], None, None),
            (Some("127.0.0.1:1"), &[("origin", "null")], None, None),
            (Some("127.0.0.1:1"), &[("sec-fetch-site", "cross-site")], None, None),
            (Some("127.0.0.1:1"), &[("sec-fetch-site", "same-site")], None, None),
            (Some("127.0.0.1:1"), &[("x-forwarded-for", "100.71.169.10")], None, None),
            (Some("127.0.0.1:1"), &[("forwarded", "for=100.71.169.10")], None, None),
            (Some("100.71.169.10:1"), &[], None, None),
            (Some("192.168.0.5:1"), &[], None, None),
            (Some("[::ffff:192.168.0.5]:1"), &[], None, None),
            (None, &[], None, None),
            (Some("100.71.169.10:1"), &[("authorization", "Bearer server-token")], Some("server-token"), Some(ServerToken)),
            (None, &[("authorization", "Bearer server-token")], Some("server-token"), Some(ServerToken)),
            (Some("100.71.169.10:1"), &[("authorization", "Bearer other")], Some("server-token"), None),
            (Some("100.71.169.10:1"), &[("authorization", "Bearer ")], Some(""), None),
            (Some("127.0.0.1:1"), &[("authorization", "Bearer ")], Some(""), Some(LocalProcess)),
        ];
        for (peer, pairs, token, expected) in cases {
            let peer = peer.map(|peer| peer.parse::<SocketAddr>().unwrap());
            assert_eq!(
                human_input_caller(&config(*token), &headers(pairs), peer),
                *expected,
                "{peer:?} {pairs:?} token={token:?}"
            );
        }
    }
}
