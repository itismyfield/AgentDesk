use crate::api_caller_observability::RequestPrincipal;
use crate::server::routes::{AppState, auth::auth_middleware};
use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, OriginalUri, Request},
    http::{Method, StatusCode},
    middleware,
    routing::any,
};
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

const NODE_PATHS: [(&str, Method); 2] = [
    ("/api/internal/node-probe", Method::GET),
    ("/api/internal/home-stop/v1", Method::POST),
];
const OTHER_INTERNAL: [&str; 5] = [
    "/api/internal/link-dispatch-thread",
    "/api/internal/card-thread",
    "/api/internal/pending-dispatch-for-thread",
    "/api/internal/escalation/emit",
    "/api/dispatched-sessions/webhook",
];
const HOOK_TUI: [&str; 6] = [
    "/api/hook/reset-status",
    "/api/hook/skill-usage",
    "/api/hook/session/example",
    "/hooks/claude/Stop",
    "/tui/send",
    "/tui/wait",
];

#[test]
fn absent_transport_or_original_uri_never_grants_tailnet_trust() {
    let mut req = request("/internal/node-probe", Method::GET, None);
    let peer = Some("100.64.0.7:1".parse().unwrap());
    assert!(!super::allows(&req, peer));
    req.extensions_mut()
        .insert(OriginalUri("/api/internal/node-probe".parse().unwrap()));
    assert!(!super::allows(&req, None));
    assert!(super::allows(&req, peer));
}

fn state(token: Option<&str>, runner: bool) -> (AppState, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.server.auth_token = token.map(str::to_owned);
    config.server.host = "0.0.0.0".into();
    config.cluster.runtime_profile = if runner {
        crate::config::RuntimeProfile::Runner
    } else {
        crate::config::RuntimeProfile::Full
    };
    config.cluster.nodes.insert(
        "known".into(),
        crate::config::ClusterNodeConfig {
            trusted_forward_origin: Some("http://100.64.0.7:8791".into()),
            ..Default::default()
        },
    );
    config.policies.dir = root.path().join("policies");
    config.policies.hot_reload = false;
    config.data.dir = root.path().join("data");
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let state = AppState {
        engine: crate::engine::PolicyEngine::new_with_pg(&config, None).unwrap(),
        config: Arc::new(config),
        pg_pool: None,
        broadcast_tx: crate::eventbus::new_broadcast(),
        batch_buffer: Default::default(),
        health_registry: None,
        cluster_instance_id: Some("worker".into()),
    };
    (state, root)
}

async fn principal(req: Request) -> axum::response::Response {
    let strength = req
        .extensions()
        .get::<RequestPrincipal>()
        .map(|p| p.auth_strength.as_str())
        .unwrap_or("missing");
    axum::response::Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("x-test-strength", strength)
        .body(Body::empty())
        .unwrap()
}

fn protected(token: Option<&str>) -> (Router, tempfile::TempDir) {
    let (state, root) = state(token, false);
    let mut api = Router::new();
    for path in NODE_PATHS
        .iter()
        .map(|(p, _)| *p)
        .chain(OTHER_INTERNAL)
        .chain(HOOK_TUI.into_iter().filter(|p| p.starts_with("/api/")))
        .chain([
            "/api/sessions/example/tmux-output",
            "/api/internal/node-probe/suffix",
        ])
    {
        api = api.route(path.strip_prefix("/api").unwrap(), any(principal));
    }
    let mut root_routes = Router::new();
    for path in HOOK_TUI.into_iter().filter(|p| !p.starts_with("/api/")) {
        root_routes = root_routes.route(path, any(principal));
    }
    let api = api.route_layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
    ));
    let root_routes =
        root_routes.route_layer(middleware::from_fn_with_state(state, auth_middleware));
    let app = Router::new().nest("/api", api).merge(root_routes);
    (app, root)
}

fn request(path: &str, method: Method, peer: Option<&str>) -> Request {
    let mut req = Request::builder()
        .uri(path)
        .method(method)
        .body(Body::empty())
        .unwrap();
    if let Some(peer) = peer {
        req.extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    }
    req
}

async fn assert_strength(app: &Router, req: Request, expected: Option<&str>) {
    let path = req.uri().clone();
    let response = app.clone().oneshot(req).await.unwrap();
    match expected {
        Some(expected) => {
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
            assert_eq!(response.headers()["x-test-strength"], expected, "{path}");
        }
        None => {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert!(
                !response.headers().contains_key("x-test-strength"),
                "{path}"
            );
        }
    }
}

