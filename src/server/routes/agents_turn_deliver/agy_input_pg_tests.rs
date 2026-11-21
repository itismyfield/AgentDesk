use std::{num::NonZeroU64, sync::Arc};

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::{
    HealthRegistry, InjectPane, end_turn, inject_hook, queue_texts,
    register_inject_runtime_for_provider, seed_external_turn_row_for_tests, start_without_gateway,
};
use crate::services::imsg_reply::agy_input::{self, SELECTED_WRITER, WriterGate, WriterSelection};
use crate::services::provider::ProviderKind;

use super::tests::{deliver, post, router};
use super::{ADMISSION_ENABLED_FOR_TEST, DELIVERY_PROBE_FOR_TEST, DeliveryProbe};

const AGENT: &str = "agy-assistant-fixture";
const GUID: &str = "p:0/Assistant-GUID";

struct Fixture {
    root: crate::config::TestRuntimeRootGuard,
    spool: tempfile::TempDir,
    db: TestPostgresDb,
    pool: PgPool,
    app: Router,
    shared: Arc<crate::services::discord::SharedData>,
    destination: u64,
}

impl Fixture {
    async fn new(destination: u64) -> Self {
        Self::with_provider(destination, ProviderKind::Antigravity).await
    }

    async fn with_provider(destination: u64, provider: ProviderKind) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ($1, $1, $2, $3)",
        )
        .bind(AGENT)
        .bind(provider.as_str())
        .bind(destination.to_string())
        .execute(&pool)
        .await
        .unwrap();
        let registry = Arc::new(HealthRegistry::new());
        let shared = register_inject_runtime_for_provider(
            &registry,
            &[destination],
            Some(pool.clone()),
            provider,
        )
        .await;
        let app = router(Some(pool.clone()), Some(registry));
        Self {
            root,
            spool: tempfile::tempdir().unwrap(),
            db,
            pool,
            app,
            shared,
            destination,
        }
    }

    fn selection(&self) -> WriterGate {
        WriterGate::ready(WriterSelection {
            root: self.spool.path().to_path_buf(),
            destination: self.destination,
            logical_channel: self.destination,
            generation: NonZeroU64::MIN,
        })
    }

    async fn request(&self, body: Value) -> (StatusCode, Value) {
        self.request_with_gate(body, self.selection()).await
    }

    async fn request_with_gate(&self, body: Value, gate: WriterGate) -> (StatusCode, Value) {
        SELECTED_WRITER
            .scope(
                gate,
                ADMISSION_ENABLED_FOR_TEST
                    .scope(false, deliver(&self.app, AGENT, &body.to_string())),
            )
            .await
    }

    async fn watcher_start(&self, input: &Value) -> (StatusCode, Value) {
        let start = json!({
            "prompt": input["text"], "channel_id": input["channel_id"],
            "provider": input["provider"], "source": input["source"],
        });
        post(
            &self.app,
            &format!("/agents/{AGENT}/turn/start"),
            &start.to_string(),
        )
        .await
    }

    async fn origin_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM imsg_reply_obligations")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn finish(self) {
        drop((self.app, self.shared));
        self.pool.close().await;
        self.db.drop().await;
        drop(self.root);
    }
}

fn assistant_input(destination: u64) -> Value {
    json!({
        "author_discord_user_id": "200", "source": "imessage", "origin_id": GUID,
        "channel_id": destination.to_string(), "provider": "antigravity", "text": "status?",
    })
}

