use std::sync::Arc;

use axum::http::StatusCode;
use sqlx::{PgPool, Row as SqlxRow};

use crate::services::discord::health::HealthRegistry;

#[derive(Debug, Clone)]
struct RuntimeSlotClearTarget {
    provider_name: String,
    thread_channel_id: u64,
    session_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SlotClearTarget {
    thread_channel_ids: Vec<u64>,
    runtime_targets: Vec<RuntimeSlotClearTarget>,
}

fn parse_slot_thread_channel_ids_from_value(value: &serde_json::Value) -> Vec<u64> {
    let mut thread_channel_ids = value
        .as_object()
        .map(|map| {
            map.values()
                .filter_map(|value| {
                    value
                        .as_str()
                        .and_then(|raw| raw.trim().parse::<u64>().ok())
                        .or_else(|| value.as_u64())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    thread_channel_ids.sort_unstable();
    thread_channel_ids.dedup();
    thread_channel_ids
}

async fn build_slot_clear_target_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<SlotClearTarget, String> {
    let raw_map = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT COALESCE(thread_id_map, '{}'::jsonb)
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot map for {agent_id}:{slot_index}: {error}"))?
    .flatten()
    .unwrap_or_else(|| serde_json::json!({}));

    let thread_channel_ids = parse_slot_thread_channel_ids_from_value(&raw_map);
    let mut runtime_targets = Vec::with_capacity(thread_channel_ids.len());

    for thread_channel_id in &thread_channel_ids {
        let row = sqlx::query(
            "SELECT provider, session_key
             FROM sessions
             WHERE thread_channel_id = $1
             ORDER BY CASE status WHEN 'turn_active' THEN 0 WHEN 'working' THEN 0 WHEN 'awaiting_bg' THEN 1 WHEN 'awaiting_user' THEN 2 WHEN 'idle' THEN 3 ELSE 4 END,
                      COALESCE(last_heartbeat, created_at) DESC,
                      id DESC
             LIMIT 1",
        )
        .bind(thread_channel_id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!(
                "load postgres slot runtime target for {agent_id}:{slot_index}:{thread_channel_id}: {error}"
            )
        })?;
        let Some(row) = row else {
            continue;
        };
        let session_key = row
            .try_get::<Option<String>, _>("session_key")
            .ok()
            .flatten();
        let provider_name = row
            .try_get::<Option<String>, _>("provider")
            .ok()
            .flatten()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                session_key.as_deref().and_then(|key| {
                    key.split_once(':').and_then(|(_, tmux_name)| {
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(
                            tmux_name,
                        )
                        .map(|(provider, _)| provider.as_str().to_string())
                    })
                })
            });
        let Some(provider_name) = provider_name else {
            continue;
        };
        runtime_targets.push(RuntimeSlotClearTarget {
            provider_name,
            thread_channel_id: *thread_channel_id,
            session_key,
        });
    }

    Ok(SlotClearTarget {
        thread_channel_ids,
        runtime_targets,
    })
}

pub async fn clear_slot_sessions_pg(
    pool: &PgPool,
    thread_channel_ids: &[u64],
) -> Result<usize, String> {
    let mut cleared_sessions = 0usize;
    for thread_channel_id in thread_channel_ids {
        let result = sqlx::query(
            "UPDATE sessions
             SET status = 'idle',
                 active_dispatch_id = NULL,
                 session_info = $1,
                 claude_session_id = NULL,
                 tokens = 0,
                 last_heartbeat = NOW()
             WHERE thread_channel_id = $2
               AND status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working', 'idle')",
        )
        .bind("Slot thread reset")
        .bind(thread_channel_id.to_string())
        .execute(pool)
        .await
        .map_err(|error| {
            format!("clear postgres slot sessions for {thread_channel_id}: {error}")
        })?;
        cleared_sessions += result.rows_affected() as usize;
    }
    Ok(cleared_sessions)
}

pub async fn clear_slot_threads_for_slot_pg(
    health_registry: Option<Arc<HealthRegistry>>,
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<usize, String> {
    let target = build_slot_clear_target_pg(pool, agent_id, slot_index).await?;
    let safe_to_clear_thread_ids =
        filter_safe_slot_thread_reset_targets(pool, &target.thread_channel_ids).await?;
    let cleared = clear_slot_sessions_pg(pool, &safe_to_clear_thread_ids).await?;

    if let Some(registry) = health_registry {
        let safe_to_clear: std::collections::HashSet<u64> =
            safe_to_clear_thread_ids.iter().copied().collect();
        let runtime_targets = target
            .runtime_targets
            .into_iter()
            .filter(|target| safe_to_clear.contains(&target.thread_channel_id))
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            for runtime_target in runtime_targets {
                crate::services::discord::health::clear_provider_channel_runtime(
                    &registry,
                    &runtime_target.provider_name,
                    poise::serenity_prelude::ChannelId::new(runtime_target.thread_channel_id),
                    runtime_target.session_key.as_deref(),
                )
                .await;
            }
        });
    }

    Ok(cleared)
}

pub async fn slot_has_active_dispatch_excluding_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    exclude_dispatch_id: Option<&str>,
    exclude_entry_id: Option<&str>,
) -> Result<bool, String> {
    let exclude_id = exclude_dispatch_id.unwrap_or("");
    let exclude_entry_id = exclude_entry_id.unwrap_or("");
    let auto_queue_active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE agent_id = $1
           AND slot_index = $2
           AND status = 'dispatched'
           AND COALESCE(dispatch_id, '') != $3
           AND id != $4",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(exclude_id)
    .bind(exclude_entry_id)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        format!("load postgres active slot entries for {agent_id}:{slot_index}: {error}")
    })?;
    if auto_queue_active > 0 {
        return Ok(true);
    }

    let rows = sqlx::query(
        "SELECT id, context
         FROM task_dispatches
         WHERE to_agent_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!("load postgres active dispatches for {agent_id}:{slot_index}: {error}")
    })?;

    for row in rows {
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            format!("read postgres dispatch id for {agent_id}:{slot_index}: {error}")
        })?;
        if dispatch_id == exclude_id {
            continue;
        }
        let context: Option<String> = row.try_get("context").ok().flatten();
        let Some(context) = context else {
            continue;
        };
        let Some(context_json) = serde_json::from_str::<serde_json::Value>(&context).ok() else {
            continue;
        };
        if context_json
            .get("slot_index")
            .and_then(|value| value.as_i64())
            != Some(slot_index)
        {
            continue;
        }
        if context_json
            .get("sidecar_dispatch")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if context_json.get("phase_gate").is_some() {
            continue;
        }
        return Ok(true);
    }

    Ok(false)
}

