use super::outbox::{format_dispatch_message, prefix_dispatch_message, use_counter_model_channel};
use super::resolve_channel_alias;
use super::thread_reuse::{
    clear_thread_for_channel, get_thread_for_channel, set_thread_for_channel, try_reuse_thread,
};
use crate::db::agents::{
    resolve_agent_channel_for_provider_on_conn, resolve_agent_dispatch_channel_on_conn,
    resolve_agent_primary_channel_on_conn,
};
use crate::db::auto_queue::{ensure_agent_slot_pool_rows, slot_has_active_dispatch};
use crate::services::auto_queue::runtime::reset_slot_thread_bindings;

const SLOT_THREAD_RESET_MESSAGE_LIMIT: u64 = 500;
const SLOT_THREAD_RESET_MAX_AGE_DAYS: i64 = 7;
const SLOT_THREAD_MAX_SLOTS: i64 = 32;
const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

#[derive(Clone, Debug)]
struct SlotThreadBinding {
    agent_id: String,
    slot_index: i64,
    thread_id: Option<String>,
}

fn discord_api_base_url() -> String {
    std::env::var("AGENTDESK_DISCORD_API_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DISCORD_API_BASE.to_string())
}

fn discord_api_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn resolve_dispatch_thread_owner_user_id(db: &crate::db::Db) -> Option<u64> {
    let config = crate::config::load_graceful();
    let conn = db.lock().ok()?;
    crate::server::routes::escalation::effective_owner_user_id(&conn, &config)
}

fn dispatch_context_value(dispatch_context: Option<&str>) -> Option<serde_json::Value> {
    dispatch_context.and_then(|ctx| serde_json::from_str::<serde_json::Value>(ctx).ok())
}

fn context_slot_index(dispatch_context: Option<&serde_json::Value>) -> Option<i64> {
    dispatch_context
        .and_then(|ctx| ctx.get("slot_index"))
        .and_then(|value| value.as_i64())
}

fn thread_id_from_slot_map(thread_id_map: Option<&str>, channel_id: u64) -> Option<String> {
    thread_id_map
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|map| {
            map.get(&channel_id.to_string())
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
}

fn persist_dispatch_slot_index(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
    slot_index: i64,
) -> rusqlite::Result<()> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT context FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    let mut context = existing
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    if context.get("slot_index").and_then(|value| value.as_i64()) == Some(slot_index) {
        return Ok(());
    }
    context["slot_index"] = serde_json::json!(slot_index);
    conn.execute(
        "UPDATE task_dispatches
         SET context = ?1,
             updated_at = datetime('now')
         WHERE id = ?2",
        rusqlite::params![context.to_string(), dispatch_id],
    )?;
    Ok(())
}

fn read_slot_thread_binding(
    conn: &rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
    channel_id: u64,
) -> Option<SlotThreadBinding> {
    ensure_agent_slot_pool_rows(conn, agent_id, slot_index + 1).ok()?;
    let thread_id_map: Option<String> = conn
        .query_row(
            "SELECT thread_id_map
             FROM auto_queue_slots
             WHERE agent_id = ?1 AND slot_index = ?2",
            rusqlite::params![agent_id, slot_index],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    Some(SlotThreadBinding {
        agent_id: agent_id.to_string(),
        slot_index,
        thread_id: thread_id_from_slot_map(thread_id_map.as_deref(), channel_id),
    })
}

fn push_unique_thread_candidate(candidates: &mut Vec<String>, thread_id: Option<&str>) {
    let Some(thread_id) = thread_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    if !candidates.iter().any(|existing| existing == thread_id) {
        candidates.push(thread_id.to_string());
    }
}

fn recent_slot_thread_history(
    conn: &rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
) -> Vec<String> {
    let mut stmt = match conn.prepare(
        "SELECT td.thread_id
         FROM task_dispatches td
         WHERE td.to_agent_id = ?1
           AND td.thread_id IS NOT NULL
           AND TRIM(td.thread_id) != ''
           AND CASE
                 WHEN td.context IS NULL OR TRIM(td.context) = '' OR json_valid(td.context) = 0
                     THEN NULL
                 ELSE CAST(json_extract(td.context, '$.slot_index') AS INTEGER)
               END = ?2
         ORDER BY datetime(COALESCE(td.updated_at, td.created_at)) DESC,
                  td.rowid DESC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };

    let rows = match stmt.query_map(rusqlite::params![agent_id, slot_index], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };

    let mut candidates = Vec::new();
    for row in rows.flatten() {
        push_unique_thread_candidate(&mut candidates, Some(row.as_str()));
    }
    candidates
}

fn collect_slot_thread_candidates(
    conn: &rusqlite::Connection,
    agent_id: &str,
    card_id: &str,
    slot_binding: Option<&SlotThreadBinding>,
    channel_id: u64,
) -> Vec<String> {
    let mut candidates = Vec::new();
    push_unique_thread_candidate(
        &mut candidates,
        slot_binding.and_then(|binding| binding.thread_id.as_deref()),
    );
    push_unique_thread_candidate(
        &mut candidates,
        get_thread_for_channel(conn, card_id, channel_id).as_deref(),
    );
    if let Some(binding) = slot_binding {
        for thread_id in recent_slot_thread_history(conn, agent_id, binding.slot_index) {
            push_unique_thread_candidate(&mut candidates, Some(thread_id.as_str()));
        }
    }
    candidates
}

fn allocate_manual_slot_binding(
    conn: &rusqlite::Connection,
    agent_id: &str,
    dispatch_id: &str,
    channel_id: u64,
) -> Option<SlotThreadBinding> {
    for slot_index in 0..SLOT_THREAD_MAX_SLOTS {
        ensure_agent_slot_pool_rows(conn, agent_id, slot_index + 1).ok()?;
        if slot_has_active_dispatch(conn, agent_id, slot_index) {
            continue;
        }
        persist_dispatch_slot_index(conn, dispatch_id, slot_index).ok()?;
        return read_slot_thread_binding(conn, agent_id, slot_index, channel_id);
    }
    None
}

fn resolve_slot_thread_binding_on_conn(
    conn: &rusqlite::Connection,
    agent_id: &str,
    card_id: &str,
    dispatch_id: &str,
    dispatch_context: Option<&serde_json::Value>,
    channel_id: u64,
) -> Option<SlotThreadBinding> {
    if let Some(slot_index) = context_slot_index(dispatch_context) {
        return read_slot_thread_binding(conn, agent_id, slot_index, channel_id);
    }

    let auto_queue_slot: Option<i64> = conn
        .query_row(
            "SELECT slot_index
             FROM auto_queue_entries
             WHERE dispatch_id = ?1
               AND agent_id = ?2
               AND slot_index IS NOT NULL",
            rusqlite::params![dispatch_id, agent_id],
            |row| row.get(0),
        )
        .ok()
        .or_else(|| {
            conn.query_row(
                "SELECT slot_index
                 FROM auto_queue_entries
                 WHERE kanban_card_id = ?1
                   AND agent_id = ?2
                   AND status IN ('pending', 'dispatched')
                   AND slot_index IS NOT NULL
                 ORDER BY CASE status WHEN 'dispatched' THEN 0 ELSE 1 END,
                          priority_rank ASC
                 LIMIT 1",
                rusqlite::params![card_id, agent_id],
                |row| row.get(0),
            )
            .ok()
        });
    if let Some(slot_index) = auto_queue_slot {
        let binding = read_slot_thread_binding(conn, agent_id, slot_index, channel_id)?;
        persist_dispatch_slot_index(conn, dispatch_id, slot_index).ok();
        return Some(binding);
    }

    allocate_manual_slot_binding(conn, agent_id, dispatch_id, channel_id)
}

fn upsert_slot_thread_id(
    conn: &rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
    channel_id: u64,
    thread_id: &str,
) {
    let existing: String = conn
        .query_row(
            "SELECT COALESCE(thread_id_map, '{}')
             FROM auto_queue_slots
             WHERE agent_id = ?1 AND slot_index = ?2",
            rusqlite::params![agent_id, slot_index],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "{}".to_string());
    let mut map: serde_json::Value = serde_json::from_str::<serde_json::Value>(&existing)
        .ok()
        .filter(|value| value.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    map[channel_id.to_string()] = serde_json::json!(thread_id);
    conn.execute(
        "UPDATE auto_queue_slots
         SET thread_id_map = ?1,
             updated_at = datetime('now')
         WHERE agent_id = ?2 AND slot_index = ?3",
        rusqlite::params![map.to_string(), agent_id, slot_index],
    )
    .ok();
}

fn clear_slot_thread_id(
    conn: &rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
    channel_id: u64,
) {
    let existing: String = conn
        .query_row(
            "SELECT COALESCE(thread_id_map, '{}')
             FROM auto_queue_slots
             WHERE agent_id = ?1 AND slot_index = ?2",
            rusqlite::params![agent_id, slot_index],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "{}".to_string());
    if let Ok(mut map) = serde_json::from_str::<serde_json::Value>(&existing) {
        if let Some(obj) = map.as_object_mut() {
            obj.remove(&channel_id.to_string());
            conn.execute(
                "UPDATE auto_queue_slots
                 SET thread_id_map = ?1,
                     updated_at = datetime('now')
                 WHERE agent_id = ?2 AND slot_index = ?3",
                rusqlite::params![map.to_string(), agent_id, slot_index],
            )
            .ok();
        }
    }
}

fn discord_thread_created_at(
    thread_id: &str,
    thread_info: &serde_json::Value,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(timestamp) = thread_info
        .get("thread_metadata")
        .and_then(|metadata| metadata.get("create_timestamp"))
        .and_then(|value| value.as_str())
    {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) {
            return Some(parsed.with_timezone(&chrono::Utc));
        }
    }

    let raw_id = thread_id.parse::<u64>().ok()?;
    let timestamp_ms = (raw_id >> 22) + 1_420_070_400_000;
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp_ms as i64)
}

async fn reset_stale_slot_thread_if_needed(
    db: &crate::db::Db,
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    dispatch_id: &str,
    slot_binding: &SlotThreadBinding,
) -> Result<bool, String> {
    let Some(thread_id) = slot_binding.thread_id.as_deref() else {
        return Ok(false);
    };

    let thread_info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
    let response = client
        .get(&thread_info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to inspect slot thread {thread_id}: {err}"))?;

    if !response.status().is_success() {
        return Ok(false);
    }

    let thread_info = response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("failed to parse slot thread {thread_id}: {err}"))?;
    let total_message_sent = thread_info
        .get("total_message_sent")
        .and_then(|value| value.as_u64())
        .or_else(|| {
            thread_info
                .get("message_count")
                .and_then(|value| value.as_u64())
        })
        .unwrap_or(0);
    let message_limit_hit = total_message_sent > SLOT_THREAD_RESET_MESSAGE_LIMIT;
    let age_limit_hit = discord_thread_created_at(thread_id, &thread_info)
        .map(|created_at| {
            chrono::Utc::now().signed_duration_since(created_at)
                > chrono::Duration::days(SLOT_THREAD_RESET_MAX_AGE_DAYS)
        })
        .unwrap_or(false);

    if !message_limit_hit && !age_limit_hit {
        return Ok(false);
    }

    tracing::info!(
        "[dispatch] resetting stale slot thread before dispatch {}: agent={} slot={} messages={} age_limit_hit={}",
        dispatch_id,
        slot_binding.agent_id,
        slot_binding.slot_index,
        total_message_sent,
        age_limit_hit,
    );
    reset_slot_thread_bindings(db, &slot_binding.agent_id, slot_binding.slot_index).await?;
    Ok(true)
}

async fn archive_duplicate_slot_threads(
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    expected_parent: u64,
    keep_thread_id: &str,
    candidate_thread_ids: &[String],
) {
    for thread_id in candidate_thread_ids {
        if thread_id == keep_thread_id {
            continue;
        }

        let thread_info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
        let response = match client
            .get(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to inspect duplicate slot thread {thread_id}: {err}"
                );
                continue;
            }
        };

        if !response.status().is_success() {
            continue;
        }

        let thread_info = match response.json::<serde_json::Value>().await {
            Ok(thread_info) => thread_info,
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to parse duplicate slot thread {thread_id}: {err}"
                );
                continue;
            }
        };

        let parent_id = thread_info
            .get("parent_id")
            .and_then(|value| value.as_str())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        if parent_id != expected_parent {
            continue;
        }

        let already_archived = thread_info
            .get("thread_metadata")
            .and_then(|metadata| metadata.get("archived"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if already_archived {
            continue;
        }

        match client
            .patch(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": true}))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("[dispatch] Archived duplicate slot thread {thread_id}");
            }
            Ok(resp) => {
                tracing::warn!(
                    "[dispatch] Failed to archive duplicate slot thread {thread_id}: {}",
                    resp.status()
                );
            }
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to archive duplicate slot thread {thread_id}: {err}"
                );
            }
        }
    }
}

