use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use poise::serenity_prelude as serenity;
use sqlx::Row;

use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;

const CHANNEL: u64 = 72_280_100;
const LEGACY_MESSAGE: u64 = 72_280_900;
const LEGACY_CONTENT: &str =
    "📋 Task event\n**Monitor event: \"coord events\"**\n\n> old event\n\n-# task monitor";

#[derive(Clone, Debug, PartialEq, Eq)]
struct WireCall {
    bot: String,
    content: String,
    nonce: String,
    message_id: u64,
}

#[derive(Default)]
struct Transport {
    posts: Mutex<Vec<WireCall>>,
    edits: Mutex<Vec<WireCall>>,
    reconciles: Mutex<Vec<(String, String, String)>>,
    committed: Mutex<HashMap<(String, String), (u64, String)>>,
    sequence: AtomicU64,
}

impl Transport {
    fn seed_committed(&self, claim: &ClaimedCard) {
        self.committed.lock().unwrap().insert(
            (claim.bot_key.clone(), claim.discord_nonce.clone()),
            (LEGACY_MESSAGE, claim.rendered_content.clone()),
        );
    }

    fn counts(&self) -> (usize, usize) {
        (
            self.posts.lock().unwrap().len(),
            self.edits.lock().unwrap().len(),
        )
    }

    fn latest(&self) -> WireCall {
        self.edits
            .lock()
            .unwrap()
            .last()
            .cloned()
            .or_else(|| self.posts.lock().unwrap().last().cloned())
            .expect("the exercised entrypoint must make a visible transport call")
    }
}

impl TaskCardTransport for Transport {
    async fn post_card(
        &self,
        bot: &CardBot,
        channel_id: u64,
        content: &str,
        nonce: &str,
    ) -> Result<u64, TaskCardTransportError> {
        assert_eq!(channel_id, CHANNEL);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let (message_id, original) = self
            .committed
            .lock()
            .unwrap()
            .entry((bot.key.clone(), nonce.to_string()))
            .or_insert_with(|| {
                (
                    72_281_000 + self.sequence.fetch_add(1, Ordering::AcqRel),
                    content.to_string(),
                )
            })
            .clone();
        self.posts.lock().unwrap().push(WireCall {
            bot: bot.key.clone(),
            content: content.to_string(),
            nonce: nonce.to_string(),
            message_id,
        });
        assert_eq!(
            original, content,
            "a nonce cannot change its committed bytes"
        );
        Ok(message_id)
    }

    async fn edit_card(
        &self,
        bot: &CardBot,
        channel_id: u64,
        message_id: u64,
        content: &str,
    ) -> Result<(), TaskCardTransportError> {
        assert_eq!(channel_id, CHANNEL);
        self.edits.lock().unwrap().push(WireCall {
            bot: bot.key.clone(),
            content: content.to_string(),
            nonce: String::new(),
            message_id,
        });
        Ok(())
    }

    async fn reconcile_card_post(
        &self,
        bot: &CardBot,
        channel_id: u64,
        nonce: &str,
        expected_hash: &str,
        _started_at: chrono::DateTime<chrono::Utc>,
    ) -> CardPostReconcile {
        assert_eq!(channel_id, CHANNEL);
        self.reconciles.lock().unwrap().push((
            bot.key.clone(),
            nonce.to_string(),
            expected_hash.to_string(),
        ));
        match self
            .committed
            .lock()
            .unwrap()
            .get(&(bot.key.clone(), nonce.to_string()))
        {
            Some((message_id, content)) if content_hash(content) == expected_hash => {
                CardPostReconcile::Found(*message_id)
            }
            _ => CardPostReconcile::Ambiguous("no exact legacy POST receipt".into()),
        }
    }
}

async fn database(label: &str) -> (DispatchPostgresTestDb, PgPool) {
    let database = DispatchPostgresTestDb::try_create("adk_inline_tc", label)
        .await
        .expect("inline contract tests require an isolated PostgreSQL fixture; never skip");
    let pool = database.connect_and_migrate().await;
    (database, pool)
}

fn clients(provider: &str) -> CardDeliveryClients {
    CardDeliveryClients::new([
        CardBot::new(
            "notify",
            std::sync::Arc::new(serenity::Http::new("offline-notify")),
        ),
        CardBot::new(
            provider_bot_key(provider),
            std::sync::Arc::new(serenity::Http::new("offline-provider")),
        ),
    ])
}

