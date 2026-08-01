#[cfg(test)]
mod cases_pg_tests {
    use super::super::activate_command::{
        complete_run_if_empty, finalize_activate_run_and_build_response,
    };
    use super::super::order_routes::{OrderBody, submit_order_with_pg};
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::auto_queue::AutoQueueLogContext;
    use axum::http::{HeaderMap, StatusCode};
    use sqlx::PgPool;

    async fn seed_run(pool: &PgPool, run_id: &str, status: &str) {
        sqlx::query("INSERT INTO auto_queue_runs (id, status) VALUES ($1, $2)")
            .bind(run_id)
            .bind(status)
            .execute(pool)
            .await
            .expect("seed entry-point run");
    }

    #[tokio::test]
    async fn activate_empty_drain_and_submit_order_zero_share_readiness_outcome_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_run(&pool, "run-activate-empty", "active").await;
        let empty_log = AutoQueueLogContext::new().run("run-activate-empty");
        let empty_response = complete_run_if_empty(&pool, "run-activate-empty", &empty_log)
            .await
            .expect_err("empty activate must short-circuit");
        assert_eq!(empty_response.0, StatusCode::OK);
        assert_eq!(empty_response.1.0["completion_outcome"], "completed");

        seed_run(&pool, "run-activate-drain", "active").await;
        let drain_log = AutoQueueLogContext::new().run("run-activate-drain");
        let drain_response = finalize_activate_run_and_build_response(
            &pool,
            "run-activate-drain",
            &drain_log,
            Vec::new(),
            Vec::new(),
        )
        .await
        .expect("build drained activate response");
        assert_eq!(drain_response.0, StatusCode::OK);
        assert_eq!(drain_response.1.0["completion_outcome"], "completed");

        seed_run(&pool, "run-submit-zero", "pending").await;
        let (_, order_body) = submit_order_with_pg(
            "run-submit-zero",
            &HeaderMap::new(),
            None,
            &OrderBody {
                order: Vec::new(),
                rationale: Some("no dispatchable cards".to_string()),
                reasoning: None,
            },
            &pool,
        )
        .await
        .expect("submit zero-card order");
        assert_eq!(order_body.0["completion_outcome"], "completed");

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn derived_completion_entry_points_have_one_writer_contract_pg() {
        let activate = include_str!("activate_command.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("activate production source");
        assert_eq!(activate.matches("finalize_run_if_ready_on_pg").count(), 2);
        assert!(!activate.contains("SET status = 'completed'"));
        assert!(!activate.contains("release_run_slots_pg"));

        let order = include_str!("order_routes.rs");
        assert_eq!(order.matches("finalize_run_if_ready_on_pg").count(), 1);
        assert!(!order.contains("SET status = 'completed'"));

        let lifecycle = include_str!("../../../policies/lib/auto-queue-lifecycle.js");
        assert_eq!(lifecycle.matches("finalizeRunIfReady(").count(), 2);
        assert!(!lifecycle.contains(".completeRun("));
        assert!(!lifecycle.contains("releaseSlots"));
        assert!(!lifecycle.contains("runHasUserCancelledEntry"));
        assert!(!lifecycle.contains("runWithinPhaseGateGrace"));

        let ops = include_str!("../../engine/ops/auto_queue_ops.rs");
        assert!(ops.contains("finalizeRunIfReady"));
        assert!(ops.contains("forceCompleteRun"));
        assert!(!ops.contains("release_run_slots_pg"));

        let runs = include_str!("../../db/auto_queue/runs.rs");
        assert_eq!(
            runs.matches("DELETE FROM auto_queue_phase_gates WHERE run_id = $1")
                .count(),
            1,
            "only force completion may delete gates"
        );
    }
}
