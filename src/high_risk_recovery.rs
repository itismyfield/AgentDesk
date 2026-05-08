use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::config::Config;
use crate::engine::PolicyEngine;
use crate::services::dispatches::discord_delivery::{
    DispatchNotifyDeliveryResult, DispatchTransport, ReviewFollowupKind,
    send_dispatch_with_delivery_guard,
};
use crate::services::dispatches::outbox_queue::{OutboxNotifier, process_outbox_batch_with_pg};

struct PgRecoveryTestDatabase {
    _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl PgRecoveryTestDatabase {
    async fn create() -> Self {
        let lifecycle = crate::db::postgres::lock_test_lifecycle();
        let admin_url = pg_test_admin_database_url();
        let database_name = format!("agentdesk_pg_recovery_{}", uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", pg_test_base_database_url(), database_name);
        crate::db::postgres::create_test_database(
            &admin_url,
            &database_name,
            "pg-only high_risk_recovery",
        )
        .await
        .expect("create postgres recovery test db");

        Self {
            _lifecycle: lifecycle,
            admin_url,
            database_name,
            database_url,
        }
    }

    async fn migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(
            &self.database_url,
            "pg-only high_risk_recovery",
        )
        .await
        .expect("connect + migrate postgres recovery test db")
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "pg-only high_risk_recovery",
        )
        .await
        .expect("drop postgres recovery test db");
    }
}

fn pg_test_base_database_url() -> String {
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

fn pg_test_admin_database_url() -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{}/{}", pg_test_base_database_url(), admin_db)
}

fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> PolicyEngine {
    crate::pipeline::ensure_loaded();
    let mut config = Config::default();
    config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    PolicyEngine::new_with_pg(&config, Some(pg_pool)).expect("create pg-backed policy engine")
}

async fn seed_agent_pg(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO agents (
             id,
             name,
             provider,
             discord_channel_id,
             discord_channel_alt,
             discord_channel_cc,
             discord_channel_cdx
         )
         VALUES ('agent-1', 'Test Agent', 'codex', '111', '222', '111', '222')
         ON CONFLICT (id) DO UPDATE SET
             name = EXCLUDED.name,
             provider = EXCLUDED.provider,
             discord_channel_id = EXCLUDED.discord_channel_id,
             discord_channel_alt = EXCLUDED.discord_channel_alt,
             discord_channel_cc = EXCLUDED.discord_channel_cc,
             discord_channel_cdx = EXCLUDED.discord_channel_cdx",
    )
    .execute(pool)
    .await
    .expect("seed postgres agent");
}

async fn seed_card_pg(pool: &sqlx::PgPool, card_id: &str, status: &str) {
    sqlx::query(
        "INSERT INTO kanban_cards (
            id,
            title,
            status,
            assigned_agent_id,
            created_at,
            updated_at
         ) VALUES (
            $1,
            'Test Card',
            $2,
            'agent-1',
            NOW(),
            NOW()
         )
         ON CONFLICT (id) DO UPDATE SET
             status = EXCLUDED.status,
             assigned_agent_id = EXCLUDED.assigned_agent_id,
             updated_at = EXCLUDED.updated_at",
    )
    .bind(card_id)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed postgres card");
}

#[derive(Clone, Default)]
struct RestartGuardTransport {
    posts: Arc<Mutex<Vec<String>>>,
}

impl RestartGuardTransport {
    fn post_count(&self) -> usize {
        self.posts.lock().unwrap().len()
    }
}

impl DispatchTransport for RestartGuardTransport {
    async fn send_dispatch(
        &self,
        _db: Option<crate::db::Db>,
        _agent_id: String,
        _title: String,
        _card_id: String,
        dispatch_id: String,
    ) -> Result<DispatchNotifyDeliveryResult, String> {
        self.posts.lock().unwrap().push(dispatch_id.clone());
        let mut result =
            DispatchNotifyDeliveryResult::success(&dispatch_id, "notify", "restart mock sent");
        result.correlation_id = Some(format!("dispatch:{dispatch_id}"));
        result.semantic_event_id = Some(format!("dispatch:{dispatch_id}:notify"));
        result.target_channel_id = Some("111".to_string());
        result.message_id = Some("restart-new-message".to_string());
        Ok(result)
    }

