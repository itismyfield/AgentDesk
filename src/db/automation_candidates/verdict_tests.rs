use super::*;

#[test]
fn crashed_always_discards() {
    assert_eq!(
        compute_verdict(
            Some(0.9),
            Some(0.95),
            false,
            "crashed",
            MetricDirection::LowerIsBetter
        ),
        "discard"
    );
    assert_eq!(
        compute_verdict(None, None, true, "crashed", MetricDirection::LowerIsBetter),
        "discard"
    );
}

#[test]
fn timeout_always_discards() {
    assert_eq!(
        compute_verdict(
            Some(0.8),
            Some(0.9),
            false,
            "timeout",
            MetricDirection::LowerIsBetter
        ),
        "discard"
    );
}

#[test]
fn simplification_always_keeps() {
    assert_eq!(
        compute_verdict(
            Some(0.9),
            Some(0.5),
            true,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "keep"
    );
    assert_eq!(
        compute_verdict(None, None, true, "ok", MetricDirection::HigherIsBetter),
        "keep"
    );
}

#[test]
fn lower_metric_improvement_keeps() {
    assert_eq!(
        compute_verdict(
            Some(0.9),
            Some(0.8),
            false,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "keep"
    );
    assert_eq!(
        compute_verdict(
            Some(1.0),
            Some(0.0),
            false,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "keep"
    );
}

#[test]
fn higher_metric_improvement_keeps() {
    assert_eq!(
        compute_verdict(
            Some(0.8),
            Some(0.9),
            false,
            "ok",
            MetricDirection::HigherIsBetter
        ),
        "keep"
    );
}

#[test]
fn metric_regression_or_equal_discards() {
    assert_eq!(
        compute_verdict(
            Some(0.8),
            Some(0.9),
            false,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "discard"
    );
    assert_eq!(
        compute_verdict(
            Some(0.9),
            Some(0.8),
            false,
            "ok",
            MetricDirection::HigherIsBetter
        ),
        "discard"
    );
    assert_eq!(
        compute_verdict(
            Some(0.5),
            Some(0.5),
            false,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "discard"
    );
}

#[test]
fn no_metrics_discards() {
    assert_eq!(
        compute_verdict(None, None, false, "ok", MetricDirection::LowerIsBetter),
        "discard"
    );
    assert_eq!(
        compute_verdict(Some(0.8), None, false, "ok", MetricDirection::LowerIsBetter),
        "discard"
    );
    assert_eq!(
        compute_verdict(None, Some(0.8), false, "ok", MetricDirection::LowerIsBetter),
        "discard"
    );
}

#[test]
fn parses_metric_direction_aliases() {
    assert_eq!(
        MetricDirection::parse(Some("higher")),
        MetricDirection::HigherIsBetter
    );
    assert_eq!(
        MetricDirection::parse(Some("higher_is_better")),
        MetricDirection::HigherIsBetter
    );
    assert_eq!(
        MetricDirection::parse(Some("lower")),
        MetricDirection::LowerIsBetter
    );
    assert_eq!(MetricDirection::parse(None), MetricDirection::LowerIsBetter);
}

#[test]
fn final_iteration_boundary() {
    assert!(!is_final_iteration(9));
    assert!(is_final_iteration(10));
    assert!(is_final_iteration(11));
}

#[test]
fn card_update_guards_require_exactly_one_row() {
    assert!(ensure_one_card_row_affected(1, "update card", "card-1").is_ok());

    let missing = ensure_one_card_row_affected(0, "update card", "card-1")
        .expect_err("zero-row updates must fail");
    assert!(missing.contains("affected 0 rows"));

    let duplicated = ensure_one_card_row_affected(2, "update card", "card-1")
        .expect_err("multi-row updates must fail");
    assert!(duplicated.contains("affected 2 rows"));
}

#[tokio::test]
async fn persist_iteration_outcome_pg_rolls_back_when_card_update_matches_zero_rows() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let card_id = "non-candidate-card";

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, metadata, created_at, updated_at)
         VALUES ($1, 'ordinary card', 'ready', $2::jsonb, NOW(), NOW())",
    )
    .bind(card_id)
    .bind(serde_json::json!({"program": {"current_iteration": 0}}).to_string())
    .execute(&pool)
    .await
    .expect("seed ordinary card");

    let err = persist_iteration_outcome_pg(
        &pool,
        InsertIterationParams {
            card_id: card_id.to_string(),
            iteration: 1,
            branch: "automation-candidate/non-candidate/1".to_string(),
            commit_hash: None,
            metric_before: Some(10.0),
            metric_after: Some(9.0),
            is_simplification: false,
            status: "keep".to_string(),
            description: Some("should rollback".to_string()),
            allowed_write_paths_used: vec!["src/example.rs".to_string()],
            run_seconds: Some(1),
            crash_trace: None,
        },
        IterationOutcomeAction::KeepContinue,
    )
    .await
    .expect_err("non automation-candidate card update must fail");

    assert!(err.contains("affected 0 rows"), "unexpected error: {err}");

    let persisted_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM automation_candidate_iterations WHERE card_id = $1",
    )
    .bind(card_id)
    .fetch_one(&pool)
    .await
    .expect("count candidate iterations");
    assert_eq!(persisted_count, 0, "iteration insert must roll back");

    pool.close().await;
    pg_db.drop().await;
}
