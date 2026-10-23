use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use poise::serenity_prelude as serenity;
use sqlx::Row;

use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;

#[path = "footer_surface_tests.rs"]
mod footer_surface_tests;
#[path = "inline_contract_tests/replacement_receipt_tests.rs"]
mod replacement_receipt_tests;
#[path = "inline_contract_tests/retention_tests.rs"]
mod retention_tests;

#[tokio::test]
async fn reconciliation_guard_only_accepts_started_posts_pg() {
    let (database, pool) = database("reconciliation guard start boundary").await;
    let unstarted = monitor(Some("guard-never-started"), 1);
    let claim = seed_old_card(&pool, &unstarted, "provider:claude", false, None).await;
    assert!(
        !inline_delivery::needs_reconciliation(&unstarted, &clients("claude"), &claim),
        "a never-started row must not enter legacy POST reconciliation"
    );
    let event = event(
        "guard-started",
        Some("guard-started"),
        None,
        "Monitor event: \"coord events\"",
        "latest event",
    );
    seed_old_card(&pool, &event, "notify", false, Some(0)).await;
    let claim = store::claim_card(
        Some(&pool),
        &event.scope,
        "notify",
        LEGACY_CONTENT,
        &content_hash(LEGACY_CONTENT),
        StoreIntent::Promotion,
    )
    .await
    .unwrap();
    let CardClaim::Owned(claim) = claim else {
        panic!("started row must be claimable")
    };
    assert!(
        inline_delivery::needs_reconciliation(&event, &clients("claude"), &claim),
        "a started legacy row must retain its reconciliation guard"
    );
    database.drop().await;
}

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
    fail_next_edit: AtomicBool,
    fail_edit_missing_once: AtomicBool,
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
        if self.fail_edit_missing_once.swap(false, Ordering::AcqRel) {
            return Err(TaskCardTransportError::ConfirmedMissing(
                "404 / Discord 10008".into(),
            ));
        }
        if self.fail_next_edit.swap(false, Ordering::AcqRel) {
            return Err(TaskCardTransportError::Transient("retry this PATCH".into()));
        }
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

fn assert_notify_body_card(call: &WireCall, snippets: &[&str]) {
    assert_eq!(call.bot, "notify");
    assert!(call.message_id > 0);
    assert!(call.content.lines().count() > 1, "{call:?}");
    for snippet in snippets {
        assert!(
            call.content.contains(snippet),
            "missing {snippet:?}: {call:?}"
        );
    }
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
    .expect("the real task-card entrypoint must deliver the selected surface contract")
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
    seed_old_card(pool, event, "notify", posted, started_seconds_ago).await
}

async fn seed_old_card(
    pool: &PgPool,
    event: &TaskCardEvent,
    bot: &str,
    posted: bool,
    started_seconds_ago: Option<i64>,
) -> ClaimedCard {
    let claim = store::claim_card(
        Some(pool),
        &event.scope,
        bot,
        LEGACY_CONTENT,
        &content_hash(LEGACY_CONTENT),
        StoreIntent::Observation,
    )
    .await
    .expect("seed a raw pre-inline row through the actual store");
    let CardClaim::Owned(claim) = claim else {
        panic!("the fresh legacy fixture must own its initial row: {claim:?}");
    };
    assert_eq!(claim.bot_key, bot);
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
    *claim
}

async fn seed_actual_legacy_render(pool: &PgPool, event: &TaskCardEvent, count: u64) -> String {
    let TaskCardPayload::Task(note) = &event.payload else {
        panic!("a legacy Monitor fixture must contain a parsed task notification");
    };
    let rendered = super::super::tui_task_card::format_task_notification_card(note, count);
    assert!(rendered.lines().count() > 1);
    let CardClaim::Owned(mut claim) = store::claim_card(
        Some(pool),
        &event.scope,
        "notify",
        &rendered,
        &content_hash(&rendered),
        StoreIntent::Observation,
    )
    .await
    .expect("seed the actual pre-inline renderer bytes") else {
        panic!("the isolated raw legacy fixture must own its first lease");
    };
    let changed = sqlx::query(
        "UPDATE task_notification_card_state SET update_count = $5
         WHERE channel_id = $1 AND provider = $2 AND session_key = $3 AND event_key = $4
           AND lease_owner = $6 AND delivery_state = 'posting'",
    )
    .bind(i64::try_from(event.scope.channel_id).unwrap())
    .bind(&event.scope.provider)
    .bind(&event.scope.session_key)
    .bind(&event.scope.event_key)
    .bind(i64::try_from(count).unwrap())
    .bind(&claim.lease_owner)
    .execute(pool)
    .await
    .expect("seed the historical renderer count")
    .rows_affected();
    assert_eq!(changed, 1);
    claim.update_count = count;
    store::mark_posted(
        Some(pool),
        &claim,
        LEGACY_MESSAGE,
        &rendered,
        &content_hash(&rendered),
    )
    .await
    .expect("persist the real old-renderer notify receipt");
    rendered
}

