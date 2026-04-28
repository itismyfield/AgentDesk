//! High-risk recovery lane for restart/reconcile/outbox boundary scenarios.
//!
//! Run with `cargo test --bin agentdesk high_risk_recovery::`.

use super::*;

mod failure_recovery {
    use super::*;

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
                "integration high_risk_recovery",
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

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "integration high_risk_recovery",
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

    #[test]
    fn scenario_3_restart_recovery_reconciles_broken_state() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-s3", "review");

        // Simulate pre-crash broken state from an older DB version:
        // 1) Drop the partial unique index (simulates pre-#116 DB)
        // 2) Insert duplicate pending review-decisions
        // 3) Set latest_dispatch_id to the loser (broken pointer)
        {
            let conn = db.lock().unwrap();
            // Remove index to simulate pre-#116 DB state
            conn.execute_batch("DROP INDEX IF EXISTS idx_single_active_review_decision;")
                .unwrap();
            // Create two pending review-decisions (duplicate — legacy race)
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('rd-loser', 'card-s3', 'agent-1', 'review-decision', 'pending', 'RD Loser', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('rd-winner', 'card-s3', 'agent-1', 'review-decision', 'pending', 'RD Winner', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            // Point latest_dispatch_id to loser (broken pointer)
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = 'rd-loser' WHERE id = 'card-s3'",
                [],
            )
            .unwrap();
            // card_review_state with stale NULL pending_dispatch_id
            conn.execute(
                "INSERT INTO card_review_state (card_id, review_round, state, pending_dispatch_id, review_entered_at, updated_at) \
                 VALUES ('card-s3', 1, 'reviewing', NULL, datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        // Simulate restart: re-run schema::migrate which includes reconciliation
        {
            let conn = db.lock().unwrap();
            db::schema::migrate(&conn).unwrap();
        }

        // Verify reconciliation results:
        {
            let conn = db.lock().unwrap();

            // 1) Only 1 active review-decision should remain (duplicate cancelled)
            let active_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_dispatches \
                     WHERE kanban_card_id = 'card-s3' AND dispatch_type = 'review-decision' \
                     AND status IN ('pending', 'dispatched')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                active_count, 1,
                "reconciliation must leave exactly 1 active review-decision"
            );

            // 2) latest_dispatch_id should point to the surviving active dispatch
            let latest: String = conn
                .query_row(
                    "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-s3'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let survivor_status: String = conn
                .query_row(
                    "SELECT status FROM task_dispatches WHERE id = ?1",
                    [&latest],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                survivor_status == "pending" || survivor_status == "dispatched",
                "latest_dispatch_id must point to active dispatch, got status: {}",
                survivor_status
            );
        }
    }

    #[test]
    fn scenario_667_restart_recovery_reconciles_duplicate_review_dispatches() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-s667", "review");

        {
            let conn = db.lock().unwrap();
            conn.execute_batch("DROP INDEX IF EXISTS idx_single_active_review;")
                .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('review-loser', 'card-s667', 'agent-1', 'review', 'pending', 'Review Loser', datetime('now', '-1 minute'), datetime('now', '-1 minute'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('review-winner', 'card-s667', 'agent-1', 'review', 'pending', 'Review Winner', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = 'review-loser' WHERE id = 'card-s667'",
                [],
            )
            .unwrap();
        }

        {
            let conn = db.lock().unwrap();
            db::schema::migrate(&conn).unwrap();
        }

        {
            let conn = db.lock().unwrap();
            let active_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_dispatches \
                     WHERE kanban_card_id = 'card-s667' AND dispatch_type = 'review' \
                     AND status IN ('pending', 'dispatched')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                active_count, 1,
                "reconciliation must leave exactly 1 active review dispatch"
            );

            let loser_status: String = conn
                .query_row(
                    "SELECT status FROM task_dispatches WHERE id = 'review-loser'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                loser_status, "cancelled",
                "legacy duplicate review dispatch must be cancelled before the index is added"
            );

            let latest: String = conn
                .query_row(
                    "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-s667'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                latest, "review-winner",
                "latest_dispatch_id must be re-pointed to the surviving active review dispatch"
            );

            let unique_violation = conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('review-blocked', 'card-s667', 'agent-1', 'review', 'pending', 'Review Blocked', datetime('now'), datetime('now'))",
                [],
            );
            assert!(
                unique_violation.is_err(),
                "idx_single_active_review must block a second active review dispatch"
            );
        }
    }

    #[tokio::test]
    async fn scenario_251_boot_reconcile_backfills_missing_notify_outbox() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-251-outbox", "in_progress");
        seed_dispatch(
            &db,
            "dispatch-251-outbox",
            "card-251-outbox",
            "implementation",
            "pending",
        );

        let stats = crate::reconcile::reconcile_boot_runtime(Some(&db), &engine, None)
            .await
            .unwrap();
        assert_eq!(
            stats.missing_notify_outbox_backfilled, 1,
            "boot reconcile must backfill missing notify outbox rows"
        );

        let conn = db.lock().unwrap();
        let outbox_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dispatch_outbox \
                 WHERE dispatch_id = 'dispatch-251-outbox' AND action = 'notify'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            outbox_count, 1,
            "notify outbox row must exist after boot reconcile"
        );
    }

    #[tokio::test]
    async fn scenario_251_boot_reconcile_resets_broken_auto_queue_entries() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-251-aq-orphan", "in_progress");
        seed_card(&db, "card-251-aq-phantom", "in_progress");
        seed_card(&db, "card-251-aq-cancelled", "in_progress");
        seed_card(&db, "card-251-aq-completed", "in_progress");
        seed_card(&db, "card-251-aq-valid", "in_progress");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
                 VALUES ('run-251-aq', 'test-repo', 'agent-1', 'active')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
                 VALUES ('entry-251-aq-orphan', 'run-251-aq', 'card-251-aq-orphan', 'agent-1', 'dispatched', NULL, datetime('now', '-3 minutes'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
                 VALUES ('entry-251-aq-phantom', 'run-251-aq', 'card-251-aq-phantom', 'agent-1', 'dispatched', 'dispatch-251-aq-phantom', datetime('now', '-3 minutes'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title) \
                 VALUES ('dispatch-251-aq-cancelled', 'card-251-aq-cancelled', 'agent-1', 'implementation', 'cancelled', 'cancelled')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
                 VALUES ('entry-251-aq-cancelled', 'run-251-aq', 'card-251-aq-cancelled', 'agent-1', 'dispatched', 'dispatch-251-aq-cancelled', datetime('now', '-3 minutes'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title) \
                 VALUES ('dispatch-251-aq-completed', 'card-251-aq-completed', 'agent-1', 'implementation', 'completed', 'completed')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
                 VALUES ('entry-251-aq-completed', 'run-251-aq', 'card-251-aq-completed', 'agent-1', 'dispatched', 'dispatch-251-aq-completed', datetime('now', '-3 minutes'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title) \
                 VALUES ('dispatch-251-aq-valid', 'card-251-aq-valid', 'agent-1', 'implementation', 'dispatched', 'valid')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
                 VALUES ('entry-251-aq-valid', 'run-251-aq', 'card-251-aq-valid', 'agent-1', 'dispatched', 'dispatch-251-aq-valid', datetime('now'))",
                [],
            )
            .unwrap();
        }

        let stats = crate::reconcile::reconcile_boot_runtime(Some(&db), &engine, None)
            .await
            .unwrap();
        assert_eq!(
            stats.broken_auto_queue_entries_reset, 4,
            "boot reconcile must reset orphan/phantom/cancelled/completed auto-queue entries"
        );

        let conn = db.lock().unwrap();
        let orphan_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-251-aq-orphan'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let phantom_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-251-aq-phantom'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let cancelled_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-251-aq-cancelled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let completed_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-251-aq-completed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let valid_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-251-aq-valid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphan_status, "pending");
        assert_eq!(phantom_status, "pending");
        assert_eq!(cancelled_status, "pending");
        assert_eq!(completed_status, "pending");
        assert_eq!(valid_status, "dispatched");
    }

    #[test]
    fn scenario_969_pg_boot_reconcile_uses_startup_pool_without_pool_timeout_logs() {
        let (result, logs) = capture_policy_logs(|| {
            tokio::runtime::Runtime::new()
                .expect("integration test runtime")
                .block_on(async {
                    let test_pg = PgRecoveryTestDatabase::create().await;
                    // #1296: bypass connect_and_migrate(&config) here — that helper
                    // honors DATABASE_URL env, which the CI sets to the shared
                    // `postgres` database. If the test went through it, every
                    // PG-backed test would hit the shared DB, polluting kv_meta /
                    // kanban_cards.metadata between runs and causing the JS review
                    // loop guard to escalate after the first invocation. Build the
                    // pools directly against the per-test database URL so the
                    // boot reconcile path runs on isolated state.
                    let runtime_pool = crate::db::postgres::connect_test_pool_and_migrate(
                        &test_pg.database_url,
                        "integration high_risk_recovery runtime pool",
                    )
                    .await
                    .expect("connect runtime postgres pool");
                    let startup_pool = crate::db::postgres::connect_test_pool_and_migrate(
                        &test_pg.database_url,
                        "integration high_risk_recovery startup pool",
                    )
                    .await
                    .expect("connect startup postgres pool");

                    let sqlite = test_db();
                    seed_agent(&sqlite);
                    seed_card(&sqlite, "card-969-review", "review");

                    sqlx::query(
                        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
                         VALUES ('agent-1', 'Test Agent', '111', '222')
                         ON CONFLICT (id) DO UPDATE SET
                             name = EXCLUDED.name,
                             discord_channel_id = EXCLUDED.discord_channel_id,
                             discord_channel_alt = EXCLUDED.discord_channel_alt",
                    )
                    .execute(&startup_pool)
                    .await
                    .expect("seed postgres agent");
                    sqlx::query(
                        "INSERT INTO kanban_cards (
                            id,
                            title,
                            status,
                            assigned_agent_id,
                            created_at,
                            updated_at
                         ) VALUES (
                            'card-969-review',
                            'Test Card',
                            'review',
                            'agent-1',
                            NOW(),
                            NOW()
                         )
                         ON CONFLICT (id) DO UPDATE SET
                             status = EXCLUDED.status,
                             assigned_agent_id = EXCLUDED.assigned_agent_id,
                             updated_at = EXCLUDED.updated_at",
                    )
                    .execute(&startup_pool)
                    .await
                    .expect("seed postgres review card");
                    sqlx::query(
                        "INSERT INTO dispatch_outbox (dispatch_id, action, status)
                         VALUES ('dispatch-969', 'notify', 'processing')
                         ON CONFLICT (dispatch_id, action) WHERE action IN ('notify', 'followup')
                         DO UPDATE SET status = EXCLUDED.status",
                    )
                    .execute(&startup_pool)
                    .await
                    .expect("seed stale outbox row");
                    sqlx::query(
                        "INSERT INTO kv_meta (key, value)
                         VALUES ('dispatch_reserving:dispatch-969', 'agent-1')
                         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                    )
                    .execute(&startup_pool)
                    .await
                    .expect("seed stale reservation");

                    let _runtime_conn = runtime_pool
                        .acquire()
                        .await
                        .expect("exhaust single runtime pool connection");

                    let engine = test_engine_with_pg(startup_pool.clone());
                    let stats = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        crate::reconcile::reconcile_boot_runtime(
                            Some(&sqlite),
                            &engine,
                            Some(&startup_pool),
                        ),
                    )
                    .await
                    .expect("boot reconcile must not hang")
                    .expect("boot reconcile succeeds with startup pool");

                    drop(_runtime_conn);
                    crate::db::postgres::close_test_pool(
                        startup_pool,
                        "integration high_risk_recovery startup pool",
                    )
                    .await
                    .expect("close startup pool");
                    crate::db::postgres::close_test_pool(
                        runtime_pool,
                        "integration high_risk_recovery runtime pool",
                    )
                    .await
                    .expect("close runtime pool");
                    test_pg.drop().await;
                    stats
                })
        });

        let stats = result;
        assert_eq!(
            stats.stale_processing_outbox_reset, 1,
            "boot reconcile must reset stale outbox rows through the startup pool"
        );
        assert_eq!(
            stats.stale_dispatch_reservations_cleared, 1,
            "boot reconcile must clear stale reservations through the startup pool"
        );
        assert_eq!(
            stats.missing_review_dispatches_refired, 1,
            "boot reconcile must re-fire OnReviewEnter through the startup pool"
        );
        assert!(
            !logs.contains("pool timed out while waiting for an open connection"),
            "startup reconcile must avoid steady-state pool timeout logs; captured logs:\n{logs}"
        );
    }

    // TODO(#850 follow-up): rewrite as PG fixture test.
    // After #850 the OnReviewEnter hook's `agentdesk.dispatch.create` call
    // requires a PG pool (rusqlite dispatch creation path deleted). This
    // test uses only the rusqlite `test_db()` fixture so the hook returns
    // `postgres pool required`. Rewriting requires bringing up a PG pool
    // inside the integration test harness — out of scope for #850.
    #[tokio::test]
    #[ignore = "rusqlite dispatch path deleted in #850; needs PG fixture rewrite"]
    async fn scenario_251_boot_reconcile_refires_missing_review_dispatch() {
        let (_repo, _repo_guard) = setup_test_repo();
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-251-review", "review");

        let stats = crate::reconcile::reconcile_boot_runtime(Some(&db), &engine, None)
            .await
            .unwrap();
        assert_eq!(
            stats.missing_review_dispatches_refired, 1,
            "boot reconcile must re-fire OnReviewEnter for review cards missing dispatch"
        );

        let conn = db.lock().unwrap();
        let review_dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-251-review' \
                   AND dispatch_type = 'review' \
                   AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_dispatch_count, 1,
            "review card must have one active review dispatch after boot reconcile"
        );
    }
}

