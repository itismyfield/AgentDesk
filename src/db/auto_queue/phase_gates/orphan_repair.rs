//! Narrow non-force cleanup for phase gates that cannot reconcile.

use super::PhaseGateRepairError;

/// Delete only blocking gate rows whose dispatch is absent. Valid gates remain
/// owned by normal dispatch reconciliation (or explicit force completion).
/// The caller has already acquired the candidate phase advisory locks, so two
/// repairs converge on the same deletion without widening force authority.
pub(super) async fn clear_orphan_gates_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase_filter: Option<i64>,
    dispatch_id_filter: Option<&str>,
) -> Result<usize, PhaseGateRepairError> {
    let cleared = sqlx::query(
        "DELETE FROM auto_queue_phase_gates pg
         WHERE pg.run_id = $1
           AND pg.status IN ('pending', 'failed')
           AND ($2::BIGINT IS NULL OR pg.phase = $2)
           AND ($3::TEXT IS NULL OR pg.dispatch_id = $3)
           AND NOT EXISTS (
               SELECT 1 FROM task_dispatches td WHERE td.id = pg.dispatch_id
           )",
    )
    .bind(run_id)
    .bind(phase_filter)
    .bind(dispatch_id_filter)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        PhaseGateRepairError::database(format!(
            "clear orphan phase gates for run {run_id}: {error}"
        ))
    })?
    .rows_affected();
    Ok(cleared as usize)
}

#[cfg(test)]
pub(super) async fn assert_filtered_then_unfiltered_repair_contract(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-pg-test', 'Agent', 'claude', '999')",
    )
    .execute(pool)
    .await
    .expect("seed agent"); // agentdesk-audit: allow-unwrap — test-only PG assertion helper
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-pg-test', 'repo', 'agent-pg-test', 'paused')",
    )
    .execute(pool)
    .await
    .expect("seed paused run"); // agentdesk-audit: allow-unwrap — test-only PG assertion helper
    super::save_phase_gate_state_on_pg(
        pool,
        "run-pg-test",
        0,
        &super::PhaseGateStateWrite {
            status: "failed".into(),
            pass_verdict: "phase_gate_passed".into(),
            dispatch_ids: vec![],
            next_phase: Some(1),
            failure_reason: Some("orphaned phase gate".into()),
            ..Default::default()
        },
    )
    .await
    .expect("seed orphan gate state"); // agentdesk-audit: allow-unwrap — test-only PG assertion helper

    // A concrete dispatch filter cannot match this NULL orphan.
    let filtered = super::repair_phase_gates_for_run_on_pg(
        pool,
        "run-pg-test",
        super::PhaseGateRepairOptions {
            dispatch_id: Some("dispatch-that-cannot-match-null".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("filtered repair leaves NULL orphan untouched"); // agentdesk-audit: allow-unwrap — test-only PG assertion helper
    assert_eq!(filtered.cleared_gates, 0);
    assert_eq!(filtered.blocking_gates_remaining, 0);
    assert_eq!(gate_count(pool).await, 1);
    assert_eq!(run_status(pool).await, "paused");

    let summary = super::repair_phase_gates_for_run_on_pg(
        pool,
        "run-pg-test",
        super::PhaseGateRepairOptions::default(),
    )
    .await
    .expect("unfiltered repair clears orphan gate"); // agentdesk-audit: allow-unwrap — test-only PG assertion helper
    assert_eq!(summary.candidate_dispatches, 0);
    assert_eq!(summary.cleared_gates, 1);
    assert_eq!(summary.orphan_gates_skipped, 0);
    assert_eq!(summary.blocking_gates_remaining, 0);
    assert_eq!(summary.run_status.as_deref(), Some("completed"));
    assert_eq!(gate_count(pool).await, 0);

    let repeated = super::repair_phase_gates_for_run_on_pg(
        pool,
        "run-pg-test",
        super::PhaseGateRepairOptions::default(),
    )
    .await
    .expect("repeat repair is idempotent"); // agentdesk-audit: allow-unwrap — test-only PG assertion helper
    assert_eq!(repeated.cleared_gates, 0);
    assert_eq!(repeated.blocking_gates_remaining, 0);
}

#[cfg(test)]
async fn gate_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_phase_gates
         WHERE run_id = 'run-pg-test' AND phase = 0",
    )
    .fetch_one(pool)
    .await
    .expect("count orphan gates") // agentdesk-audit: allow-unwrap — test-only PG assertion helper
}

#[cfg(test)]
async fn run_status(pool: &sqlx::PgPool) -> String {
    sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-pg-test'")
        .fetch_one(pool)
        .await
        .expect("load repaired run status") // agentdesk-audit: allow-unwrap — test-only PG assertion helper
}
