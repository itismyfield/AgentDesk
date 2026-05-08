use super::*;
use crate::server::routes::dispatches::discord_delivery;
use crate::server::routes::dispatches::discord_delivery::{
    DispatchNotifyDeliveryResult, DispatchTransport,
};
use sqlx::PgPool;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct TestLogWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for TestLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let log_buffer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(move || TestLogWriter {
            buffer: log_buffer.clone(),
        })
        .finish();

    let result = tracing::subscriber::with_default(subscriber, run);
    let captured = buffer.lock().unwrap().clone();
    (result, String::from_utf8_lossy(&captured).to_string())
}

fn test_db() -> crate::db::Db {
    crate::db::test_db()
}

struct TestPostgresDb {
    _lock: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl TestPostgresDb {
    async fn create() -> Self {
        let lock = crate::db::postgres::lock_test_lifecycle();
        let admin_url = postgres_admin_database_url();
        let database_name = format!(
            "agentdesk_dispatch_outbox_{}",
            uuid::Uuid::new_v4().simple()
        );
        let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
        crate::db::postgres::create_test_database(
            &admin_url,
            &database_name,
            "dispatch outbox tests",
        )
        .await
        .unwrap();

        Self {
            _lock: lock,
            admin_url,
            database_name,
            database_url,
        }
    }

    async fn connect_and_migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(
            &self.database_url,
            "dispatch outbox tests",
        )
        .await
        .unwrap()
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "dispatch outbox tests",
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn claim_pending_dispatch_outbox_batch_pg_records_owner_and_reclaims_stale() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, status, claimed_at, claim_owner
         ) VALUES ($1, 'status_reaction', 'processing', NOW() - INTERVAL '10 minutes', $2)",
    )
    .bind("dispatch-stale-outbox")
    .bind("old-node")
    .execute(&pool)
    .await
    .unwrap();

    let claimed = claim_pending_dispatch_outbox_batch_pg(&pool, "new-node").await;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].1, "dispatch-stale-outbox");

    let row: (String, String) = sqlx::query_as(
        "SELECT status, claim_owner
           FROM dispatch_outbox
          WHERE dispatch_id = $1",
    )
    .bind("dispatch-stale-outbox")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "processing");
    assert_eq!(row.1, "new-node");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn claim_pending_dispatch_outbox_batch_pg_filters_required_capabilities() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO worker_nodes (
            instance_id, hostname, process_id, role, effective_role, status,
            labels, capabilities, last_heartbeat_at, started_at, updated_at
         ) VALUES
            (
                'mac-mini-release', 'mac-mini', 100, 'auto', 'leader', 'online',
                $1, $2, NOW(), NOW(), NOW()
            ),
            (
                'mac-book-release', 'mac-book', 101, 'worker', 'worker', 'online',
                $3, $4, NOW(), NOW(), NOW()
            )",
    )
    .bind(serde_json::json!(["mac-mini"]))
    .bind(serde_json::json!({"providers": ["claude"]}))
    .bind(serde_json::json!(["mac-book"]))
    .bind(serde_json::json!({"providers": ["codex"]}))
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, status, required_capabilities
         ) VALUES ($1, 'status_reaction', 'pending', $2)",
    )
    .bind("dispatch-capability-filtered")
    .bind(serde_json::json!({"labels": ["mac-book"], "providers": ["codex"]}))
    .execute(&pool)
    .await
    .unwrap();

    let rejected = claim_pending_dispatch_outbox_batch_pg(&pool, "mac-mini-release").await;
    assert!(rejected.is_empty());
    let (diagnostics, constraint_results): (Option<serde_json::Value>, Option<serde_json::Value>) =
        sqlx::query_as(
            "SELECT routing_diagnostics, constraint_results
           FROM dispatch_outbox
          WHERE dispatch_id = 'dispatch-capability-filtered'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    let diagnostics = diagnostics.expect("mismatch should record routing diagnostics");
    assert_eq!(diagnostics["decision"]["eligible"], false);
    let constraint_results = constraint_results.expect("mismatch should record constraint results");
    assert_eq!(
        constraint_results[0]["constraints"][0]["outcome"]["outcome"],
        "available"
    );

    sqlx::query(
        "UPDATE dispatch_outbox
            SET next_attempt_at = NULL
          WHERE dispatch_id = 'dispatch-capability-filtered'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let claimed = claim_pending_dispatch_outbox_batch_pg(&pool, "mac-book-release").await;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].1, "dispatch-capability-filtered");

    pool.close().await;
    pg_db.drop().await;
}