mod outbox_boundary {
    use super::*;
    use crate::server::routes::dispatches::{OutboxNotifier, process_outbox_batch};

    /// Mock Discord transport that records calls and optionally fails.
    struct MockNotifier {
        calls: std::sync::Mutex<Vec<MockCall>>,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum MockCall {
        Notify {
            agent_id: String,
            dispatch_id: String,
        },
        Followup {
            dispatch_id: String,
        },
        StatusReaction {
            dispatch_id: String,
        },
    }

    impl MockNotifier {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn call_log(&self) -> Vec<MockCall> {
            self.calls.lock().unwrap().clone()
        }

        fn notify_count(&self) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| matches!(c, MockCall::Notify { .. }))
                .count()
        }
    }

    impl OutboxNotifier for MockNotifier {
        async fn notify_dispatch(
            &self,
            _db: Option<crate::db::Db>,
            agent_id: String,
            _title: String,
            _card_id: String,
            dispatch_id: String,
        ) -> Result<
            crate::server::routes::dispatches::discord_delivery::DispatchNotifyDeliveryResult,
            String,
        > {
            self.calls.lock().unwrap().push(MockCall::Notify {
                agent_id,
                dispatch_id: dispatch_id.clone(),
            });
            Ok(
                crate::server::routes::dispatches::discord_delivery::DispatchNotifyDeliveryResult::success(
                    dispatch_id,
                    "notify",
                    "mock notifier sent",
                ),
            )
        }

        async fn handle_followup(
            &self,
            _db: Option<crate::db::Db>,
            dispatch_id: String,
        ) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(MockCall::Followup { dispatch_id });
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
                .push(MockCall::StatusReaction { dispatch_id });
            Ok(())
        }
    }

    fn seed_outbox(db: &db::Db, dispatch_id: &str, action: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title, status) \
             VALUES (?1, ?2, 'agent-1', 'card-160', 'Test', 'pending')",
            libsql_rusqlite::params![dispatch_id, action],
        )
        .unwrap();
    }

    fn outbox_status(db: &db::Db, dispatch_id: &str) -> Vec<String> {
        let conn = db.lock().unwrap();
        // Exclude auto-generated status_reaction entries (#513) — those are
        // side-effects of dispatch status transitions, not the entries under test.
        let mut stmt = conn
            .prepare(
                "SELECT status FROM dispatch_outbox \
                 WHERE dispatch_id = ?1 AND action != 'status_reaction' \
                 ORDER BY id",
            )
            .unwrap();
        stmt.query_map([dispatch_id], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    fn outbox_status_for_action(db: &db::Db, dispatch_id: &str, action: &str) -> Vec<String> {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT status FROM dispatch_outbox
                 WHERE dispatch_id = ?1 AND action = ?2
                 ORDER BY id",
            )
            .unwrap();
        stmt.query_map(libsql_rusqlite::params![dispatch_id, action], |row| {
            row.get(0)
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    fn has_reconcile_marker(db: &db::Db, dispatch_id: &str) -> bool {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) > 0 FROM kv_meta WHERE key = ?1",
            [&format!("reconcile_dispatch:{dispatch_id}")],
            |row| row.get(0),
        )
        .unwrap_or(false)
    }

    fn get_dispatch_result_json(db: &db::Db, dispatch_id: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT result FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
    }

    /// Scenario 160-1: DB commit → outbox worker delivers exactly 1 notification.
    ///
    /// Exercises `process_outbox_batch` with MockNotifier to verify:
    /// - Outbox entry transitions pending → processing → done
    /// - Mock Discord transport receives exactly 1 notify call
    #[tokio::test]
    async fn scenario_160_1_outbox_batch_delivers_exactly_once() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-160", "ready");
        seed_dispatch(&db, "d-160-1", "card-160", "implementation", "pending");
        seed_outbox(&db, "d-160-1", "notify");

        let mock = MockNotifier::new();

        // Run one batch — this exercises the real process_outbox_batch code path
        let processed = process_outbox_batch(&db, &mock).await;

        assert_eq!(processed, 1, "Batch should process exactly 1 entry");
        assert_eq!(
            mock.notify_count(),
            1,
            "Mock should receive exactly 1 notify call"
        );
        assert_eq!(
            mock.call_log(),
            vec![MockCall::Notify {
                agent_id: "agent-1".into(),
                dispatch_id: "d-160-1".into(),
            }]
        );

        // #750: announce bot reaction writer retired — notify success no
        // longer chain-enqueues a follow-up status_reaction outbox row.
        assert_eq!(
            outbox_status_for_action(&db, "d-160-1", "notify"),
            vec!["done"]
        );
        assert!(
            outbox_status_for_action(&db, "d-160-1", "status_reaction").is_empty(),
            "#750: notify success must NOT chain-enqueue a status_reaction row"
        );
        assert_eq!(
            get_dispatch_status(&db, "d-160-1"),
            "dispatched",
            "successful notify must transition pending dispatch to dispatched"
        );

        // Second batch: nothing pending, no additional notifier calls.
        let processed2 = process_outbox_batch(&db, &mock).await;
        assert_eq!(
            processed2, 0,
            "#750: no pending entries after single notify drain (no status_reaction chained)"
        );
        assert_eq!(
            mock.notify_count(),
            1,
            "No additional notify calls after dispatch"
        );
    }

    /// Scenario 160-2: Recovery API failure → DB fallback completes dispatch
    /// and sets reconciliation marker for onTick hook chain.
    ///
    /// Simulates the turn_bridge fallback path: when finalize_dispatch fails,
    /// the system falls back to direct DB UPDATE + reconcile marker.
    #[tokio::test]
    async fn scenario_160_2_recovery_fallback_completes_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-160r", "in_progress");
        seed_dispatch(&db, "d-160r", "card-160r", "implementation", "pending");
        seed_assistant_response_for_dispatch(&db, "d-160r", "completed during downtime");

        // Step 1: Verify finalize_dispatch works on the happy path
        let result = dispatch::finalize_dispatch(
            &db,
            &engine,
            "d-160r",
            "recovery_completed_during_downtime",
            Some(&serde_json::json!({"agent_response_present": true})),
        );
        assert!(
            result.is_ok(),
            "finalize_dispatch happy path should succeed"
        );
        assert_eq!(get_dispatch_status(&db, "d-160r"), "completed");

        // Step 2: Simulate the fallback path — when finalize_dispatch fails,
        // turn_bridge does a direct DB UPDATE + reconciliation marker.
        // This is the exact SQL from turn_bridge.rs:605-617.
        seed_card(&db, "card-160r2", "in_progress");
        seed_dispatch(&db, "d-160r2", "card-160r2", "implementation", "pending");

        // Execute the fallback SQL (mirrors turn_bridge.rs separate_conn path)
        {
            let fallback_conn = db.separate_conn().unwrap();
            let changed = fallback_conn
                .execute(
                    "UPDATE task_dispatches SET status = 'completed', \
                     result = '{\"completion_source\":\"turn_bridge_db_fallback\",\"needs_reconcile\":true,\"agent_response_present\":true}', \
                     updated_at = datetime('now') WHERE id = ?1 AND status IN ('pending', 'dispatched')",
                    ["d-160r2"],
                )
                .unwrap();
            assert_eq!(changed, 1, "Fallback UPDATE should affect 1 row");

            fallback_conn
                .execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    libsql_rusqlite::params!["reconcile_dispatch:d-160r2", "d-160r2"],
                )
                .unwrap();
        }

        // Verify fallback outcome
        assert_eq!(get_dispatch_status(&db, "d-160r2"), "completed");
        assert!(
            has_reconcile_marker(&db, "d-160r2"),
            "Reconciliation marker must exist for onTick hook chain"
        );
        let result_json = get_dispatch_result_json(&db, "d-160r2").unwrap();
        assert!(
            result_json.contains("turn_bridge_db_fallback"),
            "Result should record fallback completion source"
        );
        assert!(
            result_json.contains("needs_reconcile"),
            "Result should flag reconciliation needed"
        );
    }

    /// Scenario 160-3: Multiple outbox entries processed in FIFO order via
    /// actual process_outbox_batch — mock records call sequence.
    ///
    /// Verifies: persisted queue order → catch-up order → no order reversal.
    #[tokio::test]
    async fn scenario_160_3_outbox_fifo_ordering_through_worker() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-160o", "ready");
        seed_dispatch(&db, "d-160o-a", "card-160o", "implementation", "pending");
        seed_dispatch(&db, "d-160o-b", "card-160o", "implementation", "pending");
        seed_dispatch(&db, "d-160o-c", "card-160o", "implementation", "pending");

        seed_outbox(&db, "d-160o-a", "notify");
        seed_outbox(&db, "d-160o-b", "notify");
        seed_outbox(&db, "d-160o-c", "notify");

        let mock = MockNotifier::new();
        let processed = process_outbox_batch(&db, &mock).await;

        assert_eq!(processed, 3);

        // Verify FIFO order via mock call log
        let ids: Vec<String> = mock
            .call_log()
            .iter()
            .filter_map(|c| match c {
                MockCall::Notify { dispatch_id, .. } => Some(dispatch_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec!["d-160o-a", "d-160o-b", "d-160o-c"],
            "Order reversal detected — outbox must process in id ASC (FIFO)"
        );

        // #750: notify rows are done; no status_reaction chain row is
        // enqueued since the announce-bot reaction writer is retired.
        for id in ["d-160o-a", "d-160o-b", "d-160o-c"] {
            assert_eq!(outbox_status_for_action(&db, id, "notify"), vec!["done"]);
            assert!(
                outbox_status_for_action(&db, id, "status_reaction").is_empty(),
                "#750: notify success must not chain a status_reaction row"
            );
        }
        assert_eq!(get_dispatch_status(&db, "d-160o-a"), "dispatched");
        assert_eq!(get_dispatch_status(&db, "d-160o-b"), "dispatched");
        assert_eq!(get_dispatch_status(&db, "d-160o-c"), "dispatched");
    }

    /// Scenario 160-4: Duplicate outbox entries for the same dispatch.
    /// The two-phase delivery guard (dispatch_reserving/dispatch_notified) lives in
    /// send_dispatch_to_discord, not in process_outbox_batch, so with MockNotifier
    /// both entries call the notifier. In production, RealOutboxNotifier delegates
    /// to send_dispatch_to_discord which deduplicates via the two-phase marker.
    /// Both entries transition to 'done'.
    #[tokio::test]
    async fn scenario_160_4_outbox_processes_all_entries_including_duplicates() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-160d", "ready");
        seed_dispatch(&db, "d-160d", "card-160d", "implementation", "pending");

        // Duplicate notify insertions must collapse to a single durable row.
        {
            let conn = db.lock().unwrap();
            let inserted_first = conn
                .execute(
                    "INSERT OR IGNORE INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title, status) \
                     VALUES ('d-160d', 'notify', 'agent-1', 'card-160d', 'Test', 'pending')",
                    [],
                )
                .unwrap();
            let inserted_second = conn
                .execute(
                    "INSERT OR IGNORE INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title, status) \
                     VALUES ('d-160d', 'notify', 'agent-1', 'card-160d', 'Test', 'pending')",
                    [],
                )
                .unwrap();
            assert_eq!(inserted_first, 1, "first notify row must insert");
            assert_eq!(inserted_second, 0, "duplicate notify row must be ignored");
        }

        let mock = MockNotifier::new();
        let processed = process_outbox_batch(&db, &mock).await;

        // Worker processes the single retained notify row.
        assert_eq!(processed, 1, "Worker should process one deduplicated entry");
        assert_eq!(
            mock.notify_count(),
            1,
            "deduplicated notify rows must call the notifier only once"
        );

        // #750: the retained notify row is done; no status_reaction chain.
        assert_eq!(
            outbox_status_for_action(&db, "d-160d", "notify"),
            vec!["done"]
        );
        assert!(
            outbox_status_for_action(&db, "d-160d", "status_reaction").is_empty(),
            "#750: no status_reaction row chained after notify"
        );
    }

    /// Scenario 160-5: Mixed actions (notify + followup) are dispatched to the
    /// correct notifier methods through process_outbox_batch.
    #[tokio::test]
    async fn scenario_160_5_mixed_actions_route_correctly() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-160m", "ready");
        seed_dispatch(&db, "d-160m-n", "card-160m", "implementation", "pending");
        seed_dispatch(&db, "d-160m-f", "card-160m", "implementation", "pending");

        seed_outbox(&db, "d-160m-n", "notify");
        // Insert followup entry manually (seed_outbox hardcodes card_id)
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO dispatch_outbox (dispatch_id, action, status) \
                 VALUES ('d-160m-f', 'followup', 'pending')",
                [],
            )
            .unwrap();
        }

        let mock = MockNotifier::new();
        let processed = process_outbox_batch(&db, &mock).await;

        assert_eq!(processed, 2);
        assert_eq!(
            mock.call_log(),
            vec![
                MockCall::Notify {
                    agent_id: "agent-1".into(),
                    dispatch_id: "d-160m-n".into(),
                },
                MockCall::Followup {
                    dispatch_id: "d-160m-f".into(),
                },
            ]
        );
    }

    /// Scenario 160-6: Notify success must not rewrite terminal dispatch states.
    ///
    /// Verifies the `status = 'pending'` guard keeps completed dispatches
    /// terminal while still draining the outbox entry successfully.
    #[tokio::test]
    async fn scenario_160_6_notify_success_keeps_completed_dispatch_terminal() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-160c", "done");
        seed_dispatch(&db, "d-160c", "card-160c", "implementation", "completed");
        seed_outbox(&db, "d-160c", "notify");

        let mock = MockNotifier::new();
        let processed = process_outbox_batch(&db, &mock).await;

        assert_eq!(processed, 1);
        assert_eq!(
            mock.notify_count(),
            0,
            "completed dispatches must be drained without re-delivery"
        );
        assert_eq!(outbox_status(&db, "d-160c"), vec!["done"]);
        assert_eq!(
            get_dispatch_status(&db, "d-160c"),
            "completed",
            "terminal dispatch status must stay completed after notify suppression"
        );
    }
}

