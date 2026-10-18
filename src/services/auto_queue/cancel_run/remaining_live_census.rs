use sqlx::Postgres;

/// Live dispatches tied to `run_ids` by an entry of any status, a phase-gate row or context.
/// A post-cancel observation read before commit on the cancel transaction, not the selection.
pub(super) async fn count_live_dispatches_associated_with_runs_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_ids: &[String],
) -> Result<i64, String> {
    if run_ids.is_empty() {
        return Ok(0);
    }

    sqlx::query_scalar(
        "SELECT COUNT(DISTINCT td.id)
         FROM task_dispatches td
         WHERE td.status IN ('pending', 'dispatched')
           AND (
               EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.dispatch_id = td.id
                     AND e.run_id = ANY($1)
               )
               OR EXISTS (
                   SELECT 1
                   FROM auto_queue_phase_gates pg
                   WHERE pg.dispatch_id = td.id
                     AND pg.run_id = ANY($1)
               )
               OR json_extract(td.context, '$.phase_gate.run_id') = ANY($1)
           )",
    )
    .bind(run_ids)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count postgres live dispatches for runs {run_ids:?}: {error}"))
}