fn postgres_base_database_url() -> String {
    if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
        let trimmed = base.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }

    let user = std::env::var("PGUSER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("USER")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "postgres".to_string());
    let password = std::env::var("PGPASSWORD")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let host = std::env::var("PGHOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let port = std::env::var("PGPORT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "5432".to_string());

    match password {
        Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
        None => format!("postgresql://{user}@{host}:{port}"),
    }
}

fn postgres_admin_database_url() -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{}/{}", postgres_base_database_url(), admin_db)
}

#[test]
fn parse_json_value_logs_warn_and_returns_none_for_malformed_json() {
    let (value, logs) = capture_logs(|| parse_json_value(Some("{"), "result_json"));
    assert!(value.is_none());
    assert!(logs.contains("[dispatch-outbox] malformed JSON in result_json"));
}

#[test]
fn extract_review_verdict_logs_warn_and_defaults_to_unknown_for_malformed_json() {
    let (verdict, logs) = capture_logs(|| extract_review_verdict(Some("{")));
    assert_eq!(verdict, "unknown");
    assert!(logs.contains("[dispatch-outbox] malformed JSON in result_json"));
}

#[test]
fn build_minimal_dispatch_message_logs_warn_for_malformed_dispatch_context() {
    let (message, logs) = capture_logs(|| {
        build_minimal_dispatch_message(
            "dispatch-123",
            "Title",
            Some("https://example.invalid/issues/948"),
            Some(948),
            Some("review"),
            Some("{"),
        )
    });
    assert!(!message.trim().is_empty());
    assert!(logs.contains("[dispatch-outbox] malformed JSON in dispatch_context"));
}

#[test]
fn format_dispatch_message_logs_warn_for_malformed_dispatch_context() {
    let (message, logs) = capture_logs(|| {
        format_dispatch_message(
            "dispatch-123",
            "Title",
            Some("https://example.invalid/issues/948"),
            Some(948),
            Some("review"),
            Some("{"),
        )
    });
    assert!(!message.trim().is_empty());
    assert!(logs.contains("[dispatch-outbox] malformed JSON in dispatch_context"));
}

#[derive(Clone, Default)]
struct MockOutboxNotifier {
    calls: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Default)]
struct DuplicateNotifyOutboxNotifier;

#[derive(Clone, Default)]
struct FailingNotifyOutboxNotifier;

#[derive(Clone)]
struct PgAwareTransport {
    pg_pool: PgPool,
    dispatch_calls: Arc<Mutex<Vec<String>>>,
}

impl PgAwareTransport {
    fn new(pg_pool: PgPool) -> Self {
        Self {
            pg_pool,
            dispatch_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl DispatchTransport for PgAwareTransport {
    fn pg_pool(&self) -> Option<&PgPool> {
        Some(&self.pg_pool)
    }

    async fn send_dispatch(
        &self,
        _db: Option<crate::db::Db>,
        agent_id: String,
        _title: String,
        _card_id: String,
        dispatch_id: String,
    ) -> Result<DispatchNotifyDeliveryResult, String> {
        self.dispatch_calls
            .lock()
            .unwrap()
            .push(format!("{agent_id}:{dispatch_id}"));
        Ok(DispatchNotifyDeliveryResult::success(
            dispatch_id,
            "notify",
            "pg-aware mock transport sent",
        ))
    }

    async fn send_review_followup(
        &self,
        _db: Option<crate::db::Db>,
        _review_dispatch_id: String,
        _card_id: String,
        _channel_id_num: u64,
        _message: String,
        _kind: discord_delivery::ReviewFollowupKind,
    ) -> Result<(), String> {
        Ok(())
    }
}

impl OutboxNotifier for MockOutboxNotifier {
    async fn notify_dispatch(
        &self,
        _db: Option<crate::db::Db>,
        _agent_id: String,
        _title: String,
        _card_id: String,
        dispatch_id: String,
    ) -> Result<DispatchNotifyDeliveryResult, String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("notify:{dispatch_id}"));
        Ok(DispatchNotifyDeliveryResult::success(
            dispatch_id,
            "notify",
            "mock outbox notifier sent",
        ))
    }

    async fn handle_followup(
        &self,
        _db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("followup:{dispatch_id}"));
        Ok(())
    }