fn build_slot_thread_name(
    db: &crate::db::Db,
    dispatch_id: &str,
    card_id: &str,
    slot_index: i64,
    issue_number: Option<i64>,
    title: &str,
) -> String {
    let mut batch_phase_for_label: i64 = 0;
    let grouped_issue_label: Option<String> = db.lock().ok().and_then(|conn| {
        let group_info: Option<(String, i64, i64)> = conn
            .query_row(
                "SELECT run_id, COALESCE(thread_group, 0), COALESCE(batch_phase, 0)
                 FROM auto_queue_entries
                 WHERE dispatch_id = ?1",
                [dispatch_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()
            .or_else(|| {
                conn.query_row(
                    "SELECT run_id, COALESCE(thread_group, 0), COALESCE(batch_phase, 0)
                     FROM auto_queue_entries
                     WHERE kanban_card_id = ?1
                       AND status IN ('pending', 'dispatched')
                     ORDER BY CASE status WHEN 'dispatched' THEN 0 ELSE 1 END,
                              priority_rank ASC
                     LIMIT 1",
                    [card_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .ok()
            });
        let (run_id, thread_group, batch_phase) = group_info?;
        batch_phase_for_label = batch_phase;
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, e.kanban_card_id
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = ?1
                   AND COALESCE(e.thread_group, 0) = ?2
                   AND COALESCE(e.batch_phase, 0) = (
                       SELECT COALESCE(e2.batch_phase, 0)
                       FROM auto_queue_entries e2
                       WHERE e2.kanban_card_id = ?3
                         AND e2.run_id = ?1
                       LIMIT 1
                   )
                   AND kc.github_issue_number IS NOT NULL
                 ORDER BY e.priority_rank ASC",
            )
            .ok()?;
        let issues: Vec<(i64, String)> = stmt
            .query_map(rusqlite::params![run_id, thread_group, card_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .ok()?
            .filter_map(|row| row.ok())
            .collect();
        if issues.len() <= 1 {
            return None;
        }
        Some(
            issues
                .into_iter()
                .map(|(issue_number, issue_card_id)| {
                    if issue_card_id == card_id {
                        format!("▸{}", issue_number)
                    } else {
                        format!("#{}", issue_number)
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        )
    });

    let base = if let Some(grouped) = grouped_issue_label {
        grouped
    } else if let Some(number) = issue_number {
        let short_title: String = title.chars().take(80).collect();
        format!("#{} {}", number, short_title)
    } else {
        title.chars().take(90).collect()
    };
    let phase_prefix = if batch_phase_for_label > 0 {
        format!("P{} ", batch_phase_for_label)
    } else {
        String::new()
    };
    format!("[slot {}] {}{}", slot_index, phase_prefix, base)
        .chars()
        .take(100)
        .collect()
}

fn review_source_provider_from_context(dispatch_context: Option<&str>) -> Option<String> {
    dispatch_context_value(dispatch_context).and_then(|ctx| {
        ctx.get("from_provider")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
    })
}

fn latest_completed_review_provider_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> Option<String> {
    let review_context: Option<String> = conn
        .query_row(
            "SELECT context FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review' AND status = 'completed' \
             ORDER BY COALESCE(completed_at, updated_at) DESC, updated_at DESC, rowid DESC LIMIT 1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    review_source_provider_from_context(review_context.as_deref())
}

fn latest_work_dispatch_thread_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT COALESCE(td.thread_id, json_extract(td.context, '$.thread_id'))
         FROM task_dispatches td
         WHERE td.kanban_card_id = ?1
           AND td.dispatch_type IN ('implementation', 'rework')
           AND TRIM(COALESCE(td.thread_id, json_extract(td.context, '$.thread_id'), '')) != ''
         ORDER BY
           CASE td.status
             WHEN 'dispatched' THEN 0
             WHEN 'pending' THEN 1
             WHEN 'completed' THEN 2
             ELSE 3
           END,
           datetime(COALESCE(td.completed_at, td.updated_at, td.created_at)) DESC,
           td.rowid DESC
         LIMIT 1",
        [card_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
}

fn resolve_agent_channel_with_provider_override_on_conn(
    conn: &rusqlite::Connection,
    agent_id: &str,
    dispatch_type: Option<&str>,
    provider_override: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    if let Some(provider) = provider_override.filter(|provider| !provider.trim().is_empty()) {
        if let Some(channel) =
            resolve_agent_channel_for_provider_on_conn(conn, agent_id, Some(provider))?
        {
            return Ok(Some(channel));
        }
    }
    resolve_agent_dispatch_channel_on_conn(conn, agent_id, dispatch_type)
}

pub(super) fn resolve_dispatch_delivery_channel_on_conn(
    conn: &rusqlite::Connection,
    agent_id: &str,
    card_id: &str,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    let provider_override = if dispatch_type == Some("review-decision") {
        review_source_provider_from_context(dispatch_context)
            .or_else(|| latest_completed_review_provider_on_conn(conn, card_id))
    } else {
        None
    };
    resolve_agent_channel_with_provider_override_on_conn(
        conn,
        agent_id,
        dispatch_type,
        provider_override.as_deref(),
    )
}

fn resolve_review_followup_channel_on_conn(
    conn: &rusqlite::Connection,
    agent_id: &str,
) -> rusqlite::Result<Option<String>> {
    resolve_agent_primary_channel_on_conn(conn, agent_id)
}

async fn add_thread_member_to_dispatch_thread(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    thread_id: &str,
    user_id: u64,
) -> Result<(), String> {
    let thread_info_url = discord_api_url(base_url, &format!("/channels/{thread_id}"));
    let response = client
        .get(&thread_info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to inspect thread {thread_id}: {err}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "failed to inspect thread {thread_id}: {status} {body}"
        ));
    }

    let thread_info = response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("failed to parse thread {thread_id}: {err}"))?;
    let is_archived = thread_info
        .get("thread_metadata")
        .and_then(|metadata| metadata.get("archived"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    if is_archived {
        let response = client
            .patch(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": false}))
            .send()
            .await
            .map_err(|err| format!("failed to unarchive thread {thread_id}: {err}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "failed to unarchive thread {thread_id}: {status} {body}"
            ));
        }
    }

    let member_url = discord_api_url(
        base_url,
        &format!("/channels/{thread_id}/thread-members/{user_id}"),
    );
    let response = client
        .put(&member_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to add user {user_id} to thread {thread_id}: {err}"))?;

    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "failed to add user {user_id} to thread {thread_id}: {status} {body}"
        ))
    }
}

async fn maybe_add_owner_to_dispatch_thread(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    thread_id: &str,
    dispatch_id: &str,
    owner_user_id: Option<u64>,
) {
    let Some(owner_user_id) = owner_user_id else {
        return;
    };

    if let Err(err) =
        add_thread_member_to_dispatch_thread(client, token, base_url, thread_id, owner_user_id)
            .await
    {
        tracing::warn!(
            "[dispatch] Failed to add owner {} to thread {} for dispatch {}: {}",
            owner_user_id,
            thread_id,
            dispatch_id,
            err
        );
    }
}

/// Send a dispatch notification to the target agent's Discord channel.
/// Message format: `DISPATCH:<dispatch_id> - <title>\n<issue_url>`
/// The `DISPATCH:<uuid>` prefix is required for the dcserver to link the
/// resulting Claude session back to the kanban card (via `parse_dispatch_id`).
pub(crate) async fn send_dispatch_to_discord(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
) -> Result<(), String> {
    // Two-phase delivery guard (prevents duplicates across all callers):
    // 1. Check dispatch_notified (confirmed prior delivery) → skip if present
    // 2. Claim dispatch_reserving (atomic lock) → skip if another path holds it
    // 3. Send to Discord
    // 4. On success: release reserving, commit notified
    // 5. On failure: release reserving, return Err
    // Boot recovery clears stale reserving markers on startup.
    {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for delivery guard".into()),
        };
        // Already confirmed delivered?
        let notified = conn
            .query_row(
                "SELECT 1 FROM kv_meta WHERE key = ?1",
                [&format!("dispatch_notified:{dispatch_id}")],
                |_| Ok(()),
            )
            .is_ok();
        if notified {
            return Ok(()); // Confirmed prior delivery — idempotent skip
        }
        // Atomic reservation claim
        let claimed = conn
            .execute(
                "INSERT OR IGNORE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![format!("dispatch_reserving:{dispatch_id}"), dispatch_id],
            )
            .unwrap_or(0)
            > 0;
        if !claimed {
            return Ok(()); // Another path is actively delivering — skip
        }
    }

    // Wrap the actual send so we can always release the reservation
    let send_result =
        send_dispatch_to_discord_inner(db, agent_id, title, card_id, dispatch_id).await;

    // Release reservation and commit notified marker on success
    if let Ok(conn) = db.lock() {
        conn.execute(
            "DELETE FROM kv_meta WHERE key = ?1",
            [&format!("dispatch_reserving:{dispatch_id}")],
        )
        .ok();
        if send_result.is_ok() {
            conn.execute(
                "INSERT OR IGNORE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![format!("dispatch_notified:{dispatch_id}"), dispatch_id],
            )
            .ok();
        }
    }

    send_result
}

/// Inner function: performs the actual Discord send without reservation logic.
async fn send_dispatch_to_discord_inner(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
) -> Result<(), String> {
    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => {
            tracing::warn!(
                "[dispatch] No announce bot token (missing credential/announce_bot_token)"
            );
            return Err("no announce bot token".into());
        }
    };
    let discord_api_base = discord_api_base_url();
    let thread_owner_user_id = resolve_dispatch_thread_owner_user_id(db);
    send_dispatch_to_discord_inner_with_context(
        db,
        agent_id,
        title,
        card_id,
        dispatch_id,
        &token,
        &discord_api_base,
        thread_owner_user_id,
    )
    .await
}

async fn send_dispatch_to_discord_inner_with_context(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
    token: &str,
    discord_api_base: &str,
    thread_owner_user_id: Option<u64>,
) -> Result<(), String> {
    // Determine dispatch type + status before attempting Discord delivery.
    let (dispatch_type, dispatch_status, dispatch_context): (
        Option<String>,
        Option<String>,
        Option<String>,
    ) = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for dispatch metadata query".into()),
        };
        conn.query_row(
            "SELECT dispatch_type, status, context FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| format!("dispatch {dispatch_id} not found"))?
    };

    if !matches!(
        dispatch_status.as_deref(),
        Some("pending") | Some("dispatched")
    ) {
        tracing::info!(
            "[dispatch] Skipping Discord send for dispatch {} with non-deliverable status {:?}",
            dispatch_id,
            dispatch_status
        );
        return Ok(());
    }

    // For review dispatches, use the alternate channel (counter-model)
    let use_alt = use_counter_model_channel(dispatch_type.as_deref());

    // Look up agent's discord channel
    let channel_id: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for channel lookup".into()),
        };
        resolve_dispatch_delivery_channel_on_conn(
            &conn,
            agent_id,
            card_id,
            dispatch_type.as_deref(),
            dispatch_context.as_deref(),
        )
        .ok()
        .flatten()
    };

    let channel_id = match channel_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            tracing::warn!(
                "[dispatch] No discord_channel_id for agent {agent_id}, skipping message"
            );
            return Err(format!("no discord channel for agent {agent_id}"));
        }
    };

    // Parse channel ID as u64, or resolve alias via role_map.json
    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => {
            // Try resolving channel name alias from role_map.json
            match resolve_channel_alias(&channel_id) {
                Some(n) => n,
                None => {
                    tracing::warn!(
                        "[dispatch] Cannot resolve channel '{channel_id}' for agent {agent_id}"
                    );
                    return Err(format!(
                        "cannot resolve channel '{channel_id}' for agent {agent_id}"
                    ));
                }
            }
        }
    };

    // Look up the issue URL and number for context
    let (issue_url, issue_number): (Option<String>, Option<i64>) = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for issue lookup".into()),
        };
        conn.query_row(
            "SELECT github_issue_url, github_issue_number FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or_default()
    };

    let dispatch_context_json = dispatch_context_value(dispatch_context.as_deref());

    // For review dispatches, look up reviewed commit SHA, branch, and target provider from context
    let (reviewed_commit, target_provider, review_branch): (
        Option<String>,
        Option<String>,
        Option<String>,
    ) = if use_alt {
        let ctx_val = dispatch_context_json
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        (
            ctx_val
                .get("reviewed_commit")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ctx_val
                .get("target_provider")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ctx_val
                .get("branch")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        )
    } else {
        (None, None, None)
    };

    let message = format_dispatch_message(
        dispatch_id,
        title,
        issue_url.as_deref(),
        issue_number,
        use_alt,
        reviewed_commit.as_deref(),
        target_provider.as_deref(),
        review_branch.as_deref(),
        dispatch_type.as_deref(),
        dispatch_context.as_deref(),
    );

    // ── Thread reuse: every dispatch now resolves into a slot thread ──
    let client = reqwest::Client::new();
    let dispatch_type_label = dispatch_type.as_deref().unwrap_or("implementation");
    let message = prefix_dispatch_message(dispatch_type_label, &message);
    let mut slot_binding = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for slot binding lookup".into()),
        };
        resolve_slot_thread_binding_on_conn(
            &conn,
            agent_id,
            card_id,
            dispatch_id,
            dispatch_context_json.as_ref(),
            channel_id_num,
        )
    };
    if let Some(binding) = slot_binding.clone() {
        if reset_stale_slot_thread_if_needed(
            db,
            &client,
            &token,
            discord_api_base,
            dispatch_id,
            &binding,
        )
        .await?
        {
            slot_binding = db.lock().ok().and_then(|conn| {
                read_slot_thread_binding(
                    &conn,
                    &binding.agent_id,
                    binding.slot_index,
                    channel_id_num,
                )
            });
        }
    }

    let slot_index = slot_binding
        .as_ref()
        .map(|binding| binding.slot_index)
        .or_else(|| context_slot_index(dispatch_context_json.as_ref()))
        .unwrap_or(0);
    let thread_name =
        build_slot_thread_name(db, dispatch_id, card_id, slot_index, issue_number, title);
    let existing_thread_ids = db
        .lock()
        .ok()
        .map(|conn| {
            collect_slot_thread_candidates(
                &conn,
                agent_id,
                card_id,
                slot_binding.as_ref(),
                channel_id_num,
            )
        })
        .unwrap_or_default();

    for existing_tid in &existing_thread_ids {
        if let Some(reused) = try_reuse_thread(
            &client,
            &token,
            discord_api_base,
            existing_tid,
            channel_id_num,
            &thread_name,
            &message,
            dispatch_id,
            card_id,
            db,
        )
        .await
        {
            if reused {
                if let Ok(conn) = db.lock() {
                    set_thread_for_channel(&conn, card_id, channel_id_num, existing_tid);
                    if let Some(binding) = slot_binding.as_ref() {
                        upsert_slot_thread_id(
                            &conn,
                            &binding.agent_id,
                            binding.slot_index,
                            channel_id_num,
                            existing_tid,
                        );
                    }
                }
                archive_duplicate_slot_threads(
                    &client,
                    &token,
                    discord_api_base,
                    channel_id_num,
                    existing_tid,
                    &existing_thread_ids,
                )
                .await;
                maybe_add_owner_to_dispatch_thread(
                    &client,
                    &token,
                    &discord_api_base,
                    existing_tid,
                    dispatch_id,
                    thread_owner_user_id,
                )
                .await;
                return Ok(());
            }
        }
    }

    if let Some(binding) = slot_binding.as_ref() {
        if let Ok(conn) = db.lock() {
            clear_slot_thread_id(&conn, &binding.agent_id, binding.slot_index, channel_id_num);
        }
    }

    let thread_url = discord_api_url(
        &discord_api_base,
        &format!("/channels/{channel_id_num}/threads"),
    );
    let thread_resp = client
        .post(&thread_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({
            "name": thread_name,
            "type": 11, // PUBLIC_THREAD
            "auto_archive_duration": 1440, // 24h
        }))
        .send()
        .await;

    match thread_resp {
        Ok(tr) if tr.status().is_success() => {
            if let Ok(thread_body) = tr.json::<serde_json::Value>().await {
                let thread_id = thread_body.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !thread_id.is_empty() {
                    // Send dispatch message into the thread BEFORE persisting thread_id.
                    // If the POST fails, we don't save thread_id so that
                    // [I-0] recovery sends to the channel and future dispatches won't
                    // reuse an empty thread.
                    let thread_msg_url = discord_api_url(
                        &discord_api_base,
                        &format!("/channels/{thread_id}/messages"),
                    );
                    let thread_msg_ok = client
                        .post(&thread_msg_url)
                        .header("Authorization", format!("Bot {}", token))
                        .json(&serde_json::json!({"content": message}))
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false);
                    if thread_msg_ok {
                        // Persist thread_id on success
                        if let Ok(conn) = db.lock() {
                            conn.execute(
                                "UPDATE task_dispatches SET thread_id = ?1 WHERE id = ?2",
                                rusqlite::params![thread_id, dispatch_id],
                            )
                            .ok();
                            set_thread_for_channel(&conn, card_id, channel_id_num, thread_id);
                            if let Some(binding) = slot_binding.as_ref() {
                                upsert_slot_thread_id(
                                    &conn,
                                    &binding.agent_id,
                                    binding.slot_index,
                                    channel_id_num,
                                    thread_id,
                                );
                            }
                        }
                        archive_duplicate_slot_threads(
                            &client,
                            &token,
                            discord_api_base,
                            channel_id_num,
                            thread_id,
                            &existing_thread_ids,
                        )
                        .await;
                        maybe_add_owner_to_dispatch_thread(
                            &client,
                            &token,
                            &discord_api_base,
                            thread_id,
                            dispatch_id,
                            thread_owner_user_id,
                        )
                        .await;
                        tracing::info!(
                            "[dispatch] Created thread {thread_id} and sent dispatch {dispatch_id} to {agent_id}"
                        );
                        return Ok(());
                    } else {
                        tracing::warn!(
                            "[dispatch] Thread message POST failed for dispatch {dispatch_id}"
                        );
                        return Err(format!(
                            "thread message POST failed for dispatch {dispatch_id}"
                        ));
                    }
                }
            }
            // thread_body parse failed or thread_id empty
            return Err("thread created but response parsing failed".into());
        }
        Ok(tr) => {
            // Thread creation failed — fall back to sending directly to the channel
            let status = tr.status();
            tracing::warn!(
                "[dispatch] Thread creation failed ({status}), falling back to channel message"
            );
            let url = discord_api_url(
                &discord_api_base,
                &format!("/channels/{channel_id_num}/messages"),
            );
            match client
                .post(&url)
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"content": message}))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    tracing::info!(
                        "[dispatch] Sent fallback message to {agent_id} (channel {channel_id})"
                    );
                    return Ok(());
                }
                Ok(r) => {
                    let st = r.status();
                    let body = r.text().await.unwrap_or_default();
                    tracing::warn!("[dispatch] Discord API error {st}: {body}");
                    return Err(format!("discord API error {st}: {body}"));
                }
                Err(e) => {
                    tracing::warn!("[dispatch] Request failed: {e}");
                    return Err(format!("discord request failed: {e}"));
                }
            }
        }
        Err(e) => {
            tracing::warn!("[dispatch] Thread creation request failed: {e}");
            return Err(format!("thread creation request failed: {e}"));
        }
    }
}

