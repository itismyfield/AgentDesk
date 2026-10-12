//! `POST /api/agents/{id}/turn/deliver`: human input that starts or queues a turn.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::agents_turn_target::{AgentTurnTarget, resolve_agent_turn_target};
use crate::services::discord::health::{
    HealthRegistry, HumanInputDelivery, HumanInputError, HumanInputRequest, deliver_human_input,
    deliver_with_reply, preflight_human_input,
};
use crate::services::imsg_reply;

const MAX_ORIGIN_ID_LEN: usize = 256;
const MAX_SOURCE_LEN: usize = 64;

#[derive(Debug, Deserialize)]
struct DeliverTurnInputBody {
    text: String,
    author_discord_user_id: String,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    origin_id: Option<String>,
}

type RouteResponse = (StatusCode, Json<Value>);

#[cfg(test)]
#[derive(Clone, Default)]
pub(super) struct DeliveryProbe {
    pub calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub entered: Option<std::sync::Arc<tokio::sync::Notify>>,
    pub release: Option<std::sync::Arc<tokio::sync::Notify>>,
}

#[cfg(test)]
tokio::task_local! {
    pub(super) static ADMISSION_ENABLED_FOR_TEST: bool;
    pub(super) static DELIVERY_PROBE_FOR_TEST: DeliveryProbe;
}

fn admission_enabled() -> bool {
    #[cfg(test)]
    if let Ok(enabled) = ADMISSION_ENABLED_FOR_TEST.try_with(|enabled| *enabled) {
        return enabled;
    }
    imsg_reply::is_enabled()
}

async fn observed_delivery(
    registry: &HealthRegistry,
    request: HumanInputRequest,
    reply: Option<imsg_reply::ReplyObligation>,
) -> Result<HumanInputDelivery, HumanInputError> {
    #[cfg(test)]
    if let Ok(probe) = DELIVERY_PROBE_FOR_TEST.try_with(Clone::clone) {
        probe
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(entered) = probe.entered {
            entered.notify_one();
        }
        if let Some(release) = probe.release {
            release.notified().await;
        }
    }
    match reply {
        Some(reply) => deliver_with_reply(registry, request, reply).await,
        None => deliver_human_input(registry, request).await,
    }
}

fn failure(status: StatusCode, error: &str) -> RouteResponse {
    (status, Json(json!({"ok": false, "error": error})))
}

fn bad_request(error: &str) -> RouteResponse {
    failure(StatusCode::BAD_REQUEST, error)
}

