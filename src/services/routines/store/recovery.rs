//! Expired owner runs keep their database claim until the exact owner acknowledges settlement.
use super::*;

#[derive(sqlx::FromRow)]
pub(super) struct ClosingRun {
    pub(super) routine_id: String,
    pub(super) schedule: Option<String>,
    pub(super) next_due_at: Option<DateTime<Utc>>,
    pub(super) started_at: DateTime<Utc>,
    pub(super) result_json: Option<Value>,
}

#[derive(sqlx::FromRow)]
pub(in crate::services::routines) struct RecoveringOwnerRun {
    pub(super) run_id: String,
    pub(super) turn_id: String,
    pub(super) owner_input: Value,
    pub(in crate::services::routines) owner_confirmed: bool,
}

impl RecoveringOwnerRun {
    pub(in crate::services::routines) fn identity(&self) -> (&str, &str) {
        (&self.run_id, &self.turn_id)
    }

    pub(in crate::services::routines) fn owner_input(&self) -> &Value {
        &self.owner_input
    }
}

impl RoutineStore {
    pub(in crate::services::routines) async fn begin_owner_recovery(
        &self,
    ) -> Result<Vec<RecoveringOwnerRun>> {
        Ok(sqlx::query_as(
            "WITH expired AS (
                SELECT id FROM routine_runs
                WHERE status = 'running' AND action = 'agent' AND turn_id IS NOT NULL
                  AND run_spec->>'retry_after_start' = 'false'
                  AND jsonb_typeof(result_json->'owner_input') = 'object'
                  AND (result_json->>'owner_recovering' = 'true' OR lease_expires_at < NOW())
                FOR UPDATE SKIP LOCKED
             )
             UPDATE routine_runs rr
             SET result_json = jsonb_set(rr.result_json, '{owner_recovering}', 'true'),
                 updated_at = NOW()
             FROM expired WHERE rr.id = expired.id
             RETURNING rr.id AS run_id, rr.turn_id, rr.result_json->'owner_input' AS owner_input,
                       COALESCE(rr.result_json->>'status' IN ('accepted', 'started'), FALSE)
                           AS owner_confirmed",
        )
        .fetch_all(&*self.pool)
        .await?)
    }

    pub(in crate::services::routines) async fn finish_owner_recovery(
        &self,
        run: RecoveringOwnerRun,
        (status, error): (&str, &str),
    ) -> Result<Option<RecoveredRoutineRun>> {
        Ok(sqlx::query_as(
            "WITH exact AS (
                SELECT rr.id FROM routine_runs rr JOIN routines r ON r.id = rr.routine_id
                WHERE rr.id = $1 AND rr.turn_id = $2 AND rr.status = 'running'
                  AND r.in_flight_run_id = rr.id
                  AND rr.result_json->>'owner_recovering' = 'true'
                  AND rr.result_json->'owner_input' = $3
                FOR UPDATE OF rr, r
             ), closed AS (
                UPDATE routine_runs rr
                SET status = 'interrupted', error = $5, finished_at = NOW(),
                    updated_at = NOW(), lease_expires_at = NULL,
                    result_json = (rr.result_json - 'owner_recovering')
                        || jsonb_build_object('status', $4::text, 'error', $5::text)
                FROM exact WHERE rr.id = exact.id
                RETURNING rr.id, rr.routine_id, rr.run_spec
             ), cleared AS (
                UPDATE routines r SET in_flight_run_id = NULL, updated_at = NOW()
                FROM closed WHERE r.id = closed.routine_id AND r.in_flight_run_id = closed.id
                RETURNING r.id, r.script_ref, r.name
             )
             SELECT closed.id AS run_id, cleared.id AS routine_id, cleared.script_ref, cleared.name,
                    closed.run_spec->>'agent_id' AS agent_id,
                    closed.run_spec->>'discord_thread_id' AS discord_thread_id,
                    closed.run_spec->>'execution_strategy' AS execution_strategy,
                    NULL::text AS owned_tmux_session
             FROM closed JOIN cleared ON cleared.id = closed.routine_id",
        )
        .bind(run.run_id)
        .bind(run.turn_id)
        .bind(run.owner_input)
        .bind(status)
        .bind(error)
        .fetch_optional(&*self.pool)
        .await?)
    }

    /// Late evidence annotates the exact closed run without changing its terminal outcome.
    pub(in crate::services::routines) async fn record_late_owner_completions(&self) -> Result<()> {
        sqlx::query(
            "WITH evidence AS (
                SELECT rr.id, observed.created_at, observed.kind
                FROM routine_runs rr
                CROSS JOIN LATERAL (
                    SELECT created_at, 'transcript' AS kind
                    FROM session_transcripts
                    WHERE turn_id = rr.turn_id AND created_at >= rr.started_at
                      AND BTRIM(assistant_message) <> ''
                    UNION ALL
                    SELECT created_at, 'empty_response' AS kind
                    FROM agent_quality_event
                    WHERE correlation_id = rr.turn_id AND source_event_id = rr.turn_id
                      AND created_at >= rr.started_at AND event_type = 'turn_error'
                      AND payload #>> '{details,outcome}' = 'empty_response'
                    ORDER BY created_at ASC, kind DESC
                    LIMIT 1
                ) observed
                WHERE rr.status IN ('failed', 'interrupted')
                  AND rr.result_json->>'status' = 'completion_unconfirmed'
                  AND rr.result_json#>>'{owner_input,context,run_id}' = rr.id
                  AND rr.result_json#>>'{owner_input,context,turn_id}' = rr.turn_id
                  AND NOT (rr.result_json ? 'late_completion')
             )
             UPDATE routine_runs rr SET result_json = jsonb_set(
                rr.result_json, '{late_completion}',
                jsonb_build_object('observed_at', evidence.created_at, 'kind', evidence.kind)
             ), updated_at = NOW()
             FROM evidence WHERE rr.id = evidence.id AND NOT (rr.result_json ? 'late_completion')",
        )
        .execute(&*self.pool)
        .await?;
        Ok(())
    }

    pub(in crate::services::routines) async fn close_owner_delivery(
        &self,
        run: &RunningAgentRoutineRun,
        owner: &Value,
        (error, result, pause): (&str, Value, bool),
    ) -> Result<bool> {
        let Some(turn) = run.turn_id.as_deref() else {
            return Ok(false);
        };
        self.close_run_guarded(
            &run.run_id,
            CloseRun {
                run_status: "failed",
                action: Some("agent"),
                result_json: Some(result),
                error: Some(error),
                checkpoint: None,
                last_result: Some(error),
                next_due_at: NextDueAtUpdate::Preserve,
                pause_routine: pause,
                pause_reason: terminal_failure_pause_reason(pause),
            },
            Some((turn, owner)),
        )
        .await
    }
}
