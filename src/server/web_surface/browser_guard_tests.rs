//! Browser guard: Host allowlist, cross-site rule, JSON body type, and WebSocket alignment.

use super::super::browser_guard::{BrowserGuard, RAW_JSON_ROUTES, Violation, is_raw_json_route};
use super::super::host_allowlist::{HostAllowlist, configured_names, parse_tailscale_self};
use super::*;
use crate::config::BrowserGuardMode;
use crate::server::dashboard_auth::DashboardAccess;

fn guarded_app(root: &Path, mode: BrowserGuardMode) -> Router {
    let dashboard = root.join("dashboard");
    std::fs::create_dir_all(&dashboard).unwrap();
    std::fs::write(dashboard.join("index.html"), "<html>dashboard</html>").unwrap();
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("web-entry-test-token".into());
    config.server.browser_guard = mode;
    config.cluster.runtime_profile = crate::config::RuntimeProfile::Full;
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
    router(state, &dashboard, true)
}

fn browser_request(
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder.body(Body::from(body.to_string())).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:50000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    request
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn browser_guard_warnings(buffer: &LogBuffer) -> Vec<String> {
    String::from_utf8_lossy(&buffer.0.lock().unwrap())
        .lines()
        .filter(|line| line.contains("browser guard:"))
        .map(str::to_string)
        .collect()
}

// Every surface, including the loopback-trusted prefixes that skip the token.
const HOST_PROBES: &[(&str, &str)] = &[
    ("GET", "/"),
    ("GET", "/ws"),
    ("GET", "/missing"),
    ("GET", "/api/health/detail"),
    ("POST", "/api/sessions/example/force-kill"),
    ("POST", "/tui/send"),
    ("POST", "/hooks/claude/Stop"),
    ("POST", "/api/hook/reset-status"),
    ("GET", "/api/internal/card-thread"),
    ("GET", "/api/internal/node-probe"),
];

#[tokio::test]
async fn browser_guard_enforce_rejects_foreign_host_on_every_path_including_loopback_trusted() {
    let root = tempfile::tempdir().unwrap();
    let app = guarded_app(root.path(), BrowserGuardMode::Enforce);
    for (method, path) in HOST_PROBES {
        let method = Method::from_bytes(method.as_bytes()).unwrap();
        for host in [
            "evil.example",
            "evil.example:8791",
            "attacker.tail1234.ts.net",
        ] {
            let (status, body) = send(
                &app,
                browser_request(method.clone(), path, &[("host", host)], ""),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {path} {host}: {body}"
            );
            assert!(body.contains("host_not_allowed"), "{method} {path}: {body}");
        }
        for host in [
            "127.0.0.1:8791",
            "localhost:8791",
            "[::1]:8791",
            "LOCALHOST.",
        ] {
            let (_, body) = send(
                &app,
                browser_request(method.clone(), path, &[("host", host)], ""),
            )
            .await;
            assert!(
                !body.contains("host_not_allowed"),
                "{method} {path} {host}: {body}"
            );
        }
    }
    let (status, body) = send(&app, browser_request(Method::GET, "/", &[], "")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "missing Host: {body}");
}

#[tokio::test]
async fn browser_guard_log_mode_serves_foreign_host_unchanged_and_warns_once() {
    let root = tempfile::tempdir().unwrap();
    let log = guarded_app(root.path(), BrowserGuardMode::Log);
    for (method, path) in HOST_PROBES {
        let method = Method::from_bytes(method.as_bytes()).unwrap();
        let (good_status, _) = send(
            &log,
            browser_request(method.clone(), path, &[("host", "127.0.0.1:8791")], ""),
        )
        .await;
        let (buffer, _guard) = capture_warnings();
        let (status, body) = send(
            &log,
            browser_request(method.clone(), path, &[("host", "evil.example")], ""),
        )
        .await;
        assert_eq!(status, good_status, "{method} {path}: {body}");
        assert!(
            !body.contains("host_not_allowed"),
            "{method} {path}: {body}"
        );
        let warnings = browser_guard_warnings(&buffer);
        assert_eq!(warnings.len(), 1, "{method} {path}: {warnings:#?}");
        for field in [
            "reason=\"host_not_allowed\"",
            "host=\"evil.example\"",
            "peer_ip=127.0.0.1",
            "mode=\"log\"",
        ] {
            assert!(warnings[0].contains(field), "{field}: {warnings:#?}");
        }
        assert!(
            warnings[0].contains(&format!("path=\"{path}\"")),
            "{warnings:#?}"
        );
        assert!(
            warnings[0].contains(&format!("method={method}")),
            "{warnings:#?}"
        );
    }
}

#[tokio::test]
async fn browser_guard_enforce_rejects_cross_site_state_changes() {
    let root = tempfile::tempdir().unwrap();
    let app = guarded_app(root.path(), BrowserGuardMode::Enforce);
    let host = ("host", "127.0.0.1:8791");
    for (headers, code) in [
        (
            vec![host, ("sec-fetch-site", "cross-site")],
            "cross_site_request",
        ),
        (
            vec![host, ("origin", "https://evil.example")],
            "origin_not_allowed",
        ),
        (vec![host, ("origin", "null")], "origin_not_allowed"),
        (
            vec![host, ("origin", "http://127.0.0.1:9999.evil.example")],
            "origin_not_allowed",
        ),
        (
            vec![
                host,
                ("origin", "https://evil.example"),
                ("sec-fetch-site", "same-site"),
            ],
            "origin_not_allowed",
        ),
    ] {
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            for path in [PEER_PROBE_PATH, "/tui/send", "/hooks/claude/Stop"] {
                let (status, body) =
                    send(&app, browser_request(method.clone(), path, &headers, "{}")).await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "{method} {path} {headers:?}: {body}"
                );
                assert!(body.contains(code), "{method} {path} {headers:?}: {body}");
            }
        }
    }
    // Safe methods are left to the Host rule.
    let (_, body) = send(
        &app,
        browser_request(
            Method::GET,
            "/api/health/detail",
            &[
                host,
                ("origin", "https://evil.example"),
                ("sec-fetch-site", "cross-site"),
            ],
            "",
        ),
    )
    .await;
    assert!(!body.contains("cross_site_request") && !body.contains("origin_not_allowed"));
}