pub async fn reset_slot_thread_bindings_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<(usize, usize, usize), String> {
    reset_slot_thread_bindings_excluding_pg(pool, agent_id, slot_index, None, None).await
}

pub async fn reset_slot_thread_bindings_excluding_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    exclude_dispatch_id: Option<&str>,
    exclude_entry_id: Option<&str>,
) -> Result<(usize, usize, usize), String> {
    if slot_has_active_dispatch_excluding_pg(
        pool,
        agent_id,
        slot_index,
        exclude_dispatch_id,
        exclude_entry_id,
    )
    .await?
    {
        return Err(format!(
            "slot {slot_index} for agent {agent_id} has active dispatch"
        ));
    }

    let target = build_slot_clear_target_pg(pool, agent_id, slot_index).await?;
    let safe_to_clear_thread_ids =
        filter_safe_slot_thread_reset_targets(pool, &target.thread_channel_ids).await?;
    let archived_threads = archive_slot_threads(&safe_to_clear_thread_ids).await?;
    let cleared_sessions = clear_slot_sessions_pg(pool, &safe_to_clear_thread_ids).await?;
    let cleared_bindings = if safe_to_clear_thread_ids.len() == target.thread_channel_ids.len() {
        sqlx::query(
            "UPDATE auto_queue_slots
             SET thread_id_map = '{}'::jsonb,
                 updated_at = NOW()
             WHERE agent_id = $1 AND slot_index = $2",
        )
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("clear postgres slot bindings for {agent_id}:{slot_index}: {error}")
        })?
        .rows_affected() as usize
    } else {
        tracing::warn!(
            "[auto-queue] preserving slot thread bindings for {agent_id}:{slot_index}: active thread archive was deferred"
        );
        0
    };

    Ok((archived_threads, cleared_sessions, cleared_bindings))
}

async fn archive_slot_threads(thread_channel_ids: &[u64]) -> Result<usize, String> {
    if thread_channel_ids.is_empty() {
        return Ok(0);
    }

    let token = crate::credential::read_bot_token("announce")
        .ok_or_else(|| "no announce bot token".to_string())?;
    let client = reqwest::Client::new();
    let mut archived = 0usize;

    for thread_channel_id in thread_channel_ids {
        let thread_url = format!("https://discord.com/api/v10/channels/{thread_channel_id}");
        match client
            .patch(&thread_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": true}))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND => {
                archived += 1;
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!(
                    "failed to archive slot thread {thread_channel_id}: {status} {body}"
                ));
            }
            Err(err) => {
                return Err(format!(
                    "failed to archive slot thread {thread_channel_id}: {err}"
                ));
            }
        }
    }

    Ok(archived)
}

async fn filter_safe_slot_thread_reset_targets(
    pool: &PgPool,
    thread_channel_ids: &[u64],
) -> Result<Vec<u64>, String> {
    let mut safe_to_reset = Vec::new();
    for thread_channel_id in thread_channel_ids {
        let thread_id = thread_channel_id.to_string();
        match crate::services::discord::should_defer_thread_archive_pg(Some(pool), &thread_id).await
        {
            Ok(true) => {
                tracing::warn!(
                    "[auto-queue] skipping slot thread reset for {thread_channel_id}: active turn or fresh inflight still present"
                );
            }
            Ok(false) => safe_to_reset.push(*thread_channel_id),
            Err(err) => {
                tracing::warn!(
                    "[auto-queue] skipping slot thread reset for {thread_channel_id}: active-check failed: {err}"
                );
            }
        }
    }
    Ok(safe_to_reset)
}
