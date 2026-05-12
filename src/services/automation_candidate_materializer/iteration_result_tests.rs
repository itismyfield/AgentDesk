use super::*;

#[tokio::test]
async fn submit_iteration_result_rejects_cards_outside_active_loop_state() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let card_id = "review-candidate-card";
    let metadata = serde_json::json!({
        "automation_candidate": {"source": "test"},
        "program": {
            "repo_dir": "/repo",
            "allowed_write_paths": ["src"],
            "metric_name": "failure_count",
            "metric_target": 0.0,
            "metric_direction": "lower_is_better",
            "current_iteration": 0,
            "iteration_budget": 3,
            "final_gate": "manual_review"
        }
    });

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, pipeline_stage_id, metadata, created_at, updated_at
         ) VALUES ($1, 'review candidate', 'review', $2, $3::jsonb, NOW(), NOW())",
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .bind(metadata.to_string())
    .execute(&pool)
    .await
    .expect("seed review candidate");

    let error = AutomationCandidateMaterializer::new(pool.clone())
        .submit_iteration_result(
            card_id,
            IterationResultInput {
                iteration: 1,
                branch: "automation-candidate/review-candidate-card/1".to_string(),
                commit_hash: None,
                metric_before: Some(10.0),
                metric_after: Some(9.0),
                is_simplification: Some(false),
                status: "ok".to_string(),
                description: Some("must be rejected".to_string()),
                allowed_write_paths_used: Some(vec!["src/example.rs".to_string()]),
                run_seconds: Some(1),
                crash_trace: None,
            },
        )
        .await
        .expect_err("review candidate must reject stale iteration results");

    assert!(matches!(
        error,
        MaterializerError::InactiveLoopState { status } if status == "review"
    ));

    let persisted_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM automation_candidate_iterations WHERE card_id = $1",
    )
    .bind(card_id)
    .fetch_one(&pool)
    .await
    .expect("count candidate iterations");
    assert_eq!(
        persisted_count, 0,
        "inactive candidates must not persist outcomes"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn prepare_worktree_rejects_cards_outside_active_loop_state() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let card_id = "review-worktree-card";
    let metadata = serde_json::json!({
        "automation_candidate": {"source": "test"},
        "program": {
            "repo_dir": "/repo-that-should-not-be-touched",
            "allowed_write_paths": ["src"],
            "metric_name": "failure_count",
            "metric_target": 0.0,
            "metric_direction": "lower_is_better",
            "current_iteration": 0,
            "iteration_budget": 3,
            "final_gate": "manual_review"
        }
    });

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, pipeline_stage_id, metadata, created_at, updated_at
         ) VALUES ($1, 'review candidate', 'review', $2, $3::jsonb, NOW(), NOW())",
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .bind(metadata.to_string())
    .execute(&pool)
    .await
    .expect("seed review candidate");

    let error = AutomationCandidateMaterializer::new(pool.clone())
        .prepare_worktree(card_id, 1)
        .await
        .expect_err("inactive card must fail before worktree creation");

    assert!(matches!(
        error,
        MaterializerError::InactiveLoopState { status } if status == "review"
    ));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn prepare_worktree_rejects_future_iterations_before_touching_git() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let card_id = "future-worktree-card";
    let metadata = serde_json::json!({
        "automation_candidate": {"source": "test"},
        "program": {
            "repo_dir": "/repo-that-should-not-be-touched",
            "allowed_write_paths": ["src"],
            "metric_name": "failure_count",
            "metric_target": 0.0,
            "metric_direction": "lower_is_better",
            "current_iteration": 0,
            "iteration_budget": 3,
            "final_gate": "manual_review"
        }
    });

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, pipeline_stage_id, metadata, created_at, updated_at
         ) VALUES ($1, 'future candidate', 'ready', $2, $3::jsonb, NOW(), NOW())",
    )
    .bind(card_id)
    .bind(PIPELINE_STAGE_ID)
    .bind(metadata.to_string())
    .execute(&pool)
    .await
    .expect("seed ready candidate");

    let error = AutomationCandidateMaterializer::new(pool.clone())
        .prepare_worktree(card_id, 999)
        .await
        .expect_err("future iteration must fail before worktree creation");

    assert!(matches!(
        error,
        MaterializerError::IterationOutOfSequence {
            expected: 1,
            actual: 999
        }
    ));

    pool.close().await;
    pg_db.drop().await;
}
