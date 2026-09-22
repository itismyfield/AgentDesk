use super::*;
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

fn request(method: &str, path: &str, bearer: bool) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if bearer {
        builder = builder.header("authorization", "Bearer profile-test-token");
    }
    let mut request = builder.body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "192.0.2.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    request
}

#[tokio::test]
async fn worker_profile_routes_preserve_execution_auth_and_remove_admin_methods() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.policies.dir = dir.path().join("policies");
    config.policies.hot_reload = false;
    config.data.dir = dir.path().join("data");
    config.server.auth_token = Some("profile-test-token".into());
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let engine = PolicyEngine::new_with_pg(&config, None).unwrap();
    for profile in [
        crate::config::RuntimeProfile::Full,
        crate::config::RuntimeProfile::Worker,
    ] {
        config.cluster.runtime_profile = profile;
        let state = AppState {
            engine: engine.clone(),
            config: Arc::new(config.clone()),
            pg_pool: None,
            broadcast_tx: crate::eventbus::new_broadcast(),
            batch_buffer: Default::default(),
            health_registry: None,
            cluster_instance_id: None,
        };
        let app = compose_api_router(state.clone()).with_state(state);
        for (method, path) in [
            ("GET", "/health/detail"),
            ("GET", "/dispatched-sessions"),
            ("POST", "/dispatched-sessions/webhook"),
            ("GET", "/sessions/example/tmux-output"),
            ("POST", "/sessions/example/force-kill"),
            ("POST", "/turns/123/cancel"),
            ("POST", "/hook/reset-status"),
            ("GET", "/channels/123/watcher-state"),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(method, path, false))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED,
                "{profile:?}: {method} {path}"
            );
        }
        // A valid administrator credential cannot mount disabled worker routes.
        for (method, path) in [
            ("PUT", "/settings"),
            ("PATCH", "/settings/config"),
            ("POST", "/agents"),
            ("POST", "/agents/setup"),
            ("GET", "/agents/example/execution-node"),
            ("PUT", "/agents/example/execution-node"),
            ("POST", "/onboarding/complete"),
            ("PUT", "/voice/config"),
            ("PATCH", "/v1/settings/example"),
            ("POST", "/queue/reset-global"),
            ("POST", "/provider-auth-profiles/codex/login-start"),
        ] {
            let is_worker = profile == crate::config::RuntimeProfile::Worker;
            let response = app
                .clone()
                .oneshot(request(method, path, is_worker))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if is_worker {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::UNAUTHORIZED
                },
                "{profile:?}: {method} {path}"
            );
        }
        assert_eq!(
            app.oneshot(request("GET", "/dispatched-sessions", true))
                .await
                .unwrap()
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "authorized runtime request reaches the shared PG handler"
        );
    }
}
