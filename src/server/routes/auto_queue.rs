use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};

use super::AppState;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GenerateEntryBody {
    pub issue_number: i64,
    pub batch_phase: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct GenerateBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub issue_numbers: Option<Vec<i64>>,
    pub entries: Option<Vec<GenerateEntryBody>>,
    pub mode: Option<String>, // "priority-sort" (default), "dependency-aware", "similarity-aware", or "pm-assisted"
    pub unified_thread: Option<bool>,
    pub parallel: Option<bool>,
    pub max_concurrent_threads: Option<i64>,
    // Legacy compatibility only. Accepted from callers, but ignored.
    #[allow(dead_code)]
    pub max_concurrent_per_agent: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ActivateBody {
    pub run_id: Option<String>,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub thread_group: Option<i64>,
    pub unified_thread: Option<bool>,
    /// Internal-only: continue only already-active runs, never promote generated drafts.
    pub active_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    #[serde(rename = "orderedIds")]
    pub ordered_ids: Vec<String>,
    #[serde(rename = "agentId")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRunBody {
    pub status: Option<String>,
    pub unified_thread: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct EnqueueBody {
    pub repo: String,
    pub issue_number: i64,
    pub agent_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResetBody {
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone)]
struct GenerateCandidate {
    card_id: String,
    agent_id: String,
    priority: String,
    description: Option<String>,
    metadata: Option<String>,
    github_issue_number: Option<i64>,
    batch_phase: i64,
}

#[derive(Debug, Clone)]
struct PlannedEntry {
    card_idx: usize,
    thread_group: i64,
    priority_rank: i64,
    reason: String,
}

#[derive(Debug, Clone)]
struct GroupPlan {
    entries: Vec<PlannedEntry>,
    thread_group_count: i64,
    recommended_parallel_threads: i64,
    dependency_edges: usize,
    similarity_edges: usize,
    path_backed_card_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    Independent,
    Similarity,
    Dependency,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerateMode {
    PrioritySort,
    DependencyAware,
    SimilarityAware,
    PmAssisted,
}

impl GenerateMode {
    fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.unwrap_or("priority-sort") {
            "priority-sort" => Ok(Self::PrioritySort),
            "dependency-aware" => Ok(Self::DependencyAware),
            "similarity-aware" => Ok(Self::SimilarityAware),
            "pm-assisted" => Ok(Self::PmAssisted),
            other => Err(format!(
                "mode must be one of: priority-sort, dependency-aware, similarity-aware, pm-assisted (got {other})"
            )),
        }
    }

    fn uses_similarity(self) -> bool {
        matches!(self, Self::SimilarityAware)
    }

    /// Whether auto-grouping should be used. Requires `parallel=true` explicitly;
    /// `parallel=None` (unspecified) always defaults to sequential to preserve
    /// backward compatibility with existing callers like auto-queue.js.
    fn enables_auto_grouping(self, parallel: Option<bool>) -> bool {
        parallel.unwrap_or(false)
    }
}

fn run_slot_pool_size(conn: &rusqlite::Connection, run_id: &str) -> i64 {
    conn.query_row(
        "SELECT COALESCE(max_concurrent_threads, 1)
         FROM auto_queue_runs
         WHERE id = ?1",
        [run_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(1)
    .clamp(1, 10)
}

pub(crate) fn ensure_agent_slot_pool_rows(
    conn: &rusqlite::Connection,
    agent_id: &str,
    slot_pool_size: i64,
) -> rusqlite::Result<()> {
    for slot_index in 0..slot_pool_size.clamp(1, 32) {
        conn.execute(
            "INSERT OR IGNORE INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES (?1, ?2, '{}')",
            rusqlite::params![agent_id, slot_index],
        )?;
    }
    Ok(())
}

fn ensure_agent_slot_rows(
    conn: &rusqlite::Connection,
    run_id: &str,
    agent_id: &str,
) -> rusqlite::Result<()> {
    ensure_agent_slot_pool_rows(conn, agent_id, run_slot_pool_size(conn, run_id))
}

fn clear_inactive_slot_assignments(conn: &rusqlite::Connection) {
    conn.execute(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = datetime('now')
         WHERE assigned_run_id IS NOT NULL
           AND assigned_run_id NOT IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
        [],
    )
    .ok();
}

fn completed_group_slots(conn: &rusqlite::Connection, run_id: &str) -> Vec<(String, i64)> {
    let mut stmt = match conn.prepare(
        "SELECT agent_id, slot_index, assigned_thread_group
         FROM auto_queue_slots
         WHERE assigned_run_id = ?1",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let assigned: Vec<(String, i64, i64)> = stmt
        .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .ok()
        .map(|rows| rows.filter_map(|row| row.ok()).collect())
        .unwrap_or_default();
    drop(stmt);

    let mut released = Vec::new();
    for (agent_id, slot_index, thread_group) in assigned {
        let still_active: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0
                 FROM auto_queue_entries
                 WHERE run_id = ?1
                   AND agent_id = ?2
                   AND COALESCE(thread_group, 0) = ?3
                   AND status IN ('pending', 'dispatched')",
                rusqlite::params![run_id, agent_id, thread_group],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if still_active {
            continue;
        }
        released.push((agent_id, slot_index));
    }

    released
}

fn release_group_slots(conn: &rusqlite::Connection, slots: &[(String, i64)]) {
    for (agent_id, slot_index) in slots {
        conn.execute(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL,
                 updated_at = datetime('now')
             WHERE agent_id = ?1 AND slot_index = ?2",
            rusqlite::params![agent_id, slot_index],
        )
        .ok();
    }
}

fn release_run_slots(conn: &rusqlite::Connection, run_id: &str) {
    conn.execute(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = datetime('now')
         WHERE assigned_run_id = ?1",
        [run_id],
    )
    .ok();
}

fn current_batch_phase(conn: &rusqlite::Connection, run_id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT MIN(COALESCE(batch_phase, 0))
         FROM auto_queue_entries
         WHERE run_id = ?1
           AND status IN ('pending', 'dispatched')
           AND COALESCE(batch_phase, 0) > 0",
        [run_id],
        |row| row.get::<_, Option<i64>>(0),
    )
    .ok()
    .flatten()
}

fn batch_phase_is_eligible(batch_phase: i64, current_phase: Option<i64>) -> bool {
    if batch_phase == 0 {
        return true;
    }
    match current_phase {
        Some(phase) => batch_phase == phase,
        None => true,
    }
}

fn group_has_pending_entries(
    conn: &rusqlite::Connection,
    run_id: &str,
    thread_group: i64,
    current_phase: Option<i64>,
) -> bool {
    let mut stmt = match conn.prepare(
        "SELECT COALESCE(batch_phase, 0)
         FROM auto_queue_entries
         WHERE run_id = ?1
           AND COALESCE(thread_group, 0) = ?2
           AND status = 'pending'
         ORDER BY priority_rank ASC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return false,
    };
    stmt.query_map(rusqlite::params![run_id, thread_group], |row| {
        row.get::<_, i64>(0)
    })
    .ok()
    .map(|rows| {
        rows.filter_map(|row| row.ok())
            .any(|batch_phase| batch_phase_is_eligible(batch_phase, current_phase))
    })
    .unwrap_or(false)
}

fn first_pending_entry_for_group(
    conn: &rusqlite::Connection,
    run_id: &str,
    thread_group: i64,
    current_phase: Option<i64>,
) -> Option<(String, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT e.id, e.kanban_card_id, e.agent_id, COALESCE(e.batch_phase, 0)
             FROM auto_queue_entries e
             WHERE e.run_id = ?1
               AND COALESCE(e.thread_group, 0) = ?2
               AND e.status = 'pending'
             ORDER BY e.priority_rank ASC",
        )
        .ok()?;
    stmt.query_map(rusqlite::params![run_id, thread_group], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })
    .ok()
    .and_then(|rows| {
        rows.filter_map(|row| row.ok())
            .find_map(|(entry_id, card_id, agent_id, batch_phase)| {
                batch_phase_is_eligible(batch_phase, current_phase)
                    .then_some((entry_id, card_id, agent_id))
            })
    })
}

fn assigned_groups_with_pending_entries(
    conn: &rusqlite::Connection,
    run_id: &str,
    current_phase: Option<i64>,
) -> Vec<i64> {
    let mut stmt = match conn.prepare(
        "SELECT DISTINCT s.assigned_thread_group, COALESCE(e.batch_phase, 0)
         FROM auto_queue_slots s
         JOIN auto_queue_entries e
           ON e.run_id = ?1
          AND e.agent_id = s.agent_id
          AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
         WHERE s.assigned_run_id = ?1
           AND s.assigned_thread_group IS NOT NULL
           AND EXISTS (
               SELECT 1
               FROM auto_queue_entries e
               WHERE e.run_id = ?1
                 AND e.agent_id = s.agent_id
                 AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
                 AND e.status = 'pending'
           )
           AND NOT EXISTS (
               SELECT 1
               FROM auto_queue_entries e
               WHERE e.run_id = ?1
                 AND e.agent_id = s.agent_id
                 AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
                 AND e.status = 'dispatched'
           )
         ORDER BY s.assigned_thread_group ASC, s.slot_index ASC, COALESCE(e.batch_phase, 0) ASC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let mut seen = HashSet::new();
    stmt.query_map([run_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })
    .ok()
    .map(|rows| {
        rows.filter_map(|row| row.ok())
            .filter_map(|(thread_group, batch_phase)| {
                (batch_phase_is_eligible(batch_phase, current_phase) && seen.insert(thread_group))
                    .then_some(thread_group)
            })
            .collect()
    })
    .unwrap_or_default()
}

#[derive(Debug, Clone, Copy)]
struct RequestedGenerateEntry {
    issue_number: i64,
    batch_phase: i64,
}

fn normalize_generate_entries(
    body: &GenerateBody,
) -> Result<Option<Vec<RequestedGenerateEntry>>, String> {
    if body
        .entries
        .as_ref()
        .is_some_and(|entries| !entries.is_empty())
        && body
            .issue_numbers
            .as_ref()
            .is_some_and(|issue_numbers| !issue_numbers.is_empty())
    {
        return Err("use either issue_numbers or entries, not both".to_string());
    }

    let Some(entries) = body.entries.as_ref().filter(|entries| !entries.is_empty()) else {
        return Ok(None);
    };

    let mut normalized = Vec::with_capacity(entries.len());
    let mut seen = HashSet::new();
    for entry in entries {
        let batch_phase = entry.batch_phase.unwrap_or(0);
        if batch_phase < 0 {
            return Err("batch_phase must be >= 0".to_string());
        }
        if !seen.insert(entry.issue_number) {
            return Err(format!(
                "duplicate issue_number in entries payload: {}",
                entry.issue_number
            ));
        }
        normalized.push(RequestedGenerateEntry {
            issue_number: entry.issue_number,
            batch_phase,
        });
    }

    Ok(Some(normalized))
}

fn allocate_slot_for_group_agent(
    conn: &rusqlite::Connection,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
) -> Option<(i64, bool)> {
    ensure_agent_slot_rows(conn, run_id, agent_id).ok()?;

    let existing: Option<i64> = conn
        .query_row(
            "SELECT slot_index
             FROM auto_queue_slots
             WHERE agent_id = ?1
               AND assigned_run_id = ?2
               AND COALESCE(assigned_thread_group, 0) = ?3
             LIMIT 1",
            rusqlite::params![agent_id, run_id, thread_group],
            |row| row.get(0),
        )
        .ok();
    if let Some(slot_index) = existing {
        conn.execute(
            "UPDATE auto_queue_entries
             SET slot_index = ?1
             WHERE run_id = ?2
               AND agent_id = ?3
               AND COALESCE(thread_group, 0) = ?4
               AND slot_index IS NULL",
            rusqlite::params![slot_index, run_id, agent_id, thread_group],
        )
        .ok();
        return Some((slot_index, false));
    }

    let free_slot: Option<i64> = conn
        .query_row(
            "SELECT slot_index
             FROM auto_queue_slots
             WHERE agent_id = ?1
               AND assigned_run_id IS NULL
             ORDER BY slot_index ASC
             LIMIT 1",
            [agent_id],
            |row| row.get(0),
        )
        .ok();
    let Some(slot_index) = free_slot else {
        return None;
    };

    conn.execute(
        "UPDATE auto_queue_slots
         SET assigned_run_id = ?1,
             assigned_thread_group = ?2,
             updated_at = datetime('now')
         WHERE agent_id = ?3
           AND slot_index = ?4
           AND assigned_run_id IS NULL",
        rusqlite::params![run_id, thread_group, agent_id, slot_index],
    )
    .ok()?;
    conn.execute(
        "UPDATE auto_queue_entries
         SET slot_index = ?1
         WHERE run_id = ?2
           AND agent_id = ?3
           AND COALESCE(thread_group, 0) = ?4
           AND slot_index IS NULL",
        rusqlite::params![slot_index, run_id, agent_id, thread_group],
    )
    .ok();
    Some((slot_index, true))
}

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

pub(crate) fn clear_slot_sessions_db(
    conn: &rusqlite::Connection,
    thread_channel_ids: &[u64],
) -> usize {
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

fn clear_slot_threads_for_slot(
    state: &AppState,
    conn: &rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
) -> usize {
    let target = build_slot_clear_target(conn, agent_id, slot_index);
    let cleared = clear_slot_sessions_db(conn, &target.thread_channel_ids);

    if let Some(registry) = state.health_registry.clone() {
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

pub(crate) fn slot_has_active_dispatch(
    conn: &rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
) -> bool {
    let auto_queue_active: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0
             FROM auto_queue_entries
             WHERE agent_id = ?1
               AND slot_index = ?2
               AND status = 'dispatched'",
            rusqlite::params![agent_id, slot_index],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if auto_queue_active {
        return true;
    }

    conn.query_row(
        "SELECT COUNT(*) > 0
         FROM task_dispatches
         WHERE to_agent_id = ?1
           AND status IN ('pending', 'dispatched')
           AND CAST(json_extract(COALESCE(context, '{}'), '$.slot_index') AS INTEGER) = ?2",
        rusqlite::params![agent_id, slot_index],
        |row| row.get(0),
    )
    .unwrap_or(false)
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

pub(crate) async fn reset_slot_thread_bindings(
    db: &crate::db::Db,
    agent_id: &str,
    slot_index: i64,
) -> Result<(usize, usize, usize), String> {
    let conn = db
        .separate_conn()
        .map_err(|err| format!("db open failed for slot reset: {err}"))?;
    if slot_has_active_dispatch(&conn, agent_id, slot_index) {
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

fn enqueueable_states_for(pipeline: &crate::pipeline::PipelineConfig) -> Vec<String> {
    let mut states: Vec<String> = pipeline
        .dispatchable_states()
        .iter()
        .map(|s| s.to_string())
        .collect();
    // Requested is a pre-execution staging state in the default pipeline. Allow
    // enqueueing it directly so callers can queue already-requested work.
    if pipeline.is_valid_state("requested") && !states.iter().any(|s| s == "requested") {
        states.push("requested".to_string());
    }
    // Ready is an explicit preparation state. Backlog is intentionally excluded:
    // auto-queue should only accept work that has already been prepared.
    if pipeline.is_valid_state("ready") && !states.iter().any(|s| s == "ready") {
        states.push("ready".to_string());
    }
    states
}

fn build_card_filters(
    alias: &str,
    repo: Option<&String>,
    agent_id: Option<&String>,
    issue_numbers: Option<&Vec<i64>>,
) -> (Vec<String>, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(repo) = repo {
        conditions.push(format!("{}repo_id = ?{}", prefix, params.len() + 1));
        params.push(Box::new(repo.clone()));
    }
    if let Some(agent_id) = agent_id {
        conditions.push(format!(
            "{}assigned_agent_id = ?{}",
            prefix,
            params.len() + 1
        ));
        params.push(Box::new(agent_id.clone()));
    }
    if let Some(issue_numbers) = issue_numbers.filter(|nums| !nums.is_empty()) {
        let base_idx = params.len() + 1;
        let placeholders = issue_numbers
            .iter()
            .enumerate()
            .map(|(idx, _)| format!("?{}", base_idx + idx))
            .collect::<Vec<_>>()
            .join(",");
        conditions.push(format!(
            "{}github_issue_number IN ({})",
            prefix, placeholders
        ));
        for issue_number in issue_numbers {
            params.push(Box::new(*issue_number));
        }
    }

    (conditions, params)
}

fn priority_sort_key(priority: &str) -> i32 {
    match priority {
        "urgent" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn extract_dependency_numbers(card: &GenerateCandidate) -> Vec<i64> {
    let mut deps = HashSet::new();
    let sources = [card.description.as_deref(), card.metadata.as_deref()];
    let re = regex::Regex::new(r"#(\d+)").expect("dependency regex must compile");
    for text in sources.into_iter().flatten() {
        for cap in re.captures_iter(text) {
            if let Ok(num) = cap[1].parse::<i64>() {
                if Some(num) != card.github_issue_number {
                    deps.insert(num);
                }
            }
        }
    }
    let mut out: Vec<i64> = deps.into_iter().collect();
    out.sort_unstable();
    out
}

fn normalize_similarity_path(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim_matches(|ch: char| matches!(ch, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}'))
        .trim_end_matches(|ch: char| matches!(ch, '.' | ',' | ':' | ';'));
    if trimmed.is_empty() || !trimmed.contains('/') {
        return None;
    }
    Some(trimmed.to_string())
}

fn extract_file_paths_from_text(text: &str) -> HashSet<String> {
    let re = regex::Regex::new(
        r"(?:src|dashboard|policies|tests|scripts|docs|crates|migrations|assets|prompts|templates|examples|references)/[A-Za-z0-9_./-]+",
    )
    .expect("file path regex must compile");
    re.find_iter(text)
        .filter_map(|m| normalize_similarity_path(m.as_str()))
        .collect()
}

fn similarity_paths(card: &GenerateCandidate) -> HashSet<String> {
    let description_paths = card
        .description
        .as_deref()
        .map(extract_file_paths_from_text)
        .unwrap_or_default();
    if !description_paths.is_empty() {
        return description_paths;
    }
    card.metadata
        .as_deref()
        .map(extract_file_paths_from_text)
        .unwrap_or_default()
}

fn similarity_edge_allowed(left: &GenerateCandidate, right: &GenerateCandidate) -> bool {
    // Allow cross-agent similarity edges — file overlap determines conflict,
    // not agent assignment. Cards touching the same files should be grouped
    // regardless of which agent they're assigned to.
    !left.agent_id.is_empty() && !right.agent_id.is_empty()
}

/// Compute file-path-based similarity between two sets of extracted paths.
///
/// Each element is a full file path string (e.g. `src/server/routes/auto_queue.rs`)
/// extracted from issue description text by [`extract_file_paths_from_text()`].
/// This is NOT token-level similarity — paths are compared as atomic strings.
///
/// Returns `(shared_count, score)` where score = max(Jaccard, Overlap coefficient):
/// - **Jaccard index**: |intersection| / |union| — penalizes sets of very different sizes.
/// - **Overlap coefficient**: |intersection| / min(|left|, |right|) — captures "subset" overlap.
///   e.g. if issue A touches {X, Y} and issue B touches {X, Z}, overlap = 1/2 = 0.5.
///
/// Using max() ensures that two issues sharing a file are grouped even when their
/// total file counts differ significantly.
fn path_similarity(left: &HashSet<String>, right: &HashSet<String>) -> (usize, f64) {
    if left.is_empty() || right.is_empty() {
        return (0, 0.0);
    }
    let shared = left.intersection(right).count();
    if shared == 0 {
        return (0, 0.0);
    }
    let union = left.union(right).count();
    let overlap = shared as f64 / left.len().min(right.len()) as f64;
    let jaccard = shared as f64 / union as f64;
    (shared, overlap.max(jaccard))
}

fn compact_path_label(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    }
}

fn group_path_labels(members: &[usize], paths: &[HashSet<String>]) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for &member in members {
        for path in &paths[member] {
            *counts.entry(path.clone()).or_insert(0) += 1;
        }
    }

    let mut ranked: Vec<(String, usize)> = counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .take(3)
        .map(|(path, _)| compact_path_label(&path))
        .collect()
}

fn build_group_reason(
    kind: GroupKind,
    path_labels: &[String],
    dependency_issue_nums: &[i64],
    member_count: usize,
) -> String {
    let path_suffix = if path_labels.is_empty() {
        String::new()
    } else {
        format!(" [{}]", path_labels.join(", "))
    };
    match kind {
        GroupKind::Mixed => format!(
            "의존성 + 유사도 그룹{} ({}개 카드)",
            path_suffix, member_count
        ),
        GroupKind::Dependency => {
            if dependency_issue_nums.is_empty() {
                format!("의존성 그룹 ({}개 카드)", member_count)
            } else {
                let refs = dependency_issue_nums
                    .iter()
                    .map(|num| format!("#{num}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("의존성 그룹 · 선행 {refs}")
            }
        }
        GroupKind::Similarity => {
            if path_labels.is_empty() {
                format!("유사도 그룹 ({}개 카드)", member_count)
            } else {
                format!("유사도 그룹 [{}]", path_labels.join(", "))
            }
        }
        GroupKind::Independent => "독립 그룹".to_string(),
    }
}

fn build_group_plan(cards: &[GenerateCandidate], enable_similarity: bool) -> GroupPlan {
    const SIMILARITY_THRESHOLD: f64 = 0.5;
    if cards.is_empty() {
        return GroupPlan {
            entries: Vec::new(),
            thread_group_count: 0,
            recommended_parallel_threads: 1,
            dependency_edges: 0,
            similarity_edges: 0,
            path_backed_card_count: 0,
        };
    }

    let mut issue_to_idx: HashMap<i64, usize> = HashMap::new();
    for (idx, card) in cards.iter().enumerate() {
        if let Some(num) = card.github_issue_number {
            issue_to_idx.insert(num, idx);
        }
    }

    let similarity_paths_per_card: Vec<HashSet<String>> = if enable_similarity {
        cards.iter().map(similarity_paths).collect()
    } else {
        vec![HashSet::new(); cards.len()]
    };
    let dependency_numbers: Vec<Vec<i64>> = cards.iter().map(extract_dependency_numbers).collect();
    let path_backed_card_count = similarity_paths_per_card
        .iter()
        .filter(|paths| !paths.is_empty())
        .count();

    let n = cards.len();
    let mut dependency_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut parent: Vec<usize> = (0..n).collect();
    let mut dependency_edges = 0usize;
    let mut similarity_edges = 0usize;

    fn find(parent: &mut [usize], x: usize) -> usize {
        if parent[x] != x {
            parent[x] = find(parent, parent[x]);
        }
        parent[x]
    }

    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[rb] = ra;
        }
    }

    for (idx, deps) in dependency_numbers.iter().enumerate() {
        let mut seen = HashSet::new();
        for dep_num in deps {
            if let Some(&dep_idx) = issue_to_idx.get(dep_num) {
                if dep_idx != idx && seen.insert(dep_idx) {
                    dependency_adj[dep_idx].push(idx);
                    union(&mut parent, dep_idx, idx);
                    dependency_edges += 1;
                }
            }
        }
    }

    for left in 0..n {
        for right in (left + 1)..n {
            if !similarity_edge_allowed(&cards[left], &cards[right]) {
                continue;
            }
            let (shared, score) = path_similarity(
                &similarity_paths_per_card[left],
                &similarity_paths_per_card[right],
            );
            if shared == 0 || score < SIMILARITY_THRESHOLD {
                continue;
            }
            union(&mut parent, left, right);
            similarity_edges += 1;
        }
    }

    let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..n {
        let root = find(&mut parent, idx);
        components.entry(root).or_default().push(idx);
    }

    let mut component_roots: Vec<usize> = components.keys().copied().collect();
    component_roots
        .sort_by_key(|root| components[root].iter().copied().min().unwrap_or(usize::MAX));

    let mut planned_entries = Vec::with_capacity(n);
    for (group_num, root) in component_roots.iter().enumerate() {
        let mut members = components[root].clone();
        members.sort_by_key(|idx| (priority_sort_key(&cards[*idx].priority), *idx));
        let member_set: HashSet<usize> = members.iter().copied().collect();

        let mut local_in_degree: HashMap<usize, usize> =
            members.iter().map(|idx| (*idx, 0)).collect();
        let mut group_dep_nums = HashSet::new();
        let mut group_dependency_edges = 0usize;
        let mut group_similarity_edges = 0usize;

        for &member in &members {
            for dep_num in &dependency_numbers[member] {
                if let Some(&dep_idx) = issue_to_idx.get(dep_num) {
                    if member_set.contains(&dep_idx) && dep_idx != member {
                        *local_in_degree.entry(member).or_insert(0) += 1;
                        group_dep_nums.insert(*dep_num);
                        group_dependency_edges += 1;
                    }
                }
            }
        }

        for pos in 0..members.len() {
            for next in (pos + 1)..members.len() {
                let left = members[pos];
                let right = members[next];
                if similarity_edge_allowed(&cards[left], &cards[right]) {
                    let (shared, score) = path_similarity(
                        &similarity_paths_per_card[left],
                        &similarity_paths_per_card[right],
                    );
                    if shared > 0 && score >= SIMILARITY_THRESHOLD {
                        group_similarity_edges += 1;
                    }
                }
            }
        }

        let mut available: Vec<usize> = members
            .iter()
            .copied()
            .filter(|member| local_in_degree.get(member).copied().unwrap_or(0) == 0)
            .collect();
        let mut sorted = Vec::with_capacity(members.len());
        while !available.is_empty() {
            available.sort_by_key(|idx| (priority_sort_key(&cards[*idx].priority), *idx));
            let current = available.remove(0);
            sorted.push(current);
            for &next in &dependency_adj[current] {
                if !member_set.contains(&next) {
                    continue;
                }
                if let Some(deg) = local_in_degree.get_mut(&next) {
                    if *deg > 0 {
                        *deg -= 1;
                        if *deg == 0 {
                            available.push(next);
                        }
                    }
                }
            }
        }

        if sorted.len() < members.len() {
            let seen: HashSet<usize> = sorted.iter().copied().collect();
            for member in &members {
                if !seen.contains(member) {
                    sorted.push(*member);
                }
            }
        }

        let path_labels = group_path_labels(&members, &similarity_paths_per_card);
        let mut dep_nums: Vec<i64> = group_dep_nums.into_iter().collect();
        dep_nums.sort_unstable();
        let kind = match (group_dependency_edges > 0, group_similarity_edges > 0) {
            (true, true) => GroupKind::Mixed,
            (true, false) => GroupKind::Dependency,
            (false, true) => GroupKind::Similarity,
            (false, false) => GroupKind::Independent,
        };
        let group_reason = build_group_reason(kind, &path_labels, &dep_nums, members.len());

        for (priority_rank, idx) in sorted.into_iter().enumerate() {
            let mut entry_reason = group_reason.clone();
            let deps_in_queue: Vec<i64> = dependency_numbers[idx]
                .iter()
                .copied()
                .filter(|dep_num| issue_to_idx.contains_key(dep_num))
                .collect();
            if !deps_in_queue.is_empty() {
                let refs = deps_in_queue
                    .iter()
                    .map(|dep_num| format!("#{dep_num}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                entry_reason = format!("{entry_reason} · 선행 {refs}");
            }
            planned_entries.push(PlannedEntry {
                card_idx: idx,
                thread_group: group_num as i64,
                priority_rank: priority_rank as i64,
                reason: entry_reason,
            });
        }
    }

    let distinct_agents = cards
        .iter()
        .filter(|card| !card.agent_id.is_empty())
        .map(|card| card.agent_id.clone())
        .collect::<HashSet<_>>()
        .len()
        .max(1) as i64;
    let thread_group_count = component_roots.len() as i64;
    let recommended_parallel_threads = if thread_group_count <= 1 {
        1
    } else {
        thread_group_count.min(distinct_agents).clamp(1, 4)
    };

    GroupPlan {
        entries: planned_entries,
        thread_group_count,
        recommended_parallel_threads,
        dependency_edges,
        similarity_edges,
        path_backed_card_count,
    }
}

fn entry_to_json(conn: &rusqlite::Connection, entry_id: &str) -> serde_json::Value {
    conn.query_row(
        "SELECT e.id, e.agent_id, e.kanban_card_id, e.priority_rank, e.reason, e.status,
                CAST(strftime('%s', e.created_at) AS INTEGER) * 1000,
                CASE WHEN e.dispatched_at IS NOT NULL THEN CAST(strftime('%s', e.dispatched_at) AS INTEGER) * 1000 END,
                CASE WHEN e.completed_at IS NOT NULL THEN CAST(strftime('%s', e.completed_at) AS INTEGER) * 1000 END,
                kc.title, kc.github_issue_number, kc.github_issue_url,
                COALESCE(e.thread_group, 0),
                e.slot_index,
                COALESCE(e.batch_phase, 0)
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id
         WHERE e.id = ?1",
        [entry_id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "agent_id": row.get::<_, String>(1)?,
                "card_id": row.get::<_, String>(2)?,
                "priority_rank": row.get::<_, i64>(3)?,
                "reason": row.get::<_, Option<String>>(4)?,
                "status": row.get::<_, String>(5)?,
                "created_at": row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                "dispatched_at": row.get::<_, Option<i64>>(7)?,
                "completed_at": row.get::<_, Option<i64>>(8)?,
                "card_title": row.get::<_, Option<String>>(9)?,
                "github_issue_number": row.get::<_, Option<i64>>(10)?,
                "github_repo": row.get::<_, Option<String>>(11)?,
                "thread_group": row.get::<_, i64>(12)?,
                "slot_index": row.get::<_, Option<i64>>(13)?,
                "batch_phase": row.get::<_, i64>(14)?,
            }))
        },
    )
    .unwrap_or(json!(null))
}

fn run_to_json(conn: &rusqlite::Connection, run_id: &str) -> serde_json::Value {
    conn.query_row(
        "SELECT id, repo, agent_id, status, timeout_minutes,
                ai_model, ai_rationale,
                CAST(strftime('%s', created_at) AS INTEGER) * 1000,
                CASE WHEN completed_at IS NOT NULL THEN CAST(strftime('%s', completed_at) AS INTEGER) * 1000 END,
                unified_thread, unified_thread_id,
                COALESCE(max_concurrent_threads, 1),
                COALESCE(thread_group_count, 1)
         FROM auto_queue_runs WHERE id = ?1",
        [run_id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "repo": row.get::<_, Option<String>>(1)?,
                "agent_id": row.get::<_, Option<String>>(2)?,
                "status": row.get::<_, String>(3)?,
                "timeout_minutes": row.get::<_, i64>(4)?,
                "ai_model": row.get::<_, Option<String>>(5)?,
                "ai_rationale": row.get::<_, Option<String>>(6)?,
                "created_at": row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                "completed_at": row.get::<_, Option<i64>>(8)?,
                "unified_thread": false,
                "unified_thread_id": serde_json::Value::Null,
                "max_concurrent_threads": row.get::<_, i64>(11)?,
                "thread_group_count": row.get::<_, i64>(12)?,
            }))
        },
    )
    .unwrap_or(json!(null))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueEntryOrder {
    id: String,
    status: String,
    agent_id: String,
}

fn reorder_entry_ids(
    entries: &[QueueEntryOrder],
    ordered_ids: &[String],
    agent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    if ordered_ids.is_empty() {
        return Err("orderedIds cannot be empty".to_string());
    }

    let scope_ids: Vec<String> = entries
        .iter()
        .filter(|entry| {
            entry.status == "pending"
                && agent_id
                    .map(|target| entry.agent_id == target)
                    .unwrap_or(true)
        })
        .map(|entry| entry.id.clone())
        .collect();
    if scope_ids.is_empty() {
        return Err("no pending entries found for reorder scope".to_string());
    }

    let scope_set: HashSet<&str> = scope_ids.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    let mut replacement_ids = Vec::new();
    for id in ordered_ids {
        let id_str = id.as_str();
        if scope_set.contains(id_str) && seen.insert(id_str) {
            replacement_ids.push(id.clone());
        }
    }
    if replacement_ids.is_empty() {
        return Err("orderedIds do not match any pending entries in scope".to_string());
    }

    for id in &scope_ids {
        if !seen.contains(id.as_str()) {
            replacement_ids.push(id.clone());
        }
    }

    let mut replacement_iter = replacement_ids.into_iter();
    let mut reordered = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.status == "pending"
            && agent_id
                .map(|target| entry.agent_id == target)
                .unwrap_or(true)
        {
            let next_id = replacement_iter
                .next()
                .ok_or_else(|| "replacement sequence exhausted".to_string())?;
            reordered.push(next_id);
        } else {
            reordered.push(entry.id.clone());
        }
    }

    if replacement_iter.next().is_some() {
        return Err("replacement sequence was not fully consumed".to_string());
    }

    Ok(reordered)
}

// ── Endpoints ────────────────────────────────────────────────────────────────

/// POST /api/auto-queue/generate
/// Creates a queue run from ready cards, ordered by priority.
pub async fn generate(
    State(state): State<AppState>,
    Json(body): Json<GenerateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let _ignored_unified_thread = body.unified_thread.is_some();
    let requested_entries = match normalize_generate_entries(&body) {
        Ok(entries) => entries,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
        }
    };
    let requested_issue_numbers = requested_entries
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| entry.issue_number)
                .collect::<Vec<_>>()
        })
        .or_else(|| body.issue_numbers.clone().filter(|nums| !nums.is_empty()));
    let requested_entry_meta: HashMap<i64, (usize, i64)> = requested_entries
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.issue_number, (index, entry.batch_phase)))
                .collect()
        })
        .unwrap_or_default();

    if let Some(issue_numbers) = requested_issue_numbers
        .as_ref()
        .filter(|nums| !nums.is_empty())
    {
        let conn = match state.db.separate_conn() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        let (mut conditions, params) = build_card_filters(
            "kc",
            body.repo.as_ref(),
            body.agent_id.as_ref(),
            Some(issue_numbers),
        );
        conditions.push("kc.status = 'backlog'".to_string());
        let sql = format!(
            "SELECT kc.id, kc.repo_id, kc.assigned_agent_id
             FROM kanban_cards kc
             WHERE {}",
            conditions.join(" AND ")
        );
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        let backlog_cards: Vec<(String, Option<String>, Option<String>)> = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .ok()
            .map(|rows| rows.filter_map(|row| row.ok()).collect())
            .unwrap_or_default();
        drop(stmt);
        drop(conn);

        for (card_id, repo_id, assigned_agent_id) in backlog_cards {
            let conn = match state.db.separate_conn() {
                Ok(c) => c,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{e}")})),
                    );
                }
            };
            crate::pipeline::ensure_loaded();
            let effective = crate::pipeline::resolve_for_card(
                &conn,
                repo_id.as_deref(),
                assigned_agent_id.as_deref(),
            );
            let prep_path = if effective.is_valid_state("ready") {
                effective
                    .free_path_to_state("backlog", "ready")
                    .or_else(|| effective.free_path_to_dispatchable("backlog"))
            } else {
                effective.free_path_to_dispatchable("backlog")
            };
            let Some(path) = prep_path else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!("card {card_id} has no free path from backlog to ready/dispatchable state"),
                    })),
                );
            };
            drop(conn);

            for step in &path {
                if let Err(e) = crate::kanban::transition_status_no_hooks(
                    &state.db,
                    &card_id,
                    step,
                    "auto-queue-generate",
                ) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": format!("failed to auto-transition card {card_id} to {step}: {e}"),
                        })),
                    );
                }
            }
        }
    }

    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    // Build filter — pipeline-driven enqueueable states (dispatchable + prepared staging states)
    crate::pipeline::ensure_loaded();
    let enqueueable = crate::pipeline::try_get()
        .map(|p| {
            enqueueable_states_for(p)
                .iter()
                .map(|s| format!("'{}'", s))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| "'ready','requested'".to_string());
    let (mut conditions, params) = build_card_filters(
        "kc",
        body.repo.as_ref(),
        body.agent_id.as_ref(),
        requested_issue_numbers.as_ref(),
    );
    conditions.insert(0, format!("kc.status IN ({})", enqueueable));

    let where_clause = conditions.join(" AND ");
    let sql = format!(
        "SELECT kc.id, kc.assigned_agent_id, kc.priority, kc.description, kc.metadata, kc.github_issue_number
         FROM kanban_cards kc
         WHERE {where_clause}
         ORDER BY
           CASE kc.priority
             WHEN 'urgent' THEN 0
             WHEN 'high' THEN 1
             WHEN 'medium' THEN 2
             WHEN 'low' THEN 3
             ELSE 4
           END,
           kc.created_at ASC"
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let cards: Vec<GenerateCandidate> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(GenerateCandidate {
                card_id: row.get::<_, String>(0)?,
                agent_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                priority: row
                    .get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| "medium".to_string()),
                description: row.get::<_, Option<String>>(3)?,
                metadata: row.get::<_, Option<String>>(4)?,
                github_issue_number: row.get::<_, Option<i64>>(5)?,
                batch_phase: 0,
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    let mut cards = cards;

    if !requested_entry_meta.is_empty() {
        cards.sort_by_key(|card| {
            card.github_issue_number
                .and_then(|issue_number| requested_entry_meta.get(&issue_number).copied())
                .map(|(index, _)| index)
                .unwrap_or(usize::MAX)
        });
        for card in &mut cards {
            card.batch_phase = card
                .github_issue_number
                .and_then(|issue_number| requested_entry_meta.get(&issue_number).copied())
                .map(|(_, batch_phase)| batch_phase)
                .unwrap_or(0);
        }
    }

    if cards.is_empty() {
        // Provide context: how many cards are in backlog vs other statuses
        // Uses the same repo + agent_id filters as the main ready query
        let count_with_filters = |status_val: &str| -> i64 {
            let mut sql = format!(
                "SELECT COUNT(*) FROM kanban_cards WHERE status = '{}'",
                status_val
            );
            let mut count_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(ref repo) = body.repo {
                count_params.push(Box::new(repo.clone()));
                sql.push_str(&format!(" AND repo_id = ?{}", count_params.len()));
            }
            if let Some(ref agent_id) = body.agent_id {
                count_params.push(Box::new(agent_id.clone()));
                sql.push_str(&format!(" AND assigned_agent_id = ?{}", count_params.len()));
            }
            let refs: Vec<&dyn rusqlite::types::ToSql> =
                count_params.iter().map(|p| p.as_ref()).collect();
            conn.query_row(&sql, refs.as_slice(), |row| row.get(0))
                .unwrap_or(0)
        };
        // Pipeline-driven counts: report all non-terminal states
        let mut counts_map = serde_json::Map::new();
        if let Some(pipeline) = crate::pipeline::try_get() {
            for state in &pipeline.states {
                if !state.terminal {
                    let c: i64 = count_with_filters(&state.id);
                    counts_map.insert(state.id.clone(), serde_json::json!(c));
                }
            }
        }
        return (
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": "No dispatchable cards found",
                "hint": "Move cards to a dispatchable state before generating a queue.",
                "counts": counts_map,
            })),
        );
    }

    let mode = match GenerateMode::parse(body.mode.as_deref()) {
        Ok(mode) => mode,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
        }
    };

    // PM-assisted mode: send card list to PMD for async analysis
    if mode == GenerateMode::PmAssisted {
        // Create pending run
        let run_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, ai_model, ai_rationale, unified_thread) VALUES (?1, ?2, ?3, 'pending', 'pm-assisted', ?4, 0)",
            rusqlite::params![
                run_id,
                body.repo,
                body.agent_id,
                format!("PMD 분석 대기 중 — {}개 카드 제출", cards.len())
            ],
        )
        .ok();

        // Collect card info for PMD request
        let mut card_summaries = Vec::new();
        for card in &cards {
            let (title, issue_num): (String, Option<i64>) = conn
                .query_row(
                    "SELECT title, github_issue_number FROM kanban_cards WHERE id = ?1",
                    [&card.card_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or_default();
            card_summaries.push(format!(
                "- #{} {} (priority: {}, agent: {})",
                issue_num.unwrap_or(0),
                title,
                card.priority,
                card.agent_id
            ));
        }

        let run_id_for_spawn = run_id.clone();
        let card_list_text = card_summaries.join("\n");
        let repo_name = body.repo.clone().unwrap_or_else(|| "all".to_string());
        drop(stmt);
        let run = run_to_json(&conn, &run_id);
        drop(conn);

        // Async: send PMD request via announce bot
        tokio::spawn(async move {
            let token = match crate::credential::read_bot_token("announce") {
                Some(t) => t,
                None => return,
            };

            // Kanban manager channel from config (kv_meta)
            let km_channel: Option<String> = {
                let conn = state.db.separate_conn().ok();
                conn.and_then(|c| {
                    c.query_row(
                        "SELECT value FROM kv_meta WHERE key = 'kanban_manager_channel_id'",
                        [],
                        |row| row.get(0),
                    )
                    .ok()
                })
            };
            let Some(km_channel) = km_channel else {
                tracing::warn!(
                    "[auto-queue] No kanban_manager_channel_id configured, skipping PM request"
                );
                return;
            };

            // Resolve channel name to ID if needed
            let km_channel_num: u64 = match km_channel.parse() {
                Ok(n) => n,
                Err(_) => {
                    match crate::server::routes::dispatches::resolve_channel_alias_pub(&km_channel)
                    {
                        Some(n) => n,
                        None => return,
                    }
                }
            };

            let message = format!(
                "[칸반매니저] 자동큐 순서 분석 요청\n\n\
                 repo: {}\n\
                 run_id: {}\n\n\
                 아래 일감들의 실행 순서를 분석해주세요.\n\
                 의존관계, 긴급도, 작업 내용을 고려하여 순서를 결정하고,\n\
                 `POST /api/auto-queue/runs/{}/order`에 결과를 전달해주세요.\n\n\
                 {}",
                repo_name, run_id_for_spawn, run_id_for_spawn, card_list_text
            );

            let client = reqwest::Client::new();
            let _ = client
                .post(format!(
                    "https://discord.com/api/v10/channels/{km_channel_num}/messages"
                ))
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"content": message}))
                .send()
                .await;
        });

        return (
            StatusCode::OK,
            Json(json!({
                "run": run,
                "entries": [],
                "message": "PMD 분석 요청 전송됨. 응답 대기 중.",
            })),
        );
    }

    // Dependency-aware mode: filter out cards with incomplete dependencies.
    // SimilarityAware does NOT filter — it uses dependency edges only for
    // grouping/ordering inside build_group_plan() union-find, not exclusion.
    let (filtered_cards, excluded_count) = if mode == GenerateMode::DependencyAware {
        let mut filtered = Vec::new();
        let mut excluded = 0usize;
        for card in &cards {
            let dep_numbers = extract_dependency_numbers(card);
            let mut all_deps_done = true;
            for dep_num in &dep_numbers {
                let dep_status: Option<String> = conn
                    .query_row(
                        "SELECT status FROM kanban_cards WHERE github_issue_number = ?1",
                        [dep_num],
                        |row| row.get(0),
                    )
                    .ok();
                if dep_status.as_deref() != Some("done") {
                    all_deps_done = false;
                    break;
                }
            }

            if all_deps_done {
                filtered.push(card.clone());
            } else {
                excluded += 1;
            }
        }
        (filtered, excluded)
    } else {
        (cards.clone(), 0)
    };

    if filtered_cards.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": format!("No cards available ({}개 의존성 미충족으로 제외)", excluded_count)
            })),
        );
    }

    let force_sequential = body.parallel == Some(false);
    let auto_group = !force_sequential && mode.enables_auto_grouping(body.parallel);
    let analyzed_plan =
        auto_group.then(|| build_group_plan(&filtered_cards, mode.uses_similarity()));
    let max_concurrent = match analyzed_plan.as_ref() {
        Some(plan) => body
            .max_concurrent_threads
            .unwrap_or(plan.recommended_parallel_threads)
            .clamp(1, 10)
            .min(plan.thread_group_count.max(1)),
        None => 1,
    };

    let (
        grouped_entries,
        thread_group_count,
        recommended_parallel_threads,
        dependency_edges,
        similarity_edges,
        path_backed_card_count,
    ) = if let Some(plan) = analyzed_plan.as_ref() {
        (
            plan.entries.clone(),
            plan.thread_group_count,
            plan.recommended_parallel_threads,
            plan.dependency_edges,
            plan.similarity_edges,
            plan.path_backed_card_count,
        )
    } else {
        let result: Vec<PlannedEntry> = filtered_cards
            .iter()
            .enumerate()
            .map(|(idx, _)| PlannedEntry {
                card_idx: idx,
                thread_group: 0,
                priority_rank: idx as i64,
                reason: "순차 실행".to_string(),
            })
            .collect();
        (result, 1i64, 1i64, 0usize, 0usize, 0usize)
    };

    let ai_rationale = match (mode, auto_group, force_sequential) {
        (GenerateMode::SimilarityAware, _, true) => format!(
            "파일 경로 유사도 분석을 건너뛰고 순차 실행으로 고정 ({}개 카드)",
            filtered_cards.len()
        ),
        (GenerateMode::DependencyAware, _, true) => format!(
            "의존관계 기반 필터링 후 순차 실행. {}개 큐잉, {}개 의존성 미충족 제외",
            filtered_cards.len(),
            excluded_count
        ),
        (GenerateMode::PrioritySort, _, true) => format!(
            "우선순위 기반 순차 실행 (urgent > high > medium > low), {}개 카드 큐잉",
            filtered_cards.len()
        ),
        (GenerateMode::SimilarityAware, true, false) if path_backed_card_count == 0 => format!(
            "description/metadata에서 파일 경로를 찾지 못해 dependency-only 그룹으로 fallback. 의존성 {}건, {}개 그룹, 추천 병렬 {}개, 적용 {}개",
            dependency_edges, thread_group_count, recommended_parallel_threads, max_concurrent
        ),
        (GenerateMode::SimilarityAware, true, false) => format!(
            "파일 경로 유사도 {}건 + 의존성 {}건으로 {}개 그룹 생성. 파일 경로 추출 카드 {}개, 추천 병렬 {}개, 적용 {}개",
            similarity_edges,
            dependency_edges,
            thread_group_count,
            path_backed_card_count,
            recommended_parallel_threads,
            max_concurrent
        ),
        (GenerateMode::DependencyAware, true, false) => format!(
            "의존관계 기반 필터링 + dependency 그룹 분석. {}개 큐잉, {}개 의존성 미충족 제외, {}개 그룹, 적용 {}개",
            filtered_cards.len(),
            excluded_count,
            thread_group_count,
            max_concurrent
        ),
        (GenerateMode::PrioritySort, true, false) => format!(
            "의존성 기반 그룹 분석. {}개 카드, {}개 그룹, 추천 병렬 {}개, 적용 {}개",
            filtered_cards.len(),
            thread_group_count,
            recommended_parallel_threads,
            max_concurrent
        ),
        (GenerateMode::DependencyAware, false, false) => format!(
            "의존관계 기반 필터링 + 우선순위 정렬. {}개 큐잉, {}개 의존성 미충족 제외",
            filtered_cards.len(),
            excluded_count
        ),
        (GenerateMode::SimilarityAware, false, false) => format!(
            "유사도 분석 모드이지만 parallel 미지정으로 순차 실행. {}개 카드, {}개 의존성 미충족 제외",
            filtered_cards.len(),
            excluded_count
        ),
        (GenerateMode::PrioritySort, false, false) => format!(
            "우선순위 기반 정렬 (urgent > high > medium > low), {}개 카드 큐잉",
            filtered_cards.len()
        ),
        (GenerateMode::PmAssisted, _, _) => unreachable!("pm-assisted handled above"),
    };

    // Create run
    let run_id = uuid::Uuid::new_v4().to_string();
    let ai_model = match (mode, auto_group, force_sequential) {
        (GenerateMode::SimilarityAware, _, true) => "similarity-aware-sequential",
        (GenerateMode::SimilarityAware, true, false) => "similarity-aware-thread-group",
        (GenerateMode::SimilarityAware, false, false) => "similarity-aware-sequential",
        (GenerateMode::DependencyAware, true, false) => "dependency-aware-thread-group",
        (GenerateMode::DependencyAware, _, _) => "dependency-aware-sort",
        (GenerateMode::PrioritySort, true, false) => "parallel-thread-group",
        (GenerateMode::PrioritySort, _, _) => "priority-sort",
        (GenerateMode::PmAssisted, _, _) => unreachable!("pm-assisted handled above"),
    };
    let ai_model_str = ai_model.to_string();
    conn.execute(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status, ai_model, ai_rationale, unified_thread, max_concurrent_threads, thread_group_count) \
         VALUES (?1, ?2, ?3, 'generated', ?4, ?5, 0, ?6, ?7)",
        rusqlite::params![
            run_id,
            body.repo,
            body.agent_id,
            ai_model_str,
            ai_rationale,
            max_concurrent,
            thread_group_count
        ],
    )
    .ok();

    // Create entries
    let mut entries = Vec::new();
    for planned in &grouped_entries {
        let card = &filtered_cards[planned.card_idx];
        let entry_id = uuid::Uuid::new_v4().to_string();
        let agent = if card.agent_id.is_empty() {
            body.agent_id.as_deref().unwrap_or("")
        } else {
            card.agent_id.as_str()
        };
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, reason, batch_phase)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                entry_id,
                run_id,
                card.card_id,
                agent,
                planned.priority_rank,
                planned.thread_group,
                planned.reason,
                card.batch_phase
            ],
        )
        .ok();
        entries.push(entry_to_json(&conn, &entry_id));
    }

    let run = run_to_json(&conn, &run_id);

    (
        StatusCode::OK,
        Json(json!({ "run": run, "entries": entries })),
    )
}

