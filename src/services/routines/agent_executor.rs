use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;

use crate::services::discord::health::{
    HealthRegistry, reserve_headless_agent_turn, resolve_bot_http,
    start_reserved_headless_agent_turn,
};

use super::runtime::RoutineRunOutcome;
use super::store::{ClaimedRoutineRun, NextDueAtUpdate, RoutineStore, RunningAgentRoutineRun};

const FRESH_CONTEXT_GUARANTEED: bool = false;

#[derive(Clone)]
pub struct RoutineAgentExecutor {
    pool: Arc<PgPool>,
    health_registry: Option<Arc<HealthRegistry>>,
    default_completion_timeout_secs: u64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct AgentTurnCompletion {
    assistant_message: String,
    duration_ms: Option<i64>,
    created_at: DateTime<Utc>,
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
        checkpoint: Option<Value>,
        next_due_at: Option<DateTime<Utc>>,
    ) -> Result<RoutineRunOutcome> {
        let action = "agent".to_string();
        let result = self
            .start_turn(store, &claimed, &prompt, &checkpoint, next_due_at)
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
                fresh_context_guaranteed: FRESH_CONTEXT_GUARANTEED,
            }),
            Ok(started) => {
                let last_result = "headless command consumed without starting an agent turn";
                let closed = store
                    .complete_agent_run(
                        &claimed.run_id,
                        Some(started.result_json.clone()),
                        checkpoint,
                        Some(last_result),
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
                    fresh_context_guaranteed: FRESH_CONTEXT_GUARANTEED,
                })
            }
            Err(error) => {
                let message = error.to_string();
                let result_json = Some(json!({
                    "status": "failed_to_start",
                    "error": message,
                    "routine_id": claimed.routine_id,
                    "run_id": claimed.run_id,
                    "script_ref": claimed.script_ref,
                    "fresh_context_guaranteed": FRESH_CONTEXT_GUARANTEED,
                }));
                let closed = store
                    .fail_agent_run(&claimed.run_id, &message, result_json.clone(), None)
                    .await?;
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
                    fresh_context_guaranteed: FRESH_CONTEXT_GUARANTEED,
                })
            }
        }
    }

    pub async fn poll_agent_runs(
        &self,
        store: &RoutineStore,
        limit: u32,
    ) -> Result<Vec<RoutineRunOutcome>> {
        store.heartbeat_running_agent_runs().await?;
        let pending = store.list_running_agent_runs(limit).await?;
        let mut outcomes = Vec::new();
        for run in pending {
            if let Some(completion) = self.find_turn_completion(&run).await? {
                let checkpoint = pending_checkpoint(run.result_json.as_ref());
                let next_due_at = pending_next_due_at(run.result_json.as_ref());
                let last_result = assistant_preview(&completion.assistant_message);
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
                outcomes.push(RoutineRunOutcome {
                    run_id: run.run_id,
                    routine_id: run.routine_id,
                    script_ref: run.script_ref,
                    action: "agent".to_string(),
                    status: "succeeded".to_string(),
                    result_json,
                    error: None,
                    fresh_context_guaranteed: FRESH_CONTEXT_GUARANTEED,
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
                let closed = store
                    .fail_agent_run(&run.run_id, &message, result_json.clone(), None)
                    .await?;
                if !closed {
                    continue;
                }
                outcomes.push(RoutineRunOutcome {
                    run_id: run.run_id,
                    routine_id: run.routine_id,
                    script_ref: run.script_ref,
                    action: "agent".to_string(),
                    status: "failed".to_string(),
                    result_json,
                    error: Some(message),
                    fresh_context_guaranteed: FRESH_CONTEXT_GUARANTEED,
                });
                continue;
            }
        }
        Ok(outcomes)
    }

    async fn start_turn(
        &self,
        store: &RoutineStore,
        claimed: &ClaimedRoutineRun,
        prompt: &str,
        checkpoint: &Option<Value>,
        next_due_at: Option<DateTime<Utc>>,
    ) -> Result<StartedAgentTurn> {
        let agent_id = claimed
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("routine agent action requires routine.agent_id"))?;
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
            crate::server::routes::dispatches::resolve_channel_alias_pub(&primary_channel)
                .or_else(|| primary_channel.parse::<u64>().ok())
        else {
            return Err(anyhow!(
                "agent {agent_id} primary channel is invalid: {primary_channel}"
            ));
        };
        let channel_id = poise::serenity_prelude::ChannelId::new(channel_id_num);
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
        let reservation = reserve_headless_agent_turn(turn_channel_id);
        let turn_id = reservation.turn_id().to_string();

        let mut result_json = json!({
            "status": "started",
            "turn_id": turn_id.clone(),
            "agent_id": agent_id,
            "provider": provider.as_str(),
            "channel_id": turn_channel_id.get().to_string(),
            "parent_channel_id": channel_id_num.to_string(),
            "discord_thread_id": discord_thread_id,
            "routine_id": claimed.routine_id,
            "run_id": claimed.run_id,
            "script_ref": claimed.script_ref,
            "completion_evidence": "session_transcripts",
            "fresh_context_guaranteed": FRESH_CONTEXT_GUARANTEED,
            "checkpoint": checkpoint,
            "next_due_at": next_due_at.map(|value| value.to_rfc3339()),
        });
        let updated = store
            .mark_agent_turn_started(&claimed.run_id, &turn_id, Some(result_json.clone()))
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
            "fresh_context_guaranteed": FRESH_CONTEXT_GUARANTEED,
            "turn_id": turn_id.clone(),
            "parent_channel_id": channel_id_num.to_string(),
            "discord_thread_id": discord_thread_id,
        }));
        let channel_name_hint = primary_channel
            .chars()
            .all(|ch| ch.is_ascii_digit())
            .then_some(None)
            .unwrap_or_else(|| Some(primary_channel.clone()));
        let outcome = start_reserved_headless_agent_turn(
            registry,
            turn_channel_id,
            provider.clone(),
            prompt.to_string(),
            Some("routine".to_string()),
            metadata,
            channel_name_hint,
            reservation,
        )
        .await
        .map_err(|error| anyhow!("start routine agent turn for {agent_id}: {error}"))?;

        if outcome.turn_id != turn_id {
            return Err(anyhow!(
                "reserved routine agent turn id mismatch: expected {} but started {}",
                turn_id,
                outcome.turn_id
            ));
        }
        if outcome.status.as_str() != "started"
            && let Some(object) = result_json.as_object_mut()
        {
            object.insert(
                "status".to_string(),
                Value::String(outcome.status.as_str().to_string()),
            );
            object.insert(
                "completion_evidence".to_string(),
                Value::String("headless_start_outcome".to_string()),
            );
        }

        Ok(StartedAgentTurn {
            result_json,
            started: outcome.status.as_str() == "started",
        })
    }

    async fn find_turn_completion(
        &self,
        run: &RunningAgentRoutineRun,
    ) -> Result<Option<AgentTurnCompletion>> {
        sqlx::query_as(
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
        .bind(&run.turn_id)
        .bind(run.started_at)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| {
            anyhow!(
                "lookup routine agent transcript {} for run {}: {error}",
                run.turn_id,
                run.run_id
            )
        })
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

fn completed_result(
    run: &RunningAgentRoutineRun,
    completion: &AgentTurnCompletion,
    assistant_preview: &str,
) -> Value {
    json!({
        "status": "completed",
        "turn_id": run.turn_id,
        "routine_id": run.routine_id,
        "run_id": run.run_id,
        "script_ref": run.script_ref,
        "completion_evidence": "session_transcripts",
        "assistant_message_preview": assistant_preview,
        "assistant_message_chars": completion.assistant_message.chars().count(),
        "duration_ms": completion.duration_ms,
        "transcript_created_at": completion.created_at,
        "fresh_context_guaranteed": FRESH_CONTEXT_GUARANTEED,
    })
}

fn merge_pending_result(
    run: &RunningAgentRoutineRun,
    status: &str,
    error: Option<&str>,
    completion: Option<&AgentTurnCompletion>,
) -> Value {
    json!({
        "status": status,
        "turn_id": run.turn_id,
        "routine_id": run.routine_id,
        "run_id": run.run_id,
        "script_ref": run.script_ref,
        "error": error,
        "duration_ms": completion.and_then(|value| value.duration_ms),
        "fresh_context_guaranteed": FRESH_CONTEXT_GUARANTEED,
    })
}

fn pending_checkpoint(result_json: Option<&Value>) -> Option<Value> {
    result_json
        .and_then(|value| value.get("checkpoint"))
        .filter(|value| !value.is_null())
        .cloned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn running_run(timeout_secs: Option<i32>) -> RunningAgentRoutineRun {
        RunningAgentRoutineRun {
            run_id: "run-1".to_string(),
            routine_id: "routine-1".to_string(),
            script_ref: "agent-checkpoint-review.js".to_string(),
            turn_id: "discord:123:456".to_string(),
            result_json: None,
            started_at: Utc::now(),
            timeout_secs,
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
    fn routine_thread_title_is_compact_and_bounded() {
        let long_name = format!("  Daily\nRoutine\t{}  ", "x".repeat(120));
        let title = routine_thread_title(&long_name, " maker ");

        assert!(title.starts_with("routine Daily Routine "));
        assert!(title.ends_with(" - maker") || title.len() == 90);
        assert!(title.chars().count() <= 90);
        assert!(!title.contains('\n'));
        assert!(!title.contains('\t'));
    }
}