#[tokio::test]
async fn browser_guard_passes_cli_and_same_origin_dashboard_requests() {
    let root = tempfile::tempdir().unwrap();
    let app = guarded_app(root.path(), BrowserGuardMode::Enforce);
    let cases: &[&[(&str, &str)]] = &[
        // CLI / MCP / node-to-node: no Origin, no Sec-Fetch-Site.
        &[
            ("host", "127.0.0.1:8791"),
            ("content-type", "application/json"),
        ],
        &[("host", "localhost:8791")],
        // Dashboard served by this server.
        &[
            ("host", "localhost:8791"),
            ("origin", "http://localhost:8791"),
            ("sec-fetch-site", "same-origin"),
        ],
        // Vite dev server proxy keeps Host and sends its own loopback Origin.
        &[
            ("host", "localhost:5173"),
            ("origin", "http://localhost:5173"),
        ],
        &[
            ("host", "127.0.0.1:8791"),
            ("origin", "http://localhost:5173"),
        ],
    ];
    for headers in cases {
        let (status, body) = send(
            &app,
            browser_request(Method::POST, PEER_PROBE_PATH, headers, "{}"),
        )
        .await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{headers:?}: {body}");
        assert!(!body.contains("origin_not_allowed") && !body.contains("host_not_allowed"));
    }
    // Without Origin a loopback CLI call still needs the token: the guard adds no bypass.
    let cli = browser_request(
        Method::POST,
        PEER_PROBE_PATH,
        &[("host", "127.0.0.1:8791")],
        "{}",
    );
    assert_eq!(send(&app, cli).await.0, StatusCode::UNAUTHORIZED);
    let authed = browser_request(
        Method::GET,
        "/api/health/detail",
        &[
            ("host", "localhost:8791"),
            ("authorization", "Bearer web-entry-test-token"),
        ],
        "",
    );
    let (status, body) = send(&app, authed).await;
    assert!(
        status != StatusCode::FORBIDDEN && status != StatusCode::UNAUTHORIZED,
        "{body}"
    );
}

fn concrete(template: &str) -> String {
    template.replace("{}", "1234567890")
}