/// POST /api/auto-queue/activate
/// Dispatches the next pending entry in the active run.
pub async fn activate(
    State(state): State<AppState>,
    Json(body): Json<ActivateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let _ignored_unified_thread = body.unified_thread.is_some();
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let active_only = body.active_only.unwrap_or(false);
    // Internal recovery paths must continue only active runs. Manual activation
    // may opt into promoting the latest generated draft.
    let mut run_filter = if active_only {
        "status = 'active'".to_string()
    } else {
        "status IN ('active', 'generated', 'pending')".to_string()
    };
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(ref repo) = body.repo {
        run_filter.push_str(&format!(
            " AND (repo = ?{} OR repo IS NULL OR repo = '')",
            params.len() + 1
        ));
        params.push(Box::new(repo.clone()));
    }
    if let Some(ref agent_id) = body.agent_id {
        run_filter.push_str(&format!(
            " AND (agent_id = ?{} OR agent_id IS NULL OR agent_id = '')",
            params.len() + 1
        ));
        params.push(Box::new(agent_id.clone()));
    }

    let run_id: Option<String> = if let Some(run_id) = body.run_id.clone() {
        Some(run_id)
    } else {
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        conn.query_row(
            &format!(
                "SELECT id FROM auto_queue_runs WHERE {run_filter} ORDER BY created_at DESC LIMIT 1"
            ),
            param_refs.as_slice(),
            |row| row.get(0),
        )
        .ok()
    };

    let Some(run_id) = run_id else {
        return (
            StatusCode::OK,
            Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
        );
    };

    if !active_only {
        // Promote pending/generated → active on explicit activation.
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'active' WHERE id = ?1 AND status IN ('generated', 'pending')",
            [&run_id],
        )
        .ok();
    }

    clear_inactive_slot_assignments(&conn);
    let completed_slots = completed_group_slots(&conn, &run_id);
    let mut cleared_slots: HashSet<(String, i64)> = HashSet::new();
    for (agent_id, slot_index) in &completed_slots {
        let cleared = clear_slot_threads_for_slot(&state, &conn, agent_id, *slot_index);
        if cleared > 0 {
            tracing::info!(
                "[auto-queue] cleared {cleared} slot thread session(s) before releasing {agent_id} slot {slot_index}"
            );
        }
        cleared_slots.insert((agent_id.clone(), *slot_index));
    }
    release_group_slots(&conn, &completed_slots);

    // Stale empty run cleanup: after generate()/enqueue() fixes, normal paths never
    // leave an active run with 0 entries.  Any such run is legacy corruption — complete
    // it immediately instead of auto-populating with unrelated ready cards (#85).
    let entry_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if entry_count == 0 {
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE id = ?1",
            [&run_id],
        ).ok();
        tracing::info!(
            "[auto-queue] Completed stale empty run {run_id} — no entries, skipping fallback populate (#85)"
        );
        return (
            StatusCode::OK,
            Json(
                json!({ "dispatched": [], "count": 0, "message": "Stale empty run completed — no entries to dispatch" }),
            ),
        );
    }

    // #179: When agent_id is known, apply cross-run in-flight guard to prevent
    // double-dispatch. Respects repo filter to avoid cross-repo dispatch.
    let effective_agent: Option<String> = body.agent_id.clone();

    // Build card-level repo condition for agent-scoped queries (#179).
    // Uses kanban_cards.repo_id instead of auto_queue_runs.repo to handle
    // mixed global runs (repo=NULL) that contain cards from multiple repos.
    let card_repo_condition = if body.repo.is_some() {
        " AND kc.repo_id = ?2"
    } else {
        ""
    };

    if let Some(ref agt) = effective_agent {
        // In-flight guard: any dispatched entry for this agent across active runs,
        // filtered by card's actual repo_id (not run-level repo).
        let inflight_query = format!(
            "SELECT COUNT(*) > 0 FROM auto_queue_entries e \
             JOIN auto_queue_runs r ON e.run_id = r.id \
             JOIN kanban_cards kc ON e.kanban_card_id = kc.id \
             WHERE e.agent_id = ?1 AND e.status = 'dispatched' AND r.status = 'active'{}",
            card_repo_condition
        );
        let has_inflight: bool = if let Some(ref repo) = body.repo {
            conn.query_row(&inflight_query, rusqlite::params![agt, repo], |row| {
                row.get(0)
            })
        } else {
            conn.query_row(&inflight_query, rusqlite::params![agt], |row| row.get(0))
        }
        .unwrap_or(false);
        if has_inflight {
            tracing::info!(
                "[auto-queue] Skipping activate: agent {agt} already has a dispatched entry in-flight"
            );
            return (
                StatusCode::OK,
                Json(
                    json!({ "dispatched": [], "count": 0, "message": "Already has in-flight entry" }),
                ),
            );
        }
    }

    // Slot pooling is always enabled. The legacy `unified_thread` field is
    // accepted at the API boundary for compatibility, but no longer affects runtime.
    let (max_concurrent, _thread_group_count): (i64, i64) = conn
        .query_row(
            "SELECT COALESCE(max_concurrent_threads, 1),
                    COALESCE(thread_group_count, 1)
             FROM auto_queue_runs
             WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((1, 1));
    let current_phase = current_batch_phase(&conn, &run_id);

    // Count currently active groups (groups with at least one 'dispatched' entry)
    let active_groups: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT COALESCE(thread_group, 0) FROM auto_queue_entries \
                 WHERE run_id = ?1 AND status = 'dispatched'",
            )
            .unwrap();
        stmt.query_map([&run_id], |row| row.get::<_, i64>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    let active_group_count = active_groups.len() as i64;

    // Find pending groups not currently active, ordered by group number
    let pending_groups: Vec<i64> = {
        let active_set: HashSet<i64> = active_groups.iter().copied().collect();
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT COALESCE(thread_group, 0), COALESCE(batch_phase, 0)
                 FROM auto_queue_entries
                 WHERE run_id = ?1 AND status = 'pending'
                 ORDER BY thread_group ASC, batch_phase ASC",
            )
            .unwrap();
        let mut seen = HashSet::new();
        stmt.query_map([&run_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .ok()
        .map(|rows| {
            rows.filter_map(|r| r.ok())
                .filter_map(|(thread_group, batch_phase)| {
                    (!active_set.contains(&thread_group)
                        && batch_phase_is_eligible(batch_phase, current_phase)
                        && seen.insert(thread_group))
                    .then_some(thread_group)
                })
                .collect()
        })
        .unwrap_or_default()
    };

    drop(conn);

    let mut dispatched = Vec::new();
    let mut groups_to_dispatch: Vec<i64> = Vec::new();
    let preferred_group = body.thread_group;

    if let Some(group) = preferred_group {
        let conn = state.db.separate_conn().unwrap();
        let has_pending = group_has_pending_entries(&conn, &run_id, group, current_phase);
        let has_dispatched: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0
                 FROM auto_queue_entries
                 WHERE run_id = ?1
                   AND COALESCE(thread_group, 0) = ?2
                   AND status = 'dispatched'",
                rusqlite::params![run_id, group],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if has_pending && !has_dispatched {
            groups_to_dispatch.push(group);
        }
    }

    {
        let conn = state.db.separate_conn().unwrap();
        for group in assigned_groups_with_pending_entries(&conn, &run_id, current_phase) {
            if !groups_to_dispatch.contains(&group) {
                groups_to_dispatch.push(group);
            }
        }
    }

    // Also dispatch next entry for active groups that have pending entries
    // (continuation within same group after prior entry completed)
    {
        let conn = state.db.separate_conn().unwrap();
        for &grp in &active_groups {
            let has_pending = group_has_pending_entries(&conn, &run_id, grp, current_phase);
            let has_dispatched: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM auto_queue_entries \
                     WHERE run_id = ?1 AND COALESCE(thread_group, 0) = ?2 AND status = 'dispatched'",
                    rusqlite::params![run_id, grp],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            // Only add group if it has pending entries AND no currently dispatched entries
            // (sequential within group)
            if has_pending && !has_dispatched {
                if !groups_to_dispatch.contains(&grp) {
                    groups_to_dispatch.push(grp);
                }
            }
        }
    }

    // Add new groups from available slots (dynamic — check remaining capacity)
    for &grp in &pending_groups {
        if (active_group_count + groups_to_dispatch.len() as i64) >= max_concurrent {
            break;
        }
        if !groups_to_dispatch.contains(&grp) {
            groups_to_dispatch.push(grp);
        }
    }

    for group in &groups_to_dispatch {
        // Get first pending entry in this group
        let conn = state.db.separate_conn().unwrap();
        let entry = first_pending_entry_for_group(&conn, &run_id, *group, current_phase);
        drop(conn);

        let Some((entry_id, card_id, agent_id)) = entry else {
            continue;
        };

        // Busy-agent guard (#110): skip if agent has active cards outside auto-queue.
        // Exclude the card being dispatched (#162) and cards that belong to the
        // same auto-queue run — those work in isolated worktrees so parallel
        // execution is safe.
        let conn = state.db.separate_conn().unwrap();
        let busy: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM kanban_cards \
                 WHERE assigned_agent_id = ?1 AND status IN ('requested', 'in_progress', 'review') \
                 AND id != ?2 \
                 AND id NOT IN (SELECT kanban_card_id FROM auto_queue_entries WHERE run_id = ?3)",
                rusqlite::params![agent_id, card_id, run_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        drop(conn);

        if busy {
            tracing::info!(
                "[auto-queue] Skipping activate for {agent_id}: agent has active cards outside auto-queue"
            );
            continue;
        }

        // #162: If card is in a non-dispatchable state (e.g. backlog, requested),
        // walk it through free transitions to the dispatchable state using the
        // canonical reducer path (preserves ApplyClock, AuditLog, SyncReviewState)
        // but without firing policy hooks that would create side-dispatches.
        {
            let conn = state.db.separate_conn().unwrap();
            let card_status: String = conn
                .query_row(
                    "SELECT status FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            let (card_repo_id, card_assigned_agent_id): (Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or_default();
            crate::pipeline::ensure_loaded();
            let effective = crate::pipeline::resolve_for_card(
                &conn,
                card_repo_id.as_deref(),
                card_assigned_agent_id.as_deref(),
            );
            drop(conn);
            if let Some(path) = effective.free_path_to_dispatchable(&card_status) {
                tracing::info!(
                    "[auto-queue] Silent walk: card {} from '{}' through {:?} (canonical reducer, no hooks)",
                    card_id,
                    card_status,
                    path
                );
                let mut walk_failed = false;
                for step in &path {
                    if let Err(e) = crate::kanban::transition_status_no_hooks(
                        &state.db,
                        &card_id,
                        step,
                        "auto-queue-walk",
                    ) {
                        tracing::warn!(
                            "[auto-queue] Silent walk failed for card {} at step '{}': {e}",
                            card_id,
                            step
                        );
                        walk_failed = true;
                        break;
                    }
                }
                if walk_failed {
                    continue;
                }
            }
        }

        // Get card title
        let conn = state.db.separate_conn().unwrap();
        let title: String = conn
            .query_row(
                "SELECT title FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "Dispatch".to_string());
        drop(conn);

        // Preserve the legacy JS preflight contract when activate() became the
        // authoritative dispatch path.
        {
            let conn = state.db.separate_conn().unwrap();
            let metadata: Option<String> = conn
                .query_row(
                    "SELECT metadata FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            drop(conn);

            if let Some(metadata) = metadata {
                if let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(&metadata) {
                    match parsed.get("preflight_status").and_then(|v| v.as_str()) {
                        Some("consult_required") => {
                            let consult_agent_id = {
                                let conn = state.db.separate_conn().unwrap();
                                let provider: String = conn
                                    .query_row(
                                        "SELECT COALESCE(cli_provider, 'claude') FROM agents WHERE id = ?1",
                                        [&agent_id],
                                        |row| row.get(0),
                                    )
                                    .unwrap_or_else(|_| "claude".to_string());
                                let counter_provider = if provider == "claude" {
                                    "codex"
                                } else {
                                    "claude"
                                };
                                conn.query_row(
                                    "SELECT id FROM agents WHERE cli_provider = ?1 LIMIT 1",
                                    [counter_provider],
                                    |row| row.get::<_, String>(0),
                                )
                                .unwrap_or_else(|_| agent_id.clone())
                            };

                            let dispatch_result = tokio::task::block_in_place(|| {
                                crate::dispatch::create_dispatch(
                                    &state.db,
                                    &state.engine,
                                    &card_id,
                                    &consult_agent_id,
                                    "consultation",
                                    &format!("[Consultation] {title}"),
                                    &json!({
                                        "auto_queue": true,
                                        "entry_id": entry_id,
                                        "thread_group": group,
                                    }),
                                )
                            });
                            if dispatch_result.is_err() {
                                tracing::warn!(
                                    "[auto-queue] consultation dispatch failed for entry {entry_id} (group {group})"
                                );
                                continue;
                            }

                            let dispatch_id = dispatch_result.as_ref().unwrap()["id"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            if let Some(obj) = parsed.as_object_mut() {
                                obj.insert(
                                    "consultation_status".to_string(),
                                    serde_json::json!("pending"),
                                );
                                obj.insert(
                                    "consultation_dispatch_id".to_string(),
                                    serde_json::json!(dispatch_id),
                                );
                            }

                            let conn = state.db.separate_conn().unwrap();
                            conn.execute(
                                "UPDATE kanban_cards SET metadata = ?1 WHERE id = ?2",
                                rusqlite::params![parsed.to_string(), card_id],
                            )
                            .ok();
                            conn.execute(
                                "UPDATE auto_queue_entries
                                 SET status = 'dispatched',
                                     dispatch_id = ?1,
                                     dispatched_at = datetime('now')
                                 WHERE id = ?2",
                                rusqlite::params![dispatch_id, entry_id],
                            )
                            .ok();
                            dispatched.push(entry_to_json(&conn, &entry_id));
                            drop(conn);
                            continue;
                        }
                        Some("invalid") | Some("already_applied") => {
                            let conn = state.db.separate_conn().unwrap();
                            conn.execute(
                                "UPDATE auto_queue_entries
                                 SET status = 'skipped',
                                     completed_at = datetime('now')
                                 WHERE id = ?1 AND status = 'pending'",
                                [&entry_id],
                            )
                            .ok();
                            drop(conn);
                            tracing::info!(
                                "[auto-queue] skipping entry {entry_id} for card {card_id} due to preflight_status={}",
                                parsed
                                    .get("preflight_status")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                            );
                            continue;
                        }
                        _ => {}
                    }
                }
            }
        }

        // Create dispatch
        let conn = state.db.separate_conn().unwrap();
        let slot_allocation = allocate_slot_for_group_agent(&conn, &run_id, *group, &agent_id);
        let slot_index = slot_allocation.as_ref().map(|(slot_index, _)| *slot_index);
        if slot_allocation.is_none() {
            tracing::info!(
                "[auto-queue] Skipping group {group} for {agent_id}: no free slot in pool"
            );
            continue;
        }
        if let Some((assigned_slot, _newly_assigned)) = slot_allocation {
            let slot_key = (agent_id.clone(), assigned_slot);
            if !cleared_slots.contains(&slot_key) {
                let clear_conn = state.db.separate_conn().unwrap();
                let cleared =
                    clear_slot_threads_for_slot(&state, &clear_conn, &agent_id, assigned_slot);
                if cleared > 0 {
                    tracing::info!(
                        "[auto-queue] cleared {cleared} slot thread session(s) before dispatching {agent_id} slot {assigned_slot} group {group}"
                    );
                }
                cleared_slots.insert(slot_key);
            }
        }

        let dispatch_result = tokio::task::block_in_place(|| {
            crate::dispatch::create_dispatch(
                &state.db,
                &state.engine,
                &card_id,
                &agent_id,
                "implementation",
                &title,
                &json!({
                    "auto_queue": true,
                    "entry_id": entry_id,
                    "thread_group": group,
                    "slot_index": slot_index,
                }),
            )
        });

        if dispatch_result.is_err() {
            tracing::error!(
                "[auto-queue] create_dispatch failed for entry {entry_id} (group {group}), leaving as pending for retry"
            );
            continue;
        }

        // Mark entry with dispatch_id (#145)
        let dispatch_id = dispatch_result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let conn = state.db.separate_conn().unwrap();
        conn.execute(
            "UPDATE auto_queue_entries
             SET status = 'dispatched',
                 dispatch_id = ?1,
                 slot_index = COALESCE(slot_index, ?2),
                 dispatched_at = datetime('now')
             WHERE id = ?3",
            rusqlite::params![dispatch_id, slot_index, entry_id],
        )
        .ok();
        drop(conn);

        let conn = state.db.separate_conn().unwrap();
        dispatched.push(entry_to_json(&conn, &entry_id));
        drop(conn);
    }

    // Check if all entries are done — include 'dispatched' to avoid premature run completion (#179)
    let conn = state.db.separate_conn().unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND status IN ('pending', 'dispatched')",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if remaining == 0 {
        release_run_slots(&conn, &run_id);
        let still_dispatched: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND status = 'dispatched'",
                [&run_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if still_dispatched == 0 {
            conn.execute(
                "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE id = ?1",
                [&run_id],
            )
            .ok();
        }
    }

    // Build response with group info
    let active_group_count = {
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(DISTINCT COALESCE(thread_group, 0)) FROM auto_queue_entries \
                 WHERE run_id = ?1 AND status = 'dispatched'",
            )
            .unwrap();
        stmt.query_row([&run_id], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
    };
    let pending_group_count = {
        let mut stmt = conn
            .prepare(
                "SELECT COUNT(DISTINCT COALESCE(thread_group, 0)) FROM auto_queue_entries \
                 WHERE run_id = ?1 AND status = 'pending'",
            )
            .unwrap();
        stmt.query_row([&run_id], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
    };

    (
        StatusCode::OK,
        Json(json!({
            "dispatched": dispatched,
            "count": dispatched.len(),
            "active_groups": active_group_count,
            "pending_groups": pending_group_count,
        })),
    )
}

/// GET /api/auto-queue/status
pub async fn status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    // Find latest run (NULL agent_id/repo matches any filter)
    let mut run_filter = "1=1".to_string();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(ref repo) = query.repo {
        run_filter.push_str(&format!(
            " AND (repo = ?{} OR repo IS NULL OR repo = '')",
            params.len() + 1
        ));
        params.push(Box::new(repo.clone()));
    }
    if let Some(ref agent_id) = query.agent_id {
        run_filter.push_str(&format!(
            " AND (agent_id = ?{} OR agent_id IS NULL OR agent_id = '')",
            params.len() + 1
        ));
        params.push(Box::new(agent_id.clone()));
    }

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let run_id: Option<String> = conn
        .query_row(
            &format!(
                "SELECT id FROM auto_queue_runs WHERE {run_filter} ORDER BY created_at DESC LIMIT 1"
            ),
            param_refs.as_slice(),
            |row| row.get(0),
        )
        .ok();

    let Some(run_id) = run_id else {
        return (
            StatusCode::OK,
            Json(json!({ "run": null, "entries": [], "agents": {} })),
        );
    };

    let run = run_to_json(&conn, &run_id);

    // Get entries (filtered by agent_id and repo if specified)
    let entry_ids: Vec<String> = {
        let mut entry_sql = String::from(
            "SELECT e.id FROM auto_queue_entries e \
             LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id \
             WHERE e.run_id = ?1",
        );
        let mut entry_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(run_id.clone())];
        if let Some(ref agent_id) = query.agent_id {
            entry_sql.push_str(&format!(" AND e.agent_id = ?{}", entry_params.len() + 1));
            entry_params.push(Box::new(agent_id.clone()));
        }
        if let Some(ref repo) = query.repo {
            entry_sql.push_str(&format!(" AND kc.repo_id = ?{}", entry_params.len() + 1));
            entry_params.push(Box::new(repo.clone()));
        }
        entry_sql.push_str(" ORDER BY e.priority_rank ASC");

        let entry_refs: Vec<&dyn rusqlite::types::ToSql> =
            entry_params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&entry_sql).unwrap();
        stmt.query_map(entry_refs.as_slice(), |row| row.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    let entries: Vec<serde_json::Value> = entry_ids
        .iter()
        .map(|id| entry_to_json(&conn, id))
        .collect();

    // Agent summary
    let mut agents: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for entry in &entries {
        let agent = entry["agent_id"].as_str().unwrap_or("unknown").to_string();
        let entry_status = entry["status"].as_str().unwrap_or("pending");
        let counter = agents
            .entry(agent)
            .or_insert_with(|| json!({"pending": 0, "dispatched": 0, "done": 0, "skipped": 0}));
        if let Some(obj) = counter.as_object_mut() {
            if let Some(val) = obj.get_mut(entry_status) {
                *val = json!(val.as_i64().unwrap_or(0) + 1);
            }
        }
    }

    // #140: Thread group summary
    let mut thread_groups: std::collections::HashMap<i64, serde_json::Value> =
        std::collections::HashMap::new();
    for entry in &entries {
        let group = entry["thread_group"].as_i64().unwrap_or(0);
        let entry_status = entry["status"].as_str().unwrap_or("pending");
        let counter = thread_groups.entry(group).or_insert_with(|| {
            json!({
                "pending": 0,
                "dispatched": 0,
                "done": 0,
                "skipped": 0,
                "entries": [],
                "reason": serde_json::Value::Null,
            })
        });
        if let Some(obj) = counter.as_object_mut() {
            if let Some(val) = obj.get_mut(entry_status) {
                *val = json!(val.as_i64().unwrap_or(0) + 1);
            }
            if obj
                .get("reason")
                .and_then(|value| value.as_str())
                .map(|value| value.is_empty())
                .unwrap_or(true)
            {
                if let Some(reason) = entry["reason"].as_str() {
                    obj.insert("reason".to_string(), json!(reason));
                }
            }
            if let Some(arr) = obj.get_mut("entries").and_then(|v| v.as_array_mut()) {
                arr.push(json!({
                    "id": entry["id"],
                    "card_id": entry["card_id"],
                    "status": entry_status,
                    "github_issue_number": entry["github_issue_number"],
                    "batch_phase": entry["batch_phase"],
                }));
            }
        }
    }

    // Determine group-level statuses
    let mut group_statuses: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for (group_num, summary) in &thread_groups {
        let dispatched_count = summary["dispatched"].as_i64().unwrap_or(0);
        let pending_count = summary["pending"].as_i64().unwrap_or(0);
        let group_status = if dispatched_count > 0 {
            "active"
        } else if pending_count > 0 {
            "pending"
        } else {
            "done"
        };
        let mut group_obj = summary.clone();
        group_obj["status"] = json!(group_status);
        group_statuses.insert(group_num.to_string(), group_obj);
    }

    (
        StatusCode::OK,
        Json(json!({
            "run": run,
            "entries": entries,
            "agents": agents,
            "thread_groups": group_statuses,
        })),
    )
}

/// PATCH /api/auto-queue/entries/{id}/skip
pub async fn skip_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let changed = conn
        .execute(
            "UPDATE auto_queue_entries SET status = 'skipped', completed_at = datetime('now') WHERE id = ?1 AND status = 'pending'",
            [&id],
        )
        .unwrap_or(0);

    if changed == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "entry not found or not pending"})),
        );
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// PATCH /api/auto-queue/runs/{id}
pub async fn update_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRunBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let mut changed = 0usize;

    if let Some(ref status) = body.status {
        let completed_at = if status == "completed" {
            "datetime('now')"
        } else {
            "NULL"
        };
        changed += conn
            .execute(
                &format!(
                    "UPDATE auto_queue_runs SET status = ?1, completed_at = {completed_at} WHERE id = ?2"
                ),
                rusqlite::params![status, id],
            )
            .unwrap_or(0);
    }

    let ignored_unified_thread = body.unified_thread.is_some();
    if changed == 0 && body.status.is_none() && !ignored_unified_thread {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "ignored": ignored_unified_thread.then_some(vec!["unified_thread"]),
        })),
    )
}

