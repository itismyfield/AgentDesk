use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use std::sync::Arc;

use crate::services::discord::health::{
    HealthRegistry, reserve_headless_agent_turn, reserve_headless_agent_turn_in_dm,
    resolve_bot_http, start_reserved_headless_agent_turn_in_dm,
    start_reserved_headless_agent_turn_with_owner_channel,
};

use super::fresh_context_guaranteed;
use super::runtime::RoutineRunOutcome;
use super::session_control::RoutineSessionController;
use super::store::{
    ClaimedRoutineRun, NextDueAtUpdate, RecoveredRoutineRun, RoutineStore, RunningAgentRoutineRun,
    terminal_failure_should_pause,
};

#[derive(Clone)]
pub struct RoutineAgentExecutor {
    pool: Arc<PgPool>,
    health_registry: Option<Arc<HealthRegistry>>,
    default_completion_timeout_secs: u64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct AgentTranscriptCompletionRow {
    assistant_message: String,
    duration_ms: Option<i64>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct AgentQualityCompletionRow {
    event_type: String,
    outcome: Option<String>,
    duration_ms: Option<i64>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentTurnCompletionEvidence {
    AssistantTranscript,
    NoReplyTranscript,
    TerminalTurn,
}

impl AgentTurnCompletionEvidence {
    fn as_str(self) -> &'static str {
        match self {
            Self::AssistantTranscript => "session_transcripts",
            Self::NoReplyTranscript => "session_transcripts_no_reply",
            Self::TerminalTurn => "agent_quality_event_terminal",
        }
    }

    fn confirms_assistant_delivery(self) -> bool {
        matches!(self, Self::AssistantTranscript)
    }

    fn is_transcript(self) -> bool {
        matches!(self, Self::AssistantTranscript | Self::NoReplyTranscript)
    }
}

#[derive(Debug, Clone)]
struct AgentTurnCompletion {
    assistant_message: Option<String>,
    duration_ms: Option<i64>,
    created_at: DateTime<Utc>,
    evidence: AgentTurnCompletionEvidence,
    terminal_status: Option<String>,
}

impl RoutineAgentExecutor {
    pub fn new(
        pool: Arc<PgPool>,
        health_registry: Option<Arc<HealthRegistry>>,
        default_completion_timeout_secs: u64,
    ) -> Self {
        Self {
            pool,
            health_registry,
            default_completion_timeout_secs: default_completion_timeout_secs.max(1),
        }
    }

    pub async fn start_agent_run(
        &self,
        store: &RoutineStore,
        claimed: ClaimedRoutineRun,
        prompt: String,
        dm_user_id: Option<String>,
        checkpoint: Option<Value>,
        last_result: Option<String>,
        next_due_at: Option<DateTime<Utc>>,
        pause_on_terminal_failure: bool,
    ) -> Result<RoutineRunOutcome> {
        let action = "agent".to_string();
        let agent_id = claimed
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("routine agent action requires routine.agent_id"))?
            .to_string();
        let result = self
            .start_turn(
                store,
                &claimed,
                &agent_id,
                "primary",
                &prompt,
                dm_user_id.as_deref(),
                &checkpoint,
                next_due_at,
                pause_on_terminal_failure,
            )
            .await;
        match result {
            Ok(started) if started.started => Ok(RoutineRunOutcome {
                run_id: claimed.run_id,
                routine_id: claimed.routine_id,
                script_ref: claimed.script_ref,
                action,
                status: "running".to_string(),
                result_json: Some(started.result_json),
                error: None,
                fresh_context_guaranteed: started.fresh_context_guaranteed,
            }),
            Ok(started) => {
                // Prefer the dispatch label the routine script returned (e.g.
                // "dependency update check dispatched"); fall back to the
                // generic message only when the script omitted lastResult.
                let consumed_last_result = last_result
                    .as_deref()
                    .unwrap_or("headless command consumed without starting an agent turn");
                let closed = store
                    .complete_agent_run(
                        &claimed.run_id,
                        Some(started.result_json.clone()),
                        checkpoint,
                        Some(consumed_last_result),
                        match next_due_at {
                            Some(value) => NextDueAtUpdate::Set(value),
                            None => NextDueAtUpdate::Preserve,
                        },
                    )
                    .await?;
                if !closed {
                    return Err(anyhow!(
                        "routine agent run {} was already closed before consumed outcome",
                        claimed.run_id
                    ));
                }
                Ok(RoutineRunOutcome {
                    run_id: claimed.run_id,
                    routine_id: claimed.routine_id,
                    script_ref: claimed.script_ref,
                    action,
                    status: "succeeded".to_string(),
                    result_json: Some(started.result_json),
                    error: None,
                    fresh_context_guaranteed: started.fresh_context_guaranteed,
                })
            }
            Err(error) => {
                let message = error.to_string();
                self.handle_claimed_agent_failure(
                    store,
                    claimed,
                    prompt,
                    dm_user_id,
                    checkpoint,
                    last_result,
                    next_due_at,
                    &agent_id,
                    "primary",
                    &message,
                    action,
                    pause_on_terminal_failure,
                )
                .await
            }
        }
    }

    async fn handle_claimed_agent_failure(
        &self,
        store: &RoutineStore,
        claimed: ClaimedRoutineRun,
        prompt: String,
        dm_user_id: Option<String>,
        checkpoint: Option<Value>,
        last_result: Option<String>,
        next_due_at: Option<DateTime<Utc>>,
        failed_agent_id: &str,
        attempt_kind: &str,
        message: &str,
        action: String,
        pause_on_terminal_failure: bool,
    ) -> Result<RoutineRunOutcome> {
        let fresh_context_guaranteed = fresh_context_guaranteed(&claimed.execution_strategy, false);
        match claimed_failure_recovery_plan(&claimed, failed_agent_id, attempt_kind) {
            AgentFailureRecoveryPlan::Retry {
                retry_count_after_increment,
                next_retry_at: retry_at,
            } => {
                let result_json = Some(retry_scheduled_result_for_claimed(
                    &claimed,
                    failed_agent_id,
                    attempt_kind,
                    message,
                    &prompt,
                    retry_at,
                    retry_count_after_increment,
                    &checkpoint,
                    next_due_at,
                ));
                let scheduled = store
                    .schedule_agent_retry(
                        &claimed.run_id,
                        retry_at,
                        result_json.clone(),
                        message,
                        Some(failed_agent_id),
                        attempt_kind,
                    )
                    .await?;
                if !scheduled {
                    return Err(anyhow!(
                        "routine agent run {} was already closed before retry scheduling",
                        claimed.run_id
                    ));
                }
                return Ok(RoutineRunOutcome {
                    run_id: claimed.run_id,
                    routine_id: claimed.routine_id,
                    script_ref: claimed.script_ref,
                    action,
                    status: "running".to_string(),
                    result_json,
                    error: Some(message.to_string()),
                    fresh_context_guaranteed,
                });
            }
            AgentFailureRecoveryPlan::Fallback {
                agent_id: fallback_agent_id,
            } => {
                match self
                    .start_turn(
                        store,
                        &claimed,
                        &fallback_agent_id,
                        "fallback",
                        &prompt,
                        dm_user_id.as_deref(),
                        &checkpoint,
                        next_due_at,
                        pause_on_terminal_failure,
                    )
                    .await
                {
                    Ok(started) if started.started => {
                        return Ok(RoutineRunOutcome {
                            run_id: claimed.run_id,
                            routine_id: claimed.routine_id,
                            script_ref: claimed.script_ref,
                            action,
                            status: "running".to_string(),
                            result_json: Some(started.result_json),
                            error: Some(message.to_string()),
                            fresh_context_guaranteed: started.fresh_context_guaranteed,
                        });
                    }
                    Ok(started) => {
                        // Preserve the routine's dispatch label across the
                        // fallback path too; only the agent that never started
                        // a turn reaches here, so the script's lastResult is
                        // still the most meaningful summary.
                        let consumed_last_result = last_result.as_deref().unwrap_or(
                            "fallback headless command consumed without starting an agent turn",
                        );
                        let closed = store
                            .complete_agent_run(
                                &claimed.run_id,
                                Some(started.result_json.clone()),
                                checkpoint,
                                Some(consumed_last_result),
                                match next_due_at {
                                    Some(value) => NextDueAtUpdate::Set(value),
                                    None => NextDueAtUpdate::Preserve,
                                },
                            )
                            .await?;
                        if !closed {
                            return Err(anyhow!(
                                "routine agent run {} was already closed before fallback consumed outcome",
                                claimed.run_id
                            ));
                        }
                        return Ok(RoutineRunOutcome {
                            run_id: claimed.run_id,
                            routine_id: claimed.routine_id,
                            script_ref: claimed.script_ref,
                            action,
                            status: "succeeded".to_string(),
                            result_json: Some(started.result_json),
                            error: Some(message.to_string()),
                            fresh_context_guaranteed: started.fresh_context_guaranteed,
                        });
                    }
                    Err(fallback_error) => {
                        let combined = format!(
                            "{message}; fallback agent {fallback_agent_id} failed: {fallback_error}"
                        );
                        return fail_claimed_agent_run(
                            store,
                            claimed,
                            action,
                            combined,
                            Some(&fallback_agent_id),
                            "fallback",
                            pause_on_terminal_failure,
                        )
                        .await;
                    }
                }
            }
            AgentFailureRecoveryPlan::Fail => {}
        }

        fail_claimed_agent_run(
            store,
            claimed,
            action,
            message.to_string(),
            Some(failed_agent_id),
            attempt_kind,
            pause_on_terminal_failure,
        )
        .await
    }

    pub async fn poll_agent_runs(
        &self,
        store: &RoutineStore,
        limit: u32,
        pause_on_terminal_failure: bool,
    ) -> Result<Vec<RoutineRunOutcome>> {
        store.heartbeat_running_agent_runs().await?;
        let pending = store.list_running_agent_runs(limit).await?;
        let mut outcomes = Vec::new();
        for run in pending {
            let fresh_context_guaranteed =
                fresh_context_guaranteed_from_result(run.result_json.as_ref());
            if run.turn_id.is_none() {
                if let Some(outcome) = self
                    .restart_due_retry(store, run, pause_on_terminal_failure)
                    .await?
                {
                    outcomes.push(outcome);
                }
                continue;
            }

            if let Some(completion) = self.find_turn_completion(&run).await? {
                let checkpoint =
                    pending_checkpoint_for_completion(run.result_json.as_ref(), &completion);
                let next_due_at = pending_next_due_at(run.result_json.as_ref());
                let last_result = completion_last_result(&completion);
                let result_json = Some(completed_result(&run, &completion, last_result.as_str()));
                let closed = store
                    .complete_agent_run(
                        &run.run_id,
                        result_json.clone(),
                        checkpoint,
                        Some(last_result.as_str()),
                        match next_due_at {
                            Some(value) => NextDueAtUpdate::Set(value),
                            None => NextDueAtUpdate::Preserve,
                        },
                    )
                    .await?;
                if !closed {
                    continue;
                }
                self.teardown_fresh_agent_session(
                    store,
                    &run.routine_id,
                    result_json.as_ref(),
                    "routine fresh agent run completed",
                )
                .await;
                outcomes.push(RoutineRunOutcome {
                    run_id: run.run_id,
                    routine_id: run.routine_id,
                    script_ref: run.script_ref,
                    action: "agent".to_string(),
                    status: "succeeded".to_string(),
                    result_json,
                    error: None,
                    fresh_context_guaranteed,
                });
                continue;
            }

            let timeout_secs = self.timeout_secs_for_run(&run);
            if self.has_timed_out(&run, timeout_secs).await? {
                let message = format!(
                    "routine agent turn timed out after {} seconds",
                    timeout_secs
                );
                let result_json = Some(merge_pending_result(&run, "timeout", Some(&message), None));
                self.teardown_fresh_agent_session(
                    store,
                    &run.routine_id,
                    result_json.as_ref(),
                    "routine fresh agent run timed out",
                )
                .await;
                let failed_agent_id = current_agent_id_from_result(run.result_json.as_ref())
                    .or(run.agent_id.as_deref())
                    .map(str::to_string);
                let attempt_kind = current_attempt_kind_from_result(run.result_json.as_ref())
                    .unwrap_or("primary")
                    .to_string();
                if let Some(outcome) = self
                    .handle_running_agent_failure(
                        store,
                        run,
                        &message,
                        result_json,
                        failed_agent_id.as_deref(),
                        &attempt_kind,
                        pause_on_terminal_failure,
                    )
                    .await?
                {
                    outcomes.push(outcome);
                }
                continue;
            }
        }
        Ok(outcomes)
    }

    async fn restart_due_retry(
        &self,
        store: &RoutineStore,
        run: RunningAgentRoutineRun,
        pause_on_terminal_failure: bool,
    ) -> Result<Option<RoutineRunOutcome>> {
        let prompt = match pending_prompt(run.result_json.as_ref()) {
            Some(prompt) => prompt.to_string(),
            None => {
                let message = "routine retry cannot restart because prompt is missing";
                let result_json = Some(pending_result_without_fresh_context_guarantee(
                    &run,
                    "failed",
                    Some(message),
                ));
                let closed = store
                    .fail_agent_run(&run.run_id, message, result_json.clone(), None)
                    .await?;
                return Ok(closed.then(|| RoutineRunOutcome {
                    run_id: run.run_id,
                    routine_id: run.routine_id,
                    script_ref: run.script_ref,
                    action: "agent".to_string(),
                    status: "failed".to_string(),
                    result_json,
                    error: Some(message.to_string()),
                    fresh_context_guaranteed: false,
                }));
            }
        };
        let Some(agent_id) = run
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
        else {
            let message = "routine retry cannot restart because routine.agent_id is missing";
            let result_json = Some(pending_result_without_fresh_context_guarantee(
                &run,
                "failed",
                Some(message),
            ));
            let closed = store
                .fail_agent_run(&run.run_id, message, result_json.clone(), None)
                .await?;
            return Ok(closed.then(|| RoutineRunOutcome {
                run_id: run.run_id,
                routine_id: run.routine_id,
                script_ref: run.script_ref,
                action: "agent".to_string(),
                status: "failed".to_string(),
                result_json,
                error: Some(message.to_string()),
                fresh_context_guaranteed: false,
            }));
        };
        let checkpoint = pending_checkpoint(run.result_json.as_ref());
        let next_due_at = pending_next_due_at(run.result_json.as_ref());
        let claimed = claimed_from_running_run(&run);
        match self
            .start_turn(
                store,
                &claimed,
                &agent_id,
                "retry",
                &prompt,
                None,
                &checkpoint,
                next_due_at,
                pause_on_terminal_failure,
            )
            .await
        {
            Ok(started) if started.started => Ok(Some(RoutineRunOutcome {
                run_id: run.run_id,
                routine_id: run.routine_id,
                script_ref: run.script_ref,
                action: "agent".to_string(),
                status: "running".to_string(),
                result_json: Some(started.result_json),
                error: None,
                fresh_context_guaranteed: started.fresh_context_guaranteed,
            })),
            Ok(started) => {
                let last_result = "retry headless command consumed without starting an agent turn";
                let closed = store
                    .complete_agent_run(
                        &run.run_id,
                        Some(started.result_json.clone()),
                        checkpoint,
                        Some(last_result),
                        match next_due_at {
                            Some(value) => NextDueAtUpdate::Set(value),
                            None => NextDueAtUpdate::Preserve,
                        },
                    )
                    .await?;
                Ok(closed.then(|| RoutineRunOutcome {
                    run_id: run.run_id,
                    routine_id: run.routine_id,
                    script_ref: run.script_ref,
                    action: "agent".to_string(),
                    status: "succeeded".to_string(),
                    result_json: Some(started.result_json),
                    error: None,
                    fresh_context_guaranteed: started.fresh_context_guaranteed,
                }))
            }
            Err(error) => {
                let message = error.to_string();
                let result_json = Some(pending_result_without_fresh_context_guarantee(
                    &run,
                    "failed_to_start",
                    Some(&message),
                ));
                self.handle_running_agent_failure(
                    store,
                    run,
                    &message,
                    result_json,
                    Some(&agent_id),
                    "retry",
                    pause_on_terminal_failure,
                )
                .await
            }
        }
    }

    async fn handle_running_agent_failure(
        &self,
        store: &RoutineStore,
        run: RunningAgentRoutineRun,
        message: &str,
        result_json: Option<Value>,
        failed_agent_id: Option<&str>,
        attempt_kind: &str,
        pause_on_terminal_failure: bool,
    ) -> Result<Option<RoutineRunOutcome>> {
        let fresh_context_guaranteed = fresh_context_guaranteed_from_result(result_json.as_ref());
        match running_failure_recovery_plan(&run, failed_agent_id, attempt_kind) {
            AgentFailureRecoveryPlan::Retry {
                retry_count_after_increment: retry_count,
                next_retry_at,
            } => {
                let result_json = Some(retry_scheduled_result(
                    &run,
                    result_json,
                    message,
                    retry_count,
                    next_retry_at,
                ));
                let scheduled = store
                    .schedule_agent_retry(
                        &run.run_id,
                        next_retry_at,
                        result_json.clone(),
                        message,
                        failed_agent_id,
                        attempt_kind,
                    )
                    .await?;
                return Ok(scheduled.then(|| RoutineRunOutcome {
                    run_id: run.run_id,
                    routine_id: run.routine_id,
                    script_ref: run.script_ref,
                    action: "agent".to_string(),
                    status: "running".to_string(),
                    result_json,
                    error: Some(message.to_string()),
                    fresh_context_guaranteed: false,
                }));
            }
            AgentFailureRecoveryPlan::Fallback {
                agent_id: fallback_agent_id,
            } => {
                let Some(prompt) = pending_prompt(run.result_json.as_ref()).map(str::to_string)
                else {
                    let result_json = Some(pending_result_without_fresh_context_guarantee(
                        &run,
                        "failed",
                        Some(message),
                    ));
                    let closed = if terminal_failure_should_pause(pause_on_terminal_failure) {
                        store
                            .fail_run_and_pause_routine(&run.run_id, message, result_json.clone())
                            .await?
                    } else {
                        store
                            .fail_agent_run(&run.run_id, message, result_json.clone(), None)
                            .await?
                    };
                    return Ok(closed.then(|| RoutineRunOutcome {
                        run_id: run.run_id,
                        routine_id: run.routine_id,
                        script_ref: run.script_ref,
                        action: "agent".to_string(),
                        status: "failed".to_string(),
                        result_json,
                        error: Some(message.to_string()),
                        fresh_context_guaranteed: false,
                    }));
                };
                let checkpoint = pending_checkpoint(run.result_json.as_ref());
                let next_due_at = pending_next_due_at(run.result_json.as_ref());
                let claimed = claimed_from_running_run(&run);
                match self
                    .start_turn(
                        store,
                        &claimed,
                        &fallback_agent_id,
                        "fallback",
                        &prompt,
                        None,
                        &checkpoint,
                        next_due_at,
                        pause_on_terminal_failure,
                    )
                    .await
                {
                    Ok(started) if started.started => {
                        return Ok(Some(RoutineRunOutcome {
                            run_id: run.run_id,
                            routine_id: run.routine_id,
                            script_ref: run.script_ref,
                            action: "agent".to_string(),
                            status: "running".to_string(),
                            result_json: Some(started.result_json),
                            error: Some(message.to_string()),
                            fresh_context_guaranteed: started.fresh_context_guaranteed,
                        }));
                    }
                    Ok(started) => {
                        let last_result =
                            "fallback headless command consumed without starting an agent turn";
                        let closed = store
                            .complete_agent_run(
                                &run.run_id,
                                Some(started.result_json.clone()),
                                checkpoint,
                                Some(last_result),
                                match next_due_at {
                                    Some(value) => NextDueAtUpdate::Set(value),
                                    None => NextDueAtUpdate::Preserve,
                                },
                            )
                            .await?;
                        return Ok(closed.then(|| RoutineRunOutcome {
                            run_id: run.run_id,
                            routine_id: run.routine_id,
                            script_ref: run.script_ref,
                            action: "agent".to_string(),
                            status: "succeeded".to_string(),
                            result_json: Some(started.result_json),
                            error: Some(message.to_string()),
                            fresh_context_guaranteed: started.fresh_context_guaranteed,
                        }));
                    }
                    Err(fallback_error) => {
                        let combined = format!(
                            "{message}; fallback agent {fallback_agent_id} failed: {fallback_error}"
                        );
                        let result_json = Some(pending_result_without_fresh_context_guarantee(
                            &run,
                            "failed",
                            Some(&combined),
                        ));
                        let closed = if terminal_failure_should_pause(pause_on_terminal_failure) {
                            store
                                .fail_run_and_pause_routine(
                                    &run.run_id,
                                    &combined,
                                    result_json.clone(),
                                )
                                .await?
                        } else {
                            store
                                .fail_agent_run(&run.run_id, &combined, result_json.clone(), None)
                                .await?
                        };
                        return Ok(closed.then(|| RoutineRunOutcome {
                            run_id: run.run_id,
                            routine_id: run.routine_id,
                            script_ref: run.script_ref,
                            action: "agent".to_string(),
                            status: "failed".to_string(),
                            result_json,
                            error: Some(combined),
                            fresh_context_guaranteed: false,
                        }));
                    }
                }
            }
            AgentFailureRecoveryPlan::Fail => {}
        }

        let result_json =
            result_json.or_else(|| Some(merge_pending_result(&run, "failed", Some(message), None)));
        let closed = if terminal_failure_should_pause(pause_on_terminal_failure) {
            store
                .fail_run_and_pause_routine(&run.run_id, message, result_json.clone())
                .await?
        } else {
            store
                .fail_agent_run(&run.run_id, message, result_json.clone(), None)
                .await?
        };
        Ok(closed.then(|| RoutineRunOutcome {
            run_id: run.run_id,
            routine_id: run.routine_id,
            script_ref: run.script_ref,
            action: "agent".to_string(),
            status: "failed".to_string(),
            result_json,
            error: Some(message.to_string()),
            fresh_context_guaranteed,
        }))
    }

    pub(crate) async fn teardown_fresh_agent_session(
        &self,
        store: &RoutineStore,
        routine_id: &str,
        result_json: Option<&Value>,
        reason: &str,
    ) {
        let routine = match store.get_routine(routine_id).await {
            Ok(Some(routine)) => routine,
            Ok(None) => {
                tracing::warn!(
                    routine_id,
                    "fresh routine session teardown skipped: routine row not found"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    routine_id,
                    error = %error,
                    "fresh routine session teardown skipped: routine lookup failed"
                );
                return;
            }
        };
        if routine.execution_strategy != "fresh" {
            return;
        }

        let controller =
            RoutineSessionController::new(self.pool.clone(), self.health_registry.clone());
        match controller
            .teardown_fresh_session(&routine, result_json, reason)
            .await
        {
            Ok(result) => tracing::info!(
                routine_id,
                tmux_session = %result.tmux_session,
                tmux_killed = result.tmux_killed,
                disconnected_sessions = result.disconnected_sessions,
                "fresh routine session teardown complete"
            ),
            Err(error) => tracing::warn!(
                routine_id,
                error = %error,
                "fresh routine session teardown failed"
            ),
        }
    }

    /// Boot-recovery reap (#3022): after a stale fresh run is marked
    /// `interrupted` at worker startup, tear down the exact session it recorded
    /// as owned. Requires positive ownership proof (`owned_tmux_session` set on a
    /// `fresh` run); runs that own nothing are skipped, so an interrupted run can
    /// never reap a session it did not create. Idempotent — if the session is
    /// already gone the teardown is a harmless no-op.
    ///
    /// Called ONLY from boot recovery, which runs before the routine tick loop
    /// starts, so on a single instance there is no concurrent claimer that could
    /// re-create the deterministic fresh session under this run. The periodic
    /// recovery path deliberately does NOT call this (its concurrent claims would
    /// race the reap against a live replacement turn). As defence-in-depth for a
    /// co-booting second instance whose tick loop is already claiming, the reap
    /// is skipped whenever a *different* run for the routine is already `running`
    /// (`routine_has_other_running_run`, which excludes this recovered run) —
    /// that other run owns the deterministic session now, and killing by name
    /// would tear down its live turn.
    ///
    /// The replacement decision is made by the presence of a different running
    /// run, NOT by the owned session row's status: after a dcserver restart the
    /// stranded orphan still carries its turn-start `turn_active`/`working`
    /// status (recovery never updates `sessions`), so a status check would skip
    /// the very orphan this reap must collect.
    pub(crate) async fn teardown_recovered_fresh_session(
        &self,
        store: &RoutineStore,
        recovered: &RecoveredRoutineRun,
    ) {
        let Some(owned_tmux_session) = recovered.boot_recovery_owned_session() else {
            return;
        };
        let owned_tmux_session = owned_tmux_session.to_string();
        match store
            .routine_has_other_running_run(&recovered.routine_id, &recovered.run_id)
            .await
        {
            Ok(false) => {}
            Ok(true) => {
                tracing::debug!(
                    routine_id = %recovered.routine_id,
                    run_id = %recovered.run_id,
                    "recovered fresh session teardown skipped: a replacement run is already running (owns the deterministic session)"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    routine_id = %recovered.routine_id,
                    run_id = %recovered.run_id,
                    error = %error,
                    "recovered fresh session teardown skipped: replacement-run check failed"
                );
                return;
            }
        }
        let routine = match store.get_routine(&recovered.routine_id).await {
            Ok(Some(routine)) => routine,
            Ok(None) => {
                tracing::warn!(
                    routine_id = %recovered.routine_id,
                    run_id = %recovered.run_id,
                    "recovered fresh session teardown skipped: routine row not found"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    routine_id = %recovered.routine_id,
                    run_id = %recovered.run_id,
                    error = %error,
                    "recovered fresh session teardown skipped: routine lookup failed"
                );
                return;
            }
        };
        if routine.execution_strategy != "fresh" {
            return;
        }
        let controller =
            RoutineSessionController::new(self.pool.clone(), self.health_registry.clone());
        match controller
            .teardown_fresh_session_by_name(
                &routine,
                &owned_tmux_session,
                "routine fresh run interrupted",
            )
            .await
        {
            Ok(result) => tracing::info!(
                routine_id = %recovered.routine_id,
                run_id = %recovered.run_id,
                tmux_session = %result.tmux_session,
                tmux_killed = result.tmux_killed,
                disconnected_sessions = result.disconnected_sessions,
                "recovered fresh routine session reaped"
            ),
            Err(error) => tracing::warn!(
                routine_id = %recovered.routine_id,
                run_id = %recovered.run_id,
                tmux_session = %owned_tmux_session,
                error = %error,
                "recovered fresh routine session teardown failed"
            ),
        }
    }

    async fn start_turn(
        &self,
        store: &RoutineStore,
        claimed: &ClaimedRoutineRun,
        agent_id: &str,
        attempt_kind: &str,
        prompt: &str,
        dm_user_id: Option<&str>,
        checkpoint: &Option<Value>,
        next_due_at: Option<DateTime<Utc>>,
        _pause_on_terminal_failure: bool,
    ) -> Result<StartedAgentTurn> {
        let Some(registry) = self.health_registry.as_deref() else {
            return Err(anyhow!(
                "routine agent action requires discord runtime health registry"
            ));
        };

        let bindings = crate::db::agents::load_agent_channel_bindings_pg(&self.pool, agent_id)
            .await
            .map_err(|error| anyhow!("load agent bindings for {agent_id}: {error}"))?
            .ok_or_else(|| anyhow!("agent {agent_id} not found"))?;
        let provider = bindings
            .resolved_primary_provider_kind()
            .ok_or_else(|| anyhow!("agent {agent_id} primary provider is not configured"))?;
        let primary_channel = bindings
            .primary_channel()
            .ok_or_else(|| anyhow!("agent {agent_id} primary channel is not configured"))?;
        let Some(channel_id_num) =
            crate::services::dispatches::outbox_route::resolve_channel_alias_pub(&primary_channel)
                .or_else(|| primary_channel.parse::<u64>().ok())
        else {
            return Err(anyhow!(
                "agent {agent_id} primary channel is invalid: {primary_channel}"
            ));
        };
        let channel_id = poise::serenity_prelude::ChannelId::new(channel_id_num);
        let dm_user_id_num = match dm_user_id {
            Some(value) => Some(value.parse::<u64>().map_err(|error| {
                anyhow!("RoutineAction.agent.dmUserId must be a Discord snowflake string: {error}")
            })?),
            None => None,
        };

        let (turn_channel_id, discord_thread_id, delivery_bot, reservation) = if let Some(
            dm_user_id_num,
        ) = dm_user_id_num
        {
            let (dm_channel_id, reservation) = reserve_headless_agent_turn_in_dm(
                    registry,
                    channel_id,
                    dm_user_id_num,
                    &provider,
                )
                .await
                .map_err(|error| {
                    anyhow!(
                        "reserve routine agent DM turn for {agent_id} and user {dm_user_id_num}: {error}"
                    )
                })?;
            (dm_channel_id, None, "dm".to_string(), reservation)
        } else {
            let routine_channel = self
                .resolve_or_create_routine_thread(
                    store,
                    registry,
                    claimed,
                    agent_id,
                    provider.as_str(),
                    channel_id,
                )
                .await;
            let (turn_channel_id, discord_thread_id, delivery_bot) = match routine_channel {
                Ok(target) => (
                    target.channel_id,
                    target.discord_thread_id,
                    target.delivery_bot,
                ),
                Err(error) => {
                    let warning = error.to_string();
                    let _ = store
                        .record_discord_log_result(&claimed.run_id, "failed", Some(&warning))
                        .await;
                    tracing::warn!(
                        routine_id = claimed.routine_id,
                        run_id = claimed.run_id,
                        error = %warning,
                        "routine thread setup failed; falling back to agent primary channel"
                    );
                    (channel_id, None, "notify".to_string())
                }
            };
            (
                turn_channel_id,
                discord_thread_id,
                delivery_bot,
                reserve_headless_agent_turn(turn_channel_id),
            )
        };
        let turn_id = reservation.turn_id().to_string();

        let mut result_json = json!({
            "status": "started",
            "turn_id": turn_id.clone(),
            "agent_id": agent_id,
            "provider": provider.as_str(),
            "channel_id": turn_channel_id.get().to_string(),
            "parent_channel_id": channel_id_num.to_string(),
            "discord_thread_id": discord_thread_id,
            "dm_user_id": dm_user_id,
            "is_dm": dm_user_id.is_some(),
            "routine_id": claimed.routine_id,
            "run_id": claimed.run_id,
            "script_ref": claimed.script_ref,
            "attempt_kind": attempt_kind,
            "prompt": prompt,
            "completion_evidence": "session_transcripts",
            "fresh_context_guaranteed":
                fresh_context_guaranteed(&claimed.execution_strategy, false),
            "checkpoint": checkpoint,
            "next_due_at": next_due_at.map(|value| value.to_rfc3339()),
        });
        let updated = store
            .mark_agent_turn_started(
                &claimed.run_id,
                &turn_id,
                Some(result_json.clone()),
                agent_id,
                attempt_kind,
            )
            .await?;
        if !updated {
            return Err(anyhow!(
                "routine agent run {} vanished before turn_id could be stored",
                claimed.run_id
            ));
        }

        let metadata = Some(json!({
            "agent_id": agent_id,
            "delivery_bot": delivery_bot,
            "routine_id": claimed.routine_id,
            "routine_run_id": claimed.run_id,
            "script_ref": claimed.script_ref,
            "execution_strategy": claimed.execution_strategy,
            "fresh_context_guaranteed":
                fresh_context_guaranteed(&claimed.execution_strategy, false),
            "turn_id": turn_id.clone(),
            "parent_channel_id": channel_id_num.to_string(),
            "discord_thread_id": discord_thread_id,
            "dm_user_id": dm_user_id,
            "is_dm": dm_user_id.is_some(),
        }));
        let outcome = if let Some(dm_user_id_num) = dm_user_id_num {
            start_reserved_headless_agent_turn_in_dm(
                registry,
                channel_id,
                turn_channel_id,
                dm_user_id_num,
                provider.clone(),
                prompt.to_string(),
                Some("routine".to_string()),
                metadata,
                reservation,
            )
            .await
        } else {
            // #5: Pass the agent's REAL primary channel/alias as the workspace
            // hint so `resolve_workspace` resolves for alias/by-name routine
            // agents, and carry the synthetic routine session name separately as
            // the tmux-session label so the routine still gets its DISTINCT tmux
            // session (#3463: routine-name-first avoids cross-routine collision).
            let channel_name_hint = Some(primary_channel.clone());
            let tmux_session_label = Some(routine_agent_session_name(&claimed.name, agent_id));
            start_reserved_headless_agent_turn_with_owner_channel(
                registry,
                channel_id,
                turn_channel_id,
                provider.clone(),
                prompt.to_string(),
                Some("routine".to_string()),
                metadata,
                channel_name_hint,
                tmux_session_label,
                reservation,
            )
            .await
        }
        .map_err(|error| anyhow!("start routine agent turn for {agent_id}: {error}"))?;

        if outcome.turn_id != turn_id {
            return Err(anyhow!(
                "reserved routine agent turn id mismatch: expected {} but started {}",
                turn_id,
                outcome.turn_id
            ));
        }
        let started = outcome.status.as_str() == "started";
        let provider_fresh_context = fresh_context_guaranteed(&claimed.execution_strategy, started);
        if !started && let Some(object) = result_json.as_object_mut() {
            object.insert(
                "status".to_string(),
                Value::String(outcome.status.as_str().to_string()),
            );
            object.insert(
                "completion_evidence".to_string(),
                Value::String("headless_start_outcome".to_string()),
            );
        }
        let durable_confirmation_persisted = if provider_fresh_context {
            let confirmed_result = result_with_fresh_context_guarantee(result_json.clone(), true);
            match store
                .confirm_agent_turn_started(&claimed.run_id, &turn_id)
                .await
            {
                Ok(true) => {
                    result_json = confirmed_result;
                    true
                }
                Ok(false) => {
                    tracing::warn!(
                        routine_id = %claimed.routine_id,
                        run_id = %claimed.run_id,
                        turn_id = %turn_id,
                        "fresh provider start confirmation found no matching routine agent turn"
                    );
                    false
                }
                Err(error) => {
                    tracing::warn!(
                        routine_id = %claimed.routine_id,
                        run_id = %claimed.run_id,
                        turn_id = %turn_id,
                        error = %error,
                        "failed to persist fresh provider start confirmation"
                    );
                    false
                }
            }
        } else {
            false
        };
        let fresh_context_guaranteed = confirmed_fresh_context_guarantee(
            provider_fresh_context,
            durable_confirmation_persisted,
        );
        result_json = result_with_fresh_context_guarantee(result_json, fresh_context_guaranteed);

        // #3022: persist run -> fresh-session ownership now that the session is
        // up, so boot recovery can reap this exact session if a dcserver restart
        // orphans it. Best-effort: a failure here only loses the boot-recovery
        // backstop (the in-line completion path still tears the session down),
        // so it must never fail the started turn.
        if started {
            self.record_owned_fresh_session(store, claimed, &result_json)
                .await;
        }

        Ok(StartedAgentTurn {
            result_json,
            started,
            fresh_context_guaranteed,
        })
    }

    /// Records the tmux session a freshly-started fresh-routine run owns (#3022).
    /// No-op for non-fresh routines (they reuse a persistent session that must
    /// survive). Best-effort and non-fatal: every failure path only forgoes the
    /// boot-recovery reap backstop, never the turn itself.
    ///
    /// DM-bound fresh actions (`dmUserId`) are intentionally NOT recorded: the
    /// DM session is named `dm-<user_id>` in a DM channel with no
    /// `thread_channel_id`, so the thread-based resolver would record the *wrong*
    /// token and boot recovery could reap an unrelated session. Recording nothing
    /// keeps such a session out of the reap (no positive ownership proof) and
    /// lets the existing idle-kill backstop collect it — the pre-#3022 behavior,
    /// with no risk of killing the wrong session.
    async fn record_owned_fresh_session(
        &self,
        store: &RoutineStore,
        claimed: &ClaimedRoutineRun,
        result_json: &Value,
    ) {
        if claimed.execution_strategy != "fresh" {
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
        let ownership_token = match controller
            .resolve_fresh_ownership_token(&routine, Some(result_json))
            .await
        {
            Ok(Some(ownership_token)) => ownership_token,
            Ok(None) => {
                // No concrete started session row was resolvable; recording a
                // guessed/derived token risks reaping a non-existent session
                // and leaving the real orphan alive (#3022). Leave it to the
                // idle-kill backstop instead.
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

    async fn find_turn_completion(
        &self,
        run: &RunningAgentRoutineRun,
    ) -> Result<Option<AgentTurnCompletion>> {
        let Some(turn_id) = run.turn_id.as_deref() else {
            return Ok(None);
        };
        let transcript = sqlx::query_as::<_, AgentTranscriptCompletionRow>(
            r#"
            SELECT assistant_message, duration_ms::bigint AS duration_ms, created_at
            FROM session_transcripts
            WHERE turn_id = $1
              AND created_at >= $2
              AND BTRIM(assistant_message) <> ''
            ORDER BY created_at ASC
            LIMIT 1
            "#,
        )
        .bind(turn_id)
        .bind(run.started_at)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| {
            anyhow!(
                "lookup routine agent transcript {} for run {}: {error}",
                turn_id,
                run.run_id
            )
        })?;
        if let Some(transcript) = transcript {
            let evidence = if assistant_message_is_no_reply(&transcript.assistant_message) {
                AgentTurnCompletionEvidence::NoReplyTranscript
            } else {
                AgentTurnCompletionEvidence::AssistantTranscript
            };
            return Ok(Some(AgentTurnCompletion {
                assistant_message: Some(transcript.assistant_message),
                duration_ms: transcript.duration_ms,
                created_at: transcript.created_at,
                evidence,
                terminal_status: None,
            }));
        }

        let terminal = sqlx::query_as::<_, AgentQualityCompletionRow>(
            r#"
            SELECT event_type::text AS event_type,
                   payload #>> '{details,outcome}' AS outcome,
                   CASE
                       WHEN payload #>> '{details,duration_ms}' ~ '^-?[0-9]+$'
                       THEN (payload #>> '{details,duration_ms}')::bigint
                       ELSE NULL
                   END AS duration_ms,
                   created_at
            FROM agent_quality_event
            WHERE correlation_id = $1
              AND source_event_id = $1
              AND created_at >= $2
              AND event_type = 'turn_error'::agent_quality_event_type
              AND payload #>> '{details,outcome}' = 'empty_response'
            ORDER BY created_at ASC, id ASC
            LIMIT 1
            "#,
        )
        .bind(turn_id)
        .bind(run.started_at)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| {
            anyhow!(
                "lookup routine agent terminal turn {} for run {}: {error}",
                turn_id,
                run.run_id
            )
        })?;

        Ok(terminal.and_then(terminal_completion_from_quality_event))
    }

    fn timeout_secs_for_run(&self, run: &RunningAgentRoutineRun) -> u64 {
        timeout_secs_for_run(run, self.default_completion_timeout_secs)
    }

    async fn has_timed_out(&self, run: &RunningAgentRoutineRun, timeout_secs: u64) -> Result<bool> {
        let timeout_secs = i64::try_from(timeout_secs)
            .map_err(|_| anyhow!("routine agent completion timeout exceeds i64 seconds"))?;
        sqlx::query_scalar(
            r#"
            SELECT $1::timestamptz + ($2::bigint * INTERVAL '1 second') <= NOW()
            "#,
        )
        .bind(run.started_at)
        .bind(timeout_secs)
        .fetch_one(&*self.pool)
        .await
        .map_err(|error| anyhow!("check routine agent timeout {}: {error}", run.run_id))
    }

    async fn resolve_or_create_routine_thread(
        &self,
        store: &RoutineStore,
        registry: &HealthRegistry,
        claimed: &ClaimedRoutineRun,
        agent_id: &str,
        provider_name: &str,
        parent_channel_id: poise::serenity_prelude::ChannelId,
    ) -> Result<RoutineThreadTarget> {
        if let Some(thread_id) = claimed
            .discord_thread_id
            .as_deref()
            .and_then(parse_channel_id)
        {
            match validate_routine_thread(registry, provider_name, agent_id, thread_id).await {
                Ok(delivery_bot) => {
                    return Ok(RoutineThreadTarget {
                        channel_id: thread_id,
                        discord_thread_id: Some(thread_id.get().to_string()),
                        delivery_bot,
                    });
                }
                Err(error) => {
                    let warning = format!(
                        "routine saved discord thread reuse failed; creating replacement: {error}"
                    );
                    let _ = store
                        .record_discord_log_result(&claimed.run_id, "failed", Some(&warning))
                        .await;
                    tracing::warn!(
                        routine_id = claimed.routine_id,
                        run_id = claimed.run_id,
                        error = %warning,
                        "routine discord thread reuse failed"
                    );
                }
            }
        } else if claimed.discord_thread_id.as_deref().is_some() {
            let warning = "routine saved discord_thread_id is invalid; creating replacement";
            let _ = store
                .record_discord_log_result(&claimed.run_id, "failed", Some(warning))
                .await;
        }

        let title = routine_thread_title(&claimed.name, agent_id);
        let (thread_id, delivery_bot) =
            create_routine_thread(registry, provider_name, agent_id, parent_channel_id, &title)
                .await
                .map_err(|error| anyhow!("create routine discord thread: {error}"))?;
        let thread_id_string = thread_id.get().to_string();
        if let Err(error) = store
            .update_discord_thread_id(&claimed.routine_id, &thread_id_string)
            .await
        {
            let warning = format!("persist routine discord_thread_id failed: {error}");
            let _ = store
                .record_discord_log_result(&claimed.run_id, "failed", Some(&warning))
                .await;
            tracing::warn!(
                routine_id = claimed.routine_id,
                run_id = claimed.run_id,
                error = %warning,
                "routine discord thread created but persistence failed"
            );
        }
        Ok(RoutineThreadTarget {
            channel_id: thread_id,
            discord_thread_id: Some(thread_id_string),
            delivery_bot,
        })
    }
}

struct StartedAgentTurn {
    result_json: Value,
    started: bool,
    fresh_context_guaranteed: bool,
}

struct RoutineThreadTarget {
    channel_id: poise::serenity_prelude::ChannelId,
    discord_thread_id: Option<String>,
    delivery_bot: String,
}

struct RoutineThreadHttp {
    http: Arc<poise::serenity_prelude::Http>,
    bot: String,
}

fn parse_channel_id(value: &str) -> Option<poise::serenity_prelude::ChannelId> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(poise::serenity_prelude::ChannelId::new)
}

async fn validate_routine_thread(
    registry: &HealthRegistry,
    provider_name: &str,
    agent_id: &str,
    thread_id: poise::serenity_prelude::ChannelId,
) -> Result<String> {
    let resolved = resolve_routine_thread_http(registry, provider_name, agent_id).await?;
    let channel = thread_id
        .to_channel(&*resolved.http)
        .await
        .map_err(|error| anyhow!("fetch saved routine thread {}: {error}", thread_id.get()))?;
    match channel {
        poise::serenity_prelude::Channel::Guild(channel)
            if matches!(
                channel.kind,
                poise::serenity_prelude::ChannelType::PublicThread
                    | poise::serenity_prelude::ChannelType::PrivateThread
            ) =>
        {
            Ok(resolved.bot)
        }
        _ => Err(anyhow!(
            "saved routine discord_thread_id {} is not a thread",
            thread_id.get()
        )),
    }
}

async fn create_routine_thread(
    registry: &HealthRegistry,
    provider_name: &str,
    agent_id: &str,
    parent_channel_id: poise::serenity_prelude::ChannelId,
    title: &str,
) -> Result<(poise::serenity_prelude::ChannelId, String)> {
    let resolved = resolve_routine_thread_http(registry, provider_name, agent_id).await?;
    let thread = parent_channel_id
        .create_thread(
            &*resolved.http,
            poise::serenity_prelude::builder::CreateThread::new(title)
                .kind(poise::serenity_prelude::ChannelType::PublicThread)
                .auto_archive_duration(poise::serenity_prelude::AutoArchiveDuration::OneDay),
        )
        .await
        .map_err(|error| {
            anyhow!(
                "discord create thread in {}: {error}",
                parent_channel_id.get()
            )
        })?;
    Ok((thread.id, resolved.bot))
}

async fn resolve_routine_thread_http(
    registry: &HealthRegistry,
    provider_name: &str,
    agent_id: &str,
) -> Result<RoutineThreadHttp> {
    let mut errors = Vec::new();
    let mut tried = Vec::new();
    for bot in [provider_name, agent_id, "notify"] {
        if bot.trim().is_empty() || tried.contains(&bot) {
            continue;
        }
        tried.push(bot);
        match resolve_bot_http(registry, bot).await {
            Ok(http) => {
                return Ok(RoutineThreadHttp {
                    http,
                    bot: bot.to_string(),
                });
            }
            Err((_, error)) => errors.push(format!("{bot}: {error}")),
        }
    }
    Err(anyhow!(
        "no routine discord bot available ({})",
        errors.join("; ")
    ))
}

fn routine_thread_title(routine_name: &str, agent_id: &str) -> String {
    let base = format!(
        "routine {} - {}",
        compact_for_title(routine_name),
        compact_for_title(agent_id)
    );
    base.chars().take(90).collect()
}

pub(crate) fn routine_agent_session_name(routine_name: &str, agent_id: &str) -> String {
    // Put the distinguishing routine name FIRST: `build_tmux_session_name`
    // truncates to 44 chars, so leading with `agent_id` made two routines on the
    // same agent collide to one tmux session (reusing provider/transcript state)
    // once the shared `routine <agent_id> - ` prefix consumed the budget (#3463).
    // Routine-name-first keeps per-routine sessions distinct after truncation and
    // matches `routine_thread_title`'s ordering.
    let base = format!(
        "routine {} - {}",
        compact_for_title(routine_name),
        compact_for_title(agent_id)
    );
    base.chars().take(90).collect()
}

fn compact_for_title(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        "unnamed".to_string()
    } else {
        value
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn timeout_secs_for_run(run: &RunningAgentRoutineRun, default_completion_timeout_secs: u64) -> u64 {
    run.timeout_secs
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_completion_timeout_secs)
}

async fn fail_claimed_agent_run(
    store: &RoutineStore,
    claimed: ClaimedRoutineRun,
    action: String,
    message: String,
    failed_agent_id: Option<&str>,
    attempt_kind: &str,
    pause_on_terminal_failure: bool,
) -> Result<RoutineRunOutcome> {
    let result_json = Some(json!({
        "status": "failed_to_start",
        "error": message,
        "failed_agent_id": failed_agent_id,
        "attempt_kind": attempt_kind,
        "routine_id": claimed.routine_id,
        "run_id": claimed.run_id,
        "script_ref": claimed.script_ref,
        "fresh_context_guaranteed": fresh_context_guaranteed(&claimed.execution_strategy, false),
    }));
    let fresh_context_guaranteed = fresh_context_guaranteed(&claimed.execution_strategy, false);
    let closed = if terminal_failure_should_pause(pause_on_terminal_failure) {
        store
            .fail_run_and_pause_routine(&claimed.run_id, &message, result_json.clone())
            .await?
    } else {
        store
            .fail_agent_run(&claimed.run_id, &message, result_json.clone(), None)
            .await?
    };
    if !closed {
        return Err(anyhow!(
            "routine agent run {} was already closed before failed outcome",
            claimed.run_id
        ));
    }
    Ok(RoutineRunOutcome {
        run_id: claimed.run_id,
        routine_id: claimed.routine_id,
        script_ref: claimed.script_ref,
        action,
        status: "failed".to_string(),
        result_json,
        error: Some(message),
        fresh_context_guaranteed,
    })
}

fn retry_next_at(retry_count_before_increment: i32) -> DateTime<Utc> {
    let exponent = retry_count_before_increment.max(0) as u32;
    let secs = 60_i64
        .saturating_mul(2_i64.saturating_pow(exponent))
        .min(900);
    Utc::now() + Duration::seconds(secs)
}

fn retry_scheduled_result_for_claimed(
    claimed: &ClaimedRoutineRun,
    failed_agent_id: &str,
    attempt_kind: &str,
    error: &str,
    prompt: &str,
    next_retry_at: DateTime<Utc>,
    retry_count: i32,
    checkpoint: &Option<Value>,
    next_due_at: Option<DateTime<Utc>>,
) -> Value {
    json!({
        "status": "retry_scheduled",
        "error": error,
        "failed_agent_id": failed_agent_id,
        "attempt_kind": attempt_kind,
        "prompt": prompt,
        "retry_count": retry_count,
        "next_retry_at": next_retry_at.to_rfc3339(),
        "routine_id": claimed.routine_id,
        "run_id": claimed.run_id,
        "script_ref": claimed.script_ref,
        "fresh_context_guaranteed": fresh_context_guaranteed(&claimed.execution_strategy, false),
        "checkpoint": checkpoint,
        "next_due_at": next_due_at.map(|value| value.to_rfc3339()),
    })
}

fn completed_result(
    run: &RunningAgentRoutineRun,
    completion: &AgentTurnCompletion,
    assistant_preview: &str,
) -> Value {
    let mut result = json!({
        "status": "completed",
        "turn_id": run.turn_id,
        "routine_id": run.routine_id,
        "run_id": run.run_id,
        "script_ref": run.script_ref,
        "completion_evidence": completion.evidence.as_str(),
        "assistant_message_preview": assistant_preview,
        "assistant_message_chars": completion
            .assistant_message
            .as_ref()
            .map(|message| message.chars().count())
            .unwrap_or(0),
        "duration_ms": completion.duration_ms,
        "completion_created_at": completion.created_at,
        "fresh_context_guaranteed": fresh_context_guaranteed_from_result(run.result_json.as_ref()),
    });
    if let Some(object) = result.as_object_mut() {
        if completion.evidence.is_transcript() {
            object.insert(
                "transcript_created_at".to_string(),
                Value::String(completion.created_at.to_rfc3339()),
            );
        }
        if let Some(status) = completion.terminal_status.as_deref() {
            object.insert(
                "turn_terminal_status".to_string(),
                Value::String(status.to_string()),
            );
        }
    }
    with_started_run_routing_metadata(result, run.result_json.as_ref())
}

fn pending_result_without_fresh_context_guarantee(
    run: &RunningAgentRoutineRun,
    status: &str,
    error: Option<&str>,
) -> Value {
    result_with_fresh_context_guarantee(merge_pending_result(run, status, error, None), false)
}

fn retry_scheduled_result(
    run: &RunningAgentRoutineRun,
    result_json: Option<Value>,
    message: &str,
    retry_count: i32,
    next_retry_at: DateTime<Utc>,
) -> Value {
    let mut retry_result = result_json
        .unwrap_or_else(|| merge_pending_result(run, "retry_scheduled", Some(message), None));
    if let Some(object) = retry_result.as_object_mut() {
        object.insert(
            "status".to_string(),
            Value::String("retry_scheduled".to_string()),
        );
        object.insert(
            "retry_count".to_string(),
            Value::Number(serde_json::Number::from(retry_count)),
        );
        object.insert(
            "next_retry_at".to_string(),
            Value::String(next_retry_at.to_rfc3339()),
        );
    }
    preserve_pending_agent_state(&mut retry_result, run.result_json.as_ref());
    result_with_fresh_context_guarantee(retry_result, false)
}

fn merge_pending_result(
    run: &RunningAgentRoutineRun,
    status: &str,
    error: Option<&str>,
    completion: Option<&AgentTurnCompletion>,
) -> Value {
    with_started_run_routing_metadata(
        json!({
            "status": status,
            "turn_id": run.turn_id,
            "routine_id": run.routine_id,
            "run_id": run.run_id,
            "script_ref": run.script_ref,
            "error": error,
            "duration_ms": completion.and_then(|value| value.duration_ms),
            "fresh_context_guaranteed":
                fresh_context_guaranteed_from_result(run.result_json.as_ref()),
        }),
        run.result_json.as_ref(),
    )
}

fn with_started_run_routing_metadata(mut result: Value, started_result: Option<&Value>) -> Value {
    // #730: also propagate `provider` so a routine headless turn carries the
    // routine agent's resolved provider identity through to the merged result,
    // alongside the `agent_id`/`attempt_kind` routing keys that upstream now
    // produces for the durable fallback-retry path.
    const ROUTING_KEYS: &[&str] = &[
        "channel_id",
        "parent_channel_id",
        "discord_thread_id",
        "agent_id",
        "attempt_kind",
        "provider",
    ];

    let Some(started_result) = started_result else {
        return result;
    };
    let Some(result_object) = result.as_object_mut() else {
        return result;
    };

    for key in ROUTING_KEYS {
        if result_object.contains_key(*key) {
            continue;
        }
        if let Some(value) = started_result.get(*key) {
            result_object.insert((*key).to_string(), value.clone());
        }
    }

    result
}

fn preserve_pending_agent_state(result: &mut Value, previous_result: Option<&Value>) {
    const PENDING_KEYS: &[&str] = &["prompt", "checkpoint", "next_due_at"];

    let Some(previous) = previous_result.and_then(Value::as_object) else {
        return;
    };
    let Some(result_object) = result.as_object_mut() else {
        return;
    };

    for key in PENDING_KEYS {
        if result_object.contains_key(*key) {
            continue;
        }
        if let Some(value) = previous.get(*key) {
            result_object.insert((*key).to_string(), value.clone());
        }
    }
}

fn pending_checkpoint_for_completion(
    result_json: Option<&Value>,
    completion: &AgentTurnCompletion,
) -> Option<Value> {
    let checkpoint = pending_checkpoint(result_json)?;
    if completion.evidence.confirms_assistant_delivery() {
        Some(finalize_family_profile_probe_pending_delivery(checkpoint))
    } else {
        Some(checkpoint)
    }
}

fn confirmed_fresh_context_guarantee(
    provider_fresh_context: bool,
    durable_confirmation_persisted: bool,
) -> bool {
    provider_fresh_context && durable_confirmation_persisted
}

fn result_with_fresh_context_guarantee(mut result_json: Value, guaranteed: bool) -> Value {
    if let Some(object) = result_json.as_object_mut() {
        object.insert(
            "fresh_context_guaranteed".to_string(),
            Value::Bool(guaranteed),
        );
    }
    result_json
}

fn fresh_context_guaranteed_from_result(result_json: Option<&Value>) -> bool {
    result_json
        .and_then(|value| value.get("fresh_context_guaranteed"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn pending_checkpoint(result_json: Option<&Value>) -> Option<Value> {
    result_json
        .and_then(|value| value.get("checkpoint"))
        .filter(|value| !value.is_null())
        .cloned()
}

fn pending_prompt(result_json: Option<&Value>) -> Option<&str> {
    result_json
        .and_then(|value| value.get("prompt"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn current_agent_id_from_result(result_json: Option<&Value>) -> Option<&str> {
    result_json
        .and_then(|value| value.get("agent_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn current_attempt_kind_from_result(result_json: Option<&Value>) -> Option<&str> {
    result_json
        .and_then(|value| value.get("attempt_kind"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn attempts_include_fallback(attempts: &Value) -> bool {
    attempts
        .as_array()
        .map(|items| {
            items.iter().any(|item| {
                item.get("kind").and_then(Value::as_str) == Some("fallback")
                    || item.get("attempt_kind").and_then(Value::as_str) == Some("fallback")
            })
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq)]
enum AgentFailureRecoveryPlan {
    Retry {
        retry_count_after_increment: i32,
        next_retry_at: DateTime<Utc>,
    },
    Fallback {
        agent_id: String,
    },
    Fail,
}

fn claimed_failure_recovery_plan(
    claimed: &ClaimedRoutineRun,
    failed_agent_id: &str,
    attempt_kind: &str,
) -> AgentFailureRecoveryPlan {
    if claimed.max_retries > 0 && attempt_kind != "fallback" {
        return AgentFailureRecoveryPlan::Retry {
            retry_count_after_increment: 1,
            next_retry_at: retry_next_at(0),
        };
    }

    claimed
        .fallback_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| *value != failed_agent_id)
        .filter(|_| attempt_kind != "fallback")
        .map(|agent_id| AgentFailureRecoveryPlan::Fallback {
            agent_id: agent_id.to_string(),
        })
        .unwrap_or(AgentFailureRecoveryPlan::Fail)
}

fn running_failure_recovery_plan(
    run: &RunningAgentRoutineRun,
    failed_agent_id: Option<&str>,
    attempt_kind: &str,
) -> AgentFailureRecoveryPlan {
    if attempt_kind != "fallback" && run.retry_count < run.max_retries {
        return AgentFailureRecoveryPlan::Retry {
            retry_count_after_increment: run.retry_count + 1,
            next_retry_at: retry_next_at(run.retry_count),
        };
    }

    run.fallback_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| Some(*value) != failed_agent_id)
        .filter(|_| attempt_kind != "fallback")
        .filter(|_| !attempts_include_fallback(&run.attempts))
        .map(|agent_id| AgentFailureRecoveryPlan::Fallback {
            agent_id: agent_id.to_string(),
        })
        .unwrap_or(AgentFailureRecoveryPlan::Fail)
}

fn claimed_from_running_run(run: &RunningAgentRoutineRun) -> ClaimedRoutineRun {
    ClaimedRoutineRun {
        run_id: run.run_id.clone(),
        routine_id: run.routine_id.clone(),
        agent_id: run.agent_id.clone(),
        fallback_agent_id: run.fallback_agent_id.clone(),
        max_retries: run.max_retries,
        script_ref: run.script_ref.clone(),
        name: run.name.clone(),
        execution_strategy: run.execution_strategy.clone(),
        checkpoint: pending_checkpoint(run.result_json.as_ref()),
        discord_thread_id: run.discord_thread_id.clone(),
        timeout_secs: run.timeout_secs,
        lease_expires_at: Utc::now(),
    }
}

fn finalize_family_profile_probe_pending_delivery(mut checkpoint: Value) -> Value {
    let Some(object) = checkpoint.as_object_mut() else {
        return checkpoint;
    };
    let Some(pending_delivery) = object.remove("pendingDelivery") else {
        return checkpoint;
    };
    if pending_delivery.get("kind").and_then(Value::as_str) != Some("family-profile-probe") {
        object.insert("pendingDelivery".to_string(), pending_delivery);
        return checkpoint;
    }

    let Some(trigger_date) = pending_delivery
        .get("triggerDate")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        object.insert("pendingDelivery".to_string(), pending_delivery);
        return checkpoint;
    };
    let triggered_at = pending_delivery
        .get("triggeredAt")
        .and_then(Value::as_str)
        .map(str::to_string);

    object.insert(
        "lastTriggeredDate".to_string(),
        Value::String(trigger_date.clone()),
    );
    if let Some(triggered_at) = triggered_at.clone() {
        object.insert("lastTriggeredAt".to_string(), Value::String(triggered_at));
    }

    let mut history = object
        .get("history")
        .and_then(Value::as_array)
        .map(|items| {
            let start = items.len().saturating_sub(199);
            items[start..].to_vec()
        })
        .unwrap_or_default();
    let mut item = Map::new();
    if let Some(target_key) = pending_delivery.get("targetKey").cloned() {
        item.insert("targetKey".to_string(), target_key);
    }
    if let Some(target) = pending_delivery.get("target").cloned() {
        item.insert("target".to_string(), target);
    }
    item.insert("triggerDate".to_string(), Value::String(trigger_date));
    if let Some(triggered_at) = triggered_at {
        item.insert("triggeredAt".to_string(), Value::String(triggered_at));
    }
    if let Some(plan) = pending_delivery.get("plan").cloned() {
        item.insert("plan".to_string(), plan);
    }
    history.push(Value::Object(item));
    object.insert("history".to_string(), Value::Array(history));

    checkpoint
}

fn pending_next_due_at(result_json: Option<&Value>) -> Option<DateTime<Utc>> {
    result_json
        .and_then(|value| value.get("next_due_at"))
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn assistant_preview(message: &str) -> String {
    const MAX_CHARS: usize = 500;
    let trimmed = message.trim();
    let mut preview: String = trimmed.chars().take(MAX_CHARS).collect();
    if trimmed.chars().count() > MAX_CHARS {
        preview.push_str("...");
    }
    preview
}

fn completion_last_result(completion: &AgentTurnCompletion) -> String {
    match completion.evidence {
        AgentTurnCompletionEvidence::AssistantTranscript => {
            assistant_preview(completion.assistant_message.as_deref().unwrap_or_default())
        }
        AgentTurnCompletionEvidence::NoReplyTranscript => "NO_REPLY".to_string(),
        AgentTurnCompletionEvidence::TerminalTurn => {
            let status = completion.terminal_status.as_deref().unwrap_or("terminal");
            format!("agent turn completed without assistant transcript ({status})")
        }
    }
}

fn assistant_message_is_no_reply(message: &str) -> bool {
    message.trim().eq_ignore_ascii_case("NO_REPLY")
}

fn terminal_completion_from_quality_event(
    terminal: AgentQualityCompletionRow,
) -> Option<AgentTurnCompletion> {
    if !is_no_deliverable_quality_event(&terminal.event_type, terminal.outcome.as_deref()) {
        return None;
    }
    Some(AgentTurnCompletion {
        assistant_message: None,
        duration_ms: terminal.duration_ms,
        created_at: terminal.created_at,
        evidence: AgentTurnCompletionEvidence::TerminalTurn,
        terminal_status: terminal
            .outcome
            .filter(|value| !value.trim().is_empty())
            .or(Some(terminal.event_type)),
    })
}

fn is_no_deliverable_quality_event(event_type: &str, outcome: Option<&str>) -> bool {
    event_type == "turn_error" && outcome == Some("empty_response")
}

/// Whether a started fresh-turn `result_json` describes a DM-bound turn (#3022).
/// DM sessions are named `dm-<user_id>` in a DM channel without a
/// `thread_channel_id`, so the thread-based ownership resolver cannot target
/// them; such turns are excluded from boot-recovery ownership recording. Treats
/// either an explicit `is_dm: true` or a non-null `dm_user_id` as a DM turn.
fn started_turn_is_dm(result_json: &Value) -> bool {
    result_json
        .get("is_dm")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || result_json
            .get("dm_user_id")
            .map(|value| !value.is_null())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn running_run(timeout_secs: Option<i32>) -> RunningAgentRoutineRun {
        RunningAgentRoutineRun {
            run_id: "run-1".to_string(),
            routine_id: "routine-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            fallback_agent_id: None,
            max_retries: 0,
            retry_count: 0,
            script_ref: "agent-checkpoint-review.js".to_string(),
            name: "Agent Checkpoint Review".to_string(),
            execution_strategy: "fresh".to_string(),
            discord_thread_id: None,
            turn_id: Some("discord:123:456".to_string()),
            next_retry_at: None,
            attempts: serde_json::json!([]),
            result_json: None,
            started_at: Utc::now(),
            timeout_secs,
        }
    }

    fn running_run_with_result(result_json: Value) -> RunningAgentRoutineRun {
        RunningAgentRoutineRun {
            result_json: Some(result_json),
            ..running_run(None)
        }
    }

    fn claimed_run() -> ClaimedRoutineRun {
        ClaimedRoutineRun {
            run_id: "run-1".to_string(),
            routine_id: "routine-1".to_string(),
            agent_id: Some("codex".to_string()),
            fallback_agent_id: None,
            max_retries: 0,
            script_ref: "agent-checkpoint-review.js".to_string(),
            name: "Agent Checkpoint Review".to_string(),
            execution_strategy: "fresh".to_string(),
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: None,
            lease_expires_at: Utc::now(),
        }
    }

    fn completion_with_evidence(evidence: AgentTurnCompletionEvidence) -> AgentTurnCompletion {
        AgentTurnCompletion {
            assistant_message: match evidence {
                AgentTurnCompletionEvidence::AssistantTranscript => Some("done".to_string()),
                AgentTurnCompletionEvidence::NoReplyTranscript => Some("NO_REPLY".to_string()),
                AgentTurnCompletionEvidence::TerminalTurn => None,
            },
            duration_ms: Some(50),
            created_at: Utc::now(),
            evidence,
            terminal_status: matches!(evidence, AgentTurnCompletionEvidence::TerminalTurn)
                .then(|| "empty_response".to_string()),
        }
    }

    #[test]
    fn retry_next_at_uses_exponential_backoff() {
        let before = Utc::now();
        let first = retry_next_at(0);
        let second = retry_next_at(1);

        assert!(first >= before + chrono::Duration::seconds(59));
        assert!(first <= before + chrono::Duration::seconds(61));
        assert!(second >= before + chrono::Duration::seconds(119));
        assert!(second <= before + chrono::Duration::seconds(121));
        let capped = retry_next_at(5);
        assert!(capped >= before + chrono::Duration::seconds(899));
        assert!(capped <= before + chrono::Duration::seconds(901));
    }

    #[test]
    fn claimed_failure_plan_retries_before_fallback() {
        let claimed = ClaimedRoutineRun {
            fallback_agent_id: Some("claude".to_string()),
            max_retries: 2,
            ..claimed_run()
        };

        match claimed_failure_recovery_plan(&claimed, "codex", "primary") {
            AgentFailureRecoveryPlan::Retry {
                retry_count_after_increment,
                next_retry_at,
            } => {
                assert_eq!(retry_count_after_increment, 1);
                assert!(next_retry_at > Utc::now());
            }
            other => panic!("expected retry before fallback, got {other:?}"),
        }
    }

    #[test]
    fn claimed_failure_plan_falls_back_when_retry_disabled() {
        let claimed = ClaimedRoutineRun {
            fallback_agent_id: Some("claude".to_string()),
            max_retries: 0,
            ..claimed_run()
        };

        assert_eq!(
            claimed_failure_recovery_plan(&claimed, "codex", "primary"),
            AgentFailureRecoveryPlan::Fallback {
                agent_id: "claude".to_string()
            }
        );
        assert_eq!(
            claimed_failure_recovery_plan(&claimed, "claude", "fallback"),
            AgentFailureRecoveryPlan::Fail
        );
    }

    #[test]
    fn running_failure_plan_retries_until_exhausted_then_fallback_once() {
        let mut run = RunningAgentRoutineRun {
            fallback_agent_id: Some("claude".to_string()),
            max_retries: 2,
            retry_count: 1,
            attempts: json!([
                {"event": "started", "kind": "primary", "agent_id": "codex"}
            ]),
            ..running_run_with_result(json!({"prompt": "recover me"}))
        };

        match running_failure_recovery_plan(&run, Some("codex"), "primary") {
            AgentFailureRecoveryPlan::Retry {
                retry_count_after_increment,
                next_retry_at,
            } => {
                assert_eq!(retry_count_after_increment, 2);
                assert!(next_retry_at > Utc::now());
            }
            other => panic!("expected final retry before fallback, got {other:?}"),
        }

        run.retry_count = 2;
        assert_eq!(
            running_failure_recovery_plan(&run, Some("codex"), "retry"),
            AgentFailureRecoveryPlan::Fallback {
                agent_id: "claude".to_string()
            }
        );

        run.attempts = json!([
            {"event": "started", "kind": "primary", "agent_id": "codex"},
            {"event": "started", "kind": "fallback", "agent_id": "claude"}
        ]);
        assert_eq!(
            running_failure_recovery_plan(&run, Some("codex"), "retry"),
            AgentFailureRecoveryPlan::Fail
        );
    }

    #[test]
    fn pending_prompt_trims_and_rejects_empty_values() {
        assert_eq!(
            pending_prompt(Some(&json!({"prompt": "  run me  "}))),
            Some("run me")
        );
        assert_eq!(pending_prompt(Some(&json!({"prompt": "   "}))), None);
        assert_eq!(pending_prompt(Some(&json!({}))), None);
    }

    #[test]
    fn attempts_include_fallback_detects_kind() {
        assert!(!attempts_include_fallback(&json!([
            {"kind": "primary"},
            {"attempt_kind": "retry"}
        ])));
        assert!(attempts_include_fallback(&json!([
            {"kind": "fallback"}
        ])));
    }

    fn quality_completion_row(
        event_type: &str,
        outcome: Option<&str>,
    ) -> AgentQualityCompletionRow {
        AgentQualityCompletionRow {
            event_type: event_type.to_string(),
            outcome: outcome.map(str::to_string),
            duration_ms: Some(50),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn pending_checkpoint_ignores_null() {
        assert_eq!(pending_checkpoint(Some(&json!({"checkpoint": null}))), None);
        assert_eq!(
            pending_checkpoint(Some(&json!({"checkpoint": {"cursor": 3}}))),
            Some(json!({"cursor": 3}))
        );
    }

    #[test]
    fn pending_checkpoint_finalizes_family_profile_probe_after_confirmed_delivery() {
        let completion = completion_with_evidence(AgentTurnCompletionEvidence::AssistantTranscript);
        let result = pending_checkpoint_for_completion(
            Some(&json!({
                "checkpoint": {
                    "plan": {"date": "2026-05-30", "hour": 12, "minute": 0},
                    "history": [{"targetKey": "obujang", "triggerDate": "2026-05-29"}],
                    "pendingDelivery": {
                        "kind": "family-profile-probe",
                        "targetKey": "obujang",
                        "target": "343742347365974026",
                        "triggerDate": "2026-05-30",
                        "triggeredAt": "2026-05-30T12:00:00+09:00",
                        "plan": {"date": "2026-05-30", "hour": 12, "minute": 0}
                    }
                }
            })),
            &completion,
        )
        .expect("checkpoint");

        assert_eq!(
            result.get("lastTriggeredDate").and_then(Value::as_str),
            Some("2026-05-30")
        );
        assert_eq!(
            result.get("lastTriggeredAt").and_then(Value::as_str),
            Some("2026-05-30T12:00:00+09:00")
        );
        assert!(
            result.get("pendingDelivery").is_none(),
            "confirmed delivery must clear the pending marker"
        );
        let history = result
            .get("history")
            .and_then(Value::as_array)
            .expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[1].get("targetKey").and_then(Value::as_str),
            Some("obujang")
        );
    }

    #[test]
    fn fresh_dm_probe_transcript_completion_promotes_pending_delivery_marker() {
        let completion = completion_with_evidence(AgentTurnCompletionEvidence::AssistantTranscript);
        let result = pending_checkpoint_for_completion(
            Some(&json!({
                "status": "started",
                "turn_id": "discord:500:run-family-profile",
                "agent_id": "family-counsel",
                "provider": "codex",
                "channel_id": "500",
                "parent_channel_id": "100",
                "dm_user_id": "343742347365974026",
                "is_dm": true,
                "routine_id": "routine-family-profile",
                "run_id": "run-family-profile",
                "script_ref": "family-profile-probe-obujang.js",
                "execution_strategy": "fresh",
                "completion_evidence": "session_transcripts",
                "fresh_context_guaranteed": true,
                "checkpoint": {
                    "plan": {"date": "2026-05-30", "hour": 12, "minute": 0},
                    "pendingDelivery": {
                        "kind": "family-profile-probe",
                        "targetKey": "obujang",
                        "target": "343742347365974026",
                        "triggerDate": "2026-05-30",
                        "triggeredAt": "2026-05-30T12:00:00+09:00",
                        "plan": {"date": "2026-05-30", "hour": 12, "minute": 0}
                    }
                }
            })),
            &completion,
        )
        .expect("checkpoint");

        assert_eq!(
            result.get("lastTriggeredDate").and_then(Value::as_str),
            Some("2026-05-30")
        );
        assert!(
            result.get("pendingDelivery").is_none(),
            "fresh DM transcript evidence must consume the pending delivery marker"
        );
    }

    #[test]
    fn pending_checkpoint_keeps_family_profile_marker_for_no_reply_completion() {
        for evidence in [
            AgentTurnCompletionEvidence::NoReplyTranscript,
            AgentTurnCompletionEvidence::TerminalTurn,
        ] {
            let completion = completion_with_evidence(evidence);
            let result = pending_checkpoint_for_completion(
                Some(&json!({
                    "checkpoint": {
                        "pendingDelivery": {
                            "kind": "family-profile-probe",
                            "targetKey": "yohoejang",
                            "triggerDate": "2026-05-31"
                        }
                    }
                })),
                &completion,
            )
            .expect("checkpoint");

            assert!(
                result.get("lastTriggeredDate").is_none(),
                "{evidence:?} must not consume today's delivery marker"
            );
            assert_eq!(
                result
                    .pointer("/pendingDelivery/triggerDate")
                    .and_then(Value::as_str),
                Some("2026-05-31"),
                "{evidence:?} must leave pendingDelivery for the next real send"
            );
        }
    }

    #[test]
    fn assistant_preview_caps_long_messages() {
        let long = "a".repeat(600);
        let preview = assistant_preview(&long);
        assert_eq!(preview.chars().count(), 503);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn timeout_secs_prefers_per_routine_value() {
        assert_eq!(timeout_secs_for_run(&running_run(Some(120)), 1800), 120);
        assert_eq!(timeout_secs_for_run(&running_run(None), 1800), 1800);
        assert_eq!(timeout_secs_for_run(&running_run(Some(0)), 1800), 1800);
        assert_eq!(timeout_secs_for_run(&running_run(Some(-5)), 1800), 1800);
    }

    #[test]
    fn fresh_context_guarantee_requires_provider_and_durable_confirmation() {
        assert!(!confirmed_fresh_context_guarantee(false, false));
        assert!(!confirmed_fresh_context_guarantee(true, false));
        assert!(!confirmed_fresh_context_guarantee(false, true));
        assert!(confirmed_fresh_context_guarantee(true, true));
    }

    #[test]
    fn fresh_context_guarantee_is_read_only_from_verified_durable_result() {
        assert!(fresh_context_guaranteed_from_result(Some(&json!({
            "fresh_context_guaranteed": true
        }))));
        assert!(!fresh_context_guaranteed_from_result(Some(&json!({
            "fresh_context_guaranteed": false
        }))));
        assert!(!fresh_context_guaranteed_from_result(Some(&json!({}))));
        assert!(!fresh_context_guaranteed_from_result(Some(&json!({
            "fresh_context_guaranteed": "true"
        }))));
        assert!(!fresh_context_guaranteed_from_result(None));
    }

    #[test]
    fn failed_start_clears_stale_fresh_context_guarantee() {
        let run = running_run_with_result(json!({
            "fresh_context_guaranteed": true
        }));
        let result = pending_result_without_fresh_context_guarantee(
            &run,
            "failed_to_start",
            Some("start failed"),
        );

        assert_eq!(result.get("fresh_context_guaranteed"), Some(&json!(false)));
    }

    #[test]
    fn retry_scheduled_clears_prior_attempt_fresh_context_guarantee() {
        let run = RunningAgentRoutineRun {
            max_retries: 1,
            result_json: Some(json!({
                "prompt": "retry me",
                "fresh_context_guaranteed": true
            })),
            ..running_run(None)
        };

        let result = retry_scheduled_result(
            &run,
            Some(json!({"fresh_context_guaranteed": true})),
            "attempt failed",
            1,
            Utc::now(),
        );

        assert_eq!(result.get("status"), Some(&json!("retry_scheduled")));
        assert_eq!(result.get("fresh_context_guaranteed"), Some(&json!(false)));
        assert_eq!(result.get("prompt"), Some(&json!("retry me")));
    }

    #[test]
    fn completion_preserves_verified_fresh_context_guarantee() {
        let run = running_run_with_result(json!({
            "fresh_context_guaranteed": true
        }));
        let completion = completion_with_evidence(AgentTurnCompletionEvidence::AssistantTranscript);

        assert_eq!(
            completed_result(&run, &completion, "done").get("fresh_context_guaranteed"),
            Some(&json!(true))
        );
        assert_eq!(
            merge_pending_result(&run, "timeout", Some("timed out"), None)
                .get("fresh_context_guaranteed"),
            Some(&json!(true))
        );
    }

    #[test]
    fn completed_result_preserves_started_thread_metadata() {
        let run = running_run_with_result(json!({
            "channel_id": "200",
            "parent_channel_id": "100",
            "discord_thread_id": "200"
        }));
        let completion = completion_with_evidence(AgentTurnCompletionEvidence::AssistantTranscript);

        let result = completed_result(&run, &completion, "done");

        assert_eq!(result.get("channel_id"), Some(&json!("200")));
        assert_eq!(result.get("parent_channel_id"), Some(&json!("100")));
        assert_eq!(result.get("discord_thread_id"), Some(&json!("200")));
        assert_eq!(
            result.get("completion_evidence"),
            Some(&json!("session_transcripts"))
        );
        assert_eq!(result.get("assistant_message_chars"), Some(&json!(4)));
    }

    #[test]
    fn completed_result_records_terminal_completion_without_transcript() {
        let run = running_run_with_result(json!({
            "channel_id": "200",
            "parent_channel_id": "100",
            "discord_thread_id": "200"
        }));
        let completion = completion_with_evidence(AgentTurnCompletionEvidence::TerminalTurn);

        let result = completed_result(
            &run,
            &completion,
            "agent turn completed without assistant transcript (empty_response)",
        );

        assert_eq!(
            result.get("completion_evidence"),
            Some(&json!("agent_quality_event_terminal"))
        );
        assert_eq!(result.get("assistant_message_chars"), Some(&json!(0)));
        assert_eq!(
            result.get("turn_terminal_status"),
            Some(&json!("empty_response"))
        );
        assert!(result.get("transcript_created_at").is_none());
    }

    #[test]
    fn completion_last_result_handles_no_reply() {
        let completion = completion_with_evidence(AgentTurnCompletionEvidence::NoReplyTranscript);

        assert_eq!(completion_last_result(&completion), "NO_REPLY");
    }

    #[test]
    fn normal_turn_complete_quality_event_without_transcript_is_not_no_reply_completion() {
        let completion = terminal_completion_from_quality_event(quality_completion_row(
            "turn_complete",
            Some("completed"),
        ));

        assert!(
            completion.is_none(),
            "normal turn_complete can race transcript insertion and must wait for transcript"
        );
    }

    #[test]
    fn empty_response_quality_event_is_terminal_no_reply_completion() {
        let completion = terminal_completion_from_quality_event(quality_completion_row(
            "turn_error",
            Some("empty_response"),
        ))
        .expect("empty_response should be accepted");

        assert_eq!(
            completion.evidence,
            AgentTurnCompletionEvidence::TerminalTurn
        );
        assert_eq!(
            completion.terminal_status.as_deref(),
            Some("empty_response")
        );
    }

    #[test]
    fn timeout_result_preserves_started_thread_metadata_for_teardown_fallback() {
        let run = running_run_with_result(json!({
            "channel_id": "200",
            "parent_channel_id": "100",
            "discord_thread_id": "200"
        }));

        let result = merge_pending_result(&run, "timeout", Some("timed out"), None);

        assert_eq!(result.get("channel_id"), Some(&json!("200")));
        assert_eq!(result.get("parent_channel_id"), Some(&json!("100")));
        assert_eq!(result.get("discord_thread_id"), Some(&json!("200")));
    }

    #[test]
    fn routine_thread_title_is_compact_and_bounded() {
        let long_name = format!("  Daily\nRoutine\t{}  ", "x".repeat(120));
        let title = routine_thread_title(&long_name, " maker ");

        assert!(title.starts_with("routine Daily Routine "));
        assert!(title.ends_with(" - maker") || title.len() == 90);
        assert!(title.chars().count() <= 90);
        assert!(!title.contains('\n'));
        assert!(!title.contains('\t'));
    }

    #[test]
    fn started_turn_is_dm_detects_dm_routing() {
        // #3022: DM-bound fresh turns must be excluded from ownership recording
        // (their session is not thread-resolvable).
        assert!(started_turn_is_dm(&json!({ "is_dm": true })));
        assert!(started_turn_is_dm(
            &json!({ "dm_user_id": "343742347365974026" })
        ));
        assert!(started_turn_is_dm(
            &json!({ "is_dm": false, "dm_user_id": "1" })
        ));
    }

    #[test]
    fn started_turn_is_dm_false_for_thread_turn() {
        assert!(!started_turn_is_dm(&json!({
            "is_dm": false,
            "dm_user_id": Value::Null,
            "discord_thread_id": "1485506232256168011",
        })));
        assert!(!started_turn_is_dm(&json!({
            "discord_thread_id": "1485506232256168011",
        })));
        assert!(!started_turn_is_dm(&json!({})));
    }
}