#[tokio::test]
async fn browser_guard_requires_json_on_every_raw_body_handler() {
    let root = tempfile::tempdir().unwrap();
    let app = guarded_app(root.path(), BrowserGuardMode::Enforce);
    let host = ("host", "127.0.0.1:8791");
    for template in RAW_JSON_ROUTES {
        let path = concrete(template);
        for headers in [
            vec![host],
            vec![host, ("content-type", "text/plain")],
            vec![host, ("content-type", "application/x-www-form-urlencoded")],
        ] {
            let (status, body) = send(
                &app,
                browser_request(Method::POST, &path, &headers, "{\"a\":1}"),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{path} {headers:?}: {body}"
            );
            assert!(
                body.contains("json_content_type_required"),
                "{path}: {body}"
            );
        }
        for content_type in ["application/json", "application/json; charset=utf-8"] {
            let (status, body) = send(
                &app,
                browser_request(
                    Method::POST,
                    &path,
                    &[host, ("content-type", content_type)],
                    "{}",
                ),
            )
            .await;
            // A mounted route past the guard: neither the 415 nor the 404/405 fallback.
            for unexpected in [
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                StatusCode::NOT_FOUND,
                StatusCode::METHOD_NOT_ALLOWED,
            ] {
                assert_ne!(status, unexpected, "{path} {content_type}: {body}");
            }
        }
        let (status, body) = send(&app, browser_request(Method::POST, &path, &[host], "")).await;
        assert_ne!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "empty body {path}: {body}"
        );
    }
    let (status, _) = send(
        &app,
        browser_request(Method::POST, "/api/discord/send/extra", &[host], "{}"),
    )
    .await;
    assert_ne!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

fn collect_rs_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn raw_json_route_table_covers_every_bytes_body_handler() {
    let routes = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/routes");
    let mut files = Vec::new();
    collect_rs_files(&routes, &mut files);
    let handlers: usize = files
        .iter()
        .filter(|path| !path.to_string_lossy().contains("test"))
        .map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter(|line| line.trim() == "body: Bytes,")
                .count()
        })
        .sum();
    assert_eq!(handlers, RAW_JSON_ROUTES.len());
    assert!(is_raw_json_route("/api/queue/runs/r-1/phase-gates/repair"));
    assert!(!is_raw_json_route("/api/queue/runs//phase-gates/repair"));
    assert!(!is_raw_json_route("/api/queue/reset/"));
}

fn fixed_hosts() -> std::sync::Arc<HostAllowlist> {
    HostAllowlist::fixed(
        &[
            "localhost",
            "mac-mini",
            "mac-mini.tail1234.ts.net",
            "node-b.example",
        ],
        &[
            "192.168.1.20".parse().unwrap(),
            "100.101.102.103".parse().unwrap(),
        ],
        Some("mac-mini"),
    )
}

#[test]
fn host_allowlist_accepts_own_names_and_addresses_only() {
    let hosts = fixed_hosts();
    for host in [
        "localhost",
        "LocalHost:8791",
        "127.0.0.1",
        "127.9.9.9:8791",
        "[::1]:8791",
        "192.168.1.20:8791",
        "100.101.102.103",
        "mac-mini:8791",
        "mac-mini.tail1234.ts.net",
        "mac-mini.tail1234.ts.net.:443",
        "mac-mini.tail9999.ts.net",
        "node-b.example:8791",
    ] {
        assert!(hosts.host_allowed(host), "{host}");
    }
    for host in [
        "",
        "evil.example",
        "localhost.evil.example",
        "mac-mini.evil.example",
        "other.tail1234.ts.net",
        "mac-mini.a.b.ts.net",
        "mac-mini..ts.net",
        "192.168.1.21",
        "8.8.8.8:8791",
        "user@localhost",
    ] {
        assert!(!hosts.host_allowed(host), "{host}");
    }
}

#[test]
fn origin_must_match_the_request_host_or_an_exact_allowlisted_host() {
    let hosts = fixed_hosts();
    for (origin, host) in [
        (
            "https://mac-mini.tail1234.ts.net",
            Some("mac-mini.tail1234.ts.net"),
        ),
        (
            "https://mac-mini.tail1234.ts.net",
            Some("mac-mini.tail1234.ts.net:443"),
        ),
        ("http://localhost:5173", Some("127.0.0.1:8791")),
        ("http://192.168.1.20:8791", Some("192.168.1.20:8791")),
        ("http://[::1]:8791", Some("[::1]:8791")),
        ("http://node-b.example:8791", Some("localhost:8791")),
    ] {
        assert!(hosts.origin_allowed(origin, host), "{origin} -> {host:?}");
    }
    for (origin, host) in [
        ("https://evil.example", Some("localhost:8791")),
        ("null", Some("localhost:8791")),
        ("file:///tmp/x.html", Some("localhost:8791")),
        // The *.ts.net fallback guards Host only, never a foreign Origin.
        ("https://mac-mini.tail9999.ts.net", Some("localhost:8791")),
        ("http://192.168.1.99", Some("localhost:8791")),
        ("http://localhost.evil.example:8791", Some("localhost:8791")),
    ] {
        assert!(!hosts.origin_allowed(origin, host), "{origin} -> {host:?}");
    }
}