/// POST /api/auto-queue/slots/{agent_id}/{slot_index}/reset-thread
pub async fn reset_slot_thread(
    State(state): State<AppState>,
    Path((agent_id, slot_index)): Path<(String, i64)>,
) -> (StatusCode, Json<serde_json::Value>) {
    match reset_slot_thread_bindings(&state.db, &agent_id, slot_index).await {
        Ok((archived_threads, cleared_sessions, cleared_bindings)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "agent_id": agent_id,
                "slot_index": slot_index,
                "archived_threads": archived_threads,
                "cleared_sessions": cleared_sessions,
                "cleared_bindings": cleared_bindings,
            })),
        ),
        Err(err) if err.contains("has active dispatch") => {
            (StatusCode::CONFLICT, Json(json!({"error": err})))
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err})),
        ),
    }
}

/// POST /api/auto-queue/reset
/// Clear all entries and complete all non-terminal runs.
pub async fn reset(
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let body: ResetBody = if body.is_empty() {
        ResetBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid reset body: {error}")})),
                );
            }
        }
    };

    let agent_id = body
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let (deleted_entries, completed_runs, protected_active_runs, warning) = if let Some(agent_id) =
        agent_id
    {
        let deleted_entries = conn
            .execute(
                "DELETE FROM auto_queue_entries WHERE agent_id = ?1",
                rusqlite::params![agent_id],
            )
            .unwrap_or(0);
        let completed_runs = conn
                .execute(
                    "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') \
                     WHERE status IN ('generated', 'pending', 'active', 'paused') AND agent_id = ?1",
                    rusqlite::params![agent_id],
                )
                .unwrap_or(0);
        (deleted_entries, completed_runs, 0usize, None)
    } else {
        let protected_active_runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'active'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if protected_active_runs > 0 {
            tracing::warn!(
                "[auto-queue] Global reset requested without agent_id; preserving {protected_active_runs} active run(s)"
            );
        } else {
            tracing::warn!(
                "[auto-queue] Global reset requested without agent_id; applying unscoped reset"
            );
        }

        let deleted_entries = if protected_active_runs > 0 {
            conn.execute(
                "DELETE FROM auto_queue_entries \
                     WHERE run_id IS NULL \
                        OR run_id NOT IN (SELECT id FROM auto_queue_runs WHERE status = 'active')",
                [],
            )
            .unwrap_or(0)
        } else {
            conn.execute("DELETE FROM auto_queue_entries", [])
                .unwrap_or(0)
        };
        let completed_runs = if protected_active_runs > 0 {
            conn.execute(
                "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') \
                     WHERE status IN ('generated', 'pending', 'paused')",
                [],
            )
            .unwrap_or(0)
        } else {
            conn.execute(
                "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') \
                     WHERE status IN ('generated', 'pending', 'active', 'paused')",
                [],
            )
            .unwrap_or(0)
        };
        let warning = (protected_active_runs > 0).then(|| {
                format!(
                    "global reset preserved {protected_active_runs} active run(s); use agent_id to reset a specific queue"
                )
            });
        (
            deleted_entries,
            completed_runs,
            protected_active_runs as usize,
            warning,
        )
    };

    let mut response = json!({
        "ok": true,
        "deleted_entries": deleted_entries,
        "completed_runs": completed_runs,
        "protected_active_runs": protected_active_runs,
    });
    if let Some(warning) = warning {
        response["warning"] = json!(warning);
    }
    (StatusCode::OK, Json(response))
}

