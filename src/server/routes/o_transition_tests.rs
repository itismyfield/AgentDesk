//! Who may reach the operator transition route, through the real API router and its auth
//! middleware: a refused caller resolves no context and sends nothing, whatever the token mode.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::test_hooks::SENDS;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::o_transition::test_hooks::RESOLVES;

const LOOPBACK: &str = "127.0.0.1:40000";
const REMOTE: &str = "100.64.0.5:1234";
const TOKEN: &str = "t0k";

fn port() -> u16 {
    crate::config::Config::default().server.port
}

/// A named caller: the router it reaches, its peer and its headers.
type Case<'a> = (&'a str, &'a Router, &'a str, Vec<(&'a str, &'a str)>);

/// The API router with its auth middleware over a registry and a closed pool.
async fn app(token: Option<&str>) -> Router {
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
    let tx = crate::server::ws::new_broadcast();
    let buffer = crate::server::ws::spawn_batch_flusher(tx.clone());
    let mut config = crate::config::Config::default();
    config.server.auth_token = token.map(str::to_string);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://postgres@127.0.0.1:1/absent")
        .unwrap();
    pool.close().await;
    let registry = Some(Arc::new(HealthRegistry::new()));
    crate::server::routes::api_router_with_pg(engine, config, tx, buffer, registry, Some(pool))
}

fn body(operation: &str) -> String {
    json!({
        "operation": operation, "channel": 5340, "session_key": "k", "execution_nonce": "n",
        "cutoff": 3, "job_id": "j", "operator": "op", "reason": "r", "termination": null,
    })
    .to_string()
}

/// The answer and how many contexts were resolved and requests sent meanwhile.
async fn post(
    app: &Router,
    peer: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, Value, (usize, usize)) {
    let counts = || (RESOLVES.with(|c| c.get()), SENDS.with(|c| c.get()));
    let before = counts();
    let mut request = Request::builder().method("POST").uri("/o/transition");
    request = request.header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::from(body.to_string())).unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let after = counts();
    let answer = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, answer, (after.0 - before.0, after.1 - before.1))
}

// A remote peer, even with the Bearer, and a loopback peer without exactly one loopback Origin on
// this port are refused before any context is resolved, in either token mode.
#[tokio::test]
async fn transition_route_refuses_all_but_a_loopback_peer_with_this_ports_origin() {
    let (open, token) = (app(None).await, app(Some(TOKEN)).await);
    let port = port();
    let origin = format!("http://127.0.0.1:{port}");
    let (other_port, bearer) = (
        format!("http://127.0.0.1:{}", port + 1),
        format!("Bearer {TOKEN}"),
    );
    let (external, lookalike) = (
        format!("http://evil.example:{port}"),
        format!("http://127.0.0.1.evil.example:{port}"),
    );
    let cases: [Case; 10] = [
        ("remote", &open, REMOTE, vec![("origin", &origin)]),
        (
            "remote_bearer",
            &token,
            REMOTE,
            vec![("origin", &origin), ("authorization", &bearer)],
        ),
        ("no_origin", &open, LOOPBACK, vec![]),
        (
            "no_origin_bearer",
            &token,
            LOOPBACK,
            vec![("authorization", &bearer)],
        ),
        ("other_port", &open, LOOPBACK, vec![("origin", &other_port)]),
        ("external", &open, LOOPBACK, vec![("origin", &external)]),
        ("lookalike", &open, LOOPBACK, vec![("origin", &lookalike)]),
        ("null", &open, LOOPBACK, vec![("origin", "null")]),
        ("referer_only", &open, LOOPBACK, vec![("referer", &origin)]),
        (
            "two_origins",
            &open,
            LOOPBACK,
            vec![("origin", &origin), ("origin", &external)],
        ),
    ];
    for (case, app, peer, headers) in cases {
        let (status, answer, reached) = post(app, peer, &headers, &body("inspect")).await;
        assert_eq!(
            (status, &answer["result"]),
            (StatusCode::FORBIDDEN, &json!("forbidden")),
            "[A1:{case}]"
        );
        assert_eq!(reached, (0, 0), "[A1:{case}] resolved, sent");
    }
    let localhost = format!("HTTP://LOCALHOST:{port}");
    let v6 = format!("http://[::1]:{port}");
    let admitted = [
        ("ipv4", &open, LOOPBACK, origin.as_str()),
        ("localhost_upper", &open, LOOPBACK, localhost.as_str()),
        ("ipv6", &open, "[::1]:40000", v6.as_str()),
        ("token_mode_no_bearer", &token, LOOPBACK, origin.as_str()),
    ];
    for (case, app, peer, origin) in admitted {
        let (status, answer, reached) =
            post(app, peer, &[("origin", origin)], &body("inspect")).await;
        assert_eq!(
            (status, &answer["result"]),
            (StatusCode::CONFLICT, &json!("refused")),
            "[A2:{case}] {answer}"
        );
        assert_eq!(reached, (1, 0), "[A2:{case}] resolved, sent");
    }
}

// An Apply without a well-formed v1 scope digest, or an Inspect with one, is refused before any
// context is resolved; a malformed body never reaches it, and a digested Apply goes on to it.
#[tokio::test]
async fn transition_route_refuses_an_apply_without_its_inspects_digest_before_any_read() {
    let open = app(None).await;
    let origin = format!("http://127.0.0.1:{}", port());
    let headers = [("origin", origin.as_str())];
    let hex = "ab".repeat(32);
    let with = |operation: &str, digest: &str| {
        let digest = format!("\"scope_digest\":\"{digest}\",\"reason\"");
        body(operation).replace("\"reason\"", &digest)
    };
    let refused = [
        ("apply_without", body("apply")),
        (
            "apply_null",
            body("apply").replace("\"reason\"", "\"scope_digest\":null,\"reason\""),
        ),
        ("apply_v2", with("apply", &format!("hrot-scope-v2:{hex}"))),
        ("apply_bare", with("apply", &hex)),
        (
            "apply_short",
            with("apply", &format!("hrot-scope-v1:{}", &hex[2..])),
        ),
        (
            "apply_long",
            with("apply", &format!("hrot-scope-v1:{hex}00")),
        ),
        (
            "apply_not_hex",
            with("apply", &format!("hrot-scope-v1:{}", "zz".repeat(32))),
        ),
        (
            "inspect_with",
            with("inspect", &format!("hrot-scope-v1:{hex}")),
        ),
        (
            "unknown_field",
            body("inspect").replace("\"reason\"", "\"extra\":1,\"reason\""),
        ),
    ];
    for (case, body) in refused {
        let (status, answer, reached) = post(&open, LOOPBACK, &headers, &body).await;
        let refused = (status, answer["result"].clone(), reached);
        let bad = (StatusCode::BAD_REQUEST, json!("bad_request"), (0, 0));
        assert_eq!(refused, bad, "[A3:{case}] {answer}");
    }
    let digested = with("apply", &format!("hrot-scope-v1:{hex}"));
    let (status, answer, reached) = post(&open, LOOPBACK, &headers, &digested).await;
    let resolved = (status, answer["result"].clone(), reached);
    let wanted = (StatusCode::CONFLICT, json!("refused"), (1, 0));
    assert_eq!(resolved, wanted, "[A3:digested] {answer}");
}
