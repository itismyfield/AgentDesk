use std::sync::Arc;

use axum::http::StatusCode;

use crate::db::Db;
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

fn build_slot_clear_target(
    conn: &rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
) -> SlotClearTarget {
    let raw_map: String = conn
        .query_row(
            "SELECT COALESCE(thread_id_map, '{}')
             FROM auto_queue_slots
             WHERE agent_id = ?1 AND slot_index = ?2",
            rusqlite::params![agent_id, slot_index],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "{}".to_string());

    let mut thread_channel_ids: Vec<u64> = serde_json::from_str::<serde_json::Value>(&raw_map)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .map(|map| {
            map.values()
                .filter_map(|value| {
                    value
                        .as_str()
                        .and_then(|raw| raw.trim().parse::<u64>().ok())
                        .or_else(|| value.as_u64())
                })
                .collect()
        })
        .unwrap_or_default();
    thread_channel_ids.sort_unstable();
    thread_channel_ids.dedup();

    let runtime_targets = thread_channel_ids
        .iter()
        .filter_map(|thread_channel_id| {
            let row: Option<(Option<String>, Option<String>)> = conn
                .query_row(
                    "SELECT provider, session_key
                     FROM sessions
                     WHERE thread_channel_id = ?1
                     ORDER BY CASE status WHEN 'working' THEN 0 WHEN 'idle' THEN 1 ELSE 2 END,
                              COALESCE(last_heartbeat, created_at) DESC,
                              rowid DESC
                     LIMIT 1",
                    [thread_channel_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();
            let (provider_name, session_key) = row?;
            let provider_name = provider_name
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
                })?;
            Some(RuntimeSlotClearTarget {
                provider_name,
                thread_channel_id: *thread_channel_id,
                session_key,
            })
        })
        .collect();

    SlotClearTarget {
        thread_channel_ids,
        runtime_targets,
    }
}

pub fn clear_slot_sessions_db(conn: &rusqlite::Connection, thread_channel_ids: &[u64]) -> usize {
    thread_channel_ids
        .iter()
        .map(|thread_channel_id| {
            conn.execute(
                "UPDATE sessions
                 SET status = 'idle',
                     active_dispatch_id = NULL,
                     session_info = 'Slot thread reset',
                     claude_session_id = NULL,
                     tokens = 0,
                     last_heartbeat = datetime('now')
                 WHERE thread_channel_id = ?1
                   AND status IN ('working', 'idle')",
                [thread_channel_id.to_string()],
            )
            .unwrap_or(0)
        })
        .sum()
}

pub fn clear_slot_threads_for_slot(
    health_registry: Option<Arc<HealthRegistry>>,
    conn: &rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
) -> usize {
    let target = build_slot_clear_target(conn, agent_id, slot_index);
    let cleared = clear_slot_sessions_db(conn, &target.thread_channel_ids);

    if let Some(registry) = health_registry {
        let runtime_targets = target.runtime_targets;
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

    cleared
}

pub async fn reset_slot_thread_bindings(
    db: &Db,
    agent_id: &str,
    slot_index: i64,
) -> Result<(usize, usize, usize), String> {
    let conn = db
        .separate_conn()
        .map_err(|err| format!("db open failed for slot reset: {err}"))?;
    if crate::db::auto_queue::slot_has_active_dispatch(&conn, agent_id, slot_index) {
        return Err(format!(
            "slot {slot_index} for agent {agent_id} has active dispatch"
        ));
    }
    let target = build_slot_clear_target(&conn, agent_id, slot_index);
    drop(conn);

    let archived_threads = archive_slot_threads(&target.thread_channel_ids).await?;

    let conn = db
        .separate_conn()
        .map_err(|err| format!("db reopen failed for slot reset: {err}"))?;
    let cleared_sessions = clear_slot_sessions_db(&conn, &target.thread_channel_ids);
    let cleared_bindings = clear_slot_thread_map(&conn, agent_id, slot_index);
    drop(conn);

    Ok((archived_threads, cleared_sessions, cleared_bindings))
}

fn clear_slot_thread_map(conn: &rusqlite::Connection, agent_id: &str, slot_index: i64) -> usize {
    conn.execute(
        "UPDATE auto_queue_slots
         SET thread_id_map = '{}',
             updated_at = datetime('now')
         WHERE agent_id = ?1 AND slot_index = ?2",
        rusqlite::params![agent_id, slot_index],
    )
    .unwrap_or(0)
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
