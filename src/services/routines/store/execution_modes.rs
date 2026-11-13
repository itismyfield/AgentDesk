//! Execution definition reads and the claim-time `run_spec` pin.
use std::collections::HashMap;

use super::*;
use crate::services::routines::execution_mode::{ClaimDefinitionRow, StoredExecutionDefinition};

impl RoutineStore {
    /// Pins the definition the claiming transaction already locked onto the new run.
    pub(super) async fn pin_run_spec(
        tx: &mut Transaction<'_, Postgres>,
        run_id: &str,
        routine_id: &str,
    ) -> Result<()> {
        let row: ClaimDefinitionRow = sqlx::query_as(
            "SELECT id, execution_mode, runner, execution_strategy, script_ref,
                    headless_failure_notice_limit, agent_id, fallback_agent_id, max_retries,
                    discord_thread_id, timeout_secs
             FROM routines WHERE id = $1",
        )
        .bind(routine_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| anyhow!("claim routine {routine_id}: read definition: {e}"))?;
        sqlx::query("UPDATE routine_runs SET run_spec = $2 WHERE id = $1")
            .bind(run_id)
            .bind(row.run_spec())
            .execute(&mut **tx)
            .await
            .map_err(|e| anyhow!("claim routine {routine_id}: pin run_spec: {e}"))?;
        Ok(())
    }

    pub async fn load_execution_definitions(
        &self,
        routine_ids: &[String],
    ) -> Result<HashMap<String, StoredExecutionDefinition>> {
        let rows: Vec<StoredExecutionDefinition> = sqlx::query_as(
            "SELECT id, execution_mode, runner, execution_strategy, script_ref,
                    headless_failure_notice_limit
             FROM routines WHERE id = ANY($1)",
        )
        .bind(routine_ids)
        .fetch_all(&*self.pool)
        .await
        .map_err(|e| anyhow!("load routine execution definitions: {e}"))?;
        Ok(rows.into_iter().map(|row| (row.id.clone(), row)).collect())
    }

    /// Explicit-mode runs refuse the legacy dynamic DM override; null-mode runs keep it.
    pub async fn run_forbids_dm_override(&self, run_id: &str) -> Result<bool> {
        let explicit: Option<bool> = sqlx::query_scalar(
            "SELECT run_spec ->> 'execution_mode' IS NOT NULL FROM routine_runs WHERE id = $1",
        )
        .bind(run_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| anyhow!("read routine run {run_id} execution mode: {e}"))?;
        Ok(explicit.unwrap_or(false))
    }
}
