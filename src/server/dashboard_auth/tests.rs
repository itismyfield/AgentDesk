use super::*;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::routing::{get, post};
use axum::{Extension, Router};
use tower::ServiceExt;

fn config() -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("synthetic-test-token".into());
    config.server.port = 8791;
    config
}

fn headers(origin: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "192.0.2.1:8791".parse().unwrap());
    headers.insert(header::ORIGIN, origin.parse().unwrap());
    headers
}

#[test]
fn dashboard_tickets_are_origin_bound_expiring_single_use_and_bounded() {
    let access = DashboardAccess::new(&config());
    let same_origin = headers("http://192.0.2.1:8791");
    assert_eq!(
        access.issue(&headers("http://evil.example")).unwrap_err(),
        StatusCode::FORBIDDEN
    );
    for bad in [
        "null",
        "file:///tmp",
        "http://user@192.0.2.1:8791",
        "http://192.0.2.1:8791/path",
    ] {
        assert!(access.issue(&headers(bad)).is_err());
    }
    let (ticket, ttl) = access.issue(&same_origin).unwrap();
    assert_eq!(ttl, 15);
    assert!(access.authorize_ws(&same_origin, Some(&ticket)));
    assert!(!access.authorize_ws(&same_origin, Some(&ticket)));
    let (expired, _) = access.issue(&same_origin).unwrap();
    access
        .tickets
        .lock()
        .unwrap()
        .values_mut()
        .for_each(|t| t.expires = Instant::now());
    assert!(!access.authorize_ws(&same_origin, Some(&expired)));
    let (bound, _) = access.issue(&same_origin).unwrap();
    let other_scheme = headers("https://192.0.2.1:8791");
    assert!(!access.authorize_ws(&other_scheme, Some(&bound)));
    for _ in 0..MAX_PENDING_TICKETS {
        access.issue(&same_origin).unwrap();
    }
    assert_eq!(
        access.issue(&same_origin).unwrap_err(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // Restart/token rotation has a new ticket store, even for the same origin.
    assert!(!DashboardAccess::new(&config()).authorize_ws(&same_origin, Some(&bound)));
}

#[tokio::test]
async fn dashboard_session_probe_and_ticket_route_require_real_credentials_for_remote_peers() {
    use crate::server::routes::{AppState, auth};
    let config = config();
    let access = DashboardAccess::new(&config);
    let state = AppState {
        engine: crate::engine::PolicyEngine::new_with_pg(&config, None).unwrap(),
        config: Arc::new(config),
        pg_pool: None,
        broadcast_tx: crate::eventbus::new_broadcast(),
        batch_buffer: Default::default(),
        health_registry: None,
        cluster_instance_id: None,
    };
    let app = Router::new()
        .route("/auth/session", get(auth::get_session))
        .merge(
            Router::new()
                .route("/auth/ws-ticket", post(auth::issue_ws_ticket))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    auth::auth_middleware,
                )),
        )
        .with_state(state)
        .layer(Extension(access));
    for (peer, origin, bearer, expected) in [
        ("192.0.2.2:1000", "http://192.0.2.1:8791", None, false),
        ("192.0.2.2:1000", "http://127.0.0.1:8791", None, false),
        (
            "192.0.2.2:1000",
            "http://192.0.2.1:8791",
            Some("wrong"),
            false,
        ),
        (
            "192.0.2.2:1000",
            "http://192.0.2.1:8791",
            Some("synthetic-test-token"),
            true,
        ),
        ("127.0.0.1:1000", "http://127.0.0.1:8791", None, true),
    ] {
        for path in ["/auth/session", "/auth/ws-ticket"] {
            let mut request = Request::builder()
                .method(if path.ends_with("session") {
                    "GET"
                } else {
                    "POST"
                })
                .uri(path)
                .header(header::ORIGIN, origin)
                .header(header::HOST, origin.strip_prefix("http://").unwrap());
            if let Some(bearer) = bearer {
                request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
            }
            let mut request = request.body(Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
            let response = app.clone().oneshot(request).await.unwrap();
            if path.ends_with("session") {
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                let body: serde_json::Value =
                    serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                        .unwrap();
                assert_eq!(body["authenticated"], expected);
            } else {
                assert_eq!(
                    response.status(),
                    if expected {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                );
            }
        }
    }
}

#[tokio::test]
async fn dashboard_websocket_handshake_uses_ticket_or_bearer_never_query_token() {
    let access = DashboardAccess::new(&config());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let mut browser_headers = HeaderMap::new();
    browser_headers.insert(header::HOST, address.to_string().parse().unwrap());
    browser_headers.insert(header::ORIGIN, origin.parse().unwrap());
    let (ticket, _) = access.issue(&browser_headers).unwrap();
    let app = Router::new()
        .route("/ws", get(crate::server::ws::ws_handler))
        .with_state((crate::eventbus::new_broadcast(), access));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder().http1_only().build().unwrap();
    for (query, bearer, expected) in [
        (String::new(), None, StatusCode::UNAUTHORIZED),
        (
            "?token=synthetic-test-token".into(),
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            format!("?ticket={ticket}"),
            None,
            StatusCode::SWITCHING_PROTOCOLS,
        ),
        (format!("?ticket={ticket}"), None, StatusCode::UNAUTHORIZED),
        (
            String::new(),
            Some("synthetic-test-token"),
            StatusCode::SWITCHING_PROTOCOLS,
        ),
    ] {
        let mut request = client
            .get(format!("{origin}/ws{query}"))
            .header("Origin", &origin)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==");
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        assert_eq!(request.send().await.unwrap().status(), expected);
    }
    server.abort();
    let _ = server.await;
}
