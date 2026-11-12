use super::super::{OutboxEnqueueError, OutboxMessage, enqueue_outbox_pg_on_tx};
use crate::dispatch::test_support::DispatchPostgresTestDb;
use crate::services::discord::outbound::census_scope;
use crate::test_support::wait::{WAIT_CAP, await_within};
use serde_json::{Value, json};
use sqlx::PgPool;

const LEGACY_CHANNEL: &str = "72280731";
const CODEX_CHANNEL: &str = "72280732";
const NOTICE_REASON: &str = "lifecycle.session_resume_failed";

struct Isolation {
    _guards: [crate::config::TestEnvVarGuard; 2],
    _live: crate::config_live_reload::TestLiveGuard,
    _root: tempfile::TempDir,
}

impl Isolation {
    fn new(config: crate::config::Config) -> Self {
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.yaml");
        std::fs::write(&path, "server: {}\n").unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let guards = [
            set("AGENTDESK_ROOT_DIR", root.path()),
            set("AGENTDESK_CONFIG", &path),
        ];
        crate::config_live_reload::install(config);
        Self {
            _guards: guards,
            _live: live,
            _root: root,
        }
    }
}

/// `source_agent` makes an agent id collide with the notice source, which withholds the census.
fn config(default_provider: &str, source_agent: bool) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.onboarding.default_provider = Some(default_provider.to_string());
    if source_agent {
        config.agents.push(
            serde_json::from_value(json!({"id": "lifecycle_notifier", "name": "collision"}))
                .unwrap(),
        );
    }
    config
}

fn notice(channel_target: &'static str) -> OutboxMessage<'static> {
    OutboxMessage {
        target: channel_target,
        content: "♻️ resume failed",
        bot: "notify",
        source: "lifecycle_notifier",
        reason_code: Some(NOTICE_REASON),
        session_key: None,
    }
}

async fn database(label: &str) -> (DispatchPostgresTestDb, PgPool) {
    let db = DispatchPostgresTestDb::try_create("adk_outbox_plain", label)
        .await
        .expect("plain enqueue contracts require real isolated PostgreSQL; never skip");
    // Two table gates, the producer and the lock observer each hold a connection.
    let pool = db.connect_and_migrate_with_max_connections(5).await;
    (db, pool)
}

async fn await_lock_waiter(pool: &PgPool, relation: &str) {
    await_within(
        async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_locks
                                     WHERE NOT granted AND relation = $1::text::regclass)",
                )
                .bind(relation)
                .fetch_one(pool)
                .await
                .unwrap();
                if waiting {
                    return;
                }
                tokio::task::yield_now().await;
            }
        },
        WAIT_CAP,
    )
    .await
}

async fn lock_table(pool: &PgPool, relation: &str) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut gate = pool.begin().await.unwrap();
    sqlx::query(&format!("LOCK TABLE {relation} IN ACCESS EXCLUSIVE MODE"))
        .execute(&mut *gate)
        .await
        .unwrap();
    gate
}

/// A reload while the provider rows are pending reaches D; one after the INSERT reaches C.
#[tokio::test]
async fn plain_enqueue_reads_default_after_rows_and_census_after_insert_pg() {
    let _isolation = Isolation::new(config("claude", true));
    let (db, pool) = database("plain observation points").await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('plain-legacy', 'Plain', NULL, $1)",
    )
    .bind(LEGACY_CHANNEL)
    .execute(&pool)
    .await
    .unwrap();
    let agents_gate = lock_table(&pool, "agents").await;
    let outbox_gate = lock_table(&pool, "message_outbox").await;
    let producer_pool = pool.clone();
    let producer = tokio::spawn(async move {
        let mut tx = producer_pool.begin().await.unwrap();
        let id = enqueue_outbox_pg_on_tx(&mut tx, notice("channel:72280731"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        id
    });

    await_lock_waiter(&pool, "agents").await;
    crate::config_live_reload::install(config("codex", true));
    agents_gate.commit().await.unwrap();
    await_lock_waiter(&pool, "message_outbox").await;
    crate::config_live_reload::install(config("codex", false));
    outbox_gate.commit().await.unwrap();
    let id = await_within(producer, WAIT_CAP).await.unwrap();

    let (designation, census): (Option<Value>, Option<Value>) =
        sqlx::query_as("SELECT session_notice, body_census FROM message_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        designation.expect("eligible notice is designated")["provider_hint"],
        "codex",
        "the legacy-primary default is read after the provider rows arrive"
    );
    assert_eq!(
        census.as_ref().and_then(|c| c["notice_kind"].as_str()),
        Some("lifecycle_notifier"),
        "the census sees the config installed after the INSERT"
    );
    pool.close().await;
    db.drop().await;
}

/// D, INSERT and C run on the caller's transaction and C keeps the caller task's census tag.
#[tokio::test]
async fn plain_enqueue_stays_on_caller_tx_and_task_and_propagates_provider_errors_pg() {
    let _isolation = Isolation::new(config("claude", false));
    let (db, pool) = database("plain caller transaction").await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_cdx)
         VALUES ('plain-uncommitted', 'Plain', NULL, $1)",
    )
    .bind(CODEX_CHANNEL)
    .execute(&mut *tx)
    .await
    .unwrap();
    let designated = enqueue_outbox_pg_on_tx(&mut tx, notice("channel:72280732"))
        .await
        .unwrap();
    let tag = json!({"site": "plain-fixture", "binding_mode": "TUI"});
    let headless = census_scope(
        Some(tag.clone()),
        enqueue_outbox_pg_on_tx(
            &mut tx,
            OutboxMessage {
                target: "channel:72280733",
                content: "stored answer",
                bot: "claude",
                source: "headless_turn",
                reason_code: Some("headless.delivery"),
                session_key: None,
            },
        ),
    )
    .await
    .unwrap();
    let read = |id: i64| {
        sqlx::query_as::<_, (Option<Value>, Option<Value>)>(
            "SELECT session_notice, body_census FROM message_outbox WHERE id = $1",
        )
        .bind(id)
    };
    let (designation, _) = read(designated).fetch_one(&mut *tx).await.unwrap();
    assert_eq!(
        designation.expect("eligible notice is designated")["provider_hint"],
        "codex",
        "provider rows come from the caller's uncommitted transaction"
    );
    let (_, census) = read(headless).fetch_one(&mut *tx).await.unwrap();
    assert_eq!(
        census.expect("headless census is stored")["tag"],
        tag,
        "the census is captured in the producer task"
    );
    tx.rollback().await.unwrap();
    let durable: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(durable, 0, "INSERT and census belong to the rolled-back tx");

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE agents RENAME COLUMN discord_channel_cdx TO cdx_renamed")
        .execute(&mut *tx)
        .await
        .unwrap();
    let error = enqueue_outbox_pg_on_tx(&mut tx, notice("channel:72280732"))
        .await
        .expect_err("a failed provider query fails the enqueue");
    let OutboxEnqueueError::Database(error) = error else {
        panic!("provider query failure must surface as a database error: {error:?}");
    };
    assert_eq!(
        error
            .as_database_error()
            .and_then(|db| db.code())
            .as_deref(),
        Some("42703")
    );
    tx.rollback().await.unwrap();
    let durable: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(durable, 0);
    pool.close().await;
    db.drop().await;
}
