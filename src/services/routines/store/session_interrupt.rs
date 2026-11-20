//! Record the interruption while session control holds the run and routine locks.
use super::*;

impl RoutineStore {
    pub(crate) async fn interrupt_controlled_turn(
        tx: &mut Transaction<'_, Postgres>,
        routine_id: &str,
        run_id: &str,
        turn_id: &str,
        error: &str,
        result_json: Option<Value>,
    ) -> Result<Option<String>> {
        let interrupted: Option<String> = sqlx::query_scalar(
            "UPDATE routine_runs rr
             SET status = 'interrupted', result_json = COALESCE($5, rr.result_json),
                 error = $4, finished_at = statement_timestamp(), lease_expires_at = NULL,
                 updated_at = statement_timestamp()
             WHERE rr.id = $1 AND rr.routine_id = $2 AND rr.turn_id = $3 AND rr.status = 'running'
               AND EXISTS (SELECT 1 FROM routines r WHERE r.id = $2 AND r.in_flight_run_id = $1)
             RETURNING rr.id",
        )
        .bind(run_id)
        .bind(routine_id)
        .bind(turn_id)
        .bind(error)
        .bind(result_json)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| anyhow!("interrupt routine run {run_id}: {e}"))?;
        if interrupted.is_none() {
            return Ok(None);
        }

        let updated = sqlx::query(
            "UPDATE routines SET in_flight_run_id = NULL, last_result = $3, updated_at = statement_timestamp()
             WHERE id = $1 AND in_flight_run_id = $2",
        )
        .bind(routine_id)
        .bind(run_id)
        .bind(error)
        .execute(&mut **tx)
        .await
        .map_err(|e| anyhow!("interrupt routine {routine_id}: clear in-flight: {e}"))?;
        if updated.rows_affected() != 1 {
            return Err(anyhow!(
                "interrupt routine run {run_id}: in-flight guard lost row"
            ));
        }
        Ok(interrupted)
    }
}
