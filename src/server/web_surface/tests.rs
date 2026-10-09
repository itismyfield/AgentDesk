use super::*;
use crate::config::PeerFilterMode;
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

fn app(dashboard_enabled: bool, root: &Path) -> Router {
    app_with_peer_filter(dashboard_enabled, root, PeerFilterMode::default())
}

fn app_with_peer_filter(dashboard_enabled: bool, root: &Path, mode: PeerFilterMode) -> Router {
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("web-entry-test-token".into());
    config.server.peer_filter = mode;
    config.cluster.runtime_profile = if dashboard_enabled {
        crate::config::RuntimeProfile::Full
    } else {
        crate::config::RuntimeProfile::Runner
    };
    config.policies.dir = root.join("policies");
    config.policies.hot_reload = false;
    config.data.dir = root.join("data");
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let state = crate::server::routes::AppState {
        engine: crate::engine::PolicyEngine::new_with_pg(&config, None).unwrap(),
        config: Arc::new(config),
        pg_pool: None,
        broadcast_tx: crate::eventbus::new_broadcast(),
        batch_buffer: Default::default(),
        health_registry: None,
        cluster_instance_id: None,
    };
    router(state, &root.join("dashboard"), true)
}

fn request(method: Method, path: &str, peer: &str) -> Request<Body> {
    // A fresh address-bar navigation has neither Origin nor Referer.
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
    request
}

