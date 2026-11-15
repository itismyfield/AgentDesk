//! Run history is recorded independently of Discord delivery and agent availability.
pub(super) mod formatting;
use super::runtime::RoutineRunOutcome;
use super::store::{ClaimedRoutineRun, RoutineStore};
use formatting::*;
use serde_json::Value;

async fn record(store: &RoutineStore, run_id: &str, key: &str, text: &str) {
    let (status, warning) = match store
        .record_run_discord_log_section(run_id, key, text)
        .await
    {
        Ok(_) => ("stored", None),
        Err(error) => {
            tracing::warn!(run_id, key, %error, "routine diagnostic storage failed");
            ("failed", Some("routine_diagnostics_store_failed"))
        }
    };
    if let Err(error) = store
        .record_discord_log_result(run_id, status, warning)
        .await
    {
        tracing::warn!(run_id, %error, "routine diagnostic status storage failed");
    }
}

pub async fn started(store: &RoutineStore, claimed: &ClaimedRoutineRun) {
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
) {
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
) {
    record(
        store,
        &claimed.run_id,
        "js_action",
        &run_js_action_section(claimed, action, summary, prompt, checkpoint_update),
    )
    .await
}

pub async fn outcome(store: &RoutineStore, outcome: &RoutineRunOutcome) {
    let routine = match store.get_routine(&outcome.routine_id).await {
        Ok(Some(routine)) => routine,
        other => {
            tracing::warn!(run_id = %outcome.run_id, ?other, "routine diagnostic context unavailable");
            let _ = store
                .record_discord_log_result(
                    &outcome.run_id,
                    "failed",
                    Some("routine_diagnostics_context_unavailable"),
                )
                .await;
            return;
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
