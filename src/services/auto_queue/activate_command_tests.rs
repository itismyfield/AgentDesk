use crate::services::auto_queue::route::activate_command::activate_fallback_capacity_reached;

#[test]
fn fallback_capacity_counts_only_new_dispatches_this_activate() {
    let active_turn_count = 2;
    let active_group_count = 2;
    let max_concurrent = 3;

    assert!(
        !activate_fallback_capacity_reached(
            active_turn_count,
            active_group_count,
            0,
            max_concurrent,
        ),
        "visiting already-active or reattached groups must not consume the remaining slot"
    );
    assert!(
        activate_fallback_capacity_reached(
            active_turn_count,
            active_group_count,
            1,
            max_concurrent,
        ),
        "one newly-created dispatch should consume the last available slot"
    );
}

/// #3605 (codex R2) FIX1 regression: the auto-queue activate attach decision
/// must treat an inert side-path (consultation / scope-assessment) that has
/// become a card's `latest_dispatch_id` as NON-attachable, so a pending
/// auto_queue implementation entry is not hijacked onto the side-path
/// dispatch (which would leave it stuck in `dispatched` — the side-path
/// terminal completion skips the auto-queue terminal sync, and stale recovery
/// only reclaims cancelled/failed entries).
///
/// These exercise the REAL production loader `load_activate_card_state_pg`
/// against real Postgres rows, then the `has_active_dispatch()` predicate the
/// activate loop branches on at `activate_with_deps_pg` (the
/// attach-vs-create-impl-dispatch fork). The module is `*_pg_tests` so its
/// tests are skipped via `--skip _pg_` when no local Postgres is configured.
mod side_path_hijack_pg_tests {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::auto_queue::route::view::load_activate_card_state_pg;
    use sqlx::PgPool;

