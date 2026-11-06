use super::agents_turn_deliver::{
    ADMISSION_ENABLED_FOR_TEST, DELIVERY_PROBE_FOR_TEST, DeliveryProbe,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::OWNER_ADOPTS_FOR_TEST as ADOPTS;
use crate::services::discord::health::{HealthRegistry, InjectPane, register_inject_runtime};
use crate::services::provider::ProviderKind;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

pub(super) const ENVELOPE: &str =
    "[아이메시지 요청 · 시험]\n요청 본문\n\n회신: python3 /tmp/imsg_reply.py --origin suffix-guid";

tokio::task_local! {
    pub(super) static ENQUEUE_ATTEMPTS: Arc<AtomicUsize>;
}

struct Fixture {
    db: TestPostgresDb,
    pool: PgPool,
    app: Router,
    shared: Arc<crate::services::discord::SharedData>,
    registry: Arc<HealthRegistry>,
    channel: u64,
    server: tokio::task::AbortHandle,
    rest: crate::services::discord::RestGuardForTest,
    backend: Arc<AtomicUsize>,
    enqueue: Arc<AtomicUsize>,
}

impl Fixture {
    async fn new(channel: u64, legacy: bool) -> Self {
        let mut config = crate::config::Config::default();
        config.providers.entry("claude".into()).or_default().runtime =
            Some(if legacy { "pipe" } else { "tui" }.into());
        crate::services::provider_hosting::install_provider_hosting_config(&config);
        let execution =
            crate::services::discord::owner_execution_for_test(&ProviderKind::Claude, channel);
        let expected = if legacy {
            crate::services::discord::OwnerExecutionForTest::NonTuiProvider
        } else {
            crate::services::discord::OwnerExecutionForTest::LocalTui
        };
        assert_eq!(execution, expected);
        let generation = crate::services::discord::runtime_store::generation_path().unwrap();
        std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
        if !generation.exists() {
            std::fs::write(generation, "5").unwrap();
        }
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        crate::db::agents::insert_agent_channels_for_tests(
            &pool,
            "imessage-notice-agent",
            Some(&channel.to_string()),
            None,
        )
        .await;
        crate::db::agents::insert_agent_channels_for_tests(
            &pool,
            "imessage-sender",
            Some(&(channel + 100).to_string()),
            None,
        )
        .await;
        let registry = Arc::new(HealthRegistry::new());
        let shared = register_inject_runtime(&registry, &[channel], Some(pool.clone())).await;
        let api = Router::new().route("/gateway", axum::routing::get(
            |ws: axum::extract::ws::WebSocketUpgrade| async {
                ws.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
            })).fallback(axum::routing::any(move |method: axum::http::Method, uri: axum::http::Uri| async move {
                let user = |id: &str| json!({"id":id,"username":"notice-user","discriminator":"0","avatar":null});
                if uri.path().contains("/guilds/") && uri.path().contains("/members/") {
                    let id = uri.path().rsplit('/').next().unwrap();
                    return axum::Json(json!({"user":user(id),"nick":"notice-user","roles":[],"deaf":false,"mute":false,"flags":0}));
                }
                if method == axum::http::Method::POST && uri.path().ends_with("/users/@me/channels") {
                    return axum::Json(json!({"id":(channel+1000).to_string(),"type":1,"recipients":[user("200")]}));
                }
                if method == axum::http::Method::POST && uri.path().ends_with("/messages") {
                    return axum::Json(json!({"id":"900001","channel_id":channel.to_string(),"author":user("100"),"content":"",
                        "timestamp":"2026-11-06T00:00:00Z","edited_timestamp":null,"tts":false,"mention_everyone":false,
                        "mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0}));
                }
                let id = uri.path().rsplit('/').next().unwrap_or("1");
                axum::Json(json!({"id":id,"type":0,"name":"notice-channel","guild_id":"1","position":0,
                    "permission_overwrites":[],"nsfw":false,"parent_id":null}))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let http = Arc::new(
            poise::serenity_prelude::HttpBuilder::new("test-token")
                .proxy(format!("http://{address}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let server =
            tokio::spawn(async move { axum::serve(listener, api).await.unwrap() }).abort_handle();
        let rest = crate::services::discord::rest_for_test(http.clone());
        let context = crate::services::discord::gateway_context_for_test(
            http,
            format!("ws://{address}/gateway"),
        )
        .await;
        crate::services::discord::ready_gateway_for_test(&shared, context);
        let engine = crate::engine::PolicyEngine::new(&config).unwrap();
        let broadcast_tx = crate::eventbus::new_broadcast();
        let state = super::AppState {
            pg_pool: Some(pool.clone()),
            engine,
            config: Arc::new(config),
            batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
            broadcast_tx,
            health_registry: Some(registry.clone()),
            cluster_instance_id: None,
        };
        let app = super::domains::runtime::router(state.clone())
            .merge(super::domains::agents::router(state.clone()))
            .with_state(state);
        Self {
            db,
            pool,
            app,
            shared,
            registry,
            channel,
            server,
            rest,
            backend: Arc::new(AtomicUsize::new(0)),
            enqueue: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn call(&self, suffix: &str, body: &Value, obligations: bool) -> (StatusCode, Value) {
        self.call_with_probe(suffix, body, obligations, false).await
    }

    async fn call_with_probe(
        &self,
        suffix: &str,
        body: &Value,
        obligations: bool,
        inherited: bool,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/agents/imessage-notice-agent/{suffix}"))
            .header("content-type", "application/json")
            .extension(super::auth::loopback_peer())
            .body(Body::from(body.to_string()))
            .unwrap();
        let probe = DeliveryProbe {
            calls: self.backend.clone(),
            ..Default::default()
        };
        let response = ADOPTS
            .scope(
                true,
                ADMISSION_ENABLED_FOR_TEST.scope(
                    obligations,
                    ENQUEUE_ATTEMPTS.scope(self.enqueue.clone(), async {
                        if inherited {
                            self.app.clone().oneshot(request).await
                        } else {
                            DELIVERY_PROBE_FOR_TEST
                                .scope(probe, self.app.clone().oneshot(request))
                                .await
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn notices(&self) -> Vec<(String, String, String, Option<String>, Option<String>, bool)> {
        sqlx::query_as("SELECT content, bot, source, reason_code, session_key, dedupe_expires_at IS NULL FROM message_outbox WHERE source = 'imessage_input_notice' ORDER BY id")
            .fetch_all(&self.pool).await.unwrap()
    }

    async fn one(&self, expected: &str) {
        let rows = self.notices().await;
        assert_eq!(rows.len(), 1, "one durable echo: {rows:?}");
        assert_eq!(rows[0].0, expected);
        assert_eq!(
            rows[0].1,
            crate::services::discord::bot_role::UtilityBotRole::Notify.alias()
        );
        assert_eq!(rows[0].3.as_deref(), Some("imessage_input_notice"));
        assert!(
            rows[0]
                .4
                .as_deref()
                .unwrap()
                .starts_with(&format!("imessage_input_notice:{}:", self.channel))
        );
        assert!(rows[0].5);
    }

    async fn close(self) {
        drop((self.app, self.registry, self.shared));
        self.pool.close().await;
        self.db.drop().await;
        self.server.abort();
        drop(self.rest);
    }
}

fn input(text: &str, origin: Option<&str>) -> Value {
    json!({"text":text,"author_discord_user_id":"200","source":"imessage","origin_id":origin})
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_start_echoes_one_input_notice_line_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (channel, busy) in [(7_811_301, false), (7_811_302, true)] {
        let f = Fixture::new(channel, false).await;
        let pane = busy.then(|| InjectPane::new(channel, "external"));
        let (status, body) = f
            .call(
                "turn/start",
                &json!({"prompt":ENVELOPE,"source":"imessage"}),
                false,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "accepted", "{body}");
        assert!(
            crate::services::discord::input_owner_installed_for_test(
                &crate::services::discord::input_mailbox_for_test(&f.shared, channel)
            )
            .await
            .unwrap()
        );
        f.one("-# ⌨️ 시험: 요청 본문").await;
        drop(pane);
        f.close().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_decision_reply_start_echoes_label_and_body_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_303, false).await;
    let text = "[아이메시지 요청 · 시험 · 결정 알림 123 답장]\n결정 답장\n\n답장 대상 원문: python3 /tmp/imsg_reply.py --show 123\n회신: python3 /tmp/imsg_reply.py --origin suffix-guid";
    let (status, body) = f
        .call(
            "turn/start",
            &json!({"prompt":text,"source":"imessage"}),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    f.one("-# ⌨️ 시험: 결정 답장").await;
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_start_conflict_echoes_nothing_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_304, true).await;
    let pane = InjectPane::new(f.channel, "external");
    let (status, body) = f
        .call(
            "turn/start",
            &json!({"prompt":ENVELOPE,"source":"imessage"}),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(f.enqueue.load(Ordering::SeqCst), 0);
    assert!(pane.keys().is_empty());
    assert!(f.notices().await.is_empty());
    drop(pane);
    f.close().await;
}

async fn replay_echo(obligations: bool, channel: u64) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(channel, false).await;
    let request = input(ENVELOPE, Some("notice-replay-origin"));
    let first = f.call("turn/deliver", &request, obligations).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["delivery"], "queued", "{}", first.1);
    let second = f.call("turn/deliver", &request, obligations).await;
    assert_eq!(second, first);
    assert_eq!(f.backend.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.enqueue.load(Ordering::SeqCst),
        1,
        "replay must not call echo even when outbox dedupes it"
    );
    f.one("-# ⌨️ 시험: 요청 본문").await;
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_deliver_echoes_once_and_replay_adds_none_pg() {
    replay_echo(true, 7_811_305).await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_deliver_legacy_origin_echoes_once_and_replay_adds_none_pg() {
    replay_echo(false, 7_811_306).await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_deliver_without_origin_keeps_distinct_none_turn_inputs_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_307, true).await;
    let pane = InjectPane::new(f.channel, "external");
    for text in [
        ENVELOPE,
        "[아이메시지 요청 · 시험]\n두번째 입력\n회신: python3 /tmp/imsg_reply.py --origin other",
    ] {
        let (status, body) = f.call("turn/deliver", &input(text, None), false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["delivery"], "injected", "{body}");
        assert!(body["turn_id"].is_null(), "{body}");
    }
    assert_eq!(f.backend.load(Ordering::SeqCst), 2);
    assert_eq!(f.enqueue.load(Ordering::SeqCst), 2);
    assert_eq!(f.notices().await.len(), 2);
    drop(pane);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_deliver_unconfirmed_still_echoes_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_308, true).await;
    let pane = InjectPane::new(f.channel, "external");
    pane.set("fail_paste", "");
    let (status, body) = f
        .call(
            "turn/deliver",
            &input(ENVELOPE, Some("unconfirmed-notice")),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivery"], "unconfirmed", "{body}");
    assert_eq!(f.backend.load(Ordering::SeqCst), 1);
    assert_eq!(f.enqueue.load(Ordering::SeqCst), 1);
    f.one("-# ⌨️ 시험: 요청 본문").await;
    drop(pane);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn unparsed_imessage_envelope_still_delivers_and_echoes_flat_text_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_309, false).await;
    let (status, body) = f
        .call(
            "turn/start",
            &json!({"prompt":"해석 실패\n두 줄", "source":"imessage"}),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    f.one("-# ⌨️ iMessage: 해석 실패 두 줄").await;
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_handoff_echoes_one_input_notice_line_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_310, false).await;
    let (status, body) = f
        .call(
            "handoff",
            &json!({"from_agent_id":"imessage-sender", "prompt":ENVELOPE,"source":"imessage"}),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["channel_id"], f.channel.to_string());
    assert_eq!(body["status"], "accepted");
    f.one("-# ⌨️ 시험: 요청 본문").await;
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn unparsed_imessage_deliver_envelope_with_origin_still_echoes_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_311, false).await;
    let (status, body) = f
        .call(
            "turn/deliver",
            &input("해석 실패\n본문", Some("fallback-origin")),
            true,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivery"], "queued", "{body}");
    f.one("-# ⌨️ iMessage: 해석 실패 본문").await;
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn non_imessage_source_echoes_nothing_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_312, false).await;
    for (source, expected) in [
        ("external", 0),
        ("iMessage", 0),
        ("imessage-x", 0),
        (" imessage ", 1),
    ] {
        let (status, body) = f
            .call(
                "turn/start",
                &json!({"prompt":ENVELOPE,"source":source}),
                false,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{source}: {body}");
        assert_eq!(f.notices().await.len(), expected, "{source}");
    }
    f.one("-# ⌨️ 시험: 요청 본문").await;
    f.close().await;
}

fn metadata_objects(value: &Value, found: &mut Vec<Value>) {
    match value {
        Value::Object(fields) => {
            if let Some(metadata) = fields.get("metadata")
                && metadata.get("kept").is_some()
            {
                found.push(metadata.clone());
            }
            for value in fields.values() {
                metadata_objects(value, found);
            }
        }
        Value::Array(values) => {
            for value in values {
                metadata_objects(value, found);
            }
        }
        Value::String(text) => {
            if text.starts_with('{')
                && let Ok(value) = serde_json::from_str::<Value>(text)
            {
                metadata_objects(&value, found);
            }
        }
        _ => {}
    }
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_start_metadata_unchanged_by_echo() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_313, false).await;
    let (status, body) = f
        .call(
            "turn/start",
            &json!({"prompt":ENVELOPE,"source":"imessage", "metadata":{"kept":"unchanged"}}),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let path = crate::services::discord::input_queue_path_for_test(&f.shared, f.channel);
    let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut found = Vec::new();
    metadata_objects(&value, &mut found);
    assert!(
        !found.is_empty(),
        "the real admitted owner context contains its metadata: {value}"
    );
    assert!(
        found
            .iter()
            .all(|value| value == &json!({"kept":"unchanged"})),
        "suffix GUID must not enter metadata: {found:?}"
    );
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_start_dm_echoes_nothing_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_314, false).await;
    let (status, body) = f
        .call(
            "turn/start",
            &json!({"prompt":ENVELOPE,"source":"imessage","dm_user_id":"200",
        "metadata":{"parent_channel_id":f.channel.to_string()}}),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["turn_id"]
            .as_str()
            .unwrap()
            .starts_with(&format!("discord:{}:", f.channel + 1000)),
        "{body}"
    );
    assert!(f.notices().await.is_empty());
    assert_eq!(f.enqueue.load(Ordering::SeqCst), 0);
    f.close().await;
}

type BackendResult = Result<
    crate::services::discord::health::HumanInputDelivery,
    crate::services::discord::health::HumanInputError,
>;
tokio::task_local! {
    static BACKEND_RESULTS: Arc<std::sync::Mutex<Vec<BackendResult>>>;
}

pub(super) fn record_delivery_for_test(result: &BackendResult) {
    let _ = BACKEND_RESULTS.try_with(|results| results.lock().unwrap().push(result.clone()));
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_echo_enqueue_failure_preserves_result_pg() {
    use crate::services::discord::health::HumanInputDelivery;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_315, false).await;
    sqlx::raw_sql("CREATE FUNCTION reject_imessage_notice() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.source = 'imessage_input_notice' THEN RAISE EXCEPTION 'forced echo INSERT failure'; END IF;
        RETURN NEW; END; $$;
        CREATE TRIGGER reject_imessage_notice BEFORE INSERT ON message_outbox
        FOR EACH ROW EXECUTE FUNCTION reject_imessage_notice();")
        .execute(&f.pool).await.unwrap();
    sqlx::query("INSERT INTO message_outbox (target,content,bot,source) VALUES ('channel:1','other producer','notify','system')")
        .execute(&f.pool).await.unwrap();
    let results = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (status, body) = BACKEND_RESULTS
        .scope(
            results.clone(),
            f.call(
                "turn/deliver",
                &input(ENVELOPE, Some("enqueue-failure-origin")),
                true,
            ),
        )
        .await;
    let actual = results.lock().unwrap().clone();
    assert_eq!(actual.len(), 1);
    let HumanInputDelivery::Queued {
        turn_id,
        reason,
        inject_veto,
    } = actual[0].clone().unwrap()
    else {
        panic!("real owner accepted a queued request: {actual:?}");
    };
    assert!(inject_veto.is_none());
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"ok":true,"channel_id":f.channel.to_string(),"delivery":"queued",
        "turn_id":turn_id,"reason":reason})
    );
    assert_eq!(f.backend.load(Ordering::SeqCst), 1);
    assert_eq!(f.enqueue.load(Ordering::SeqCst), 1);
    assert!(f.notices().await.is_empty());
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_owner_external_hold_refusal_echoes_nothing_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_316, false).await;
    let pane = InjectPane::new(f.channel, "external");
    let (status, body) = f
        .call(
            "turn/deliver",
            &input(ENVELOPE, Some("owner-hold-refusal")),
            false,
        )
        .await;
    assert!(!status.is_success(), "known owner hold refusal: {body}");
    assert_eq!(body["error"], "queue_refused", "{body}");
    assert!(f.notices().await.is_empty());
    assert_eq!(f.enqueue.load(Ordering::SeqCst), 0);
    assert!(pane.keys().is_empty());
    drop(pane);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_owner_busy_start_echoes_one_input_notice_line_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_317, false).await;
    let pane = InjectPane::new(f.channel, "external");
    let (status, body) = f
        .call(
            "turn/start",
            &json!({"prompt": ENVELOPE, "source":"imessage"}),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "accepted");
    f.one("-# ⌨️ 시험: 요청 본문").await;
    drop(pane);
    f.close().await;
}

tokio::task_local! {
    static LEGACY_PREPARE_PAUSE: (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
}

pub(super) async fn before_legacy_prepare() {
    if let Ok((entered, release)) = LEGACY_PREPARE_PAUSE.try_with(Clone::clone) {
        entered.notify_one();
        release.notified().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_legacy_race_replay_after_preparation_refusal_enqueues_nothing_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_318, false).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let request = input(ENVELOPE, Some("legacy-prepare-race"));
    let loser = LEGACY_PREPARE_PAUSE.scope(
        (entered.clone(), release.clone()),
        f.call("turn/deliver", &request, false),
    );
    let winner = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        let result = f.call("turn/deliver", &request, false).await;
        assert_eq!(result.0, StatusCode::OK, "{}", result.1);
        crate::services::discord::revoke_notice_author_for_test(&f.shared).await;
        release.notify_one();
        result
    };
    let (loser, winner) = tokio::join!(loser, winner);
    assert_eq!(
        loser, winner,
        "the real preparation refusal replays the new winner"
    );
    assert_eq!(f.backend.load(Ordering::SeqCst), 1);
    assert_eq!(f.enqueue.load(Ordering::SeqCst), 1);
    f.one("-# ⌨️ 시험: 요청 본문").await;
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_deliver_concurrent_origin_has_one_backend_and_enqueue_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (obligations, channel) in [(true, 7_811_319), (false, 7_811_320)] {
        let f = Fixture::new(channel, false).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let request = input(ENVELOPE, Some("concurrent-origin"));
        let probe = DeliveryProbe {
            calls: f.backend.clone(),
            entered: Some(entered.clone()),
            release: Some(release.clone()),
        };
        let winner = DELIVERY_PROBE_FOR_TEST.scope(
            probe,
            f.call_with_probe("turn/deliver", &request, obligations, true),
        );
        let loser = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
                .await
                .unwrap();
            let answer = f.call("turn/deliver", &request, obligations).await;
            assert_eq!(answer.0, StatusCode::ACCEPTED, "{}", answer.1);
            assert_eq!(answer.1["error"], "admission_pending");
            assert_eq!(f.enqueue.load(Ordering::SeqCst), 0);
            release.notify_one();
        };
        let (winner, ()) = tokio::join!(winner, loser);
        assert_eq!(winner.0, StatusCode::OK, "{}", winner.1);
        assert_eq!(f.call("turn/deliver", &request, obligations).await, winner);
        assert_eq!(f.backend.load(Ordering::SeqCst), 1);
        assert_eq!(f.enqueue.load(Ordering::SeqCst), 1);
        f.one("-# ⌨️ 시험: 요청 본문").await;
        f.close().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn imessage_handoff_other_source_and_error_echo_nothing_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_321, false).await;
    let (status, body) = f
        .call(
            "handoff",
            &json!({"from_agent_id":"imessage-sender", "prompt": ENVELOPE, "source":"external"}),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = f
        .call(
            "handoff",
            &json!({"from_agent_id":"imessage-sender", "prompt": ENVELOPE, "source":"imessage", "channel_kind":"invalid-kind"}),
            false,
        )
        .await;
    assert!(!status.is_success(), "{body}");
    assert_eq!(f.enqueue.load(Ordering::SeqCst), 0);
    assert!(f.notices().await.is_empty());
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn unparsed_imessage_header_label_and_suffix_each_keep_the_full_text_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::new(7_811_322, false).await;
    let variants = [
        "[아이메시지 요청 · ]\n본문",
        "[다른 요청 · 시험]\n본문",
        "[아이메시지 요청 · 시험]\n본문\n회신: 알 수 없는 지시",
    ];
    for text in variants {
        let (status, body) = f.call("turn/deliver", &input(text, None), false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let rows = f.notices().await;
    assert_eq!(rows.len(), variants.len());
    for (row, text) in rows.iter().zip(variants) {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            row.0,
            crate::services::discord::input_notice_line(&format!("iMessage: {flat}"))
        );
    }
    f.close().await;
}
