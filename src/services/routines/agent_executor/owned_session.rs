//! Persist the actual session a fresh routine started; resolve legacy starts only without a receipt.
use super::*;

impl RoutineAgentExecutor {
    // Process, persistent and DM sessions retain their existing ownership policy.
    pub(super) async fn record_owned_fresh_session(
        &self,
        store: &RoutineStore,
        claimed: &ClaimedRoutineRun,
        result_json: &Value,
        receipt: Option<&str>,
    ) {
        if claimed.execution_strategy != "fresh"
            || execution::AgentExecutionMode::from_result(Some(result_json))
                == execution::AgentExecutionMode::Process
        {
            return;
        }
        if started_turn_is_dm(result_json) {
            tracing::debug!(
                routine_id = %claimed.routine_id,
                run_id = %claimed.run_id,
                "fresh routine ownership not recorded: DM-bound session is not thread-resolvable"
            );
            return;
        }
        let ownership_token = if let Some(receipt) = receipt {
            receipt.to_owned()
        } else {
            let routine = match store.get_routine(&claimed.routine_id).await {
                Ok(Some(routine)) => routine,
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(
                        routine_id = %claimed.routine_id,
                        run_id = %claimed.run_id,
                        error = %error,
                        "fresh routine ownership not recorded: routine lookup failed"
                    );
                    return;
                }
            };
            let controller =
                RoutineSessionController::new(self.pool.clone(), self.health_registry.clone());
            match controller
                .resolve_fresh_ownership_token(&routine, Some(result_json))
                .await
            {
                Ok(Some(ownership_token)) => ownership_token,
                Ok(None) => {
                    // Without a concrete session, leave recovery to the idle backstop;
                    // guessing could target a different session.
                    tracing::debug!(
                        routine_id = %claimed.routine_id,
                        run_id = %claimed.run_id,
                        "fresh routine ownership not recorded: no concrete started session resolvable"
                    );
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        routine_id = %claimed.routine_id,
                        run_id = %claimed.run_id,
                        error = %error,
                        "fresh routine ownership not recorded: session unresolved"
                    );
                    return;
                }
            }
        };
        match store
            .set_run_owned_tmux_session(&claimed.run_id, &ownership_token)
            .await
        {
            Ok(true) => tracing::debug!(
                routine_id = %claimed.routine_id,
                run_id = %claimed.run_id,
                ownership_token = %ownership_token,
                "fresh routine run owned-session recorded"
            ),
            Ok(false) => tracing::debug!(
                routine_id = %claimed.routine_id,
                run_id = %claimed.run_id,
                "fresh routine ownership not recorded: run no longer running"
            ),
            Err(error) => tracing::warn!(
                routine_id = %claimed.routine_id,
                run_id = %claimed.run_id,
                error = %error,
                "fresh routine ownership not recorded: persist failed"
            ),
        }
    }
}

#[cfg(test)]
mod tests;