    async fn seed_base(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, max_concurrent_threads)
             VALUES ('run-1', 'repo-1', 'agent-1', 'active', 2)",
        )
        .execute(pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-1', 'Agent 1', 'claude', '123')",
        )
        .execute(pool)
        .await
        .expect("seed agent");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id)
             VALUES ('card-1', 'Card 1', 'requested', 'agent-1', 'repo-1')",
        )
        .execute(pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-1', 'run-1', 'card-1', 'agent-1', 'pending', 0, 0)",
        )
        .execute(pool)
        .await
        .expect("seed pending entry");
    }

    /// Attaches a live (dispatched) dispatch of `dispatch_type` to `card-1` and
    /// points the card's `latest_dispatch_id` at it — exactly the state
    /// `engine::transition::decide_dispatch_attached` produces when a dispatch is
    /// attached to a card.
    async fn attach_live_dispatch(pool: &PgPool, dispatch_id: &str, dispatch_type: &str) {
        sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, 'card-1', 'agent-1', $2, 'dispatched', 'D')",
    )
    .bind(dispatch_id)
    .bind(dispatch_type)
    .execute(pool)
    .await
    .expect("seed dispatch");
        sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = 'card-1'")
            .bind(dispatch_id)
            .execute(pool)
            .await
            .expect("set latest_dispatch_id");
    }

    #[tokio::test]
    async fn scope_assessment_latest_dispatch_is_not_attachable_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_base(&pool).await;
        // A scope-assessment side-path is in-flight and is the card's
        // latest_dispatch_id (status `dispatched`) while the impl entry is still
        // pending — the exact hijack-prone state.
        attach_live_dispatch(&pool, "dispatch-scope", "scope-assessment").await;

        let state = load_activate_card_state_pg(&pool, "card-1", "entry-1")
            .await
            .expect("load card state");

        // The scope-assessment IS the latest dispatch and IS live...
        assert_eq!(state.latest_dispatch_id.as_deref(), Some("dispatch-scope"));
        assert_eq!(state.latest_dispatch_status.as_deref(), Some("dispatched"));
        assert_eq!(
            state.latest_dispatch_type.as_deref(),
            Some("scope-assessment")
        );
        // ...but it must NOT be treated as an attachable implementation dispatch,
        // so the activate loop falls through to creating a real impl dispatch
        // instead of binding the pending entry to the side-path (the #3605 root
        // bug: entry hijacked → stuck `dispatched`).
        assert!(
            !state.has_active_dispatch(),
            "scope-assessment side-path must not be an attachable active dispatch"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn consultation_latest_dispatch_is_not_attachable_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_base(&pool).await;
        // Parity guard: consultation (the original side-path) is treated
        // identically — even if it ever reached activate with a pending entry.
        attach_live_dispatch(&pool, "dispatch-consult", "consultation").await;

        let state = load_activate_card_state_pg(&pool, "card-1", "entry-1")
            .await
            .expect("load card state");

        assert_eq!(state.latest_dispatch_type.as_deref(), Some("consultation"));
        assert!(
            !state.has_active_dispatch(),
            "consultation side-path must not be an attachable active dispatch"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn implementation_latest_dispatch_is_attachable_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_base(&pool).await;
        // Positive control: a genuine live implementation dispatch IS attachable
        // — the side-path exclusion must not over-fire and suppress the normal
        // idempotent reattach path.
        attach_live_dispatch(&pool, "dispatch-impl", "implementation").await;

        let state = load_activate_card_state_pg(&pool, "card-1", "entry-1")
            .await
            .expect("load card state");

        assert_eq!(
            state.latest_dispatch_type.as_deref(),
            Some("implementation")
        );
        assert!(
            state.has_active_dispatch(),
            "a live implementation dispatch must remain an attachable active dispatch"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn scope_assessment_completion_does_not_finalize_pending_entry_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        seed_base(&pool).await;
        attach_live_dispatch(&pool, "dispatch-scope", "scope-assessment").await;

        // FIX1 keeps the entry from ever binding to the side-path, so the entry
        // stays `pending` (not `dispatched`). Completing the scope-assessment
        // then runs the terminal-sync, which must NOT reach back and finalize the
        // still-pending impl entry as `done` (that would close the card with no
        // implementation ever run).
        sqlx::query(
        "UPDATE task_dispatches SET status = 'completed', completed_at = NOW() WHERE id = 'dispatch-scope'",
    )
    .execute(&pool)
    .await
    .expect("complete scope-assessment");

        let mut tx = pool.begin().await.expect("begin tx");
        let changed =
            crate::services::auto_queue::transactions::sync_dispatch_terminal_entries_on_pg_tx(
                &mut tx,
                "dispatch-scope",
                crate::db::auto_queue::ENTRY_STATUS_DONE,
                "test_scope_assessment_completion",
                false,
            )
            .await
            .expect("terminal sync");
        tx.commit().await.expect("commit tx");

        assert_eq!(
            changed, 0,
            "side-path completion must not finalize the pending impl entry"
        );
        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-1'")
                .fetch_one(&pool)
                .await
                .expect("entry status");
        assert_eq!(
            entry_status, "pending",
            "impl entry must remain pending — not stuck `dispatched` nor wrongly `done`"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // #3594 (T3) impl-before-depth race gate: the EFFECTIVE row-counting tests
    // that drive the real `activate_with_deps_pg` live in the sibling module
    // `depth_gate_activate_pg_tests` below. The earlier predicate-only tests
    // (asserting just `scope_assessment_pending()` on a loaded state without
    // running activate or counting dispatch rows) were vacuous — they passed
    // even with the loop guard deleted — and have been replaced.
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

/// #3594 (T3, codex Finding 4): EFFECTIVE race-gate tests. The previous
/// `scope_assessment_*_pg` tests only asserted the `scope_assessment_pending()`
/// PREDICATE on a loaded `ActivateCardState` — they never ran the real activate
/// entry point, so deleting the loop guard left them green (vacuous).
///
/// These drive the REAL `activate_with_deps_pg` against real Postgres rows and
/// assert on what the gate actually controls. Creating a *fresh* implementation
/// dispatch needs a resolvable git worktree a unit fixture cannot cheaply
/// provide, so to keep the post-gate step observable WITHOUT worktree infra each
/// card is seeded with a pre-existing live (dispatched) implementation dispatch.
/// The activate loop's order is: scope-pending guard FIRST, then the
/// `has_active_dispatch()` attach step. So:
///   - scope pending  → the `if scope_assessment_pending() { continue; }` guard
///                       fires BEFORE the attach step: the pending entry is NOT
///                       bound (stays `pending`), and no NEW impl dispatch row is
///                       created (exactly one pre-existing impl row remains).
///   - scope completed → the guard does NOT fire, so the loop reaches the attach
///                       step and binds the entry to the existing impl dispatch
///                       (entry → `dispatched`).
///   - no scope-assessment key → core no-regression: identical to "completed"
///                       (absent ≠ pending) — entry binds.
///
/// Mutation check: deleting the loop guard makes the "pending" case fall through
/// to the attach step too → its entry becomes `dispatched` → the pending-case
/// `entry stays pending` assertion fails. (Verified by hand.)
mod depth_gate_activate_pg_tests {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::auto_queue::route::{
        ActivateBody, AutoQueueActivateDeps, activate_with_deps_pg,
    };
    use sqlx::PgPool;
    use std::sync::Arc;

    async fn seed_run_card_entry(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, max_concurrent_threads, thread_group_count)
                 VALUES ('run-dg', 'repo-1', 'agent-dg', 'active', 2, 1)",
        )
        .execute(pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
                 VALUES ('agent-dg', 'Agent DG', 'claude', '999')",
        )
        .execute(pool)
        .await
        .expect("seed agent");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id)
                 VALUES ('card-dg', 'Card DG', 'in_progress', 'agent-dg', 'repo-1')",
        )
        .execute(pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                    (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
                 VALUES ('entry-dg', 'run-dg', 'card-dg', 'agent-dg', 'pending', 0, 0)",
        )
        .execute(pool)
        .await
        .expect("seed pending entry");
        // Assign the agent's slot 0 to this run+group so the activate planner's
        // `assigned_groups_with_pending_entries_pg` selects the group and the
        // per-entry loop runs for `entry-dg`.
        sqlx::query(
            "INSERT INTO auto_queue_slots
                    (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
                 VALUES ('agent-dg', 0, 'run-dg', 0, CAST('{}' AS jsonb))",
        )
        .execute(pool)
        .await
        .expect("seed assigned slot");
    }

    /// Seed a live (dispatched) IMPLEMENTATION dispatch as the card's latest, so
    /// the activate loop's attach path (`has_active_dispatch()` true) can bind
    /// the entry to it WITHOUT creating a fresh worktree-requiring dispatch.
    async fn seed_existing_impl_dispatch(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
                 VALUES ('impl-existing', 'card-dg', 'agent-dg', 'implementation', 'dispatched', 'Impl')",
        )
        .execute(pool)
        .await
        .expect("seed existing impl dispatch");
        sqlx::query(
            "UPDATE kanban_cards SET latest_dispatch_id = 'impl-existing' WHERE id = 'card-dg'",
        )
        .execute(pool)
        .await
        .expect("set latest_dispatch_id");
    }

    async fn set_scope_status(pool: &PgPool, status: Option<&str>) {
        let metadata = match status {
            Some(s) => serde_json::json!({ "scope_assessment_status": s }).to_string(),
            None => "{}".to_string(),
        };
        sqlx::query("UPDATE kanban_cards SET metadata = $1::jsonb WHERE id = 'card-dg'")
            .bind(metadata)
            .execute(pool)
            .await
            .expect("set scope metadata");
    }

    fn make_deps(pool: &PgPool) -> AutoQueueActivateDeps {
        let config = crate::config::Config::default();
        let engine = crate::engine::PolicyEngine::new_with_pg(
            &config,
            Some(pool.clone()),
            crate::services::discord::health::slot_row_host_refusal,
        )
        .expect("test policy engine");
        AutoQueueActivateDeps {
            pg_pool: Some(pool.clone()),
            slot_row_guard: engine.slot_row_guard(),
            engine,
            config: Arc::new(config),
            health_registry: None,
            guild_id: None,
        }
    }

    fn activate_body() -> ActivateBody {
        ActivateBody {
            run_id: Some("run-dg".to_string()),
            repo: None,
            agent_id: None,
            thread_group: None,
            unified_thread: None,
            active_only: None,
        }
    }

    async fn impl_dispatch_row_count(pool: &PgPool) -> i64 {
        dispatch_row_count_of_type(pool, "implementation").await
    }

    /// #3594 (T3, codex R2 Finding 1): count dispatch rows of an arbitrary
    /// `dispatch_type` for `card-dg`. Used by the late-entry depth-aware tests
    /// to assert that activate created a `plan` (or did NOT) instead of a
    /// plain `implementation`.
    async fn dispatch_row_count_of_type(pool: &PgPool, dispatch_type: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM task_dispatches
                 WHERE kanban_card_id = 'card-dg' AND dispatch_type = $1",
        )
        .bind(dispatch_type)
        .fetch_one(pool)
        .await
        .expect("count dispatches of type")
    }

    /// #3594 (T3, codex R2 Finding 1): set BOTH `scope_assessment_status` and
    /// `scope_depth` on `card-dg` metadata — the late-entry state where the
    /// scope-assessment already COMPLETED (so the scope-pending gate is
    /// released) and recorded a depth, exactly as
    /// `kanban-scope-assessment._recordScopeAssessment` writes it.
    async fn set_scope_completed_with_depth(pool: &PgPool, depth: &str) {
        let metadata = serde_json::json!({
            "scope_assessment_status": "completed",
            "scope_depth": depth,
        })
        .to_string();
        sqlx::query("UPDATE kanban_cards SET metadata = $1::jsonb WHERE id = 'card-dg'")
            .bind(metadata)
            .execute(pool)
            .await
            .expect("set scope metadata with depth");
    }

    /// #3594 (T3, codex R2 Finding 1): seed a COMPLETED `plan` dispatch so the
    /// `has_completed_plan_dispatch` guard fires — the "plan already finished"
    /// idempotency case where activate must advance to `implementation`, not
    /// re-create a plan.
    async fn seed_completed_plan_dispatch(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, completed_at)
                 VALUES ('plan-done', 'card-dg', 'agent-dg', 'plan', 'completed', 'Plan', NOW())",
        )
        .execute(pool)
        .await
        .expect("seed completed plan dispatch");
    }

    async fn entry_status(pool: &PgPool) -> String {
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-dg'",
        )
        .fetch_one(pool)
        .await
        .expect("entry status")
    }

    #[tokio::test]
    async fn scope_pending_defers_entry_not_bound_no_new_impl_row_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_run_card_entry(&pool).await;
        seed_existing_impl_dispatch(&pool).await;
        // scope-assessment in flight → the loop's scope-pending guard fires
        // BEFORE the attach step, so the entry is never bound.
        set_scope_status(&pool, Some("pending")).await;

        let deps = make_deps(&pool);
        let (status, _body) = activate_with_deps_pg(&deps, activate_body())
            .await
            .expect("activate response");
        assert_eq!(status, axum::http::StatusCode::OK);

        // The gate fired: the entry was NOT bound to the existing impl dispatch.
        assert_eq!(
            entry_status(&pool).await,
            "pending",
            "scope-assessment pending must defer: the entry stays pending (not attached)"
        );
        // And no NEW impl dispatch was created — only the seeded one remains.
        assert_eq!(
            impl_dispatch_row_count(&pool).await,
            1,
            "no new implementation dispatch must be created while scope is pending"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn scope_completed_releases_gate_entry_binds_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_run_card_entry(&pool).await;
        seed_existing_impl_dispatch(&pool).await;
        // scope-assessment completed → the gate releases; the loop reaches the
        // attach step and binds the entry to the existing impl dispatch.
        set_scope_status(&pool, Some("completed")).await;

        let deps = make_deps(&pool);
        let (status, _body) = activate_with_deps_pg(&deps, activate_body())
            .await
            .expect("activate response");
        assert_eq!(status, axum::http::StatusCode::OK);

        assert_eq!(
            entry_status(&pool).await,
            "dispatched",
            "completed scope-assessment must release the gate so the entry binds to the impl dispatch"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn card_without_scope_assessment_entry_binds_no_regression_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_run_card_entry(&pool).await;
        seed_existing_impl_dispatch(&pool).await;
        // CORE NO-REGRESSION: a card that never ran scope-assessment has NO
        // scope_assessment_status key. absent ≠ pending → the gate must NOT fire
        // and the entry binds exactly as before T3.
        set_scope_status(&pool, None).await;

        let deps = make_deps(&pool);
        let (status, _body) = activate_with_deps_pg(&deps, activate_body())
            .await
            .expect("activate response");
        assert_eq!(status, axum::http::StatusCode::OK);

        assert_eq!(
            entry_status(&pool).await,
            "dispatched",
            "a card with no scope-assessment must bind the entry (no regression)"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    // ── #3594 (T3, codex R2 Finding 1): LATE-ENTRY depth-aware tests ──────
    //
    // The "late entry" ordering: a card runs scope-assessment FIRST (so
    // `/queue/generate` skipped it while the scope dispatch was
    // pending/dispatched). scope-assessment COMPLETES → its JS resume + the
    // by-card fallback find NO auto-queue entry yet (it was never generated).
    // THEN `/queue/generate` finally creates the entry (`pending`) — it
    // reaches activate with `scope_assessment_status == "completed"` (gate
    // released) and NO active dispatch. Pre-fix, activate created a plain
    // `implementation`, bypassing the depth gate. These assert activate is now
    // depth-aware: it creates a `plan` for full/plan_only (mirroring the JS
    // `_resolveScopeFlow`), and stays on `implementation` for direct / absent
    // / plan-already-done (no regression).
    //
    // The full/plan_only cards seed NO existing dispatch, so activate reaches
    // the CREATE path; `plan` does not require a fresh worktree
    // (dispatch_type_requires_fresh_worktree = implementation|rework only), so
    // the plan dispatch is creatable in a PG-only fixture. The direct/absent
    // cards seed an existing live `implementation` dispatch so activate
    // ATTACHES to it (proving it chose `implementation`) without needing
    // worktree infra to create a fresh impl.

    async fn plan_dispatch_row_count(pool: &PgPool) -> i64 {
        dispatch_row_count_of_type(pool, "plan").await
    }

    /// Read the `scope_depth` carried on the newly-created `plan` dispatch
    /// context — must equal the card's recorded depth so the JS
    /// plan-completion arm branches (plan_only→impl, full→plan-review)
    /// without re-reading metadata.
    async fn latest_plan_dispatch_scope_depth(pool: &PgPool) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT context::jsonb->>'scope_depth'
                 FROM task_dispatches
                 WHERE kanban_card_id = 'card-dg' AND dispatch_type = 'plan'
                 ORDER BY created_at DESC
                 LIMIT 1",
        )
        .fetch_one(pool)
        .await
        .expect("read plan dispatch scope_depth")
    }

    #[tokio::test]
    async fn late_entry_full_depth_creates_plan_not_impl_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_run_card_entry(&pool).await;
        // No existing dispatch → activate reaches the create path.
        set_scope_completed_with_depth(&pool, "full").await;

        let deps = make_deps(&pool);
        let (status, _body) = activate_with_deps_pg(&deps, activate_body())
            .await
            .expect("activate response");
        assert_eq!(status, axum::http::StatusCode::OK);

        // Depth-aware: a `plan` dispatch was created, NOT a plain impl.
        assert_eq!(
            plan_dispatch_row_count(&pool).await,
            1,
            "full late entry must create a depth-gated `plan` dispatch"
        );
        assert_eq!(
            impl_dispatch_row_count(&pool).await,
            0,
            "full late entry must NOT create a plain `implementation` dispatch (depth gate bypass)"
        );
        // The resolved depth rides on the plan context so the JS
        // plan-completion arm fans out to plan-review (full) without re-reading
        // card metadata.
        assert_eq!(
            latest_plan_dispatch_scope_depth(&pool).await.as_deref(),
            Some("full"),
            "plan dispatch must carry scope_depth=full for the JS plan-completion fan-out"
        );
        // The late entry was bound to the plan dispatch (not left pending).
        assert_eq!(entry_status(&pool).await, "dispatched");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn late_entry_plan_only_depth_creates_plan_not_impl_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_run_card_entry(&pool).await;
        set_scope_completed_with_depth(&pool, "plan_only").await;

        let deps = make_deps(&pool);
        let (status, _body) = activate_with_deps_pg(&deps, activate_body())
            .await
            .expect("activate response");
        assert_eq!(status, axum::http::StatusCode::OK);

        assert_eq!(
            plan_dispatch_row_count(&pool).await,
            1,
            "plan_only late entry must create a `plan` dispatch"
        );
        assert_eq!(
            impl_dispatch_row_count(&pool).await,
            0,
            "plan_only late entry must NOT create a plain `implementation` dispatch"
        );
        // plan_only must still carry its depth so the plan-completion arm goes
        // plan→impl (NOT plan-review). The activate side does not itself create
        // plan-review; it only seeds the plan stage with the correct depth.
        assert_eq!(
            latest_plan_dispatch_scope_depth(&pool).await.as_deref(),
            Some("plan_only"),
            "plan dispatch must carry scope_depth=plan_only"
        );
        assert_eq!(entry_status(&pool).await, "dispatched");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn late_entry_direct_depth_creates_impl_no_plan_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_run_card_entry(&pool).await;
        // direct → fast track. Seed an existing live impl so activate ATTACHES
        // (no worktree-requiring fresh-impl create) — proving it chose the
        // `implementation` path, NOT `plan`.
        seed_existing_impl_dispatch(&pool).await;
        set_scope_completed_with_depth(&pool, "direct").await;

        let deps = make_deps(&pool);
        let (status, _body) = activate_with_deps_pg(&deps, activate_body())
            .await
            .expect("activate response");
        assert_eq!(status, axum::http::StatusCode::OK);

        // No plan stage for direct.
        assert_eq!(
            plan_dispatch_row_count(&pool).await,
            0,
            "direct late entry must NOT create a `plan` dispatch (fast track)"
        );
        // It attached to the existing impl (the `implementation` decision).
        assert_eq!(
            entry_status(&pool).await,
            "dispatched",
            "direct late entry must bind to the implementation dispatch"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn late_entry_full_depth_with_completed_plan_does_not_create_new_plan_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_run_card_entry(&pool).await;
        // full depth, but a plan ALREADY completed AND the impl is already
        // live. activate attaches the entry to the existing impl (the
        // `has_active_dispatch()` branch fires before the create branch), so it
        // must NOT spin up a SECOND plan. This pins the end-to-end invariant
        // "a finished plan is never re-run by activate"; the create-path
        // decision for the same state (completed plan → `implementation`) is
        // unit-tested directly in `view::activate_next_dispatch_type_tests`
        // (which needs no worktree infra).
        seed_completed_plan_dispatch(&pool).await;
        seed_existing_impl_dispatch(&pool).await;
        set_scope_completed_with_depth(&pool, "full").await;

        let deps = make_deps(&pool);
        let (status, _body) = activate_with_deps_pg(&deps, activate_body())
            .await
            .expect("activate response");
        assert_eq!(status, axum::http::StatusCode::OK);

        // No NEW plan was created — only the pre-existing completed plan remains.
        assert_eq!(
            plan_dispatch_row_count(&pool).await,
            1,
            "a completed plan must not be re-created (idempotency)"
        );
        assert_eq!(
            entry_status(&pool).await,
            "dispatched",
            "with a completed plan + live impl, activate binds the entry to the implementation dispatch"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Core no-regression, explicit plan-absence form: a card that never ran
    /// scope-assessment (no `scope_depth` key) must NEVER get a `plan` stage
    /// from activate — it stays on the plain `implementation` path exactly as
    /// before T3. (The sibling
    /// `card_without_scope_assessment_entry_binds_no_regression_pg` asserts the
    /// entry binds; this adds the `plan` must NOT exist invariant.)
    ///
    /// Mutation check: if `activate_next_dispatch_type` dropped the
    /// `scope_depth.is_none()` short-circuit and treated absent as plan-worthy,
    /// this test would see a `plan` dispatch created and fail.
    #[tokio::test]
    async fn absent_scope_depth_never_creates_plan_no_regression_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        seed_run_card_entry(&pool).await;
        seed_existing_impl_dispatch(&pool).await;
        // No scope metadata at all (absent ≠ scope-assessed).
        set_scope_status(&pool, None).await;

        let deps = make_deps(&pool);
        let (status, _body) = activate_with_deps_pg(&deps, activate_body())
            .await
            .expect("activate response");
        assert_eq!(status, axum::http::StatusCode::OK);

        assert_eq!(
            plan_dispatch_row_count(&pool).await,
            0,
            "a card that never ran scope-assessment must NEVER get a plan stage from activate"
        );
        assert_eq!(entry_status(&pool).await, "dispatched");

        pool.close().await;
        pg_db.drop().await;
    }
}
