//! The operator terminate route's admission: who may call it, its refusal order and the switch
//! read before any IO, through the real API router and a real loopback listener.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::services::discord::health::HealthRegistry;
use crate::services::discord::herdr_terminate::route_tests::{
    JSON, LOOPBACK, app, body, capture, closed_pool, current_thread, in_child, post, refusal,
    switch, unreachable_pool,
};
use crate::services::discord::herdr_terminate::{
    OperatorTerminate, TerminationResult, terminate_registered_herdr,
};
use crate::services::termination_audit::host_terminate::herdr_terminate::{
    HerdrTerminateResult, TerminateRefusal,
};

const REMOTE: &str = "100.64.0.5:1234";
const TOKEN: &str = "t0k";

fn config(token: Option<&str>) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.server.auth_token = token.map(str::to_string);
    config
}

/// The state behind a router built here without the auth middleware.
fn state(token: Option<&str>, pool: sqlx::PgPool) -> crate::server::routes::AppState {
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let broadcast_tx = crate::server::ws::new_broadcast();
    crate::server::routes::AppState {
        pg_pool: Some(pool),
        engine: crate::engine::PolicyEngine::new(&engine_config).unwrap(),
        config: Arc::new(config(token)),
        batch_buffer: crate::server::ws::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: Some(Arc::new(HealthRegistry::new())),
        cluster_instance_id: None,
    }
}

// With the switch off, every authorized request is the same typed 409 whatever its body or
// dependencies, and neither the route nor the adapter reads PG or the runtime registry.
#[test]
fn act_route_switch_off_refuses_before_any_read() {
    if !in_child(concat!(
        module_path!(),
        "::act_route_switch_off_refuses_before_any_read"
    )) {
        return;
    }
    switch(false);
    current_thread().block_on(async {
        let (results, _logs) = capture();
        let disabled = refusal(StatusCode::CONFLICT, "disabled", "terminate disabled");
        let empty = Arc::new(HealthRegistry::new());
        let valid = body("claude/h/mac-mini:AgentDesk-claude-act", "nonce");
        let unknown = r#"{"session_key":"k","execution_nonce":"n","reason":"r","pane_id":"w1-1"}"#;
        let cases = [
            ("no registry or pool", app(None, None, None), valid.as_str()),
            (
                "closed pool, empty registry",
                app(None, Some(empty.clone()), Some(closed_pool().await)),
                &valid,
            ),
            (
                "unreachable pool, no runtime",
                app(None, Some(empty.clone()), Some(unreachable_pool())),
                &valid,
            ),
            ("unknown field", app(None, None, None), unknown),
        ];
        for (case, app, body) in cases {
            assert_eq!(
                post(&app, LOOPBACK, &[JSON], body).await,
                disabled,
                "{case}"
            );
        }
        let request = OperatorTerminate {
            session_key: "k".into(),
            execution_nonce: "n".into(),
        };
        let direct = terminate_registered_herdr(&empty, closed_pool().await, request, "r".into());
        assert_eq!(direct.await, TerminationResult::Disabled);
        assert!(
            results.take().is_empty(),
            "a refusal with no effect logs nothing"
        );
    });
}

// A remote peer, even with the operator Bearer, and a loopback browser page are refused before
// the switch; an adapter reached on the closed pool would answer 409 `refused` instead.
#[test]
fn act_route_non_loopback_peer_or_browser_origin_refused() {
    if !in_child(concat!(
        module_path!(),
        "::act_route_non_loopback_peer_or_browser_origin_refused"
    )) {
        return;
    }
    current_thread().block_on(async {
        let forbidden = refusal(StatusCode::FORBIDDEN, "forbidden", "local operator only");
        let pool = closed_pool().await;
        let registry = Some(Arc::new(HealthRegistry::new()));
        let valid = body("k", "n");
        let bearer = format!("Bearer {TOKEN}");
        for on in [true, false] {
            switch(on);
            let open = app(None, registry.clone(), Some(pool.clone()));
            assert_eq!(
                post(&open, REMOTE, &[JSON], &valid).await,
                forbidden,
                "N {on}"
            );
            let token = app(Some(TOKEN), registry.clone(), Some(pool.clone()));
            let headers = [JSON, ("authorization", bearer.as_str())];
            assert_eq!(
                post(&token, REMOTE, &headers, &valid).await,
                forbidden,
                "T {on}"
            );
        }
        switch(true);
        let origin = format!("http://127.0.0.1:{}", config(None).server.port);
        let open = app(None, registry.clone(), Some(pool.clone()));
        let headers = [JSON, ("origin", origin.as_str())];
        assert_eq!(post(&open, LOOPBACK, &headers, &valid).await, forbidden);
    });
}