/// Only a canonical positive decimal snowflake is accepted, so no alternate
/// spelling can alias an allowed id.
fn parse_author_id(raw: &str) -> Option<u64> {
    let canonical = !raw.starts_with('0') && raw.len() <= 20;
    let digits = !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit());
    (canonical && digits)
        .then(|| raw.parse::<u64>().ok())
        .flatten()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub async fn deliver_turn_input(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> RouteResponse {
    let Ok(body) = serde_json::from_slice::<DeliverTurnInputBody>(&body) else {
        return bad_request("invalid_body");
    };
    let text = body.text.trim();
    if text.is_empty() {
        return bad_request("text_required");
    }
    let Some(author_id) = parse_author_id(&body.author_discord_user_id) else {
        return bad_request("invalid_author_id");
    };
    let source = non_empty(body.source).unwrap_or_else(|| "external".to_string());
    if source.len() > MAX_SOURCE_LEN {
        return bad_request("source_too_long");
    }
    let obligations_enabled = imsg_reply::enabled_for_source(admission_enabled(), &source);
    let origin_id = if obligations_enabled {
        body.origin_id.filter(|origin| !origin.trim().is_empty())
    } else {
        non_empty(body.origin_id)
    };
    if obligations_enabled && origin_id.is_none() {
        return bad_request("origin_id_required");
    }
    if origin_id
        .as_ref()
        .is_some_and(|origin| origin.len() > MAX_ORIGIN_ID_LEN)
    {
        return bad_request("origin_id_too_long");
    }

    let Some(pool) = state.pg_pool_ref() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "postgres pool unavailable");
    };
    let provider_override = non_empty(body.provider);
    let channel_override = non_empty(body.channel_id);
    let AgentTurnTarget {
        provider,
        primary_channel,
        channel_id,
    } = match resolve_agent_turn_target(
        pool,
        &id,
        provider_override.as_deref(),
        channel_override.as_deref(),
    )
    .await
    {
        Ok(target) => target,
        Err(response) => return response,
    };
    let Some(registry) = state.health_registry.as_deref() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "runtime_unavailable");
    };

    let channel_name_hint =
        (!primary_channel.chars().all(|ch| ch.is_ascii_digit())).then_some(primary_channel);
    let request = HumanInputRequest {
        channel_id: poise::serenity_prelude::ChannelId::new(channel_id),
        provider,
        text: text.to_string(),
        author_id,
        source: source.clone(),
        metadata: Some(json!({"human_input": {
            "source": source,
            "origin_id": origin_id,
            "author_discord_user_id": author_id.to_string(),
        }})),
        channel_name_hint,
    };
    if let (true, Some(origin_id)) = (obligations_enabled, origin_id.as_deref()) {
        let input = imsg_reply::AdmissionInput {
            origin_id,
            agent_id: &id,
            channel_id,
            author_id,
            source: &source,
            text,
        };
        let provider = request.provider.clone();
        let reply = imsg_reply::ReplyObligation::new(pool.clone(), origin_id);
        imsg_reply::deliver_once(
            pool,
            &input,
            || {
                preflight_human_input(
                    registry,
                    poise::serenity_prelude::ChannelId::new(channel_id),
                    &provider,
                    author_id,
                )
            },
            || observed_delivery(registry, request, Some(reply)),
            |result| delivery_response(channel_id, result),
        )
        .await
    } else {
        delivery_response(channel_id, observed_delivery(registry, request, None).await)
    }
}