/// POST /api/auto-queue/pause — pause all active runs
pub async fn pause(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let paused = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'paused' WHERE status = 'active'",
            [],
        )
        .unwrap_or(0);
    (
        StatusCode::OK,
        Json(json!({"ok": true, "paused_runs": paused})),
    )
}

/// POST /api/auto-queue/resume — resume paused runs and dispatch next entry
pub async fn resume_run(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let resumed = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'active' WHERE status = 'paused'",
            [],
        )
        .unwrap_or(0);
    drop(conn);

    // Trigger dispatch of next pending entry
    if resumed > 0 {
        let (_status, body) = activate(
            State(state),
            Json(ActivateBody {
                run_id: None,
                repo: None,
                agent_id: None,
                thread_group: None,
                unified_thread: None,
                active_only: Some(true),
            }),
        )
        .await;
        let dispatched = body.0.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "resumed_runs": resumed, "dispatched": dispatched})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({"ok": true, "resumed_runs": 0, "message": "No paused runs"})),
    )
}

/// POST /api/auto-queue/cancel — cancel all active/paused runs and pending entries
pub async fn cancel(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let cancelled_entries = conn
        .execute(
            "UPDATE auto_queue_entries SET status = 'skipped' WHERE status IN ('pending', 'dispatched')",
            [],
        )
        .unwrap_or(0);
    let cancelled_runs = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'cancelled', completed_at = datetime('now') WHERE status IN ('active', 'paused')",
            [],
        )
        .unwrap_or(0);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "cancelled_entries": cancelled_entries,
            "cancelled_runs": cancelled_runs,
        })),
    )
}

