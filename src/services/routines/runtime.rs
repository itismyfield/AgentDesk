use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::{collections::HashSet, path::PathBuf};

use super::action::RoutineAction;
use super::agent_executor::RoutineAgentExecutor;
use super::discord_log::RoutineDiscordLogger;
use super::loader::{
    MAX_AUTOMATION_INVENTORY_ITEMS, MAX_AUTOMATION_INVENTORY_PAYLOAD_BYTES,
    MAX_OBSERVATION_PAYLOAD_BYTES, MAX_OBSERVATIONS_PER_TICK, ObservationLimits,
    RoutineScriptLoader, RoutineTickAgent, RoutineTickContext, RoutineTickRoutine, RoutineTickRun,
};
use super::migrated::{is_migrated_launchd_script_ref, validate_migrated_launchd_activation};
use super::store::{ClaimedRoutineRun, RoutineStore, terminal_failure_should_pause};
use crate::error::{AppError, AppResult, ErrorCode};

#[derive(Debug, Clone, Serialize)]
pub struct RoutineRunOutcome {
    pub run_id: String,
    pub routine_id: String,
    pub script_ref: String,
    pub action: String,
    pub status: String,
    pub result_json: Option<Value>,
    pub error: Option<String>,
    pub fresh_context_guaranteed: bool,
}

pub async fn run_due_tick(
    store: &RoutineStore,
    loader: &RoutineScriptLoader,
    routine_dirs: &[PathBuf],
    agent_executor: Option<&RoutineAgentExecutor>,
    discord_logger: Option<&RoutineDiscordLogger>,
    max_due_per_tick: u32,
    pause_on_terminal_failure: bool,
) -> Result<Vec<RoutineRunOutcome>> {
    let claimed = store.claim_due_runs(max_due_per_tick).await?;
    let mut outcomes = Vec::with_capacity(claimed.len());
    for run in claimed {
        if let Some(logger) = discord_logger {
            logger.log_run_started(store, &run).await;
        }
        if let Err(error) = validate_claimed_migrated_launchd_run(loader, &run, routine_dirs) {
            let message = error.to_string();
            tracing::warn!(
                routine_id = %run.routine_id,
                routine_script = %run.script_ref,
                error = %message,
                "routine due run blocked by migrated launchd validation"
            );
            let result_json = Some(json!({
                "error": message.as_str(),
                "script_ref": run.script_ref.as_str(),
                "blocked_by": "migrated_launchd_validation",
            }));
            let closed = store
                .fail_run_and_pause_as_migration_invalid(&run.run_id, &message, result_json.clone())
                .await?;
            if closed {
                let outcome = RoutineRunOutcome {
                    run_id: run.run_id.clone(),
                    routine_id: run.routine_id.clone(),
                    script_ref: run.script_ref.clone(),
                    action: "error".to_string(),
                    status: "failed".to_string(),
                    result_json,
                    error: Some(message),
                    fresh_context_guaranteed: false,
                };
                if let Some(logger) = discord_logger {
                    logger.log_run_outcome(store, &outcome).await;
                }
                outcomes.push(outcome);
            } else {
                tracing::warn!(
                    routine_run_id = %run.run_id,
                    "routine due run validation failure could not close claimed run"
                );
            }
            continue;
        }
        match execute_claimed_script_run(
            store,
            loader,
            agent_executor,
            discord_logger,
            run,
            pause_on_terminal_failure,
        )
        .await
        {
            Ok(Some(outcome)) => {
                if let Some(logger) = discord_logger {
                    logger.log_run_outcome(store, &outcome).await;
                }
                outcomes.push(outcome);
            }
            Ok(None) => {
                tracing::info!("routine due run was closed before outcome capture");
            }
            Err(error) => {
                tracing::warn!(error = %error, "routine due run failed before outcome capture");
            }
        }
    }
    Ok(outcomes)
}

fn validate_claimed_migrated_launchd_run(
    loader: &RoutineScriptLoader,
    claimed: &ClaimedRoutineRun,
    routine_dirs: &[PathBuf],
) -> AppResult<()> {
    if !is_migrated_launchd_script_ref(&claimed.script_ref) {
        return Ok(());
    }
    let script = loader
        .get_script(&claimed.script_ref)
        .map_err(|error| {
            AppError::internal(format!("routine script lookup failed: {error}"))
                .with_code(ErrorCode::Config)
        })?
        .ok_or_else(|| {
            AppError::conflict(format!(
                "migrated routine {} is invalid: routine script not loaded",
                claimed.script_ref
            ))
        })?;
    validate_migrated_launchd_activation(
        &claimed.script_ref,
        claimed.checkpoint.as_ref(),
        Some(&script.metadata),
        routine_dirs,
    )
}

