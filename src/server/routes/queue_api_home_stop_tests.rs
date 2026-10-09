//! At the HTTP boundary a direct cancel never takes the home-stop envelope, and the holder
//! route sits behind the internal-path auth and the trusted forward.
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use super::super::{AppState, auth::auth_middleware, domains};

const TOKEN: &str = "home-stop-route-token";

fn app() -> Router {
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some(TOKEN.into());
    let broadcast_tx = crate::eventbus::new_broadcast();
    let state = AppState {
        pg_pool: None,
        engine: crate::engine::PolicyEngine::new(&config).expect("policy engine"),
        config: Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: Some("holder-a".into()),
    };
    let auth = axum::middleware::from_fn_with_state(state.clone(), auth_middleware);
    domains::runtime::router(state.clone())
        .layer(auth)
        .with_state(state)
}

async fn post(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(Method::POST).uri(uri);
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    let request = request.body(Body::from(body.to_owned())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn f1_direct_cancel_refuses_the_envelope_before_the_service_and_keeps_legacy_bodies() {
    let app = app();
    let bearer = format!("Bearer {TOKEN}");
    let auth = [(header::AUTHORIZATION.as_str(), bearer.as_str())];
    let envelope = String::from_utf8(
        crate::services::session_forwarding::home_stop::tests::request_body(
            5_340_300_900,
            "holder-a",
            7,
            "claude",
        ),
    )
    .unwrap();
    let uri = "/turns/5340300900/cancel";
    let (status, body) = post(&app, uri, &auth, &envelope).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["context"]["code"], "home_stop_envelope_on_cancel",
        "{body}"
    );
    for field in [
        "{\"request_id\":\"x\",\"force\":false}",
        "{\"home_epoch\":1}",
    ] {
        assert_eq!(
            post(&app, uri, &auth, field).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    // Legacy bodies reach the service, which here has no pool: never the envelope refusal.
    for legacy in [
        "",
        "{\"force\":true}",
        "{\"force\":false,\"note\":1}",
        "junk",
    ] {
        let (status, body) = post(&app, &format!("{uri}?force=true"), &auth, legacy).await;
        assert_ne!(status, StatusCode::BAD_REQUEST, "{legacy}: {body}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn f1_holder_route_needs_internal_auth_and_the_trusted_forward() {
    let app = app();
    let uri = "/internal/home-stop/v1";
    let body = String::from_utf8(
        crate::services::session_forwarding::home_stop::tests::request_body(
            5_340_300_901,
            "holder-a",
            7,
            "claude",
        ),
    )
    .unwrap();
    assert_eq!(
        post(&app, uri, &[], &body).await.0,
        StatusCode::UNAUTHORIZED
    );
    let bearer = format!("Bearer {TOKEN}");
    let auth = (header::AUTHORIZATION.as_str(), bearer.as_str());
    let (status, answer) = post(&app, uri, &[auth], &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(answer["code"], "home_stop_untrusted");
    let trusted = [
        auth,
        ("x-agentdesk-forwarded-by", "gw"),
        ("x-agentdesk-session-owner", "holder-a"),
    ];
    let (status, answer) = post(&app, uri, &trusted, &body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["reason"], "delegation_off", "{answer}");
}
