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
    HumanInputDelivery, HumanInputError, HumanInputRequest, deliver_human_input,
};

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
    let origin_id = non_empty(body.origin_id);
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
    delivery_response(channel_id, deliver_human_input(registry, request).await)
}

fn delivery_response(
    channel_id: u64,
    result: Result<HumanInputDelivery, HumanInputError>,
) -> RouteResponse {
    let channel = channel_id.to_string();
    let ok = |delivery: &str, turn_id: String, reason: Option<String>| {
        (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "delivery": delivery,
                "turn_id": turn_id,
                "channel_id": channel,
                "reason": reason,
            })),
        )
    };
    match result {
        Ok(HumanInputDelivery::Started { turn_id }) => ok("started", turn_id, None),
        Ok(HumanInputDelivery::Queued { turn_id, reason }) => ok("queued", turn_id, Some(reason)),
        Err(HumanInputError::AuthorNotAllowed) => {
            failure(StatusCode::FORBIDDEN, "author_not_allowed")
        }
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
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/agents/{agent}/turn/deliver"))
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
mod pg_tests {
    use std::sync::Arc;

    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use super::tests::{deliver, router};
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::health::{HealthRegistry, register_bot_auth_for_tests};

    #[tokio::test(flavor = "current_thread")]
    async fn target_and_author_gates_run_at_the_route_boundary_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (cc, cdx) = (6_245_101_u64, 6_245_102_u64);
        let (cc_id, cdx_id) = (cc.to_string(), cdx.to_string());
        let seed = crate::db::agents::insert_agent_channels_for_tests;
        seed(&pool, "deliver-agent", Some(&cc_id), Some(&cdx_id)).await;
        seed(&pool, "unbound-agent", None, None).await;
        let registry = Arc::new(HealthRegistry::new());
        register_bot_auth_for_tests(&registry, "claude", cc, Some(100), vec![200], false).await;
        // A second bot that opens itself to everyone must not widen who may inject.
        register_bot_auth_for_tests(&registry, "codex", cdx, None, vec![200], true).await;
        let app = router(Some(pool), Some(registry));
        let with = |extra: Value| {
            let mut payload = json!({"text": "status?", "author_discord_user_id": "200"});
            payload
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            payload.to_string()
        };
        #[rustfmt::skip]
        let cases = [
            ("missing-agent", with(json!({})), StatusCode::NOT_FOUND, "agent not found"),
            ("unbound-agent", with(json!({})), StatusCode::CONFLICT, "agent primary provider is not configured"),
            ("deliver-agent", with(json!({"channel_id": "999"})), StatusCode::FORBIDDEN, "channel override 999 is not allowed for agent deliver-agent"),
            ("deliver-agent", with(json!({"provider": "nope"})), StatusCode::BAD_REQUEST, "unsupported provider override: nope"),
            ("deliver-agent", with(json!({"author_discord_user_id": "300"})), StatusCode::FORBIDDEN, "author_not_allowed"),
            ("deliver-agent", with(json!({"provider": "codex"})), StatusCode::FORBIDDEN, "author_not_allowed"),
            ("deliver-agent", with(json!({})), StatusCode::SERVICE_UNAVAILABLE, "runtime_unavailable"),
        ];
        for (agent, payload, status, error) in cases {
            let (actual, response) = deliver(&app, agent, &payload).await;
            assert_eq!(
                (actual, response["error"].as_str()),
                (status, Some(error)),
                "{agent} {payload}"
            );
        }
    }
}