/// PATCH /api/auto-queue/reorder
pub async fn reorder(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let run_id = body.ordered_ids.iter().find_map(|id| {
        conn.query_row(
            "SELECT run_id FROM auto_queue_entries WHERE id = ?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    });
    let Some(run_id) = run_id else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no matching queue entries found"})),
        );
    };

    let current_entries: Vec<QueueEntryOrder> = {
        let mut stmt = match conn.prepare(
            "SELECT id, COALESCE(status, 'pending'), COALESCE(agent_id, '')
             FROM auto_queue_entries
             WHERE run_id = ?1
             ORDER BY priority_rank ASC, created_at ASC, id ASC",
        ) {
            Ok(stmt) => stmt,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        stmt.query_map([&run_id], |row| {
            Ok(QueueEntryOrder {
                id: row.get(0)?,
                status: row.get(1)?,
                agent_id: row.get(2)?,
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|row| row.ok()).collect())
        .unwrap_or_default()
    };

    let reordered_ids = match reorder_entry_ids(
        &current_entries,
        &body.ordered_ids,
        body.agent_id.as_deref(),
    ) {
        Ok(ids) => ids,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
        }
    };

    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    for (rank, id) in reordered_ids.iter().enumerate() {
        if let Err(e) = tx.execute(
            "UPDATE auto_queue_entries SET priority_rank = ?1 WHERE id = ?2",
            rusqlite::params![rank as i64, id],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    if let Err(e) = tx.commit() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// POST /api/auto-queue/enqueue
pub async fn enqueue(
    State(state): State<AppState>,
    Json(body): Json<EnqueueBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    // Resolve agent_id
    let agent_id = match body.agent_id {
        Some(ref id) if !id.is_empty() => id.clone(),
        _ => match conn.query_row(
            "SELECT default_agent_id FROM github_repos WHERE full_name = ?1",
            [&body.repo],
            |row| row.get::<_, Option<String>>(0),
        ) {
            Ok(Some(id)) if !id.is_empty() => id,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "no agent_id provided and repo has no default_agent_id"})),
                );
            }
        },
    };

    // Find or create kanban card
    let card_id: Option<String> = conn
        .query_row(
            "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
            rusqlite::params![body.issue_number, body.repo],
            |row| row.get(0),
        )
        .ok();

    let card_id = match card_id {
        Some(id) => id,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "kanban card not found for this issue"})),
            );
        }
    };

    let (card_status, card_repo_id, card_assigned_agent_id): (
        String,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, repo_id, assigned_agent_id FROM kanban_cards WHERE id = ?1",
            [&card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or_default();

    // Complete stale active/pending runs that no longer have actionable entries.
    // A finished run must not silently absorb new work just because its status
    // was left behind as active/pending.
    conn.execute(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = COALESCE(completed_at, datetime('now'))
         WHERE id IN (
             SELECT r.id
             FROM auto_queue_runs r
             WHERE r.status IN ('active', 'pending')
               AND (r.repo = ?1 OR r.repo IS NULL)
               AND (r.agent_id = ?2 OR r.agent_id IS NULL)
               AND NOT EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.run_id = r.id
                     AND e.status IN ('pending', 'dispatched')
               )
         )",
        rusqlite::params![body.repo, agent_id],
    )
    .ok();

    // Find existing live active/pending run (do NOT create yet — preserves idempotent retry)
    let existing_run_id: Option<String> = conn
        .query_row(
            "SELECT r.id
             FROM auto_queue_runs r
             WHERE r.status IN ('active', 'pending')
               AND (r.repo = ?1 OR r.repo IS NULL)
               AND (r.agent_id = ?2 OR r.agent_id IS NULL)
               AND EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.run_id = r.id
                     AND e.status IN ('pending', 'dispatched')
               )
             ORDER BY r.created_at DESC
             LIMIT 1",
            rusqlite::params![body.repo, agent_id],
            |row| row.get(0),
        )
        .ok();

    // Check if already in queue (idempotent retry) — must run BEFORE status validation
    if let Some(ref rid) = existing_run_id {
        let already: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND kanban_card_id = ?2 AND status = 'pending'",
                rusqlite::params![rid, card_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;

        if already {
            return (
                StatusCode::OK,
                Json(
                    json!({"ok": true, "card_id": card_id, "agent_id": agent_id, "already_queued": true}),
                ),
            );
        }
    }

    // Never enqueue a card that already has an active dispatch. Previously the
    // caller force-transitioned such cards to ready first; with direct enqueue,
    // we must keep that guard here.
    let has_active_dispatch: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')",
            [&card_id],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if has_active_dispatch {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "card already has an active dispatch; cannot enqueue duplicate work",
                "card_id": card_id,
                "status": card_status,
            })),
        );
    }

    // Accept only prepared staging states directly so PMD does not need
    // redundant state nudges before enqueueing work.
    crate::pipeline::ensure_loaded();
    let effective_pipeline = crate::pipeline::resolve_for_card(
        &conn,
        card_repo_id.as_deref(),
        card_assigned_agent_id.as_deref(),
    );
    let enqueueable_states = enqueueable_states_for(&effective_pipeline);
    if !enqueueable_states.iter().any(|s| s == &card_status) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("card status is '{}', only ready/requested/dispatchable states can be enqueued", card_status),
                "card_id": card_id,
                "status": card_status,
                "allowed_states": enqueueable_states,
            })),
        );
    }

    // Enqueue only appends to an already-live run. Creating a brand-new pending
    // run here is confusing because nothing dispatches until a separate activate.
    // Reject instead so callers explicitly generate/activate a queue first.
    let run_id = match existing_run_id {
        Some(id) => id,
        None => {
            let last_run: Option<(String, String)> = conn
                .query_row(
                    "SELECT id, status
                     FROM auto_queue_runs
                     WHERE (repo = ?1 OR repo IS NULL)
                       AND (agent_id = ?2 OR agent_id IS NULL)
                     ORDER BY created_at DESC
                     LIMIT 1",
                    rusqlite::params![body.repo, agent_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "no live active/pending auto-queue run available; completed runs cannot accept enqueue. Generate or activate a queue first",
                    "card_id": card_id,
                    "agent_id": agent_id,
                    "last_run_id": last_run.as_ref().map(|(id, _)| id.clone()),
                    "last_run_status": last_run.as_ref().map(|(_, status)| status.clone()),
                })),
            );
        }
    };

    let entry_id = uuid::Uuid::new_v4().to_string();
    let max_rank: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(priority_rank), -1) FROM auto_queue_entries WHERE run_id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    conn.execute(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![entry_id, run_id, card_id, agent_id, max_rank + 1],
    )
    .ok();

    (
        StatusCode::OK,
        Json(json!({"ok": true, "card_id": card_id, "agent_id": agent_id})),
    )
}