mod delayed_worker {
    use super::*;

    #[test]
    #[ignore = "SQLite message_outbox runtime path removed in #868; delayed-worker notification coverage needs PG fixtures."]
    fn scenario_421_deadlock_recent_output_extends_watchdog() {
        let runtime_root = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootOverride::new(runtime_root.path());
        let policies_dir = setup_timeouts_policy_dir();
        let db = test_db();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        let session_key = "host:tmux-421-recent";

        seed_agent(&db);
        set_kv(&db, "deadlock_manager_channel_id", "999");
        set_kv(&db, "server_port", "8791");
        set_kv(
            &db,
            &format!("deadlock_check:{session_key}"),
            r#"{"count":2,"ts":0}"#,
        );

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, last_heartbeat, created_at) \
                 VALUES (?1, 'agent-1', 'codex', 'working', datetime('now', '-31 minutes'), datetime('now', '-90 minutes'))",
                [session_key],
            )
            .unwrap();
        }

        write_codex_inflight(
            runtime_root.path(),
            "111",
            &relative_local_time(90),
            &relative_local_time(1),
            session_key,
            "tmux-421-recent",
            None,
        );

        engine
            .try_fire_hook_by_name("OnTick30s", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            kv_value(&db, &format!("deadlock_check:{session_key}")),
            None,
            "recent output should clear the deadlock counter"
        );
        assert_eq!(kv_value(&db, "test_http_count").as_deref(), Some("1"));

        let http_last: serde_json::Value =
            serde_json::from_str(&kv_value(&db, "test_http_last").unwrap()).unwrap();
        assert_eq!(http_last["body"]["extend_secs"], 1800);
        assert!(
            http_last["url"]
                .as_str()
                .unwrap_or("")
                .ends_with("/api/turns/111/extend-timeout"),
            "watchdog extension must target the inflight channel"
        );

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "channel:999");
        assert!(messages[0].1.contains("정상 진행 확인, +30분 연장"));
        assert!(!messages[0].1.contains("watchdog 연장 실패"));
    }

    #[test]
    #[ignore = "SQLite message_outbox runtime path removed in #868; delayed-worker notification coverage needs PG fixtures."]
    fn scenario_421_deadlock_stale_output_only_marks_suspected_deadlock() {
        let runtime_root = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootOverride::new(runtime_root.path());
        let policies_dir = setup_timeouts_policy_dir();
        let db = test_db();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        let session_key = "host:tmux-421-stale";

        seed_agent(&db);
        set_kv(&db, "deadlock_manager_channel_id", "999");
        set_kv(&db, "server_port", "8791");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, last_heartbeat, created_at) \
                 VALUES (?1, 'agent-1', 'codex', 'working', datetime('now', '-31 minutes'), datetime('now', '-90 minutes'))",
                [session_key],
            )
            .unwrap();
        }

        write_codex_inflight(
            runtime_root.path(),
            "111",
            &relative_local_time(90),
            &relative_local_time(31),
            session_key,
            "tmux-421-stale",
            None,
        );

        engine
            .try_fire_hook_by_name("OnTick30s", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let counter: serde_json::Value =
            serde_json::from_str(&kv_value(&db, &format!("deadlock_check:{session_key}")).unwrap())
                .unwrap();
        assert_eq!(counter["count"], 1);
        assert!(kv_value(&db, "test_http_count").is_none());

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "channel:999");
        assert!(messages[0].1.contains("[Deadlock 의심]"));
        assert!(messages[0].1.contains("무응답: 30분 (연장 1/3)"));
    }

    #[test]
    #[ignore = "SQLite message_outbox runtime path removed in #868; delayed-worker notification coverage needs PG fixtures."]
    fn scenario_421_long_turn_alerts_start_at_30_minutes() {
        let runtime_root = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootOverride::new(runtime_root.path());
        let policies_dir = setup_timeouts_policy_dir();
        let db = test_db();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        let session_key = "host:tmux-421-long";

        seed_agent(&db);
        set_kv(&db, "deadlock_manager_channel_id", "999");
        set_kv(&db, "server_port", "8791");

        write_codex_inflight(
            runtime_root.path(),
            "111",
            &relative_local_time(20),
            &relative_local_time(1),
            session_key,
            "tmux-421-long",
            None,
        );
        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);
        assert!(
            message_outbox_rows(&db).is_empty(),
            "20-minute turn must not trigger the removed 15-minute alert tier"
        );

        write_codex_inflight(
            runtime_root.path(),
            "111",
            &relative_local_time(31),
            &relative_local_time(1),
            session_key,
            "tmux-421-long",
            None,
        );
        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].1.contains("경과: 31분 (30분 단계)"));
        assert!(messages[0].1.contains("watchdog: +60분 연장 요청 완료"));
        assert_eq!(kv_value(&db, "test_http_count").as_deref(), Some("1"));
        let http_last: serde_json::Value =
            serde_json::from_str(&kv_value(&db, "test_http_last").unwrap()).unwrap();
        assert_eq!(http_last["body"]["extend_secs"], 3600);
        assert!(
            http_last["url"]
                .as_str()
                .unwrap_or("")
                .ends_with("/api/turns/111/extend-timeout"),
            "30-minute long-turn tier must extend beyond the original 60-minute watchdog"
        );

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);
        assert_eq!(
            message_outbox_rows(&db).len(),
            1,
            "same tier must not alert twice"
        );
        assert_eq!(
            kv_value(&db, "test_http_count").as_deref(),
            Some("1"),
            "same tier must not extend twice"
        );

        let old_extension_ms = chrono::Utc::now().timestamp_millis() - 21 * 60 * 1000;
        set_kv(
            &db,
            "long_turn_watchdog_extension:codex:111",
            &old_extension_ms.to_string(),
        );
        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);
        assert_eq!(
            message_outbox_rows(&db).len(),
            1,
            "extension cooldown retry must not duplicate the same alert tier"
        );
        assert_eq!(
            kv_value(&db, "test_http_count").as_deref(),
            Some("2"),
            "long-turn extension must be cadence-based, not alert-tier-only"
        );

        write_codex_inflight(
            runtime_root.path(),
            "111",
            &relative_local_time(61),
            &relative_local_time(1),
            session_key,
            "tmux-421-long",
            None,
        );
        set_kv(
            &db,
            "long_turn_watchdog_extension:codex:111",
            &old_extension_ms.to_string(),
        );
        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 2);
        assert!(messages[1].1.contains("경과: 61분 (60분 단계)"));
        assert!(messages[1].1.contains("watchdog: +60분 연장 요청 완료"));
        assert_eq!(kv_value(&db, "test_http_count").as_deref(), Some("3"));
        let http_last: serde_json::Value =
            serde_json::from_str(&kv_value(&db, "test_http_last").unwrap()).unwrap();
        assert_eq!(http_last["body"]["extend_secs"], 3600);
        assert!(
            http_last["url"]
                .as_str()
                .unwrap_or("")
                .ends_with("/api/turns/111/extend-timeout"),
            "60-minute long-turn tier must extend the inflight channel watchdog"
        );

        write_codex_inflight(
            runtime_root.path(),
            "222",
            &relative_local_time(31),
            &relative_local_time(20),
            "host:tmux-421-long-stale",
            "tmux-421-long-stale",
            None,
        );
        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 3);
        assert!(messages[2].1.contains("경과: 31분 (30분 단계)"));
        assert!(messages[2].1.contains("최근 progress 없음"));
        assert!(messages[2].1.contains("연장 안 함"));
        assert_eq!(
            kv_value(&db, "test_http_count").as_deref(),
            Some("3"),
            "20-minute-old long-turn progress must alert without extending the watchdog"
        );

        {
            let conn = db.lock().unwrap();
            conn.execute("DELETE FROM kv_meta WHERE key = ?1", ["server_port"])
                .unwrap();
        }
        write_codex_inflight(
            runtime_root.path(),
            "333",
            &relative_local_time(31),
            &relative_local_time(1),
            "host:tmux-421-long-retry",
            "tmux-421-long-retry",
            None,
        );
        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 4);
        assert!(messages[3].1.contains("연장 실패"));
        assert!(messages[3].1.contains("server_port missing"));
        assert_eq!(
            kv_value(&db, "test_http_count").as_deref(),
            Some("3"),
            "extension failure before HTTP must not create a cooldown record"
        );

        set_kv(&db, "server_port", "8791");
        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);
        assert_eq!(
            message_outbox_rows(&db).len(),
            4,
            "extension retry after failure must not duplicate the same alert tier"
        );
        assert_eq!(
            kv_value(&db, "test_http_count").as_deref(),
            Some("4"),
            "failed extension must retry before the next alert tier"
        );

        write_codex_inflight(
            runtime_root.path(),
            "111",
            &relative_local_time(121),
            &relative_local_time(1),
            session_key,
            "tmux-421-long",
            None,
        );
        set_kv(
            &db,
            "long_turn_watchdog_extension:codex:111",
            &old_extension_ms.to_string(),
        );
        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 5);
        assert!(messages[4].1.contains("경과: 121분 (120분 단계)"));
    }
}