    async fn send_review_followup(
        &self,
        _db: Option<crate::db::Db>,
        _review_dispatch_id: String,
        _card_id: String,
        _channel_id_num: u64,
        _message: String,
        _kind: ReviewFollowupKind,
    ) -> Result<(), String> {
        Ok(())
    }
}

struct RestartGuardNotifier {
    pool: sqlx::PgPool,
    transport: RestartGuardTransport,
}

impl OutboxNotifier for RestartGuardNotifier {
    async fn notify_dispatch(
        &self,
        _db: Option<crate::db::Db>,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> Result<DispatchNotifyDeliveryResult, String> {
        send_dispatch_with_delivery_guard(
            None,
            Some(&self.pool),
            &agent_id,
            &title,
            &card_id,
            &dispatch_id,
            &self.transport,
        )
        .await
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

#[tokio::test]
async fn boot_reconcile_pg_resets_stale_runtime_rows() {
    let pg_db = PgRecoveryTestDatabase::create().await;
    let pool = pg_db.migrate().await;
    let engine = test_engine_with_pg(pool.clone());

    seed_agent_pg(&pool).await;
    seed_card_pg(&pool, "card-pg-runtime", "in_progress").await;
    seed_card_pg(&pool, "card-pg-runtime-valid", "in_progress").await;

    sqlx::query(
        "INSERT INTO task_dispatches (
            id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            created_at,
            updated_at
         ) VALUES (
            'dispatch-valid',
            'card-pg-runtime-valid',
            'agent-1',
            'implementation',
            'pending',
            'Valid pending dispatch',
            NOW(),
            NOW()
         )",
    )
    .execute(&pool)
    .await
    .expect("seed active dispatch");
    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, status)
         VALUES ('dispatch-processing', 'notify', 'processing')",
    )
    .execute(&pool)
    .await
    .expect("seed stale outbox row");
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ('dispatch_reserving:dispatch-valid', 'agent-1')",
    )
    .execute(&pool)
    .await
    .expect("seed stale reservation");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
         VALUES ('run-pg-runtime', 'agent-1', 'active')",
    )
    .execute(&pool)
    .await
    .expect("seed auto queue run");
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id,
            run_id,
            kanban_card_id,
            agent_id,
            status,
            dispatch_id,
            dispatched_at
         ) VALUES (
            'entry-broken',
            'run-pg-runtime',
            'card-pg-runtime',
            'agent-1',
            'dispatched',
            NULL,
            NOW()
         )",
    )
    .execute(&pool)
    .await
    .expect("seed broken auto queue entry");
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id,
            run_id,
            kanban_card_id,
            agent_id,
            status,
            dispatch_id,
            dispatched_at
         ) VALUES (
            'entry-valid',
            'run-pg-runtime',
            'card-pg-runtime-valid',
            'agent-1',
            'dispatched',
            'dispatch-valid',
            NOW()
         )",
    )
    .execute(&pool)
    .await
    .expect("seed valid auto queue entry");

    let stats = crate::reconcile::reconcile_boot_runtime(None, &engine, Some(&pool))
        .await
        .expect("pg boot reconcile succeeds");

    assert_eq!(stats.stale_processing_outbox_reset, 1);
    assert_eq!(stats.stale_dispatch_reservations_cleared, 1);
    assert_eq!(stats.missing_notify_outbox_backfilled, 1);
    assert_eq!(stats.broken_auto_queue_entries_reset, 1);

    let outbox_status: String = sqlx::query_scalar(
        "SELECT status FROM dispatch_outbox
         WHERE dispatch_id = 'dispatch-processing' AND action = 'notify'",
    )
    .fetch_one(&pool)
    .await
    .expect("load outbox status");
    assert_eq!(outbox_status, "pending");

    let reservation_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM kv_meta WHERE key = 'dispatch_reserving:dispatch-valid'
        )",
    )
    .fetch_one(&pool)
    .await
    .expect("load reservation status");
    assert!(!reservation_exists);

    let broken_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-broken'")
            .fetch_one(&pool)
            .await
            .expect("load broken entry status");
    let valid_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-valid'")
            .fetch_one(&pool)
            .await
            .expect("load valid entry status");
    assert_eq!(broken_status, "pending");
    assert_eq!(valid_status, "dispatched");

    drop(engine);
    crate::db::postgres::close_test_pool(pool, "pg-only high_risk_recovery")
        .await
        .expect("close pg-only high_risk_recovery pool");
    pg_db.drop().await;
}

