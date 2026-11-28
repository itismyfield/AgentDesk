//! Fresh issue worktrees created while a dispatch is being made: hook deadline and baseline.

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::git::test_origin_fixture::{OriginFixture, git};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::Path;

const AGENT: &str = "agent-fresh-wt";

async fn seed_agent(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id) VALUES ($1, 'Fresh WT', '123456789012345678')",
    )
    .bind(AGENT)
    .execute(pool)
    .await
    .expect("seed agent");
}

async fn seed_card(pool: &PgPool, card_id: &str, issue_number: i64, repo_dir: &str) {
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, github_issue_number, repo_id, assigned_agent_id)
         VALUES ($1, 'Fresh worktree card', 'requested', $2, $3, $4)",
    )
    .bind(card_id)
    .bind(issue_number)
    .bind(repo_dir)
    .bind(AGENT)
    .execute(pool)
    .await
    .expect("seed card");
}

async fn dispatch_context(pool: &PgPool, dispatch_id: &str) -> Value {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT context FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_one(pool)
            .await
            .expect("dispatch context");
    serde_json::from_str(raw.as_deref().unwrap_or("{}")).expect("context json")
}

fn worktree_head(context: &Value) -> String {
    let path = context["worktree_path"].as_str().expect("worktree_path");
    let head = git(Path::new(path), &["rev-parse", "HEAD"]);
    let _ = std::fs::remove_dir_all(path);
    head
}

async fn create_implementation(pool: &PgPool, card_id: &str, context: Value) -> Value {
    let (dispatch_id, _, _) = crate::dispatch::create_dispatch_core_with_options(
        pool,
        card_id,
        AGENT,
        "implementation",
        "Fresh worktree",
        &context,
        crate::dispatch::DispatchCreateOptions {
            skip_outbox: true,
            sidecar_dispatch: false,
        },
    )
    .await
    .expect("create implementation dispatch");
    dispatch_context(pool, &dispatch_id).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_worktree_start_is_the_automatic_baseline_and_explicit_baseline_is_kept_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let fixture = OriginFixture::new("main");
    let latest = fixture.push_new_commit("landed after the desk cloned");
    seed_agent(&pool).await;
    seed_card(&pool, "card-auto-baseline", 761_301, fixture.desk_dir()).await;
    seed_card(&pool, "card-kept-baseline", 761_302, fixture.desk_dir()).await;

    let auto = create_implementation(&pool, "card-auto-baseline", json!({})).await;
    assert_eq!(auto["baseline_commit"], json!(latest));
    assert_eq!(worktree_head(&auto), latest);

    let explicit = json!({ "baseline_commit": fixture.main_tip });
    let kept = create_implementation(&pool, "card-kept-baseline", explicit).await;
    assert_eq!(kept["baseline_commit"], json!(fixture.main_tip));
    assert_eq!(worktree_head(&kept), latest);

    pool.close().await;
    pg_db.drop().await;
}

/// The default 5s policy hook deadline must survive a fetch that only finishes after 6s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_direct_implementation_is_created_and_linked_despite_a_slow_origin_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let fixture = OriginFixture::new("main");
    fixture.push_new_commit("not fetched within the hook");
    fixture.delay_desk_fetches(6);
    let card_id = "card-scope-direct";
    seed_agent(&pool).await;
    seed_card(&pool, card_id, 761_303, fixture.desk_dir()).await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-scope', 'owner/repo', $1, 'active')",
    )
    .bind(AGENT)
    .execute(&pool)
    .await
    .expect("seed run");
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status)
         VALUES ('entry-scope', 'run-scope', $1, $2, 'pending')",
    )
    .bind(card_id)
    .bind(AGENT)
    .execute(&pool)
    .await
    .expect("seed entry");
    sqlx::query(
        "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
         VALUES ('dispatch-scope', $1, $2, 'scope-assessment', 'completed', 'Scope', $3, NOW(), NOW())",
    )
    .bind(card_id)
    .bind(AGENT)
    .bind(json!({ "scope_depth": "direct" }).to_string())
    .execute(&pool)
    .await
    .expect("seed completed scope-assessment");

    let mut config = crate::config::Config::default();
    assert_eq!(config.policies.hook_timeout_ms, 5_000);
    config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new_with_pg(
        &config,
        Some(pool.clone()),
        crate::services::discord::health::slot_row_host_refusal,
    )
    .expect("create policy engine");
    let hook_engine = engine.clone();
    tokio::task::spawn_blocking(move || {
        crate::kanban::fire_event_hooks_with_backends(
            &hook_engine,
            "on_dispatch_completed",
            "OnDispatchCompleted",
            json!({
                "dispatch_id": "dispatch-scope",
                "kanban_card_id": card_id,
                "result": { "scope_depth": "direct" },
            }),
        );
    })
    .await
    .expect("fire scope completion");

    let implementations: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM task_dispatches WHERE kanban_card_id = $1 AND dispatch_type = 'implementation'",
    )
    .bind(card_id)
    .fetch_all(&pool)
    .await
    .expect("implementation dispatches");
    assert_eq!(implementations.len(), 1, "{implementations:?}");
    let (entry_status, entry_dispatch): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-scope'",
    )
    .fetch_one(&pool)
    .await
    .expect("entry");
    assert_eq!(entry_status, "dispatched");
    assert_eq!(entry_dispatch.as_deref(), Some(implementations[0].as_str()));
    let context = dispatch_context(&pool, &implementations[0]).await;
    assert_eq!(worktree_head(&context), fixture.main_tip);

    drop(engine);
    pg_db.drop().await;
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