#[tokio::test]
async fn socket_peer_and_route_matrix_preserves_control_boundaries() {
    let (app, _root) = protected(None);
    for (peer, internal) in [
        (Some("127.0.0.1:40000"), Some("Loopback")),
        (Some("[::1]:40000"), Some("Loopback")),
        (Some("100.64.0.7:40000"), Some("TailnetPeer")),
        (Some("100.100.0.9:40000"), Some("TailnetPeer")),
        (Some("[fd7a:115c:a1e0::7]:40000"), Some("TailnetPeer")),
        (Some("192.168.1.7:40000"), None),
        (Some("10.1.2.3:40000"), None),
        (Some("203.0.113.7:40000"), None),
        (Some("[2001:db8::7]:40000"), None),
        (Some("[::ffff:100.64.0.7]:40000"), Some("TailnetPeer")),
        (Some("[::ffff:192.168.1.7]:40000"), None),
        (Some("[::ffff:127.0.0.1]:40000"), None),
        (None, None),
    ] {
        for (path, method) in NODE_PATHS {
            assert_strength(&app, request(path, method, peer), internal).await;
        }
        let local = internal.filter(|s| *s == "Loopback");
        for path in OTHER_INTERNAL.into_iter().chain(HOOK_TUI) {
            assert_strength(&app, request(path, Method::POST, peer), local).await;
        }
        assert_strength(
            &app,
            request("/api/sessions/example/tmux-output", Method::GET, peer),
            Some("None"),
        )
        .await;
    }
}

#[tokio::test]
async fn tailnet_cidr_boundaries_mapped_and_scoped_addresses() {
    let (app, _root) = protected(None);
    for peer in [
        "100.64.0.0:1",
        "100.127.255.255:1",
        "[::ffff:6440:1]:1",
        "[FD7A:115C:A1E0::]:1",
        "[fd7a:115c:a1e0:ffff:ffff:ffff:ffff:ffff]:1",
    ] {
        assert_strength(
            &app,
            request(NODE_PATHS[0].0, Method::GET, Some(peer)),
            Some("TailnetPeer"),
        )
        .await;
    }
    for peer in [
        "100.63.255.255:1",
        "100.128.0.0:1",
        "[fd7a:115c:a1df:ffff:ffff:ffff:ffff:ffff]:1",
        "[fd7a:115c:a1e1::]:1",
        "[fd00::1]:1",
        "[fe80::1%1]:1",
        "[::100.64.0.7]:1",
        "[64:ff9b::6440:7]:1",
        "[fd7a:115c:a1e0::7%1]:1",
        "[::ffff:100.64.0.7%1]:1",
    ] {
        assert_strength(
            &app,
            request(NODE_PATHS[0].0, Method::GET, Some(peer)),
            None,
        )
        .await;
    }
}

#[tokio::test]
async fn receiver_token_and_bearer_precedence_remains_explicit() {
    for token in [None, Some(""), Some("fixture-token"), Some(" ")] {
        let (app, _root) = protected(token);
        for peer in [Some("100.64.0.7:1"), Some("192.168.1.7:1"), None] {
            for bearer in [None, Some("fixture-token"), Some("wrong"), Some(" ")] {
                let mut req = request(NODE_PATHS[0].0, Method::GET, peer);
                if let Some(bearer) = bearer {
                    req.headers_mut()
                        .insert("authorization", format!("Bearer {bearer}").parse().unwrap());
                }
                let expected = if token.is_some_and(|t| !t.is_empty()) {
                    (token == bearer).then_some("ServerAdmin")
                } else {
                    (peer == Some("100.64.0.7:1")).then_some("TailnetPeer")
                };
                assert_strength(&app, req, expected).await;
            }
        }
        for path in OTHER_INTERNAL.into_iter().chain(HOOK_TUI) {
            assert_strength(
                &app,
                request(path, Method::POST, Some("127.0.0.1:1")),
                Some("Loopback"),
            )
            .await;
        }
        let expected = token.filter(|t| !t.is_empty()).is_none().then_some("None");
        assert_strength(
            &app,
            request(
                "/api/sessions/example/tmux-output",
                Method::GET,
                Some("100.64.0.7:1"),
            ),
            expected,
        )
        .await;
    }
}

