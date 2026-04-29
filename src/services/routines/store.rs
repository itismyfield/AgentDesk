use anyhow::{Result, anyhow};
use sqlx::PgPool;
use std::sync::Arc;

/// Durable PG-backed store for routines and routine_runs.
///
/// All mutating operations are transaction-scoped. Callers never hold a
/// connection across JS execution — claim and finish are always separate
/// transactions (see M-1 in PRD review notes).
#[derive(Clone)]
pub struct RoutineStore {
    pool: Arc<PgPool>,
}

impl RoutineStore {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Boot recovery: mark all `running` runs as `interrupted`, clear
    /// `in_flight_run_id` on their parent routines. Called once at worker
    /// startup before the tick loop begins.
    ///
    /// Returns the number of stale runs that were recovered.
    pub async fn recover_stale_running_runs(&self) -> Result<u64> {
        let mut tx = self.pool.begin().await?;

        // Collect stale running run IDs and their routine IDs.
        let stale: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT id, routine_id
            FROM routine_runs
            WHERE status = 'running'
            "#,
        )
        .fetch_all(&mut *tx)
        .await?;

        if stale.is_empty() {
            tx.commit().await?;
            return Ok(0);
        }

        let count = stale.len() as u64;
        let stale_run_ids: Vec<&str> = stale.iter().map(|(id, _)| id.as_str()).collect();
        let stale_routine_ids: Vec<&str> = stale.iter().map(|(_, rid)| rid.as_str()).collect();

        // Close stale runs.
        sqlx::query(
            r#"
            UPDATE routine_runs
            SET status = 'interrupted',
                finished_at = NOW(),
                updated_at = NOW(),
                error = 'interrupted by server restart'
            WHERE id = ANY($1)
            "#,
        )
        .bind(&stale_run_ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("recover: close stale runs: {e}"))?;

        // Release in_flight_run_id locks on affected routines.
        sqlx::query(
            r#"
            UPDATE routines
            SET in_flight_run_id = NULL,
                updated_at = NOW()
            WHERE id = ANY($1)
            "#,
        )
        .bind(&stale_routine_ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("recover: clear in_flight_run_id: {e}"))?;

        tx.commit().await?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    // Integration tests that require a live PG connection live in
    // src/integration_tests.rs and are gated on the `integration` feature.
    // Unit-level store tests will be added in ORDER-P0-002 alongside the
    // due-claim and finish/fail/pause/resume transaction methods.
}
