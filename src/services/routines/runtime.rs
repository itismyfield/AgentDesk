use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::HashSet;

use super::action::RoutineAction;
use super::agent_executor::RoutineAgentExecutor;
use super::discord_log::RoutineDiscordLogger;
use super::loader::{
    MAX_AUTOMATION_INVENTORY_ITEMS, MAX_AUTOMATION_INVENTORY_PAYLOAD_BYTES,
    MAX_OBSERVATION_PAYLOAD_BYTES, MAX_OBSERVATIONS_PER_TICK, ObservationLimits,
    RoutineScriptLoader, RoutineTickAgent, RoutineTickContext, RoutineTickRoutine, RoutineTickRun,
};
use super::store::{ClaimedRoutineRun, RoutineStore};

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
    agent_executor: Option<&RoutineAgentExecutor>,
    discord_logger: Option<&RoutineDiscordLogger>,
    max_due_per_tick: u32,
) -> Result<Vec<RoutineRunOutcome>> {
    let claimed = store.claim_due_runs(max_due_per_tick).await?;
    let mut outcomes = Vec::with_capacity(claimed.len());
    for run in claimed {
        if let Some(logger) = discord_logger {
            logger.log_run_started(store, &run).await;
        }
        match execute_claimed_script_run(store, loader, agent_executor, run).await {
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

pub async fn poll_agent_turns(
    store: &RoutineStore,
    agent_executor: &RoutineAgentExecutor,
    max_per_tick: u32,
) -> Result<Vec<RoutineRunOutcome>> {
    agent_executor.poll_agent_runs(store, max_per_tick).await
}

pub async fn execute_claimed_script_run(
    store: &RoutineStore,
    loader: &RoutineScriptLoader,
    agent_executor: Option<&RoutineAgentExecutor>,
    claimed: ClaimedRoutineRun,
) -> Result<Option<RoutineRunOutcome>> {
    let fresh_context_guaranteed = false;
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
    let automation_inventory = if automation_inventory.is_empty() {
        None
    } else {
        Some(automation_inventory)
    };
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
            let closed = store
                .fail_run(&claimed.run_id, &message, result_json.clone(), None)
                .await?;
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
                fresh_context_guaranteed,
            }));
        }
    };

    close_action(
        store,
        agent_executor,
        claimed,
        action,
        fresh_context_guaranteed,
    )
    .await
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

async fn close_action(
    store: &RoutineStore,
    agent_executor: Option<&RoutineAgentExecutor>,
    claimed: ClaimedRoutineRun,
    action: RoutineAction,
    fresh_context_guaranteed: bool,
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
            Ok(Some(RoutineRunOutcome {
                run_id: claimed.run_id,
                routine_id: claimed.routine_id,
                script_ref: claimed.script_ref,
                action: action_name,
                status: "succeeded".to_string(),
                result_json,
                error: None,
                fresh_context_guaranteed,
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
            Ok(Some(RoutineRunOutcome {
                run_id: claimed.run_id,
                routine_id: claimed.routine_id,
                script_ref: claimed.script_ref,
                action: action_name,
                status: "skipped".to_string(),
                result_json,
                error: None,
                fresh_context_guaranteed,
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
            Ok(Some(RoutineRunOutcome {
                run_id: claimed.run_id,
                routine_id: claimed.routine_id,
                script_ref: claimed.script_ref,
                action: action_name,
                status: "paused".to_string(),
                result_json,
                error: None,
                fresh_context_guaranteed,
            }))
        }
        RoutineAction::Agent {
            prompt,
            checkpoint,
            next_due_at,
        } => {
            let Some(agent_executor) = agent_executor else {
                let message = "RoutineAction.agent requires RoutineAgentExecutor";
                let result_json = Some(json!({
                    "error": message,
                    "fresh_context_guaranteed": fresh_context_guaranteed,
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
                    fresh_context_guaranteed,
                }));
            };
            agent_executor
                .start_agent_run(store, claimed, prompt, checkpoint, next_due_at)
                .await
                .map(Some)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::merge_loaded_script_automation_inventory;

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
}
