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

    /// The routine as `run_id` was claimed (pinned strategy, thread, agents) so a mid-run PATCH
    /// cannot retarget its session cleanup; runs with a NULL `run_spec` keep the live row.
    pub async fn routine_as_claimed(
        &self,
        routine_id: &str,
        run_id: &str,
    ) -> Result<Option<RoutineRecord>> {
        let Some(mut routine) = self.get_routine(routine_id).await? else {
            return Ok(None);
        };
        let spec: Option<Option<Value>> = sqlx::query_scalar(
            "SELECT run_spec FROM routine_runs WHERE id = $1 AND routine_id = $2",
        )
        .bind(run_id)
        .bind(routine_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| anyhow!("read routine run {run_id} run_spec: {e}"))?;
        if let Some(spec) = spec.flatten() {
            apply_run_spec(&mut routine, &spec);
        }
        Ok(Some(routine))
    }

    /// The explicit execution mode pinned on `run_id` at claim; `None` for a null-mode run.
    pub async fn run_execution_mode(&self, run_id: &str) -> Result<Option<String>> {
        let mode: Option<Option<String>> = sqlx::query_scalar(
            "SELECT run_spec ->> 'execution_mode' FROM routine_runs WHERE id = $1",
        )
        .bind(run_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|e| anyhow!("read routine run {run_id} execution mode: {e}"))?;
        mode.ok_or_else(|| anyhow!("routine run {run_id} not found while reading its mode"))
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

fn apply_run_spec(routine: &mut RoutineRecord, spec: &Value) {
    let text = |key: &str| spec.get(key).and_then(Value::as_str).map(str::to_string);
    let int = |key: &str| {
        spec.get(key)
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
    };
    if let Some(strategy) = text("execution_strategy") {
        routine.execution_strategy = strategy;
    }
    routine.agent_id = text("agent_id");
    routine.fallback_agent_id = text("fallback_agent_id");
    routine.discord_thread_id = text("discord_thread_id");
    routine.timeout_secs = int("timeout_secs");
    if let Some(max_retries) = int("max_retries") {
        routine.max_retries = max_retries;
    }
}