async fn resolve_review_followup_target_channel(
    db: &crate::db::Db,
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    card_id: &str,
    channel_id_num: u64,
) -> Result<String, String> {
    let active_thread_id: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for thread lookup".into()),
        };
        get_thread_for_channel(&conn, card_id, channel_id_num)
            .or_else(|| latest_work_dispatch_thread_on_conn(&conn, card_id))
    };
    let channel_id = channel_id_num.to_string();

    let Some(thread_id) = active_thread_id else {
        return Ok(channel_id);
    };

    let info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
    let response = match client
        .get(&info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(
                "[review] Failed to inspect thread {thread_id} for review followup: {err}"
            );
            return Ok(channel_id);
        }
    };

    if !response.status().is_success() {
        tracing::warn!(
            "[review] Thread {thread_id} unavailable for review followup: HTTP {}",
            response.status()
        );
        if let Ok(conn) = db.lock() {
            clear_thread_for_channel(&conn, card_id, channel_id_num);
        }
        return Ok(channel_id);
    }

    let body = match response.json::<serde_json::Value>().await {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(
                "[review] Failed to parse thread {thread_id} for review followup: {err}"
            );
            return Ok(channel_id);
        }
    };

    let metadata = body.get("thread_metadata");
    let locked = metadata
        .and_then(|value| value.get("locked"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if locked {
        tracing::warn!("[review] Thread {thread_id} is locked, falling back to channel");
        if let Ok(conn) = db.lock() {
            clear_thread_for_channel(&conn, card_id, channel_id_num);
        }
        return Ok(channel_id);
    }

    let archived = metadata
        .and_then(|value| value.get("archived"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !archived {
        return Ok(thread_id);
    }

    let mut last_error = None;
    for attempt in 1..=2 {
        match client
            .patch(&info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": false}))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(thread_id),
            Ok(response) => {
                let err = format!("HTTP {}", response.status());
                tracing::warn!(
                    "[review] Failed to unarchive thread {thread_id} (attempt {attempt}/2): {err}"
                );
                last_error = Some(err);
            }
            Err(err) => {
                tracing::warn!(
                    "[review] Failed to unarchive thread {thread_id} (attempt {attempt}/2): {err}"
                );
                last_error = Some(err.to_string());
            }
        }
    }

    Err(format!(
        "failed to unarchive review followup thread {thread_id}: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    ))
}

/// Handle primary-channel followup after a counter-model review completes.
/// pass/unknown verdicts send an immediate message; improve/rework/reject
/// create a review-decision dispatch whose notify row is delivered by outbox.
pub(super) async fn send_review_result_to_primary(
    db: &crate::db::Db,
    card_id: &str,
    review_dispatch_id: &str,
    verdict: &str,
) -> Result<(), String> {
    let discord_api_base = discord_api_base_url();
    let token = crate::credential::read_bot_token("announce");
    send_review_result_to_primary_with_context(
        db,
        card_id,
        review_dispatch_id,
        verdict,
        token.as_deref(),
        &discord_api_base,
    )
    .await
}

async fn send_review_result_to_primary_with_context(
    db: &crate::db::Db,
    card_id: &str,
    review_dispatch_id: &str,
    verdict: &str,
    token: Option<&str>,
    discord_api_base: &str,
) -> Result<(), String> {
    // Look up card info
    let (agent_id, title, issue_url): (String, String, Option<String>) = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for card lookup".into()),
        };
        let result = conn.query_row(
            "SELECT kc.assigned_agent_id, kc.title, kc.github_issue_url \
             FROM kanban_cards kc \
             WHERE kc.id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        );
        match result {
            Ok(r) => r,
            Err(_) => return Err(format!("card {card_id} not found or missing agent")),
        }
    };
    let review_dispatch_context: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for review dispatch lookup".into()),
        };
        conn.query_row(
            "SELECT context FROM task_dispatches WHERE id = ?1",
            [review_dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    };

    // For improve/rework/reject: create a review-decision dispatch via the
    // authoritative path and let the outbox worker deliver the message.
    if verdict != "pass" && verdict != "approved" && verdict != "unknown" {
        // #118: If approach-change already created a rework dispatch (review_status = rework_pending),
        // skip creating the review-decision dispatch to avoid double dispatch.
        {
            let skip = db
                .lock()
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT review_status FROM kanban_cards WHERE id = ?1",
                        [card_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten()
                })
                .map(|s| s == "rework_pending")
                .unwrap_or(false);
            if skip {
                tracing::info!(
                    "[review-followup] #118 skipping review-decision for {card_id} — approach-change rework already dispatched"
                );
                return Ok(());
            }
        }

        let review_context_json = review_dispatch_context
            .as_deref()
            .and_then(|ctx| serde_json::from_str::<serde_json::Value>(ctx).ok());
        let mut decision_context = serde_json::Map::new();
        decision_context.insert("verdict".to_string(), serde_json::json!(verdict));
        if let Some(provider) = review_context_json
            .as_ref()
            .and_then(|ctx| ctx.get("from_provider"))
            .and_then(|value| value.as_str())
        {
            decision_context.insert("from_provider".to_string(), serde_json::json!(provider));
        }
        if let Some(provider) = review_context_json
            .as_ref()
            .and_then(|ctx| ctx.get("target_provider"))
            .and_then(|value| value.as_str())
        {
            decision_context.insert("target_provider".to_string(), serde_json::json!(provider));
        }

        return match crate::dispatch::create_dispatch_core(
            db,
            card_id,
            &agent_id,
            "review-decision",
            &format!("[리뷰 검토] {title}"),
            &serde_json::Value::Object(decision_context),
        ) {
            Ok((id, _old_status, _reused)) => {
                if let Ok(conn) = db.lock() {
                    crate::engine::ops::review_state_sync_on_conn(
                        &conn,
                        &serde_json::json!({
                            "card_id": card_id,
                            "state": "suggestion_pending",
                            "pending_dispatch_id": id,
                            "last_verdict": verdict,
                        })
                        .to_string(),
                    );
                }
                tracing::info!(
                    "[review-followup] enqueued review-decision dispatch {} for card {}",
                    id,
                    card_id
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    "[review-followup] skipping review-decision dispatch for card {card_id}: {e}"
                );
                Err(format!(
                    "create_dispatch_core failed for review-decision: {e}"
                ))
            }
        };
    }

    let channel_id = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for primary channel lookup".into()),
        };
        resolve_review_followup_channel_on_conn(&conn, &agent_id)
            .ok()
            .flatten()
            .ok_or_else(|| {
                format!("agent {agent_id} missing primary discord channel for review followup")
            })?
    };

    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => match resolve_channel_alias(&channel_id) {
            Some(n) => n,
            None => return Err(format!("cannot resolve channel alias '{channel_id}'")),
        },
    };

    let token = token.ok_or_else(|| "no announce bot token".to_string())?;
    let client = reqwest::Client::new();
    let target_channel = resolve_review_followup_target_channel(
        db,
        &client,
        token,
        discord_api_base,
        card_id,
        channel_id_num,
    )
    .await?;

    if verdict == "pass" || verdict == "approved" {
        let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
        let message = format!("✅ [리뷰 통과] {title} — done으로 이동{url_line}");
        let url = discord_api_url(
            discord_api_base,
            &format!("/channels/{target_channel}/messages"),
        );

        match client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => {
                return Err(format!(
                    "discord API error {} for pass notification",
                    r.status()
                ));
            }
            Err(e) => return Err(format!("discord request failed for pass notification: {e}")),
        }
    }

    if verdict == "unknown" {
        let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
        let message = format!(
            "⚠️ [리뷰 verdict 미제출] {title}\n\
             ⛔ 코드 리뷰 금지 — 이것은 리뷰 결과 확인 요청입니다\n\
             카운터모델이 verdict를 제출하지 않고 세션이 종료됐습니다.\n\
             GitHub 이슈 코멘트를 확인하고 리뷰 내용이 있으면 반영해주세요.{url_line}"
        );
        let message = prefix_dispatch_message("review-decision", &message);
        let url = discord_api_url(
            discord_api_base,
            &format!("/channels/{target_channel}/messages"),
        );

        match client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => {
                return Err(format!(
                    "discord API error {} for unknown-verdict notification",
                    r.status()
                ));
            }
            Err(e) => {
                return Err(format!(
                    "discord request failed for unknown-verdict notification: {e}"
                ));
            }
        }
    }

    unreachable!("explicit review verdicts should return earlier");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, State},
        response::IntoResponse,
        routing::{get, post, put},
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    fn test_db() -> crate::db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    #[derive(Clone, Debug, Default)]
    struct MockDiscordState {
        archived: bool,
        unarchive_failures_remaining: usize,
        calls: Vec<String>,
        thread_names: HashMap<String, String>,
        thread_parents: HashMap<String, String>,
    }

    async fn spawn_mock_discord_server(
        initial_archived: bool,
    ) -> (
        String,
        Arc<Mutex<MockDiscordState>>,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_mock_discord_server_with_failures(initial_archived, 0).await
    }

    async fn spawn_mock_discord_server_with_failures(
        initial_archived: bool,
        unarchive_failures_remaining: usize,
    ) -> (
        String,
        Arc<Mutex<MockDiscordState>>,
        tokio::task::JoinHandle<()>,
    ) {
        async fn get_channel(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path(thread_id): Path<String>,
        ) -> impl IntoResponse {
            let (archived, thread_name, parent_id) = {
                let mut state = state.lock().unwrap();
                state.calls.push(format!("GET /channels/{thread_id}"));
                (
                    state.archived,
                    state
                        .thread_names
                        .get(&thread_id)
                        .cloned()
                        .unwrap_or_else(|| format!("seed-{thread_id}")),
                    state
                        .thread_parents
                        .get(&thread_id)
                        .cloned()
                        .unwrap_or_else(|| "123".to_string()),
                )
            };
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({
                    "id": thread_id,
                    "name": thread_name,
                    "parent_id": parent_id,
                    "thread_metadata": {
                        "archived": archived,
                    }
                })),
            )
        }

        async fn patch_channel(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path(thread_id): Path<String>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state.calls.push(format!("PATCH /channels/{thread_id}"));
            if body.get("archived").and_then(|value| value.as_bool()) == Some(false)
                && state.unarchive_failures_remaining > 0
            {
                state.unarchive_failures_remaining -= 1;
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"id": thread_id, "ok": false})),
                );
            }
            if let Some(name) = body.get("name").and_then(|value| value.as_str()) {
                state
                    .thread_names
                    .insert(thread_id.clone(), name.to_string());
            }
            if let Some(archived) = body.get("archived").and_then(|value| value.as_bool()) {
                state.archived = archived;
            }
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"id": thread_id, "ok": true})),
            )
        }

        async fn create_thread(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path(channel_id): Path<String>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state
                .calls
                .push(format!("POST /channels/{channel_id}/threads"));
            let thread_id = "thread-created".to_string();
            state
                .thread_parents
                .insert(thread_id.clone(), channel_id.clone());
            if let Some(name) = body.get("name").and_then(|value| value.as_str()) {
                state
                    .thread_names
                    .insert(thread_id.clone(), name.to_string());
            }
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"id": thread_id})),
            )
        }

        async fn post_message(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path(channel_id): Path<String>,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state
                .calls
                .push(format!("POST /channels/{channel_id}/messages"));
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"id": format!("message-{channel_id}")})),
            )
        }

        async fn add_thread_member(
            State(state): State<Arc<Mutex<MockDiscordState>>>,
            Path((thread_id, user_id)): Path<(String, String)>,
        ) -> impl IntoResponse {
            let mut state = state.lock().unwrap();
            state.calls.push(format!(
                "PUT /channels/{thread_id}/thread-members/{user_id}"
            ));
            axum::http::StatusCode::NO_CONTENT
        }

        let state = Arc::new(Mutex::new(MockDiscordState {
            archived: initial_archived,
            unarchive_failures_remaining,
            calls: Vec::new(),
            thread_names: HashMap::new(),
            thread_parents: HashMap::new(),
        }));
        let app = Router::new()
            .route(
                "/channels/{thread_id}",
                get(get_channel).patch(patch_channel),
            )
            .route("/channels/{channel_id}/threads", post(create_thread))
            .route("/channels/{channel_id}/messages", post(post_message))
            .route(
                "/channels/{thread_id}/thread-members/{user_id}",
                put(add_thread_member),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state, handle)
    }

    #[tokio::test]
    async fn add_thread_member_unarchives_archived_thread_before_put() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(true).await;
        let client = reqwest::Client::new();

        add_thread_member_to_dispatch_thread(&client, "announce-token", &base_url, "thread-1", 42)
            .await
            .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-1",
                "PATCH /channels/thread-1",
                "PUT /channels/thread-1/thread-members/42",
            ]
        );
    }

    #[tokio::test]
    async fn send_dispatch_to_discord_adds_configured_owner_to_created_thread() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-1', 'Test card', 'requested', 'agent-1', 'dispatch-1', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
                ) VALUES (
                    'dispatch-1', 'card-1', 'agent-1', 'implementation', 'pending', 'Test card', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Test card",
            "card-1",
            "dispatch-1",
            "announce-token",
            &base_url,
            Some(343742347365974026),
        )
        .await
        .unwrap();

        server_handle.abort();

        let state = state.lock().unwrap();
        assert!(
            state
                .calls
                .contains(&"POST /channels/123/threads".to_string())
        );
        assert!(
            state
                .calls
                .contains(&"POST /channels/thread-created/messages".to_string())
        );
        assert!(
            state
                .calls
                .contains(&"GET /channels/thread-created".to_string())
        );
        assert!(state.calls.contains(
            &"PUT /channels/thread-created/thread-members/343742347365974026".to_string()
        ));

        let conn = db.lock().unwrap();
        let thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(thread_id.as_deref(), Some("thread-created"));
    }

    #[tokio::test]
    async fn send_dispatch_reuses_recent_slot_thread_history_when_slot_map_is_empty() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, github_issue_number,
                    created_at, updated_at
                ) VALUES (
                    'card-current', 'Reuse card', 'requested', 'agent-1', 'dispatch-current', 506,
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-old', 'Old card', 'done', 'agent-1', 'dispatch-old',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-current', 'card-current', 'agent-1', 'implementation', 'pending',
                    'Reuse card', '{\"slot_index\":1}', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, thread_id,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-old', 'card-old', 'agent-1', 'implementation', 'completed',
                    'Old card', '{\"slot_index\":1}', 'thread-history',
                    datetime('now', '-1 day'), datetime('now', '-1 day')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES ('agent-1', 1, '{}')",
                [],
            )
            .unwrap();
        }

        send_dispatch_to_discord_inner_with_context(
            &db,
            "agent-1",
            "Reuse card",
            "card-current",
            "dispatch-current",
            "announce-token",
            &base_url,
            None,
        )
        .await
        .unwrap();

        server_handle.abort();

        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-history",
                "PATCH /channels/thread-history",
                "POST /channels/thread-history/messages",
            ]
        );
        assert_eq!(
            state.thread_names.get("thread-history").map(String::as_str),
            Some("[slot 1] #506 Reuse card")
        );

        let conn = db.lock().unwrap();
        let reused_thread_id: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM task_dispatches WHERE id = 'dispatch-current'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reused_thread_id.as_deref(), Some("thread-history"));

        let (active_thread_id, channel_thread_map): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT active_thread_id, channel_thread_map
                 FROM kanban_cards
                 WHERE id = 'card-current'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(active_thread_id.as_deref(), Some("thread-history"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(channel_thread_map.as_deref().unwrap())
                .unwrap()["123"],
            "thread-history"
        );

        let slot_map: String = conn
            .query_row(
                "SELECT thread_id_map
                 FROM auto_queue_slots
                 WHERE agent_id = 'agent-1' AND slot_index = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&slot_map).unwrap()["123"],
            "thread-history"
        );
    }

    fn insert_review_followup_fixture(db: &crate::db::Db) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '123')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                created_at, updated_at
            ) VALUES (
                'card-review', 'Review Card', 'review', 'agent-1', 'dispatch-review',
                '{\"123\":\"thread-primary\"}', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
            ) VALUES (
                'dispatch-review', 'card-review', 'agent-1', 'review', 'completed',
                '[Review R1] card-review', '{\"from_provider\":\"claude\"}',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn review_pass_notification_unarchives_and_posts_to_thread() {
        let (base_url, state, server_handle) = spawn_mock_discord_server(true).await;
        let db = test_db();
        insert_review_followup_fixture(&db);

        send_review_result_to_primary_with_context(
            &db,
            "card-review",
            "dispatch-review",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-primary",
                "PATCH /channels/thread-primary",
                "POST /channels/thread-primary/messages",
            ]
        );
    }

    #[tokio::test]
    async fn review_pass_notification_uses_primary_thread_even_when_review_context_points_to_alt_channel()
     {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (
                    id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, channel_thread_map,
                    created_at, updated_at
                ) VALUES (
                    'card-review-alt', 'Review Card', 'review', 'agent-1', 'dispatch-review-alt',
                    '{\"123\":\"thread-impl\"}', datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-review-alt', 'card-review-alt', 'agent-1', 'review', 'completed',
                    '[Review R1] card-review-alt', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_review_result_to_primary_with_context(
            &db,
            "card-review-alt",
            "dispatch-review-alt",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-impl",
                "POST /channels/thread-impl/messages",
            ]
        );
        assert!(
            !state
                .calls
                .contains(&"POST /channels/456/messages".to_string()),
            "review followup must not fall back to the review channel"
        );
    }

    #[tokio::test]
    async fn review_pass_notification_falls_back_to_primary_channel_when_no_implementation_thread_exists()
     {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (
                    id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-review-fallback', 'Review Card', 'review', 'agent-1', 'dispatch-review-fallback',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-review-fallback', 'card-review-fallback', 'agent-1', 'review', 'completed',
                    '[Review R1] card-review-fallback', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_review_result_to_primary_with_context(
            &db,
            "card-review-fallback",
            "dispatch-review-fallback",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(state.calls, vec!["POST /channels/123/messages"]);
        assert!(
            !state
                .calls
                .contains(&"POST /channels/456/messages".to_string()),
            "review followup fallback must use the implementation channel"
        );
    }

    #[tokio::test]
    async fn review_pass_notification_reuses_latest_work_dispatch_thread_when_channel_map_is_missing()
     {
        let (base_url, state, server_handle) = spawn_mock_discord_server(false).await;
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (
                    id, name, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 ) VALUES ('agent-1', 'Agent 1', 'claude', '123', '456', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at
                ) VALUES (
                    'card-review-history', 'Review Card', 'review', 'agent-1', 'dispatch-review-history',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-impl-history', 'card-review-history', 'agent-1', 'implementation', 'completed',
                    'Implementation', 'thread-history', datetime('now', '-1 minute'), datetime('now', '-1 minute')
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                    created_at, updated_at
                ) VALUES (
                    'dispatch-review-history', 'card-review-history', 'agent-1', 'review', 'completed',
                    '[Review R1] card-review-history', '{\"from_provider\":\"codex\",\"target_provider\":\"claude\"}',
                    datetime('now'), datetime('now')
                )",
                [],
            )
            .unwrap();
        }

        send_review_result_to_primary_with_context(
            &db,
            "card-review-history",
            "dispatch-review-history",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .unwrap();

        server_handle.abort();
        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-history",
                "POST /channels/thread-history/messages",
            ]
        );
        assert!(
            !state
                .calls
                .contains(&"POST /channels/456/messages".to_string()),
            "latest work thread must win over the review channel"
        );
    }

    #[tokio::test]
    async fn review_pass_notification_does_not_fallback_to_parent_when_unarchive_fails() {
        let (base_url, state, server_handle) =
            spawn_mock_discord_server_with_failures(true, 2).await;
        let db = test_db();
        insert_review_followup_fixture(&db);

        let err = send_review_result_to_primary_with_context(
            &db,
            "card-review",
            "dispatch-review",
            "pass",
            Some("announce-token"),
            &base_url,
        )
        .await
        .expect_err("review pass should fail closed when thread unarchive keeps failing");

        server_handle.abort();
        assert!(err.contains("failed to unarchive review followup thread thread-primary"));

        let state = state.lock().unwrap();
        assert_eq!(
            state.calls,
            vec![
                "GET /channels/thread-primary",
                "PATCH /channels/thread-primary",
                "PATCH /channels/thread-primary",
            ]
        );
        assert!(
            !state
                .calls
                .contains(&"POST /channels/123/messages".to_string()),
            "main channel fallback must not happen when the mapped thread still exists"
        );
    }
}