pub async fn poll_agent_turns(
    store: &RoutineStore,
    agent_executor: &RoutineAgentExecutor,
    max_per_tick: u32,
    pause_on_terminal_failure: bool,
) -> Result<Vec<RoutineRunOutcome>> {
    agent_executor
        .poll_agent_runs(store, max_per_tick, pause_on_terminal_failure)
        .await
}

pub async fn execute_claimed_script_run(
    store: &RoutineStore,
    loader: &RoutineScriptLoader,
    agent_executor: Option<&RoutineAgentExecutor>,
    discord_logger: Option<&RoutineDiscordLogger>,
    claimed: ClaimedRoutineRun,
    pause_on_terminal_failure: bool,
) -> Result<Option<RoutineRunOutcome>> {
    let fresh_context_guaranteed = pre_provider_fresh_context_guaranteed();
    let agent = load_tick_agent_context(store, claimed.agent_id.as_deref()).await?;
    let observations = match store
        .fetch_recent_run_observations(
            Some(&claimed.script_ref),
            MAX_OBSERVATIONS_PER_TICK,
            MAX_OBSERVATION_PAYLOAD_BYTES,
        )
        .await
    {
        Ok(observations) => observations,
        Err(error) => {
            tracing::warn!(
                error = %error,
                routine_script = %claimed.script_ref,
                "routine observation provider failed"
            );
            Vec::new()
        }
    };
    let observation_count = observations.len();
    let observations = if observations.is_empty() {
        None
    } else {
        Some(observations)
    };
    let mut automation_inventory = match store
        .fetch_active_routine_automation_inventory(
            MAX_AUTOMATION_INVENTORY_ITEMS,
            MAX_AUTOMATION_INVENTORY_PAYLOAD_BYTES,
        )
        .await
    {
        Ok(inventory) => inventory,
        Err(error) => {
            tracing::warn!(
                error = %error,
                routine_script = %claimed.script_ref,
                "routine automation inventory provider failed"
            );
            Vec::new()
        }
    };
    match loader.script_refs() {
        Ok(script_refs) => merge_loaded_script_automation_inventory(
            &mut automation_inventory,
            script_refs,
            MAX_AUTOMATION_INVENTORY_ITEMS,
            MAX_AUTOMATION_INVENTORY_PAYLOAD_BYTES,
        ),
        Err(error) => {
            tracing::warn!(
                error = %error,
                routine_script = %claimed.script_ref,
                "loaded routine script inventory unavailable"
            );
        }
    }
    let automation_inventory_count = automation_inventory.len();
    let automation_inventory = if automation_inventory.is_empty() {
        None
    } else {
        Some(automation_inventory)
    };
    if let Some(logger) = discord_logger {
        logger
            .log_run_js_inputs(
                store,
                &claimed,
                observation_count,
                automation_inventory_count,
                claimed.checkpoint.as_ref(),
            )
            .await;
    }
    let context = RoutineTickContext {
        routine: RoutineTickRoutine {
            id: claimed.routine_id.clone(),
            agent_id: claimed.agent_id.clone(),
            script_ref: claimed.script_ref.clone(),
            name: claimed.name.clone(),
            execution_strategy: claimed.execution_strategy.clone(),
            fresh_context_guaranteed,
        },
        run: RoutineTickRun {
            id: claimed.run_id.clone(),
            lease_expires_at: claimed.lease_expires_at,
        },
        agent,
        checkpoint: claimed.checkpoint.clone(),
        now: chrono::Utc::now(),
        observations,
        automation_inventory,
        limits: ObservationLimits::default(),
    };

    store.heartbeat_run(&claimed.run_id).await?;
    let action = match loader.execute_tick(&claimed.script_ref, context) {
        Ok(action) => action,
        Err(error) => {
            let message = error.to_string();
            let result_json = Some(json!({
                "error": message,
                "script_ref": claimed.script_ref,
            }));
            let closed = if terminal_failure_should_pause(pause_on_terminal_failure) {
                store
                    .fail_run_and_pause_routine(&claimed.run_id, &message, result_json.clone())
                    .await?
            } else {
                store
                    .fail_run(&claimed.run_id, &message, result_json.clone(), None)
                    .await?
            };
            if !closed {
                return Ok(None);
            }
            return Ok(Some(RoutineRunOutcome {
                run_id: claimed.run_id,
                routine_id: claimed.routine_id,
                script_ref: claimed.script_ref,
                action: "error".to_string(),
                status: "failed".to_string(),
                result_json,
                error: Some(message),
                fresh_context_guaranteed: pre_provider_fresh_context_guaranteed(),
            }));
        }
    };
    if let Some(logger) = discord_logger {
        logger
            .log_run_js_action(
                store,
                &claimed,
                action.action_name(),
                action_detail(&action).as_deref(),
                action_prompt(&action),
                action_has_checkpoint(&action),
            )
            .await;
    }

    close_action(
        store,
        agent_executor,
        claimed,
        action,
        pause_on_terminal_failure,
    )
    .await
}

