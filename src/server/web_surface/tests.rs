use super::*;
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
};
use std::sync::Arc;
use tower::ServiceExt;

fn app(dashboard_enabled: bool, root: &Path) -> Router {
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("web-entry-test-token".into());
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
