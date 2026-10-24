//! Direct cancel rejects legacy home-stop envelopes before reaching the ordinary cancel service.
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use super::super::{AppState, domains};

const TOKEN: &str = "home-stop-route-token";

pub(crate) fn app_with(
    config: crate::config::Config,
    pool: Option<sqlx::PgPool>,
    registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    holder: &str,
) -> Router {
    let broadcast_tx = crate::eventbus::new_broadcast();
    let state = AppState {
        pg_pool: pool,
        engine: crate::engine::PolicyEngine::new(&config).expect("policy engine"),
        config: Arc::new(config),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: registry,
        cluster_instance_id: Some(holder.into()),
    };
    domains::runtime::router(state.clone()).with_state(state)
}

fn app() -> Router {
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some(TOKEN.into());
    app_with(config, None, None, "holder-a")
}

pub(crate) async fn post(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, Value) {
    post_from_peer(app, uri, headers, body, "192.0.2.1:1234").await
}

async fn post_from_peer(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
    peer: &str,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(Method::POST).uri(uri);
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    let mut request = request.body(Body::from(body.to_owned())).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<std::net::SocketAddr>().unwrap(),
    ));
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
    super::CANCEL_SERVICE_ENTRIES.with(|count| count.set(0));
    let bearer = format!("Bearer {TOKEN}");
    let auth = [(header::AUTHORIZATION.as_str(), bearer.as_str())];
    let envelope =
        serde_json::json!({"v": 1, "request_id": "old-request", "channel_id": "5340300900",
        "provider": "claude", "expected_holder": "holder-a", "home_epoch": 7,
        "surface": "slash_stop", "intent": "user_stop", "force": false})
        .to_string();
    let uri = "/turns/5340300900/cancel";
    let (status, body) = post(&app, uri, &auth, &envelope).await;
    assert_eq!(
        super::CANCEL_SERVICE_ENTRIES.with(|count| count.get()),
        0,
        "v1 must not enter the direct cancel service"
    );
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
    assert_eq!(super::CANCEL_SERVICE_ENTRIES.with(|count| count.get()), 0);
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
