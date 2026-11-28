use chrono::Utc;

use super::RoutineRecord;

pub fn routine_with_thread(
    execution_strategy: &str,
    discord_thread_id: Option<&str>,
) -> RoutineRecord {
    RoutineRecord {
        id: "routine-1".to_string(),
        agent_id: Some("agent-1".to_string()),
        fallback_agent_id: None,
        max_retries: 0,
        script_ref: "script".to_string(),
        name: "Routine".to_string(),
        status: "enabled".to_string(),
        execution_strategy: execution_strategy.to_string(),
        schedule: None,
        next_due_at: None,
        last_run_at: None,
        last_result: None,
        checkpoint: None,
        discord_thread_id: discord_thread_id.map(ToOwned::to_owned),
        timeout_secs: None,
        in_flight_run_id: None,
        pause_reason: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}