#[tokio::test]
async fn restart_recovery_does_not_repost_prior_typed_dispatch_delivery() {
    let pg_db = PgRecoveryTestDatabase::create().await;
    let pool = pg_db.migrate().await;
    let engine = test_engine_with_pg(pool.clone());

    seed_agent_pg(&pool).await;
    seed_card_pg(&pool, "card-pg-restart-delivery", "in_progress").await;
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            'dispatch-pg-restart-delivery',
            'card-pg-restart-delivery',
            'agent-1',
            'implementation',
            'pending',
            'Restart delivery',
            NOW(),
            NOW()
         )",
    )
    .execute(&pool)
    .await
    .expect("seed restart dispatch");
    sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, agent_id, card_id, title, status, claimed_at, claim_owner
         ) VALUES (
            'dispatch-pg-restart-delivery',
            'notify',
            'agent-1',
            'card-pg-restart-delivery',
            'Restart delivery',
            'processing',
            NOW() - INTERVAL '1 minute',
            'old-dcserver'
         )",
    )
    .execute(&pool)
    .await
    .expect("seed processing notify outbox");
    sqlx::query(
        "INSERT INTO dispatch_delivery_events (
            dispatch_id,
            correlation_id,
            semantic_event_id,
            operation,
            target_kind,
            target_channel_id,
            status,
            attempt,
            message_id,
            messages_json,
            result_json
         ) VALUES (
            'dispatch-pg-restart-delivery',
            'dispatch:dispatch-pg-restart-delivery',
            'dispatch:dispatch-pg-restart-delivery:notify',
            'send',
            'channel',
            '111',
            'sent',
            1,
            'restart-prior-message',
            '[{\"channel_id\":\"111\",\"message_id\":\"restart-prior-message\"}]'::jsonb,
            '{\"status\":\"success\",\"message_id\":\"restart-prior-message\"}'::jsonb
         )",
    )
    .execute(&pool)
    .await
    .expect("seed prior typed sent delivery");

    let stats = crate::reconcile::reconcile_boot_runtime(None, &engine, Some(&pool))
        .await
        .expect("pg boot reconcile succeeds");
    assert_eq!(stats.stale_processing_outbox_reset, 1);

    let transport = RestartGuardTransport::default();
    let notifier = RestartGuardNotifier {
        pool: pool.clone(),
        transport: transport.clone(),
    };
    let processed =
        process_outbox_batch_with_pg(None, Some(&pool), &notifier, Some("restart-test")).await;
    assert_eq!(processed, 1);
    assert_eq!(
        transport.post_count(),
        0,
        "typed prior delivery must suppress restart replay posts"
    );

    let (outbox_status, delivery_status, delivery_result): (
        String,
        Option<String>,
        Option<serde_json::Value>,
    ) = sqlx::query_as(
        "SELECT status, delivery_status, delivery_result
           FROM dispatch_outbox
          WHERE dispatch_id = 'dispatch-pg-restart-delivery'
            AND action = 'notify'",
    )
    .fetch_one(&pool)
    .await
    .expect("load restart outbox result");
    assert_eq!(outbox_status, "done");
    assert_eq!(delivery_status.as_deref(), Some("duplicate"));
    assert_eq!(
        delivery_result
            .as_ref()
            .and_then(|value| value.get("message_id"))
            .and_then(|value| value.as_str()),
        Some("restart-prior-message")
    );

    drop(engine);
    crate::db::postgres::close_test_pool(pool, "pg-only high_risk_recovery")
        .await
        .expect("close pg-only high_risk_recovery pool");
    pg_db.drop().await;
}