fn event(
    task: &str,
    source: Option<&str>,
    status: Option<&str>,
    summary: &str,
    result: &str,
) -> TaskCardEvent {
    let status = status
        .map(|value| format!("<status>{value}</status>"))
        .unwrap_or_default();
    TaskCardEvent::from_task_prompt_with_source_event_id(
        CHANNEL,
        "claude",
        "inline-contract-session",
        &format!(
            "<task-notification><task-id>{task}</task-id>{status}<summary>{summary}</summary><result>{result}</result></task-notification>"
        ),
        source,
    )
}

fn monitor(source: Option<&str>, number: usize) -> TaskCardEvent {
    event(
        "monitor",
        source,
        None,
        "Monitor event: \"coord events\"",
        &format!("event-{number} *preview*\nsecond line\nthird line"),
    )
}

fn assert_inline(call: &WireCall, provider: &str, icon: &str, text: &str) {
    assert_eq!(call.bot, provider_bot_key(provider));
    assert!(call.message_id > 0);
    assert!(call.nonce.len() <= 25);
    assert!(call.content.starts_with(&format!("-# {icon} ")), "{call:?}");
    assert!(call.content.contains(text), "{call:?}");
    assert_eq!(call.content.lines().count(), 1, "{call:?}");
    assert!(!call.content.chars().any(char::is_control), "{call:?}");
    assert!(!call.content.contains(['\u{2028}', '\u{2029}']), "{call:?}");
    assert!(call.content.encode_utf16().count() <= 2_000, "{call:?}");
    assert!(!call.content.contains("```"), "{call:?}");
    assert!(!call.content.contains("**"), "{call:?}");
    assert!(!call.content.contains("-# task "), "{call:?}");
}

async fn deliver(pool: &PgPool, transport: &Transport, event: &TaskCardEvent) -> CardEnsureOutcome {
    ensure_card(
        Some(pool),
        &clients(&event.scope.provider),
        transport,
        event,
        EnsureIntent::Observation,
    )
    .await
    .expect("the real task-card entrypoint must deliver the inline contract")
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    bot: String,
    nonce: String,
    message: Option<i64>,
    revision: i32,
    count: i64,
    content: String,
    hash: String,
}

async fn snapshot(pool: &PgPool, event: &TaskCardEvent) -> Snapshot {
    let row = sqlx::query(
        "SELECT bot_key, discord_nonce, discord_message_id, revision, update_count,
                rendered_content, content_hash
         FROM task_notification_card_state
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
    )
    .bind(i64::try_from(event.scope.channel_id).unwrap())
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .fetch_one(pool)
    .await
    .expect("load the exact PG card state exercised by ensure_card");
    Snapshot {
        bot: row.get("bot_key"),
        nonce: row.get("discord_nonce"),
        message: row.get("discord_message_id"),
        revision: row.get("revision"),
        count: row.get("update_count"),
        content: row.get("rendered_content"),
        hash: row.get("content_hash"),
    }
}

async fn seed_legacy(
    pool: &PgPool,
    event: &TaskCardEvent,
    posted: bool,
    started_seconds_ago: Option<i64>,
) -> ClaimedCard {
    let claim = store::claim_card(
        Some(pool),
        &event.scope,
        "notify",
        LEGACY_CONTENT,
        &content_hash(LEGACY_CONTENT),
        StoreIntent::Observation,
    )
    .await
    .expect("seed a raw pre-inline notify row through the actual store");
    let CardClaim::Owned(claim) = claim else {
        panic!("the fresh legacy fixture must own its initial row: {claim:?}");
    };
    assert_eq!(claim.bot_key, "notify");
    assert_eq!(claim.rendered_content, LEGACY_CONTENT);
    if posted {
        store::mark_posted(
            Some(pool),
            &claim,
            LEGACY_MESSAGE,
            LEGACY_CONTENT,
            &content_hash(LEGACY_CONTENT),
        )
        .await
        .expect("seed a confirmed legacy receipt and terminal ledger");
    } else {
        if started_seconds_ago.is_some() {
            store::begin_card_post(Some(pool), &claim)
                .await
                .expect("cross the real durable POST boundary");
        }
        let changed = sqlx::query(
            "UPDATE task_notification_card_state
             SET lease_owner = NULL, lease_expires_at = NULL,
                 post_started_at = CASE WHEN $5::BIGINT IS NULL THEN NULL
                    ELSE NOW() - make_interval(secs => $5::DOUBLE PRECISION) END
             WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4",
        )
        .bind(i64::try_from(event.scope.channel_id).unwrap())
        .bind(&event.scope.provider)
        .bind(&event.scope.session_key)
        .bind(&event.scope.event_key)
        .bind(started_seconds_ago)
        .execute(pool)
        .await
        .expect("release only the lane-owned legacy fixture lease")
        .rows_affected();
        assert_eq!(changed, 1);
    }
    claim
}