async fn assert_started_post_immutable(pool: &PgPool, event: &TaskCardEvent, bot: &str, age: i64) {
    let claim = seed_old_card(pool, event, bot, false, Some(age)).await;
    let before = snapshot(pool, event).await;
    let transport = Transport::default();
    transport.seed_committed(&claim);
    let result = ensure_card(
        Some(pool),
        &clients("claude"),
        &transport,
        event,
        EnsureIntent::Observation,
    )
    .await;
    match result {
        Ok(receipt) => {
            assert_eq!(receipt.message_id, LEGACY_MESSAGE);
            assert_eq!(receipt.bot_key, bot);
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
        "a crossed POST boundary cannot be replayed as a new POST"
    );
    let after = snapshot(pool, event).await;
    assert_eq!(after.bot, before.bot);
    assert_eq!(after.nonce, before.nonce);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.count, before.count);
    assert_eq!(after.content, before.content);
    assert_eq!(after.hash, before.hash);
    for (actual_bot, nonce, hash) in transport.reconciles.lock().unwrap().iter() {
        assert_eq!(actual_bot, bot);
        assert_eq!(nonce, &before.nonce);
        assert_eq!(hash, &before.hash);
    }
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
async fn monitor_wire_text_is_bounded_and_strips_control_separators_pg() {
    let (database, pool) = database("Monitor inline wire safety and Discord bound").await;
    for (task, summary) in [
        (
            "monitor-controls",
            "Monitor event: \"coord\u{2028} events\"".to_string(),
        ),
        (
            "monitor-long",
            format!("Monitor event: \"coord {}\"", "🦀".repeat(3_000)),
        ),
    ] {
        let event = event(
            task,
            Some(task),
            None,
            &summary,
            "safe *preview*\u{2028}next\u{2029}tail\u{000b}done\u{0000}\r\nsecond line",
        );
        let transport = Transport::default();
        deliver(&pool, &transport, &event).await;
        assert_eq!(transport.counts(), (1, 0));
        assert_inline(&transport.latest(), "claude", "📡", "coord");
    }
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
    let separate_event = monitor(Some("different-source-same-payload"), 1);
    let separate = deliver(&pool, &transport, &separate_event).await;
    assert_eq!(separate.message_id, receipt.message_id);
    assert_eq!(transport.counts(), (0, 1));
    assert_eq!(snapshot(&pool, &first).await.count, 2);
    let second = monitor(None, 2);
    let next = deliver(&pool, &transport, &second).await;
    assert_eq!(next.message_id, receipt.message_id);
    assert_eq!(transport.counts(), (0, 2));
    assert_inline(&transport.latest(), "claude", "📡", "event-2");
    assert_eq!(snapshot(&pool, &second).await.count, 3);
    database.drop().await;
}

#[tokio::test]
async fn subagent_notify_card_preserves_body_pg() {
    let (database, pool) = database("Subagent envelope retains its notify body card").await;
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
    assert_eq!(outcome.bot_key, "notify");
    assert_eq!(transport.counts(), (1, 0));
    assert_notify_body_card(
        &transport.latest(),
        &["Reviewed", "second private detail", "third detail"],
    );
    database.drop().await;
}

#[tokio::test]
async fn workflow_notify_card_preserves_body_pg() {
    let (database, pool) = database("workflow retains its notify body card").await;
    let event = event(
        "workflow",
        Some("workflow-source"),
        Some("completed"),
        "Dynamic workflow \"review **module**\" completed\nsecond summary line",
        "workflow result\nsecond result line",
    );
    let transport = Transport::default();
    let outcome = deliver(&pool, &transport, &event).await;
    assert_eq!(outcome.bot_key, "notify");
    assert_eq!(transport.counts(), (1, 0));
    assert_notify_body_card(
        &transport.latest(),
        &["Dynamic workflow", "workflow result", "second result line"],
    );
    database.drop().await;
}

#[tokio::test]
async fn failed_and_stopped_notify_cards_preserve_body_pg() {
    let (database, pool) = database("failed and stopped tasks retain notify body cards").await;
    for status in ["failed", "stopped"] {
        let event = event(
            status,
            Some(status),
            Some(status),
            &format!("Agent \"{status} **module**\"\nsecond summary line"),
            "result first line\nresult second line",
        );
        let transport = Transport::default();
        let outcome = deliver(&pool, &transport, &event).await;
        assert_eq!(outcome.bot_key, "notify");
        assert_eq!(transport.counts(), (1, 0));
        assert_notify_body_card(
            &transport.latest(),
            &[status, "result first line", "result second line"],
        );
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
async fn monitor_failed_edit_retry_counts_the_event_once_pg() {
    let (database, pool) = database("Monitor PATCH retry preserves one observation count").await;
    let transport = Transport::default();
    let first = monitor(Some("edit-retry-first"), 1);
    let receipt = deliver(&pool, &transport, &first).await;
    let next = monitor(Some("edit-retry-next"), 2);
    transport.fail_next_edit.store(true, Ordering::Release);
    let failed = ensure_card(
        Some(&pool),
        &clients("claude"),
        &transport,
        &next,
        EnsureIntent::Observation,
    )
    .await;
    assert!(
        matches!(failed, Err(CardEnsureError::Transient(_))),
        "{failed:?}"
    );
    assert_eq!(transport.counts(), (1, 1));
    assert_eq!(
        snapshot(&pool, &next).await.count,
        1,
        "failed PATCH cannot commit an observation count"
    );
    let attempted_content = transport.latest().content;
    assert!(attempted_content.ends_with(" · 2회"));

    let retried = deliver(&pool, &transport, &next).await;
    assert_eq!(retried.message_id, receipt.message_id);
    assert_eq!(
        transport.counts(),
        (1, 2),
        "a failed PATCH must actually retry"
    );
    assert_eq!(transport.latest().content, attempted_content);
    assert_eq!(snapshot(&pool, &next).await.count, 2);
    let after = snapshot(&pool, &next).await;
    deliver(&pool, &transport, &next).await;
    assert_eq!(transport.counts(), (1, 2));
    assert_eq!(snapshot(&pool, &next).await, after);
    database.drop().await;
}

#[tokio::test]
async fn short_subagent_without_result_is_provider_subtext_pg() {
    let (database, pool) = database("body-free subagent summary uses provider subtext").await;
    let event = event(
        "short-subagent",
        Some("short-subagent-source"),
        Some("completed"),
        "Agent \"short worker\" completed",
        "",
    );
    let transport = Transport::default();
    let receipt = deliver(&pool, &transport, &event).await;
    assert_eq!(receipt.bot_key, "provider:claude");
    assert_eq!(transport.counts(), (1, 0));
    assert_inline(&transport.latest(), "claude", "✅", "short worker");
    database.drop().await;
}

#[tokio::test]
async fn summary_then_body_completion_preserves_both_delivery_receipts_pg() {
    let (database, pool) = database("task summary to body completion surface transition").await;
    let short = event(
        "surface-transition",
        Some("short-summary-source"),
        Some("completed"),
        "Agent \"transition worker\" completed",
        "",
    );
    let transport = Transport::default();
    let original = deliver(&pool, &transport, &short).await;
    assert_inline(&transport.latest(), "claude", "✅", "transition worker");
    let complete = event(
        "surface-transition",
        Some("body-completion-source"),
        Some("completed"),
        "Agent \"transition worker\" completed",
        "completion body first\ncompletion body second",
    );
    let completion = deliver(&pool, &transport, &complete).await;
    assert_ne!(completion.message_id, original.message_id);
    assert_eq!(completion.bot_key, "notify");
    assert_eq!(transport.counts(), (2, 0));
    assert_notify_body_card(
        &transport.latest(),
        &["completion body first", "completion body second"],
    );
    let after = snapshot(&pool, &complete).await;
    let short_replay = deliver(&pool, &transport, &short).await;
    assert_eq!(short_replay.message_id, original.message_id);
    assert_eq!(short_replay.bot_key, "provider:claude");
    let completed_replay = deliver(&pool, &transport, &complete).await;
    assert_eq!(completed_replay.message_id, completion.message_id);
    assert_eq!(completed_replay.bot_key, "notify");
    assert_eq!(transport.counts(), (2, 0));
    assert_eq!(snapshot(&pool, &complete).await, after);
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
        assert_started_post_immutable(&pool, &event, "notify", age).await;
    }
    database.drop().await;
}

#[tokio::test]
async fn provider_pinned_never_posted_old_bytes_are_canonicalized_pg() {
    let (database, pool) = database("provider-pinned unsent old bytes are safe to replace").await;
    let event = monitor(Some("provider-old-unsent-source"), 1);
    seed_old_card(&pool, &event, "provider:claude", false, None).await;
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
async fn provider_pinned_started_old_bytes_and_nonce_are_immutable_pg() {
    let (database, pool) = database("provider-pinned old bytes crossed the POST boundary").await;
    for (task, age) in [("provider-young", 0), ("provider-aged", 300)] {
        let event = event(
            task,
            Some(task),
            None,
            "Monitor event: \"coord events\"",
            "provider event\nsecond line",
        );
        assert_started_post_immutable(&pool, &event, "provider:claude", age).await;
    }
    database.drop().await;
}

#[tokio::test]
async fn monitor_missing_edit_replaces_once_and_replays_after_restart_pg() {
    let (database, pool) = database("Monitor missing PATCH replacement durable receipt").await;
    let transport = Transport::default();
    let first = monitor(Some("missing-edit-source-one"), 1);
    let original = deliver(&pool, &transport, &first).await;
    let old_state = snapshot(&pool, &first).await;
    let second = monitor(Some("missing-edit-source-two"), 2);
    transport
        .fail_edit_missing_once
        .store(true, Ordering::Release);
    let replacement = deliver(&pool, &transport, &second).await;
    assert_eq!(replacement.disposition, CardDisposition::Replaced);
    assert_eq!(replacement.bot_key, "provider:claude");
    assert_ne!(replacement.message_id, original.message_id);
    assert_eq!(transport.counts(), (2, 1));
    let posted = transport.posts.lock().unwrap().last().cloned().unwrap();
    assert_inline(&posted, "claude", "📡", "event-2");
    assert!(posted.content.ends_with(" · 2회"));
    assert_ne!(posted.nonce, old_state.nonce);
    let state = snapshot(&pool, &second).await;
    assert_eq!(
        state.message,
        Some(i64::try_from(replacement.message_id).unwrap())
    );
    assert_eq!(state.revision, old_state.revision + 1);
    assert_eq!(state.count, 2);
    assert_eq!(state.content, posted.content);
    assert_eq!(state.hash, content_hash(&posted.content));

    pool.close().await;
    let pool = database.connect_and_migrate().await;
    let replay_transport = Transport::default();
    let replay = deliver(&pool, &replay_transport, &second).await;
    assert_eq!(replay.message_id, replacement.message_id);
    assert_eq!(replay.bot_key, "provider:claude");
    assert_eq!(replay_transport.counts(), (0, 0));
    assert_eq!(snapshot(&pool, &second).await, state);
    database.drop().await;
}

#[tokio::test]
async fn public_missing_card_replacement_canonicalizes_legacy_and_ignores_stale_ids_pg() {
    let (database, pool) =
        database("public task-card replacement uses canonical Monitor surface").await;
    for (task, bot) in [
        ("public-notify", "notify"),
        ("public-provider", "provider:claude"),
    ] {
        let event = event(
            task,
            Some(task),
            None,
            "Monitor event: \"coord events\"",
            "replacement event\nsecond line\nthird line",
        );
        seed_old_card(&pool, &event, bot, true, None).await;
        let before = snapshot(&pool, &event).await;
        let transport = Transport::default();
        let replacement = replace_confirmed_missing_card(
            Some(&pool),
            &clients("claude"),
            &transport,
            &event,
            LEGACY_MESSAGE,
        )
        .await
        .expect(
            "the actual public replacement entrypoint must post the canonical provider surface",
        );
        assert_eq!(replacement.bot_key, "provider:claude");
        assert_ne!(replacement.message_id, LEGACY_MESSAGE);
        assert_eq!(transport.counts(), (1, 0));
        let call = transport.latest();
        assert_inline(&call, "claude", "📡", "replacement event");
        assert!(call.content.contains("(+2줄)"));
        assert!(call.content.ends_with(" · 1회"));
        let state = snapshot(&pool, &event).await;
        assert_eq!(state.revision, before.revision + 1);
        assert_eq!(state.count, before.count);
        assert_ne!(state.nonce, before.nonce);
        assert_eq!(state.content, call.content);
        for stale_id in [LEGACY_MESSAGE, LEGACY_MESSAGE - 1] {
            let existing = replace_confirmed_missing_card(
                Some(&pool),
                &clients("claude"),
                &transport,
                &event,
                stale_id,
            )
            .await
            .expect("a stale missing-card snapshot must reuse the current replacement receipt");
            assert_eq!(existing.message_id, replacement.message_id);
            assert_eq!(existing.bot_key, "provider:claude");
            assert_eq!(transport.counts(), (1, 0));
            assert_eq!(snapshot(&pool, &event).await, state);
        }
    }
    database.drop().await;
}

#[tokio::test]
async fn monitor_source_free_identity_includes_nonpreview_details_pg() {
    let (database, pool) = database("source-free Monitor full payload identity").await;
    let first = event(
        "nonpreview-detail",
        None,
        None,
        "Monitor event: \"coord events\"",
        "shared preview\nfirst detail A\nsecond detail A",
    );
    let second = event(
        "nonpreview-detail",
        None,
        None,
        "Monitor event: \"coord events\"",
        "shared preview\nfirst detail B\nsecond detail B",
    );
    let transport = Transport::default();
    let receipt = deliver(&pool, &transport, &first).await;
    let first_content = transport.latest().content;
    let edited = deliver(&pool, &transport, &second).await;
    assert_eq!(edited.message_id, receipt.message_id);
    assert_eq!(transport.counts(), (1, 1));
    let second_content = transport.latest().content;
    let first_preview = first_content
        .strip_suffix(" · 1회")
        .expect("the first observation count must be visible");
    let second_preview = second_content
        .strip_suffix(" · 2회")
        .expect("the second observation count must be visible");
    assert_eq!(first_preview, second_preview,);
    assert!(second_content.contains("shared preview"));
    assert!(second_content.contains("(+2줄)"));
    assert!(!second_content.contains("detail A"));
    assert!(!second_content.contains("detail B"));
    let after = snapshot(&pool, &second).await;
    assert_eq!(after.count, 2);
    let replay = deliver(&pool, &transport, &first).await;
    assert_eq!(replay.message_id, receipt.message_id);
    assert_eq!(transport.counts(), (1, 1));
    assert_eq!(snapshot(&pool, &second).await, after);
    database.drop().await;
}

#[tokio::test]
async fn legacy_source_free_old_renderer_replay_survives_new_provider_revision_pg() {
    let (database, pool) = database("source-free old-renderer Monitor late replay").await;
    let first = event(
        "legacy-render-count",
        None,
        None,
        "Monitor event: \"coord events\"",
        "legacy first preview\nfirst detail A\nsecond detail A",
    );
    let old_content = seed_actual_legacy_render(&pool, &first, 7).await;
    let old_state = snapshot(&pool, &first).await;
    assert_eq!(old_state.bot, "notify");
    assert_eq!(old_state.count, 7);
    assert_eq!(old_state.content, old_content);
    let next = event(
        "legacy-render-count",
        None,
        None,
        "Monitor event: \"coord events\"",
        "new provider preview\nfirst detail B\nsecond detail B",
    );
    let transport = Transport::default();
    let latest = deliver(&pool, &transport, &next).await;
    assert_ne!(latest.message_id, LEGACY_MESSAGE);
    assert_eq!(latest.bot_key, "provider:claude");
    assert_eq!(transport.counts(), (1, 0));
    assert_inline(&transport.latest(), "claude", "📡", "new provider preview");
    let after = snapshot(&pool, &next).await;
    assert_eq!(after.revision, old_state.revision + 1);
    assert_eq!(after.count, 8);
    let replay = deliver(&pool, &transport, &first).await;
    assert_eq!(replay.message_id, LEGACY_MESSAGE);
    assert_eq!(replay.bot_key, "notify");
    assert_eq!(transport.counts(), (1, 0));
    assert_eq!(snapshot(&pool, &next).await, after);
    pool.close().await;
    let pool = database.connect_and_migrate().await;
    let replay_transport = Transport::default();
    let restarted = deliver(&pool, &replay_transport, &first).await;
    assert_eq!(restarted.message_id, LEGACY_MESSAGE);
    assert_eq!(restarted.bot_key, "notify");
    assert_eq!(replay_transport.counts(), (0, 0));
    assert_eq!(snapshot(&pool, &next).await, after);
    database.drop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn monitor_auto_turn_stream_context_uses_provider_inline_once_pg() {
    let (database, pool) = database("MonitorAutoTurn actual stream context entrypoint").await;
    let context = |summary| {
        TaskNotificationContext::from_stream_json(
            &serde_json::json!({
                "type": "system",
                "subtype": "task_notification",
                "task_notification_kind": "monitor_auto_turn",
                "task_id": "monitor-auto-turn",
                "summary": summary,
            }),
            &StreamLineState::new(),
        )
        .expect("the actual stream entrypoint must recognize a status-free MonitorAutoTurn")
    };
    let prior_context = context("before stream promotion");
    let context = context("heartbeat event");
    assert_eq!(
        context.routing_kind(),
        TaskNotificationKind::MonitorAutoTurn
    );
    let prior_event = prior_context.to_event(CHANNEL, "claude", "inline-contract-session");
    let event = context.to_event(CHANNEL, "claude", "inline-contract-session");
    let transport = Transport::default();
    let first = deliver(&pool, &transport, &prior_event).await;
    let before = snapshot(&pool, &event).await;
    assert_eq!(before.count, 1);
    assert_inline(
        &transport.latest(),
        "claude",
        "📡",
        "before stream promotion",
    );

    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let promoted = crate::services::discord::session_relay_sink::task_notification_context::confirm_task_context_card(
        Some(&shared),
        &clients("claude"),
        &transport,
        &shared.ui.placeholder_live_events,
        CHANNEL,
        "claude",
        "inline-contract-session",
        Some(&context),
    )
    .await
    .expect("the actual stream gate must confirm the current Monitor card")
    .expect("the Monitor context must resolve a card");
    assert_eq!(promoted.message_id, first.message_id);
    assert_eq!(promoted.bot_key, "provider:claude");
    assert_eq!(
        transport.counts(),
        (1, 0),
        "stream Promotion must not count or publish an unobserved Monitor event"
    );
    assert_eq!(
        snapshot(&pool, &event).await,
        before,
        "stream Promotion must preserve the last observed Monitor payload and count"
    );

    let observed = deliver(&pool, &transport, &event).await;
    assert_eq!(observed.message_id, first.message_id);
    assert_eq!(transport.counts(), (1, 1));
    assert_inline(&transport.latest(), "claude", "📡", "heartbeat event");
    let after = snapshot(&pool, &event).await;
    assert_eq!(
        after.count, 2,
        "only the subsequent TUI Observation increments the Monitor count"
    );
    assert!(after.content.ends_with(" · 2회"));
    let replay = deliver(&pool, &transport, &event).await;
    assert_eq!(replay.message_id, first.message_id);
    assert_eq!(transport.counts(), (1, 1));
    assert_eq!(snapshot(&pool, &event).await, after);
    drop(shared);
    database.drop().await;
}