#[tokio::test]
async fn runtime_reconcile_auto_queue_pending_delivery_orphans_requeues_notify_outbox() {
    let pg_db = PgRecoveryTestDatabase::create().await;
    let pool = pg_db.migrate().await;

    seed_agent_pg(&pool).await;
    for card_id in [
        "card-pg-aq-orphan-failed",
        "card-pg-aq-orphan-missing",
        "card-pg-aq-orphan-live",
    ] {
        seed_card_pg(&pool, card_id, "in_progress").await;
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
         VALUES ('run-pg-aq-orphan', 'agent-1', 'active')",
    )
    .execute(&pool)
    .await
    .expect("seed auto queue run");

    for (dispatch_id, card_id, entry_id, slot_index) in [
        (
            "dispatch-pg-aq-orphan-failed",
            "card-pg-aq-orphan-failed",
            "entry-pg-aq-orphan-failed",
            0_i32,
        ),
        (
            "dispatch-pg-aq-orphan-missing",
            "card-pg-aq-orphan-missing",
            "entry-pg-aq-orphan-missing",
            1_i32,
        ),
        (
            "dispatch-pg-aq-orphan-live",
            "card-pg-aq-orphan-live",
            "entry-pg-aq-orphan-live",
            2_i32,
        ),
    ] {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id,
                kanban_card_id,
                to_agent_id,
                dispatch_type,
                status,
                title,
                context,
                required_capabilities,
                created_at,
                updated_at
             ) VALUES (
                $1,
                $2,
                'agent-1',
                'implementation',
                'pending',
                'Auto queue pending delivery',
                $3,
                $4::jsonb,
                NOW() - INTERVAL '10 minutes',
                NOW() - INTERVAL '10 minutes'
             )",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind(
            json!({
                "auto_queue": true,
                "entry_id": entry_id,
                "slot_index": slot_index
            })
            .to_string(),
        )
        .bind(json!(["shell"]))
        .execute(&pool)
        .await
        .expect("seed pending auto queue dispatch");

        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id,
                run_id,
                kanban_card_id,
                agent_id,
                status,
                dispatch_id,
                slot_index,
                dispatched_at,
                created_at
             ) VALUES (
                $1,
                'run-pg-aq-orphan',
                $2,
                'agent-1',
                'dispatched',
                $3,
                $4,
                NOW() - INTERVAL '10 minutes',
                NOW() - INTERVAL '10 minutes'
             )",
        )
        .bind(entry_id)
        .bind(card_id)
        .bind(dispatch_id)
        .bind(slot_index)
        .execute(&pool)
        .await
        .expect("seed dispatched auto queue entry");
    }

    sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id,
            action,
            agent_id,
            card_id,
            title,
            status,
            retry_count,
            next_attempt_at,
            processed_at,
            error,
            delivery_status,
            delivery_result,
            claimed_at,
            claim_owner,
            required_capabilities
         ) VALUES (
            'dispatch-pg-aq-orphan-failed',
            'notify',
            'agent-1',
            'card-pg-aq-orphan-failed',
            'Stale failed notify',
            'failed',
            4,
            NOW() + INTERVAL '1 hour',
            NOW() - INTERVAL '9 minutes',
            'delivery failed',
            'failed',
            $1::jsonb,
            NOW() - INTERVAL '9 minutes',
            'old-worker',
            $2::jsonb
         ),
         (
            'dispatch-pg-aq-orphan-live',
            'notify',
            'agent-1',
            'card-pg-aq-orphan-live',
            'Live failed notify',
            'failed',
            7,
            NOW() + INTERVAL '1 hour',
            NOW() - INTERVAL '9 minutes',
            'delivery failed',
            'failed',
            $1::jsonb,
            NOW() - INTERVAL '9 minutes',
            'old-worker',
            $2::jsonb
         )",
    )
    .bind(json!({"ok": false}))
    .bind(json!(["shell"]))
    .execute(&pool)
    .await
    .expect("seed failed notify outbox rows");

    sqlx::query(
        "INSERT INTO sessions (
            session_key,
            agent_id,
            provider,
            status,
            active_dispatch_id,
            last_heartbeat,
            created_at
         ) VALUES (
            'session-pg-aq-orphan-live',
            'agent-1',
            'codex',
            'turn_active',
            'dispatch-pg-aq-orphan-live',
            NOW(),
            NOW() - INTERVAL '9 minutes'
         )",
    )
    .execute(&pool)
    .await
    .expect("seed live session linked to dispatch");

    let stats = crate::reconcile::reconcile_auto_queue_pending_delivery_orphans_pg(&pool)
        .await
        .expect("runtime orphan reconcile succeeds");
    assert_eq!(stats.candidates, 2);
    assert_eq!(stats.requeued_notify, 2);
    assert_eq!(stats.skipped, 0);

    let repaired_failed: (String, i64, bool, bool, bool, bool, bool, bool, bool, bool) =
        sqlx::query_as(
            "SELECT status,
                    retry_count,
                    next_attempt_at IS NULL,
                    processed_at IS NULL,
                    error IS NULL,
                    delivery_status IS NULL,
                    delivery_result IS NULL,
                    claimed_at IS NULL,
                    claim_owner IS NULL,
                    required_capabilities = '[\"shell\"]'::jsonb
               FROM dispatch_outbox
              WHERE dispatch_id = 'dispatch-pg-aq-orphan-failed'
                AND action = 'notify'",
        )
        .fetch_one(&pool)
        .await
        .expect("load repaired failed notify row");
    assert_eq!(
        repaired_failed,
        (
            "pending".to_string(),
            0,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true
        )
    );

    let inserted_missing: (String, i64, bool) = sqlx::query_as(
        "SELECT status,
                retry_count,
                required_capabilities = '[\"shell\"]'::jsonb
           FROM dispatch_outbox
          WHERE dispatch_id = 'dispatch-pg-aq-orphan-missing'
            AND action = 'notify'",
    )
    .fetch_one(&pool)
    .await
    .expect("load inserted missing notify row");
    assert_eq!(inserted_missing, ("pending".to_string(), 0, true));

    let untouched_live: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, retry_count, claim_owner
           FROM dispatch_outbox
          WHERE dispatch_id = 'dispatch-pg-aq-orphan-live'
            AND action = 'notify'",
    )
    .fetch_one(&pool)
    .await
    .expect("load live notify row");
    assert_eq!(
        untouched_live,
        ("failed".to_string(), 7, Some("old-worker".to_string()))
    );

    let runtime_states: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT td.id, td.status, e.status
           FROM task_dispatches td
           JOIN auto_queue_entries e ON e.dispatch_id = td.id
          WHERE td.id LIKE 'dispatch-pg-aq-orphan-%'
          ORDER BY td.id",
    )
    .fetch_all(&pool)
    .await
    .expect("load runtime states");
    assert_eq!(
        runtime_states,
        vec![
            (
                "dispatch-pg-aq-orphan-failed".to_string(),
                "pending".to_string(),
                "dispatched".to_string()
            ),
            (
                "dispatch-pg-aq-orphan-live".to_string(),
                "pending".to_string(),
                "dispatched".to_string()
            ),
            (
                "dispatch-pg-aq-orphan-missing".to_string(),
                "pending".to_string(),
                "dispatched".to_string()
            ),
        ]
    );

    crate::db::postgres::close_test_pool(pool, "pg-only high_risk_recovery")
        .await
        .expect("close pg-only high_risk_recovery pool");
    pg_db.drop().await;
}