    async fn sync_status_reaction(
        &self,
        _db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("status_reaction:{dispatch_id}"));
        Ok(())
    }
}

impl OutboxNotifier for DuplicateNotifyOutboxNotifier {
    async fn notify_dispatch(
        &self,
        _db: Option<crate::db::Db>,
        _agent_id: String,
        _title: String,
        _card_id: String,
        dispatch_id: String,
    ) -> Result<DispatchNotifyDeliveryResult, String> {
        Ok(DispatchNotifyDeliveryResult::duplicate(
            dispatch_id,
            "mock delivery guard duplicate",
        ))
    }

    async fn handle_followup(
        &self,
        _db: Option<crate::db::Db>,
        _dispatch_id: String,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn sync_status_reaction(
        &self,
        _db: Option<crate::db::Db>,
        _dispatch_id: String,
    ) -> Result<(), String> {
        Ok(())
    }
}

impl OutboxNotifier for FailingNotifyOutboxNotifier {
    async fn notify_dispatch(
        &self,
        _db: Option<crate::db::Db>,
        _agent_id: String,
        _title: String,
        _card_id: String,
        _dispatch_id: String,
    ) -> Result<DispatchNotifyDeliveryResult, String> {
        Err("mock permanent discord failure".to_string())
    }

    async fn handle_followup(
        &self,
        _db: Option<crate::db::Db>,
        _dispatch_id: String,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn sync_status_reaction(
        &self,
        _db: Option<crate::db::Db>,
        _dispatch_id: String,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// #750: status_reaction outbox rows route through notifier.sync_status_reaction.
/// The real notifier's sync is narrowed to write ❌ only for failed/cancelled
/// dispatches (command bot's ⏳/✅ covers normal lifecycle); mock captures
/// every invocation so we can assert the action is wired through.
#[tokio::test]
async fn process_outbox_batch_routes_status_reaction_through_notifier() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action) VALUES ('dispatch-status', 'status_reaction')",
            [],
        )
        .unwrap();
    }

    let notifier = MockOutboxNotifier::default();
    let processed = process_outbox_batch(&db, &notifier).await;
    assert_eq!(processed, 1);
    assert_eq!(
        *notifier.calls.lock().unwrap(),
        vec!["status_reaction:dispatch-status".to_string()],
        "#750: status_reaction action must flow through notifier.sync_status_reaction"
    );

