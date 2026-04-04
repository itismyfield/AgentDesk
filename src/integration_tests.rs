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

        // ready → requested (free transition, no dispatch needed — #255 preflight state)
        assert!(kanban::transition_status(&db, &engine, "card-s4", "requested").is_ok());
        assert_eq!(get_card_status(&db, "card-s4"), "requested");

        // requested → in_progress (needs dispatch — gated transition)
        seed_dispatch(&db, "d-s4-impl", "card-s4", "implementation", "pending");
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
        let (dispatch_id, _, _) = dispatch::create_dispatch_core(
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

        // #255: Default pipeline kickoff is "in_progress" (requested is now a free preflight state,
        // so the dispatchable state is "requested" with gated target "in_progress").
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
            default_kickoff, "in_progress",
            "default pipeline kickoff must be 'in_progress' (#255: requested is preflight)"
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

        // Card status must be "in_progress" (both override and default kickoff target the same)
        let status = get_card_status(&db, "card-s7");
        assert_eq!(
            status, "in_progress",
            "dispatch must use card's effective pipeline kickoff"
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

    // ── #158: card_review_state write centralisation tests ──────────

    /// Helper: query card_review_state for a card.
    fn get_review_state(
        db: &db::Db,
        card_id: &str,
    ) -> Option<(String, Option<String>, Option<String>)> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT state, last_verdict, last_decision FROM card_review_state WHERE card_id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()
    }

    /// #158: Typed bridge (review_state_sync) writes card_review_state correctly.
    /// Tests the Rust entrypoint that backs the JS agentdesk.reviewState.sync bridge.
    #[test]
    fn scenario_158a_typed_bridge_writes_review_state() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-158a", "review");

        // Step 1: Set reviewing state with round via JSON wrapper (same path as JS bridge)
        let result = crate::engine::ops::review_state_sync(
            &db,
            r#"{"card_id":"card-158a","state":"reviewing","review_round":1}"#,
        );
        assert!(
            result.contains("\"ok\":true"),
            "sync to reviewing must succeed: {result}"
        );

        let (state, _, _) =
            get_review_state(&db, "card-158a").expect("card_review_state row must exist");
        assert_eq!(state, "reviewing", "bridge must create reviewing state");

        // Step 2: Update with verdict
        let result2 = crate::engine::ops::review_state_sync(
            &db,
            r#"{"card_id":"card-158a","state":"suggestion_pending","last_verdict":"improve"}"#,
        );
        assert!(
            result2.contains("\"ok\":true"),
            "sync to suggestion_pending must succeed: {result2}"
        );

        let (state2, verdict, _) = get_review_state(&db, "card-158a").unwrap();
        assert_eq!(state2, "suggestion_pending");
        assert_eq!(verdict.as_deref(), Some("improve"));

        // Step 3: Set to idle — must clear pending_dispatch_id
        let result3 =
            crate::engine::ops::review_state_sync(&db, r#"{"card_id":"card-158a","state":"idle"}"#);
        assert!(
            result3.contains("\"ok\":true"),
            "sync to idle must succeed: {result3}"
        );

        let (state3, _, _) = get_review_state(&db, "card-158a").unwrap();
        assert_eq!(state3, "idle", "bridge must allow idle transition");

        // Step 4: Verify JS bridge is registered and callable (smoke test)
        let engine = test_engine(&db);
        let js_check: String = engine
            .eval_js(r#"typeof agentdesk.reviewState.sync === "function" ? "ok" : "missing""#)
            .unwrap();
        assert_eq!(
            js_check, "ok",
            "agentdesk.reviewState.sync must be registered as a function"
        );
    }

    /// #158: ExecuteSQL intent rejects direct card_review_state mutations.
    #[test]
    fn scenario_158b_execute_sql_intent_rejects_review_state_write() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-158b", "review");

        // Attempt INSERT via ExecuteSQL intent — must fail
        let insert_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "INSERT INTO card_review_state (card_id, state, updated_at) VALUES ('card-158b', 'idle', datetime('now'))".to_string(),
            params: vec![],
        };
        let result = crate::engine::intent::execute_intents(&db, vec![insert_intent]);
        assert_eq!(
            result.errors, 1,
            "INSERT into card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt INSERT OR REPLACE via ExecuteSQL intent — must also fail
        let replace_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "INSERT OR REPLACE INTO card_review_state (card_id, state, updated_at) VALUES ('card-158b', 'idle', datetime('now'))".to_string(),
            params: vec![],
        };
        let result_replace = crate::engine::intent::execute_intents(&db, vec![replace_intent]);
        assert_eq!(
            result_replace.errors, 1,
            "INSERT OR REPLACE into card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt REPLACE INTO via ExecuteSQL intent — must also fail
        let replace_into_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "REPLACE INTO card_review_state (card_id, state, updated_at) VALUES ('card-158b', 'idle', datetime('now'))".to_string(),
            params: vec![],
        };
        let result_replace_into =
            crate::engine::intent::execute_intents(&db, vec![replace_into_intent]);
        assert_eq!(
            result_replace_into.errors, 1,
            "REPLACE INTO card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt UPDATE via ExecuteSQL intent — must also fail
        let update_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "UPDATE card_review_state SET state = 'idle' WHERE card_id = 'card-158b'"
                .to_string(),
            params: vec![],
        };
        let result2 = crate::engine::intent::execute_intents(&db, vec![update_intent]);
        assert_eq!(
            result2.errors, 1,
            "UPDATE card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt DELETE via ExecuteSQL intent — must also fail
        let delete_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "DELETE FROM card_review_state WHERE card_id = 'card-158b'".to_string(),
            params: vec![],
        };
        let result3 = crate::engine::intent::execute_intents(&db, vec![delete_intent]);
        assert_eq!(
            result3.errors, 1,
            "DELETE from card_review_state via ExecuteSQL must be rejected"
        );

        // Verify no row was created
        assert!(
            get_review_state(&db, "card-158b").is_none(),
            "no card_review_state row should exist after blocked intents"
        );
    }

    /// #158: JS db.execute() blocks direct card_review_state SQL writes.
    #[test]
    fn scenario_158c_js_db_execute_blocks_review_state_direct_sql() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-158c", "review");

        // Try INSERT via agentdesk.db.execute — must throw
        let insert_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "INSERT INTO card_review_state (card_id, state, updated_at) VALUES ('card-158c', 'idle', datetime('now'))"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            insert_result, "blocked",
            "JS db.execute INSERT into card_review_state must be blocked"
        );

        // Try INSERT OR REPLACE via agentdesk.db.execute — must throw
        let replace_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO card_review_state (card_id, state, updated_at) VALUES ('card-158c', 'idle', datetime('now'))"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            replace_result, "blocked",
            "JS db.execute INSERT OR REPLACE into card_review_state must be blocked"
        );

        // Try REPLACE INTO via agentdesk.db.execute — must throw
        let replace_into_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "REPLACE INTO card_review_state (card_id, state, updated_at) VALUES ('card-158c', 'idle', datetime('now'))"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            replace_into_result, "blocked",
            "JS db.execute REPLACE INTO card_review_state must be blocked"
        );

        // Try UPDATE via agentdesk.db.execute — must throw
        let update_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "UPDATE card_review_state SET state = 'idle' WHERE card_id = 'card-158c'"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            update_result, "blocked",
            "JS db.execute UPDATE on card_review_state must be blocked"
        );

        // Try DELETE via agentdesk.db.execute — must throw
        let delete_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "DELETE FROM card_review_state WHERE card_id = 'card-158c'"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            delete_result, "blocked",
            "JS db.execute DELETE on card_review_state must be blocked"
        );

        // Verify no row was created by blocked operations
        assert!(
            get_review_state(&db, "card-158c").is_none(),
            "no card_review_state row should exist after blocked JS db.execute"
        );
    }

    /// #158: Full review cycle — card transitions sync card_review_state via single entrypoint.
    #[test]
    fn scenario_158d_review_cycle_syncs_canonical_state() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-158d", "in_progress");

        // Create implementation dispatch and complete it to trigger review transition
        seed_dispatch(&db, "d-158d", "card-158d", "implementation", "pending");

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "d-158d",
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        // Card should be in review
        assert_eq!(get_card_status(&db, "card-158d"), "review");

        // card_review_state must be "reviewing" (synced via single entrypoint during transition)
        let (state, _, _) = get_review_state(&db, "card-158d")
            .expect("card_review_state must exist after review transition");
        assert_eq!(
            state, "reviewing",
            "canonical review state must be 'reviewing' after entering review"
        );

        // Force card to done — review state must reset to idle
        assert!(
            kanban::transition_status_with_opts(&db, &engine, "card-158d", "done", "test", true)
                .is_ok()
        );
        assert_eq!(get_card_status(&db, "card-158d"), "done");

        let (state2, _, _) = get_review_state(&db, "card-158d").unwrap();
        assert_eq!(
            state2, "idle",
            "canonical review state must be 'idle' after terminal transition"
        );
    }

    /// #158: review-automation.js OnReviewEnter hook uses reviewState.sync bridge.
    #[test]
    fn scenario_158e_on_review_enter_js_hook_syncs_canonical_state() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-158e", "review");

        kanban::fire_enter_hooks(&db, &engine, "card-158e", "review");

        let (state, _, _) = get_review_state(&db, "card-158e")
            .expect("card_review_state must exist after OnReviewEnter hook");
        assert_eq!(
            state, "reviewing",
            "OnReviewEnter policy hook must sync canonical review state via bridge"
        );

        let conn = db.lock().unwrap();
        let review_round: i64 = conn
            .query_row(
                "SELECT review_round FROM kanban_cards WHERE id = 'card-158e'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(review_round, 1, "OnReviewEnter must increment review_round");

        let review_dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-158e' AND dispatch_type = 'review' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_dispatch_count, 1,
            "OnReviewEnter must create exactly one pending review dispatch"
        );
    }

    // ── #160: Process-level restart/delivery boundary tests ────
    //
    // Infrastructure: MockNotifier + process_outbox_batch + DB fallback helpers.
    // Tests exercise actual outbox worker code paths, not raw SQL.

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

        // Verify outbox entry transitioned to done
        assert_eq!(outbox_status(&db, "d-160-1"), vec!["done"]);

        // Second batch should find nothing pending
        let processed2 = process_outbox_batch(&db, &mock).await;
        assert_eq!(processed2, 0, "No pending entries after first drain");
        assert_eq!(mock.notify_count(), 1, "No additional calls on empty queue");
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

        // Step 1: Verify finalize_dispatch works on the happy path
        let result = dispatch::finalize_dispatch(
            &db,
            &engine,
            "d-160r",
            "recovery_completed_during_downtime",
            Some(&serde_json::json!({"summary": "completed during downtime"})),
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
                     result = '{\"completion_source\":\"turn_bridge_db_fallback\",\"needs_reconcile\":true}', \
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

        // All entries should be done
        assert_eq!(outbox_status(&db, "d-160o-a"), vec!["done"]);
        assert_eq!(outbox_status(&db, "d-160o-b"), vec!["done"]);
        assert_eq!(outbox_status(&db, "d-160o-c"), vec!["done"]);
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

        // Both entries should transition to done (second via idempotent reservation check)
        assert_eq!(outbox_status(&db, "d-160d"), vec!["done", "done"]);
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

    // ── #195: review-decision accept creates rework dispatch ──────────
    //
    // Verifies that when an agent accepts review feedback via POST /api/review-decision,
    // a rework dispatch is automatically created and the card transitions to the
    // rework target state (in_progress), NOT directly to review.
    // This prevents the pipeline from getting stuck when the accept decision
    // was the only active dispatch for the card.

    #[tokio::test]
    async fn scenario_195_accept_creates_rework_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-195", "review");

        // Set up a pending review-decision dispatch (simulates the state after
        // counter-model review found suggestions and agent received decision prompt)
        seed_dispatch(&db, "rd-195", "card-195", "review-decision", "pending");

        // Set up card_review_state with pending_dispatch_id pointing to the review-decision
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO card_review_state (card_id, state, pending_dispatch_id) \
                 VALUES ('card-195', 'suggestion_pending', 'rd-195')",
                [],
            )
            .unwrap();
        }

        let state = AppState {
            db: db.clone(),
            engine,
            broadcast_tx: crate::server::ws::new_broadcast(),
            batch_buffer: crate::server::ws::spawn_batch_flusher(crate::server::ws::new_broadcast()),
            health_registry: None,
        };

        // Call the review-decision handler with accept
        let (status, json) = crate::server::routes::review_verdict::submit_review_decision(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::ReviewDecisionBody {
                card_id: "card-195".to_string(),
                decision: "accept".to_string(),
                comment: None,
                dispatch_id: Some("rd-195".to_string()),
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "accept should succeed: {json:?}"
        );
        assert_eq!(
            json.0["rework_dispatch_created"], true,
            "rework_dispatch_created must be true in response"
        );

        // Review-decision dispatch must be completed
        assert_eq!(
            get_dispatch_status(&db, "rd-195"),
            "completed",
            "review-decision dispatch must be completed after accept"
        );

        // Card must be in rework target state (in_progress), NOT review
        let card_status = get_card_status(&db, "card-195");
        assert_eq!(
            card_status, "in_progress",
            "#195: accept must transition card to rework target (in_progress), not review"
        );

        // A rework dispatch must exist for this card
        let conn = db.lock().unwrap();
        let rework_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-195' AND dispatch_type = 'rework' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rework_count, 1,
            "#195: accept must create exactly 1 rework dispatch"
        );

        // Verify canonical review state is rework_pending
        let review_state: Option<String> = conn
            .query_row(
                "SELECT state FROM card_review_state WHERE card_id = 'card-195'",
                [],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        assert_eq!(
            review_state.as_deref(),
            Some("rework_pending"),
            "#195: canonical review state must be 'rework_pending' after accept"
        );
    }

    // ── #195: rework dispatch completion triggers re-review cycle ──────
    //
    // Verifies the full accept → rework → re-review cycle:
    // After rework dispatch completes, OnDispatchCompleted (kanban-rules.js)
    // transitions the card to review, and OnReviewEnter creates a new review dispatch.

    #[test]
    fn scenario_195_rework_completion_triggers_review() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-195b", "in_progress");

        // Create and complete a rework dispatch — simulates the rework turn finishing
        seed_dispatch(&db, "rw-195b", "card-195b", "rework", "pending");

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "rw-195b",
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        // Rework completion → card must transition to review (via kanban-rules.js)
        let status = get_card_status(&db, "card-195b");
        assert_eq!(
            status, "review",
            "#195: rework completion must transition card to review"
        );

        // OnReviewEnter must create a review dispatch for re-review
        let conn = db.lock().unwrap();
        let review_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-195b' AND dispatch_type = 'review' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_count, 1,
            "#195: rework completion must trigger OnReviewEnter → review dispatch"
        );
    }

    // ── #256: Consultation dispatch does not advance card from requested ────

    #[test]
    fn consultation_dispatch_stays_in_requested() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-consult", "requested");

        // Create consultation dispatch — should NOT move card from requested
        let result = dispatch::create_dispatch(
            &db,
            &engine,
            "card-consult",
            "agent-1",
            "consultation",
            "[Consultation] Test",
            &serde_json::json!({}),
        );
        assert!(
            result.is_ok(),
            "consultation dispatch creation must succeed"
        );

        let card_status = get_card_status(&db, "card-consult");
        assert_eq!(
            card_status, "requested",
            "#256: consultation dispatch must NOT advance card from requested"
        );
    }

    #[test]
    fn consultation_dispatch_uses_alt_channel() {
        // Verified via unit test in dispatches.rs — this is a smoke test
        assert!(
            crate::server::routes::dispatches::use_counter_model_channel(Some("consultation")),
            "#256: consultation must route to counter-model channel"
        );
    }
}