fn action_detail(action: &RoutineAction) -> Option<String> {
    match action {
        RoutineAction::Complete {
            result_json,
            last_result,
            ..
        } => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(text) = last_result.as_deref().filter(|s| !s.trim().is_empty()) {
                parts.push(text.to_string());
            }
            if let Some(json) = result_json {
                for key in [
                    "outcome_summary",
                    "decision_summary",
                    "top_evidence_summary",
                    "suppression_summary",
                    "scoring_summary",
                    "candidates_scored",
                    "new_candidates",
                    "recommendations",
                    "suppressed",
                    "candidates",
                    "summary",
                    "status",
                ] {
                    if let Some(v) = json.get(key) {
                        parts.push(format!("{}={}", key, compact_json_val(v)));
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" / "))
            }
        }
        RoutineAction::Skip {
            reason,
            result_json,
            last_result,
            ..
        } => last_result
            .clone()
            .or_else(|| reason.clone())
            .or_else(|| result_json_summary(result_json.as_ref())),
        RoutineAction::Pause {
            reason,
            result_json,
            last_result,
            ..
        } => last_result
            .clone()
            .or_else(|| reason.clone())
            .or_else(|| result_json_summary(result_json.as_ref())),
        RoutineAction::Agent {
            prompt,
            dm_user_id,
            checkpoint,
            ..
        } => {
            let char_count = prompt.chars().count();
            let preview: String = prompt.chars().take(300).collect();
            let suffix = if char_count > 300 { "…" } else { "" };
            let mut parts = Vec::new();
            if let Some(summary) = latest_recommendation_summary(checkpoint.as_ref()) {
                parts.push(summary);
            }
            if let Some(dm_user_id) = dm_user_id.as_deref() {
                parts.push(format!("dm_user_id={dm_user_id}"));
            }
            parts.push(format!(
                "agent prompt generated ({char_count}자): {preview}{suffix}"
            ));
            Some(parts.join(" / "))
        }
    }
}

fn latest_recommendation_summary(checkpoint: Option<&Value>) -> Option<String> {
    let latest = checkpoint?
        .get("recommendations")
        .and_then(Value::as_array)?
        .last()?;
    let mut parts = Vec::new();
    for key in [
        "decision_summary",
        "outcome_summary",
        "top_evidence_summary",
    ] {
        if let Some(text) = latest
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            parts.push(text.to_string());
        }
    }
    if let Some(delta) = latest.get("score_delta_last_tick") {
        if let Some(delta) = delta.as_i64() {
            parts.push(format!("score_delta_last_tick={delta}"));
        } else if let Some(delta) = delta.as_f64() {
            parts.push(format!("score_delta_last_tick={delta:.1}"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" / "))
    }
}

fn compact_json_val(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Array(arr) => format!("[{}개]", arr.len()),
        other => {
            let s = other.to_string();
            if s.len() > 80 {
                format!("{}…", &s[..80])
            } else {
                s
            }
        }
    }
}

fn action_prompt(action: &RoutineAction) -> Option<&str> {
    match action {
        RoutineAction::Agent { prompt, .. } => Some(prompt.as_str()),
        _ => None,
    }
}