#[tokio::test]
async fn boot_reconcile_pg_refires_missing_review_dispatch() {
    let pg_db = PgRecoveryTestDatabase::create().await;
    let pool = pg_db.migrate().await;
    let engine = test_engine_with_pg(pool.clone());

    seed_agent_pg(&pool).await;
    seed_card_pg(&pool, "card-pg-review", "review").await;

    let reviewed_commit = crate::services::platform::git_head_commit(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or_else(|| "0000000000000000000000000000000000000000".to_string());
    sqlx::query(
        "INSERT INTO task_dispatches (
            id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            context,
            created_at,
            updated_at,
            completed_at
         ) VALUES (
            'dispatch-pg-work',
            'card-pg-review',
            'agent-1',
            'implementation',
            'completed',
            'Completed implementation',
            $1::jsonb,
            NOW() - INTERVAL '2 minutes',
            NOW() - INTERVAL '1 minute',
            NOW() - INTERVAL '1 minute'
         )",
    )
    .bind(json!({
        "reviewed_commit": reviewed_commit,
        "branch": "test-review-target"
    }))
    .execute(&pool)
    .await
    .expect("seed completed implementation dispatch");

    let stats = crate::reconcile::reconcile_boot_runtime(None, &engine, Some(&pool))
        .await
        .expect("pg boot reconcile succeeds");

    assert_eq!(
        stats.missing_review_dispatches_refired, 1,
        "boot reconcile must re-fire OnReviewEnter through PG"
    );
    let active_review_dispatches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM task_dispatches
         WHERE kanban_card_id = 'card-pg-review'
           AND dispatch_type IN ('review', 'review-decision')
           AND status IN ('pending', 'dispatched')",
    )
    .fetch_one(&pool)
    .await
    .expect("count active review dispatches");
    assert_eq!(active_review_dispatches, 1);

    drop(engine);
    crate::db::postgres::close_test_pool(pool, "pg-only high_risk_recovery")
        .await
        .expect("close pg-only high_risk_recovery pool");
    pg_db.drop().await;
}