fn delivery_response(
    channel_id: u64,
    result: Result<HumanInputDelivery, HumanInputError>,
) -> RouteResponse {
    let channel = channel_id.to_string();
    // `inject_veto` and `detail` are omitted when unset; with injection off only the base fields remain.
    let ok = |delivery: &str, turn_id: Option<String>, reason: Option<&str>, extra: Value| {
        let mut body = json!({
            "ok": true,
            "delivery": delivery,
            "turn_id": turn_id,
            "channel_id": channel,
            "reason": reason,
        });
        if let (Some(body), Value::Object(extra)) = (body.as_object_mut(), extra) {
            body.extend(extra);
        }
        (StatusCode::OK, Json(body))
    };
    match result {
        Ok(HumanInputDelivery::Started { turn_id }) => {
            ok("started", Some(turn_id), None, json!({}))
        }
        Ok(HumanInputDelivery::Queued {
            turn_id,
            reason,
            inject_veto,
        }) => {
            let extra = inject_veto.map_or_else(|| json!({}), |veto| json!({"inject_veto": veto}));
            ok("queued", Some(turn_id), Some(&reason), extra)
        }
        Ok(HumanInputDelivery::Injected { turn_id }) => ok("injected", turn_id, None, json!({})),
        Ok(HumanInputDelivery::Unconfirmed { turn_id, detail }) => ok(
            "unconfirmed",
            turn_id,
            Some("inject_unconfirmed"),
            json!({"detail": detail}),
        ),
        Err(HumanInputError::AuthorNotAllowed) => {
            failure(StatusCode::FORBIDDEN, "author_not_allowed")
        }
        Err(HumanInputError::InvalidTarget(detail)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"ok": false, "error": "invalid_target", "detail": detail})),
        ),
        Err(HumanInputError::RuntimeUnavailable(detail)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "runtime_unavailable", "detail": detail})),
        ),
        Err(HumanInputError::QueueRefused(detail)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "queue_refused", "detail": detail})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::super::{AppState, domains};
    use crate::services::discord::health::HealthRegistry;

    #[test]
    fn invalid_turn_target_is_422() {
        let (status, body) = super::delivery_response(
            101,
            Err(super::HumanInputError::InvalidTarget(
                "provider mismatch".into(),
            )),
        );
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body.0["error"], "invalid_target");
    }

    #[test]
    fn each_delivery_carries_only_its_documented_fields() {
        use super::HumanInputDelivery::{Injected, Queued, Started, Unconfirmed};
        let queued = |veto: Option<&str>| Queued {
            turn_id: "discord:7:9".into(),
            reason: "external_turn_active".into(),
            inject_veto: veto.map(str::to_string),
        };
        #[rustfmt::skip]
        let cases = [
            (Started { turn_id: "discord:7:9".into() }, json!({"delivery": "started", "turn_id": "discord:7:9", "reason": null})),
            (queued(None), json!({"delivery": "queued", "turn_id": "discord:7:9", "reason": "external_turn_active"})),
            (queued(Some("not_busy")), json!({"delivery": "queued", "turn_id": "discord:7:9", "reason": "external_turn_active", "inject_veto": "not_busy"})),
            (Injected { turn_id: None }, json!({"delivery": "injected", "turn_id": null, "reason": null})),
            (Unconfirmed { turn_id: Some("discord:7:5".into()), detail: "not_observed".into() },
                json!({"delivery": "unconfirmed", "turn_id": "discord:7:5", "reason": "inject_unconfirmed", "detail": "not_observed"})),
        ];
        for (delivery, fields) in cases {
            let (status, body) = super::delivery_response(7, Ok(delivery));
            let mut expected = json!({"ok": true, "channel_id": "7"});
            expected
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            assert_eq!((status, body.0), (StatusCode::OK, expected));
        }
    }

    pub(super) fn router(
        pg_pool: Option<sqlx::PgPool>,
        health_registry: Option<Arc<HealthRegistry>>,
    ) -> Router {
        let config = crate::config::Config::default();
        let engine = crate::engine::PolicyEngine::new(&config).expect("policy engine");
        let broadcast_tx = crate::eventbus::new_broadcast();
        let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
        let state = AppState {
            pg_pool,
            engine,
            config: Arc::new(config),
            broadcast_tx,
            batch_buffer,
            health_registry,
            cluster_instance_id: None,
        };
        domains::runtime::router(state.clone()).with_state(state)
    }

    pub(super) async fn deliver(app: &Router, agent: &str, body: &str) -> (StatusCode, Value) {
        post(app, &format!("/agents/{agent}/turn/deliver"), body).await
    }

    pub(super) async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    pub(super) async fn post(app: &Router, uri: &str, body: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    fn body(author: Value) -> String {
        json!({"text": "status?", "author_discord_user_id": author}).to_string()
    }

    #[tokio::test]
    async fn malformed_input_is_rejected_before_any_lookup() {
        let app = router(None, None);
        let long_origin = "x".repeat(257);
        #[rustfmt::skip]
        let cases = [
            ("not json".to_string(), "invalid_body"),
            (json!({"text": "hi"}).to_string(), "invalid_body"),
            (body(json!(42)), "invalid_body"),
            (json!({"text": "  ", "author_discord_user_id": "42"}).to_string(), "text_required"),
            (body(json!("")), "invalid_author_id"),
            (body(json!("abc")), "invalid_author_id"),
            (body(json!("0")), "invalid_author_id"),
            (body(json!("-42")), "invalid_author_id"),
            (body(json!("+42")), "invalid_author_id"),
            (body(json!(" 42")), "invalid_author_id"),
            (body(json!("042")), "invalid_author_id"),
            (body(json!("4.2e1")), "invalid_author_id"),
            (body(json!("18446744073709551616")), "invalid_author_id"),
            (json!({"text": "hi", "author_discord_user_id": "42", "origin_id": long_origin}).to_string(), "origin_id_too_long"),
        ];
        for (payload, error) in cases {
            let (status, response) = deliver(&app, "agent", &payload).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{payload}");
            assert_eq!(response["error"], error, "{payload}");
        }
        let (status, _) = deliver(&app, "agent", &body(json!("18446744073709551615"))).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "valid input stops at the missing pool"
        );
    }
}

#[cfg(test)]
mod pg_tests;