// Unknown fields, empty values and a missing JSON content type stop at 400 before the adapter.
#[test]
fn act_route_rejects_unknown_or_empty_fields() {
    if !in_child(concat!(
        module_path!(),
        "::act_route_rejects_unknown_or_empty_fields"
    )) {
        return;
    }
    switch(true);
    current_thread().block_on(async {
        let app = app(
            None,
            Some(Arc::new(HealthRegistry::new())),
            Some(closed_pool().await),
        );
        let field = |name: &str, value: &str| {
            let mut fields = json!({"session_key": "k", "execution_nonce": "n", "reason": "r"});
            fields[name] = json!(value);
            fields.to_string()
        };
        let cases = [
            (field("pane_id", "w1-1"), true),
            (field("reason", " "), true),
            (field("execution_nonce", ""), true),
            (body("k", "n"), false),
        ];
        for (body, typed) in cases {
            let headers: &[(&str, &str)] = if typed { &[JSON] } else { &[] };
            let (status, reply) = post(&app, LOOPBACK, headers, &body).await;
            assert_eq!(
                (status, &reply["result"]),
                (StatusCode::BAD_REQUEST, &json!("bad_request")),
                "{body} typed={typed}: {reply}"
            );
        }
    });
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    addr
}

/// One POST over a real loopback connection, as the CLI or a browser would send it.
async fn http(addr: SocketAddr, headers: &[(&str, &str)]) -> (u16, Value) {
    let payload = body("k", "n");
    let mut request = format!(
        "POST /herdr/terminate HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n",
        payload.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&payload);
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8_lossy(&raw);
    let status = raw[9..12].parse().unwrap();
    let reply = raw.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    (status, serde_json::from_str(reply).unwrap_or(Value::Null))
}

// With a token configured only the operator Bearer reaches the switch: a loopback browser
// Referer is admitted by the middleware as `Loopback` and refused by the route. Without one,
// a loopback peer does; a route outside the middleware admits nobody.
#[test]
fn act_route_token_mode_requires_server_admin() {
    if !in_child(concat!(
        module_path!(),
        "::act_route_token_mode_requires_server_admin"
    )) {
        return;
    }
    switch(false);
    current_thread().block_on(async {
        let pool = closed_pool().await;
        let registry = Some(Arc::new(HealthRegistry::new()));
        let local = format!("http://127.0.0.1:{}", config(None).server.port);
        let referer = format!("{local}/");
        let (bearer, wrong) = (format!("Bearer {TOKEN}"), "Bearer nope".to_string());
        let token = serve(app(Some(TOKEN), registry.clone(), Some(pool.clone()))).await;
        let result = |reply: Value| reply["result"].as_str().map(str::to_string);
        let cases: [(&[(&str, &str)], u16, Option<&str>); 4] = [
            (&[("origin", local.as_str())], 403, Some("forbidden")),
            (&[("referer", referer.as_str())], 401, Some("unauthorized")),
            (&[("authorization", bearer.as_str())], 409, Some("disabled")),
            (&[("authorization", wrong.as_str())], 401, None),
        ];
        for (headers, status, expected) in cases {
            let (got, reply) = http(token, headers).await;
            assert_eq!(
                (got, result(reply.clone())),
                (status, expected.map(str::to_string)),
                "{headers:?}: {reply}"
            );
        }
        for token in [None, Some("")] {
            let open = serve(app(token, registry.clone(), Some(pool.clone()))).await;
            assert_eq!(http(open, &[]).await.0, 409, "{token:?} is the open mode");
        }
        for token in [None, Some(TOKEN)] {
            let bare = Router::new()
                .route("/herdr/terminate", axum::routing::post(super::terminate))
                .with_state(state(token, pool.clone()));
            let addr = serve(bare).await;
            let headers = [("authorization", bearer.as_str())];
            let (status, reply) = http(addr, &headers).await;
            assert_eq!(
                (status, result(reply)),
                (401, Some("unauthorized".into())),
                "{token:?}"
            );
        }
    });
}

// The reply table: one success, typed refusals, and server errors for unfinished runs.
#[test]
fn act_result_mapping_has_retired_as_the_only_success() {
    use HerdrTerminateResult as Close;
    let gate = crate::services::session_host::HerdrGateRefusal::MutationBusy;
    let table = [
        (TerminationResult::Retired, 200, "retired"),
        (TerminationResult::Disabled, 409, "disabled"),
        (
            TerminationResult::Close(Close::Refused(TerminateRefusal::Disabled)),
            409,
            "disabled",
        ),
        (TerminationResult::Refused("x".into()), 409, "refused"),
        (
            TerminationResult::Close(Close::Refused(TerminateRefusal::Gate(gate))),
            409,
            "gate_refused",
        ),
        (
            TerminationResult::Close(Close::NotSent("x".into())),
            409,
            "not_sent",
        ),
        (
            TerminationResult::Close(Close::ConfirmationRequired),
            409,
            "confirmation_required",
        ),
        (
            TerminationResult::Close(Close::Acknowledged),
            500,
            "indeterminate",
        ),
        (
            TerminationResult::Close(Close::Indeterminate("x".into())),
            500,
            "indeterminate",
        ),
        (
            TerminationResult::ProviderSurvivedClose,
            409,
            "provider_survived_close",
        ),
        (
            TerminationResult::Indeterminate("x".into()),
            500,
            "indeterminate",
        ),
    ];
    for (result, status, name) in table {
        let (got, _) = result.outcome();
        assert_eq!(
            (super::status_of(got).as_u16(), got),
            (status, name),
            "{result:?}"
        );
    }
}