#[test]
fn allowlist_is_built_from_config_and_tailscale_status_without_operator_input() {
    let mut config = crate::config::Config::default();
    config.server.host = "Dash-Host.lan".into();
    config.cluster.api_base_url = Some("http://mac-mini.tail1234.ts.net:8791".into());
    config.cluster.nodes.insert(
        "worker".into(),
        crate::config::ClusterNodeConfig {
            trusted_forward_origin: Some("https://Worker.Example:8791/".into()),
            ..Default::default()
        },
    );
    let mut names = configured_names(&config);
    names.sort();
    assert_eq!(
        names,
        [
            "dash-host.lan",
            "mac-mini.tail1234.ts.net",
            "worker.example"
        ]
    );
    config.server.host = "0.0.0.0".into();
    assert!(!configured_names(&config).contains(&"0.0.0.0".to_string()));

    let status = serde_json::json!({
        "Self": {
            "DNSName": "Renamed-Box.tail1234.ts.net.",
            "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"]
        }
    });
    let (names, ips) = parse_tailscale_self(&status);
    assert_eq!(names, ["renamed-box", "renamed-box.tail1234.ts.net"]);
    assert_eq!(ips.len(), 2);
    assert_eq!(
        parse_tailscale_self(&serde_json::json!({})),
        (vec![], vec![])
    );
}

#[test]
fn check_reports_the_first_failing_rule() {
    let guard = BrowserGuard::with_hosts(BrowserGuardMode::Enforce, fixed_hosts());
    let headers = |pairs: &[(&'static str, &'static str)]| {
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    };
    let check = |method: Method, path: &str, pairs: &[(&'static str, &'static str)], body: bool| {
        guard.check(&method, path, &headers(pairs), None, body)
    };
    assert_eq!(
        check(Method::GET, "/", &[], false),
        Some(Violation::HostNotAllowed)
    );
    assert_eq!(
        guard.check(
            &Method::GET,
            "/",
            &headers(&[]),
            Some("localhost:8791"),
            false
        ),
        None
    );
    assert_eq!(
        check(
            Method::POST,
            "/api/discord/send",
            &[("host", "evil.example"), ("origin", "https://evil.example")],
            true
        ),
        Some(Violation::HostNotAllowed)
    );
    assert_eq!(
        check(
            Method::POST,
            "/api/discord/send",
            &[("host", "localhost"), ("sec-fetch-site", "CROSS-SITE")],
            true
        ),
        Some(Violation::CrossSite)
    );
    assert_eq!(
        check(
            Method::POST,
            "/api/discord/send",
            &[("host", "localhost"), ("content-type", "text/plain")],
            true
        ),
        Some(Violation::JsonBodyRequired)
    );
    assert_eq!(
        check(
            Method::POST,
            "/api/discord/send",
            &[
                ("host", "localhost"),
                ("content-type", "application/vnd.api+json")
            ],
            true
        ),
        None
    );
    assert_eq!(
        check(
            Method::POST,
            "/api/discord/send",
            &[("host", "localhost")],
            false
        ),
        None
    );
    assert_eq!(
        check(
            Method::OPTIONS,
            "/api/discord/send",
            &[("host", "localhost"), ("sec-fetch-site", "cross-site")],
            true
        ),
        None
    );
}

#[test]
fn websocket_admission_uses_the_same_host_allowlist_and_mode() {
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("ws-token".into());
    let ws_headers = |host: &'static str, origin: &'static str| {
        let mut map = axum::http::HeaderMap::new();
        map.insert(header::HOST, host.parse().unwrap());
        map.insert(header::ORIGIN, origin.parse().unwrap());
        map
    };
    let enforce = DashboardAccess::new(&config).with_browser_guard(BrowserGuard::with_hosts(
        BrowserGuardMode::Enforce,
        fixed_hosts(),
    ));
    let foreign = ws_headers("evil.example:8791", "http://evil.example:8791");
    assert_eq!(enforce.issue(&foreign).unwrap_err(), StatusCode::FORBIDDEN);
    assert!(!enforce.authorize_ws(&foreign, Some("anything")));
    let own = ws_headers(
        "mac-mini.tail1234.ts.net",
        "https://mac-mini.tail1234.ts.net",
    );
    let (ticket, _) = enforce.issue(&own).unwrap();
    assert!(enforce.authorize_ws(&own, Some(&ticket)));

    // Log mode leaves the decision to the HTTP layer's warning.
    let log = DashboardAccess::new(&config).with_browser_guard(BrowserGuard::with_hosts(
        BrowserGuardMode::Log,
        fixed_hosts(),
    ));
    assert!(log.issue(&foreign).is_ok());
}
