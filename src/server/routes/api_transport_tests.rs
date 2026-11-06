use super::*;
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode, header},
};
use serde_json::json;
use std::{io::Read, net::SocketAddr};
use tower::ServiceExt;

fn app(root: &std::path::Path, pool: sqlx::PgPool, token: Option<&str>, host: &str) -> Router {
    let mut config = crate::config::Config::default();
    config.server.host = host.into();
    config.server.auth_token = token.map(str::to_owned);
    config.policies.hot_reload = false;
    config.policies.dir = root.join("policies");
    config.data.dir = root.join("data");
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let state = AppState {
        engine: PolicyEngine::new_with_pg(&config, None).unwrap(),
        config: Arc::new(config),
        pg_pool: Some(pool),
        broadcast_tx: crate::eventbus::new_broadcast(),
        batch_buffer: Default::default(),
        health_registry: None,
        cluster_instance_id: None,
    };
    let access = crate::server::dashboard_auth::DashboardAccess::new(&state.config);
    Router::new().nest("/api", api_router_with_dashboard_access(state, access))
}

fn request(method: &str, path: &str, peer: &str, body: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    req
}

#[tokio::test]
async fn tailnet_read_and_control_route_matrix_pg() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let reads = [
        "/api/health/detail",
        "/api/doctor/startup/latest",
        "/api/message-outbox/failed?ids=1",
    ];
    // Valid extractor bodies let each request reach the handler's access gate.
    let controls = [
        (
            "/api/doctor/stale-mailbox/repair",
            "{}",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/channels/1/relay-recovery",
            "{}",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        ("/api/discord/send", "{}", StatusCode::SERVICE_UNAVAILABLE),
        (
            "/api/discord/send-dm",
            "{}",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            "/api/discord/send-to-agent",
            "{}",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            "/api/discord/bot-tokens/reload",
            "{}",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            "/api/inflight/rebind",
            "{}",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            "/api/message-outbox/failed/redrive",
            r#"{"ids":[],"idempotency_key":"fixture","reason":"fixture","dry_run":true}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/message-outbox/monitor-alerts",
            r#"{"target":"invalid","content":"fixture","action_id":"00000000000000000000000000000000","action":"alert","kind":"STUCK"}"#,
            StatusCode::BAD_REQUEST,
        ),
    ];
    let mut failures = Vec::new();
    for token in [None, Some(""), Some("fixture-token")] {
        let app = app(root.path(), pool.clone(), token, "0.0.0.0");
        for (peer, read_allowed, local) in [
            ("100.64.0.0:1", true, false),
            ("100.127.255.255:1", true, false),
            ("[fd7a:115c:a1e0::7]:1", true, false),
            ("[::ffff:100.64.0.7]:1", true, false),
            ("[fd7a:115c:a1e0::7%1]:1", false, false),
            ("[::ffff:100.64.0.7%1]:1", false, false),
            ("100.63.255.255:1", false, false),
            ("100.128.0.0:1", false, false),
            ("192.168.1.7:1", false, false),
            ("10.1.2.3:1", false, false),
            ("[fd00::7]:1", false, false),
            ("127.0.0.1:1", true, true),
            ("[::1]:1", true, true),
        ] {
            for bearer in [None, Some("fixture-token"), Some("wrong")] {
                let configured = token.is_some_and(|token| !token.is_empty());
                let authenticated = !configured || bearer == token;
                for (method, path, body, allowed, status) in reads
                    .iter()
                    .map(|path| ("GET", *path, "", read_allowed, StatusCode::OK))
                    .chain(controls.iter().map(|(path, body, status)| {
                        ("POST", *path, *body, local || configured, *status)
                    }))
                {
                    let mut req = request(method, path, peer, body);
                    // Spoofed forwarding headers must never turn a LAN/scoped peer into tailnet.
                    req.headers_mut()
                        .insert("x-forwarded-for", "100.64.0.7".parse().unwrap());
                    req.headers_mut()
                        .insert("forwarded", "for=100.64.0.7".parse().unwrap());
                    if let Some(bearer) = bearer {
                        req.headers_mut().insert(
                            header::AUTHORIZATION,
                            format!("Bearer {bearer}").parse().unwrap(),
                        );
                    }
                    let expected = if !authenticated {
                        StatusCode::UNAUTHORIZED
                    } else if allowed || configured {
                        status
                    } else {
                        StatusCode::FORBIDDEN
                    };
                    let response = app.clone().oneshot(req).await.unwrap();
                    if response.status() != expected {
                        failures.push(format!("token={token:?} bearer={bearer:?} {peer} {method} {path}: {} != {expected}", response.status()));
                    }
                }
            }
        }
    }
    // A loopback bind retains the pre-existing read-access exception.
    let app = app(root.path(), pool.clone(), None, "127.0.0.1");
    for path in reads {
        let response = app
            .clone()
            .oneshot(request("GET", path, "192.168.1.7:1", ""))
            .await
            .unwrap();
        if response.status() != StatusCode::OK {
            failures.push(format!("loopback bind {path}: {}", response.status()));
        }
    }
    drop(app);
    pool.close().await;
    fixture.drop().await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

async fn get(
    app: &Router,
    path: &str,
    encoding: &str,
    tag: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = request("GET", path, "127.0.0.1:1", "");
    req.headers_mut()
        .insert(header::ACCEPT_ENCODING, encoding.parse().unwrap());
    if let Some(tag) = tag {
        req.headers_mut()
            .insert(header::IF_NONE_MATCH, tag.parse().unwrap());
    }
    let response = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = response.into_parts();
    (
        parts.status,
        parts.headers,
        to_bytes(body, 1_000_000).await.unwrap().to_vec(),
    )
}

fn varies_encoding(headers: &HeaderMap) -> bool {
    headers.get_all(header::VARY).iter().any(|value| {
        value
            .to_str()
            .unwrap()
            .split(',')
            .any(|name| name.trim().eq_ignore_ascii_case("accept-encoding"))
    })
}

#[tokio::test]
async fn api_gzip_sse_and_conditional_response_matrix_pg() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let app = app(root.path(), pool.clone(), None, "0.0.0.0");
    let mut create = request(
        "POST",
        "/api/campaigns",
        "127.0.0.1:1",
        &json!({"id":"transport", "title":"Transport", "status":"active", "round":1, "nodes":[]})
            .to_string(),
    );
    create
        .headers_mut()
        .insert(header::AUTHORIZATION, "Bearer unused".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(create).await.unwrap().status(),
        StatusCode::CREATED
    );
    for path in ["/api/campaigns/transport", "/api/doctor/startup/latest"] {
        let (status, plain_headers, plain) = get(&app, path, "identity", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!plain_headers.contains_key(header::CONTENT_ENCODING));
        let (status, headers, compressed) = get(&app, path, "gzip", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(header::CONTENT_ENCODING)
                .map(|v| v.to_str().unwrap()),
            Some("gzip"),
            "{path}"
        );
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_slice())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, plain, "{path}");
        assert!(varies_encoding(&headers));
        assert!(varies_encoding(&plain_headers));
        if let Some(tag) = headers.get(header::ETAG) {
            let tag = tag.to_str().unwrap();
            assert!(tag.starts_with("W/\""));
            assert_eq!(plain_headers[header::ETAG], tag);
            for candidate in [
                tag.to_owned(),
                tag.trim_start_matches("W/").to_owned(),
                format!("\"other\", {tag}"),
                "*".into(),
            ] {
                for encoding in ["gzip", "identity"] {
                    let (status, headers, bytes) =
                        get(&app, path, encoding, Some(&candidate)).await;
                    assert_eq!(status, StatusCode::NOT_MODIFIED);
                    assert!(bytes.is_empty());
                    assert_eq!(headers[header::ETAG], tag);
                    assert!(!headers.contains_key(header::CONTENT_ENCODING));
                    assert!(varies_encoding(&headers));
                }
            }
        }
    }
    // No API handler currently emits SSE; exercise its streaming body with the same default layer.
    let stream = Router::new()
        .route(
            "/sse",
            axum::routing::get(|| async {
                axum::response::Sse::new(futures::stream::iter([
                    Ok::<_, std::convert::Infallible>(
                        axum::response::sse::Event::default().data("event".repeat(100)),
                    ),
                ]))
            }),
        )
        .layer(tower_http::compression::CompressionLayer::new());
    let (status, headers, bytes) = get(&stream, "/sse", "gzip", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_ENCODING));
    assert!(bytes.starts_with(b"data: "));
    drop(app);
    pool.close().await;
    fixture.drop().await;
}