#[tokio::test]
async fn runner_browser_entry_explains_profile_without_token_or_dashboard_assets() {
    let root = tempfile::tempdir().unwrap();
    let app = app(false, root.path());
    for peer in ["127.0.0.1:50000", "[::1]:50000", "192.0.2.2:50000"] {
        for path in ["/", "/settings?settingsPanel=providers"] {
            let response = app
                .clone()
                .oneshot(request(Method::GET, path, peer))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{peer} {path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(
                response.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with("text/html")
            );
            let body =
                String::from_utf8(to_bytes(response.into_body(), 8192).await.unwrap().to_vec())
                    .unwrap();
            assert!(body.contains("AgentDesk Runner"));
            assert!(body.contains("Hub 장비의 주소"));
            assert!(!body.contains("web-entry-test-token"));
            assert!(!body.contains("Bearer token required"));
        }
    }
    assert!(!root.path().join("dashboard").exists());
    let head = app
        .oneshot(request(Method::HEAD, "/", "127.0.0.1:50000"))
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert!(to_bytes(head.into_body(), 8192).await.unwrap().is_empty());
}

#[tokio::test]
async fn runner_entry_keeps_execution_routes_protected_and_absent_routes_not_found() {
    let root = tempfile::tempdir().unwrap();
    let app = app(false, root.path());
    for (method, path, status) in [
        (Method::GET, "/api/health/detail", StatusCode::UNAUTHORIZED),
        (
            Method::POST,
            "/api/sessions/example/force-kill",
            StatusCode::UNAUTHORIZED,
        ),
        (Method::POST, "/tui/send", StatusCode::UNAUTHORIZED),
        (Method::POST, "/hooks/claude/Stop", StatusCode::UNAUTHORIZED),
        (Method::PUT, "/api/settings", StatusCode::NOT_FOUND),
        (
            Method::POST,
            "/api/provider-auth-profiles/codex/login-start",
            StatusCode::NOT_FOUND,
        ),
        (Method::GET, "/missing", StatusCode::NOT_FOUND),
        (Method::GET, "/api/missing", StatusCode::NOT_FOUND),
        (Method::GET, "/ws", StatusCode::NOT_FOUND),
        (Method::POST, "/", StatusCode::METHOD_NOT_ALLOWED),
    ] {
        let response = app
            .clone()
            .oneshot(request(method.clone(), path, "192.0.2.2:50000"))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{method} {path}");
    }
}

#[tokio::test]
async fn full_profile_keeps_public_spa_entry_and_protected_api() {
    let root = tempfile::tempdir().unwrap();
    let dashboard = root.path().join("dashboard");
    std::fs::create_dir(&dashboard).unwrap();
    std::fs::write(
        dashboard.join("index.html"),
        "<html>dashboard fixture</html>",
    )
    .unwrap();
    let app = app(true, root.path());
    for path in ["/", "/settings?settingsPanel=providers"] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path, "127.0.0.1:50000"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            "<html>dashboard fixture</html>"
        );
    }
    let response = app
        .oneshot(request(
            Method::GET,
            "/api/health/detail",
            "192.0.2.2:50000",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// Protected in every profile, so a peer that passes the filter gets the auth 401.
const PEER_PROBE_PATH: &str = "/api/sessions/example/force-kill";

async fn peer_probe(app: &Router, peer: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(request(Method::POST, PEER_PROBE_PATH, peer))
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 8192).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture_warnings() -> (LogBuffer, tracing::subscriber::DefaultGuard) {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(buffer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    (buffer, tracing::subscriber::set_default(subscriber))
}

fn peer_filter_warnings(buffer: &LogBuffer) -> Vec<String> {
    String::from_utf8_lossy(&buffer.0.lock().unwrap())
        .lines()
        .filter(|line| line.contains("peer filter:"))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn enforce_rejects_only_peers_outside_loopback_tailscale_and_private_lan() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with_peer_filter(false, root.path(), PeerFilterMode::Enforce);
    for peer in [
        "127.0.0.1",
        "127.255.0.9",
        "[::1]",
        "100.64.0.0",
        "100.127.255.255",
        "[fd7a:115c:a1e0::1]",
        "10.0.0.5",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.10",
        "169.254.1.1",
        "[fc00::1]",
        "[fdff::1]",
        "[fe80::1]",
        "[::ffff:127.0.0.1]",
        "[::ffff:100.71.1.1]",
        "[::ffff:192.168.1.10]",
    ] {
        let (status, body) = peer_probe(&app, &format!("{peer}:50000")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{peer} must reach auth");
        assert!(!body.contains("peer_not_allowed"), "{peer}: {body}");
    }
    for peer in [
        "192.0.2.2",
        "8.8.8.8",
        "11.0.0.1",
        "100.63.255.255",
        "100.128.0.0",
        "172.15.255.255",
        "172.32.0.1",
        "[2001:db8::1]",
        "[2606:4700::1111]",
        "[fec0::1]",
        "[::ffff:8.8.8.8]",
    ] {
        let (status, body) = peer_probe(&app, &format!("{peer}:50000")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{peer} must be rejected");
        assert!(body.contains("peer_not_allowed"), "{peer}: {body}");
    }
}

#[tokio::test]
async fn log_mode_serves_outside_peers_unchanged_and_warns_once_per_peer() {
    let root = tempfile::tempdir().unwrap();
    let off = app_with_peer_filter(false, root.path(), PeerFilterMode::Off);
    let log = app_with_peer_filter(false, root.path(), PeerFilterMode::Log);
    let unfiltered = peer_probe(&off, "8.8.8.8:50000").await;
    assert_eq!(unfiltered.0, StatusCode::UNAUTHORIZED);

    let (buffer, _guard) = capture_warnings();
    for peer in [
        "8.8.8.8:50000",
        "8.8.8.8:50001",
        "[2606:4700::1111]:50000",
        "192.168.1.10:50000",
    ] {
        assert_eq!(peer_probe(&log, peer).await, unfiltered, "{peer}");
    }
    let warnings = peer_filter_warnings(&buffer);
    assert_eq!(warnings.len(), 2, "{warnings:#?}");
    assert!(warnings[0].contains("peer_ip=8.8.8.8"), "{warnings:#?}");
    assert!(
        warnings[1].contains("peer_ip=2606:4700::1111"),
        "{warnings:#?}"
    );
    assert!(warnings.iter().all(|line| line.contains("WARN")));
}

#[tokio::test]
async fn off_mode_keeps_unfiltered_behavior_without_warnings() {
    let root = tempfile::tempdir().unwrap();
    let app = app_with_peer_filter(false, root.path(), PeerFilterMode::Off);
    let (buffer, _guard) = capture_warnings();
    let (status, body) = peer_probe(&app, "8.8.8.8:50000").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!body.contains("peer_not_allowed"), "{body}");
    assert!(peer_filter_warnings(&buffer).is_empty());
}

#[tokio::test]
async fn enforce_covers_dashboard_static_entry_and_websocket_route() {
    let root = tempfile::tempdir().unwrap();
    let dashboard = root.path().join("dashboard");
    std::fs::create_dir(&dashboard).unwrap();
    std::fs::write(dashboard.join("index.html"), "<html>dashboard</html>").unwrap();
    let app = app_with_peer_filter(true, root.path(), PeerFilterMode::Enforce);
    for path in ["/", "/ws", "/missing"] {
        let response = app
            .clone()
            .oneshot(request(Method::GET, path, "8.8.8.8:50000"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let response = app
        .oneshot(request(Method::GET, "/", "192.168.1.10:50000"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