    let conn = db.lock().unwrap();
    let row: (String, Option<String>) = conn
        .query_row(
            "SELECT status, processed_at FROM dispatch_outbox WHERE dispatch_id = 'dispatch-status'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(row.0, "done");
    assert!(row.1.is_some());
}

#[tokio::test]
async fn process_outbox_batch_records_duplicate_notify_delivery_result() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name) VALUES ('agent-dup', 'Duplicate Agent')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ('card-dup', 'Duplicate', 'ready', 'agent-dup', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, status, title, created_at, updated_at)
             VALUES ('dispatch-dup', 'card-dup', 'agent-dup', 'pending', 'Duplicate', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title, status)
             VALUES ('dispatch-dup', 'notify', 'agent-dup', 'card-dup', 'Duplicate', 'pending')",
            [],
        )
        .unwrap();
    }

    let processed = process_outbox_batch(&db, &DuplicateNotifyOutboxNotifier).await;
    assert_eq!(processed, 1);

    let conn = db.lock().unwrap();
    let row: (String, String, String) = conn
        .query_row(
            "SELECT status, delivery_status, delivery_result
               FROM dispatch_outbox
              WHERE dispatch_id = 'dispatch-dup'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row.0, "done");
    assert_eq!(row.1, "duplicate");
    let delivery: serde_json::Value = serde_json::from_str(&row.2).unwrap();
    assert_eq!(
        delivery["semantic_event_id"],
        "dispatch:dispatch-dup:notify"
    );
    assert_eq!(delivery["correlation_id"], "dispatch:dispatch-dup");
}

#[tokio::test]
async fn process_outbox_batch_records_permanent_failure_delivery_result() {
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name) VALUES ('agent-fail', 'Fail Agent')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ('card-fail', 'Failure', 'ready', 'agent-fail', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, status, title, created_at, updated_at)
             VALUES ('dispatch-fail', 'card-fail', 'agent-fail', 'pending', 'Failure', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (
                dispatch_id, action, agent_id, card_id, title, status, retry_count
             ) VALUES (
                'dispatch-fail', 'notify', 'agent-fail', 'card-fail', 'Failure', 'pending', 4
             )",
            [],
        )
        .unwrap();
    }

    let processed = process_outbox_batch(&db, &FailingNotifyOutboxNotifier).await;
    assert_eq!(processed, 1);

    let conn = db.lock().unwrap();
    let row: (String, String, String, String) = conn
        .query_row(
            "SELECT status, error, delivery_status, delivery_result
               FROM dispatch_outbox
              WHERE dispatch_id = 'dispatch-fail'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.0, "failed");
    assert_eq!(row.1, "mock permanent discord failure");
    assert_eq!(row.2, "permanent_failure");
    let delivery: serde_json::Value = serde_json::from_str(&row.3).unwrap();
    assert_eq!(delivery["status"], "permanent_failure");
    assert_eq!(delivery["detail"], "mock permanent discord failure");
}