#[tokio::test(flavor = "current_thread")]
async fn s1b_activated_legacy_watcher_start_409_delivers_the_same_original_once_pg() {
    let fixture = Fixture::new(7_896_450).await;
    let input = assistant_input(fixture.destination);
    let answer = SELECTED_WRITER
        .scope(fixture.selection(), fixture.watcher_start(&input))
        .await;
    assert_eq!(answer.0, StatusCode::CONFLICT, "{}", answer.1);
    assert_eq!(answer.1["error"], "imessage_deliver_required");
    assert_eq!(fixture.origin_count().await, 0);
    assert!(
        queue_texts(&fixture.shared, fixture.destination)
            .await
            .is_empty()
    );
    assert_eq!(std::fs::read_dir(fixture.spool.path()).unwrap().count(), 0);

    seed_external_turn_row_for_tests(&ProviderKind::Antigravity, fixture.destination);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let probe = DeliveryProbe {
        entered: Some(entered.clone()),
        release: Some(release.clone()),
        ..Default::default()
    };
    let (first, racing) = tokio::join!(
        DELIVERY_PROBE_FOR_TEST.scope(probe.clone(), fixture.request(input.clone())),
        async {
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await;
            let racing = fixture.request(input.clone()).await;
            release.notify_one();
            result.expect("the first delivery owns the origin");
            racing
        },
    );
    assert_eq!(racing.0, StatusCode::ACCEPTED, "{}", racing.1);
    assert_eq!(racing.1["error"], "admission_pending");
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["delivery"], "queued", "{}", first.1);
    assert_eq!(first.1["reason"], "external_turn_active");
    assert_eq!(
        fixture
            .request_with_gate(input.clone(), WriterGate::unready(fixture.destination))
            .await,
        first
    );
    assert_eq!(probe.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        queue_texts(&fixture.shared, fixture.destination).await,
        ["status?"]
    );
    assert_eq!(fixture.origin_count().await, 1);
    let row = crate::db::imsg_reply::read_obligation(&fixture.pool, GUID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.source, "external");
    let message = row.queued_message_id.as_deref().unwrap().parse().unwrap();
    let bound = agy_input::bound_row(
        Some(&fixture.pool),
        &ProviderKind::Antigravity,
        fixture.destination,
        message,
        false,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(bound.origin_id, GUID);
    let (stored, _) = crate::services::turn_orchestrator::load_pending_queues(
        &ProviderKind::Antigravity,
        fixture.shared.queue_fixture_parts().1,
    );
    let channel = poise::serenity_prelude::ChannelId::new(fixture.destination);
    let entry = &stored[&channel][0];
    assert_eq!(entry.message_id.get(), message);
    assert_eq!(
        entry.source_message_queued_generations[0]
            .origin_id
            .as_deref(),
        Some(GUID)
    );
    let restore = |channel, author, text| {
        agy_input::restore_queued(
            &row,
            &ProviderKind::Antigravity,
            channel,
            message,
            author,
            text,
        )
    };
    SELECTED_WRITER
        .scope(fixture.selection(), async {
            assert!(restore(fixture.destination, 200, "status?").is_some());
            assert!(restore(fixture.destination, 7, "status?").is_none());
            assert!(restore(fixture.destination, 200, "changed").is_none());
            assert!(restore(fixture.destination + 1, 200, "status?").is_none());
        })
        .await;
    assert!(restore(fixture.destination, 200, "status?").is_none());
    let mut changed = input;
    changed["text"] = json!("changed");
    assert_eq!(fixture.request(changed).await.0, StatusCode::CONFLICT);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s1b_watcher_409_deliver_starts_idle_agy_and_binds_execution_id_pg() {
    let fixture = Fixture::new(7_896_452).await;
    let _gateway = fixture
        .shared
        .gateway_ready_for_tests(&[fixture.destination])
        .await;
    let _started = start_without_gateway(fixture.destination);
    let input = assistant_input(fixture.destination);
    let start = SELECTED_WRITER
        .scope(fixture.selection(), fixture.watcher_start(&input))
        .await;
    assert_eq!(start.0, StatusCode::CONFLICT);
    assert_eq!(start.1["error"], "imessage_deliver_required");
    assert!(inject_hook::starts_seen(fixture.destination).is_empty());
    assert_eq!(fixture.origin_count().await, 0);

    let answer = fixture.request(input.clone()).await;
    assert_eq!(answer.0, StatusCode::OK, "{}", answer.1);
    assert_eq!(answer.1["delivery"], "started");
    let turn = answer.1["turn_id"].as_str().unwrap();
    let message = turn.rsplit(':').next().unwrap();
    let episode = format!("turn:antigravity:{}:{message}", fixture.destination);
    assert_eq!(
        inject_hook::starts_seen(fixture.destination),
        [inject_hook::StartSeen {
            episode: Some(episode),
            contract: None
        }]
    );
    assert_eq!(fixture.request(input).await, answer);
    assert!(
        queue_texts(&fixture.shared, fixture.destination)
            .await
            .is_empty()
    );
    assert_eq!(fixture.origin_count().await, 1);
    end_turn(&fixture.shared, fixture.destination).await;
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s1b_watcher_409_deliver_injects_once_into_busy_claude_pane_pg() {
    let fixture = Fixture::with_provider(7_896_453, ProviderKind::Claude).await;
    let pane = InjectPane::new(fixture.destination, "external");
    let mut input = assistant_input(fixture.destination);
    input["provider"] = json!("claude");

    let start = fixture.watcher_start(&input).await;
    assert_eq!(start.0, StatusCode::CONFLICT, "{}", start.1);
    assert_eq!(start.1["reason"], "external_turn_active");
    assert_eq!(fixture.origin_count().await, 0);
    assert!(pane.records().is_empty());

    let body = input.to_string();
    let request = || ADMISSION_ENABLED_FOR_TEST.scope(false, deliver(&fixture.app, AGENT, &body));
    let response = request().await;
    assert_eq!(response.0, StatusCode::OK, "{}", response.1);
    assert_eq!(response.1["delivery"], "injected");
    assert_eq!(pane.records().len(), 1);
    assert!(pane.transcript_recorded_the_paste());
    assert_eq!(request().await, response);
    assert_eq!(pane.records().len(), 1);
    assert!(
        queue_texts(&fixture.shared, fixture.destination)
            .await
            .is_empty()
    );
    assert_eq!(fixture.origin_count().await, 1);
    drop(pane);
    fixture.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s1b_activated_originless_and_unproved_http_input_has_no_effect_pg() {
    let fixture = Fixture::new(7_896_451).await;
    let mut input = assistant_input(fixture.destination);
    input["origin_id"] = Value::Null;
    for (source, guid, expected) in [
        (
            "imessage",
            Value::Null,
            (StatusCode::BAD_REQUEST, "origin_id_required"),
        ),
        (
            "imessage",
            json!(" p:0/bad "),
            (StatusCode::BAD_REQUEST, "invalid_origin"),
        ),
        (
            "external",
            json!(GUID),
            (StatusCode::UNPROCESSABLE_ENTITY, "unsupported_input_origin"),
        ),
    ] {
        input["source"] = json!(source);
        input["origin_id"] = guid;
        let result = fixture.request(input.clone()).await;
        assert_eq!((result.0, result.1["error"].as_str().unwrap()), expected);
    }
    let mut denied = assistant_input(fixture.destination);
    denied["author_discord_user_id"] = json!("201");
    assert_eq!(fixture.request(denied).await.0, StatusCode::FORBIDDEN);
    let unready = fixture
        .request_with_gate(
            assistant_input(fixture.destination),
            WriterGate::unready(fixture.destination),
        )
        .await;
    assert_eq!(unready.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(unready.1["error"], "agy_writer_unavailable");
    assert_eq!(fixture.origin_count().await, 0);
    assert!(
        queue_texts(&fixture.shared, fixture.destination)
            .await
            .is_empty()
    );

    seed_external_turn_row_for_tests(&ProviderKind::Antigravity, fixture.destination);
    input["source"] = json!("imessage");
    input["origin_id"] = Value::Null;
    let disabled = ADMISSION_ENABLED_FOR_TEST
        .scope(false, deliver(&fixture.app, AGENT, &input.to_string()))
        .await;
    assert_eq!(disabled.0, StatusCode::OK, "{}", disabled.1);
    assert_eq!(disabled.1["delivery"], "queued");
    assert_eq!(fixture.origin_count().await, 0);
    fixture.finish().await;
}