fn action_has_checkpoint(action: &RoutineAction) -> bool {
    match action {
        RoutineAction::Complete { checkpoint, .. }
        | RoutineAction::Skip { checkpoint, .. }
        | RoutineAction::Pause { checkpoint, .. }
        | RoutineAction::Agent { checkpoint, .. } => checkpoint.is_some(),
    }
}

fn result_json_summary(result_json: Option<&Value>) -> Option<String> {
    let value = result_json?;
    for key in ["outcome_summary", "summary", "status"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn merge_loaded_script_automation_inventory(
    inventory: &mut Vec<Value>,
    script_refs: Vec<String>,
    max_items: usize,
    max_payload_bytes: usize,
) {
    if max_items == 0 || max_payload_bytes == 0 {
        return;
    }

    let existing_inventory = std::mem::take(inventory);
    let mut merged = Vec::with_capacity(existing_inventory.len().min(max_items));
    let mut seen = HashSet::new();
    let mut total_bytes = 0;
    let updated_at = chrono::Utc::now().to_rfc3339();

    for script_ref in script_refs {
        let script_ref = script_ref.trim();
        if script_ref.is_empty() {
            continue;
        }
        let pattern_id = format!("{script_ref}:*");

        let item = json!({
            "pattern_id": pattern_id,
            "status": "implemented",
            "reason": "loaded routine script",
            "source_ref": format!("routine-script:{script_ref}"),
            "updated_at": updated_at,
        });
        if !push_inventory_item(
            &mut merged,
            &mut seen,
            &mut total_bytes,
            item,
            max_items,
            max_payload_bytes,
        ) {
            break;
        }
    }

    for item in existing_inventory {
        if !push_inventory_item(
            &mut merged,
            &mut seen,
            &mut total_bytes,
            item,
            max_items,
            max_payload_bytes,
        ) {
            break;
        }
    }

    *inventory = merged;
}

fn push_inventory_item(
    inventory: &mut Vec<Value>,
    seen: &mut HashSet<String>,
    total_bytes: &mut usize,
    item: Value,
    max_items: usize,
    max_payload_bytes: usize,
) -> bool {
    if inventory.len() >= max_items {
        return false;
    }
    let pattern_id = item
        .get("pattern_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if pattern_id.is_empty() || !seen.insert(pattern_id) {
        return true;
    }
    let item_bytes = item.to_string().len();
    if *total_bytes + item_bytes > max_payload_bytes {
        return false;
    }
    *total_bytes += item_bytes;
    inventory.push(item);
    true
}

async fn load_tick_agent_context(
    store: &RoutineStore,
    agent_id: Option<&str>,
) -> Result<Option<RoutineTickAgent>> {
    let Some(agent_id) = agent_id.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };

    let row = sqlx::query(
        r#"
        SELECT a.id,
               COALESCE(NULLIF(BTRIM(a.status), ''), 'idle') AS status,
               (SELECT td2.id
                  FROM task_dispatches td2
                  JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                 WHERE td2.to_agent_id = a.id
                   AND kc.status = 'in_progress'
                 ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                 LIMIT 1) AS current_task_id,
               (SELECT s.thread_channel_id
                  FROM sessions s
                 WHERE s.agent_id = a.id
                   AND s.status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working')
                 ORDER BY s.last_heartbeat DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_thread_channel_id,
               EXISTS (
                   SELECT 1
                    FROM sessions s
                   WHERE s.agent_id = a.id
                      AND s.status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working')
               ) AS has_busy_session
          FROM agents a
         WHERE a.id = $1
        "#,
    )
    .bind(agent_id)
    .fetch_optional(store.pool())
    .await
    .map_err(|error| anyhow::anyhow!("load routine tick agent context for {agent_id}: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let status = row
        .try_get::<String, _>("status")
        .unwrap_or_else(|_| "idle".to_string());
    let current_task_id = row
        .try_get::<Option<String>, _>("current_task_id")
        .ok()
        .flatten();
    let current_thread_channel_id = row
        .try_get::<Option<String>, _>("current_thread_channel_id")
        .ok()
        .flatten();
    let has_busy_session = row.try_get::<bool, _>("has_busy_session").unwrap_or(false);
    let has_active_task = current_task_id.is_some();
    let has_busy_signal =
        has_busy_session || current_thread_channel_id.is_some() || has_active_task;
    let is_idle = status == "idle" && !has_busy_signal;

    Ok(Some(RoutineTickAgent {
        id: agent_id.to_string(),
        status,
        is_idle,
        current_task_id,
        current_thread_channel_id,
    }))
}

fn pre_provider_fresh_context_guaranteed() -> bool {
    false
}

async fn close_action(
    store: &RoutineStore,
    agent_executor: Option<&RoutineAgentExecutor>,
    claimed: ClaimedRoutineRun,
    action: RoutineAction,
    pause_on_terminal_failure: bool,
) -> Result<Option<RoutineRunOutcome>> {
    let action_name = action.action_name().to_string();
    match action {
        RoutineAction::Complete {
            result_json,
            checkpoint,
            last_result,
            next_due_at,
        } => {
            let closed = store
                .finish_run(
                    &claimed.run_id,
                    result_json.clone(),
                    checkpoint,
                    last_result.as_deref(),
                    next_due_at,
                )
                .await?;
            if !closed {
                return Ok(None);
            }
            teardown_fresh_session_for_close(
                store,
                agent_executor,
                &claimed,
                "routine fresh run completed",
            )
            .await;
            Ok(Some(RoutineRunOutcome {
                run_id: claimed.run_id,
                routine_id: claimed.routine_id,
                script_ref: claimed.script_ref,
                action: action_name,
                status: "succeeded".to_string(),
                result_json,
                error: None,
                fresh_context_guaranteed: pre_provider_fresh_context_guaranteed(),
            }))
        }
        RoutineAction::Skip {
            reason,
            result_json,
            checkpoint,
            last_result,
            next_due_at,
        } => {
            let result_json =
                result_json.or_else(|| reason.as_ref().map(|reason| json!({ "reason": reason })));
            let closed = store
                .skip_run(
                    &claimed.run_id,
                    result_json.clone(),
                    checkpoint,
                    last_result.as_deref().or(reason.as_deref()),
                    next_due_at,
                )
                .await?;
            if !closed {
                return Ok(None);
            }
            teardown_fresh_session_for_close(
                store,
                agent_executor,
                &claimed,
                "routine fresh run skipped",
            )
            .await;
            Ok(Some(RoutineRunOutcome {
                run_id: claimed.run_id,
                routine_id: claimed.routine_id,
                script_ref: claimed.script_ref,
                action: action_name,
                status: "skipped".to_string(),
                result_json,
                error: None,
                fresh_context_guaranteed: pre_provider_fresh_context_guaranteed(),
            }))
        }
        RoutineAction::Pause {
            reason,
            result_json,
            checkpoint,
            last_result,
        } => {
            let result_json =
                result_json.or_else(|| reason.as_ref().map(|reason| json!({ "reason": reason })));
            let closed = store
                .pause_after_run(
                    &claimed.run_id,
                    result_json.clone(),
                    checkpoint,
                    last_result.as_deref().or(reason.as_deref()),
                )
                .await?;
            if !closed {
                return Ok(None);
            }
            teardown_fresh_session_for_close(
                store,
                agent_executor,
                &claimed,
                "routine fresh run paused",
            )
            .await;
            Ok(Some(RoutineRunOutcome {
                run_id: claimed.run_id,
                routine_id: claimed.routine_id,
                script_ref: claimed.script_ref,
                action: action_name,
                status: "paused".to_string(),
                result_json,
                error: None,
                fresh_context_guaranteed: pre_provider_fresh_context_guaranteed(),
            }))
        }
        RoutineAction::Agent {
            prompt,
            dm_user_id,
            checkpoint,
            last_result,
            next_due_at,
        } => {
            let Some(agent_executor) = agent_executor else {
                let message = "RoutineAction.agent requires RoutineAgentExecutor";
                let result_json = Some(json!({
                    "error": message,
                    "fresh_context_guaranteed": pre_provider_fresh_context_guaranteed(),
                }));
                let closed = store
                    .fail_agent_run(&claimed.run_id, message, result_json.clone(), None)
                    .await?;
                if !closed {
                    return Ok(None);
                }
                return Ok(Some(RoutineRunOutcome {
                    run_id: claimed.run_id,
                    routine_id: claimed.routine_id,
                    script_ref: claimed.script_ref,
                    action: action_name,
                    status: "failed".to_string(),
                    result_json,
                    error: Some(message.to_string()),
                    fresh_context_guaranteed: pre_provider_fresh_context_guaranteed(),
                }));
            };
            agent_executor
                .start_agent_run(
                    store,
                    claimed,
                    prompt,
                    dm_user_id,
                    checkpoint,
                    last_result,
                    next_due_at,
                    pause_on_terminal_failure,
                )
                .await
                .map(Some)
        }
    }
}

/// Tears down the fresh agent session backing a routine run when a terminal
/// JS-script action (`Complete`/`Skip`/`Pause`) closes it. Without this, fresh
/// routines that finish via a script action (rather than an agent turn) leave a
/// stranded session that the reaper later collects and reports as a false
/// "session ended" notification. No-op for non-fresh routines, and when no
/// executor is available (the executor owns the session controller). The
/// underlying teardown re-checks `execution_strategy == "fresh"`, so it is safe
/// against races and runs exactly once per closed run.
///
/// Unlike the agent-turn completion path, the `result_json` here is the
/// arbitrary object returned by the routine script, so it is NOT forwarded as
/// teardown routing metadata: doing so would let untrusted `channel_id` /
/// `discord_thread_id` fields target an unrelated (or the primary) session.
/// Teardown therefore relies solely on the trusted persisted
/// `routine.discord_thread_id`; if that is absent the controller safely refuses
/// to tear down rather than risk killing the wrong session.
///
/// Gating on `execution_strategy == "fresh"` alone is too broad: a fresh
/// routine's `discord_thread_id` is also used as the LOG thread for pure-script
/// routines, and that thread can be shared with an unrelated operator/user
/// session. Resolving "the latest session in the thread" and force-killing it
/// would tear down that unrelated session (#3006). The teardown therefore only
/// runs when *this run* actually started the agent turn whose fresh session it
/// is closing (`run_started_agent_turn`).
///
/// A fresh agent session is created exclusively by the run that calls
/// `mark_agent_turn_started` (which stamps `turn_id` on that run), and that same
/// run is closed — and its session torn down — by the agent-completion path. A
/// terminal JS-script action (`Complete`/`Skip`/`Pause`) is a *different* run
/// that never started a turn (`turn_id` is NULL), so it owns no session and must
/// not kill whatever session is currently latest in the thread. Gating on any
/// prior routine turn is unsafe: a mixed routine that returned `agent` once
/// would then let every later script-only close terminate an unrelated session
/// created after that prior turn. The check is intentionally per-run.
async fn teardown_fresh_session_for_close(
    store: &RoutineStore,
    agent_executor: Option<&RoutineAgentExecutor>,
    claimed: &ClaimedRoutineRun,
    reason: &str,
) {
    if claimed.execution_strategy != "fresh" {
        return;
    }
    match store.run_started_agent_turn(&claimed.run_id).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                routine_id = %claimed.routine_id,
                run_id = %claimed.run_id,
                "fresh routine session teardown skipped: run did not start an agent turn (owns no fresh session)"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(
                routine_id = %claimed.routine_id,
                run_id = %claimed.run_id,
                error = %error,
                "fresh routine session teardown skipped: run agent-turn ownership check failed"
            );
            return;
        }
    }
    let Some(agent_executor) = agent_executor else {
        tracing::warn!(
            routine_id = %claimed.routine_id,
            run_id = %claimed.run_id,
            "fresh routine session teardown skipped: no agent executor available"
        );
        return;
    };
    agent_executor
        .teardown_fresh_agent_session(store, &claimed.routine_id, None, reason)
        .await;
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::{
        action_detail, merge_loaded_script_automation_inventory,
        pre_provider_fresh_context_guaranteed, validate_claimed_migrated_launchd_run,
    };
    use crate::services::routines::{RoutineAction, RoutineScriptLoader, store::ClaimedRoutineRun};

    #[test]
    fn pre_provider_paths_cannot_claim_fresh_context() {
        assert!(!pre_provider_fresh_context_guaranteed());
    }

    #[test]
    fn loaded_script_refs_extend_automation_inventory_as_implemented_prefixes() {
        let mut inventory = vec![json!({
            "pattern_id": "monitoring/existing.js:*",
            "status": "implemented",
        })];

        merge_loaded_script_automation_inventory(
            &mut inventory,
            vec![
                "monitoring/working-watchdog.js".to_string(),
                " monitoring/automation-candidate-recommender.js ".to_string(),
                "monitoring/working-watchdog.js".to_string(),
                String::new(),
            ],
            8,
            4096,
        );

        let pattern_ids = inventory
            .iter()
            .filter_map(|item| item.get("pattern_id").and_then(|value| value.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            pattern_ids,
            vec![
                "monitoring/working-watchdog.js:*",
                "monitoring/automation-candidate-recommender.js:*",
                "monitoring/existing.js:*",
            ]
        );

        let added = inventory
            .iter()
            .find(|item| {
                item.get("pattern_id").and_then(|value| value.as_str())
                    == Some("monitoring/working-watchdog.js:*")
            })
            .expect("loaded routine script inventory item");
        assert_eq!(
            added.get("status").and_then(|value| value.as_str()),
            Some("implemented")
        );
        assert_eq!(
            added.get("source_ref").and_then(|value| value.as_str()),
            Some("routine-script:monitoring/working-watchdog.js")
        );
    }

    #[test]
    fn claimed_migrated_due_run_reuses_activation_validation() {
        let temp = tempfile::tempdir().unwrap();
        let routines_dir = temp.path().join("routines");
        let script_dir = routines_dir.join("migrated-launchd");
        std::fs::create_dir_all(&script_dir).unwrap();
        let script_path = script_dir.join("missing-entrypoint.js");
        std::fs::write(
            &script_path,
            r#"
            agentdesk.routines.register({
              name: "Missing entrypoint",
              metadata: {
                migrated_launchd: {
                  entrypoint: "scripts/launchd-migrated/missing-entrypoint.sh"
                }
              },
              tick(ctx) {
                return { action: "complete", result: { ok: true } };
              }
            });
            "#,
        )
        .unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        let script_ref = loader.load_script(&routines_dir, &script_path).unwrap();
        let claimed = ClaimedRoutineRun {
            run_id: "run-1".to_string(),
            routine_id: "routine-1".to_string(),
            agent_id: None,
            fallback_agent_id: None,
            max_retries: 0,
            script_ref,
            name: "Missing entrypoint".to_string(),
            execution_strategy: "script".to_string(),
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: None,
            lease_expires_at: Utc::now(),
        };

        let error = validate_claimed_migrated_launchd_run(&loader, &claimed, &[routines_dir])
            .expect_err("missing migrated entrypoint must block scheduled execution");

        assert!(error.to_string().contains("shell entrypoint not found"));
    }

    #[test]
    fn loaded_script_inventory_respects_item_cap() {
        let mut inventory = Vec::new();

        merge_loaded_script_automation_inventory(
            &mut inventory,
            vec!["monitoring/a.js".to_string(), "monitoring/b.js".to_string()],
            1,
            4096,
        );

        assert_eq!(inventory.len(), 1);
        assert_eq!(
            inventory[0]
                .get("pattern_id")
                .and_then(|value| value.as_str()),
            Some("monitoring/a.js:*")
        );
    }

    #[test]
    fn loaded_script_inventory_is_prioritized_over_existing_items() {
        let mut inventory = vec![json!({
            "pattern_id": "existing/noisy-pattern",
            "status": "observing",
        })];

        merge_loaded_script_automation_inventory(
            &mut inventory,
            vec!["monitoring/a.js".to_string()],
            1,
            4096,
        );

        assert_eq!(inventory.len(), 1);
        assert_eq!(
            inventory[0]
                .get("pattern_id")
                .and_then(|value| value.as_str()),
            Some("monitoring/a.js:*")
        );
    }

    #[test]
    fn agent_action_detail_includes_checkpoint_decision_summary() {
        let action = RoutineAction::Agent {
            prompt: "# 자동화 후보 추천\n패턴: api-friction".to_string(),
            dm_user_id: None,
            last_result: None,
            checkpoint: Some(json!({
                "recommendations": [{
                    "decision_summary": "선택 이유: api-friction 후보가 기준을 충족했습니다.",
                    "outcome_summary": "실패 요약: API 마찰 패턴이 7회 반복됐습니다.",
                    "top_evidence_summary": "1) /api/docs 우회 반복 (occurrences=7, weight=2)",
                    "score_delta_last_tick": 70
                }]
            })),
            next_due_at: None,
        };

        let detail = action_detail(&action).expect("agent action detail");

        assert!(detail.contains("선택 이유:"));
        assert!(detail.contains("실패 요약:"));
        assert!(detail.contains("/api/docs 우회 반복"));
        assert!(detail.contains("score_delta_last_tick=70"));
    }
}