#[tokio::test]
async fn monitor_statusless_event_is_one_provider_subtext_pg() {
    let (database, pool) = database("statusless Monitor inline delivery").await;
    let transport = Transport::default();
    let event = monitor(Some("monitor-one"), 1);
    let result = deliver(&pool, &transport, &event).await;
    assert_eq!(result.bot_key, "provider:claude");
    assert_eq!(transport.counts(), (1, 0));
    let call = transport.latest();
    assert_inline(&call, "claude", "📡", "coord events");
    assert!(call.content.contains("event-1"));
    assert!(call.content.contains("(+2줄)"));
    assert!(call.content.ends_with(" · 1회"));
    assert!(!call.content.contains("second line"));
    assert!(!call.content.contains("third line"));
    database.drop().await;
}

#[tokio::test]
async fn monitor_replay_is_noop_and_next_event_edits_latest_pg() {
    let (database, pool) = database("Monitor exact replay and latest event edit").await;
    let transport = Transport::default();
    let first = monitor(Some("monitor-source-one"), 1);
    let receipt = deliver(&pool, &transport, &first).await;
    let before = snapshot(&pool, &first).await;
    let replay = deliver(&pool, &transport, &first).await;
    assert_eq!(receipt.message_id, replay.message_id);
    assert_eq!(transport.counts(), (1, 0));
    assert_eq!(snapshot(&pool, &first).await, before);

    let second = monitor(Some("monitor-source-two"), 2);
    let latest = deliver(&pool, &transport, &second).await;
    assert_eq!(latest.message_id, receipt.message_id);
    assert_eq!(transport.counts(), (1, 1));
    let call = transport.latest();
    assert_inline(&call, "claude", "📡", "event-2");
    assert!(call.content.contains("(+2줄)"));
    assert!(call.content.ends_with(" · 2회"));
    assert!(!call.content.contains("event-1"));
    let after = snapshot(&pool, &second).await;
    assert_eq!(after.count, 2);
    assert_eq!(after.revision, before.revision);
    deliver(&pool, &transport, &first).await;
    assert_eq!(
        transport.counts(),
        (1, 1),
        "late replay must not revert the latest event"
    );
    assert_eq!(snapshot(&pool, &second).await, after);
    database.drop().await;
}

#[tokio::test]
async fn monitor_without_source_replays_once_after_pool_restart_pg() {
    let (database, pool) = database("source-free Monitor durable payload replay").await;
    let first = monitor(None, 1);
    let receipt = deliver(&pool, &Transport::default(), &first).await;
    let before = snapshot(&pool, &first).await;
    pool.close().await;
    let pool = database.connect_and_migrate().await;
    let transport = Transport::default();
    let replay = deliver(&pool, &transport, &first).await;
    assert_eq!(receipt.message_id, replay.message_id);
    assert_eq!(transport.counts(), (0, 0));
    assert_eq!(snapshot(&pool, &first).await, before);
    let sourced = monitor(Some("source-discovered-after-restart"), 1);
    let aliased = deliver(&pool, &transport, &sourced).await;
    assert_eq!(aliased.message_id, receipt.message_id);
    assert_eq!(transport.counts(), (0, 0));
    assert_eq!(snapshot(&pool, &first).await, before);
    let second = monitor(None, 2);
    let next = deliver(&pool, &transport, &second).await;
    assert_eq!(next.message_id, receipt.message_id);
    assert_eq!(transport.counts(), (0, 1));
    assert_inline(&transport.latest(), "claude", "📡", "event-2");
    assert_eq!(snapshot(&pool, &second).await.count, 2);
    database.drop().await;
}

