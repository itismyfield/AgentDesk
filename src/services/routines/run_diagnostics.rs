//! Run history is recorded independently of Discord delivery and agent availability.
use super::discord_log::{
    RoutineDiscordLogStatus, RoutineDiscordLogger, run_js_action_section, run_js_inputs_section,
    run_outcome_section, run_started_section,
};
use super::runtime::RoutineRunOutcome;
use super::store::{ClaimedRoutineRun, RoutineStore};
use serde_json::Value;

async fn record(
    store: &RoutineStore,
    run_id: &str,
    key: &str,
    text: &str,
) -> RoutineDiscordLogStatus {
    let status = match store
        .record_run_discord_log_section(run_id, key, text)
        .await
    {
        Ok(_) => RoutineDiscordLogStatus {
            status: "stored".into(),
            warning_code: None,
            warning: None,
        },
        Err(error) => {
            tracing::warn!(run_id, key, %error, "routine diagnostic storage failed");
            RoutineDiscordLogStatus::failed("routine_diagnostics_store_failed")
        }
    };
    if let Err(error) = store
        .record_discord_log_result(run_id, &status.status, status.warning.as_deref())
        .await
    {
        tracing::warn!(run_id, %error, "routine diagnostic status storage failed");
        return RoutineDiscordLogStatus::failed("routine_diagnostics_store_failed");
    }
    status
}

pub async fn started(store: &RoutineStore, claimed: &ClaimedRoutineRun) -> RoutineDiscordLogStatus {
    record(
        store,
        &claimed.run_id,
        "started",
        &run_started_section(claimed),
    )
    .await
}

pub async fn inputs(
    store: &RoutineStore,
    claimed: &ClaimedRoutineRun,
    observations: usize,
    inventory: usize,
    checkpoint: Option<&Value>,
) -> RoutineDiscordLogStatus {
    record(
        store,
        &claimed.run_id,
        "js_inputs",
        &run_js_inputs_section(claimed, observations, inventory, checkpoint),
    )
    .await
}

pub async fn action(
    store: &RoutineStore,
    claimed: &ClaimedRoutineRun,
    action: &str,
    summary: Option<&str>,
    prompt: Option<&str>,
    checkpoint_update: bool,
) -> RoutineDiscordLogStatus {
    record(
        store,
        &claimed.run_id,
        "js_action",
        &run_js_action_section(claimed, action, summary, prompt, checkpoint_update),
    )
    .await
}

pub async fn outcome(store: &RoutineStore, outcome: &RoutineRunOutcome) -> RoutineDiscordLogStatus {
    let routine = match store.get_routine(&outcome.routine_id).await {
        Ok(Some(routine)) => routine,
        other => {
            tracing::warn!(run_id = %outcome.run_id, ?other, "routine diagnostic context unavailable");
            let status = RoutineDiscordLogStatus::failed("routine_diagnostics_context_unavailable");
            let _ = store
                .record_discord_log_result(
                    &outcome.run_id,
                    &status.status,
                    status.warning.as_deref(),
                )
                .await;
            return status;
        }
    };
    record(
        store,
        &outcome.run_id,
        "outcome",
        &run_outcome_section(&routine, outcome, None),
    )
    .await
}

pub(super) async fn finish(
    store: &RoutineStore,
    logger: Option<&RoutineDiscordLogger>,
    outcome: &RoutineRunOutcome,
) {
    self::outcome(store, outcome).await;
    if let Some(logger) = logger {
        logger.log_run_outcome(store, outcome).await;
    }
}
