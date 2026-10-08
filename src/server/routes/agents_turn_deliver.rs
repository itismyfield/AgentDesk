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
mod pg_tests {
    use std::sync::Arc;

    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use super::tests::{deliver, post, router};
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::health::{
        HealthRegistry, register_bot_auth_for_tests, seed_external_turn_row_for_tests,
    };
    use crate::services::provider::ProviderKind;

    #[tokio::test(flavor = "current_thread")]
    async fn an_external_tui_turn_row_is_busy_for_start_and_deliver_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let cc = 6_245_111_u64;
        crate::db::agents::insert_agent_channels_for_tests(
            &pool,
            "busy-agent",
            Some(&cc.to_string()),
            None,
        )
        .await;
        let registry = Arc::new(HealthRegistry::new());
        register_bot_auth_for_tests(&registry, "claude", cc, Some(100), vec![200], false).await;
        // The TUI-direct turn holds only its durable row; the mailbox stays free.
        seed_external_turn_row_for_tests(&ProviderKind::Claude, cc);
        let app = router(Some(pool), Some(registry));

        let start = json!({"prompt": "status?"}).to_string();
        let input = json!({"text": "status?", "author_discord_user_id": "200"}).to_string();
        let (start_status, started) = post(&app, "/agents/busy-agent/turn/start", &start).await;
        let (deliver_status, delivered) = deliver(&app, "busy-agent", &input).await;
        let shape = |status: StatusCode, body: &Value, key: &str| {
            let field = |name: &str| body[name].as_str().unwrap_or("-").to_string();
            format!("{} {} {}", status.as_u16(), field(key), field("reason"))
        };
        let observed = [
            shape(start_status, &started, "status"),
            shape(deliver_status, &delivered, "delivery"),
        ];
        assert_eq!(
            observed,
            [
                "409 conflict external_turn_active",
                "200 queued external_turn_active"
            ],
            "{started} {delivered}"
        );
    }

    /// The route reaches a scripted pane only when switched on; a pane veto hands the input back to
    /// the queue front and names itself.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn the_route_injects_into_a_busy_tui_direct_turn_only_when_switched_on_pg() {
        use crate::services::discord::health::{InjectPane, queue_texts, register_inject_runtime};
        let _root = crate::config::TestRuntimeRootGuard::new();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let agents = [
            ("inject-on", 6_245_121_u64, "external"),
            ("inject-off", 6_245_122, "off"),
            ("inject-attached", 6_245_123, "external"),
            ("inject-unconfirmed", 6_245_124, "external"),
        ];
        let channels = agents.map(|(_, channel, _)| channel);
        for (agent, channel, _) in agents {
            let seed = crate::db::agents::insert_agent_channels_for_tests;
            seed(&pool, agent, Some(&channel.to_string()), None).await;
        }
        let registry = Arc::new(HealthRegistry::new());
        let shared = register_inject_runtime(&registry, &channels, Some(pool.clone())).await;
        let [on, off, attached, unconfirmed] =
            agents.map(|(_, ch, mode)| InjectPane::new(ch, mode));
        attached.set("attach", "1");
        unconfirmed.set("fail_paste", "");
        let app = router(Some(pool), Some(registry));

        let input =
            json!({"text": "status?", "author_discord_user_id": "200", "source": "imessage"});
        let mut observed = Vec::new();
        for ((agent, channel, _), pane) in agents.iter().zip([&on, &off, &attached, &unconfirmed]) {
            let (status, body) = deliver(&app, agent, &input.to_string()).await;
            let field = |name: &str| match body.get(name) {
                None => "-".to_string(),
                Some(value) => value.as_str().unwrap_or("null").to_string(),
            };
            let queued = queue_texts(&shared, *channel).await.len();
            // A queued reply names a fresh reservation id on this channel.
            let prefix = format!("discord:{channel}:");
            let turn = field("turn_id");
            let turn = if turn.starts_with(&prefix) {
                format!("{prefix}*")
            } else {
                turn
            };
            observed.push(format!(
                "{agent}: {} {} turn={turn} reason={} veto={} detail={} keys={:?} tmux={} queued={queued}",
                status.as_u16(),
                field("delivery"),
                field("reason"),
                field("inject_veto"),
                field("detail"),
                pane.keys(),
                pane.tmux_calls() > 0,
            ));
        }
        assert_eq!(
            observed,
            [
                "inject-on: 200 injected turn=null reason=null veto=- detail=- keys=[\"paste-buffer\", \"send-keys\"] tmux=true queued=0",
                "inject-off: 200 queued turn=discord:6245122:* reason=external_turn_active veto=- detail=- keys=[] tmux=false queued=1",
                "inject-attached: 200 queued turn=discord:6245123:* reason=handed_back veto=human_attached detail=- keys=[] tmux=true queued=1",
                "inject-unconfirmed: 200 unconfirmed turn=null reason=inject_unconfirmed veto=- detail=paste_failed keys=[] tmux=true queued=0",
            ]
        );
        assert!(on.transcript_recorded_the_paste());
        let alerts = crate::services::observability::events::recent(10_000);
        let alerted = |channel: u64| {
            alerts.iter().any(|event| {
                event.event_type == "busy_inject_unconfirmed" && event.channel_id == Some(channel)
            })
        };
        assert_eq!(channels.map(alerted), [false, false, false, true]);
    }

    /// With the switch off the route answers as it did before injection existed: a start claims
    /// at once even past queued input, no success adds an injection field, and errors keep `detail`.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn the_switch_off_keeps_every_route_answer_field_for_field_pg() {
        use crate::services::discord::health::{
            end_turn, queue_texts, register_inject_runtime, start_without_gateway,
        };
        let _root = crate::config::TestRuntimeRootGuard::new();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let agents = [
            ("off-idle", 6_845_291_u64),
            ("off-row", 6_845_292),
            ("off-nogw", 6_845_293),
        ];
        for (agent, channel) in agents {
            let seed = crate::db::agents::insert_agent_channels_for_tests;
            seed(&pool, agent, Some(&channel.to_string()), None).await;
        }
        let registry = Arc::new(HealthRegistry::new());
        let channels = agents.map(|(_, channel)| channel);
        let shared = register_inject_runtime(&registry, &channels, Some(pool.clone())).await;
        let _starts = start_without_gateway(channels[0]);
        seed_external_turn_row_for_tests(&ProviderKind::Claude, channels[1]);
        let app = router(Some(pool), Some(registry));
        let input = json!({"text": "status?", "author_discord_user_id": "200"}).to_string();
        let answer = |(status, mut body): (StatusCode, Value), channel: u64| {
            let prefix = format!("discord:{channel}:");
            if body["turn_id"]
                .as_str()
                .is_some_and(|id| id.starts_with(&prefix))
            {
                body["turn_id"] = json!(format!("{prefix}*"));
            }
            format!("{} {body}", status.as_u16())
        };
        let (idle, row, nogw) = (channels[0], channels[1], channels[2]);
        let mut observed = vec![answer(deliver(&app, "off-idle", &input).await, idle)];
        observed.push(answer(deliver(&app, "off-idle", &input).await, idle));
        end_turn(&shared, idle).await;
        observed.push(answer(deliver(&app, "off-idle", &input).await, idle));
        observed.push(format!("left={}", queue_texts(&shared, idle).await.len()));
        observed.push(answer(deliver(&app, "off-row", &input).await, row));
        observed.push(answer(deliver(&app, "off-nogw", &input).await, nogw));
        assert_eq!(
            observed,
            [
                r#"200 {"channel_id":"6845291","delivery":"started","ok":true,"reason":null,"turn_id":"discord:6845291:*"}"#,
                r#"200 {"channel_id":"6845291","delivery":"queued","ok":true,"reason":"turn_active","turn_id":"discord:6845291:*"}"#,
                r#"200 {"channel_id":"6845291","delivery":"started","ok":true,"reason":null,"turn_id":"discord:6845291:*"}"#,
                "left=1",
                r#"200 {"channel_id":"6845292","delivery":"queued","ok":true,"reason":"external_turn_active","turn_id":"discord:6845292:*"}"#,
                r#"503 {"detail":"provider runtime is not ready","error":"runtime_unavailable","ok":false}"#,
            ]
        );
    }

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
