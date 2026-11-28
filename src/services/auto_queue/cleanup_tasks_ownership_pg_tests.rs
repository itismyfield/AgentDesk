use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::auto_queue::cleanup_tasks::{ReleasedSlot, slot_taken_by_foreign_run_pg};
use crate::services::auto_queue::runtime::clear_slot_threads_for_runs_pg;
use crate::services::discord::health::slot_row_host_refusal;

#[tokio::test]
async fn slot_cleanup_preserves_successor_session_after_ownership_probe_pg() {
    crate::services::discord::install_engine_probes();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ('cleanup-owner', 'Cleanup owner', 'claude')",
    )
    .execute(&pool)
    .await
    .expect("seed agent"); // agentdesk-audit: allow-unwrap — test fixture
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status)
         VALUES ('cleanup-old', 'cleanup-owner', 'cancelled'),
                ('cleanup-sibling', 'cleanup-owner', 'cancelled'),
                ('cleanup-new', 'cleanup-owner', 'active')",
    )
    .execute(&pool)
    .await
    .expect("seed cleanup and successor runs"); // agentdesk-audit: allow-unwrap — test fixture
    sqlx::query(
        "INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id, thread_id_map)
         VALUES ('cleanup-owner', 7, 'cleanup-old', '{\"0\":\"7522201\"}'::jsonb),
                ('cleanup-owner', 8, 'cleanup-old', '{}'::jsonb)",
    )
    .execute(&pool)
    .await
    .expect("seed owned slots"); // agentdesk-audit: allow-unwrap — test fixture
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, thread_channel_id, claude_session_id)
         VALUES ('test-host:AgentDesk-claude-cleanup-owner', 'claude', 'idle',
                 '7522201', 'old-session')",
    )
    .execute(&pool)
    .await
    .expect("seed slot session"); // agentdesk-audit: allow-unwrap — test fixture

    let run_ids = vec!["cleanup-old".to_string(), "cleanup-sibling".to_string()];
    let slot = ReleasedSlot {
        agent_id: "cleanup-owner".to_string(),
        slot_index: 7,
    };
    assert!(
        !slot_taken_by_foreign_run_pg(&pool, &run_ids, &slot)
            .await
            .expect("ownership probe passes") // agentdesk-audit: allow-unwrap — test assertion
    );
    // Reassign only after the real probe has returned, before the real clear.
    sqlx::query(
        "UPDATE auto_queue_slots SET assigned_run_id = 'cleanup-new'
         WHERE agent_id = 'cleanup-owner' AND slot_index = 7",
    )
    .execute(&pool)
    .await
    .expect("reassign after probe"); // agentdesk-audit: allow-unwrap — test fixture
    sqlx::query(
        "UPDATE sessions SET claude_session_id = 'successor-session'
         WHERE thread_channel_id = '7522201'",
    )
    .execute(&pool)
    .await
    .expect("successor establishes its session"); // agentdesk-audit: allow-unwrap — test fixture
    let cleared = clear_slot_threads_for_runs_pg(
        slot_row_host_refusal,
        None,
        &pool,
        &slot.agent_id,
        7,
        &run_ids,
    )
    .await
    .expect("foreign ownership is a successful skip"); // agentdesk-audit: allow-unwrap — test assertion
    let session: Option<String> = sqlx::query_scalar(
        "SELECT claude_session_id FROM sessions WHERE thread_channel_id = '7522201'",
    )
    .fetch_one(&pool)
    .await
    .expect("read successor session"); // agentdesk-audit: allow-unwrap — test assertion
    assert_eq!(session.as_deref(), Some("successor-session"));
    assert_eq!(cleared, 0);

    for owner in [None, Some("cleanup-old"), Some("cleanup-sibling")] {
        sqlx::query(
            "UPDATE auto_queue_slots SET assigned_run_id = $1
             WHERE agent_id = 'cleanup-owner' AND slot_index = 7",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .expect("restore eligible slot ownership"); // agentdesk-audit: allow-unwrap — test fixture
        assert_eq!(
            clear_slot_threads_for_runs_pg(
                slot_row_host_refusal,
                None,
                &pool,
                &slot.agent_id,
                7,
                &run_ids,
            )
            .await
            .expect("clear released or task-owned slot"), // agentdesk-audit: allow-unwrap — test assertion
            1,
            "owner {owner:?} must remain eligible"
        );
    }

    pool.close().await;
    pg_db.drop().await;
}

