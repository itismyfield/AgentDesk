use super::*;

#[tokio::test]
async fn auto_queue_dispatched_completion_tick_recovers_and_advances_pg() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(pool.clone());
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
        engine
            .fire_hook_by_name_blocking("OnTick1min", json!({}))
            .unwrap();
        crate::kanban::drain_hook_side_effects_with_backends(&engine);
        crate::reconcile::reconcile_completed_queue_review_drift_pg(&pool, &engine)
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
            let (status, dispatch): (String, String) =
                sqlx::query_as("SELECT status, dispatch_id FROM auto_queue_entries WHERE id = $1")
                    .bind(format!("entry-recover-{name}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(status, "dispatched", "{name} must remain untouched");
            assert_eq!(dispatch, format!("dispatch-recover-{name}"));
        }
    }
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
         UPDATE task_dispatches SET status = 'cancelled'
         WHERE kanban_card_id != 'card-recover-completed' AND status IN ('pending', 'dispatched');
         UPDATE sessions SET status = 'disconnected', active_dispatch_id = NULL;",
    )
    .execute(&pool)
    .await
    .unwrap();
    seed_card_pg(&pool, "card-recover-next", "requested").await;
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
         VALUES ('entry-recover-next', 'run-recover-completed', 'card-recover-next', 'agent-1', 'pending', 2)"
    ).execute(&pool).await.unwrap();
    sqlx::query(
        "UPDATE task_dispatches SET status = 'completed', result = '{\"verdict\":\"pass\"}', completed_at = NOW(), updated_at = NOW()
         WHERE kanban_card_id = 'card-recover-completed' AND dispatch_type = 'review'"
    ).execute(&pool).await.unwrap();
    crate::kanban::transition_status_with_opts_pg(
        &pool,
        &engine,
        "card-recover-completed",
        "done",
        "test_review_completed",
        crate::engine::transition::ForceIntent::None,
    )
    .await
    .unwrap();
    crate::kanban::drain_hook_side_effects_with_backends(&engine);
    let next: (String, bool) = sqlx::query_as(
        "SELECT status, dispatch_id IS NOT NULL FROM auto_queue_entries WHERE id = 'entry-recover-next'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(
        next,
        ("dispatched".to_string(), true),
        "same run advances after recovered review"
    );
    pool.close().await;
    pg_db.drop().await;
}
