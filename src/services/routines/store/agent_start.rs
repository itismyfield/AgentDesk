use super::*;

impl RoutineStore {
    pub async fn mark_agent_turn_started(
        &self,
        run_id: &str,
        turn_id: &str,
        result_json: Option<Value>,
        agent_id: &str,
        attempt_kind: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE routine_runs
            SET action = 'agent',
                turn_id = $2,
                result_json = $3,
                lease_expires_at = NOW() + ($4::bigint * INTERVAL '1 second'),
                next_retry_at = NULL,
                owned_tmux_session = NULL,
                attempts = COALESCE(attempts, '[]'::jsonb) || jsonb_build_array(
                    jsonb_build_object(
                        'event', 'started',
                        'agent_id', $5,
                        'kind', $6,
                        'turn_id', $2,
                        'execution_mode', $3::jsonb ->> 'execution_mode',
                        'provider', $3::jsonb ->> 'provider',
                        'channel_id', $3::jsonb ->> 'channel_id',
                        'at', NOW()
                    )
                ),
                updated_at = NOW()
            WHERE id = $1
              AND status = 'running'
            "#,
        )
        .bind(run_id)
        .bind(turn_id)
        .bind(result_json)
        .bind(RUN_LEASE_SECS)
        .bind(agent_id)
        .bind(attempt_kind)
        .execute(&*self.pool)
        .await
        .map_err(|e| anyhow!("mark routine agent turn started {run_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn confirm_agent_turn_started(&self, run_id: &str, turn_id: &str) -> Result<bool> {
        let result = sqlx::query(CONFIRM_AGENT_TURN_STARTED_QUERY)
            .bind(run_id)
            .bind(turn_id)
            .execute(&*self.pool)
            .await
            .map_err(|e| anyhow!("confirm routine agent turn started {run_id}: {e}"))?;

        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
impl RoutineStore {
    pub(crate) async fn agent_input_intent(
        &self,
        run_id: &str,
    ) -> Result<(Option<String>, Option<Value>)> {
        Ok(sqlx::query_as(
            "SELECT turn_id,result_json FROM routine_runs WHERE id=$1 AND status='running'",
        )
        .bind(run_id)
        .fetch_one(&*self.pool)
        .await?)
    }
    pub(crate) async fn record_agent_input_intent(
        &self,
        run_id: &str,
        previous_turn: Option<&str>,
        turn_id: &str,
        result: &Value,
    ) -> Result<bool> {
        let updated = sqlx::query("UPDATE routine_runs SET action='agent',turn_id=$3,result_json=$4,
            next_retry_at=NULL,owned_tmux_session=NULL,lease_expires_at=NOW()+($5::bigint*INTERVAL '1 second'),
            attempts=COALESCE(attempts,'[]'::jsonb)||jsonb_build_array(jsonb_build_object('event','input_pending','turn_id',$3::text,'execution_mode',$4->'execution_mode','provider',$4->'provider','channel_id',$4->'channel_id','owner_input',$4->'owner_input','at',NOW())),updated_at=NOW()
            WHERE id=$1 AND status='running' AND turn_id IS NOT DISTINCT FROM $2
              AND (result_json->'owner_input' IS NULL
                   OR (result_json->>'status'='started' AND result_json#>'{owner_input,bind}' IS NOT NULL
                       AND result_json#>'{owner_input,bind}'<>'null'::jsonb
                       AND result_json->>'attempt_kind' IS DISTINCT FROM $4->>'attempt_kind'))")
            .bind(run_id).bind(previous_turn).bind(turn_id).bind(result).bind(RUN_LEASE_SECS).execute(&*self.pool).await?;
        Ok(updated.rows_affected() == 1)
    }

    pub(crate) async fn project_agent_input(
        &self,
        run_id: &str,
        turn_id: &str,
        ticket: &str,
        result: &Value,
    ) -> Result<bool> {
        let updated = sqlx::query("UPDATE routine_runs SET result_json=$4,
            attempts=CASE WHEN $4->>'status'='started' AND result_json->>'status' IS DISTINCT FROM 'started'
                THEN COALESCE(attempts,'[]'::jsonb)||jsonb_build_array(jsonb_build_object('event','started','turn_id',$2::text,'execution_mode',$4->'execution_mode','provider',$4->'provider','channel_id',$4->'channel_id','owner_input',$4->'owner_input','at',NOW()))
                ELSE attempts END,updated_at=NOW()
            WHERE id=$1 AND turn_id=$2 AND status='running' AND action='agent'
              AND result_json#>>'{owner_input,ticket}'=$3
              AND ((result_json->'owner_input')-'bind'::text)=(($4::jsonb->'owner_input')-'bind'::text)
              AND (result_json#>'{owner_input,bind}' IS NULL OR result_json#>'{owner_input,bind}'='null'::jsonb
                   OR result_json#>'{owner_input,bind}'=$4#>'{owner_input,bind}')")
            .bind(run_id).bind(turn_id).bind(ticket).bind(result).execute(&*self.pool).await?;
        Ok(updated.rows_affected() == 1)
    }
}
