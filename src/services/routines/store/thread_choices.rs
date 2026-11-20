//! Save a thread only into the definition that selected its agent and provider.
use super::*;

impl RoutineStore {
    pub async fn update_choice_discord_thread_id(
        &self,
        routine_id: &str,
        thread_id: &str,
        agent_id: &str,
        choice: &ProviderChoice,
    ) -> Result<bool> {
        let thread = thread_id.trim();
        if thread.is_empty() {
            return Err(anyhow!("discord_thread_id must not be empty"));
        }
        let found: Option<i32> = sqlx::query_scalar(
            "WITH routine AS (
                UPDATE routines SET discord_thread_id = $2, updated_at = NOW()
                WHERE id = $1 AND status <> 'detached'
                  AND agent_id IS NOT DISTINCT FROM $3::text
                  AND provider IS NOT DISTINCT FROM $4::text
                RETURNING in_flight_run_id
             ), pinned AS (
                UPDATE routine_runs rr
                SET run_spec = jsonb_set(rr.run_spec, '{discord_thread_id}', to_jsonb($2::text))
                FROM routine WHERE rr.id = routine.in_flight_run_id
                  AND rr.status = 'running' AND rr.run_spec IS NOT NULL
                  AND rr.run_spec ->> 'agent_id' IS NOT DISTINCT FROM $3::text
                  AND rr.run_spec ->> 'provider' IS NOT DISTINCT FROM $4::text
             ) SELECT 1 FROM routine",
        )
        .bind(routine_id)
        .bind(thread)
        .bind(agent_id)
        .bind(&choice.provider)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| anyhow!("save routine {routine_id} provider thread: {error}"))?;
        Ok(found.is_some())
    }

    /// A late start can replace its own thread after PATCH, but cannot give it to the new choice.
    pub async fn update_run_discord_thread_id(
        &self,
        run_id: &str,
        thread_id: &str,
    ) -> Result<bool> {
        let thread = thread_id.trim();
        if thread.is_empty() {
            return Err(anyhow!("discord_thread_id must not be empty"));
        }
        sqlx::query_scalar(
            "WITH locked AS (
                SELECT r.id FROM routines r JOIN routine_runs rr ON rr.routine_id = r.id
                WHERE rr.id = $1 FOR UPDATE OF r
             ), pinned AS (
                UPDATE routine_runs rr SET run_spec = CASE WHEN rr.run_spec IS NULL THEN NULL
                    ELSE jsonb_set(rr.run_spec, '{discord_thread_id}', to_jsonb($2::text)) END
                FROM locked WHERE rr.id = $1 AND rr.routine_id = locked.id AND rr.status = 'running'
                RETURNING rr.routine_id, rr.run_spec
             ), routine AS (
                UPDATE routines r SET discord_thread_id = $2, updated_at = NOW()
                FROM pinned WHERE r.id = pinned.routine_id AND r.in_flight_run_id = $1
                  AND r.status <> 'detached'
                  AND (pinned.run_spec IS NULL OR (
                      r.agent_id IS NOT DISTINCT FROM pinned.run_spec ->> 'agent_id'
                      AND r.provider IS NOT DISTINCT FROM pinned.run_spec ->> 'provider'))
             ) SELECT EXISTS(SELECT 1 FROM pinned)",
        )
        .bind(run_id)
        .bind(thread)
        .fetch_one(&*self.pool)
        .await
        .map_err(|error| anyhow!("save routine run {run_id} thread: {error}"))
    }
}