#[tokio::test]
async fn subagent_envelope_is_one_provider_subtext_pg() {
    let (database, pool) = database("Subagent envelope inline delivery").await;
    let raw = serde_json::json!({"task_id": "subagent", "status": {
        "completed": "Reviewed *module* [one].\nsecond private detail\nthird detail"
    }});
    let event = TaskCardEvent::from_subagent_prompt_with_source_event_id(
        CHANNEL,
        "codex",
        "inline-contract-session",
        &format!("<subagent_notification>{raw}</subagent_notification>"),
        Some("subagent-source"),
    );
    let transport = Transport::default();
    let outcome = deliver(&pool, &transport, &event).await;
    assert_eq!(outcome.bot_key, "provider:codex");
    assert_eq!(transport.counts(), (1, 0));
    assert_inline(&transport.latest(), "codex", "✅", "Reviewed");
    database.drop().await;
}

#[tokio::test]
async fn workflow_completed_is_one_provider_subtext_pg() {
    let (database, pool) = database("workflow inline delivery").await;
    let event = event(
        "workflow",
        Some("workflow-source"),
        Some("completed"),
        "Dynamic workflow \"review **module**\" completed\nsecond summary line",
        "workflow result\nsecond result line",
    );
    let transport = Transport::default();
    deliver(&pool, &transport, &event).await;
    assert_eq!(transport.counts(), (1, 0));
    assert_inline(&transport.latest(), "claude", "✅", "Dynamic workflow");
    database.drop().await;
}

#[tokio::test]
async fn failed_and_stopped_tasks_are_one_provider_subtext_pg() {
    let (database, pool) = database("failed and stopped task inline delivery").await;
    for (status, icon) in [("failed", "❌"), ("stopped", "🛑")] {
        let event = event(
            status,
            Some(status),
            Some(status),
            &format!("Agent \"{status} **module**\"\nsecond summary line"),
            "result first line\nresult second line",
        );
        let transport = Transport::default();
        deliver(&pool, &transport, &event).await;
        assert_eq!(transport.counts(), (1, 0));
        assert_inline(&transport.latest(), "claude", icon, status);
    }
    database.drop().await;
}

#[tokio::test]
async fn unavailable_provider_never_falls_back_to_notify_pg() {
    let (database, pool) = database("provider-required task delivery").await;
    let clients = CardDeliveryClients::new([CardBot::new(
        "notify",
        std::sync::Arc::new(serenity::Http::new("offline-notify")),
    )]);
    let transport = Transport::default();
    let result = ensure_card(
        Some(&pool),
        &clients,
        &transport,
        &monitor(Some("missing-provider"), 1),
        EnsureIntent::Observation,
    )
    .await;
    assert!(
        result.is_err(),
        "missing provider must keep the delivery retryable"
    );
    assert_eq!(transport.counts(), (0, 0));
    database.drop().await;
}

#[tokio::test]
async fn concurrent_monitor_observation_posts_once_and_counts_once_pg() {
    let (database, pool) = database("concurrent Monitor observation dedup").await;
    let transport = Transport::default();
    let event = monitor(Some("concurrent-monitor"), 1);
    let (left, right) = tokio::join!(
        deliver(&pool, &transport, &event),
        deliver(&pool, &transport, &event),
    );
    assert_eq!(left.message_id, right.message_id);
    assert_eq!(transport.counts(), (1, 0));
    assert_eq!(snapshot(&pool, &event).await.count, 1);
    assert_inline(&transport.latest(), "claude", "📡", "event-1");
    database.drop().await;
}

#[tokio::test]
async fn legacy_notify_posted_exact_replay_has_no_network_effect_pg() {
    let (database, pool) = database("legacy notify confirmed exact replay").await;
    let event = monitor(Some("legacy-exact-source"), 1);
    seed_legacy(&pool, &event, true, None).await;
    let before = snapshot(&pool, &event).await;
    let transport = Transport::default();
    let receipt = deliver(&pool, &transport, &event).await;
    assert_eq!(receipt.message_id, LEGACY_MESSAGE);
    assert_eq!(receipt.bot_key, "notify");
    assert_eq!(transport.counts(), (0, 0));
    assert_eq!(snapshot(&pool, &event).await, before);
    database.drop().await;
}