// ── PM-assisted callback ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OrderBody {
    /// Ordered list of GitHub issue numbers (or card IDs)
    pub order: Vec<serde_json::Value>,
    pub rationale: Option<String>,
    /// Alias for rationale (compatibility)
    pub reasoning: Option<String>,
}

/// POST /api/auto-queue/runs/:id/order
/// Callback from PMD: provides the ordered card list for a pending run.
pub async fn submit_order(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<OrderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    // Verify run exists and is pending, get repo for filtering
    let run_info: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT status, repo FROM auto_queue_runs WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    match run_info.as_ref().map(|(s, _)| s.as_str()) {
        Some("pending") => {}
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "run not found or not pending"})),
            );
        }
    }
    let run_repo = run_info.as_ref().and_then(|(_, r)| r.clone());

    // Create entries from the ordered list
    let mut created = 0;
    for (rank, item) in body.order.iter().enumerate() {
        // Item can be issue number (i64) or card_id (string)
        // When matching by issue number, filter by repo to prevent cross-repo collisions
        let card_id: Option<String> = if let Some(num) = item.as_i64() {
            if let Some(ref repo) = run_repo {
                conn.query_row(
                    "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
                    rusqlite::params![num, repo],
                    |row| row.get(0),
                )
                .ok()
            } else {
                conn.query_row(
                    "SELECT id FROM kanban_cards WHERE github_issue_number = ?1",
                    [num],
                    |row| row.get(0),
                )
                .ok()
            }
        } else if let Some(id) = item.as_str() {
            Some(id.to_string())
        } else {
            None
        };

        let Some(card_id) = card_id else { continue };

        // Only enqueue cards in dispatchable states (pipeline-driven)
        let card_status: String = conn
            .query_row(
                "SELECT COALESCE(status, '') FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            )
            .unwrap_or_default();
        let dispatchable_check = crate::pipeline::try_get()
            .map(|p| p.dispatchable_states().iter().any(|s| *s == card_status))
            .unwrap_or(card_status == "ready");
        if !dispatchable_check {
            tracing::info!(
                "[auto-queue] Skipping card {card_id} (status={card_status}, not dispatchable)"
            );
            continue;
        }

        let agent_id: String = conn
            .query_row(
                "SELECT COALESCE(assigned_agent_id, '') FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            )
            .unwrap_or_default();

        let entry_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![entry_id, run_id, card_id, agent_id, rank as i64],
        )
        .ok();
        created += 1;
    }

    // Only activate if at least one card was enqueued; otherwise leave as pending
    // to prevent the activate() fallback from filling the run with unintended cards
    let rationale = body
        .rationale
        .as_deref()
        .or(body.reasoning.as_deref())
        .unwrap_or("PMD 분석 완료");
    if created > 0 {
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'active', ai_rationale = ?1 WHERE id = ?2",
            rusqlite::params![rationale, run_id],
        )
        .ok();
    } else {
        tracing::warn!(
            "[auto-queue] submit_order: no ready cards enqueued, run {run_id} stays pending"
        );
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'completed', ai_rationale = ?1 WHERE id = ?2",
            rusqlite::params![
                format!("{rationale} (no ready cards — auto-completed)"),
                run_id
            ],
        )
        .ok();
    }

    // Queue created and activated — dispatch is a separate step via POST /api/auto-queue/activate
    // This allows PMD to review/adjust the order before dispatching begins.
    drop(conn);

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "created": created,
            "run_id": run_id,
            "message": "Queue active. Call POST /api/auto-queue/activate to start dispatching.",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::{QueueEntryOrder, reorder_entry_ids};

    fn entry(id: &str, status: &str, agent_id: &str) -> QueueEntryOrder {
        QueueEntryOrder {
            id: id.to_string(),
            status: status.to_string(),
            agent_id: agent_id.to_string(),
        }
    }

    #[test]
    fn reorder_entry_ids_reorders_only_pending_entries_in_scope() {
        let entries = vec![
            entry("done-a", "done", "agent-a"),
            entry("a-1", "pending", "agent-a"),
            entry("b-1", "pending", "agent-b"),
            entry("a-2", "pending", "agent-a"),
            entry("done-b", "done", "agent-b"),
        ];

        let reordered = reorder_entry_ids(
            &entries,
            &["a-2".to_string(), "a-1".to_string()],
            Some("agent-a"),
        )
        .expect("agent reorder should succeed");

        assert_eq!(
            reordered,
            vec![
                "done-a".to_string(),
                "a-2".to_string(),
                "b-1".to_string(),
                "a-1".to_string(),
                "done-b".to_string(),
            ]
        );
    }

    #[test]
    fn reorder_entry_ids_filters_non_pending_ids_from_legacy_payloads() {
        let entries = vec![
            entry("done-a", "done", "agent-a"),
            entry("p-1", "pending", "agent-a"),
            entry("p-2", "pending", "agent-a"),
            entry("done-b", "done", "agent-a"),
        ];

        let reordered = reorder_entry_ids(
            &entries,
            &[
                "done-a".to_string(),
                "p-2".to_string(),
                "p-1".to_string(),
                "done-b".to_string(),
            ],
            None,
        )
        .expect("legacy payload should still reorder pending entries");

        assert_eq!(
            reordered,
            vec![
                "done-a".to_string(),
                "p-2".to_string(),
                "p-1".to_string(),
                "done-b".to_string(),
            ]
        );
    }
}
