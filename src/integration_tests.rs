//! #124: Pipeline integration test harness — 6 mandatory scenarios
//!
//! These tests verify pipeline correctness end-to-end before #106 data-driven transition.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::db;
    use crate::dispatch;
    use crate::engine::PolicyEngine;
    use crate::kanban;
    use crate::server::routes::AppState;

    fn test_db() -> db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::schema::migrate(&conn).unwrap();
        db::wrap_conn(conn)
    }

    fn test_engine(db: &db::Db) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn seed_agent(db: &db::Db) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Test Agent', '111', '222')",
            [],
        )
        .unwrap();
    }

    fn seed_card(db: &db::Db, card_id: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES (?1, 'Test Card', ?2, 'agent-1', datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status],
        )
        .unwrap();
    }

    fn seed_dispatch(db: &db::Db, dispatch_id: &str, card_id: &str, dtype: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES (?1, ?2, 'agent-1', ?3, ?4, 'Test Dispatch', datetime('now'), datetime('now'))",
            rusqlite::params![dispatch_id, card_id, dtype, status],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1 WHERE id = ?2",
            rusqlite::params![dispatch_id, card_id],
        )
        .unwrap();
    }

    fn get_card_status(db: &db::Db, card_id: &str) -> String {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn get_dispatch_status(db: &db::Db, dispatch_id: &str) -> String {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    // ── Scenario 1: Implementation idle does not complete (#115) ────

    #[tokio::test]
    async fn scenario_1_implementation_idle_does_not_complete() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-s1", "in_progress");
        seed_dispatch(&db, "d-s1", "card-s1", "implementation", "pending");

        let state = AppState {
            db: db.clone(),
            engine: test_engine(&db),
            broadcast_tx: crate::server::ws::new_broadcast(),
            batch_buffer: crate::server::ws::spawn_batch_flusher(crate::server::ws::new_broadcast()),
            health_registry: None,
        };

        let (status, _) = crate::server::routes::dispatched_sessions::hook_session(
            axum::extract::State(state),
            axum::Json(
                crate::server::routes::dispatched_sessions::HookSessionBody {
                    session_key: "test-session".to_string(),
                    status: Some("idle".to_string()),
                    provider: Some("claude".to_string()),
                    session_info: None,
                    name: None,
                    model: None,
                    tokens: None,
                    cwd: None,
                    dispatch_id: Some("d-s1".to_string()),
                    claude_session_id: None,
                    session_id: None,
                },
            ),
        )
        .await;

        assert_eq!(status, axum::http::StatusCode::OK);

        // Implementation dispatch must NOT be auto-completed by idle
        let d_status = get_dispatch_status(&db, "d-s1");
        assert_eq!(
            d_status, "pending",
            "implementation dispatch must NOT be completed by idle heartbeat"
        );
    }

    // ── Scenario 2: Single active review-decision per card (#116) ───

    #[test]
    fn scenario_2_single_active_review_decision_per_card() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-s2", "review");

        let r1 = dispatch::create_dispatch_core(
            &db,
            "card-s2",
            "agent-1",
            "review-decision",
            "[RD1]",
            &serde_json::json!({"verdict": "improve"}),
        );
        assert!(r1.is_ok(), "first review-decision should succeed");

        let r2 = dispatch::create_dispatch_core(
            &db,
            "card-s2",
            "agent-1",
            "review-decision",
            "[RD2]",
            &serde_json::json!({"verdict": "rework"}),
        );
        assert!(r2.is_ok(), "second review-decision should succeed");

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-s2' AND dispatch_type = 'review-decision' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "exactly 1 active review-decision per card");

        let r1_id = r1.unwrap().0;
        let r1_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [&r1_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            r1_status, "cancelled",
            "first review-decision should be cancelled"
        );
    }

    // ── Scenario 3: Restart recovery — reconciliation fixes broken state ──

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

    // ── Scenario 4: Card status full cycle ──────────────────────────

    #[test]
    fn scenario_4_card_status_full_cycle() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-s4", "backlog");

        // backlog → ready
        assert!(kanban::transition_status(&db, &engine, "card-s4", "ready").is_ok());
        assert_eq!(get_card_status(&db, "card-s4"), "ready");

        // ready → requested (needs dispatch)
        seed_dispatch(&db, "d-s4-impl", "card-s4", "implementation", "pending");
        assert!(kanban::transition_status(&db, &engine, "card-s4", "requested").is_ok());
        assert_eq!(get_card_status(&db, "card-s4"), "requested");

        // requested → in_progress
        assert!(kanban::transition_status(&db, &engine, "card-s4", "in_progress").is_ok());
        assert_eq!(get_card_status(&db, "card-s4"), "in_progress");

        // Verify started_at
        {
            let conn = db.lock().unwrap();
            let started_at: Option<String> = conn
                .query_row(
                    "SELECT started_at FROM kanban_cards WHERE id = 'card-s4'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(started_at.is_some(), "started_at must be set");
        }

        // in_progress → review
        assert!(kanban::transition_status(&db, &engine, "card-s4", "review").is_ok());
        assert_eq!(get_card_status(&db, "card-s4"), "review");

        // review → done (force)
        assert!(
            kanban::transition_status_with_opts(&db, &engine, "card-s4", "done", "test", true)
                .is_ok()
        );
        assert_eq!(get_card_status(&db, "card-s4"), "done");

        // Verify done cleanup
        {
            let conn = db.lock().unwrap();
            let review_status: Option<String> = conn
                .query_row(
                    "SELECT review_status FROM kanban_cards WHERE id = 'card-s4'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(review_status, None, "review_status cleared on done");

            let completed_at: Option<String> = conn
                .query_row(
                    "SELECT completed_at FROM kanban_cards WHERE id = 'card-s4'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(completed_at.is_some(), "completed_at set on done");
        }
    }

    // ── Scenario 5: Timeout recovery ────────────────────────────────

    #[test]
    fn scenario_5_timeout_recovery_stale_to_pending_decision() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);

        // Card stuck in requested for 50 min with exhausted retries
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, requested_at, created_at, updated_at) \
                 VALUES ('card-s5', 'Stale', 'requested', 'agent-1', datetime('now', '-50 minutes'), datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, retry_count, created_at, updated_at) \
                 VALUES ('d-s5', 'card-s5', 'agent-1', 'implementation', 'pending', 'Test', 10, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = 'd-s5' WHERE id = 'card-s5'",
                [],
            )
            .unwrap();
        }

        // Fire onTick1min — [A] requested timeout lives in 1min tier (#127)
        let _ = engine.try_fire_hook_by_name("OnTick1min", serde_json::json!({}));

        // Drain transitions
        loop {
            let transitions = engine.drain_pending_transitions();
            if transitions.is_empty() {
                break;
            }
            for (card_id, old_s, new_s) in &transitions {
                kanban::fire_transition_hooks(&db, &engine, card_id, old_s, new_s);
            }
        }

        let status = get_card_status(&db, "card-s5");
        assert_eq!(
            status, "pending_decision",
            "stale requested card with exhausted retries → pending_decision"
        );
    }

    // ── Scenario 6: Dispatch roundtrip — create → complete_dispatch → PM gate → review ──
    //
    // Tests the full dispatch lifecycle using the canonical completion path:
    // 1. dispatch::create_dispatch_core creates a pending dispatch
    // 2. dispatch::complete_dispatch completes via the same path as PATCH /api/dispatches/:id
    //    (DB update → OnDispatchCompleted → drain transitions → fire_transition_hooks)
    // 3. PM gate passes (no DoD, no duration check) → card transitions to review
    // 4. OnReviewEnter fires → review dispatch is created

    #[test]
    fn scenario_6_dispatch_roundtrip() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-s6", "in_progress");

        // Step 1: Create implementation dispatch via canonical path
        let (dispatch_id, _) = dispatch::create_dispatch_core(
            &db,
            "card-s6",
            "agent-1",
            "implementation",
            "[Impl]",
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(get_dispatch_status(&db, &dispatch_id), "pending");

        // Verify latest_dispatch_id was updated
        {
            let conn = db.lock().unwrap();
            let latest: String = conn
                .query_row(
                    "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-s6'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                latest, dispatch_id,
                "latest_dispatch_id must point to new dispatch"
            );
        }

        // Step 2: Complete via dispatch::complete_dispatch — the canonical path
        // used by PATCH /api/dispatches/:id and turn_bridge.
        // This handles: DB update → OnDispatchCompleted → drain transitions → fire_transition_hooks
        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            &dispatch_id,
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );
        assert_eq!(get_dispatch_status(&db, &dispatch_id), "completed");

        // Step 3: PM gate passes (no DoD items, no duration constraint) → card must be in review
        let final_status = get_card_status(&db, "card-s6");
        assert_eq!(
            final_status, "review",
            "PM gate with empty DoD should pass → card must be in review"
        );

        // Step 4: Verify review state was properly initialized
        {
            let conn = db.lock().unwrap();

            // review_entered_at must be set
            let review_entered: Option<String> = conn
                .query_row(
                    "SELECT review_entered_at FROM kanban_cards WHERE id = 'card-s6'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(review_entered.is_some(), "review_entered_at must be set");

            // OnReviewEnter should have created a review dispatch
            let review_dispatch_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_dispatches \
                     WHERE kanban_card_id = 'card-s6' AND dispatch_type = 'review' \
                     AND status IN ('pending', 'dispatched')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                review_dispatch_count, 1,
                "OnReviewEnter should create exactly 1 review dispatch"
            );
        }
    }

    // ── Scenario 7: dispatch uses card's effective pipeline, not global default (#134/#136) ──

    #[test]
    fn scenario_7_dispatch_uses_card_effective_pipeline() {
        let db = test_db();
        seed_agent(&db);
        crate::pipeline::ensure_loaded();

        // Simple pipeline override: ready→in_progress (gated), in_progress→done (gated)
        // No "requested" state at all — kickoff should be "in_progress"
        let simple_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "done", "type": "gated", "gates": ["active_dispatch"]}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"}
            },
            "hooks": {
                "in_progress": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "done": {"on_enter": ["OnCardTransition", "OnCardTerminal"], "on_exit": []}
            },
            "clocks": {
                "in_progress": {"set": "started_at"},
                "done": {"set": "completed_at"}
            },
            "events": {
                "on_dispatch_completed": ["OnDispatchCompleted"]
            },
            "timeouts": {
                "in_progress": {"duration": "4h", "clock": "started_at", "on_exhaust": "done"}
            }
        });

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, pipeline_config) VALUES ('repo-s7', 'test/s7', ?1)",
                [simple_override.to_string()],
            ).unwrap();
            // Card with repo_id pointing to override — in "ready" (dispatchable in simple pipeline)
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-s7', 'S7 Card', 'ready', 'repo-s7', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            // Need a completed dispatch so the pending-dispatch guard doesn't block
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-s7-old', 'card-s7', 'agent-1', 'implementation', 'completed', 'old', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        // Default pipeline kickoff is "requested", but simple pipeline kickoff is "in_progress"
        let default_kickoff = crate::pipeline::get()
            .transitions
            .iter()
            .find(|t| {
                t.transition_type == crate::pipeline::TransitionType::Gated
                    && crate::pipeline::get()
                        .dispatchable_states()
                        .contains(&t.from.as_str())
            })
            .map(|t| t.to.as_str())
            .unwrap();
        assert_eq!(
            default_kickoff, "requested",
            "default pipeline kickoff must be 'requested'"
        );

        // Create dispatch via create_dispatch_core_with_id — should use card's effective pipeline
        let result = dispatch::create_dispatch_core_with_id(
            &db,
            "d-s7-new",
            "card-s7",
            "agent-1",
            "implementation",
            "[S7 test]",
            &serde_json::json!({}),
        );
        assert!(
            result.is_ok(),
            "dispatch creation should succeed: {:?}",
            result.err()
        );

        // Card status must be "in_progress" (override kickoff), NOT "requested" (default kickoff)
        let status = get_card_status(&db, "card-s7");
        assert_eq!(
            status, "in_progress",
            "dispatch must use card's effective pipeline kickoff, not global default"
        );

        // Also test create_dispatch_core (the non-ID path)
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-s7b', 'S7b Card', 'ready', 'repo-s7', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-s7b-old', 'card-s7b', 'agent-1', 'implementation', 'completed', 'old', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }
        let result2 = dispatch::create_dispatch_core(
            &db,
            "card-s7b",
            "agent-1",
            "implementation",
            "[S7b test]",
            &serde_json::json!({}),
        );
        assert!(
            result2.is_ok(),
            "create_dispatch_core should succeed: {:?}",
            result2.err()
        );
        assert_eq!(
            get_card_status(&db, "card-s7b"),
            "in_progress",
            "create_dispatch_core must also use card's effective pipeline kickoff"
        );
    }

    // ── Scenario 8: Custom pipeline override — resolve and validate (#135/#136) ──

    #[test]
    fn scenario_8_custom_pipeline_override_resolve_and_validate() {
        let db = test_db();
        seed_agent(&db);
        crate::pipeline::ensure_loaded();

        // Insert a repo with a simple pipeline override (no review state)
        let simple_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "done", "type": "gated", "gates": ["active_dispatch"]}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"}
            },
            "hooks": {
                "in_progress": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "done": {"on_enter": ["OnCardTransition", "OnCardTerminal"], "on_exit": []}
            },
            "clocks": {
                "in_progress": {"set": "started_at"},
                "done": {"set": "completed_at"}
            },
            "events": {
                "on_dispatch_completed": ["OnDispatchCompleted"]
            },
            "timeouts": {
                "in_progress": {"duration": "4h", "clock": "started_at", "on_exhaust": "done"}
            }
        });

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, pipeline_config) VALUES ('repo-simple', 'test/simple', ?1)",
                [simple_override.to_string()],
            )
            .unwrap();
        }

        // Resolve effective pipeline for this repo
        let conn = db.lock().unwrap();
        let effective = crate::pipeline::resolve_for_card(&conn, Some("repo-simple"), None);
        drop(conn);

        // Validate the effective pipeline
        assert!(
            effective.validate().is_ok(),
            "simple pipeline override must be valid"
        );

        // Verify states: no "review" or "requested" state
        let state_ids: Vec<&str> = effective.states.iter().map(|s| s.id.as_str()).collect();
        assert!(
            !state_ids.contains(&"review"),
            "simple pipeline has no review state"
        );
        assert!(
            !state_ids.contains(&"requested"),
            "simple pipeline has no requested state"
        );
        assert!(
            state_ids.contains(&"in_progress"),
            "simple pipeline has in_progress"
        );
        assert!(state_ids.contains(&"done"), "simple pipeline has done");

        // Verify terminal state
        assert!(effective.is_terminal("done"), "done is terminal");
        assert!(
            !effective.is_terminal("in_progress"),
            "in_progress is not terminal"
        );

        // Verify dispatchable state
        let dispatchable = effective.dispatchable_states();
        assert_eq!(
            dispatchable,
            vec!["ready"],
            "ready is the only dispatchable state"
        );

        // Verify transitions work: card can go ready → in_progress (gated)
        assert!(
            effective.find_transition("ready", "in_progress").is_some(),
            "ready → in_progress transition must exist"
        );
        assert!(
            effective.find_transition("in_progress", "done").is_some(),
            "in_progress → done transition must exist"
        );
        // No review transition
        assert!(
            effective.find_transition("in_progress", "review").is_none(),
            "in_progress → review must NOT exist in simple pipeline"
        );
    }

    // ── Scenario 9: QA pipeline override with custom qa_test state (#136) ──

    #[test]
    fn scenario_9_qa_pipeline_override_transitions() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        crate::pipeline::ensure_loaded();

        // Store QA pipeline as repo override
        let qa_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "requested", "label": "Requested"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "review", "label": "Review"},
                {"id": "qa_test", "label": "QA Test"},
                {"id": "pending_decision", "label": "Pending"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "requested", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "requested", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "review", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "review", "to": "qa_test", "type": "gated", "gates": ["review_passed"]},
                {"from": "review", "to": "in_progress", "type": "gated", "gates": ["review_rework"]},
                {"from": "qa_test", "to": "done", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "qa_test", "to": "in_progress", "type": "force_only"},
                {"from": "requested", "to": "pending_decision", "type": "force_only"},
                {"from": "pending_decision", "to": "done", "type": "force_only"}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"},
                "review_passed": {"type": "builtin", "check": "review_verdict_pass"},
                "review_rework": {"type": "builtin", "check": "review_verdict_rework"}
            },
            "hooks": {
                "in_progress": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "review": {"on_enter": ["OnCardTransition", "OnReviewEnter"], "on_exit": []},
                "qa_test": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "done": {"on_enter": ["OnCardTransition", "OnCardTerminal"], "on_exit": []}
            },
            "clocks": {
                "requested": {"set": "requested_at"},
                "in_progress": {"set": "started_at", "mode": "coalesce"},
                "review": {"set": "review_entered_at"},
                "done": {"set": "completed_at"}
            }
        });

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, pipeline_config) VALUES ('repo-qa', 'test/qa', ?1)",
                [qa_override.to_string()],
            )
            .unwrap();
        }

        // Resolve and validate
        let conn = db.lock().unwrap();
        let effective = crate::pipeline::resolve_for_card(&conn, Some("repo-qa"), None);
        drop(conn);
        assert!(effective.validate().is_ok(), "QA pipeline must be valid");

        // Key assertion: review → qa_test transition exists (not review → done)
        let review_pass = effective.find_transition("review", "qa_test");
        assert!(
            review_pass.is_some(),
            "review → qa_test must exist in QA pipeline"
        );
        let review_done = effective.find_transition("review", "done");
        assert!(
            review_done.is_none(),
            "review → done must NOT exist in QA pipeline"
        );

        // qa_test → done transition
        let qa_done = effective.find_transition("qa_test", "done");
        assert!(qa_done.is_some(), "qa_test → done must exist");

        // qa_test → in_progress force transition
        let qa_rework = effective.find_transition("qa_test", "in_progress");
        assert!(
            qa_rework.is_some(),
            "qa_test → in_progress (force) must exist"
        );

        // Verify custom state has hooks
        let qa_hooks = effective.hooks_for_state("qa_test");
        assert!(qa_hooks.is_some(), "qa_test must have hook bindings");
        assert!(
            qa_hooks
                .unwrap()
                .on_enter
                .contains(&"OnCardTransition".to_string()),
            "qa_test on_enter must include OnCardTransition"
        );

        // Test actual card transition through qa_test
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-qa', 'QA Card', 'qa_test', 'repo-qa', 'agent-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-qa', 'card-qa', 'agent-1', 'implementation', 'dispatched', 'QA test', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = 'd-qa' WHERE id = 'card-qa'",
                [],
            )
            .unwrap();
        }

        // Force transition qa_test → in_progress (simulating QA failure)
        let result = kanban::transition_status_with_opts(
            &db,
            &engine,
            "card-qa",
            "in_progress",
            "qa-fail",
            true,
        );
        assert!(
            result.is_ok(),
            "qa_test → in_progress force transition must work"
        );
        assert_eq!(get_card_status(&db, "card-qa"), "in_progress");
    }

    // ── Scenario 10: Multi-dispatchable pipeline — kickoff resolves from card's current state ──

    #[test]
    fn scenario_10_multi_dispatchable_kickoff_uses_current_state() {
        let db = test_db();
        seed_agent(&db);
        crate::pipeline::ensure_loaded();

        // Pipeline with TWO dispatchable states, each with a DIFFERENT gated target:
        //   ready      → (gated) → in_progress
        //   qa_ready   → (gated) → qa_test
        // If kickoff resolution ignores old_status, it picks the first one arbitrarily.
        let multi_disp_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "review", "label": "Review"},
                {"id": "qa_ready", "label": "QA Ready"},
                {"id": "qa_test", "label": "QA Test"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "review", "type": "free"},
                {"from": "review", "to": "qa_ready", "type": "free"},
                {"from": "qa_ready", "to": "qa_test", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "qa_test", "to": "done", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "review", "to": "in_progress", "type": "gated", "gates": ["review_rework"]}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"},
                "review_rework": {"type": "builtin", "check": "review_verdict_rework"}
            },
            "hooks": {},
            "clocks": {},
            "events": {},
            "timeouts": {}
        });

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, pipeline_config) VALUES ('repo-multi', 'test/multi', ?1)",
                [multi_disp_override.to_string()],
            ).unwrap();
        }

        // Card A: in "ready" — dispatch should kick off to "in_progress"
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-multi-a', 'Multi A', 'ready', 'repo-multi', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-multi-a-old', 'card-multi-a', 'agent-1', 'implementation', 'completed', 'old', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let result_a = dispatch::create_dispatch_core_with_id(
            &db,
            "d-multi-a",
            "card-multi-a",
            "agent-1",
            "implementation",
            "[Multi A]",
            &serde_json::json!({}),
        );
        assert!(
            result_a.is_ok(),
            "dispatch for card-multi-a should succeed: {:?}",
            result_a.err()
        );
        assert_eq!(
            get_card_status(&db, "card-multi-a"),
            "in_progress",
            "card in 'ready' must kick off to 'in_progress', not 'qa_test'"
        );

        // Card B: in "qa_ready" — dispatch should kick off to "qa_test"
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-multi-b', 'Multi B', 'qa_ready', 'repo-multi', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-multi-b-old', 'card-multi-b', 'agent-1', 'implementation', 'completed', 'old', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let result_b = dispatch::create_dispatch_core_with_id(
            &db,
            "d-multi-b",
            "card-multi-b",
            "agent-1",
            "implementation",
            "[Multi B]",
            &serde_json::json!({}),
        );
        assert!(
            result_b.is_ok(),
            "dispatch for card-multi-b should succeed: {:?}",
            result_b.err()
        );
        assert_eq!(
            get_card_status(&db, "card-multi-b"),
            "qa_test",
            "card in 'qa_ready' must kick off to 'qa_test', not 'in_progress'"
        );
    }
}
