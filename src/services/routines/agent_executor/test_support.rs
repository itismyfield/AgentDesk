use chrono::Utc;

#[cfg(unix)]
use super::RoutineAgentExecutor;
use super::RunningAgentRoutineRun;

pub fn running_run(timeout_secs: Option<i32>) -> RunningAgentRoutineRun {
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

#[cfg(unix)]
pub async fn fresh_provider_session_failure_for_test(
    executor: &RoutineAgentExecutor,
    run: &RunningAgentRoutineRun,
) -> Option<String> {
    executor.fresh_provider_session_failure(run).await
}