#[tokio::test]
async fn completed_queue_review_drift_reconcile_promotes_only_stale_done_entries() {
    let pg_db = PgRecoveryTestDatabase::create().await;
    let pool = pg_db.migrate().await;
    let engine = test_engine_with_pg(pool.clone());

    seed_agent_pg(&pool).await;
    seed_card_pg(&pool, "card-pg-drift-old", "in_progress").await;
    seed_card_pg(&pool, "card-pg-drift-fresh", "in_progress").await;
    seed_card_pg(&pool, "card-pg-drift-null-completed", "in_progress").await;
    seed_card_pg(&pool, "card-pg-drift-review-state", "review").await;
    seed_card_pg(&pool, "card-pg-drift-active-dispatch", "in_progress").await;
    seed_card_pg(&pool, "card-pg-drift-active-entry", "in_progress").await;

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
         VALUES
            ('run-pg-review-drift', 'agent-1', 'active'),
            ('run-pg-review-drift-active-entry', 'agent-1', 'active')",
    )
    .execute(&pool)
    .await
    .expect("seed auto queue run");

    let reviewed_commit = crate::services::platform::git_head_commit(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or_else(|| "0000000000000000000000000000000000000000".to_string());

    for card_id in [
        "card-pg-drift-old",
        "card-pg-drift-fresh",
        "card-pg-drift-null-completed",
        "card-pg-drift-review-state",
        "card-pg-drift-active-dispatch",
        "card-pg-drift-active-entry",
    ] {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id,
                kanban_card_id,
                to_agent_id,
                dispatch_type,
                status,
                title,
                context,
                created_at,
                updated_at,
                completed_at
             ) VALUES (
                $1,
                $2,
                'agent-1',
                'implementation',
                'completed',
                'Completed implementation',
                $3::jsonb,
                NOW() - INTERVAL '10 minutes',
                NOW() - INTERVAL '9 minutes',
                NOW() - INTERVAL '9 minutes'
             )",
        )
        .bind(format!("dispatch-{card_id}"))
        .bind(card_id)
        .bind(json!({
            "reviewed_commit": reviewed_commit,
            "branch": "test-review-drift"
        }))
        .execute(&pool)
        .await
        .expect("seed completed implementation dispatch");
    }

    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id,
            run_id,
            kanban_card_id,
            agent_id,
            status,
            dispatch_id,
            completed_at,
            created_at
         ) VALUES
           (
             'entry-pg-drift-old',
             'run-pg-review-drift',
             'card-pg-drift-old',
             'agent-1',
             'done',
             'dispatch-card-pg-drift-old',
             NOW() - INTERVAL '6 minutes',
             NOW() - INTERVAL '10 minutes'
           ),
           (
             'entry-pg-drift-fresh',
             'run-pg-review-drift',
             'card-pg-drift-fresh',
             'agent-1',
             'done',
             'dispatch-card-pg-drift-fresh',
             NOW() - INTERVAL '1 minute',
             NOW() - INTERVAL '10 minutes'
           ),
           (
             'entry-pg-drift-null-completed',
             'run-pg-review-drift',
             'card-pg-drift-null-completed',
             'agent-1',
             'done',
             'dispatch-card-pg-drift-null-completed',
             NULL,
             NOW() - INTERVAL '10 minutes'
           ),
           (
             'entry-pg-drift-review-state',
             'run-pg-review-drift',
             'card-pg-drift-review-state',
             'agent-1',
             'done',
             'dispatch-card-pg-drift-review-state',
             NOW() - INTERVAL '6 minutes',
             NOW() - INTERVAL '10 minutes'
           ),
           (
             'entry-pg-drift-active-dispatch',
             'run-pg-review-drift',
             'card-pg-drift-active-dispatch',
             'agent-1',
             'done',
             'dispatch-card-pg-drift-active-dispatch',
             NOW() - INTERVAL '6 minutes',
             NOW() - INTERVAL '10 minutes'
           ),
           (
             'entry-pg-drift-active-entry-done',
             'run-pg-review-drift',
             'card-pg-drift-active-entry',
             'agent-1',
             'done',
             'dispatch-card-pg-drift-active-entry',
             NOW() - INTERVAL '6 minutes',
             NOW() - INTERVAL '10 minutes'
           ),
           (
             'entry-pg-drift-active-entry-pending',
             'run-pg-review-drift-active-entry',
             'card-pg-drift-active-entry',
             'agent-1',
             'pending',
             NULL,
             NULL,
             NOW()
           )",
    )
    .execute(&pool)
    .await
    .expect("seed auto queue entries");
    sqlx::query(
        "INSERT INTO task_dispatches (
            id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            created_at,
            updated_at
         ) VALUES (
            'dispatch-card-pg-drift-active-dispatch-pending',
            'card-pg-drift-active-dispatch',
            'agent-1',
            'implementation',
            'pending',
            'Active implementation',
            NOW(),
            NOW()
         )",
    )
    .execute(&pool)
    .await
    .expect("seed active implementation dispatch");

    let recovered =
        crate::reconcile::reconcile_completed_queue_review_drift_pg(&pool, None, &engine)
            .await
            .expect("review drift reconcile succeeds");
    assert_eq!(recovered, 1);

    let statuses: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, status
         FROM kanban_cards
         WHERE id LIKE 'card-pg-drift-%'
         ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("load card statuses");
    assert_eq!(
        statuses,
        vec![
            (
                "card-pg-drift-active-dispatch".to_string(),
                "in_progress".to_string()
            ),
            (
                "card-pg-drift-active-entry".to_string(),
                "in_progress".to_string()
            ),
            ("card-pg-drift-fresh".to_string(), "in_progress".to_string()),
            (
                "card-pg-drift-null-completed".to_string(),
                "in_progress".to_string()
            ),
            ("card-pg-drift-old".to_string(), "review".to_string()),
            (
                "card-pg-drift-review-state".to_string(),
                "review".to_string()
            ),
        ]
    );

    let active_review_dispatches: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM task_dispatches
         WHERE kanban_card_id = 'card-pg-drift-old'
           AND dispatch_type IN ('review', 'review-decision')
           AND status IN ('pending', 'dispatched')",
    )
    .fetch_one(&pool)
    .await
    .expect("count active review dispatches");
    assert_eq!(active_review_dispatches, 1);

    drop(engine);
    crate::db::postgres::close_test_pool(pool, "pg-only high_risk_recovery")
        .await
        .expect("close pg-only high_risk_recovery pool");
    pg_db.drop().await;
}