mod idle_session_cleanup {
    use super::*;

    /// PG seed: ensure a single test agent (`agent-1`) exists.
    async fn seed_agent_pg(pool: &sqlx::PgPool) {
        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Test Agent', '111', '222') \
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("seed agent-1 in postgres");
    }

    /// PG kv_meta upsert sibling for the legacy `set_kv` SQLite helper.
    async fn set_kv_pg(pool: &sqlx::PgPool, key: &str, value: &str) {
        sqlx::query(
            "INSERT INTO kv_meta (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(pool)
        .await
        .expect("upsert kv_meta in postgres");
    }

    /// PG kv_meta read sibling for the legacy `kv_value` SQLite helper.
    async fn kv_value_pg(pool: &sqlx::PgPool, key: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
            .bind(key)
            .fetch_optional(pool)
            .await
            .expect("query kv_meta from postgres")
    }

    /// PG message_outbox sibling for the legacy `message_outbox_rows` helper.
    /// Returns rows ordered by ascending id, as `(target, content)` tuples.
    async fn message_outbox_rows_pg(pool: &sqlx::PgPool) -> Vec<(String, String)> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT target, content FROM message_outbox ORDER BY id ASC",
        )
        .fetch_all(pool)
        .await
        .expect("read message_outbox from postgres")
    }

    /// Seed an idle session row in postgres. Uses TIMESTAMPTZ arithmetic
    /// (`NOW() - INTERVAL '<n> minutes'`) instead of SQLite's
    /// `datetime('now', '-N minutes')`.
    async fn seed_idle_session_pg(
        pool: &sqlx::PgPool,
        session_key: &str,
        active_dispatch_id: Option<&str>,
        idle_minutes: i32,
    ) {
        let sql = format!(
            "INSERT INTO sessions
                 (session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at)
             VALUES (
                 $1, 'agent-1', 'codex', 'idle', $2,
                 NOW() - INTERVAL '{idle_minutes} minutes',
                 NOW() - INTERVAL '{idle_minutes} minutes'
             )"
        );
        sqlx::query(&sql)
            .bind(session_key)
            .bind(active_dispatch_id)
            .execute(pool)
            .await
            .expect("seed idle session in postgres");
    }

    /// Bump an existing session's last_heartbeat / created_at to `idle_minutes`
    /// ago. Used by the safety-TTL scenario.
    async fn age_session_pg(pool: &sqlx::PgPool, session_key: &str, idle_minutes: i32) {
        let sql = format!(
            "UPDATE sessions
                SET last_heartbeat = NOW() - INTERVAL '{idle_minutes} minutes',
                    created_at    = NOW() - INTERVAL '{idle_minutes} minutes'
              WHERE session_key = $1"
        );
        sqlx::query(&sql)
            .bind(session_key)
            .execute(pool)
            .await
            .expect("age session in postgres");
    }

    // #1342: migrated to PG fixtures because the timeouts.js policy + the JS
    // bridge db.* now route exclusively through PG once the engine is built
    // with a pg_pool. The legacy SQLite `set_kv` / `kv_value` /
    // `message_outbox_rows` helpers no longer observe the runtime state.
    #[tokio::test]
    async fn scenario_492_idle_session_without_active_dispatch_force_kills_once_after_60_minutes() {
        let pg_db = IntegrationPgDatabase::create().await;
        let pool = pg_db.migrate().await;
        let policies_dir = setup_timeouts_policy_dir();
        let db = test_db();
        let engine = test_engine_with_pg_and_dir(pool.clone(), policies_dir.path());
        let session_key = "host:idle-492-no-dispatch";

        seed_agent_pg(&pool).await;
        set_kv_pg(&pool, "server_port", "8791").await;
        seed_idle_session_pg(&pool, session_key, None, 181).await;

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            kv_value_pg(&pool, "test_http_count").await.as_deref(),
            Some("1")
        );

        let http_last: serde_json::Value = serde_json::from_str(
            &kv_value_pg(&pool, "test_http_last")
                .await
                .expect("test_http_last must be recorded by force-kill stub"),
        )
        .unwrap();
        let url = http_last["url"].as_str().unwrap_or("");
        assert!(url.contains("/api/sessions/"));
        assert!(url.contains("host%3Aidle-492-no-dispatch"));
        assert!(url.ends_with("/force-kill"));
        assert!(
            message_outbox_rows_pg(&pool).await.is_empty(),
            "idle force-kill policy path must not enqueue a duplicate notify alert"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn scenario_492_idle_session_with_active_dispatch_uses_180_minute_safety_ttl() {
        let pg_db = IntegrationPgDatabase::create().await;
        let pool = pg_db.migrate().await;
        let policies_dir = setup_timeouts_policy_dir();
        let db = test_db();
        let engine = test_engine_with_pg_and_dir(pool.clone(), policies_dir.path());
        let session_key = "host:idle-492-active-dispatch";

        seed_agent_pg(&pool).await;
        set_kv_pg(&pool, "server_port", "8791").await;
        seed_idle_session_pg(&pool, session_key, Some("dispatch-492"), 61).await;

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            kv_value_pg(&pool, "test_http_count").await,
            None,
            "active dispatch rows must not be reaped by the 60-minute idle TTL"
        );
        assert!(message_outbox_rows_pg(&pool).await.is_empty());

        age_session_pg(&pool, session_key, 181).await;

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            kv_value_pg(&pool, "test_http_count").await.as_deref(),
            Some("1")
        );

        let http_last: serde_json::Value = serde_json::from_str(
            &kv_value_pg(&pool, "test_http_last")
                .await
                .expect("test_http_last must be recorded by safety-TTL force-kill"),
        )
        .unwrap();
        let url = http_last["url"].as_str().unwrap_or("");
        assert!(url.contains("host%3Aidle-492-active-dispatch"));
        assert!(url.ends_with("/force-kill"));
        assert!(
            message_outbox_rows_pg(&pool).await.is_empty(),
            "idle safety TTL policy path must not enqueue a duplicate notify alert"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn scenario_632_idle_session_force_kill_response_with_dead_tmux_stays_silent() {
        let pg_db = IntegrationPgDatabase::create().await;
        let pool = pg_db.migrate().await;
        let policies_dir = setup_timeouts_policy_dir();
        let db = test_db();
        let engine = test_engine_with_pg_and_dir(pool.clone(), policies_dir.path());
        let session_key = "host:idle-632-dead-tmux";

        seed_agent_pg(&pool).await;
        set_kv_pg(&pool, "server_port", "8791").await;
        set_kv_pg(&pool, "test_force_kill_tmux_killed", "false").await;
        seed_idle_session_pg(&pool, session_key, None, 181).await;

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            kv_value_pg(&pool, "test_http_count").await.as_deref(),
            Some("1")
        );

        let http_last: serde_json::Value = serde_json::from_str(
            &kv_value_pg(&pool, "test_http_last")
                .await
                .expect("test_http_last must be recorded by dead-tmux force-kill"),
        )
        .unwrap();
        let url = http_last["url"].as_str().unwrap_or("");
        assert!(url.contains("host%3Aidle-632-dead-tmux"));
        assert!(url.ends_with("/force-kill"));
        assert!(
            message_outbox_rows_pg(&pool).await.is_empty(),
            "idle cleanup must stay silent when force-kill reports tmux_killed=false"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
