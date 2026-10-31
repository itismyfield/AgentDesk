use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_dispatched_completion_tick_recovers_and_advances_pg() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(16).await;
    let policy_pool = pg_db.connect_and_migrate_with_max_connections(16).await;
    let engine = test_engine_with_pg(policy_pool.clone());
    seed_agent_pg(&pool).await;
    let reviewed_commit =
        crate::services::platform::git_head_commit(env!("CARGO_MANIFEST_DIR")).unwrap();
    for (name, dispatch_status, result, age) in [
        (
            "completed",
            "completed",
            Some(r#"{"work_outcome":"changed"}"#),
            10,
        ),
        ("pr", "completed", None, 10),
        ("review-orphan", "completed", Some("{}"), 10),
        ("disabled", "completed", Some("{}"), 10),
        ("orphan-pr", "completed", None, 10),
        ("empty", "completed", None, 10),
        ("failed", "failed", None, 10),
        ("exhausted", "failed", None, 10),
        ("null-age", "completed", Some("{}"), 10),
        ("cancelled", "cancelled", None, 10),
        ("live", "completed", Some("{}"), 10),
        ("running", "dispatched", Some("{}"), 10),
        ("fresh", "completed", Some("{}"), 0),
        ("fresh-dispatch", "completed", Some("{}"), 10),
        ("successor", "completed", Some("{}"), 10),
    ] {
        let card_id = format!("card-recover-{name}");
        let dispatch_id = format!("dispatch-recover-{name}");
        seed_card_pg(&pool, &card_id, "in_progress").await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status) VALUES ($1, 'agent-1', 'active')",
        )
        .bind(format!("run-recover-{name}"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, result, context, created_at, updated_at)
             VALUES ($1, $2, 'agent-1', 'implementation', $3, $4::jsonb, $5::jsonb,
                     NOW() - ($6 * INTERVAL '1 minute'), NOW() - ($6 * INTERVAL '1 minute'))"
        ).bind(&dispatch_id).bind(&card_id).bind(dispatch_status).bind(result).bind(json!({"reviewed_commit": reviewed_commit, "branch": "test-autoqueue-recovery"})).bind(age as f64)
            .execute(&pool).await.unwrap();
        sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
            .bind(&dispatch_id)
            .bind(&card_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
             VALUES ($1, $2, $3, 'agent-1', 'dispatched', $4, NOW() - ($5 * INTERVAL '1 minute'))"
        ).bind(format!("entry-recover-{name}")).bind(format!("run-recover-{name}"))
            .bind(&card_id).bind(&dispatch_id).bind(age as f64).execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE task_dispatches SET updated_at = NOW() WHERE id = 'dispatch-recover-fresh-dispatch'")
        .execute(&pool).await.unwrap();
    sqlx::query(
        "UPDATE kanban_cards SET status = 'review' WHERE id = 'card-recover-review-orphan'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE auto_queue_runs SET review_mode = 'disabled' WHERE id = 'run-recover-disabled'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE auto_queue_entries SET retry_count = 2 WHERE id = 'entry-recover-exhausted'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE auto_queue_entries SET dispatched_at = NULL, created_at = NOW() - INTERVAL '10 minutes' WHERE id = 'entry-recover-null-age'")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO pr_tracking (card_id, pr_number) VALUES ('card-recover-pr', 7610)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, provider, status, active_dispatch_id)
         VALUES ('recover-live', 'agent-1', 'codex', 'turn_active', 'dispatch-recover-live')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status)
         VALUES ('recover-successor', 'card-recover-successor', 'agent-1', 'rework', 'pending')",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO pr_tracking (card_id, pr_number) VALUES ('card-recover-orphan-pr', 7611)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE pr_tracking SET head_sha = $1, branch = 'test-autoqueue-recovery'")
        .bind(&reviewed_commit)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM task_dispatches WHERE id = 'dispatch-recover-orphan-pr'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE auto_queue_runs SET created_at = NOW() + INTERVAL '1 minute' WHERE id = 'run-recover-completed'")
        .execute(&pool).await.unwrap();
    let service = crate::services::auto_queue::AutoQueueService::new(engine.clone());
    let before = service
        .status_with_pg(&pool, Default::default())
        .await
        .unwrap();
    assert_eq!(
        before.diagnostics.entry_dispatch_delivery_mismatches.len(),
        1
    );
    for pass in 0..2 {
        eprintln!("recovery pass {pass}: tick");
        let tick_engine = engine.clone();
        tokio::task::spawn_blocking(move || {
            tick_engine.try_fire_hook_by_name("OnTick1min", json!({}))
        })
        .await
        .unwrap()
        .unwrap();
        eprintln!("recovery pass {pass}: tick returned");
        crate::kanban::drain_hook_side_effects_with_backends(&engine);
        eprintln!(
            "recovery pass {pass}: pool size={}, idle={}, closed={}",
            pool.size(),
            pool.num_idle(),
            pool.is_closed()
        );
        crate::server::fire_five_min_policy_hook_for_test(&pool, &engine)
            .await
            .unwrap();
        for name in ["completed", "pr", "orphan-pr", "null-age", "review-orphan"] {
            let card_id = format!("card-recover-{name}");
            let (status, review_count): (String, i64) = sqlx::query_as(
                "SELECT status, (SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = c.id AND dispatch_type = 'review')
                 FROM kanban_cards c WHERE id = $1"
            ).bind(&card_id).fetch_one(&pool).await.unwrap();
            assert_eq!(status, "review", "{name}, pass {pass}");
            assert_eq!(review_count, 1, "{name}, pass {pass}: one review dispatch");
            let entry = service
                .entry_json_with_pg(&pool, &format!("entry-recover-{name}"), None)
                .await
                .unwrap();
            assert_eq!(
                entry["status"], "dispatched",
                "entry remains bound through review"
            );
            sqlx::query(
                "UPDATE auto_queue_runs SET created_at = NOW() + INTERVAL '1 minute' WHERE id = $1",
            )
            .bind(format!("run-recover-{name}"))
            .execute(&pool)
            .await
            .unwrap();
            let after = service
                .status_with_pg(&pool, Default::default())
                .await
                .unwrap();
            assert!(
                after
                    .diagnostics
                    .entry_dispatch_delivery_mismatches
                    .is_empty(),
                "{name}: recovered review has delivery backing"
            );
        }
        let after = service
            .status_with_pg(&pool, Default::default())
            .await
            .unwrap();
        assert!(
            after
                .diagnostics
                .entry_dispatch_delivery_mismatches
                .is_empty(),
            "recovered review has delivery backing"
        );
        let exhausted: String = sqlx::query_scalar(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-recover-exhausted'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(exhausted, "failed", "existing retry limit is preserved");
        let disabled: (String, i64) = sqlx::query_as(
            "SELECT status, (SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = e.kanban_card_id AND dispatch_type = 'review')
             FROM auto_queue_entries e WHERE id = 'entry-recover-disabled'"
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(
            disabled,
            ("done".to_string(), 0),
            "disabled reviews retain normal completion policy"
        );
        if pass == 0 {
            for name in ["empty", "failed", "cancelled"] {
                let status: String =
                    sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = $1")
                        .bind(format!("entry-recover-{name}"))
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                assert_eq!(
                    status, "pending",
                    "{name}: release for existing retry policy"
                );
            }
        }
        for name in ["live", "running", "fresh", "fresh-dispatch", "successor"] {
            let (status, dispatch, card_status, reviews): (String, String, String, i64) = sqlx::query_as(
                "SELECT e.status, COALESCE(e.dispatch_id, ''), c.status,
                        (SELECT COUNT(*) FROM task_dispatches d WHERE d.kanban_card_id = c.id AND d.dispatch_type = 'review')
                 FROM auto_queue_entries e JOIN kanban_cards c ON c.id = e.kanban_card_id WHERE e.id = $1"
            ).bind(format!("entry-recover-{name}")).fetch_one(&pool).await.unwrap();
            assert_eq!(status, "dispatched", "{name} must remain untouched");
            assert_eq!(dispatch, format!("dispatch-recover-{name}"));
            assert_eq!(card_status, "in_progress", "{name}: no review transition");
            assert_eq!(reviews, 0, "{name}: no review dispatch");
        }
    }
    let live_session: (String, String) = sqlx::query_as(
        "SELECT status, active_dispatch_id FROM sessions WHERE session_key = 'recover-live'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        live_session,
        (
            "turn_active".to_string(),
            "dispatch-recover-live".to_string()
        )
    );
    eprintln!("recovery: repeated ticks verified; checking restart");
    crate::reconcile::reconcile_boot_db_pg(&pool, "recovery-restart", None)
        .await
        .unwrap();
    let retained: String = sqlx::query_scalar(
        "SELECT status FROM auto_queue_entries WHERE id = 'entry-recover-completed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        retained, "dispatched",
        "restart must retain work awaiting its review"
    );
    sqlx::raw_sql(
        "UPDATE auto_queue_entries SET status = 'skipped', dispatch_id = NULL, slot_index = NULL
         WHERE run_id != 'run-recover-completed';
         UPDATE task_dispatches SET status = 'cancelled',
             created_at = NOW() - INTERVAL '11 minutes', updated_at = NOW() - INTERVAL '10 minutes',
             completed_at = NOW() - INTERVAL '10 minutes'
         WHERE kanban_card_id != 'card-recover-completed' AND status IN ('pending', 'dispatched');
         UPDATE kanban_cards SET status = 'done' WHERE id != 'card-recover-completed';
         UPDATE sessions SET status = 'disconnected', active_dispatch_id = NULL;
         UPDATE auto_queue_runs SET status = 'completed', completed_at = NOW()
         WHERE id != 'run-recover-completed';",
    )
    .execute(&pool)
    .await
    .unwrap();
    seed_card_pg(&pool, "card-recover-next", "requested").await;
    // Plan creation exercises normal activation without fresh implementation worktree infrastructure.
    sqlx::query("UPDATE kanban_cards SET metadata = '{\"scope_assessment_status\":\"completed\",\"scope_depth\":\"full\"}'::jsonb WHERE id = 'card-recover-next'")
        .execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
         VALUES ('entry-recover-next', 'run-recover-completed', 'card-recover-next', 'agent-1', 'pending', 2)"
    ).execute(&pool).await.unwrap();
    sqlx::query(
        "UPDATE task_dispatches SET status = 'completed', result = '{\"verdict\":\"pass\"}', completed_at = NOW(), updated_at = NOW()
         WHERE kanban_card_id = 'card-recover-completed' AND dispatch_type = 'review'"
    ).execute(&pool).await.unwrap();
    drop(service);
    drop(engine);
    policy_pool.close().await;
    let resumed_policy_pool = pg_db.connect_and_migrate_with_max_connections(16).await;
    let resumed_engine = test_engine_with_pg(resumed_policy_pool.clone());
    eprintln!("recovery: completing review and advancing the run");
    crate::kanban::transition_status_with_opts_pg(
        &pool,
        &resumed_engine,
        "card-recover-completed",
        "done",
        "test_review_completed",
        crate::engine::transition::ForceIntent::None,
    )
    .await
    .unwrap();
    crate::kanban::drain_hook_side_effects_with_backends(&resumed_engine);
    let next_tick_engine = resumed_engine.clone();
    tokio::task::spawn_blocking(move || {
        next_tick_engine.try_fire_hook_by_name("OnTick1min", json!({}))
    })
    .await
    .unwrap()
    .unwrap();
    crate::kanban::drain_hook_side_effects_with_backends(&resumed_engine);
    let next: (String, bool) = sqlx::query_as(
        "SELECT status, dispatch_id IS NOT NULL FROM auto_queue_entries WHERE id = 'entry-recover-next'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(
        next,
        ("dispatched".to_string(), true),
        "same run advances after recovered review"
    );
    eprintln!("recovery: next entry dispatched");
    drop(resumed_engine);
    resumed_policy_pool.close().await;
    pool.close().await;
    pg_db.drop().await;
}