/// #5356 S4 (r2): the upstream activate filter's domain. Its predicate is
/// `is_activate_eligible_run_status`, NOT `is_live_run_status` — activate's
/// primary job is promoting `generated`/`pending` runs, so the live set alone
/// would fail-fast the whole `generate -> dispatch-next` path. These tests
/// pin both halves: promotable runs are accepted end to end, terminal runs
/// are refused.
mod activate_upstream_eligibility_gate_pg_tests {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::auto_queue::route::activate_command::resolve_activate_target_run_id;
    use crate::services::auto_queue::route::{
        ActivateBody, AutoQueueActivateDeps, activate_with_deps_pg,
    };
    use crate::test_support::wait::poll_within;
    use sqlx::{Connection, PgConnection, PgPool};
    use std::sync::Arc;

    type ActivateState = (
        (String, bool, i64),
        (String, Option<String>, Option<i64>),
        (Option<String>, Option<i64>, Option<String>),
        i64,
    );

    async fn seed_activate_state(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO auto_queue_runs
                    (id, repo, agent_id, status, max_concurrent_threads, thread_group_count)
                 VALUES ('activate-live-run', 'repo-1', 'activate-agent', 'active', 2, 1)",
        )
        .execute(pool)
        .await
        .expect("seed activate run");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
                 VALUES ('activate-agent', 'Activate Agent', 'claude', '5356')",
        )
        .execute(pool)
        .await
        .expect("seed activate agent");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id)
                 VALUES ('activate-card', 'Activate Card', 'in_progress', 'activate-agent', 'repo-1')",
        )
        .execute(pool)
        .await
        .expect("seed activate card");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                    (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
                 VALUES ('activate-entry', 'activate-live-run', 'activate-card',
                         'activate-agent', 'pending', 0, 0)",
        )
        .execute(pool)
        .await
        .expect("seed activate entry");
        sqlx::query(
            "INSERT INTO auto_queue_slots
                    (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
                 VALUES ('activate-agent', 0, 'activate-live-run', 0, '{}'::jsonb)",
        )
        .execute(pool)
        .await
        .expect("seed activate slot");
    }

    fn activate_body() -> ActivateBody {
        ActivateBody {
            run_id: Some("activate-live-run".to_string()),
            repo: None,
            agent_id: None,
            thread_group: None,
            unified_thread: None,
            active_only: None,
        }
    }

    /// The implicit shape the CLI `generate -> dispatch-next` flow sends: no
    /// `run_id`, only repo/agent, so `resolve_activate_target_run_id` picks
    /// the newest run matching its `status_clause`.
    fn implicit_activate_body() -> ActivateBody {
        ActivateBody {
            run_id: None,
            repo: Some("repo-1".to_string()),
            agent_id: Some("activate-agent".to_string()),
            thread_group: None,
            unified_thread: None,
            active_only: None,
        }
    }

    fn activate_deps(pool: &PgPool) -> AutoQueueActivateDeps {
        let config = crate::config::Config::default();
        let engine = crate::engine::PolicyEngine::new_with_pg(
            &config,
            Some(pool.clone()),
            crate::services::discord::health::slot_row_host_refusal,
        )
        .expect("create activate policy engine");
        AutoQueueActivateDeps {
            pg_pool: Some(pool.clone()),
            slot_row_guard: engine.slot_row_guard(),
            engine,
            config: Arc::new(config),
            health_registry: None,
            guild_id: None,
        }
    }

    async fn set_run_status(pool: &PgPool, status: &str) {
        crate::db::status_check_test_support::execute_with_status_check_lifted(
            pool,
            "auto_queue_runs",
            &format!(
                "UPDATE auto_queue_runs SET status = '{status}' WHERE id = 'activate-live-run'"
            ),
        )
        .await;
    }

    async fn run_status(pool: &PgPool) -> String {
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM auto_queue_runs WHERE id = 'activate-live-run'",
        )
        .fetch_one(pool)
        .await
        .expect("load activate run status")
    }

    async fn entry_status(pool: &PgPool) -> String {
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM auto_queue_entries WHERE id = 'activate-entry'",
        )
        .fetch_one(pool)
        .await
        .expect("load activate entry status")
    }

    /// Seed a live (dispatched) implementation dispatch as the card's latest
    /// so the activate loop takes its `has_active_dispatch()` attach branch.
    /// Creating a fresh dispatch would need a resolvable git worktree; this
    /// keeps the post-promotion step observable without worktree infra, the
    /// same technique `depth_gate_activate_pg_tests` uses.
    async fn seed_attachable_impl_dispatch(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
                 VALUES ('activate-impl-existing', 'activate-card', 'activate-agent',
                         'implementation', 'dispatched', 'Impl')",
        )
        .execute(pool)
        .await
        .expect("seed attachable impl dispatch");
        sqlx::query(
            "UPDATE kanban_cards
                 SET latest_dispatch_id = 'activate-impl-existing'
                 WHERE id = 'activate-card'",
        )
        .execute(pool)
        .await
        .expect("point activate card at the attachable dispatch");
    }

    async fn activate_state(pool: &PgPool) -> ActivateState {
        let run = sqlx::query_as::<_, (String, bool, i64)>(
            "SELECT status, completed_at IS NOT NULL, max_concurrent_threads::BIGINT
                 FROM auto_queue_runs WHERE id = 'activate-live-run'",
        )
        .fetch_one(pool)
        .await
        .expect("load activate run state");
        let entry = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
            "SELECT status, dispatch_id, slot_index::BIGINT
                 FROM auto_queue_entries WHERE id = 'activate-entry'",
        )
        .fetch_one(pool)
        .await
        .expect("load activate entry state");
        let slot = sqlx::query_as::<_, (Option<String>, Option<i64>, Option<String>)>(
            "SELECT assigned_run_id, assigned_thread_group::BIGINT, thread_id_map::TEXT
                 FROM auto_queue_slots WHERE agent_id = 'activate-agent' AND slot_index = 0",
        )
        .fetch_one(pool)
        .await
        .expect("load activate slot state");
        let dispatch_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM task_dispatches WHERE kanban_card_id = 'activate-card'",
        )
        .fetch_one(pool)
        .await
        .expect("count activate card dispatches");
        (run, entry, slot, dispatch_count)
    }

    #[tokio::test]
    async fn explicit_run_id_admits_eligible_and_rejects_terminal_statuses_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_activate_state(&pool).await;
        let body = activate_body();

        for eligible_status in crate::db::auto_queue::run_status::ACTIVATE_ELIGIBLE_RUN_STATUSES {
            set_run_status(&pool, eligible_status).await;
            assert_eq!(
                resolve_activate_target_run_id(&pool, &body, false)
                    .await
                    .unwrap_or_else(|(status, payload)| panic!(
                        "explicit {eligible_status} run must stay eligible, got {status}: {}",
                        payload.0
                    )),
                "activate-live-run"
            );
        }

        // `completed` and `cancelled` are the only terminal statuses
        // production writes to `auto_queue_runs.status`. `failed` and
        // `user_cancelled` are entry/phase-gate statuses that never reach a
        // run row; they are exercised here to pin the predicate's
        // deny-by-default shape — anything outside the allowlist is refused.
        for terminal_status in ["completed", "cancelled", "failed", "user_cancelled"] {
            set_run_status(&pool, terminal_status).await;
            let (status, payload) = resolve_activate_target_run_id(&pool, &body, false)
                .await
                .expect_err("explicit terminal run must be rejected");
            assert_eq!(status, axum::http::StatusCode::CONFLICT);
            assert_eq!(
                payload.0["error"],
                format!(
                    "activate requires a non-terminal auto-queue run: run_id=activate-live-run, status={terminal_status}"
                )
            );
        }

        pool.close().await;
        pg_db.drop().await;
    }

    /// #5356 S4 (r2, P2-a): the explicit path gained a 404 it never had. An
    /// unknown `run_id` used to fall through to the promote/dispatch phases
    /// as a silent no-op; it is now refused by name.
    #[tokio::test]
    async fn explicit_unknown_run_id_is_not_found_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_activate_state(&pool).await;
        let body = ActivateBody {
            run_id: Some("activate-absent-run".to_string()),
            ..activate_body()
        };

        let (status, payload) = resolve_activate_target_run_id(&pool, &body, false)
            .await
            .expect_err("unknown run id must be rejected");
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(
            payload.0["error"],
            "auto-queue run not found for activate: activate-absent-run"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// #5356 S4 (r2, P0 regression lock): a `generated` run — what
    /// `route_generate` inserts — must survive the upstream filter and be
    /// promoted to `active`, all the way to a dispatched entry. Gating this
    /// call on `is_live_run_status` made every `generate -> activate` 409.
    #[tokio::test]
    async fn generated_run_explicit_activate_promotes_and_dispatches_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_activate_state(&pool).await;
        seed_attachable_impl_dispatch(&pool).await;
        set_run_status(&pool, "generated").await;

        let deps = activate_deps(&pool);
        let (status, _payload) = activate_with_deps_pg(&deps, activate_body())
            .await
            .expect("explicit activate of a generated run must not be refused upstream");

        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            run_status(&pool).await,
            "active",
            "activate must promote the generated run it was given"
        );
        assert_eq!(
            entry_status(&pool).await,
            "dispatched",
            "promotion must carry through the downstream dispatch choke point"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// #5356 S4 (r2, P0 regression lock): the implicit path selects runs with
    /// `status IN ('active', 'generated', 'pending')`. A `pending` run picked
    /// by that clause must not then be refused by the upstream filter — the
    /// two predicates have to agree.
    #[tokio::test]
    async fn pending_run_implicit_activate_promotes_and_dispatches_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_activate_state(&pool).await;
        seed_attachable_impl_dispatch(&pool).await;
        set_run_status(&pool, "pending").await;

        let deps = activate_deps(&pool);
        let (status, _payload) = activate_with_deps_pg(&deps, implicit_activate_body())
            .await
            .expect("implicit activate of a pending run must not be refused upstream");

        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            run_status(&pool).await,
            "active",
            "activate must promote the pending run its own selection clause chose"
        );
        assert_eq!(
            entry_status(&pool).await,
            "dispatched",
            "promotion must carry through the downstream dispatch choke point"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// #5356 S4 (r3, P2): measured against the real activate entry point,
    /// explicit `active_only=true` leaves a promotable run unpromoted and
    /// returns a harmless zero-dispatch success for both pre-active states.
    #[tokio::test]
    async fn active_only_explicit_promotable_run_is_harmless_no_op_pg() {
        for promotable_status in ["generated", "pending"] {
            let pg_db = TestPostgresDb::create().await;
            let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
            seed_activate_state(&pool).await;
            set_run_status(&pool, promotable_status).await;

            let deps = activate_deps(&pool);
            let body = ActivateBody {
                active_only: Some(true),
                ..activate_body()
            };
            let (status, payload) = activate_with_deps_pg(&deps, body)
                .await
                .expect("active_only promotable run must remain a harmless no-op");

            assert_eq!(status, axum::http::StatusCode::OK);
            assert_eq!(
                payload.0,
                serde_json::json!({
                    "dispatched": [],
                    "count": 0,
                    "active_groups": 0,
                    "active_turn_count": 0,
                    "pending_groups": 1,
                })
            );
            assert_eq!(
                run_status(&pool).await,
                promotable_status,
                "active_only must skip promotion"
            );
            assert_eq!(
                entry_status(&pool).await,
                "pending",
                "active_only must not dispatch the promotable run's entry"
            );
            assert_eq!(
                activate_state(&pool).await.3,
                0,
                "active_only must not create a dispatch for a promotable run"
            );

            pool.close().await;
            pg_db.drop().await;
        }
    }

    #[tokio::test]
    async fn terminal_race_rejects_after_activate_lock_without_side_effects_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_activate_state(&pool).await;

        let mut holder = PgConnection::connect(&pg_db.database_url)
            .await
            .expect("connect activate lock holder");
        sqlx::query("SELECT pg_advisory_lock(hashtext($1), hashtext($2))")
            .bind("aq_activate")
            .bind("activate-live-run")
            .execute(&mut holder)
            .await
            .expect("hold activate advisory lock");

        let deps = activate_deps(&pool);
        let activate =
            tokio::spawn(async move { activate_with_deps_pg(&deps, activate_body()).await });
        poll_within(
            async || {
                let conn = &mut holder;
                sqlx::query("SELECT pg_stat_clear_snapshot()")
                    .execute(&mut *conn)
                    .await
                    .expect("clear activate backend-status snapshot");
                let waiting = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (
                     SELECT 1 FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND wait_event_type = 'Lock'
                       AND query LIKE '%pg_advisory_lock(hashtext($1), hashtext($2))%'
                 )",
                )
                .fetch_one(&mut *conn)
                .await
                .expect("inspect activate lock waiter");
                if waiting {
                    Ok(())
                } else {
                    Err("activate did not wait for its per-run advisory lock".into())
                }
            },
            std::time::Duration::from_secs(5),
            "PostgreSQL lock waiter",
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE auto_queue_runs
                 SET status = 'completed', completed_at = NOW()
                 WHERE id = 'activate-live-run'",
        )
        .execute(&pool)
        .await
        .expect("complete run while activate waits");
        let terminal_state = activate_state(&pool).await;

        sqlx::query("SELECT pg_advisory_unlock(hashtext($1), hashtext($2))")
            .bind("aq_activate")
            .bind("activate-live-run")
            .execute(&mut holder)
            .await
            .expect("release activate advisory lock");
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), activate)
            .await
            .expect("activate completes after lock release")
            .expect("join activate task")
            .expect_err("post-lock eligibility check must reject the completed run");
        assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
        assert_eq!(
            error.message(),
            "activate requires a non-terminal auto-queue run: run_id=activate-live-run, status=completed"
        );
        assert_eq!(
            activate_state(&pool).await,
            terminal_state,
            "post-lock rejection must leave run, entry, slot, and dispatch state unchanged"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