#[tokio::test]
async fn legacy_notify_new_event_posts_provider_revision_and_preserves_ledger_pg() {
    let (database, pool) = database("legacy notify latest event provider migration").await;
    let old = monitor(Some("legacy-before-source"), 1);
    let claim = seed_legacy(&pool, &old, true, None).await;
    let next = monitor(Some("legacy-after-source"), 2);
    let transport = Transport::default();
    let receipt = deliver(&pool, &transport, &next).await;
    assert_ne!(receipt.message_id, LEGACY_MESSAGE);
    assert_eq!(receipt.bot_key, "provider:claude");
    assert_eq!(transport.counts(), (1, 0));
    assert_inline(&transport.latest(), "claude", "📡", "event-2");
    let current = snapshot(&pool, &next).await;
    assert_eq!(current.revision, 2);
    assert_ne!(current.nonce, claim.discord_nonce);
    let ledger = sqlx::query(
        "SELECT bot_key, content_hash FROM task_notification_terminal_delivery
         WHERE channel_id = $1 AND provider = 'claude' AND discord_message_id = $2",
    )
    .bind(i64::try_from(CHANNEL).unwrap())
    .bind(i64::try_from(LEGACY_MESSAGE).unwrap())
    .fetch_one(&pool)
    .await
    .expect("the old notify receipt must remain in the durable terminal ledger");
    assert_eq!(ledger.get::<String, _>("bot_key"), "notify");
    assert_eq!(
        ledger.get::<String, _>("content_hash"),
        content_hash(LEGACY_CONTENT)
    );

    let third = monitor(Some("legacy-third-source"), 3);
    let updated = deliver(&pool, &transport, &third).await;
    assert_eq!(updated.message_id, receipt.message_id);
    assert_eq!(transport.counts(), (1, 1));
    assert_inline(&transport.latest(), "claude", "📡", "event-3");
    let before_replay = snapshot(&pool, &third).await;
    let replay = deliver(&pool, &transport, &old).await;
    assert_eq!(replay.message_id, LEGACY_MESSAGE);
    assert_eq!(transport.counts(), (1, 1));
    assert_eq!(snapshot(&pool, &third).await, before_replay);
    database.drop().await;
}

#[tokio::test]
async fn legacy_notify_never_posted_repins_before_its_first_send_pg() {
    let (database, pool) = database("legacy notify never-posted migration").await;
    let event = monitor(Some("legacy-unsent-source"), 1);
    seed_legacy(&pool, &event, false, None).await;
    let transport = Transport::default();
    let receipt = deliver(&pool, &transport, &event).await;
    assert_eq!(receipt.bot_key, "provider:claude");
    assert_eq!(transport.counts(), (1, 0));
    assert_inline(&transport.latest(), "claude", "📡", "event-1");
    let after = snapshot(&pool, &event).await;
    assert_eq!(after.bot, "provider:claude");
    assert_eq!(after.count, 1);
    assert_ne!(after.content, LEGACY_CONTENT);
    database.drop().await;
}

#[tokio::test]
async fn legacy_notify_started_post_preserves_bytes_nonce_without_notify_post_pg() {
    let (database, pool) = database("legacy notify POST boundary protection").await;
    for (task, age) in [("young-started", 0), ("aged-started", 300)] {
        let event = event(
            task,
            Some(task),
            None,
            "Monitor event: \"coord events\"",
            "latest event\nsecond line",
        );
        let claim = seed_legacy(&pool, &event, false, Some(age)).await;
        let before = snapshot(&pool, &event).await;
        let transport = Transport::default();
        transport.seed_committed(&claim);
        let result = ensure_card(
            Some(&pool),
            &clients("claude"),
            &transport,
            &event,
            EnsureIntent::Observation,
        )
        .await;
        match result {
            Ok(receipt) => {
                assert_eq!(receipt.message_id, LEGACY_MESSAGE);
                assert_eq!(receipt.bot_key, "notify");
                assert!(!transport.reconciles.lock().unwrap().is_empty());
            }
            Err(
                CardEnsureError::Ambiguous(_)
                | CardEnsureError::Permanent(_)
                | CardEnsureError::Transient(_),
            ) => {}
            other => panic!("legacy POST must reconcile its receipt or fail closed: {other:?}"),
        }
        assert_eq!(
            transport.counts(),
            (0, 0),
            "a crossed POST boundary may never repost as notify"
        );
        let after = snapshot(&pool, &event).await;
        assert_eq!(after.bot, before.bot);
        assert_eq!(after.nonce, before.nonce);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.count, before.count);
        assert_eq!(after.content, before.content);
        assert_eq!(after.hash, before.hash);
        for (bot, nonce, hash) in transport.reconciles.lock().unwrap().iter() {
            assert_eq!(bot, "notify");
            assert_eq!(nonce, &before.nonce);
            assert_eq!(hash, &before.hash);
        }
    }
    database.drop().await;
}