#[tokio::test]
async fn forwarded_headers_never_replace_socket_proof() {
    let (app, _root) = protected(None);
    for peer in [Some("192.168.1.7:1"), None] {
        let mut req = request(NODE_PATHS[0].0, Method::GET, peer);
        for (name, value) in [
            ("x-agentdesk-forwarded-by", "worker"),
            ("x-agentdesk-session-owner", "worker"),
            ("forwarded", "for=100.64.0.7"),
            ("x-forwarded-for", "100.64.0.7"),
            ("tailscale-user-login", "fixture"),
            ("origin", "http://localhost:8791"),
            ("referer", "http://localhost:8791/"),
        ] {
            req.headers_mut().insert(name, value.parse().unwrap());
        }
        assert_strength(&app, req, None).await;
    }
}

#[tokio::test]
async fn exact_methods_original_uri_and_absent_original_uri() {
    let (app, _root) = protected(None);
    for (path, expected_method) in NODE_PATHS {
        for method in [Method::GET, Method::POST, Method::HEAD, Method::DELETE] {
            let expected = (method == expected_method).then_some("TailnetPeer");
            assert_strength(&app, request(path, method, Some("100.64.0.7:1")), expected).await;
        }
        let mut req = request(path, expected_method.clone(), Some("100.64.0.7:1"));
        req.extensions_mut()
            .insert(OriginalUri("/elsewhere".parse().unwrap()));
        assert_strength(&app, req, None).await;
        assert_strength(
            &app,
            request(
                &format!("{path}?check=1"),
                expected_method.clone(),
                Some("100.64.0.7:1"),
            ),
            Some("TailnetPeer"),
        )
        .await;
        let no_original =
            app.clone()
                .route_layer(middleware::map_request(|mut req: Request| async {
                    req.extensions_mut().remove::<OriginalUri>();
                    req
                }));
        assert_strength(
            &no_original,
            request(path, expected_method, Some("100.64.0.7:1")),
            None,
        )
        .await;
    }
    assert_strength(
        &app,
        request(
            "/api/internal/node-probe/suffix",
            Method::GET,
            Some("100.64.0.7:1"),
        ),
        None,
    )
    .await;
}

#[tokio::test]
async fn unknown_path_variants_never_enter_handlers_or_gain_principal() {
    for runner in [false, true] {
        let (state, _root) = state(None, runner);
        let hits = Arc::new(AtomicUsize::new(0));
        let principals = Arc::new(AtomicUsize::new(0));
        let route_hits = hits.clone();
        let fallback_principals = principals.clone();
        let app = Router::new()
            .nest(
                "/api",
                Router::new()
                    .route(
                        "/internal/node-probe",
                        any(move || {
                            let hits = route_hits.clone();
                            async move {
                                hits.fetch_add(1, Ordering::SeqCst);
                                StatusCode::NO_CONTENT
                            }
                        }),
                    )
                    .route_layer(middleware::from_fn_with_state(state, auth_middleware)),
            )
            .fallback(move |req: Request| {
                let seen = fallback_principals.clone();
                async move {
                    if req.extensions().get::<RequestPrincipal>().is_some() {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                    if runner {
                        StatusCode::NOT_FOUND
                    } else {
                        StatusCode::OK
                    }
                }
            });
        for path in [
            "/api/internal/node-probe/..",
            "/api/internal/node-probe/",
            "/api/internal/Node-Probe",
            "/api//internal/node-probe",
            "/api/internal//node-probe",
            "/api/internal/node%2Dprobe",
            "/api/internal/node-probe%2f..",
            "/api/internal/node-probe/%2e%2e",
            "/internal/node-probe",
            "/other/internal/node-probe",
        ] {
            let response = app
                .clone()
                .oneshot(request(path, Method::GET, Some("100.64.0.7:1")))
                .await
                .unwrap();
            if runner {
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            }
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert_eq!(principals.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn actual_runtime_profiles_accept_only_tailnet_node_probe() {
    for runner in [false, true] {
        let (state, _root) = state(None, runner);
        let access = crate::server::dashboard_auth::DashboardAccess::new(&state.config);
        let app = Router::new().nest(
            "/api",
            crate::server::routes::api_router_with_dashboard_access(state, access),
        );
        for (peer, status) in [
            (Some("100.64.0.7:1"), StatusCode::OK),
            (Some("192.168.1.7:1"), StatusCode::UNAUTHORIZED),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let response = app
                .clone()
                .oneshot(request(NODE_PATHS[0].0, Method::GET, peer))
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            if status == StatusCode::OK {
                let bytes = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(body["protocol"], 1);
                assert_eq!(body["instance_id"], "worker");
            }
        }
    }
}
