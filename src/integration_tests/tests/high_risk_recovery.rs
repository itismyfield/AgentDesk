//! High-risk recovery lane for restart/reconcile/outbox boundary scenarios.
//!
//! Run with `cargo test --bin agentdesk high_risk_recovery::`.

use super::*;

mod failure_recovery {
    use super::*;

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
    fn scenario_251_boot_reconcile_backfills_missing_notify_outbox() {
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

        let stats = crate::reconcile::reconcile_boot_runtime(&db, &engine).unwrap();
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

    #[test]
    fn scenario_251_boot_reconcile_resets_broken_auto_queue_entries() {
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

        let stats = crate::reconcile::reconcile_boot_runtime(&db, &engine).unwrap();
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
    fn scenario_251_boot_reconcile_refires_missing_review_dispatch() {
        let (_repo, _repo_guard) = setup_test_repo();
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-251-review", "review");

        let stats = crate::reconcile::reconcile_boot_runtime(&db, &engine).unwrap();
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
            _db: crate::db::Db,
            agent_id: String,
            _title: String,
            _card_id: String,
            dispatch_id: String,
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push(MockCall::Notify {
                agent_id,
                dispatch_id,
            });
            Ok(())
        }

        async fn handle_followup(
            &self,
            _db: crate::db::Db,
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
            _db: crate::db::Db,
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
            rusqlite::params![dispatch_id, action],
        )
        .unwrap();
    }

    fn outbox_status(db: &db::Db, dispatch_id: &str) -> Vec<String> {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT status FROM dispatch_outbox WHERE dispatch_id = ?1 ORDER BY id")
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
        stmt.query_map(rusqlite::params![dispatch_id, action], |row| row.get(0))
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

        // Verify notify row is done and a follow-up status sync row is queued.
        assert_eq!(
            outbox_status_for_action(&db, "d-160-1", "notify"),
            vec!["done"]
        );
        assert_eq!(
            outbox_status_for_action(&db, "d-160-1", "status_reaction"),
            vec!["pending"]
        );
        assert_eq!(
            get_dispatch_status(&db, "d-160-1"),
            "dispatched",
            "successful notify must transition pending dispatch to dispatched"
        );

        // Second batch drains the queued status reaction.
        let processed2 = process_outbox_batch(&db, &mock).await;
        assert_eq!(
            processed2, 1,
            "status reaction should be processed on next drain"
        );
        assert_eq!(
            mock.notify_count(),
            1,
            "No additional notify calls after dispatch"
        );
        assert!(
            mock.call_log().contains(&MockCall::StatusReaction {
                dispatch_id: "d-160-1".into(),
            }),
            "status reaction must be queued after notify success"
        );
        assert_eq!(
            outbox_status_for_action(&db, "d-160-1", "status_reaction"),
            vec!["done"]
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
                    rusqlite::params!["reconcile_dispatch:d-160r2", "d-160r2"],
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

        // Notify rows are done and each dispatch gets a queued status sync.
        assert_eq!(
            outbox_status_for_action(&db, "d-160o-a", "notify"),
            vec!["done"]
        );
        assert_eq!(
            outbox_status_for_action(&db, "d-160o-a", "status_reaction"),
            vec!["pending"]
        );
        assert_eq!(
            outbox_status_for_action(&db, "d-160o-b", "notify"),
            vec!["done"]
        );
        assert_eq!(
            outbox_status_for_action(&db, "d-160o-b", "status_reaction"),
            vec!["pending"]
        );
        assert_eq!(
            outbox_status_for_action(&db, "d-160o-c", "notify"),
            vec!["done"]
        );
        assert_eq!(
            outbox_status_for_action(&db, "d-160o-c", "status_reaction"),
            vec!["pending"]
        );
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

        // Insert duplicate outbox entries for the same dispatch
        seed_outbox(&db, "d-160d", "notify");
        seed_outbox(&db, "d-160d", "notify");

        let mock = MockNotifier::new();
        let processed = process_outbox_batch(&db, &mock).await;

        // Worker processes all pending entries
        assert_eq!(processed, 2, "Worker should process both pending entries");
        // MockNotifier doesn't have the two-phase guard — both entries call through.
        // In production, send_dispatch_to_discord deduplicates via dispatch_reserving/notified.
        assert_eq!(
            mock.notify_count(),
            2,
            "MockNotifier receives both calls (production dedup is in send_dispatch_to_discord)"
        );

        // Both notify rows are done; dispatch transition queues a single status sync.
        assert_eq!(
            outbox_status_for_action(&db, "d-160d", "notify"),
            vec!["done", "done"]
        );
        assert_eq!(
            outbox_status_for_action(&db, "d-160d", "status_reaction"),
            vec!["pending"]
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
        assert_eq!(mock.notify_count(), 1);
        assert_eq!(outbox_status(&db, "d-160c"), vec!["done"]);
        assert_eq!(
            get_dispatch_status(&db, "d-160c"),
            "completed",
            "terminal dispatch status must not be rewritten by notify success"
        );
    }
}

mod delayed_worker {
    use super::*;

    #[test]
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
    fn scenario_421_long_turn_alerts_start_at_30_minutes() {
        let runtime_root = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootOverride::new(runtime_root.path());
        let policies_dir = setup_timeouts_policy_dir();
        let db = test_db();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        let session_key = "host:tmux-421-long";

        seed_agent(&db);
        set_kv(&db, "deadlock_manager_channel_id", "999");

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

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);
        assert_eq!(
            message_outbox_rows(&db).len(),
            1,
            "same tier must not alert twice"
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
        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 2);
        assert!(messages[1].1.contains("경과: 61분 (60분 단계)"));

        write_codex_inflight(
            runtime_root.path(),
            "111",
            &relative_local_time(121),
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
        assert_eq!(messages.len(), 3);
        assert!(messages[2].1.contains("경과: 121분 (120분 단계)"));
    }
}

mod idle_session_cleanup {
    use super::*;

    #[test]
    fn scenario_492_idle_session_without_active_dispatch_force_kills_once_after_60_minutes() {
        let policies_dir = setup_timeouts_policy_dir();
        let db = test_db();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        let session_key = "host:idle-492-no-dispatch";

        seed_agent(&db);
        set_kv(&db, "server_port", "8791");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions
                 (session_key, agent_id, provider, status, last_heartbeat, created_at)
                 VALUES (?1, 'agent-1', 'codex', 'idle', datetime('now', '-181 minutes'), datetime('now', '-181 minutes'))",
                [session_key],
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(kv_value(&db, "test_http_count").as_deref(), Some("1"));

        let http_last: serde_json::Value =
            serde_json::from_str(&kv_value(&db, "test_http_last").unwrap()).unwrap();
        let url = http_last["url"].as_str().unwrap_or("");
        assert!(url.contains("/api/sessions/"));
        assert!(url.contains("host%3Aidle-492-no-dispatch"));
        assert!(url.ends_with("/force-kill"));

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "channel:111");
        assert!(
            messages[0]
                .1
                .contains("idle 60분 경과 (active_dispatch_id 없음)")
        );
    }

    #[test]
    fn scenario_492_idle_session_with_active_dispatch_uses_180_minute_safety_ttl() {
        let policies_dir = setup_timeouts_policy_dir();
        let db = test_db();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        let session_key = "host:idle-492-active-dispatch";

        seed_agent(&db);
        set_kv(&db, "server_port", "8791");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions
                 (session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at)
                 VALUES (?1, 'agent-1', 'codex', 'idle', 'dispatch-492', datetime('now', '-61 minutes'), datetime('now', '-61 minutes'))",
                [session_key],
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            kv_value(&db, "test_http_count"),
            None,
            "active dispatch rows must not be reaped by the 60-minute idle TTL"
        );
        assert!(message_outbox_rows(&db).is_empty());

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE sessions
                 SET last_heartbeat = datetime('now', '-181 minutes'),
                     created_at = datetime('now', '-181 minutes')
                 WHERE session_key = ?1",
                [session_key],
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(kv_value(&db, "test_http_count").as_deref(), Some("1"));

        let http_last: serde_json::Value =
            serde_json::from_str(&kv_value(&db, "test_http_last").unwrap()).unwrap();
        let url = http_last["url"].as_str().unwrap_or("");
        assert!(url.contains("host%3Aidle-492-active-dispatch"));
        assert!(url.ends_with("/force-kill"));

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "channel:111");
        assert!(messages[0].1.contains("idle 180분 경과 (safety TTL)"));
    }
}