async fn seed_completed_queue_work(pool: &sqlx::PgPool, name: &str, kind: &str, status: &str) {
    seed_card_pg(pool, name, status).await;
    let commit = crate::services::platform::git_head_commit(env!("CARGO_MANIFEST_DIR")).unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, agent_id, status) VALUES ($1, 'agent-1', 'active')",
    )
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, result, context, created_at, updated_at, completed_at)
        VALUES ($1, $1, 'agent-1', $2, 'completed', '{}', $3::jsonb, NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes')")
        .bind(name).bind(kind).bind(json!({"reviewed_commit": commit, "branch": "test-autoqueue-recovery"}))
        .execute(pool).await.unwrap();
    sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1, review_status = 'reviewing', review_entered_at = NOW() - INTERVAL '10 minutes' WHERE id = $1")
        .bind(name).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
        VALUES ($1, $1, $1, 'agent-1', 'dispatched', $1, NOW() - INTERVAL '10 minutes')")
        .bind(name).execute(pool).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn periodic_queue_review_recovery_preserves_operator_wait_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(16).await;
    let policy_pool = db.connect_and_migrate_with_max_connections(16).await;
    let engine = test_engine_with_pg(policy_pool.clone());
    seed_agent_pg(&pool).await;
    seed_completed_queue_work(&pool, "manual-review", "implementation", "review").await;
    sqlx::query("UPDATE kanban_cards SET review_round = 2 WHERE id = 'manual-review'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, result, context, completed_at)
        VALUES ('manual-r1', 'manual-review', 'agent-1', 'review', 'completed', '{\"verdict\":\"improve\"}', '{\"review_round\":1}', NOW() - INTERVAL '5 minutes'),
               ('manual-r2', 'manual-review', 'agent-1', 'review', 'completed', '{\"verdict\":\"reject\"}', '{\"review_round\":2}', NOW())")
        .execute(&pool).await.unwrap();
    let verdict_engine = engine.clone();
    tokio::task::spawn_blocking(move || {
        verdict_engine.try_fire_hook_by_name(
            "OnReviewVerdict",
            json!({
                "card_id":"manual-review", "dispatch_id":"manual-r2", "verdict":"reject"
            }),
        )
    })
    .await
    .unwrap()
    .unwrap();
    crate::kanban::drain_hook_side_effects_with_backends(&engine);
    let before: (String, String, i64, i64) = sqlx::query_as(
        "SELECT review_status, blocked_reason, review_round::BIGINT,
            (SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = c.id AND dispatch_type = 'review')
         FROM kanban_cards c WHERE id = 'manual-review'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        before.0, "dilemma_pending",
        "actual R2 verdict must request an operator decision"
    );
    assert!(!before.1.is_empty());
    assert_eq!((before.2, before.3), (2, 2));
    for status in ["suggestion_pending", "rework_pending"] {
        seed_completed_queue_work(&pool, status, "implementation", "review").await;
        sqlx::query("UPDATE kanban_cards SET review_status = $1, blocked_reason = 'keep operator decision', review_round = 2 WHERE id = $1")
            .bind(status).execute(&pool).await.unwrap();
    }
    for _ in 0..2 {
        crate::server::fire_five_min_policy_hook_for_test(&pool, &engine)
            .await
            .unwrap();
        let after: (String, String, i64, i64) = sqlx::query_as(
            "SELECT review_status, COALESCE(blocked_reason, ''), review_round::BIGINT,
                (SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = c.id AND dispatch_type = 'review')
             FROM kanban_cards c WHERE id = 'manual-review'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(
            after, before,
            "periodic recovery must retain the R2 operator decision"
        );
        for status in ["suggestion_pending", "rework_pending"] {
            let state: (String, String, i64) = sqlx::query_as(
                "SELECT review_status, COALESCE(blocked_reason, ''),
                    (SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = c.id AND dispatch_type = 'review')
                 FROM kanban_cards c WHERE id = $1")
                .bind(status).fetch_one(&pool).await.unwrap();
            assert_eq!(
                state,
                (status.to_string(), "keep operator decision".to_string(), 0)
            );
        }
    }
    drop(engine);
    policy_pool.close().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn periodic_queue_review_recovery_precedes_orphan_timeout_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(16).await;
    let policy_pool = db.connect_and_migrate_with_max_connections(16).await;
    let engine = test_engine_with_pg(policy_pool.clone());
    seed_agent_pg(&pool).await;
    seed_completed_queue_work(&pool, "old-orphan", "implementation", "review").await;
    for _ in 0..2 {
        crate::server::fire_five_min_policy_hook_for_test(&pool, &engine)
            .await
            .unwrap();
        let state: (String, Option<String>, i64) = sqlx::query_as(
            "SELECT review_status, blocked_reason,
                (SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = c.id AND dispatch_type = 'review')
             FROM kanban_cards c WHERE id = 'old-orphan'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(
            state,
            ("reviewing".to_string(), None, 1),
            "repair a real orphan before the timeout escalates it"
        );
    }
    drop(engine);
    policy_pool.close().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_queue_recovery_preserves_impl_but_resumes_completed_plan_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(16).await;
    let policy_pool = db.connect_and_migrate_with_max_connections(16).await;
    let engine = test_engine_with_pg(policy_pool.clone());
    seed_agent_pg(&pool).await;
    seed_completed_queue_work(&pool, "boot-impl", "implementation", "in_progress").await;
    seed_completed_queue_work(&pool, "boot-plan", "plan", "in_progress").await;
    sqlx::query("UPDATE kanban_cards SET metadata = '{\"scope_depth\":\"plan_only\",\"scope_assessment_status\":\"completed\",\"sandbox_preflight\":true,\"production_mutation_allowed\":false}'::jsonb WHERE id = 'boot-plan'")
        .execute(&pool).await.unwrap();
    let backing: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM pr_tracking),
                (SELECT COUNT(*) FROM task_dispatches WHERE status IN ('pending', 'dispatched')),
                (SELECT COUNT(*) FROM sessions)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        backing,
        (0, 0, 0),
        "boot evidence must not be masked by PR, successor or session guards"
    );
    crate::reconcile::reconcile_boot_db_pg(&pool, "recovery-before-first-tick", None)
        .await
        .unwrap();
    let implementation: (String, Option<String>) =
        sqlx::query_as("SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'boot-impl'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        implementation,
        ("dispatched".to_string(), Some("boot-impl".to_string())),
        "completed implementation must retain its original dispatch before any review exists"
    );
    let plan: (String, Option<String>) =
        sqlx::query_as("SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'boot-plan'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        plan,
        ("pending".to_string(), None),
        "completed plan must retain its pre-existing reset path"
    );
    let tick_engine = engine.clone();
    tokio::task::spawn_blocking(move || tick_engine.try_fire_hook_by_name("OnTick1min", json!({})))
        .await
        .unwrap()
        .unwrap();
    crate::kanban::drain_hook_side_effects_with_backends(&engine);
    let resumed: (String, String, i64) = sqlx::query_as(
        "SELECT e.status, d.dispatch_type,
            (SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = e.kanban_card_id AND dispatch_type = 'review')
         FROM auto_queue_entries e JOIN task_dispatches d ON d.id = e.dispatch_id WHERE e.id = 'boot-plan'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        resumed,
        ("dispatched".to_string(), "implementation".to_string(), 0),
        "plan_only resumes implementation without review"
    );
    let reviews: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'boot-impl' AND dispatch_type = 'review'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(reviews, 1);
    drop(engine);
    policy_pool.close().await;
    pool.close().await;
    db.drop().await;
}