#[tokio::test]
async fn handle_completed_dispatch_followups_with_pg_clears_done_card_threads() {
    let sqlite = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, active_thread_id, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-done")
    .bind("Done Card")
    .bind("done")
    .bind("thread-final")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, dispatch_type, status, title, thread_id, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
    )
    .bind("dispatch-final")
    .bind("card-done")
    .bind("implementation")
    .bind("completed")
    .bind("Done Card")
    .bind("thread-final")
    .execute(&pool)
    .await
    .unwrap();

    handle_completed_dispatch_followups_with_pg(Some(&sqlite), Some(&pool), "dispatch-final")
        .await
        .unwrap();

    let active_thread: Option<String> =
        sqlx::query_scalar("SELECT active_thread_id FROM kanban_cards WHERE id = $1")
            .bind("card-done")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        active_thread.is_none(),
        "done-card followup should clear active_thread_id in postgres"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn handle_completed_dispatch_followups_defers_archive_for_active_thread_turn() {
    let sqlite = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, active_thread_id, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-active-thread")
    .bind("Active Thread Card")
    .bind("done")
    .bind("1492434645395177545")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, dispatch_type, status, title, thread_id, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
    )
    .bind("dispatch-active-thread")
    .bind("card-active-thread")
    .bind("implementation")
    .bind("completed")
    .bind("Active Thread Card")
    .bind("1492434645395177545")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, active_dispatch_id, thread_channel_id, created_at, last_heartbeat
         ) VALUES ($1, $2, $3, $4, $5, NOW(), NOW())",
    )
    .bind("test:AgentDesk-claude-adk-cc-t1492434645395177545")
    .bind("claude")
    .bind("turn_active")
    .bind("dispatch-active-thread")
    .bind("1492434645395177545")
    .execute(&pool)
    .await
    .unwrap();

    let err = handle_completed_dispatch_followups_with_pg(
        Some(&sqlite),
        Some(&pool),
        "dispatch-active-thread",
    )
    .await
    .expect_err("active thread turn should defer archive/followup processing");
    assert!(err.contains("still has an active turn"));

    let active_thread: Option<String> =
        sqlx::query_scalar("SELECT active_thread_id FROM kanban_cards WHERE id = $1")
            .bind("card-active-thread")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active_thread.as_deref(), Some("1492434645395177545"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn handle_completed_dispatch_followups_with_transport_uses_transport_pg_pool() {
    let sqlite = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, active_thread_id, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-transport-pg")
    .bind("Transport PG Card")
    .bind("done")
    .bind("thread-transport")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, dispatch_type, status, title, thread_id, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
    )
    .bind("dispatch-transport-pg")
    .bind("card-transport-pg")
    .bind("implementation")
    .bind("completed")
    .bind("Transport PG Card")
    .bind("thread-transport")
    .execute(&pool)
    .await
    .unwrap();

    let transport = PgAwareTransport::new(pool.clone());
    let config = DispatchFollowupConfig {
        discord_api_base: "http://127.0.0.1:9".to_string(),
        notify_bot_token: None,
        announce_bot_token: None,
    };

    handle_completed_dispatch_followups_with_config_and_transport(
        &sqlite,
        "dispatch-transport-pg",
        &config,
        &transport,
    )
    .await
    .unwrap();

    let active_thread: Option<String> =
        sqlx::query_scalar("SELECT active_thread_id FROM kanban_cards WHERE id = $1")
            .bind("card-transport-pg")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        active_thread.is_none(),
        "transport-backed PG followup should clear active_thread_id without SQLite mirroring"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn send_dispatch_with_transport_uses_transport_pg_pool_for_delivery_guard() {
    let sqlite = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let transport = PgAwareTransport::new(pool.clone());

    discord_delivery::send_dispatch_to_discord_with_transport(
        &sqlite,
        "agent-pg-transport",
        "Transport dispatch",
        "card-pg-transport",
        "dispatch-pg-transport",
        &transport,
    )
    .await
    .unwrap();

    assert_eq!(
        *transport.dispatch_calls.lock().unwrap(),
        vec!["agent-pg-transport:dispatch-pg-transport".to_string()]
    );

    let notified: Option<String> =
        sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
            .bind("dispatch_notified:dispatch-pg-transport")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(
        notified.as_deref(),
        Some("dispatch-pg-transport"),
        "delivery guard should persist notification state in postgres when transport carries a pool"
    );

    let sqlite_notified: Option<String> = sqlite
        .lock()
        .unwrap()
        .query_row(
            "SELECT value FROM kv_meta WHERE key = ?1",
            ["dispatch_notified:dispatch-pg-transport"],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    assert!(
        sqlite_notified.is_none(),
        "transport-backed PG delivery should not backfill SQLite guard keys"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn process_outbox_batch_with_pg_notify_transitions_dispatch_and_enqueues_reaction() {
    let sqlite = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (
            id, name, created_at, updated_at
         ) VALUES ($1, $2, NOW(), NOW())",
    )
    .bind("agent-pg")
    .bind("Agent PG")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, assigned_agent_id, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-pg-notify")
    .bind("PG Notify Card")
    .bind("todo")
    .bind("agent-pg")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
    )
    .bind("dispatch-pg-notify")
    .bind("card-pg-notify")
    .bind("agent-pg")
    .bind("implementation")
    .bind("pending")
    .bind("PG Notify Card")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, agent_id, card_id, title
         ) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("dispatch-pg-notify")
    .bind("notify")
    .bind("agent-pg")
    .bind("card-pg-notify")
    .bind("PG Notify Card")
    .execute(&pool)
    .await
    .unwrap();

    let notifier = MockOutboxNotifier::default();
    let processed = process_outbox_batch_with_pg(Some(&sqlite), Some(&pool), &notifier, None).await;
    assert_eq!(processed, 1);
    assert_eq!(
        notifier.calls.lock().unwrap().as_slice(),
        ["notify:dispatch-pg-notify"]
    );

    let outbox_row: (String, Option<String>) = sqlx::query_as(
        "SELECT status, processed_at::text
           FROM dispatch_outbox
          WHERE dispatch_id = $1
            AND action = 'notify'",
    )
    .bind("dispatch-pg-notify")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outbox_row.0, "done");
    assert!(outbox_row.1.is_some());

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-pg-notify")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "dispatched");

    let reaction_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
           FROM dispatch_outbox
          WHERE dispatch_id = $1
            AND action = 'status_reaction'
            AND status = 'pending'",
    )
    .bind("dispatch-pg-notify")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reaction_count, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn queue_dispatch_followup_pg_inserts_one_shot_row() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    queue_dispatch_followup_pg(&pool, "dispatch-pg-followup")
        .await
        .unwrap();
    queue_dispatch_followup_pg(&pool, "dispatch-pg-followup")
        .await
        .unwrap();

    let row: (String, String, String) = sqlx::query_as(
        "SELECT dispatch_id, action, status
           FROM dispatch_outbox
          WHERE dispatch_id = $1",
    )
    .bind("dispatch-pg-followup")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "dispatch-pg-followup");
    assert_eq!(row.1, "followup");
    assert_eq!(row.2, "pending");

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
           FROM dispatch_outbox
          WHERE dispatch_id = $1
            AND action = 'followup'",
    )
    .bind("dispatch-pg-followup")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn requeue_dispatch_notify_pg_inserts_and_rearms_notify_row() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (
            id, name, created_at, updated_at
         ) VALUES ($1, $2, NOW(), NOW())",
    )
    .bind("agent-requeue")
    .bind("Agent Requeue")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, assigned_agent_id, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-requeue")
    .bind("PG Requeue Card")
    .bind("todo")
    .bind("agent-requeue")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
    )
    .bind("dispatch-requeue")
    .bind("card-requeue")
    .bind("agent-requeue")
    .bind("implementation")
    .bind("pending")
    .bind("PG Requeue Card")
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        requeue_dispatch_notify_pg(&pool, "dispatch-requeue")
            .await
            .unwrap()
    );

    sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'failed',
                retry_count = 3,
                next_attempt_at = NOW() + INTERVAL '10 minutes',
                processed_at = NOW(),
                error = 'boom'
          WHERE dispatch_id = $1
            AND action = 'notify'",
    )
    .bind("dispatch-requeue")
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        requeue_dispatch_notify_pg(&pool, "dispatch-requeue")
            .await
            .unwrap()
    );

    let row: (
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT agent_id, card_id, title, status, retry_count,
                    next_attempt_at::text, processed_at::text, error
               FROM dispatch_outbox
              WHERE dispatch_id = $1
                AND action = 'notify'",
    )
    .bind("dispatch-requeue")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "agent-requeue");
    assert_eq!(row.1, "card-requeue");
    assert_eq!(row.2, "PG Requeue Card");
    assert_eq!(row.3, "pending");
    assert_eq!(row.4, 0);
    assert!(row.5.is_none());
    assert!(row.6.is_none());
    assert!(row.7.is_none());

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
           FROM dispatch_outbox
          WHERE dispatch_id = $1
            AND action = 'notify'",
    )
    .bind("dispatch-requeue")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn queue_dispatch_followup_sync_prefers_postgres_when_available() {
    let sqlite = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    queue_dispatch_followup_sync(&sqlite, Some(&pool), "dispatch-sync-followup");
    queue_dispatch_followup_sync(&sqlite, Some(&pool), "dispatch-sync-followup");

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
           FROM dispatch_outbox
          WHERE dispatch_id = $1
            AND action = 'followup'",
    )
    .bind("dispatch-sync-followup")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    pool.close().await;
    pg_db.drop().await;
}
