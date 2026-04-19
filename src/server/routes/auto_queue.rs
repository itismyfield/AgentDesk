use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder, Row as SqlxRow};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use super::AppState;
use crate::services::{auto_queue::AutoQueueLogContext, provider::ProviderKind};

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct GenerateEntryBody {
    pub issue_number: i64,
    pub batch_phase: Option<i64>,
    pub thread_group: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct GenerateBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub issue_numbers: Option<Vec<i64>>,
    pub entries: Option<Vec<GenerateEntryBody>>,
    // Legacy compatibility only. Accepted from callers, but ignored.
    #[allow(dead_code)]
    pub mode: Option<String>,
    pub unified_thread: Option<bool>,
    // Legacy compatibility only. Accepted from callers, but ignored.
    #[allow(dead_code)]
    pub parallel: Option<bool>,
    pub max_concurrent_threads: Option<i64>,
    pub force: Option<bool>,
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
pub struct HistoryQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub limit: Option<usize>,
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
    pub deploy_phases: Option<Vec<i64>>,
    pub max_concurrent_threads: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct DispatchGroupBody {
    pub issues: Vec<i64>,
    pub sequential: Option<bool>,
    pub batch_phase: Option<i64>,
    pub thread_group: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct DispatchBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub groups: Vec<DispatchGroupBody>,
    pub unified_thread: Option<bool>,
    pub activate: Option<bool>,
    pub auto_assign_agent: Option<bool>,
    pub max_concurrent_threads: Option<i64>,
    pub deploy_phases: Option<Vec<i64>>,
    pub force: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateEntryBody {
    pub thread_group: Option<i64>,
    pub priority_rank: Option<i64>,
    pub batch_phase: Option<i64>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RebindSlotBody {
    pub run_id: String,
    pub thread_group: i64,
}

#[derive(Debug, Deserialize)]
pub struct AddRunEntryBody {
    pub issue_number: i64,
    pub thread_group: Option<i64>,
    pub batch_phase: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResetBody {
    pub agent_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CancelQuery {
    pub run_id: Option<String>,
}

#[derive(Debug, Clone)]
struct GenerateCandidate {
    card_id: String,
    agent_id: String,
    priority: String,
    description: Option<String>,
    metadata: Option<String>,
    github_issue_number: Option<i64>,
}

#[derive(Debug, Clone)]
struct PlannedEntry {
    card_idx: usize,
    thread_group: i64,
    priority_rank: i64,
    batch_phase: i64,
    reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DependencyParseResult {
    numbers: Vec<i64>,
    signals: Vec<String>,
}

fn deploy_phase_api_enabled(state: &AppState) -> bool {
    state
        .config
        .server
        .auth_token
        .as_deref()
        .map(|token| !token.trim().is_empty())
        .unwrap_or(false)
}

fn slot_thread_map_has_bindings(
    conn: &libsql_rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
) -> bool {
    let raw_map: Option<String> = conn
        .query_row(
            "SELECT thread_id_map
             FROM auto_queue_slots
             WHERE agent_id = ?1 AND slot_index = ?2",
            libsql_rusqlite::params![agent_id, slot_index],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    raw_map
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .map(|map| {
            map.values().any(|value| {
                value
                    .as_str()
                    .map(|raw| !raw.trim().is_empty())
                    .or_else(|| value.as_u64().map(|_| true))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn load_slot_bindings_for_runs(
    conn: &libsql_rusqlite::Connection,
    run_ids: &[String],
) -> libsql_rusqlite::Result<Vec<(String, String, i64)>> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat("?")
        .take(run_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT DISTINCT assigned_run_id, agent_id, slot_index
         FROM auto_queue_slots
         WHERE assigned_run_id IN ({placeholders})
           AND assigned_run_id IS NOT NULL"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(libsql_rusqlite::params_from_iter(run_ids.iter()), |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?
    .collect::<Result<Vec<_>, _>>()
}

fn slot_has_dispatch_thread_history(
    conn: &libsql_rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0
         FROM task_dispatches
         WHERE to_agent_id = ?1
           AND thread_id IS NOT NULL
           AND TRIM(thread_id) != ''
           AND CASE
                 WHEN context IS NULL OR TRIM(context) = '' OR json_valid(context) = 0
                     THEN NULL
                 ELSE CAST(json_extract(context, '$.slot_index') AS INTEGER)
               END = ?2",
        libsql_rusqlite::params![agent_id, slot_index],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn slot_requires_thread_reset_before_reuse(
    conn: &libsql_rusqlite::Connection,
    agent_id: &str,
    slot_index: i64,
    newly_assigned: bool,
    reassigned_from_other_group: bool,
) -> bool {
    (newly_assigned || reassigned_from_other_group)
        && (slot_thread_map_has_bindings(conn, agent_id, slot_index)
            || slot_has_dispatch_thread_history(conn, agent_id, slot_index))
}

async fn slot_thread_map_has_bindings_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<bool, String> {
    let raw_map = sqlx::query_scalar::<_, Option<String>>(
        "SELECT COALESCE(thread_id_map::text, '{}')
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot thread map for {agent_id}:{slot_index}: {error}"))?
    .flatten()
    .unwrap_or_else(|| "{}".to_string());

    Ok(serde_json::from_str::<serde_json::Value>(&raw_map)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .map(|map| {
            map.values().any(|value| {
                value
                    .as_str()
                    .map(|raw| !raw.trim().is_empty())
                    .or_else(|| value.as_u64().map(|_| true))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false))
}

async fn slot_has_dispatch_thread_history_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<bool, String> {
    let rows = sqlx::query(
        "SELECT thread_id, context
         FROM task_dispatches
         WHERE to_agent_id = $1
           AND thread_id IS NOT NULL
           AND BTRIM(thread_id) != ''",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!("load postgres dispatch thread history for {agent_id}:{slot_index}: {error}")
    })?;

    for row in rows {
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
            == Some(slot_index)
        {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn slot_requires_thread_reset_before_reuse_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
    slot_index: i64,
    newly_assigned: bool,
    reassigned_from_other_group: bool,
) -> Result<bool, String> {
    if !(newly_assigned || reassigned_from_other_group) {
        return Ok(false);
    }

    Ok(
        slot_thread_map_has_bindings_pg(pool, agent_id, slot_index).await?
            || slot_has_dispatch_thread_history_pg(pool, agent_id, slot_index).await?,
    )
}

fn build_auto_queue_dispatch_context(
    entry_id: &str,
    thread_group: i64,
    slot_index: Option<i64>,
    reset_slot_thread_before_reuse: bool,
    extra_fields: impl IntoIterator<Item = (&'static str, serde_json::Value)>,
) -> serde_json::Value {
    let mut context = serde_json::Map::new();
    context.insert("auto_queue".to_string(), json!(true));
    context.insert("entry_id".to_string(), json!(entry_id));
    context.insert("thread_group".to_string(), json!(thread_group));
    context.insert("slot_index".to_string(), json!(slot_index));
    if reset_slot_thread_before_reuse {
        context.insert(
            "reset_slot_thread_before_reuse".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    for (key, value) in extra_fields {
        context.insert(key.to_string(), value);
    }
    serde_json::Value::Object(context)
}

fn resolve_activate_dispatch_channel_id(channel: &str) -> Option<u64> {
    channel
        .parse::<u64>()
        .ok()
        .or_else(|| crate::server::routes::dispatches::resolve_channel_alias_pub(channel))
}

async fn group_has_dispatched_entries_pg(
    pool: &sqlx::PgPool,
    run_id: &str,
    thread_group: i64,
) -> Result<bool, String> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND COALESCE(thread_group, 0) = $2
           AND status = 'dispatched'",
    )
    .bind(run_id)
    .bind(thread_group)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        format!("count dispatched postgres auto-queue entries for {run_id}:{thread_group}: {error}")
    })?;
    Ok(count > 0)
}

async fn create_activate_dispatch_pg(
    deps: &AutoQueueActivateDeps,
    pool: &sqlx::PgPool,
    card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<String, String> {
    if dispatch_type != "review-decision"
        && let Some(existing_id) = sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = $2
               AND status IN ('pending', 'dispatched')
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(card_id)
        .bind(dispatch_type)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("lookup active postgres dispatch for {card_id}: {error}"))?
    {
        return Ok(existing_id);
    }

    let row = sqlx::query(
        "SELECT status,
                review_status,
                latest_dispatch_id,
                repo_id,
                assigned_agent_id,
                github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch card {card_id}: {error}"))?
    .ok_or_else(|| format!("card {card_id} not found"))?;

    let old_status: String = row
        .try_get("status")
        .map_err(|error| format!("decode old status for {card_id}: {error}"))?;
    let review_status: Option<String> = row
        .try_get("review_status")
        .map_err(|error| format!("decode review_status for {card_id}: {error}"))?;
    let latest_dispatch_id: Option<String> = row
        .try_get("latest_dispatch_id")
        .map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?;
    let repo_id: Option<String> = row
        .try_get("repo_id")
        .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?;
    let assigned_agent_id: Option<String> = row
        .try_get("assigned_agent_id")
        .map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?;
    let github_issue_number: Option<i64> = row
        .try_get("github_issue_number")
        .map_err(|error| format!("decode github_issue_number for {card_id}: {error}"))?;

    let agent_exists =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM agents WHERE id = $1")
            .bind(to_agent_id)
            .fetch_one(pool)
            .await
            .map_err(|error| format!("check postgres dispatch agent {to_agent_id}: {error}"))?
            > 0;
    if !agent_exists {
        return Err(format!(
            "Cannot create {dispatch_type} dispatch: agent '{to_agent_id}' not found (card {card_id})"
        ));
    }

    let channel_value = crate::db::agents::resolve_agent_dispatch_channel_pg(
        pool,
        to_agent_id,
        Some(dispatch_type),
    )
    .await
    .map_err(|error| {
        format!("resolve postgres dispatch channel for {to_agent_id} ({dispatch_type}): {error}")
    })?
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
    .ok_or_else(|| {
        format!(
            "Cannot create {dispatch_type} dispatch: agent '{to_agent_id}' has no discord channel (card {card_id})"
        )
    })?;
    if resolve_activate_dispatch_channel_id(&channel_value).is_none() {
        return Err(format!(
            "Cannot create {dispatch_type} dispatch: agent '{to_agent_id}' has invalid discord channel '{channel_value}' (card {card_id})"
        ));
    }

    let effective =
        resolve_activate_pipeline_pg(pool, repo_id.as_deref(), assigned_agent_id.as_deref())
            .await?;
    if effective.is_terminal(&old_status) {
        return Err(format!(
            "Cannot create {dispatch_type} dispatch for terminal card {card_id} (status: {old_status})"
        ));
    }

    let mut context_with_strategy = if context.is_object() {
        context.clone()
    } else {
        json!({})
    };
    if let Some(default_force_new_session) =
        crate::dispatch::dispatch_type_force_new_session_default(Some(dispatch_type))
        && let Some(obj) = context_with_strategy.as_object_mut()
    {
        obj.entry("force_new_session".to_string())
            .or_insert(json!(default_force_new_session));
    }
    if let Some(obj) = context_with_strategy.as_object_mut() {
        if let Some(repo_id) = repo_id.as_deref() {
            obj.entry("repo".to_string())
                .or_insert_with(|| json!(repo_id));
            obj.entry("target_repo".to_string())
                .or_insert_with(|| json!(repo_id));
        }
        if let Some(issue_number) = github_issue_number {
            obj.entry("issue_number".to_string())
                .or_insert_with(|| json!(issue_number));
        }
    }
    if let Ok(Some((worktree_path, worktree_branch, _))) =
        crate::dispatch::resolve_card_worktree(&deps.db, card_id, Some(&context_with_strategy))
        && let Some(obj) = context_with_strategy.as_object_mut()
    {
        obj.entry("worktree_path".to_string())
            .or_insert_with(|| json!(worktree_path));
        obj.entry("worktree_branch".to_string())
            .or_insert_with(|| json!(worktree_branch));
    }

    let parent_dispatch_id = context_with_strategy
        .get("parent_dispatch_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let chain_depth = if let Some(parent_dispatch_id) = parent_dispatch_id.as_deref() {
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(chain_depth, 0)::BIGINT + 1
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(parent_dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!("load parent dispatch chain depth for {parent_dispatch_id}: {error}")
        })?
        .unwrap_or(1)
    } else {
        0
    };

    let dispatch_id = uuid::Uuid::new_v4().to_string();
    let kickoff_state = if matches!(
        dispatch_type,
        "review" | "review-decision" | "rework" | "consultation"
    ) {
        None
    } else {
        Some(
            effective
                .kickoff_for(&old_status)
                .unwrap_or_else(|| effective.initial_state().to_string()),
        )
    };
    let decision = crate::engine::transition::decide_transition(
        &crate::engine::transition::TransitionContext {
            card: crate::engine::transition::CardState {
                id: card_id.to_string(),
                status: old_status.clone(),
                review_status,
                latest_dispatch_id,
            },
            pipeline: effective.clone(),
            gates: crate::engine::transition::GateSnapshot::default(),
        },
        &crate::engine::transition::TransitionEvent::DispatchAttached {
            dispatch_id: dispatch_id.clone(),
            dispatch_type: dispatch_type.to_string(),
            kickoff_state,
        },
    );
    if let crate::engine::transition::TransitionOutcome::Blocked(reason) = &decision.outcome {
        return Err(reason.clone());
    }

    let context_str = serde_json::to_string(&context_with_strategy)
        .map_err(|error| format!("encode dispatch context for {card_id}: {error}"))?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("open postgres activate dispatch transaction: {error}"))?;

    if dispatch_type != "review-decision"
        && let Some(existing_id) = sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = $2
               AND status IN ('pending', 'dispatched')
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(card_id)
        .bind(dispatch_type)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| {
            format!("recheck active postgres dispatch for {card_id} during create: {error}")
        })?
    {
        tx.rollback().await.ok();
        return Ok(existing_id);
    }

    sqlx::query(
        "INSERT INTO task_dispatches (
            id,
            kanban_card_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            context,
            parent_dispatch_id,
            chain_depth,
            created_at,
            updated_at
        ) VALUES (
            $1, $2, $3, $4, 'pending', $5, $6, $7, $8, NOW(), NOW()
        )",
    )
    .bind(&dispatch_id)
    .bind(card_id)
    .bind(to_agent_id)
    .bind(dispatch_type)
    .bind(title)
    .bind(&context_str)
    .bind(parent_dispatch_id.as_deref())
    .bind(chain_depth)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres dispatch {dispatch_id} for {card_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES (
            $1, $2, $3, NULL, 'pending', 'create_dispatch', NULL
        )",
    )
    .bind(&dispatch_id)
    .bind(card_id)
    .bind(dispatch_type)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres dispatch event for {dispatch_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title)
         VALUES ($1, 'notify', $2, $3, $4)
         ON CONFLICT DO NOTHING",
    )
    .bind(&dispatch_id)
    .bind(to_agent_id)
    .bind(card_id)
    .bind(title)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert postgres dispatch outbox for {dispatch_id}: {error}"))?;

    for intent in &decision.intents {
        crate::engine::transition_executor_pg::execute_activate_transition_intent_pg(
            &mut tx, intent,
        )
        .await?;
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres dispatch {dispatch_id}: {error}"))?;

    Ok(dispatch_id)
}

fn load_live_dispatch_ids_for_runs(
    conn: &libsql_rusqlite::Connection,
    run_ids: &[String],
) -> libsql_rusqlite::Result<Vec<String>> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    let sql = live_dispatches_for_runs_sql("td.id", run_ids.len());
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(libsql_rusqlite::params_from_iter(run_ids.iter()), |row| {
        row.get(0)
    })?
    .collect::<Result<Vec<_>, _>>()
}

fn load_dispatched_card_ids_for_runs(
    conn: &libsql_rusqlite::Connection,
    run_ids: &[String],
) -> libsql_rusqlite::Result<Vec<String>> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat("?")
        .take(run_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT DISTINCT kanban_card_id
         FROM auto_queue_entries
         WHERE run_id IN ({placeholders})
           AND status = 'dispatched'
           AND kanban_card_id IS NOT NULL
           AND TRIM(kanban_card_id) != ''"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(libsql_rusqlite::params_from_iter(run_ids.iter()), |row| {
        row.get(0)
    })?
    .collect::<Result<Vec<_>, _>>()
}

fn delete_phase_gate_rows_for_runs(
    conn: &libsql_rusqlite::Connection,
    run_ids: &[String],
) -> usize {
    if run_ids.is_empty() {
        return 0;
    }

    let placeholders = std::iter::repeat("?")
        .take(run_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "DELETE FROM auto_queue_phase_gates
         WHERE run_id IN ({placeholders})"
    );
    conn.execute(&sql, libsql_rusqlite::params_from_iter(run_ids.iter()))
        .unwrap_or(0)
}

fn count_live_dispatches_for_runs(conn: &libsql_rusqlite::Connection, run_ids: &[String]) -> i64 {
    if run_ids.is_empty() {
        return 0;
    }

    let sql = live_dispatches_for_runs_sql("COUNT(*)", run_ids.len());
    conn.query_row(
        &sql,
        libsql_rusqlite::params_from_iter(run_ids.iter()),
        |row| row.get(0),
    )
    .unwrap_or(0)
}

fn live_dispatches_for_runs_sql(select_expr: &str, run_count: usize) -> String {
    let values = std::iter::repeat("(?)")
        .take(run_count)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "WITH target_runs(id) AS (VALUES {values})
         SELECT {select_expr}
         FROM task_dispatches td
         WHERE td.status IN ('pending', 'dispatched')
           AND (
               EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   JOIN target_runs tr ON tr.id = e.run_id
                   WHERE e.dispatch_id = td.id
               )
               OR EXISTS (
                   SELECT 1
                   FROM auto_queue_phase_gates pg
                   JOIN target_runs tr ON tr.id = pg.run_id
                   WHERE pg.dispatch_id = td.id
               )
               OR (
                   json_valid(td.context)
                   AND json_extract(td.context, '$.phase_gate.run_id') IN (
                       SELECT id FROM target_runs
                   )
               )
           )"
    )
}

fn cancel_live_dispatches_for_runs(
    conn: &libsql_rusqlite::Connection,
    run_ids: &[String],
    reason: &str,
) -> usize {
    let dispatch_ids = load_live_dispatch_ids_for_runs(conn, run_ids).unwrap_or_default();
    dispatch_ids
        .into_iter()
        .map(|dispatch_id| {
            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                conn,
                &dispatch_id,
                Some(reason),
            )
            .unwrap_or(0)
        })
        .sum()
}

fn clear_and_release_slots_for_runs(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    conn: &libsql_rusqlite::Connection,
    run_ids: &[String],
) -> SlotCleanupResult {
    let mut released_slots: HashSet<(String, i64)> = HashSet::new();
    let mut run_release_candidates: HashMap<String, usize> = HashMap::new();
    let mut cleared_sessions = 0usize;
    let mut warnings = Vec::new();
    let base_log_ctx = run_ids
        .first()
        .map(|run_id| AutoQueueLogContext::new().run(run_id))
        .unwrap_or_default();

    match load_slot_bindings_for_runs(conn, run_ids) {
        Ok(bindings) => {
            for (bound_run_id, agent_id, slot_index) in bindings {
                *run_release_candidates.entry(bound_run_id).or_default() += 1;
                if released_slots.insert((agent_id.clone(), slot_index)) {
                    cleared_sessions +=
                        crate::services::auto_queue::runtime::clear_slot_threads_for_slot(
                            health_registry.clone(),
                            conn,
                            &agent_id,
                            slot_index,
                        );
                }
            }
        }
        Err(error) => {
            crate::auto_queue_log!(
                warn,
                "clear_slot_bindings_load_failed",
                base_log_ctx.clone(),
                "[auto-queue] failed to load slot bindings for runs {:?}: {}",
                run_ids,
                error
            );
            warnings.push(format!(
                "failed to load slot bindings for runs {:?}: {}",
                run_ids, error
            ));
        }
    }

    let mut released_slot_count = 0usize;
    for run_id in run_ids {
        match crate::db::auto_queue::release_run_slots(conn, run_id) {
            Ok(()) => {
                released_slot_count += run_release_candidates.get(run_id).copied().unwrap_or(0);
            }
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "clear_slot_release_failed",
                    AutoQueueLogContext::new().run(run_id),
                    "[auto-queue] failed to release slots while clearing run {}: {}",
                    run_id,
                    error
                );
                warnings.push(format!("failed to release slots for run {run_id}: {error}"));
            }
        }
    }

    SlotCleanupResult {
        released_slots: released_slot_count,
        cleared_slot_sessions: cleared_sessions,
        warnings,
    }
}

#[derive(Debug, Default)]
struct SlotCleanupResult {
    released_slots: usize,
    cleared_slot_sessions: usize,
    warnings: Vec<String>,
}

fn slot_cleanup_warning(warnings: &[String]) -> Option<String> {
    (!warnings.is_empty()).then(|| warnings.join("; "))
}

async fn load_run_ids_with_status_pg(
    pool: &sqlx::PgPool,
    statuses: &[&str],
) -> Result<Vec<String>, String> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }

    let mut query =
        QueryBuilder::<Postgres>::new("SELECT id FROM auto_queue_runs WHERE status IN (");
    let mut separated = query.separated(", ");
    for status in statuses {
        separated.push_bind(*status);
    }
    separated.push_unseparated(") ORDER BY created_at ASC, id ASC");
    query
        .build_query_scalar::<String>()
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load postgres auto_queue_runs by status: {error}"))
}

async fn load_slot_bindings_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<Vec<(String, String, i64)>, String> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut query = QueryBuilder::<Postgres>::new(
        "SELECT DISTINCT assigned_run_id, agent_id, slot_index
         FROM auto_queue_slots
         WHERE assigned_run_id IN (",
    );
    let mut separated = query.separated(", ");
    for run_id in run_ids {
        separated.push_bind(run_id);
    }
    separated.push_unseparated(") AND assigned_run_id IS NOT NULL");

    let rows = query
        .build()
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load postgres slot bindings for runs: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get::<String, _>("assigned_run_id")
                    .map_err(|error| format!("decode postgres assigned_run_id: {error}"))?,
                row.try_get::<String, _>("agent_id")
                    .map_err(|error| format!("decode postgres slot agent_id: {error}"))?,
                row.try_get::<i64, _>("slot_index")
                    .map_err(|error| format!("decode postgres slot_index: {error}"))?,
            ))
        })
        .collect()
}

async fn load_live_dispatch_ids_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<Vec<String>, String> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    sqlx::query_scalar(
        "SELECT DISTINCT td.id
         FROM task_dispatches td
         WHERE td.status IN ('pending', 'dispatched')
           AND (
               EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.dispatch_id = td.id
                     AND e.run_id = ANY($1)
               )
               OR EXISTS (
                   SELECT 1
                   FROM auto_queue_phase_gates pg
                   WHERE pg.dispatch_id = td.id
                     AND pg.run_id = ANY($1)
               )
               OR (
                   CASE
                       WHEN td.context IS NULL OR BTRIM(td.context) = '' THEN NULL
                       ELSE (td.context::jsonb #>> '{phase_gate,run_id}')
                   END
               ) = ANY($1)
           )
         ORDER BY td.id",
    )
    .bind(run_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!(
            "load postgres live dispatch ids for runs {:?}: {error}",
            run_ids
        )
    })
}

async fn load_dispatched_card_ids_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<Vec<String>, String> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    sqlx::query_scalar(
        "SELECT DISTINCT e.kanban_card_id
         FROM auto_queue_entries e
         WHERE e.run_id = ANY($1)
           AND e.status = 'dispatched'
           AND e.kanban_card_id IS NOT NULL
           AND BTRIM(e.kanban_card_id) <> ''
         ORDER BY e.kanban_card_id",
    )
    .bind(run_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!(
            "load postgres dispatched card ids for runs {:?}: {error}",
            run_ids
        )
    })
}

async fn delete_phase_gate_rows_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<usize, String> {
    if run_ids.is_empty() {
        return Ok(0);
    }

    let mut query =
        QueryBuilder::<Postgres>::new("DELETE FROM auto_queue_phase_gates WHERE run_id IN (");
    let mut separated = query.separated(", ");
    for run_id in run_ids {
        separated.push_bind(run_id);
    }
    separated.push_unseparated(")");

    query
        .build()
        .execute(pool)
        .await
        .map(|result| result.rows_affected() as usize)
        .map_err(|error| format!("delete postgres auto_queue_phase_gates: {error}"))
}

async fn count_live_dispatches_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> Result<i64, String> {
    load_live_dispatch_ids_for_runs_pg(pool, run_ids)
        .await
        .map(|rows| rows.len() as i64)
}

async fn cancel_live_dispatches_for_runs_pg(
    pool: &sqlx::PgPool,
    run_ids: &[String],
    reason: &str,
) -> Result<usize, String> {
    let dispatch_ids = load_live_dispatch_ids_for_runs_pg(pool, run_ids).await?;
    let mut cancelled = 0usize;
    for dispatch_id in dispatch_ids {
        cancelled += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
            pool,
            &dispatch_id,
            Some(reason),
        )
        .await?;
    }
    Ok(cancelled)
}

async fn clear_sessions_for_dispatches_pg(
    pool: &sqlx::PgPool,
    dispatch_ids: &[String],
) -> Result<usize, String> {
    let mut cleared_sessions = 0usize;
    for dispatch_id in dispatch_ids {
        let result = sqlx::query(
            "UPDATE sessions
             SET status = 'idle',
                 active_dispatch_id = NULL,
                 session_info = $1,
                 claude_session_id = NULL,
                 tokens = 0,
                 last_heartbeat = NOW()
             WHERE active_dispatch_id = $2
               AND status IN ('working', 'idle')",
        )
        .bind("Dispatch cancelled")
        .bind(dispatch_id)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("clear postgres sessions for cancelled dispatch {dispatch_id}: {error}")
        })?;
        cleared_sessions += result.rows_affected() as usize;
    }
    Ok(cleared_sessions)
}

async fn transition_entry_to_skipped_pg(
    pool: &sqlx::PgPool,
    entry_id: &str,
    trigger_source: &str,
) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres entry skip transaction {entry_id}: {error}"))?;

    let current_status = sqlx::query_scalar::<_, Option<String>>(
        "SELECT status
         FROM auto_queue_entries
         WHERE id = $1",
    )
    .bind(entry_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| format!("load postgres entry status {entry_id}: {error}"))?
    .flatten();
    let Some(current_status) = current_status else {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback missing postgres entry {entry_id}: {error}"))?;
        return Ok(false);
    };
    if !matches!(current_status.as_str(), "pending" | "dispatched") {
        tx.rollback().await.map_err(|error| {
            format!("rollback non-skippable postgres entry {entry_id}: {error}")
        })?;
        return Ok(false);
    }

    let changed = sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'skipped',
             dispatch_id = NULL,
             dispatched_at = NULL,
             completed_at = NOW()
         WHERE id = $1
           AND status = $2",
    )
    .bind(entry_id)
    .bind(&current_status)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("skip postgres entry {entry_id}: {error}"))?
    .rows_affected() as usize;
    if changed == 0 {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback unchanged postgres entry {entry_id}: {error}"))?;
        return Ok(false);
    }

    let _ = sqlx::query(
        "INSERT INTO auto_queue_entry_transitions (
            entry_id,
            from_status,
            to_status,
            trigger_source
        ) VALUES ($1, $2, 'skipped', $3)",
    )
    .bind(entry_id)
    .bind(&current_status)
    .bind(trigger_source)
    .execute(&mut *tx)
    .await;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres entry skip {entry_id}: {error}"))?;
    Ok(true)
}

async fn clear_and_release_slots_for_runs_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &sqlx::PgPool,
    run_ids: &[String],
) -> SlotCleanupResult {
    let mut released_slots: HashSet<(String, i64)> = HashSet::new();
    let mut released_slot_count = 0usize;
    let mut cleared_sessions = 0usize;
    let mut warnings = Vec::new();
    for run_id in run_ids {
        match sqlx::query(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL,
                 updated_at = NOW()
             WHERE assigned_run_id = $1
             RETURNING agent_id, slot_index",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                released_slot_count += rows.len();
                for row in rows {
                    let agent_id = match row.try_get::<String, _>("agent_id") {
                        Ok(value) => value,
                        Err(error) => {
                            warnings.push(format!(
                                "failed to decode released slot agent for run {run_id}: {error}"
                            ));
                            continue;
                        }
                    };
                    let slot_index = match row.try_get::<i64, _>("slot_index") {
                        Ok(value) => value,
                        Err(error) => {
                            warnings.push(format!(
                                "failed to decode released slot index for run {run_id}: {error}"
                            ));
                            continue;
                        }
                    };
                    if released_slots.insert((agent_id.clone(), slot_index)) {
                        match crate::services::auto_queue::runtime::clear_slot_threads_for_slot_pg(
                            health_registry.clone(),
                            pool,
                            &agent_id,
                            slot_index,
                        )
                        .await
                        {
                            Ok(cleared) => cleared_sessions += cleared,
                            Err(error) => {
                                crate::auto_queue_log!(
                                    warn,
                                    "clear_slot_threads_pg_failed",
                                    AutoQueueLogContext::new().agent(&agent_id),
                                    "[auto-queue] failed to clear postgres slot thread sessions for {}:{}: {}",
                                    agent_id,
                                    slot_index,
                                    error
                                );
                                warnings.push(format!(
                                    "failed to clear slot thread sessions for {agent_id}:{slot_index}: {error}"
                                ));
                            }
                        }
                    }
                }
            }
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "clear_slot_release_pg_failed",
                    AutoQueueLogContext::new().run(run_id),
                    "[auto-queue] failed to release postgres slots while clearing run {}: {}",
                    run_id,
                    error
                );
                warnings.push(format!("failed to release slots for run {run_id}: {error}"));
            }
        }
    }

    SlotCleanupResult {
        released_slots: released_slot_count,
        cleared_slot_sessions: cleared_sessions,
        warnings,
    }
}

async fn cancel_selected_runs_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &sqlx::PgPool,
    target_run_ids: &[String],
    reason: &str,
) -> Result<serde_json::Value, String> {
    crate::services::auto_queue::cancel_run::cancel_selected_runs_with_pg(
        health_registry,
        pool,
        target_run_ids,
        reason,
    )
    .await
}

async fn reset_with_pg(body: &ResetBody, pool: &sqlx::PgPool) -> Result<serde_json::Value, String> {
    let agent_id = body
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let (deleted_entries, completed_runs, protected_active_runs, warning) = if let Some(agent_id) =
        agent_id
    {
        let deleted_entries = sqlx::query("DELETE FROM auto_queue_entries WHERE agent_id = $1")
            .bind(agent_id)
            .execute(pool)
            .await
            .map_err(|error| format!("delete auto_queue_entries for agent {agent_id}: {error}"))?
            .rows_affected() as usize;
        let completed_runs = sqlx::query(
            "UPDATE auto_queue_runs
                 SET status = 'completed',
                     completed_at = NOW()
                 WHERE status IN ('generated', 'pending', 'active', 'paused')
                   AND agent_id = $1",
        )
        .bind(agent_id)
        .execute(pool)
        .await
        .map_err(|error| format!("complete auto_queue_runs for agent {agent_id}: {error}"))?
        .rows_affected() as usize;
        (deleted_entries, completed_runs, 0usize, None)
    } else {
        let protected_active_runs = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM auto_queue_runs WHERE status = 'active'",
        )
        .fetch_one(pool)
        .await
        .map_err(|error| format!("count active auto_queue_runs: {error}"))?;
        if protected_active_runs > 0 {
            crate::auto_queue_log!(
                warn,
                "reset_global_preserved_active_runs",
                AutoQueueLogContext::new(),
                "[auto-queue] Global PG reset requested without agent_id; preserving {protected_active_runs} active run(s)"
            );
        } else {
            crate::auto_queue_log!(
                warn,
                "reset_global_unscoped",
                AutoQueueLogContext::new(),
                "[auto-queue] Global PG reset requested without agent_id; applying unscoped reset"
            );
        }

        let deleted_entries = if protected_active_runs > 0 {
            sqlx::query(
                "DELETE FROM auto_queue_entries
                     WHERE run_id IS NULL
                        OR run_id NOT IN (
                            SELECT id FROM auto_queue_runs WHERE status = 'active'
                        )",
            )
            .execute(pool)
            .await
            .map_err(|error| format!("delete inactive auto_queue_entries: {error}"))?
            .rows_affected() as usize
        } else {
            sqlx::query("DELETE FROM auto_queue_entries")
                .execute(pool)
                .await
                .map_err(|error| format!("delete all auto_queue_entries: {error}"))?
                .rows_affected() as usize
        };
        let completed_runs = if protected_active_runs > 0 {
            sqlx::query(
                "UPDATE auto_queue_runs
                     SET status = 'completed',
                         completed_at = NOW()
                     WHERE status IN ('generated', 'pending', 'paused')",
            )
            .execute(pool)
            .await
            .map_err(|error| format!("complete inactive auto_queue_runs: {error}"))?
            .rows_affected() as usize
        } else {
            sqlx::query(
                "UPDATE auto_queue_runs
                     SET status = 'completed',
                         completed_at = NOW()
                     WHERE status IN ('generated', 'pending', 'active', 'paused')",
            )
            .execute(pool)
            .await
            .map_err(|error| format!("complete all auto_queue_runs: {error}"))?
            .rows_affected() as usize
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
    Ok(response)
}

async fn update_run_with_pg(
    run_id: &str,
    body: &UpdateRunBody,
    pool: &sqlx::PgPool,
) -> Result<usize, String> {
    let mut changed = 0usize;

    if let Some(ref status) = body.status {
        let result = if status == "completed" {
            sqlx::query(
                "UPDATE auto_queue_runs
                 SET status = $1,
                     completed_at = NOW()
                 WHERE id = $2",
            )
            .bind(status)
            .bind(run_id)
            .execute(pool)
            .await
            .map_err(|error| {
                format!("update postgres auto_queue_runs status for {run_id}: {error}")
            })?
        } else {
            sqlx::query(
                "UPDATE auto_queue_runs
                 SET status = $1,
                     completed_at = NULL
                 WHERE id = $2",
            )
            .bind(status)
            .bind(run_id)
            .execute(pool)
            .await
            .map_err(|error| {
                format!("update postgres auto_queue_runs status for {run_id}: {error}")
            })?
        };
        changed += result.rows_affected() as usize;
    }

    if let Some(ref deploy_phases) = body.deploy_phases {
        let json_str = serde_json::to_string(deploy_phases)
            .map_err(|error| format!("serialize deploy_phases for run {run_id}: {error}"))?;
        let result = sqlx::query(
            "UPDATE auto_queue_runs
             SET deploy_phases = $1::jsonb
             WHERE id = $2",
        )
        .bind(json_str)
        .bind(run_id)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("update postgres auto_queue_runs deploy_phases for {run_id}: {error}")
        })?;
        changed += result.rows_affected() as usize;
    }

    if let Some(max_concurrent_threads) = body.max_concurrent_threads {
        let result = sqlx::query(
            "UPDATE auto_queue_runs
             SET max_concurrent_threads = $1
             WHERE id = $2",
        )
        .bind(max_concurrent_threads)
        .bind(run_id)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("update postgres auto_queue_runs max_concurrent_threads for {run_id}: {error}")
        })?;
        changed += result.rows_affected() as usize;
    }

    Ok(changed)
}

async fn reorder_with_pg(body: &ReorderBody, pool: &sqlx::PgPool) -> Result<(), String> {
    let mut run_id = None;
    for id in &body.ordered_ids {
        let found = sqlx::query_scalar::<_, String>(
            "SELECT run_id
             FROM auto_queue_entries
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load auto_queue_entries run_id for {id}: {error}"))?;
        if found.is_some() {
            run_id = found;
            break;
        }
    }

    let Some(run_id) = run_id else {
        return Err("not_found:no matching queue entries found".to_string());
    };

    let current_entries: Vec<QueueEntryOrder> = sqlx::query(
        "SELECT id,
                COALESCE(status, 'pending') AS status,
                COALESCE(agent_id, '') AS agent_id
         FROM auto_queue_entries
         WHERE run_id = $1
         ORDER BY priority_rank ASC, created_at ASC, id ASC",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load auto_queue_entries for reorder run {run_id}: {error}"))?
    .into_iter()
    .map(|row| {
        Ok(QueueEntryOrder {
            id: row
                .try_get("id")
                .map_err(|error| format!("decode reorder entry id: {error}"))?,
            status: row
                .try_get("status")
                .map_err(|error| format!("decode reorder entry status: {error}"))?,
            agent_id: row
                .try_get("agent_id")
                .map_err(|error| format!("decode reorder entry agent_id: {error}"))?,
        })
    })
    .collect::<Result<Vec<_>, String>>()?;

    let reordered_ids = reorder_entry_ids(
        &current_entries,
        &body.ordered_ids,
        body.agent_id.as_deref(),
    )?;

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin reorder transaction: {error}"))?;
    for (rank, id) in reordered_ids.iter().enumerate() {
        sqlx::query("UPDATE auto_queue_entries SET priority_rank = $1 WHERE id = $2")
            .bind(rank as i64)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("update auto_queue_entries priority_rank for {id}: {error}")
            })?;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit reorder transaction: {error}"))?;

    Ok(())
}

pub(crate) fn cancel_with_conn(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    conn: &libsql_rusqlite::Connection,
) -> serde_json::Value {
    crate::services::auto_queue::cancel_run::cancel_with_conn(health_registry, conn)
}

async fn pause_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &sqlx::PgPool,
) -> Result<serde_json::Value, String> {
    let active_run_ids =
        crate::services::auto_queue::cancel_run::load_run_ids_with_status_pg(pool, &["active"])
            .await?;
    let live_dispatch_ids =
        crate::services::auto_queue::cancel_run::load_live_dispatch_ids_for_runs_pg(
            pool,
            &active_run_ids,
        )
        .await?;
    let cancelled_dispatches =
        crate::services::auto_queue::cancel_run::cancel_live_dispatches_for_runs_pg(
            pool,
            &active_run_ids,
            "auto_queue_pause",
        )
        .await?;
    let mut slot_cleanup =
        crate::services::auto_queue::cancel_run::clear_and_release_slots_for_runs_pg(
            health_registry,
            pool,
            &active_run_ids,
        )
        .await;
    match crate::services::auto_queue::cancel_run::clear_sessions_for_dispatches_pg(
        pool,
        &live_dispatch_ids,
    )
    .await
    {
        Ok(cleared) => slot_cleanup.cleared_slot_sessions += cleared,
        Err(error) => {
            crate::auto_queue_log!(
                warn,
                "run_pause_dispatch_session_clear_pg_failed",
                active_run_ids
                    .first()
                    .map(|run_id| AutoQueueLogContext::new().run(run_id))
                    .unwrap_or_default(),
                "[auto-queue] failed to clear postgres sessions for paused dispatches {:?}: {}",
                live_dispatch_ids,
                error
            );
            slot_cleanup.warnings.push(format!(
                "failed to clear sessions for paused dispatches {:?}: {}",
                live_dispatch_ids, error
            ));
        }
    }
    let paused = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'paused',
             completed_at = NULL
         WHERE status = 'active'",
    )
    .execute(pool)
    .await
    .map_err(|error| format!("pause postgres auto_queue_runs: {error}"))?
    .rows_affected() as usize;

    let mut response = json!({
        "ok": true,
        "paused_runs": paused,
        "cancelled_dispatches": cancelled_dispatches,
        "released_slots": slot_cleanup.released_slots,
        "cleared_slot_sessions": slot_cleanup.cleared_slot_sessions,
    });
    if let Some(warning) =
        crate::services::auto_queue::cancel_run::slot_cleanup_warning(&slot_cleanup.warnings)
    {
        response["warning"] = json!(warning);
    }
    Ok(response)
}

async fn cancel_with_pg(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &sqlx::PgPool,
) -> Result<serde_json::Value, String> {
    crate::services::auto_queue::cancel_run::cancel_with_pg(health_registry, pool).await
}

fn cancel_selected_runs_with_conn(
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    conn: &libsql_rusqlite::Connection,
    target_run_ids: &[String],
    reason: &str,
) -> serde_json::Value {
    crate::services::auto_queue::cancel_run::cancel_selected_runs_with_conn(
        health_registry,
        conn,
        target_run_ids,
        reason,
    )
}

#[derive(Debug, Serialize)]
struct AutoQueueHistoryRun {
    id: String,
    repo: Option<String>,
    agent_id: Option<String>,
    status: String,
    created_at: i64,
    completed_at: Option<i64>,
    duration_ms: i64,
    entry_count: i64,
    done_count: i64,
    skipped_count: i64,
    pending_count: i64,
    dispatched_count: i64,
    success_rate: f64,
    failure_rate: f64,
}

#[derive(Debug, Serialize)]
struct AutoQueueHistorySummary {
    total_runs: usize,
    completed_runs: usize,
    success_rate: f64,
    failure_rate: f64,
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

#[derive(Debug, Clone, Copy)]
struct RequestedGenerateEntry {
    issue_number: i64,
    batch_phase: i64,
    thread_group: Option<i64>,
}

#[derive(Debug, Clone)]
struct ResolvedDispatchCard {
    issue_number: i64,
    card_id: String,
    repo_id: Option<String>,
    status: String,
    assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ActivateCardState {
    status: String,
    title: String,
    metadata: Option<String>,
    latest_dispatch_id: Option<String>,
    latest_dispatch_status: Option<String>,
    entry_status: String,
    repo_id: Option<String>,
    assigned_agent_id: Option<String>,
}

impl ActivateCardState {
    fn has_active_dispatch(&self) -> bool {
        self.latest_dispatch_id.is_some()
            && matches!(
                self.latest_dispatch_status.as_deref(),
                Some("pending") | Some("dispatched")
            )
    }

    fn is_terminal(&self, conn: &libsql_rusqlite::Connection) -> bool {
        crate::pipeline::ensure_loaded();
        crate::pipeline::resolve_for_card(
            conn,
            self.repo_id.as_deref(),
            self.assigned_agent_id.as_deref(),
        )
        .is_terminal(&self.status)
    }
}

#[derive(Debug, Clone)]
struct RestoreEntryRecord {
    entry_id: String,
    card_id: String,
    agent_id: String,
    thread_group: i64,
}

#[derive(Debug, Default)]
struct RestoreRunCounts {
    restored_pending: usize,
    restored_done: usize,
    restored_dispatched: usize,
    rebound_slots: usize,
    created_dispatches: usize,
    unbound_dispatches: usize,
}

const RUN_STATUS_RESTORING: &str = "restoring";

#[derive(Debug, Clone)]
enum RestoreEntryDecision {
    Pending,
    Done,
    ExistingDispatch { dispatch_id: String },
    NewDispatch { title: String },
}

#[derive(Debug, Clone)]
struct RestoreDispatchCandidate {
    entry: RestoreEntryRecord,
    title: String,
}

#[derive(Debug, Default)]
struct RestoreDispatchAttemptResult {
    dispatched: bool,
    created_dispatch: bool,
    rebound_slot: bool,
    unbound_dispatch: bool,
}

fn load_activate_card_state(
    conn: &libsql_rusqlite::Connection,
    card_id: &str,
    entry_id: &str,
) -> libsql_rusqlite::Result<ActivateCardState> {
    let (status, title, metadata, latest_dispatch_id, repo_id, assigned_agent_id): (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn.query_row(
        "SELECT status, title, metadata, latest_dispatch_id, repo_id, assigned_agent_id
         FROM kanban_cards
         WHERE id = ?1",
        [card_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    let latest_dispatch_status = latest_dispatch_id.as_deref().and_then(|dispatch_id| {
        conn.query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
    });
    let entry_status = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = ?1",
            [entry_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "pending".to_string());

    Ok(ActivateCardState {
        status,
        title,
        metadata,
        latest_dispatch_id,
        latest_dispatch_status,
        entry_status,
        repo_id,
        assigned_agent_id,
    })
}

async fn load_activate_card_state_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    entry_id: &str,
) -> Result<ActivateCardState, String> {
    let row = sqlx::query(
        "SELECT status, title, metadata, latest_dispatch_id, repo_id, assigned_agent_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card {card_id}: {error}"))?
    .ok_or_else(|| format!("postgres card {card_id} not found"))?;

    let latest_dispatch_id: Option<String> = row
        .try_get("latest_dispatch_id")
        .map_err(|error| format!("decode latest_dispatch_id for {card_id}: {error}"))?;
    let latest_dispatch_status = if let Some(dispatch_id) = latest_dispatch_id.as_deref() {
        sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load postgres dispatch status for {dispatch_id}: {error}"))?
    } else {
        None
    };
    let entry_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_entries WHERE id = $1")
            .bind(entry_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                format!("load postgres auto-queue entry status for {entry_id}: {error}")
            })?
            .unwrap_or_else(|| "pending".to_string());

    Ok(ActivateCardState {
        status: row
            .try_get("status")
            .map_err(|error| format!("decode status for {card_id}: {error}"))?,
        title: row
            .try_get("title")
            .map_err(|error| format!("decode title for {card_id}: {error}"))?,
        metadata: row
            .try_get("metadata")
            .map_err(|error| format!("decode metadata for {card_id}: {error}"))?,
        latest_dispatch_id,
        latest_dispatch_status,
        entry_status,
        repo_id: row
            .try_get("repo_id")
            .map_err(|error| format!("decode repo_id for {card_id}: {error}"))?,
        assigned_agent_id: row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id for {card_id}: {error}"))?,
    })
}

async fn resolve_activate_pipeline_pg(
    pool: &sqlx::PgPool,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<crate::pipeline::PipelineConfig, String> {
    crate::pipeline::ensure_loaded();

    let repo_override = if let Some(repo_id) = repo_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config FROM github_repos WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load repo pipeline override for {repo_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| format!("parse repo pipeline override for {repo_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    let agent_override = if let Some(agent_id) = agent_id {
        sqlx::query_scalar::<_, Option<String>>("SELECT pipeline_config FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load agent pipeline override for {agent_id}: {error}"))?
            .flatten()
            .map(|json| crate::pipeline::parse_override(&json))
            .transpose()
            .map_err(|error| format!("parse agent pipeline override for {agent_id}: {error}"))?
            .flatten()
    } else {
        None
    };

    Ok(crate::pipeline::resolve(
        repo_override.as_ref(),
        agent_override.as_ref(),
    ))
}

fn load_restore_entries(
    conn: &libsql_rusqlite::Connection,
    run_id: &str,
) -> libsql_rusqlite::Result<Vec<RestoreEntryRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, kanban_card_id, agent_id, COALESCE(thread_group, 0)
         FROM auto_queue_entries
         WHERE run_id = ?1
           AND status = 'skipped'
         ORDER BY priority_rank ASC, created_at ASC, id ASC",
    )?;
    let rows = stmt.query_map([run_id], |row| {
        Ok(RestoreEntryRecord {
            entry_id: row.get(0)?,
            card_id: row.get(1)?,
            agent_id: row.get(2)?,
            thread_group: row.get(3)?,
        })
    })?;
    rows.collect()
}

fn decide_restore_transition(
    conn: &libsql_rusqlite::Connection,
    entry: &RestoreEntryRecord,
) -> libsql_rusqlite::Result<RestoreEntryDecision> {
    let card_state = load_activate_card_state(conn, &entry.card_id, &entry.entry_id)?;
    let dispatch_history =
        crate::db::auto_queue::list_entry_dispatch_history(conn, &entry.entry_id)?;

    if dispatch_history.is_empty() {
        return Ok(RestoreEntryDecision::Pending);
    }
    if card_state.is_terminal(conn) {
        return Ok(RestoreEntryDecision::Done);
    }
    if card_state.has_active_dispatch() {
        if let Some(dispatch_id) = card_state.latest_dispatch_id {
            return Ok(RestoreEntryDecision::ExistingDispatch { dispatch_id });
        }
    }
    if !card_state.is_terminal(conn) {
        return Ok(RestoreEntryDecision::NewDispatch {
            title: card_state.title,
        });
    }

    Ok(RestoreEntryDecision::Pending)
}

fn apply_restore_state_changes(
    conn: &mut libsql_rusqlite::Connection,
    run_id: &str,
    run_status: Option<&str>,
) -> Result<(RestoreRunCounts, Vec<RestoreDispatchCandidate>), String> {
    let tx = conn
        .transaction()
        .map_err(|error| format!("open restore transaction failed: {error}"))?;
    if run_status == Some("cancelled") {
        let restored_run = tx
            .execute(
                "UPDATE auto_queue_runs
                 SET status = ?2,
                     completed_at = NULL
                 WHERE id = ?1
                   AND status = 'cancelled'",
                libsql_rusqlite::params![run_id, RUN_STATUS_RESTORING],
            )
            .map_err(|error| {
                format!("failed to start restore for cancelled run '{run_id}': {error}")
            })?;
        if restored_run == 0 {
            return Err(format!(
                "failed to start restore for cancelled run '{run_id}'"
            ));
        }
    }

    let entries = load_restore_entries(&tx, run_id)
        .map_err(|error| format!("load restore entries: {error}"))?;
    let mut counts = RestoreRunCounts::default();
    let mut dispatch_candidates = Vec::new();

    for entry in entries {
        match decide_restore_transition(&tx, &entry) {
            Ok(RestoreEntryDecision::Pending) => {
                match crate::db::auto_queue::update_entry_status_on_conn(
                    &tx,
                    &entry.entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_PENDING,
                    "restore_run_pending",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                ) {
                    Ok(result) if result.changed => counts.restored_pending += 1,
                    Ok(_) => {}
                    Err(error) => {
                        return Err(format!(
                            "{}: restore to pending failed: {error}",
                            entry.entry_id
                        ));
                    }
                }
            }
            Ok(RestoreEntryDecision::Done) => {
                match crate::db::auto_queue::update_entry_status_on_conn(
                    &tx,
                    &entry.entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_DONE,
                    "restore_run_done",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                ) {
                    Ok(result) if result.changed => counts.restored_done += 1,
                    Ok(_) => {}
                    Err(error) => {
                        return Err(format!(
                            "{}: restore to done failed: {error}",
                            entry.entry_id
                        ));
                    }
                }
            }
            Ok(RestoreEntryDecision::ExistingDispatch { dispatch_id }) => {
                let slot_allocation = crate::db::auto_queue::allocate_slot_for_group_agent(
                    &tx,
                    run_id,
                    entry.thread_group,
                    &entry.agent_id,
                )
                .map_err(|error| {
                    format!(
                        "{}: attach existing dispatch slot allocation failed: {error}",
                        entry.entry_id
                    )
                })?;
                let slot_index = slot_allocation
                    .as_ref()
                    .map(|allocation| allocation.slot_index);
                if let Some(allocation) = slot_allocation {
                    if allocation.newly_assigned || allocation.reassigned_from_other_group {
                        counts.rebound_slots += 1;
                    }
                } else {
                    counts.unbound_dispatches += 1;
                }
                match crate::db::auto_queue::update_entry_status_on_conn(
                    &tx,
                    &entry.entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                    "restore_run_attach_existing_dispatch",
                    &crate::db::auto_queue::EntryStatusUpdateOptions {
                        dispatch_id: Some(dispatch_id),
                        slot_index,
                    },
                ) {
                    Ok(result) if result.changed => counts.restored_dispatched += 1,
                    Ok(_) => {}
                    Err(error) => {
                        return Err(format!(
                            "{}: attach existing dispatch failed: {error}",
                            entry.entry_id
                        ));
                    }
                }
            }
            Ok(RestoreEntryDecision::NewDispatch { title }) => {
                match crate::db::auto_queue::update_entry_status_on_conn(
                    &tx,
                    &entry.entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_PENDING,
                    "restore_run_pending_new_dispatch",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                ) {
                    Ok(result) if result.changed => counts.restored_pending += 1,
                    Ok(_) => {}
                    Err(error) => {
                        return Err(format!(
                            "{}: restore pending for redispatch failed: {error}",
                            entry.entry_id
                        ));
                    }
                }
                dispatch_candidates.push(RestoreDispatchCandidate { entry, title });
            }
            Err(error) => {
                return Err(format!(
                    "{}: decide restore transition failed: {error}",
                    entry.entry_id
                ));
            }
        }
    }

    tx.commit()
        .map_err(|error| format!("commit restore state failed: {error}"))?;
    Ok((counts, dispatch_candidates))
}

fn attempt_restore_dispatch(
    deps: &AutoQueueActivateDeps,
    run_id: &str,
    candidate: &RestoreDispatchCandidate,
) -> Result<RestoreDispatchAttemptResult, String> {
    let entry = &candidate.entry;
    let entry_log_ctx = AutoQueueLogContext::new()
        .run(run_id)
        .entry(&entry.entry_id)
        .card(&entry.card_id)
        .agent(&entry.agent_id)
        .thread_group(entry.thread_group);
    let conn = deps
        .db
        .separate_conn()
        .map_err(|error| format!("{}: eager restore DB open failed: {error}", entry.entry_id))?;
    let card_state = load_activate_card_state(&conn, &entry.card_id, &entry.entry_id)
        .map_err(|error| format!("{}: eager restore reload failed: {error}", entry.entry_id))?;
    if card_state.entry_status != crate::db::auto_queue::ENTRY_STATUS_PENDING {
        return Ok(RestoreDispatchAttemptResult::default());
    }

    if card_state.has_active_dispatch() {
        let dispatch_id = card_state.latest_dispatch_id.clone().ok_or_else(|| {
            format!(
                "{}: active dispatch state missing dispatch id during eager restore",
                entry.entry_id
            )
        })?;
        let slot_allocation = crate::db::auto_queue::allocate_slot_for_group_agent(
            &conn,
            run_id,
            entry.thread_group,
            &entry.agent_id,
        )
        .map_err(|error| {
            format!(
                "{}: eager existing dispatch slot allocation failed: {error}",
                entry.entry_id
            )
        })?;
        let slot_index = slot_allocation
            .as_ref()
            .map(|allocation| allocation.slot_index);
        let mut result = RestoreDispatchAttemptResult::default();
        if let Some(allocation) = slot_allocation {
            if allocation.newly_assigned || allocation.reassigned_from_other_group {
                result.rebound_slot = true;
            }
        } else {
            result.unbound_dispatch = true;
        }
        match crate::db::auto_queue::update_entry_status_on_conn(
            &conn,
            &entry.entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
            "restore_run_attach_existing_dispatch",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: Some(dispatch_id),
                slot_index,
            },
        ) {
            Ok(_) => {
                result.dispatched = true;
                return Ok(result);
            }
            Err(error) => {
                return Err(format!(
                    "{}: eager attach existing dispatch failed: {error}",
                    entry.entry_id
                ));
            }
        }
    }

    let slot_allocation = crate::db::auto_queue::allocate_slot_for_group_agent(
        &conn,
        run_id,
        entry.thread_group,
        &entry.agent_id,
    )
    .map_err(|error| {
        format!(
            "{}: eager restore slot allocation failed: {error}",
            entry.entry_id
        )
    })?;
    let slot_index = slot_allocation
        .as_ref()
        .map(|allocation| allocation.slot_index);
    let mut result = RestoreDispatchAttemptResult::default();
    let reset_slot_thread_before_reuse = if let Some(allocation) = slot_allocation {
        let reset = slot_requires_thread_reset_before_reuse(
            &conn,
            &entry.agent_id,
            allocation.slot_index,
            allocation.newly_assigned,
            allocation.reassigned_from_other_group,
        );
        if allocation.newly_assigned || allocation.reassigned_from_other_group {
            result.rebound_slot = true;
        }
        reset
    } else {
        return Ok(result);
    };
    match crate::db::auto_queue::update_entry_status_on_conn(
        &conn,
        &entry.entry_id,
        crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
        "restore_run_dispatch_reserve",
        &crate::db::auto_queue::EntryStatusUpdateOptions {
            dispatch_id: None,
            slot_index,
        },
    ) {
        Ok(update) if !update.changed => return Ok(result),
        Ok(_) => {}
        Err(error) => {
            return Err(format!(
                "{}: eager restore reservation failed: {error}",
                entry.entry_id
            ));
        }
    }
    drop(conn);

    let dispatch_result = run_activate_blocking(|| {
        let dispatch_context = build_auto_queue_dispatch_context(
            &entry.entry_id,
            entry.thread_group,
            slot_index,
            reset_slot_thread_before_reuse,
            [("restored_run", json!(true)), ("run_id", json!(run_id))],
        );
        crate::dispatch::create_dispatch(
            &deps.db,
            &deps.engine,
            &entry.card_id,
            &entry.agent_id,
            "implementation",
            &candidate.title,
            &dispatch_context,
        )
    });
    let created_dispatch = dispatch_result.is_ok();

    let dispatch_id = match dispatch_result {
        Ok(dispatch) => dispatch
            .get("id")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        Err(error) => {
            let error_text = error.to_string();
            crate::auto_queue_log!(
                warn,
                "restore_run_create_dispatch_failed",
                entry_log_ctx.clone().maybe_slot_index(slot_index),
                "[auto-queue] restore_run create_dispatch failed for entry {}: {}",
                entry.entry_id,
                error_text
            );
            let conn = deps.db.separate_conn().map_err(|open_error| {
                format!(
                    "{}: reload after create_dispatch failure failed: {open_error}",
                    entry.entry_id
                )
            })?;
            let recovered_dispatch =
                load_activate_card_state(&conn, &entry.card_id, &entry.entry_id)
                    .ok()
                    .filter(|state| state.has_active_dispatch())
                    .and_then(|state| state.latest_dispatch_id);
            if recovered_dispatch.is_none() {
                let failure = record_entry_dispatch_failure(
                    deps,
                    run_id,
                    &entry.entry_id,
                    &entry.card_id,
                    &entry.agent_id,
                    entry.thread_group,
                    slot_index,
                    "restore_run_create_dispatch_failed",
                    &error_text,
                    &entry_log_ctx,
                )?;
                crate::auto_queue_log!(
                    warn,
                    "restore_run_create_dispatch_retry_scheduled",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] restore_run dispatch failure for entry {} scheduled retry {}/{} -> {}",
                    entry.entry_id,
                    failure.retry_count,
                    failure.retry_limit,
                    failure.to_status
                );
            }
            recovered_dispatch
        }
    };

    let Some(dispatch_id) = dispatch_id else {
        return Ok(result);
    };

    let conn = deps.db.separate_conn().map_err(|error| {
        format!(
            "{}: eager dispatch DB reopen failed: {error}",
            entry.entry_id
        )
    })?;
    match crate::db::auto_queue::update_entry_status_on_conn(
        &conn,
        &entry.entry_id,
        crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
        "restore_run_create_dispatch",
        &crate::db::auto_queue::EntryStatusUpdateOptions {
            dispatch_id: Some(dispatch_id.clone()),
            slot_index,
        },
    ) {
        Ok(_) => {
            result.dispatched = true;
            result.created_dispatch = created_dispatch;
            Ok(result)
        }
        Err(error) => {
            crate::auto_queue_log!(
                warn,
                "restore_run_mark_dispatched_failed",
                entry_log_ctx
                    .clone()
                    .dispatch(&dispatch_id)
                    .maybe_slot_index(slot_index),
                "[auto-queue] failed to mark restored entry {} dispatched after create_dispatch: {}",
                entry.entry_id,
                error
            );
            Ok(result)
        }
    }
}

fn finalize_restore_run(conn: &libsql_rusqlite::Connection, run_id: &str) -> Result<(), String> {
    let finalized = conn
        .execute(
            "UPDATE auto_queue_runs
             SET status = 'active',
                 completed_at = NULL
             WHERE id = ?1
               AND status = ?2",
            libsql_rusqlite::params![run_id, RUN_STATUS_RESTORING],
        )
        .map_err(|error| format!("failed to finalize restore for run '{run_id}': {error}"))?;
    if finalized > 0 {
        return Ok(());
    }

    let current_status: Option<String> = conn
        .query_row(
            "SELECT status
             FROM auto_queue_runs
             WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .ok();
    match current_status.as_deref() {
        Some("active") => Ok(()),
        Some(status) => Err(format!(
            "failed to finalize restore for run '{run_id}' (status={status})"
        )),
        None => Err(format!(
            "failed to finalize restore for missing run '{run_id}'"
        )),
    }
}

#[derive(Clone)]
pub(crate) struct AutoQueueActivateDeps {
    db: crate::db::Db,
    pg_pool: Option<sqlx::PgPool>,
    engine: crate::engine::PolicyEngine,
    config: Arc<crate::config::Config>,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    guild_id: Option<String>,
}

impl AutoQueueActivateDeps {
    fn from_state(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            pg_pool: state.pg_pool.clone(),
            engine: state.engine.clone(),
            config: state.config.clone(),
            health_registry: state.health_registry.clone(),
            guild_id: state.config.discord.guild_id.clone(),
        }
    }

    pub(crate) fn for_bridge(db: crate::db::Db, engine: crate::engine::PolicyEngine) -> Self {
        Self {
            db,
            pg_pool: None,
            engine,
            config: Arc::new(crate::config::Config::default()),
            health_registry: None,
            guild_id: None,
        }
    }

    fn auto_queue_service(&self) -> crate::services::auto_queue::AutoQueueService {
        crate::services::auto_queue::AutoQueueService::new(self.db.clone(), self.engine.clone())
    }

    fn entry_json(&self, entry_id: &str) -> serde_json::Value {
        self.auto_queue_service()
            .entry_json(entry_id, self.guild_id.as_deref())
            .unwrap_or(serde_json::Value::Null)
    }

    async fn entry_json_pg(&self, pool: &sqlx::PgPool, entry_id: &str) -> serde_json::Value {
        self.auto_queue_service()
            .entry_json_with_pg(pool, entry_id, self.guild_id.as_deref())
            .await
            .unwrap_or(serde_json::Value::Null)
    }
}

enum ActivatePreflightOutcome {
    Continue,
    Dispatched(serde_json::Value),
    Skipped,
    Deferred,
}

fn run_activate_blocking<T, F>(operation: F) -> T
where
    F: FnOnce() -> T,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

fn effective_max_entry_retries(
    conn: &libsql_rusqlite::Connection,
    deps: &AutoQueueActivateDeps,
) -> i64 {
    crate::services::settings::runtime_config_u64(conn, deps.config.as_ref(), "maxEntryRetries")
        .unwrap_or(3)
        .max(1)
        .min(i64::MAX as u64) as i64
}

fn human_alert_target_on_conn(
    conn: &libsql_rusqlite::Connection,
    deps: &AutoQueueActivateDeps,
) -> Option<String> {
    let channel = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'kanban_human_alert_channel_id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .or_else(|| deps.config.kanban.human_alert_channel_id.clone())?;
    let channel = channel.trim();
    if channel.is_empty() {
        return None;
    }
    Some(if channel.starts_with("channel:") {
        channel.to_string()
    } else {
        format!("channel:{channel}")
    })
}

fn compact_failure_summary(message: &str) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let truncated: String = chars.by_ref().take(180).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn queue_failed_entry_escalation_on_conn(
    conn: &libsql_rusqlite::Connection,
    deps: &AutoQueueActivateDeps,
    run_id: &str,
    entry_id: &str,
    card_id: &str,
    agent_id: &str,
    thread_group: i64,
    retry_count: i64,
    retry_limit: i64,
    cause: &str,
) -> libsql_rusqlite::Result<bool> {
    let Some(target) = human_alert_target_on_conn(conn, deps) else {
        return Ok(false);
    };
    let short_run_id = &run_id[..8.min(run_id.len())];
    let short_entry_id = &entry_id[..8.min(entry_id.len())];
    let content = format!(
        "자동큐 entry 실패: run {short_run_id} / entry {short_entry_id} / card {card_id} / agent {agent_id} / G{thread_group} / retry {retry_count}/{retry_limit} / {}",
        compact_failure_summary(cause)
    );
    conn.execute(
        "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, 'notify', 'system')",
        libsql_rusqlite::params![target, content],
    )?;
    Ok(true)
}

fn record_entry_dispatch_failure(
    deps: &AutoQueueActivateDeps,
    run_id: &str,
    entry_id: &str,
    card_id: &str,
    agent_id: &str,
    thread_group: i64,
    slot_index: Option<i64>,
    trigger_source: &str,
    cause: &str,
    log_ctx: &AutoQueueLogContext<'_>,
) -> Result<crate::db::auto_queue::EntryDispatchFailureResult, String> {
    let conn = deps
        .db
        .separate_conn()
        .map_err(|error| format!("{entry_id}: dispatch failure DB open failed: {error}"))?;
    let retry_limit = effective_max_entry_retries(&conn, deps);
    let result = crate::db::auto_queue::record_entry_dispatch_failure_on_conn(
        &conn,
        entry_id,
        retry_limit,
        trigger_source,
    )
    .map_err(|error| format!("{entry_id}: dispatch failure state update failed: {error}"))?;

    if result.changed {
        if let Some(assigned_slot) = slot_index {
            if let Err(error) = crate::db::auto_queue::release_slot_for_group_agent(
                &conn,
                run_id,
                thread_group,
                agent_id,
                assigned_slot,
            ) {
                crate::auto_queue_log!(
                    warn,
                    "entry_dispatch_failure_release_slot_failed",
                    log_ctx.clone().slot_index(assigned_slot),
                    "[auto-queue] failed to release slot {} for entry {} after dispatch failure: {}",
                    assigned_slot,
                    entry_id,
                    error
                );
            }
        }
    }

    if result.changed && result.to_status == crate::db::auto_queue::ENTRY_STATUS_FAILED {
        if let Err(error) = queue_failed_entry_escalation_on_conn(
            &conn,
            deps,
            run_id,
            entry_id,
            card_id,
            agent_id,
            thread_group,
            result.retry_count,
            result.retry_limit,
            cause,
        ) {
            crate::auto_queue_log!(
                warn,
                "entry_dispatch_failure_escalation_failed",
                log_ctx.clone(),
                "[auto-queue] failed to queue escalation for failed entry {}: {}",
                entry_id,
                error
            );
        }
    }

    Ok(result)
}

fn handle_activate_preflight_metadata(
    deps: &AutoQueueActivateDeps,
    run_id: &str,
    entry_id: &str,
    card_id: &str,
    agent_id: &str,
    group: i64,
    batch_phase: i64,
    title: &str,
    metadata: Option<&str>,
) -> ActivatePreflightOutcome {
    let Some(metadata) = metadata else {
        return ActivatePreflightOutcome::Continue;
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return ActivatePreflightOutcome::Continue;
    };
    let log_ctx = AutoQueueLogContext::new()
        .run(run_id)
        .entry(entry_id)
        .card(card_id)
        .agent(agent_id)
        .thread_group(group)
        .batch_phase(batch_phase);

    match parsed.get("preflight_status").and_then(|v| v.as_str()) {
        Some("consult_required") => {
            match deps.db.separate_conn() {
                Ok(conn) => match crate::db::auto_queue::update_entry_status_on_conn(
                    &conn,
                    entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                    "activate_preflight_consultation_reserve",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                ) {
                    Ok(result) if !result.changed => {
                        crate::auto_queue_log!(
                            info,
                            "activate_preflight_consultation_reserve_already_claimed",
                            log_ctx.clone(),
                            "[auto-queue] consultation entry {entry_id} was already reserved before preflight dispatch creation"
                        );
                        return ActivatePreflightOutcome::Deferred;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        crate::auto_queue_log!(
                            warn,
                            "activate_preflight_consultation_reserve_failed",
                            log_ctx.clone(),
                            "[auto-queue] failed to reserve consultation entry {entry_id} before dispatch creation: {error}"
                        );
                        return ActivatePreflightOutcome::Deferred;
                    }
                },
                Err(error) => {
                    crate::auto_queue_log!(
                        warn,
                        "activate_preflight_consultation_reserve_db_open_failed",
                        log_ctx.clone(),
                        "[auto-queue] failed to open DB while reserving consultation entry {entry_id}: {error}"
                    );
                    return ActivatePreflightOutcome::Deferred;
                }
            }

            let consult_agent_id = {
                match deps.db.separate_conn() {
                    Ok(conn) => {
                        let provider = conn
                            .query_row(
                                "SELECT COALESCE(provider, 'claude') FROM agents WHERE id = ?1",
                                [agent_id],
                                |row| row.get::<_, String>(0),
                            )
                            .map(|raw| ProviderKind::from_str_or_unsupported(&raw))
                            .unwrap_or_else(|_| {
                                ProviderKind::default_channel_provider()
                                    .unwrap_or(ProviderKind::Claude)
                            });
                        let available_agents: Vec<(String, ProviderKind)> = conn
                            .prepare(
                                "SELECT id, COALESCE(provider, 'claude')
                                 FROM agents
                                 WHERE id != ?1
                                 ORDER BY id ASC",
                            )
                            .ok()
                            .and_then(|mut stmt| {
                                stmt.query_map([agent_id], |row| {
                                    let provider_raw: String = row.get(1)?;
                                    Ok((
                                        row.get::<_, String>(0)?,
                                        ProviderKind::from_str_or_unsupported(&provider_raw),
                                    ))
                                })
                                .ok()
                                .map(|rows| rows.filter_map(|row| row.ok()).collect())
                            })
                            .unwrap_or_default();
                        provider
                            .select_counterpart_from(
                                available_agents
                                    .iter()
                                    .map(|(_, candidate_provider)| candidate_provider.clone()),
                            )
                            .and_then(|counterpart| {
                                available_agents.iter().find_map(
                                    |(candidate_id, candidate_provider)| {
                                        (*candidate_provider == counterpart)
                                            .then_some(candidate_id.clone())
                                    },
                                )
                            })
                            .unwrap_or_else(|| agent_id.to_string())
                    }
                    Err(error) => {
                        crate::auto_queue_log!(
                            warn,
                            "activate_preflight_consultation_db_open_failed",
                            log_ctx.clone(),
                            "[auto-queue] failed to open DB while selecting consultation counterpart for entry {entry_id}: {error}"
                        );
                        agent_id.to_string()
                    }
                }
            };

            let dispatch_result = run_activate_blocking(|| {
                let dispatch_context = build_auto_queue_dispatch_context(
                    entry_id,
                    group,
                    None,
                    false,
                    [
                        ("run_id", json!(run_id)),
                        ("batch_phase", json!(batch_phase)),
                    ],
                );
                crate::dispatch::create_dispatch(
                    &deps.db,
                    &deps.engine,
                    card_id,
                    &consult_agent_id,
                    "consultation",
                    &format!("[Consultation] {title}"),
                    &dispatch_context,
                )
            });
            let dispatch = match dispatch_result {
                Ok(dispatch) => dispatch,
                Err(error) => {
                    let failure = record_entry_dispatch_failure(
                        deps,
                        run_id,
                        entry_id,
                        card_id,
                        agent_id,
                        group,
                        None,
                        "activate_preflight_consultation_dispatch_failed",
                        &error.to_string(),
                        &log_ctx,
                    );
                    match failure {
                        Ok(result) => crate::auto_queue_log!(
                            warn,
                            "activate_preflight_consultation_dispatch_failed",
                            log_ctx.clone(),
                            "[auto-queue] consultation dispatch failed for entry {entry_id} (group {group}); retry {}/{} -> {}",
                            result.retry_count,
                            result.retry_limit,
                            result.to_status
                        ),
                        Err(record_error) => crate::auto_queue_log!(
                            warn,
                            "activate_preflight_consultation_dispatch_failed",
                            log_ctx.clone(),
                            "[auto-queue] consultation dispatch failed for entry {entry_id} (group {group}); failed to persist retry state: {record_error}"
                        ),
                    }
                    return ActivatePreflightOutcome::Deferred;
                }
            };

            let dispatch_id = dispatch["id"].as_str().unwrap_or("").to_string();
            match deps.db.separate_conn() {
                Ok(mut conn) => {
                    if let Err(error) = crate::db::auto_queue::record_consultation_dispatch_on_conn(
                        &mut conn,
                        entry_id,
                        card_id,
                        &dispatch_id,
                        "activate_preflight_consultation_dispatch",
                        metadata,
                    ) {
                        crate::auto_queue_log!(
                            warn,
                            "activate_preflight_consultation_record_failed",
                            log_ctx.clone().dispatch(&dispatch_id),
                            "[auto-queue] failed to persist consultation dispatch state for entry {entry_id}: {error}"
                        );
                    }
                }
                Err(error) => {
                    crate::auto_queue_log!(
                        warn,
                        "activate_preflight_consultation_record_db_open_failed",
                        log_ctx.clone().dispatch(&dispatch_id),
                        "[auto-queue] failed to open DB while persisting consultation dispatch state for entry {entry_id}: {error}"
                    );
                }
            }
            crate::auto_queue_log!(
                info,
                "activate_preflight_consultation_dispatch_created",
                log_ctx.clone().dispatch(&dispatch_id),
                "[auto-queue] created consultation dispatch for entry {entry_id} (group {group})"
            );
            ActivatePreflightOutcome::Dispatched(deps.entry_json(entry_id))
        }
        Some("invalid") | Some("already_applied") => {
            match deps.db.separate_conn() {
                Ok(conn) => {
                    if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
                        &conn,
                        entry_id,
                        crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
                        "activate_preflight_invalid",
                        &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                    ) {
                        crate::auto_queue_log!(
                            warn,
                            "activate_preflight_invalid_skip_failed",
                            log_ctx.clone(),
                            "[auto-queue] failed to skip preflight-invalid entry {entry_id}: {error}"
                        );
                    }
                }
                Err(error) => {
                    crate::auto_queue_log!(
                        warn,
                        "activate_preflight_invalid_db_open_failed",
                        log_ctx.clone(),
                        "[auto-queue] failed to open DB while skipping preflight-invalid entry {entry_id}: {error}"
                    );
                }
            }
            crate::auto_queue_log!(
                info,
                "activate_preflight_skipped",
                log_ctx,
                "[auto-queue] skipping entry {entry_id} for card {card_id} due to preflight_status={}",
                parsed
                    .get("preflight_status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            );
            ActivatePreflightOutcome::Skipped
        }
        _ => ActivatePreflightOutcome::Continue,
    }
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
            thread_group: entry.thread_group,
        });
    }

    Ok(Some(normalized))
}

fn normalize_dispatch_entries(body: &DispatchBody) -> Result<Vec<GenerateEntryBody>, String> {
    if body.groups.is_empty() {
        return Err("groups must contain at least one issue group".to_string());
    }

    let mut entries = Vec::new();
    let mut seen_issues = HashSet::new();
    let mut seen_groups = HashSet::new();

    for (index, group) in body.groups.iter().enumerate() {
        if group.issues.is_empty() {
            return Err(format!("groups[{index}] must contain at least one issue"));
        }

        let thread_group = group.thread_group.unwrap_or(index as i64);
        if thread_group < 0 {
            return Err(format!("groups[{index}].thread_group must be >= 0"));
        }
        if !seen_groups.insert(thread_group) {
            return Err(format!(
                "duplicate thread_group in dispatch payload: {thread_group}"
            ));
        }

        let batch_phase = group.batch_phase.unwrap_or(0);
        if batch_phase < 0 {
            return Err(format!("groups[{index}].batch_phase must be >= 0"));
        }

        if group.sequential == Some(false) && group.issues.len() > 1 {
            return Err(format!(
                "groups[{index}] sets sequential=false, but multi-issue groups are always sequential"
            ));
        }

        for issue_number in &group.issues {
            if !seen_issues.insert(*issue_number) {
                return Err(format!(
                    "duplicate issue_number in dispatch payload: {issue_number}"
                ));
            }
            entries.push(GenerateEntryBody {
                issue_number: *issue_number,
                batch_phase: Some(batch_phase),
                thread_group: Some(thread_group),
            });
        }
    }

    Ok(entries)
}

fn resolve_dispatch_cards(
    conn: &libsql_rusqlite::Connection,
    repo: Option<&String>,
    issue_numbers: &[i64],
) -> Result<HashMap<i64, ResolvedDispatchCard>, String> {
    if issue_numbers.is_empty() {
        return Ok(HashMap::new());
    }

    let mut params: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();
    let mut conditions = Vec::new();

    if let Some(repo) = repo {
        conditions.push(format!("repo_id = ?{}", params.len() + 1));
        params.push(Box::new(repo.clone()));
    }

    let placeholders = issue_numbers
        .iter()
        .enumerate()
        .map(|(index, _)| format!("?{}", params.len() + index + 1))
        .collect::<Vec<_>>()
        .join(",");
    conditions.push(format!("github_issue_number IN ({placeholders})"));
    for issue_number in issue_numbers {
        params.push(Box::new(*issue_number));
    }

    let sql = format!(
        "SELECT id, repo_id, status, assigned_agent_id, github_issue_number
         FROM kanban_cards
         WHERE {}",
        conditions.join(" AND ")
    );
    let param_refs: Vec<&dyn libsql_rusqlite::types::ToSql> =
        params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql).map_err(|err| format!("{err}"))?;
    let rows: Vec<ResolvedDispatchCard> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(ResolvedDispatchCard {
                card_id: row.get(0)?,
                repo_id: row.get(1)?,
                status: row.get(2)?,
                assigned_agent_id: row.get(3)?,
                issue_number: row.get(4)?,
            })
        })
        .map_err(|err| format!("{err}"))?
        .filter_map(|row| row.ok())
        .collect();
    drop(stmt);

    let mut cards_by_issue = HashMap::new();
    for card in rows {
        if cards_by_issue
            .insert(card.issue_number, card.clone())
            .is_some()
        {
            return Err(format!(
                "multiple kanban cards matched issue #{}; specify repo to disambiguate",
                card.issue_number
            ));
        }
    }

    for issue_number in issue_numbers {
        if !cards_by_issue.contains_key(issue_number) {
            let suffix = repo
                .map(|repo| format!(" in repo {repo}"))
                .unwrap_or_default();
            return Err(format!(
                "kanban card not found for issue #{issue_number}{suffix}"
            ));
        }
    }

    Ok(cards_by_issue)
}

async fn resolve_dispatch_cards_with_pg(
    pool: &sqlx::PgPool,
    repo: Option<&String>,
    issue_numbers: &[i64],
) -> Result<HashMap<i64, ResolvedDispatchCard>, String> {
    if issue_numbers.is_empty() {
        return Ok(HashMap::new());
    }

    let rows = sqlx::query(
        "SELECT id,
                repo_id,
                status,
                assigned_agent_id,
                github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards
         WHERE ($1::TEXT IS NULL OR repo_id = $1)
           AND github_issue_number::BIGINT = ANY($2::BIGINT[])",
    )
    .bind(repo.map(String::as_str))
    .bind(issue_numbers.to_vec())
    .fetch_all(pool)
    .await
    .map_err(|err| format!("{err}"))?;

    let mut cards_by_issue = HashMap::new();
    for row in rows {
        let card = ResolvedDispatchCard {
            card_id: row.try_get("id").map_err(|err| format!("{err}"))?,
            repo_id: row.try_get("repo_id").map_err(|err| format!("{err}"))?,
            status: row.try_get("status").map_err(|err| format!("{err}"))?,
            assigned_agent_id: row
                .try_get("assigned_agent_id")
                .map_err(|err| format!("{err}"))?,
            issue_number: row
                .try_get("github_issue_number")
                .map_err(|err| format!("{err}"))?,
        };
        if cards_by_issue
            .insert(card.issue_number, card.clone())
            .is_some()
        {
            return Err(format!(
                "multiple kanban cards matched issue #{}; specify repo to disambiguate",
                card.issue_number
            ));
        }
    }

    for issue_number in issue_numbers {
        if !cards_by_issue.contains_key(issue_number) {
            let suffix = repo
                .map(|repo| format!(" in repo {repo}"))
                .unwrap_or_default();
            return Err(format!(
                "kanban card not found for issue #{issue_number}{suffix}"
            ));
        }
    }

    Ok(cards_by_issue)
}

fn apply_dispatch_agent_assignments(
    conn: &libsql_rusqlite::Connection,
    cards_by_issue: &mut HashMap<i64, ResolvedDispatchCard>,
    agent_id: Option<&str>,
    auto_assign_agent: bool,
) -> Result<(), String> {
    let target_agent = agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    for issue_number in cards_by_issue.keys().copied().collect::<Vec<_>>() {
        let Some(card) = cards_by_issue.get_mut(&issue_number) else {
            continue;
        };
        let current_agent = card
            .assigned_agent_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        match (target_agent.as_deref(), current_agent.as_deref()) {
            (Some(target), Some(current)) if current != target => {
                let repo_hint = card
                    .repo_id
                    .as_deref()
                    .map(|repo| format!(" in repo {repo}"))
                    .unwrap_or_default();
                return Err(format!(
                    "issue #{issue_number}{repo_hint} is assigned to {current}, not {target}"
                ));
            }
            (Some(target), None) if auto_assign_agent => {
                conn.execute(
                    "UPDATE kanban_cards
                     SET assigned_agent_id = ?1,
                         updated_at = datetime('now')
                     WHERE id = ?2
                       AND (assigned_agent_id IS NULL OR TRIM(assigned_agent_id) = '')",
                    libsql_rusqlite::params![target, card.card_id],
                )
                .map_err(|err| format!("{err}"))?;
                card.assigned_agent_id = Some(target.to_string());
            }
            (Some(_), None) => {
                return Err(format!(
                    "issue #{issue_number} has no assigned agent; provide auto_assign_agent=true or assign it first"
                ));
            }
            (None, None) => {
                return Err(format!(
                    "issue #{issue_number} has no assigned agent; provide agent_id or assign it first"
                ));
            }
            _ => {}
        }
    }

    Ok(())
}

fn validate_dispatchable_cards(
    conn: &libsql_rusqlite::Connection,
    cards_by_issue: &HashMap<i64, ResolvedDispatchCard>,
) -> Result<(), String> {
    crate::pipeline::ensure_loaded();

    for card in cards_by_issue.values() {
        if card.status == "backlog" {
            continue;
        }

        let effective = crate::pipeline::resolve_for_card(
            conn,
            card.repo_id.as_deref(),
            card.assigned_agent_id.as_deref(),
        );
        let enqueueable_states = enqueueable_states_for(&effective);
        if enqueueable_states.iter().any(|state| state == &card.status) {
            continue;
        }

        return Err(format!(
            "issue #{} is in status '{}' and cannot be auto-queued; allowed states are backlog or {}",
            card.issue_number,
            card.status,
            enqueueable_states.join(", ")
        ));
    }

    Ok(())
}

fn find_matching_active_run_id(
    conn: &libsql_rusqlite::Connection,
    repo: Option<&str>,
    agent_id: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let mut sql =
        String::from("SELECT id, status FROM auto_queue_runs WHERE status IN ('active', 'paused')");
    let mut params: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();

    if let Some(repo) = repo.map(str::trim).filter(|value| !value.is_empty()) {
        params.push(Box::new(repo.to_string()));
        sql.push_str(&format!(
            " AND (repo = ?{} OR repo IS NULL OR repo = '')",
            params.len()
        ));
    }
    if let Some(agent_id) = agent_id.map(str::trim).filter(|value| !value.is_empty()) {
        params.push(Box::new(agent_id.to_string()));
        sql.push_str(&format!(
            " AND (agent_id = ?{} OR agent_id IS NULL OR agent_id = '')",
            params.len()
        ));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC");

    let param_refs: Vec<&dyn libsql_rusqlite::types::ToSql> =
        params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|err| format!("prepare live run lookup: {err}"))?;
    stmt.query_map(param_refs.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|err| format!("query live runs: {err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect live runs: {err}"))
}

#[derive(Debug)]
struct AddedRunEntry {
    entry_id: String,
    thread_group: i64,
    priority_rank: i64,
}

fn enqueue_entries_into_existing_run(
    conn: &mut libsql_rusqlite::Connection,
    run_id: &str,
    requested_entries: &[GenerateEntryBody],
    cards_by_issue: &HashMap<i64, ResolvedDispatchCard>,
) -> Result<Vec<AddedRunEntry>, String> {
    let existing_live_cards: HashSet<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT kanban_card_id
                 FROM auto_queue_entries
                 WHERE run_id = ?1
                   AND status IN ('pending', 'dispatched')",
            )
            .map_err(|err| format!("prepare existing queued cards: {err}"))?;
        stmt.query_map([run_id], |row| row.get::<_, String>(0))
            .map_err(|err| format!("query existing queued cards: {err}"))?
            .filter_map(|row| row.ok())
            .collect()
    };

    let mut next_rank_by_group: HashMap<i64, i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT COALESCE(thread_group, 0), COALESCE(MAX(priority_rank), -1) + 1
                 FROM auto_queue_entries
                 WHERE run_id = ?1
                 GROUP BY COALESCE(thread_group, 0)",
            )
            .map_err(|err| format!("prepare group ranks: {err}"))?;
        stmt.query_map([run_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|err| format!("query group ranks: {err}"))?
        .filter_map(|row| row.ok())
        .collect()
    };
    let mut next_auto_group = conn
        .query_row(
            "SELECT COALESCE(MAX(COALESCE(thread_group, 0)), -1) + 1
             FROM auto_queue_entries
             WHERE run_id = ?1",
            [run_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|err| format!("query next thread group: {err}"))?;
    let mut existing_live_cards = existing_live_cards;
    let tx = conn
        .transaction()
        .map_err(|err| format!("begin enqueue transaction: {err}"))?;
    let mut inserted = Vec::new();

    for entry in requested_entries {
        let Some(card) = cards_by_issue.get(&entry.issue_number) else {
            continue;
        };
        if existing_live_cards.contains(&card.card_id) {
            return Err(format!(
                "issue #{} is already queued in run {run_id}",
                entry.issue_number
            ));
        }

        let has_active_dispatch: bool = tx
            .query_row(
                "SELECT COUNT(*) > 0
                 FROM task_dispatches
                 WHERE kanban_card_id = ?1
                   AND status IN ('pending', 'dispatched')",
                [&card.card_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if has_active_dispatch {
            return Err(format!(
                "issue #{} already has an active dispatch and cannot be queued again",
                entry.issue_number
            ));
        }

        let thread_group = entry.thread_group.unwrap_or_else(|| {
            let chosen = next_auto_group;
            next_auto_group += 1;
            chosen
        });
        let priority_rank = *next_rank_by_group.entry(thread_group).or_insert(0);
        next_rank_by_group.insert(thread_group, priority_rank + 1);
        let entry_id = uuid::Uuid::new_v4().to_string();

        tx.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, batch_phase, reason
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
             )",
            libsql_rusqlite::params![
                &entry_id,
                run_id,
                &card.card_id,
                card.assigned_agent_id.as_deref().unwrap_or(""),
                priority_rank,
                thread_group,
                entry.batch_phase.unwrap_or(0),
                format!("manual run entry add for issue #{}", entry.issue_number),
            ],
        )
        .map_err(|err| format!("insert auto-queue entry: {err}"))?;
        existing_live_cards.insert(card.card_id.clone());
        inserted.push(AddedRunEntry {
            entry_id,
            thread_group,
            priority_rank,
        });
    }

    if !inserted.is_empty() {
        crate::db::auto_queue::sync_run_group_metadata(&tx, run_id)
            .map_err(|err| format!("sync run group metadata: {err}"))?;
    }

    tx.commit()
        .map_err(|err| format!("commit enqueue transaction: {err}"))?;
    Ok(inserted)
}

async fn sync_run_group_metadata_with_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let thread_group_count = sqlx::query_scalar::<_, i64>(
        "SELECT GREATEST(
                COALESCE(COUNT(DISTINCT COALESCE(thread_group, 0)), 0),
                1
            )::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|err| format!("count thread groups for run {run_id}: {err}"))?;

    sqlx::query(
        "UPDATE auto_queue_runs
         SET thread_group_count = $1,
             max_concurrent_threads = $1
         WHERE id = $2",
    )
    .bind(thread_group_count)
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|err| format!("sync run group metadata for {run_id}: {err}"))?;
    Ok(())
}

async fn enqueue_entries_into_existing_run_with_pg(
    pool: &sqlx::PgPool,
    run_id: &str,
    requested_entries: &[GenerateEntryBody],
    cards_by_issue: &HashMap<i64, ResolvedDispatchCard>,
) -> Result<Vec<AddedRunEntry>, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|err| format!("begin enqueue transaction: {err}"))?;

    let existing_live_cards: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT kanban_card_id
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|err| format!("query existing queued cards: {err}"))?
    .into_iter()
    .collect();

    let mut next_rank_by_group = HashMap::new();
    for row in sqlx::query(
        "SELECT COALESCE(thread_group, 0)::BIGINT AS thread_group,
                (COALESCE(MAX(priority_rank), -1) + 1)::BIGINT AS next_priority_rank
         FROM auto_queue_entries
         WHERE run_id = $1
         GROUP BY COALESCE(thread_group, 0)",
    )
    .bind(run_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|err| format!("query group ranks: {err}"))?
    {
        let thread_group: i64 = row
            .try_get("thread_group")
            .map_err(|err| format!("decode thread_group: {err}"))?;
        let next_priority_rank: i64 = row
            .try_get("next_priority_rank")
            .map_err(|err| format!("decode next_priority_rank: {err}"))?;
        next_rank_by_group.insert(thread_group, next_priority_rank);
    }

    let mut next_auto_group = sqlx::query_scalar::<_, i64>(
        "SELECT (COALESCE(MAX(COALESCE(thread_group, 0)), -1) + 1)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| format!("query next thread group: {err}"))?;

    let mut existing_live_cards = existing_live_cards;
    let mut inserted = Vec::new();

    for entry in requested_entries {
        let Some(card) = cards_by_issue.get(&entry.issue_number) else {
            continue;
        };
        if existing_live_cards.contains(&card.card_id) {
            return Err(format!(
                "issue #{} is already queued in run {run_id}",
                entry.issue_number
            ));
        }

        let has_active_dispatch = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND status IN ('pending', 'dispatched')",
        )
        .bind(&card.card_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| format!("query active dispatches for {}: {err}", card.card_id))?;
        if has_active_dispatch > 0 {
            return Err(format!(
                "issue #{} already has an active dispatch and cannot be queued again",
                entry.issue_number
            ));
        }

        let thread_group = entry.thread_group.unwrap_or_else(|| {
            let chosen = next_auto_group;
            next_auto_group += 1;
            chosen
        });
        let priority_rank = *next_rank_by_group.entry(thread_group).or_insert(0);
        next_rank_by_group.insert(thread_group, priority_rank + 1);
        let entry_id = uuid::Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, batch_phase, reason
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8
             )",
        )
        .bind(&entry_id)
        .bind(run_id)
        .bind(&card.card_id)
        .bind(card.assigned_agent_id.as_deref().unwrap_or(""))
        .bind(priority_rank)
        .bind(thread_group)
        .bind(entry.batch_phase.unwrap_or(0))
        .bind(format!(
            "manual run entry add for issue #{}",
            entry.issue_number
        ))
        .execute(&mut *tx)
        .await
        .map_err(|err| format!("insert auto-queue entry: {err}"))?;

        existing_live_cards.insert(card.card_id.clone());
        inserted.push(AddedRunEntry {
            entry_id,
            thread_group,
            priority_rank,
        });
    }

    if !inserted.is_empty() {
        sync_run_group_metadata_with_pg_tx(&mut tx, run_id).await?;
    }

    tx.commit()
        .await
        .map_err(|err| format!("commit enqueue transaction: {err}"))?;
    Ok(inserted)
}

fn existing_live_run_conflict_response(
    run_id: &str,
    status: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": format!(
                "live auto-queue run already exists: run_id={run_id}, status={status}; pass force=true to cancel it before creating a new run"
            ),
            "existing_run_id": run_id,
            "existing_run_status": status,
        })),
    )
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

fn priority_sort_key(priority: &str) -> i32 {
    match priority {
        "urgent" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn planning_sort_key(card: &GenerateCandidate, idx: usize) -> (i32, usize) {
    (priority_sort_key(&card.priority), idx)
}

fn dependency_issue_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"#(\d+)").expect("dependency regex must compile"))
}

fn dependency_section_header_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^\s*(?:#{1,6}\s*)?(dependencies?|dependency|depends on|선행 작업|선행작업|의존성)\s*:?\s*$",
        )
        .expect("dependency section regex must compile")
    })
}

fn dependency_inline_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^\s*(?:[-*]\s*)?(?:#{1,6}\s*)?(dependencies?|dependency|depends on|선행 작업|선행작업|의존성)\s*:?\s+(.+)$",
        )
        .expect("dependency inline regex must compile")
    })
}

fn markdown_header_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^#{1,6}\s").expect("markdown header regex must compile"))
}

fn bare_dependency_list_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^\s*#\d+(?:[\s,]+#\d+)*\s*$")
            .expect("dependency bare-list regex must compile")
    })
}

fn insert_dependency_number(deps: &mut HashSet<i64>, self_issue_number: Option<i64>, num: i64) {
    if Some(num) != self_issue_number {
        deps.insert(num);
    }
}

fn collect_dependency_numbers_from_issue_refs(
    text: &str,
    deps: &mut HashSet<i64>,
    self_issue_number: Option<i64>,
) -> bool {
    let mut matched = false;
    for cap in dependency_issue_regex().captures_iter(text) {
        if let Ok(num) = cap[1].parse::<i64>() {
            matched = true;
            insert_dependency_number(deps, self_issue_number, num);
        }
    }
    matched
}

fn collect_dependency_numbers_from_json_value(
    value: &Value,
    deps: &mut HashSet<i64>,
    self_issue_number: Option<i64>,
) -> bool {
    match value {
        Value::Number(num) => num
            .as_i64()
            .map(|issue_number| {
                insert_dependency_number(deps, self_issue_number, issue_number);
                true
            })
            .unwrap_or(false),
        Value::String(raw) => {
            let trimmed = raw.trim();
            let mut matched =
                collect_dependency_numbers_from_issue_refs(trimmed, deps, self_issue_number);
            if let Ok(issue_number) = trimmed.trim_start_matches('#').parse::<i64>() {
                insert_dependency_number(deps, self_issue_number, issue_number);
                matched = true;
            }
            matched
        }
        Value::Array(items) => {
            let mut matched = false;
            for item in items {
                matched |=
                    collect_dependency_numbers_from_json_value(item, deps, self_issue_number);
            }
            matched
        }
        _ => false,
    }
}

fn extract_dependency_numbers_from_text(
    text: &str,
    source_label: &str,
    allow_bare_ref_list: bool,
    deps: &mut HashSet<i64>,
    signals: &mut HashSet<String>,
    self_issue_number: Option<i64>,
) {
    let trimmed = text.trim();
    if allow_bare_ref_list && bare_dependency_list_regex().is_match(trimmed) {
        if collect_dependency_numbers_from_issue_refs(trimmed, deps, self_issue_number) {
            signals.insert(format!("{source_label}:bare-list"));
        }
        return;
    }

    let mut active_section: Option<String> = None;
    for line in text.lines() {
        let trimmed_line = line.trim();
        if trimmed_line.is_empty() {
            continue;
        }

        if dependency_section_header_regex().is_match(trimmed_line) {
            active_section = Some(trimmed_line.to_string());
            continue;
        }

        if active_section.is_some() && markdown_header_regex().is_match(trimmed_line) {
            active_section = None;
        }

        if let Some(caps) = dependency_inline_regex().captures(trimmed_line) {
            let signal = format!("{source_label}:inline:{}", caps[1].trim().to_lowercase());
            if let Some(rest) = caps.get(2) {
                if collect_dependency_numbers_from_issue_refs(
                    rest.as_str(),
                    deps,
                    self_issue_number,
                ) {
                    signals.insert(signal);
                }
            }
            continue;
        }

        if let Some(section_label) = active_section.as_ref() {
            if collect_dependency_numbers_from_issue_refs(trimmed_line, deps, self_issue_number) {
                signals.insert(format!("{source_label}:section:{section_label}"));
            }
        }
    }
}

fn extract_dependency_parse_result(card: &GenerateCandidate) -> DependencyParseResult {
    let mut deps = HashSet::new();
    let mut signals = HashSet::new();

    if let Some(description) = card.description.as_deref() {
        extract_dependency_numbers_from_text(
            description,
            "description",
            false,
            &mut deps,
            &mut signals,
            card.github_issue_number,
        );
    }

    if let Some(metadata) = card.metadata.as_deref() {
        if let Ok(value) = serde_json::from_str::<Value>(metadata) {
            if let Some(object) = value.as_object() {
                for (key, field_value) in object {
                    if key.eq_ignore_ascii_case("depends_on")
                        || key.eq_ignore_ascii_case("dependencies")
                    {
                        if collect_dependency_numbers_from_json_value(
                            field_value,
                            &mut deps,
                            card.github_issue_number,
                        ) {
                            signals.insert(format!("metadata:json:{key}"));
                        }
                    }
                }
            }
        } else {
            extract_dependency_numbers_from_text(
                metadata,
                "metadata",
                true,
                &mut deps,
                &mut signals,
                card.github_issue_number,
            );
        }
    }

    let mut numbers: Vec<i64> = deps.into_iter().collect();
    numbers.sort_unstable();
    let mut signals: Vec<String> = signals.into_iter().collect();
    signals.sort();

    DependencyParseResult { numbers, signals }
}

fn extract_dependency_numbers(card: &GenerateCandidate) -> Vec<i64> {
    extract_dependency_parse_result(card).numbers
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

fn build_group_plan(cards: &[GenerateCandidate]) -> GroupPlan {
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

    let similarity_paths_per_card: Vec<HashSet<String>> =
        cards.iter().map(similarity_paths).collect();
    let dependency_numbers: Vec<Vec<i64>> = cards.iter().map(extract_dependency_numbers).collect();
    let path_backed_card_count = similarity_paths_per_card
        .iter()
        .filter(|paths| !paths.is_empty())
        .count();

    let n = cards.len();
    let mut dependency_adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut dependency_predecessors: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut similarity_conflicts: Vec<HashSet<usize>> = vec![HashSet::new(); n];
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
                    dependency_predecessors[idx].push(dep_idx);
                    union(&mut parent, dep_idx, idx);
                    dependency_edges += 1;
                }
            }
        }
    }

    let dependency_roots: Vec<usize> = (0..n).map(|idx| find(&mut parent, idx)).collect();

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
            similarity_edges += 1;
            if dependency_roots[left] != dependency_roots[right] {
                similarity_conflicts[left].insert(right);
                similarity_conflicts[right].insert(left);
            }
        }
    }

    let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..n {
        let root = dependency_roots[idx];
        components.entry(root).or_default().push(idx);
    }

    let mut component_roots: Vec<usize> = components.keys().copied().collect();
    component_roots
        .sort_by_key(|root| components[root].iter().copied().min().unwrap_or(usize::MAX));

    let mut planned_entries = Vec::with_capacity(n);
    for (group_num, root) in component_roots.iter().enumerate() {
        let mut members = components[root].clone();
        members.sort_by_key(|idx| planning_sort_key(&cards[*idx], *idx));
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
            available.sort_by_key(|idx| planning_sort_key(&cards[*idx], *idx));
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
                batch_phase: 0,
                reason: entry_reason,
            });
        }
    }

    let mut global_in_degree: Vec<usize> = dependency_predecessors
        .iter()
        .map(|preds| preds.len())
        .collect();
    let mut ready: Vec<usize> = (0..n).filter(|idx| global_in_degree[*idx] == 0).collect();
    let mut dependency_order = Vec::with_capacity(n);
    let mut emitted = vec![false; n];

    while !ready.is_empty() {
        ready.sort_by_key(|idx| planning_sort_key(&cards[*idx], *idx));
        let current = ready.remove(0);
        if emitted[current] {
            continue;
        }
        emitted[current] = true;
        dependency_order.push(current);
        for &next in &dependency_adj[current] {
            if global_in_degree[next] > 0 {
                global_in_degree[next] -= 1;
                if global_in_degree[next] == 0 {
                    ready.push(next);
                }
            }
        }
    }

    if dependency_order.len() < n {
        let mut remaining: Vec<usize> = (0..n).filter(|idx| !emitted[*idx]).collect();
        remaining.sort_by_key(|idx| planning_sort_key(&cards[*idx], *idx));
        dependency_order.extend(remaining);
    }

    let mut batch_phase_by_idx = vec![0i64; n];
    let mut phase_assigned = vec![false; n];
    for idx in dependency_order {
        let earliest_phase = dependency_predecessors[idx]
            .iter()
            .copied()
            .filter(|pred| phase_assigned[*pred])
            .map(|pred| batch_phase_by_idx[pred] + 1)
            .max()
            .unwrap_or(0);
        let mut batch_phase = earliest_phase;
        while similarity_conflicts[idx]
            .iter()
            .copied()
            .filter(|other| phase_assigned[*other])
            .any(|other| batch_phase_by_idx[other] == batch_phase)
        {
            batch_phase += 1;
        }
        batch_phase_by_idx[idx] = batch_phase;
        phase_assigned[idx] = true;
    }

    for planned in &mut planned_entries {
        planned.batch_phase = batch_phase_by_idx[planned.card_idx];
    }

    let thread_group_count = component_roots.len() as i64;
    let recommended_parallel_threads = if thread_group_count <= 1 {
        1
    } else {
        thread_group_count.clamp(1, 4)
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
    let guild_id = state.config.discord.guild_id.as_deref();
    let _ignored_unified_thread = body.unified_thread.is_some();
    let force = body.force.unwrap_or(false);
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
    // (index, batch_phase, thread_group)
    let requested_entry_meta: HashMap<i64, (usize, i64, Option<i64>)> = requested_entries
        .as_ref()
        .map(|entries| {
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    (
                        entry.issue_number,
                        (index, entry.batch_phase, entry.thread_group),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let conn = match state.db.separate_conn() {
        Ok(conn) => conn,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };
    let conflicting_live_runs =
        match find_matching_active_run_id(&conn, body.repo.as_deref(), body.agent_id.as_deref()) {
            Ok(runs) => runs,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };
    if let Some((run_id, status)) = conflicting_live_runs.first() {
        if !force {
            return existing_live_run_conflict_response(run_id, status);
        }
        let target_run_ids: Vec<String> = conflicting_live_runs
            .iter()
            .map(|(run_id, _)| run_id.clone())
            .collect();
        crate::services::auto_queue::cancel_run::cancel_selected_runs_with_conn(
            state.health_registry.clone(),
            &conn,
            &target_run_ids,
            "auto_queue_force_new_run",
        );
    }
    drop(conn);
    let mut cards: Vec<GenerateCandidate> = match state.auto_queue_service().prepare_generate_cards(
        &crate::services::auto_queue::PrepareGenerateInput {
            repo: body.repo.clone(),
            agent_id: body.agent_id.clone(),
            issue_numbers: requested_issue_numbers.clone(),
        },
    ) {
        Ok(cards) => cards
            .into_iter()
            .map(|card| GenerateCandidate {
                card_id: card.card_id,
                agent_id: card.agent_id,
                priority: card.priority,
                description: card.description,
                metadata: card.metadata,
                github_issue_number: card.github_issue_number,
            })
            .collect(),
        Err(error) => return error.into_json_response(),
    };

    if !requested_entry_meta.is_empty() {
        cards.sort_by_key(|card| {
            card.github_issue_number
                .and_then(|issue_number| requested_entry_meta.get(&issue_number).copied())
                .map(|(index, _, _)| index)
                .unwrap_or(usize::MAX)
        });
    }

    if cards.is_empty() {
        let mut counts_map = serde_json::Map::new();
        if let Some(pipeline) = crate::pipeline::try_get() {
            for pipeline_state in &pipeline.states {
                if !pipeline_state.terminal {
                    let c = state
                        .auto_queue_service()
                        .count_cards_by_status(
                            body.repo.as_deref(),
                            body.agent_id.as_deref(),
                            &pipeline_state.id,
                        )
                        .unwrap_or(0);
                    counts_map.insert(pipeline_state.id.clone(), serde_json::json!(c));
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

    let mut conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let issue_to_idx: HashMap<i64, usize> = cards
        .iter()
        .enumerate()
        .filter_map(|(idx, card)| {
            card.github_issue_number
                .map(|issue_number| (issue_number, idx))
        })
        .collect();
    let mut filtered_cards = Vec::with_capacity(cards.len());
    let mut dependency_status_cache: HashMap<i64, Option<String>> = HashMap::new();
    let mut excluded_count = 0usize;
    for card in &cards {
        let dep_parse = extract_dependency_parse_result(card);
        crate::auto_queue_log!(
            info,
            "generate.dependency_parse",
            AutoQueueLogContext::new()
                .card(card.card_id.as_str())
                .agent(card.agent_id.as_str()),
            "issue_number={} parsed_dependencies={:?} signals={:?}",
            card.github_issue_number
                .map(|issue_number| format!("#{issue_number}"))
                .unwrap_or_else(|| "<none>".to_string()),
            dep_parse.numbers,
            dep_parse.signals
        );

        let unresolved_external_dependencies: Vec<String> = dep_parse
            .numbers
            .iter()
            .filter_map(|dep_num| {
                if issue_to_idx.contains_key(dep_num) {
                    return None;
                }
                let dep_status = dependency_status_cache
                    .entry(*dep_num)
                    .or_insert_with(|| {
                        conn.query_row(
                            "SELECT status FROM kanban_cards WHERE github_issue_number = ?1",
                            [dep_num],
                            |row| row.get(0),
                        )
                        .ok()
                    })
                    .clone();
                if dep_status.as_deref() == Some("done") {
                    None
                } else {
                    Some(format!(
                        "#{dep_num}:{}",
                        dep_status.as_deref().unwrap_or("missing")
                    ))
                }
            })
            .collect();

        if unresolved_external_dependencies.is_empty() {
            filtered_cards.push(card.clone());
        } else {
            crate::auto_queue_log!(
                info,
                "generate.exclude_unresolved_dependencies",
                AutoQueueLogContext::new()
                    .card(card.card_id.as_str())
                    .agent(card.agent_id.as_str()),
                "issue_number={} unresolved_external_dependencies={:?}",
                card.github_issue_number
                    .map(|issue_number| format!("#{issue_number}"))
                    .unwrap_or_else(|| "<none>".to_string()),
                unresolved_external_dependencies
            );
            excluded_count += 1;
        }
    }

    if filtered_cards.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": format!("No cards available ({}개 외부 의존성 미충족으로 제외)", excluded_count)
            })),
        );
    }

    let plan = build_group_plan(&filtered_cards);
    let mut grouped_entries = plan.entries.clone();
    let mut thread_group_count = plan.thread_group_count.max(1);
    let mut recommended_parallel_threads = plan.recommended_parallel_threads.max(1);
    let dependency_edges = plan.dependency_edges;
    let similarity_edges = plan.similarity_edges;
    let path_backed_card_count = plan.path_backed_card_count;
    let mut max_concurrent = body
        .max_concurrent_threads
        .unwrap_or(recommended_parallel_threads)
        .clamp(1, 10)
        .min(thread_group_count.max(1));

    // Apply explicit batch_phase/thread_group overrides from API entries.
    if !requested_entry_meta.is_empty() {
        let mut has_explicit_groups = false;
        for planned in &mut grouped_entries {
            let card = &filtered_cards[planned.card_idx];
            if let Some(issue_number) = card.github_issue_number {
                if let Some(&(_, batch_phase, thread_group)) =
                    requested_entry_meta.get(&issue_number)
                {
                    planned.batch_phase = batch_phase;
                    if let Some(tg) = thread_group {
                        planned.thread_group = tg;
                        has_explicit_groups = true;
                    }
                }
            }
        }
        if has_explicit_groups {
            thread_group_count = grouped_entries
                .iter()
                .map(|e| e.thread_group)
                .collect::<std::collections::HashSet<_>>()
                .len() as i64;
            recommended_parallel_threads = thread_group_count.clamp(1, 4);
            if let Some(requested_max) = body.max_concurrent_threads {
                max_concurrent = requested_max.clamp(1, 10).min(thread_group_count.max(1));
            } else {
                max_concurrent = recommended_parallel_threads;
            }
        }
    }

    let batch_phase_count = grouped_entries
        .iter()
        .map(|entry| entry.batch_phase)
        .max()
        .unwrap_or(0)
        + 1;
    let ai_rationale = if path_backed_card_count == 0 && dependency_edges == 0 {
        format!(
            "스마트 플래너: 의존성/파일 경로 신호가 약해 {}개 독립 그룹, {}개 페이즈로 계획. {}개 카드 큐잉, 추천 병렬 {}개, 적용 {}개",
            thread_group_count,
            batch_phase_count,
            filtered_cards.len(),
            recommended_parallel_threads,
            max_concurrent
        )
    } else if path_backed_card_count == 0 {
        format!(
            "스마트 플래너: 파일 경로 신호 없이 의존성 {}건으로 {}개 그룹, {}개 페이즈 계획. {}개 카드 큐잉, {}개 외부 의존성 미충족 제외, 추천 병렬 {}개, 적용 {}개",
            dependency_edges,
            thread_group_count,
            batch_phase_count,
            filtered_cards.len(),
            excluded_count,
            recommended_parallel_threads,
            max_concurrent
        )
    } else {
        format!(
            "스마트 플래너: 파일 경로 유사도 {}건 + 의존성 {}건으로 {}개 그룹, {}개 페이즈 계획. 파일 경로 추출 카드 {}개, {}개 카드 큐잉, {}개 외부 의존성 미충족 제외, 추천 병렬 {}개, 적용 {}개",
            similarity_edges,
            dependency_edges,
            thread_group_count,
            batch_phase_count,
            path_backed_card_count,
            filtered_cards.len(),
            excluded_count,
            recommended_parallel_threads,
            max_concurrent
        )
    };

    // Create run + entries atomically so partial inserts cannot masquerade as success.
    let run_id = uuid::Uuid::new_v4().to_string();
    let ai_model_str = "smart-planner".to_string();
    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("begin auto-queue generate transaction: {error}")})),
            );
        }
    };
    if let Err(error) = tx.execute(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status, ai_model, ai_rationale, unified_thread, max_concurrent_threads, thread_group_count) \
         VALUES (?1, ?2, ?3, 'generated', ?4, ?5, 0, ?6, ?7)",
        libsql_rusqlite::params![
            run_id,
            body.repo,
            body.agent_id,
            ai_model_str,
            ai_rationale,
            max_concurrent,
            thread_group_count
        ],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("create auto-queue run: {error}")})),
        );
    }

    let mut entry_ids = Vec::new();
    for planned in &grouped_entries {
        let card = &filtered_cards[planned.card_idx];
        let entry_id = uuid::Uuid::new_v4().to_string();
        let agent = if card.agent_id.is_empty() {
            body.agent_id.as_deref().unwrap_or("")
        } else {
            card.agent_id.as_str()
        };
        if let Err(error) = tx.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, reason, batch_phase)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            libsql_rusqlite::params![
                entry_id,
                run_id,
                card.card_id,
                agent,
                planned.priority_rank,
                planned.thread_group,
                planned.reason,
                planned.batch_phase
            ],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("create auto-queue entry: {error}")})),
            );
        }
        entry_ids.push(entry_id);
    }
    if let Err(error) = tx.commit() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("commit auto-queue generate transaction: {error}")})),
        );
    }

    let entries = entry_ids
        .iter()
        .map(|entry_id| {
            state
                .auto_queue_service()
                .entry_json(entry_id, guild_id)
                .unwrap_or(serde_json::Value::Null)
        })
        .collect::<Vec<_>>();

    let run = state
        .auto_queue_service()
        .run_json(&run_id)
        .unwrap_or(serde_json::Value::Null);

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
    let deps = AutoQueueActivateDeps::from_state(&state);
    let body = if let Some(pool) = state.pg_pool.as_ref() {
        match activate_preflight_with_pg(pool, body).await {
            ActivatePgPreflight::Return(response) => return response,
            ActivatePgPreflight::Continue(body) => body,
        }
    } else {
        body
    };

    if deps.pg_pool.is_some() {
        activate_with_deps_pg(&deps, body).await
    } else {
        activate_with_deps(&deps, body)
    }
}

enum ActivatePgPreflight {
    Return((StatusCode, Json<serde_json::Value>)),
    Continue(ActivateBody),
}

async fn activate_preflight_with_pg(
    pool: &sqlx::PgPool,
    mut body: ActivateBody,
) -> ActivatePgPreflight {
    let active_only = body.active_only.unwrap_or(false);
    let selected_run = if let Some(run_id) = body
        .run_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        match sqlx::query(
            "SELECT id, status
             FROM auto_queue_runs
             WHERE id = $1",
        )
        .bind(run_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(row)) => {
                let id = match row.try_get::<String, _>("id") {
                    Ok(value) => value,
                    Err(error) => {
                        return ActivatePgPreflight::Return((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("decode postgres auto-queue run {run_id}: {error}")}),
                            ),
                        ));
                    }
                };
                let status = match row.try_get::<String, _>("status") {
                    Ok(value) => value,
                    Err(error) => {
                        return ActivatePgPreflight::Return((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("decode postgres auto-queue run status {run_id}: {error}")}),
                            ),
                        ));
                    }
                };
                Some((id, status))
            }
            Ok(None) => None,
            Err(error) => {
                return ActivatePgPreflight::Return((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres auto-queue run {run_id}: {error}")}),
                    ),
                ));
            }
        }
    } else {
        let repo = body
            .repo
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let agent_id = body
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let status_clause = if active_only {
            "status = 'active'"
        } else {
            "status IN ('active', 'generated', 'pending')"
        };
        let query = format!(
            "SELECT id, status
             FROM auto_queue_runs
             WHERE ($1::TEXT IS NULL OR repo = $1 OR repo IS NULL OR repo = '')
               AND ($2::TEXT IS NULL OR agent_id = $2 OR agent_id IS NULL OR agent_id = '')
               AND {status_clause}
             ORDER BY created_at DESC
             LIMIT 1"
        );
        match sqlx::query(&query)
            .bind(repo)
            .bind(agent_id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(row)) => {
                let id = match row.try_get::<String, _>("id") {
                    Ok(value) => value,
                    Err(error) => {
                        return ActivatePgPreflight::Return((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("decode postgres auto-queue selected run id: {error}")}),
                            ),
                        ));
                    }
                };
                let status = match row.try_get::<String, _>("status") {
                    Ok(value) => value,
                    Err(error) => {
                        return ActivatePgPreflight::Return((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("decode postgres auto-queue selected run status: {error}")}),
                            ),
                        ));
                    }
                };
                Some((id, status))
            }
            Ok(None) => None,
            Err(error) => {
                return ActivatePgPreflight::Return((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres auto-queue selected run: {error}")}),
                    ),
                ));
            }
        }
    };

    let Some((run_id, status)) = selected_run else {
        return ActivatePgPreflight::Return((
            StatusCode::OK,
            Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
        ));
    };

    let blocking_phase_gate = match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1
             FROM auto_queue_phase_gates
             WHERE run_id = $1
               AND status IN ('pending', 'failed')
         )",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return ActivatePgPreflight::Return((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue phase gates for {run_id}: {error}")}),
                ),
            ));
        }
    };

    match status.as_str() {
        "paused" => {
            let message = if blocking_phase_gate {
                "Run is waiting on phase gate"
            } else {
                "Run is paused"
            };
            return ActivatePgPreflight::Return((
                StatusCode::OK,
                Json(json!({ "dispatched": [], "count": 0, "message": message })),
            ));
        }
        RUN_STATUS_RESTORING => {
            return ActivatePgPreflight::Return((
                StatusCode::OK,
                Json(json!({ "dispatched": [], "count": 0, "message": "Run is restoring" })),
            ));
        }
        _ if active_only && status != "active" => {
            return ActivatePgPreflight::Return((
                StatusCode::OK,
                Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
            ));
        }
        _ if blocking_phase_gate => {
            return ActivatePgPreflight::Return((
                StatusCode::OK,
                Json(
                    json!({ "dispatched": [], "count": 0, "message": "Run is waiting on phase gate" }),
                ),
            ));
        }
        _ => {}
    }

    if body.run_id.is_none() {
        body.run_id = Some(run_id);
    }

    ActivatePgPreflight::Continue(body)
}

async fn activate_with_deps_pg(
    deps: &AutoQueueActivateDeps,
    body: ActivateBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let _ignored_unified_thread = body.unified_thread.is_some();
    let Some(pool) = deps.pg_pool.as_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "postgres pool is not configured"})),
        );
    };

    let active_only = body.active_only.unwrap_or(false);
    let run_id = if let Some(run_id) = body.run_id.clone() {
        run_id
    } else {
        let repo = body
            .repo
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let agent_id = body
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let status_clause = if active_only {
            "status = 'active'"
        } else {
            "status IN ('active', 'generated', 'pending')"
        };
        let query = format!(
            "SELECT id
             FROM auto_queue_runs
             WHERE ($1::TEXT IS NULL OR repo = $1 OR repo IS NULL OR repo = '')
               AND ($2::TEXT IS NULL OR agent_id = $2 OR agent_id IS NULL OR agent_id = '')
               AND {status_clause}
             ORDER BY created_at DESC
             LIMIT 1"
        );
        match sqlx::query_scalar::<_, String>(&query)
            .bind(repo)
            .bind(agent_id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(run_id)) => run_id,
            Ok(None) => {
                return (
                    StatusCode::OK,
                    Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("load postgres auto-queue run: {error}")})),
                );
            }
        }
    };
    let run_log_ctx = AutoQueueLogContext::new().run(&run_id);

    if !active_only
        && let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'active'
             WHERE id = $1
               AND status IN ('generated', 'pending')",
        )
        .bind(&run_id)
        .execute(pool)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("promote postgres auto-queue run {run_id}: {error}")})),
        );
    }

    if let Err(error) = crate::db::auto_queue::clear_inactive_slot_assignments_pg(pool).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"error": format!("clear inactive postgres auto-queue slots for {run_id}: {error}")}),
            ),
        );
    }
    let mut cleared_slots: HashSet<(String, i64)> = HashSet::new();

    let entry_count = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(count) => count,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres auto-queue entries for {run_id}: {error}")}),
                ),
            );
        }
    };
    if entry_count == 0 {
        if let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'completed',
                 completed_at = NOW()
             WHERE id = $1",
        )
        .bind(&run_id)
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("complete stale postgres auto-queue run {run_id}: {error}")}),
                ),
            );
        }
        crate::auto_queue_log!(
            info,
            "activate_stale_empty_run_completed_pg",
            run_log_ctx.clone(),
            "[auto-queue] Completed stale empty PG run {run_id} — no entries, skipping fallback populate (#85)"
        );
        return (
            StatusCode::OK,
            Json(
                json!({ "dispatched": [], "count": 0, "message": "Stale empty run completed — no entries to dispatch" }),
            ),
        );
    }

    let (max_concurrent, _thread_group_count) = match sqlx::query(
        "SELECT COALESCE(max_concurrent_threads, 1)::BIGINT AS max_concurrent_threads,
                COALESCE(thread_group_count, 1)::BIGINT AS thread_group_count
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(row) => {
            let max_concurrent = match row.try_get::<i64, _>("max_concurrent_threads") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres auto-queue max_concurrent_threads for {run_id}: {error}")}),
                        ),
                    );
                }
            };
            let thread_group_count = match row.try_get::<i64, _>("thread_group_count") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres auto-queue thread_group_count for {run_id}: {error}")}),
                        ),
                    );
                }
            };
            (max_concurrent, thread_group_count)
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue run capacity for {run_id}: {error}")}),
                ),
            );
        }
    };

    let run_agents_rows = match sqlx::query(
        "SELECT DISTINCT agent_id
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue run agents for {run_id}: {error}")}),
                ),
            );
        }
    };
    for row in run_agents_rows {
        let agent_id: String = match row.try_get("agent_id") {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("decode postgres auto-queue run agent for {run_id}: {error}")}),
                    ),
                );
            }
        };
        if let Err(error) =
            crate::db::auto_queue::ensure_agent_slot_pool_rows_pg(pool, &agent_id, max_concurrent)
                .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("prepare postgres slot pool rows for run {run_id} agent {agent_id}: {error}")}),
                ),
            );
        }
    }

    let current_phase = match crate::db::auto_queue::current_batch_phase_pg(pool, &run_id).await {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue current phase for {run_id}: {error}")}),
                ),
            );
        }
    };

    let active_groups_rows = match sqlx::query(
        "SELECT DISTINCT COALESCE(thread_group, 0)::BIGINT AS thread_group
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'dispatched'
         ORDER BY thread_group ASC",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres active groups for {run_id}: {error}")}),
                ),
            );
        }
    };
    let active_groups: Vec<i64> = {
        let mut groups = Vec::with_capacity(active_groups_rows.len());
        for row in active_groups_rows {
            match row.try_get::<i64, _>("thread_group") {
                Ok(value) => groups.push(value),
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres active group for {run_id}: {error}")}),
                        ),
                    );
                }
            }
        }
        groups
    };
    let active_group_count = active_groups.len() as i64;

    let pending_group_rows = match sqlx::query(
        "SELECT DISTINCT COALESCE(thread_group, 0)::BIGINT AS thread_group,
                         COALESCE(batch_phase, 0)::BIGINT AS batch_phase
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'pending'
         ORDER BY thread_group ASC, batch_phase ASC",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres pending groups for {run_id}: {error}")}),
                ),
            );
        }
    };
    let pending_groups: Vec<i64> = {
        let active_set: HashSet<i64> = active_groups.iter().copied().collect();
        let mut groups = Vec::new();
        let mut seen = HashSet::new();
        for row in pending_group_rows {
            let thread_group = match row.try_get::<i64, _>("thread_group") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres pending group for {run_id}: {error}")}),
                        ),
                    );
                }
            };
            let batch_phase = match row.try_get::<i64, _>("batch_phase") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode postgres pending batch_phase for {run_id}: {error}")}),
                        ),
                    );
                }
            };
            if !active_set.contains(&thread_group)
                && crate::db::auto_queue::batch_phase_is_eligible(batch_phase, current_phase)
                && seen.insert(thread_group)
            {
                groups.push(thread_group);
            }
        }
        groups
    };

    let mut dispatched = Vec::new();
    let mut groups_to_dispatch = Vec::new();

    if let Some(group) = body.thread_group {
        let has_pending = match crate::db::auto_queue::group_has_pending_entries_pg(
            pool,
            &run_id,
            group,
            current_phase,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres pending group eligibility for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        let has_dispatched = match group_has_dispatched_entries_pg(pool, &run_id, group).await {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres dispatched group state for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        if has_pending && !has_dispatched {
            groups_to_dispatch.push(group);
        }
    }

    match crate::db::auto_queue::assigned_groups_with_pending_entries_pg(
        pool,
        &run_id,
        current_phase,
    )
    .await
    {
        Ok(groups) => {
            for group in groups {
                if !groups_to_dispatch.contains(&group) {
                    groups_to_dispatch.push(group);
                }
            }
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres assigned groups for {run_id}: {error}")}),
                ),
            );
        }
    }

    for &group in &active_groups {
        let has_pending = match crate::db::auto_queue::group_has_pending_entries_pg(
            pool,
            &run_id,
            group,
            current_phase,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres continuation eligibility for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        let has_dispatched = match group_has_dispatched_entries_pg(pool, &run_id, group).await {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres dispatched continuation state for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        if has_pending && !has_dispatched && !groups_to_dispatch.contains(&group) {
            groups_to_dispatch.push(group);
        }
    }

    for group in pending_groups {
        if !groups_to_dispatch.contains(&group) {
            groups_to_dispatch.push(group);
        }
    }

    let mut dispatched_groups_this_activate = 0_i64;
    for group in &groups_to_dispatch {
        if (active_group_count + dispatched_groups_this_activate) >= max_concurrent {
            break;
        }

        let entry = match crate::db::auto_queue::first_pending_entry_for_group_pg(
            pool,
            &run_id,
            *group,
            current_phase,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres pending entry for {run_id}:{group}: {error}")}),
                    ),
                );
            }
        };
        let Some((entry_id, card_id, agent_id, batch_phase)) = entry else {
            continue;
        };
        let entry_log_ctx = AutoQueueLogContext::new()
            .run(&run_id)
            .entry(&entry_id)
            .card(&card_id)
            .agent(&agent_id)
            .thread_group(*group)
            .batch_phase(batch_phase);

        let initial_state = match load_activate_card_state_pg(pool, &card_id, &entry_id).await {
            Ok(state) => state,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_load_card_failed_pg",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to load PG card {} before activate for entry {}: {error}",
                    card_id,
                    entry_id
                );
                continue;
            }
        };

        let busy = match sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                 SELECT 1
                 FROM kanban_cards
                 WHERE assigned_agent_id = $1
                   AND status IN ('requested', 'in_progress', 'review')
                   AND id != $2
                   AND id NOT IN (
                       SELECT kanban_card_id
                       FROM auto_queue_entries
                       WHERE run_id = $3
                   )
             )",
        )
        .bind(&agent_id)
        .bind(&card_id)
        .bind(&run_id)
        .fetch_one(pool)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres busy-agent guard for run {run_id} agent {agent_id}: {error}")}),
                    ),
                );
            }
        };
        if busy {
            crate::auto_queue_log!(
                info,
                "activate_busy_agent_guard_blocked_pg",
                entry_log_ctx.clone(),
                "[auto-queue] Skipping activate for {agent_id}: agent has active cards outside auto-queue"
            );
            continue;
        }

        let effective = match resolve_activate_pipeline_pg(
            pool,
            initial_state.repo_id.as_deref(),
            initial_state.assigned_agent_id.as_deref(),
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_pipeline_resolve_failed_pg",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to resolve PG pipeline for card {} during activate: {}",
                    card_id,
                    error
                );
                continue;
            }
        };

        if initial_state.entry_status != "pending" {
            if initial_state.entry_status == "dispatched" {
                dispatched_groups_this_activate += 1;
            }
            continue;
        }

        if effective.is_terminal(&initial_state.status) || initial_state.status == "done" {
            if let Err(error) = crate::db::auto_queue::update_entry_status_on_pg(
                pool,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
                "activate_done_skip_pg",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            )
            .await
            {
                crate::auto_queue_log!(
                    warn,
                    "activate_done_skip_failed_pg",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to skip terminal PG card entry {} during activate: {}",
                    entry_id,
                    error
                );
            }
            continue;
        }

        if initial_state.has_active_dispatch() {
            let dispatch_id = initial_state
                .latest_dispatch_id
                .as_ref()
                .expect("active dispatch state requires dispatch id")
                .clone();
            if let Err(error) = crate::db::auto_queue::update_entry_status_on_pg(
                pool,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                "activate_attach_existing_dispatch_pg",
                &crate::db::auto_queue::EntryStatusUpdateOptions {
                    dispatch_id: Some(dispatch_id.clone()),
                    slot_index: None,
                },
            )
            .await
            {
                crate::auto_queue_log!(
                    warn,
                    "activate_attach_existing_dispatch_failed_pg",
                    entry_log_ctx.clone().dispatch(&dispatch_id),
                    "[auto-queue] failed to attach existing PG dispatch {dispatch_id} to entry {entry_id}: {error}"
                );
            }
            dispatched_groups_this_activate += 1;
            continue;
        }

        let still_pending = match sqlx::query_scalar::<_, bool>(
            "SELECT status = 'pending'
             FROM auto_queue_entries
             WHERE id = $1",
        )
        .bind(&entry_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(value)) => value,
            Ok(None) => false,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("recheck postgres auto-queue entry status for {entry_id}: {error}")}),
                    ),
                );
            }
        };
        if !still_pending {
            crate::auto_queue_log!(
                warn,
                "activate_concurrent_race_detected_pg",
                entry_log_ctx.clone(),
                "[auto-queue] entry {entry_id} is no longer pending before slot allocation; concurrent activate likely claimed it"
            );
            dispatched_groups_this_activate += 1;
            continue;
        }

        let slot_allocation = match crate::db::auto_queue::allocate_slot_for_group_agent_pg(
            pool, &run_id, *group, &agent_id,
        )
        .await
        {
            Ok(allocation) => allocation,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_slot_allocation_failed_pg",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to allocate PG slot for entry {} run {} agent {} group {}: {}",
                    entry_id,
                    run_id,
                    agent_id,
                    group,
                    error
                );
                continue;
            }
        };
        let slot_index = slot_allocation
            .as_ref()
            .map(|allocation| allocation.slot_index);
        let mut reset_slot_thread_before_reuse = false;
        let Some(allocation) = slot_allocation else {
            crate::auto_queue_log!(
                warn,
                "activate_slot_pool_exhausted_pg",
                entry_log_ctx.clone(),
                "[auto-queue] Skipping group {group} for {agent_id}: no free PG slot in pool (possible concurrent slot claim)"
            );
            continue;
        };

        reset_slot_thread_before_reuse = match slot_requires_thread_reset_before_reuse_pg(
            pool,
            &agent_id,
            allocation.slot_index,
            allocation.newly_assigned,
            allocation.reassigned_from_other_group,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_slot_reset_probe_failed_pg",
                    entry_log_ctx.clone().slot_index(allocation.slot_index),
                    "[auto-queue] failed to inspect PG slot reuse state for {} slot {}: {}",
                    agent_id,
                    allocation.slot_index,
                    error
                );
                false
            }
        };
        let clear_slot_session_before_dispatch =
            reset_slot_thread_before_reuse || !allocation.newly_assigned;
        let slot_key = (agent_id.clone(), allocation.slot_index);
        if clear_slot_session_before_dispatch && !cleared_slots.contains(&slot_key) {
            match crate::services::auto_queue::runtime::clear_slot_threads_for_slot_pg(
                deps.health_registry.clone(),
                pool,
                &agent_id,
                allocation.slot_index,
            )
            .await
            {
                Ok(cleared) => {
                    if cleared > 0 {
                        crate::auto_queue_log!(
                            info,
                            "activate_slot_cleared_before_dispatch_pg",
                            entry_log_ctx.clone().slot_index(allocation.slot_index),
                            "[auto-queue] cleared {cleared} PG slot thread session(s) before dispatching {agent_id} slot {} group {group}",
                            allocation.slot_index
                        );
                    }
                }
                Err(error) => crate::auto_queue_log!(
                    warn,
                    "activate_slot_clear_failed_pg",
                    entry_log_ctx.clone().slot_index(allocation.slot_index),
                    "[auto-queue] failed to clear PG slot thread session(s) for {} slot {}: {}",
                    agent_id,
                    allocation.slot_index,
                    error
                ),
            }
            cleared_slots.insert(slot_key);
        }

        match crate::db::auto_queue::update_entry_status_on_pg(
            pool,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
            "activate_dispatch_reserve_pg",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: None,
                slot_index,
            },
        )
        .await
        {
            Ok(result) if !result.changed => {
                crate::auto_queue_log!(
                    info,
                    "activate_dispatch_reserve_already_claimed_pg",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] entry {entry_id} was already reserved by another activate worker; skipping duplicate PG dispatch creation"
                );
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_dispatch_reserve_failed_pg",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] failed to reserve PG entry {} before create_dispatch: {}",
                    entry_id,
                    error
                );
                continue;
            }
        }

        let dispatch_context = build_auto_queue_dispatch_context(
            &entry_id,
            *group,
            slot_index,
            reset_slot_thread_before_reuse,
            std::iter::empty(),
        );
        let dispatch_id = match create_activate_dispatch_pg(
            deps,
            pool,
            &card_id,
            &agent_id,
            "implementation",
            &initial_state.title,
            &dispatch_context,
        )
        .await
        {
            Ok(dispatch_id) => dispatch_id,
            Err(error) => {
                let recovered_state = load_activate_card_state_pg(pool, &card_id, &entry_id)
                    .await
                    .ok();
                if let Some(dispatch_id) = recovered_state
                    .as_ref()
                    .filter(|state| state.has_active_dispatch())
                    .and_then(|state| state.latest_dispatch_id.clone())
                {
                    if let Err(update_error) = crate::db::auto_queue::update_entry_status_on_pg(
                        pool,
                        &entry_id,
                        crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                        "activate_dispatch_error_recover_pg",
                        &crate::db::auto_queue::EntryStatusUpdateOptions {
                            dispatch_id: Some(dispatch_id),
                            slot_index,
                        },
                    )
                    .await
                    {
                        crate::auto_queue_log!(
                            warn,
                            "activate_create_dispatch_recover_failed_pg",
                            entry_log_ctx.clone().maybe_slot_index(slot_index),
                            "[auto-queue] failed to recover PG entry {entry_id} after create_dispatch error: {update_error}"
                        );
                    } else {
                        continue;
                    }
                }

                if recovered_state.as_ref().is_some_and(|state| {
                    state.latest_dispatch_id.is_some() || state.status != initial_state.status
                }) {
                    crate::auto_queue_log!(
                        warn,
                        "activate_create_dispatch_error_kept_reservation_pg",
                        entry_log_ctx
                            .clone()
                            .maybe_slot_index(slot_index)
                            .maybe_dispatch(
                                recovered_state
                                    .as_ref()
                                    .and_then(|state| state.latest_dispatch_id.as_deref())
                            ),
                        "[auto-queue] create_dispatch PG errored for entry {entry_id} after card progressed; keeping reservation"
                    );
                    continue;
                }

                if let Err(update_error) = crate::db::auto_queue::update_entry_status_on_pg(
                    pool,
                    &entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_PENDING,
                    "activate_dispatch_reserve_revert_pg",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                )
                .await
                {
                    crate::auto_queue_log!(
                        warn,
                        "activate_dispatch_reserve_revert_failed_pg",
                        entry_log_ctx.clone().maybe_slot_index(slot_index),
                        "[auto-queue] failed to revert PG reservation for entry {} after create_dispatch error: {}",
                        entry_id,
                        update_error
                    );
                } else if let Some(assigned_slot) = slot_index
                    && let Err(release_error) =
                        crate::db::auto_queue::release_slot_for_group_agent_pg(
                            pool,
                            &run_id,
                            *group,
                            &agent_id,
                            assigned_slot,
                        )
                        .await
                {
                    crate::auto_queue_log!(
                        warn,
                        "activate_dispatch_revert_slot_release_failed_pg",
                        entry_log_ctx.clone().slot_index(assigned_slot),
                        "[auto-queue] failed to release PG slot {} for entry {} after create_dispatch error: {}",
                        assigned_slot,
                        entry_id,
                        release_error
                    );
                }
                crate::auto_queue_log!(
                    error,
                    "activate_dispatch_create_failed_pg",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] create_dispatch PG failed for entry {entry_id} (group {group}), leaving as pending for retry: {error}"
                );
                continue;
            }
        };

        if let Err(error) = crate::db::auto_queue::update_entry_status_on_pg(
            pool,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
            "activate_dispatch_created_pg",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: Some(dispatch_id.clone()),
                slot_index,
            },
        )
        .await
        {
            crate::auto_queue_log!(
                warn,
                "activate_dispatch_mark_failed_pg",
                entry_log_ctx
                    .clone()
                    .dispatch(&dispatch_id)
                    .maybe_slot_index(slot_index),
                "[auto-queue] failed to mark PG entry {} dispatched after create_dispatch: {}",
                entry_id,
                error
            );
        }

        dispatched_groups_this_activate += 1;
        dispatched.push(deps.entry_json_pg(pool, &entry_id).await);
    }

    let remaining = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres remaining entries for {run_id}: {error}")}),
                ),
            );
        }
    };
    if remaining == 0 {
        if let Err(error) = crate::db::auto_queue::release_run_slots_pg(pool, &run_id).await {
            crate::auto_queue_log!(
                warn,
                "activate_release_run_slots_failed_pg",
                run_log_ctx.clone(),
                "[auto-queue] failed to release PG slots for drained run {}: {}",
                run_id,
                error
            );
        }
        let still_dispatched = match sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_entries
             WHERE run_id = $1
               AND status = 'dispatched'",
        )
        .bind(&run_id)
        .fetch_one(pool)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("count postgres dispatched entries for {run_id}: {error}")}),
                    ),
                );
            }
        };
        if still_dispatched == 0
            && let Err(error) = sqlx::query(
                "UPDATE auto_queue_runs
                 SET status = 'completed',
                     completed_at = NOW()
                 WHERE id = $1
                   AND status IN ('active', 'paused', 'generated', 'pending')",
            )
            .bind(&run_id)
            .execute(pool)
            .await
        {
            crate::auto_queue_log!(
                warn,
                "activate_finalize_run_failed_pg",
                run_log_ctx.clone(),
                "[auto-queue] failed to finalize PG run {} after dispatch drain: {}",
                run_id,
                error
            );
        }
    }

    let active_group_count = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT COALESCE(thread_group, 0))::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'dispatched'",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres active groups for {run_id}: {error}")}),
                ),
            );
        }
    };
    let pending_group_count = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT COALESCE(thread_group, 0))::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status = 'pending'",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("count postgres pending groups for {run_id}: {error}")}),
                ),
            );
        }
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

pub(crate) fn activate_with_deps(
    deps: &AutoQueueActivateDeps,
    body: ActivateBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let _ignored_unified_thread = body.unified_thread.is_some();
    let conn = match deps.db.separate_conn() {
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
    let mut params: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();
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
        let run_status: Option<String> = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = ?1",
                [&run_id],
                |row| row.get(0),
            )
            .ok();
        match run_status.as_deref() {
            Some("paused") => {
                let message = if crate::db::auto_queue::run_has_blocking_phase_gate(&conn, &run_id)
                {
                    "Run is waiting on phase gate"
                } else {
                    "Run is paused"
                };
                return (
                    StatusCode::OK,
                    Json(json!({ "dispatched": [], "count": 0, "message": message })),
                );
            }
            Some(RUN_STATUS_RESTORING) => {
                return (
                    StatusCode::OK,
                    Json(json!({ "dispatched": [], "count": 0, "message": "Run is restoring" })),
                );
            }
            Some(status) if active_only && status != "active" => {
                return (
                    StatusCode::OK,
                    Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
                );
            }
            Some(_) => {}
            None => {
                return (
                    StatusCode::OK,
                    Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
                );
            }
        }
        if crate::db::auto_queue::run_has_blocking_phase_gate(&conn, &run_id) {
            return (
                StatusCode::OK,
                Json(
                    json!({ "dispatched": [], "count": 0, "message": "Run is waiting on phase gate" }),
                ),
            );
        }
        Some(run_id)
    } else {
        let param_refs: Vec<&dyn libsql_rusqlite::types::ToSql> =
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
    let run_log_ctx = AutoQueueLogContext::new().run(&run_id);

    if crate::db::auto_queue::run_has_blocking_phase_gate(&conn, &run_id) {
        return (
            StatusCode::OK,
            Json(
                json!({ "dispatched": [], "count": 0, "message": "Run is waiting on phase gate" }),
            ),
        );
    }

    if !active_only {
        // Promote pending/generated → active on explicit activation.
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'active' WHERE id = ?1 AND status IN ('generated', 'pending')",
            [&run_id],
        )
        .ok();
    }

    crate::db::auto_queue::clear_inactive_slot_assignments(&conn);
    let mut cleared_slots: HashSet<(String, i64)> = HashSet::new();

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
        crate::auto_queue_log!(
            info,
            "activate_stale_empty_run_completed",
            run_log_ctx.clone(),
            "[auto-queue] Completed stale empty run {run_id} — no entries, skipping fallback populate (#85)"
        );
        return (
            StatusCode::OK,
            Json(
                json!({ "dispatched": [], "count": 0, "message": "Stale empty run completed — no entries to dispatch" }),
            ),
        );
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
    {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT agent_id
                 FROM auto_queue_entries
                 WHERE run_id = ?1",
            )
            .unwrap();
        let run_agents: Vec<String> = stmt
            .query_map([&run_id], |row| row.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|row| row.ok()).collect())
            .unwrap_or_default();
        drop(stmt);
        for run_agent_id in run_agents {
            crate::db::auto_queue::ensure_agent_slot_pool_rows(
                &conn,
                &run_agent_id,
                max_concurrent,
            )
            .ok();
        }
    }
    let current_phase = crate::db::auto_queue::current_batch_phase(&conn, &run_id);

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
                        && crate::db::auto_queue::batch_phase_is_eligible(
                            batch_phase,
                            current_phase,
                        )
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
        let conn = deps.db.separate_conn().unwrap();
        let has_pending =
            crate::db::auto_queue::group_has_pending_entries(&conn, &run_id, group, current_phase);
        let has_dispatched: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0
                 FROM auto_queue_entries
                 WHERE run_id = ?1
                   AND COALESCE(thread_group, 0) = ?2
                   AND status = 'dispatched'",
                libsql_rusqlite::params![run_id, group],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if has_pending && !has_dispatched {
            groups_to_dispatch.push(group);
        }
    }

    {
        let conn = deps.db.separate_conn().unwrap();
        for group in crate::db::auto_queue::assigned_groups_with_pending_entries(
            &conn,
            &run_id,
            current_phase,
        ) {
            if !groups_to_dispatch.contains(&group) {
                groups_to_dispatch.push(group);
            }
        }
    }

    // Also dispatch next entry for active groups that have pending entries
    // (continuation within same group after prior entry completed)
    {
        let conn = deps.db.separate_conn().unwrap();
        for &grp in &active_groups {
            let has_pending = crate::db::auto_queue::group_has_pending_entries(
                &conn,
                &run_id,
                grp,
                current_phase,
            );
            let has_dispatched: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM auto_queue_entries \
                     WHERE run_id = ?1 AND COALESCE(thread_group, 0) = ?2 AND status = 'dispatched'",
                    libsql_rusqlite::params![run_id, grp],
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
        if !groups_to_dispatch.contains(&grp) {
            groups_to_dispatch.push(grp);
        }
    }

    let mut dispatched_groups_this_activate = 0_i64;
    for group in &groups_to_dispatch {
        if (active_group_count + dispatched_groups_this_activate) >= max_concurrent {
            break;
        }

        // Get first pending entry in this group
        let conn = deps.db.separate_conn().unwrap();
        let entry = crate::db::auto_queue::first_pending_entry_for_group(
            &conn,
            &run_id,
            *group,
            current_phase,
        );
        drop(conn);

        let Some((entry_id, card_id, agent_id, batch_phase)) = entry else {
            continue;
        };
        let entry_log_ctx = AutoQueueLogContext::new()
            .run(&run_id)
            .entry(&entry_id)
            .card(&card_id)
            .agent(&agent_id)
            .thread_group(*group)
            .batch_phase(batch_phase);

        let initial_state = {
            let conn = deps.db.separate_conn().unwrap();
            let card_state = load_activate_card_state(&conn, &card_id, &entry_id);
            drop(conn);
            match card_state {
                Ok(card_state) => card_state,
                Err(error) => {
                    crate::auto_queue_log!(
                        warn,
                        "activate_load_card_failed",
                        entry_log_ctx.clone(),
                        "[auto-queue] failed to load card {} before activate for entry {}: {error}",
                        card_id,
                        entry_id
                    );
                    continue;
                }
            }
        };

        // Busy-agent guard (#110): skip if agent has active cards outside auto-queue.
        // Exclude the card being dispatched (#162) and cards that belong to the
        // same auto-queue run — those work in isolated worktrees so parallel
        // execution is safe.
        let conn = deps.db.separate_conn().unwrap();
        let busy: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM kanban_cards \
                 WHERE assigned_agent_id = ?1 AND status IN ('requested', 'in_progress', 'review') \
                 AND id != ?2 \
                 AND id NOT IN (SELECT kanban_card_id FROM auto_queue_entries WHERE run_id = ?3)",
                libsql_rusqlite::params![agent_id, card_id, run_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        drop(conn);

        if busy {
            crate::auto_queue_log!(
                info,
                "activate_busy_agent_guard_blocked",
                entry_log_ctx.clone(),
                "[auto-queue] Skipping activate for {agent_id}: agent has active cards outside auto-queue"
            );
            continue;
        }

        // #162/#500: If card is in a non-dispatchable state (e.g. backlog),
        // walk it through free transitions using the same canonical transition
        // path as manual status changes so requested-state hooks/preflight fire.
        let walk_path = {
            let conn = deps.db.separate_conn().unwrap();
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
            effective.free_path_to_dispatchable(&initial_state.status)
        }
        .filter(|path| {
            // `create_dispatch()` already handles the canonical ready -> in_progress
            // kickoff path. Replaying the single-hop ready -> requested free edge here
            // would rerun requested-state preflight and change longstanding activate()
            // semantics for already-ready cards.
            !(initial_state.status == "ready"
                && path.len() == 1
                && path.first().is_some_and(|step| step == "requested"))
        });

        if walk_path.is_none() {
            match handle_activate_preflight_metadata(
                deps,
                &run_id,
                &entry_id,
                &card_id,
                &agent_id,
                *group,
                batch_phase,
                &initial_state.title,
                initial_state.metadata.as_deref(),
            ) {
                ActivatePreflightOutcome::Continue => {}
                ActivatePreflightOutcome::Dispatched(entry_json) => {
                    dispatched_groups_this_activate += 1;
                    dispatched.push(entry_json);
                    continue;
                }
                ActivatePreflightOutcome::Skipped => continue,
                ActivatePreflightOutcome::Deferred => continue,
            }
        }

        // Get card title
        let conn = deps.db.separate_conn().unwrap();
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
            let conn = deps.db.separate_conn().unwrap();
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
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&metadata) {
                    match parsed.get("preflight_status").and_then(|v| v.as_str()) {
                        Some("consult_required") => {
                            let conn = deps.db.separate_conn().unwrap();
                            if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
                                &conn,
                                &entry_id,
                                crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                                "activate_consultation_reserve",
                                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                            ) {
                                crate::auto_queue_log!(
                                    warn,
                                    "activate_consultation_reserve_failed",
                                    entry_log_ctx.clone(),
                                    "[auto-queue] failed to reserve consultation entry {} before dispatch creation: {}",
                                    entry_id,
                                    error
                                );
                                drop(conn);
                                continue;
                            }
                            drop(conn);

                            let consult_agent_id = {
                                let conn = deps.db.separate_conn().unwrap();
                                let provider = conn
                                    .query_row(
                                        "SELECT COALESCE(provider, 'claude') FROM agents WHERE id = ?1",
                                        [&agent_id],
                                        |row| row.get::<_, String>(0),
                                    )
                                    .map(|raw| ProviderKind::from_str_or_unsupported(&raw))
                                    .unwrap_or_else(|_| {
                                        ProviderKind::default_channel_provider()
                                            .unwrap_or(ProviderKind::Claude)
                                    });
                                let mut stmt = conn
                                    .prepare(
                                        "SELECT id, COALESCE(provider, 'claude')
                                         FROM agents
                                         WHERE id != ?1
                                         ORDER BY id ASC",
                                    )
                                    .unwrap();
                                let available_agents: Vec<(String, ProviderKind)> = stmt
                                    .query_map([&agent_id], |row| {
                                        let provider_raw: String = row.get(1)?;
                                        Ok((
                                            row.get::<_, String>(0)?,
                                            ProviderKind::from_str_or_unsupported(&provider_raw),
                                        ))
                                    })
                                    .ok()
                                    .map(|rows| rows.filter_map(|row| row.ok()).collect())
                                    .unwrap_or_default();
                                provider
                                    .select_counterpart_from(
                                        available_agents.iter().map(|(_, candidate_provider)| {
                                            candidate_provider.clone()
                                        }),
                                    )
                                    .and_then(|counterpart| {
                                        available_agents.iter().find_map(
                                            |(candidate_id, candidate_provider)| {
                                                (*candidate_provider == counterpart)
                                                    .then_some(candidate_id.clone())
                                            },
                                        )
                                    })
                                    .unwrap_or_else(|| agent_id.clone())
                            };

                            let dispatch_result = run_activate_blocking(|| {
                                let dispatch_context = build_auto_queue_dispatch_context(
                                    &entry_id,
                                    *group,
                                    None,
                                    false,
                                    [
                                        ("run_id", json!(run_id)),
                                        ("batch_phase", json!(batch_phase)),
                                    ],
                                );
                                crate::dispatch::create_dispatch(
                                    &deps.db,
                                    &deps.engine,
                                    &card_id,
                                    &consult_agent_id,
                                    "consultation",
                                    &format!("[Consultation] {title}"),
                                    &dispatch_context,
                                )
                            });
                            let dispatch = match dispatch_result {
                                Ok(dispatch) => dispatch,
                                Err(error) => {
                                    match record_entry_dispatch_failure(
                                        deps,
                                        &run_id,
                                        &entry_id,
                                        &card_id,
                                        &agent_id,
                                        *group,
                                        None,
                                        "activate_consultation_dispatch_failed",
                                        &error.to_string(),
                                        &entry_log_ctx,
                                    ) {
                                        Ok(result) => crate::auto_queue_log!(
                                            warn,
                                            "activate_consultation_dispatch_failed",
                                            entry_log_ctx.clone(),
                                            "[auto-queue] consultation dispatch failed for entry {entry_id} (group {group}); retry {}/{} -> {}",
                                            result.retry_count,
                                            result.retry_limit,
                                            result.to_status
                                        ),
                                        Err(record_error) => crate::auto_queue_log!(
                                            warn,
                                            "activate_consultation_dispatch_failed",
                                            entry_log_ctx.clone(),
                                            "[auto-queue] consultation dispatch failed for entry {entry_id} (group {group}); failed to persist retry state: {record_error}"
                                        ),
                                    }
                                    continue;
                                }
                            };

                            let dispatch_id = dispatch["id"].as_str().unwrap_or("").to_string();
                            let mut conn = deps.db.separate_conn().unwrap();
                            if let Err(error) =
                                crate::db::auto_queue::record_consultation_dispatch_on_conn(
                                    &mut conn,
                                    &entry_id,
                                    &card_id,
                                    &dispatch_id,
                                    "activate_consultation_dispatch",
                                    &metadata,
                                )
                            {
                                crate::auto_queue_log!(
                                    warn,
                                    "activate_consultation_record_failed",
                                    entry_log_ctx.clone().dispatch(&dispatch_id),
                                    "[auto-queue] failed to persist consultation dispatch state for entry {}: {}",
                                    entry_id,
                                    error
                                );
                            }
                            dispatched_groups_this_activate += 1;
                            dispatched.push(deps.entry_json(&entry_id));
                            crate::auto_queue_log!(
                                info,
                                "activate_consultation_dispatched",
                                entry_log_ctx.clone().dispatch(&dispatch_id),
                                "[auto-queue] consultation dispatch ready for entry {entry_id}"
                            );
                            drop(conn);
                            continue;
                        }
                        Some("invalid") | Some("already_applied") => {
                            let conn = deps.db.separate_conn().unwrap();
                            if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
                                &conn,
                                &entry_id,
                                crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
                                "activate_preflight_invalid",
                                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                            ) {
                                crate::auto_queue_log!(
                                    warn,
                                    "activate_preflight_invalid_skip_failed",
                                    entry_log_ctx.clone(),
                                    "[auto-queue] failed to skip preflight-invalid entry {}: {}",
                                    entry_id,
                                    error
                                );
                            }
                            drop(conn);
                            crate::auto_queue_log!(
                                info,
                                "activate_preflight_invalid_skipped",
                                entry_log_ctx.clone(),
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

        // #500: Silent walk with hooks enabled
        if let Some(path) = walk_path {
            crate::auto_queue_log!(
                info,
                "activate_silent_walk_start",
                entry_log_ctx.clone(),
                "[auto-queue] Silent walk: card {} from '{}' through {:?} (canonical reducer, hooks enabled)",
                card_id,
                initial_state.status,
                path
            );
            let mut walk_failed = false;
            for step in &path {
                if let Err(e) = crate::kanban::transition_status_with_opts(
                    &deps.db,
                    &deps.engine,
                    &card_id,
                    step,
                    "auto-queue-walk",
                    false,
                ) {
                    crate::auto_queue_log!(
                        warn,
                        "activate_silent_walk_failed",
                        entry_log_ctx.clone(),
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

        let post_walk = {
            let conn = deps.db.separate_conn().unwrap();
            let state_after_walk = load_activate_card_state(&conn, &card_id, &entry_id);
            drop(conn);
            match state_after_walk {
                Ok(card_state) => card_state,
                Err(error) => {
                    crate::auto_queue_log!(
                        warn,
                        "activate_reload_card_failed",
                        entry_log_ctx.clone(),
                        "[auto-queue] failed to reload card {} after walk for entry {}: {error}",
                        card_id,
                        entry_id
                    );
                    continue;
                }
            }
        };

        if post_walk.entry_status != "pending" {
            if post_walk.entry_status == "dispatched" {
                // Another activate worker already reserved this group while this
                // call was walking the card. Treat the slot as occupied for
                // scheduling, but do not count it as a dispatch created by this
                // request.
                dispatched_groups_this_activate += 1;
            }
            continue;
        }

        if post_walk.status == "done" {
            let conn = deps.db.separate_conn().unwrap();
            if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
                &conn,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
                "activate_done_skip",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            ) {
                crate::auto_queue_log!(
                    warn,
                    "activate_done_skip_failed",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to skip done card entry {entry_id} during activate: {error}"
                );
            }
            drop(conn);
            continue;
        }

        if post_walk.has_active_dispatch() {
            let dispatch_id = post_walk
                .latest_dispatch_id
                .as_ref()
                .expect("active dispatch state requires dispatch id");
            let conn = deps.db.separate_conn().unwrap();
            match crate::db::auto_queue::update_entry_status_on_conn(
                &conn,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                "activate_attach_existing_dispatch",
                &crate::db::auto_queue::EntryStatusUpdateOptions {
                    dispatch_id: Some(dispatch_id.clone()),
                    slot_index: None,
                },
            ) {
                Ok(_) => {}
                Err(error) => crate::auto_queue_log!(
                    warn,
                    "activate_attach_existing_dispatch_failed",
                    entry_log_ctx.clone().dispatch(dispatch_id),
                    "[auto-queue] failed to attach existing dispatch {dispatch_id} to entry {entry_id}: {error}"
                ),
            }
            drop(conn);
            // Repair the entry linkage to the dispatch that already exists, but
            // do not report it as a new dispatch created by this activate call.
            dispatched_groups_this_activate += 1;
            continue;
        }

        match handle_activate_preflight_metadata(
            deps,
            &run_id,
            &entry_id,
            &card_id,
            &agent_id,
            *group,
            batch_phase,
            &post_walk.title,
            post_walk.metadata.as_deref(),
        ) {
            ActivatePreflightOutcome::Continue => {}
            ActivatePreflightOutcome::Dispatched(entry_json) => {
                dispatched_groups_this_activate += 1;
                dispatched.push(entry_json);
                continue;
            }
            ActivatePreflightOutcome::Skipped => continue,
            ActivatePreflightOutcome::Deferred => continue,
        }

        // #628: Re-verify entry is still pending before slot allocation to guard
        // against concurrent activate calls that may have already dispatched this entry.
        {
            let conn = deps.db.separate_conn().unwrap();
            let still_pending: bool = conn
                .query_row(
                    "SELECT status = 'pending' FROM auto_queue_entries WHERE id = ?1",
                    [&entry_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            drop(conn);
            if !still_pending {
                crate::auto_queue_log!(
                    warn,
                    "activate_concurrent_race_detected",
                    entry_log_ctx.clone(),
                    "[auto-queue] entry {entry_id} is no longer pending before slot allocation; concurrent activate likely claimed it"
                );
                dispatched_groups_this_activate += 1;
                continue;
            }
        }

        // Create dispatch
        let conn = deps.db.separate_conn().unwrap();
        let slot_allocation = match crate::db::auto_queue::allocate_slot_for_group_agent(
            &conn, &run_id, *group, &agent_id,
        ) {
            Ok(allocation) => allocation,
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_slot_allocation_failed",
                    entry_log_ctx.clone(),
                    "[auto-queue] failed to allocate slot for entry {} run {} agent {} group {}: {}",
                    entry_id,
                    run_id,
                    agent_id,
                    group,
                    error
                );
                drop(conn);
                continue;
            }
        };
        let slot_index = slot_allocation
            .as_ref()
            .map(|allocation| allocation.slot_index);
        let mut reset_slot_thread_before_reuse = false;
        if slot_allocation.is_none() {
            crate::auto_queue_log!(
                warn,
                "activate_slot_pool_exhausted",
                entry_log_ctx.clone(),
                "[auto-queue] Skipping group {group} for {agent_id}: no free slot in pool (possible concurrent slot claim)"
            );
            continue;
        }
        if let Some(allocation) = slot_allocation {
            reset_slot_thread_before_reuse = slot_requires_thread_reset_before_reuse(
                &conn,
                &agent_id,
                allocation.slot_index,
                allocation.newly_assigned,
                allocation.reassigned_from_other_group,
            );
            let assigned_slot = allocation.slot_index;
            let clear_slot_session_before_dispatch =
                reset_slot_thread_before_reuse || !allocation.newly_assigned;
            let slot_key = (agent_id.clone(), assigned_slot);
            if clear_slot_session_before_dispatch && !cleared_slots.contains(&slot_key) {
                let cleared = crate::services::auto_queue::runtime::clear_slot_threads_for_slot(
                    deps.health_registry.clone(),
                    &conn,
                    &agent_id,
                    assigned_slot,
                );
                if cleared > 0 {
                    crate::auto_queue_log!(
                        info,
                        "activate_slot_cleared_before_dispatch",
                        entry_log_ctx.clone().slot_index(assigned_slot),
                        "[auto-queue] cleared {cleared} slot thread session(s) before dispatching {agent_id} slot {assigned_slot} group {group}"
                    );
                }
                cleared_slots.insert(slot_key);
            }
        };

        let conn = deps.db.separate_conn().unwrap();
        let reserve_result = crate::db::auto_queue::update_entry_status_on_conn(
            &conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
            "activate_dispatch_reserve",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: None,
                slot_index,
            },
        );
        match reserve_result {
            Ok(result) => {
                if !result.changed {
                    crate::auto_queue_log!(
                        info,
                        "activate_dispatch_reserve_already_claimed",
                        entry_log_ctx.clone().maybe_slot_index(slot_index),
                        "[auto-queue] entry {entry_id} was already reserved by another activate worker; skipping duplicate dispatch creation"
                    );
                    drop(conn);
                    continue;
                }
            }
            Err(error) => {
                crate::auto_queue_log!(
                    warn,
                    "activate_dispatch_reserve_failed",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] failed to reserve entry {} before create_dispatch: {}",
                    entry_id,
                    error
                );
                drop(conn);
                continue;
            }
        }
        drop(conn);

        let dispatch_result = run_activate_blocking(|| {
            let dispatch_context = build_auto_queue_dispatch_context(
                &entry_id,
                *group,
                slot_index,
                reset_slot_thread_before_reuse,
                std::iter::empty(),
            );
            crate::dispatch::create_dispatch(
                &deps.db,
                &deps.engine,
                &card_id,
                &agent_id,
                "implementation",
                &post_walk.title,
                &dispatch_context,
            )
        });

        let dispatch = match dispatch_result {
            Ok(dispatch) => dispatch,
            Err(error) => {
                let error_text = error.to_string();
                let recovered_state = {
                    let conn = deps.db.separate_conn().unwrap();
                    let recovered = load_activate_card_state(&conn, &card_id, &entry_id).ok();
                    drop(conn);
                    recovered
                };

                if let Some(dispatch_id) = recovered_state
                    .as_ref()
                    .filter(|state| state.has_active_dispatch())
                    .and_then(|state| state.latest_dispatch_id.clone())
                {
                    let conn = deps.db.separate_conn().unwrap();
                    match crate::db::auto_queue::update_entry_status_on_conn(
                        &conn,
                        &entry_id,
                        crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                        "activate_dispatch_error_recover",
                        &crate::db::auto_queue::EntryStatusUpdateOptions {
                            dispatch_id: Some(dispatch_id),
                            slot_index,
                        },
                    ) {
                        Ok(_) => {
                            drop(conn);
                            continue;
                        }
                        Err(error) => crate::auto_queue_log!(
                            warn,
                            "activate_create_dispatch_recover_failed",
                            entry_log_ctx.clone().maybe_slot_index(slot_index),
                            "[auto-queue] failed to recover entry {entry_id} after create_dispatch error: {error}"
                        ),
                    }
                    drop(conn);
                }

                let recovered_dispatch_id = recovered_state
                    .as_ref()
                    .and_then(|state| state.latest_dispatch_id.as_deref());
                if recovered_state.as_ref().is_some_and(|state| {
                    state.latest_dispatch_id.is_some() || state.status != post_walk.status
                }) {
                    crate::auto_queue_log!(
                        warn,
                        "activate_create_dispatch_error_kept_reservation",
                        entry_log_ctx
                            .clone()
                            .maybe_slot_index(slot_index)
                            .maybe_dispatch(recovered_dispatch_id),
                        "[auto-queue] create_dispatch errored for entry {entry_id} after card progressed to status={} latest_dispatch_id={:?}; keeping reservation",
                        recovered_state
                            .as_ref()
                            .map(|state| state.status.as_str())
                            .unwrap_or("unknown"),
                        recovered_dispatch_id
                    );
                    continue;
                }

                match record_entry_dispatch_failure(
                    deps,
                    &run_id,
                    &entry_id,
                    &card_id,
                    &agent_id,
                    *group,
                    slot_index,
                    "activate_dispatch_create_failed",
                    &error_text,
                    &entry_log_ctx,
                ) {
                    Ok(result) => crate::auto_queue_log!(
                        error,
                        "activate_dispatch_create_failed",
                        entry_log_ctx.clone().maybe_slot_index(slot_index),
                        "[auto-queue] create_dispatch failed for entry {entry_id} (group {group}); retry {}/{} -> {}",
                        result.retry_count,
                        result.retry_limit,
                        result.to_status
                    ),
                    Err(record_error) => crate::auto_queue_log!(
                        error,
                        "activate_dispatch_create_failed",
                        entry_log_ctx.clone().maybe_slot_index(slot_index),
                        "[auto-queue] create_dispatch failed for entry {entry_id} (group {group}); failed to persist retry state: {record_error}"
                    ),
                }
                continue;
            }
        };

        // Mark entry with dispatch_id (#145)
        let dispatch_id = dispatch["id"].as_str().unwrap_or("").to_string();
        let conn = deps.db.separate_conn().unwrap();
        if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
            &conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
            "activate_dispatch_created",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: Some(dispatch_id.clone()),
                slot_index,
            },
        ) {
            crate::auto_queue_log!(
                warn,
                "activate_dispatch_mark_failed",
                entry_log_ctx
                    .clone()
                    .dispatch(&dispatch_id)
                    .maybe_slot_index(slot_index),
                "[auto-queue] failed to mark entry {} dispatched after create_dispatch: {}",
                entry_id,
                error
            );
        }
        drop(conn);

        dispatched_groups_this_activate += 1;
        dispatched.push(deps.entry_json(&entry_id));
    }

    // Check if all entries are done — include 'dispatched' to avoid premature run completion (#179)
    let conn = deps.db.separate_conn().unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND status IN ('pending', 'dispatched')",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if remaining == 0 {
        if let Err(error) = crate::db::auto_queue::release_run_slots(&conn, &run_id) {
            crate::auto_queue_log!(
                warn,
                "activate_release_run_slots_failed",
                run_log_ctx.clone(),
                "[auto-queue] failed to release slots for drained run {run_id}: {error}"
            );
        }
        let still_dispatched: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND status = 'dispatched'",
                [&run_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if still_dispatched == 0 {
            if let Err(error) = crate::db::auto_queue::complete_run_on_conn(&conn, &run_id) {
                crate::auto_queue_log!(
                    warn,
                    "activate_finalize_run_failed",
                    run_log_ctx.clone(),
                    "[auto-queue] failed to finalize run {} after dispatch drain: {}",
                    run_id,
                    error
                );
            }
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

/// POST /api/auto-queue/dispatch
/// Declaratively generate and optionally activate an auto-queue run.
pub async fn dispatch(
    State(state): State<AppState>,
    Json(body): Json<DispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body
        .deploy_phases
        .as_ref()
        .is_some_and(|phases| !phases.is_empty())
        && !deploy_phase_api_enabled(&state)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "deploy_phases requires server.auth_token to be configured"
            })),
        );
    }

    let force = body.force.unwrap_or(false);
    let requested_entries = match normalize_dispatch_entries(&body) {
        Ok(entries) => entries,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
        }
    };
    let issue_numbers: Vec<i64> = requested_entries
        .iter()
        .map(|entry| entry.issue_number)
        .collect();
    let auto_assign_agent = body.auto_assign_agent.unwrap_or(body.agent_id.is_some());

    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut cards_by_issue = match resolve_dispatch_cards(&conn, body.repo.as_ref(), &issue_numbers)
    {
        Ok(cards) => cards,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
        }
    };

    if let Err(err) = apply_dispatch_agent_assignments(
        &conn,
        &mut cards_by_issue,
        body.agent_id.as_deref(),
        auto_assign_agent,
    ) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
    }

    if let Err(err) = validate_dispatchable_cards(&conn, &cards_by_issue) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
    }

    let conflicting_live_runs =
        match find_matching_active_run_id(&conn, body.repo.as_deref(), body.agent_id.as_deref()) {
            Ok(runs) => runs,
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": err})),
                );
            }
        };
    if let Some((run_id, status)) = conflicting_live_runs.first() {
        if !force {
            return existing_live_run_conflict_response(run_id, status);
        }
        let target_run_ids: Vec<String> = conflicting_live_runs
            .iter()
            .map(|(run_id, _)| run_id.clone())
            .collect();
        crate::services::auto_queue::cancel_run::cancel_selected_runs_with_conn(
            state.health_registry.clone(),
            &conn,
            &target_run_ids,
            "auto_queue_force_new_run",
        );
    }
    drop(conn);

    let distinct_groups = requested_entries
        .iter()
        .filter_map(|entry| entry.thread_group)
        .collect::<HashSet<_>>()
        .len()
        .max(1) as i64;
    let generate_body = GenerateBody {
        repo: body.repo.clone(),
        agent_id: body.agent_id.clone(),
        issue_numbers: None,
        entries: Some(requested_entries.clone()),
        mode: None,
        unified_thread: body.unified_thread,
        parallel: None,
        max_concurrent_threads: Some(
            body.max_concurrent_threads
                .unwrap_or(distinct_groups)
                .clamp(1, 10),
        ),
        force: Some(false),
        max_concurrent_per_agent: None,
    };

    let (generate_status, generated_body) =
        generate(State(state.clone()), Json(generate_body)).await;
    if generate_status != StatusCode::OK {
        return (generate_status, generated_body);
    }

    let run_id = match generated_body
        .0
        .get("run")
        .and_then(|run| run.get("id"))
        .and_then(Value::as_str)
    {
        Some(run_id) => run_id.to_string(),
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "dispatch generation did not produce a run"})),
            );
        }
    };

    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    if let Some(ref deploy_phases) = body.deploy_phases {
        if !deploy_phases.is_empty() {
            if let Ok(json_str) = serde_json::to_string(deploy_phases) {
                let _ = conn.execute(
                    "UPDATE auto_queue_runs SET deploy_phases = ?1 WHERE id = ?2",
                    libsql_rusqlite::params![json_str, run_id],
                );
            }
        }
    }

    let mut rank_per_group = HashMap::<i64, i64>::new();
    for entry in &requested_entries {
        let thread_group = entry.thread_group.unwrap_or(0);
        let priority_rank = rank_per_group.entry(thread_group).or_insert(0);
        let Some(card) = cards_by_issue.get(&entry.issue_number) else {
            continue;
        };
        if let Err(err) = conn.execute(
            "UPDATE auto_queue_entries
             SET thread_group = ?1,
                 priority_rank = ?2
             WHERE run_id = ?3
               AND kanban_card_id = ?4",
            libsql_rusqlite::params![thread_group, *priority_rank, run_id, card.card_id],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{err}")})),
            );
        }
        *priority_rank += 1;
    }
    drop(conn);

    let activate_now = body.activate.unwrap_or(true);
    let activation = if activate_now {
        let (activate_status, activate_body) = activate(
            State(state.clone()),
            Json(ActivateBody {
                run_id: Some(run_id.clone()),
                repo: body.repo.clone(),
                agent_id: body.agent_id.clone(),
                thread_group: None,
                unified_thread: body.unified_thread,
                active_only: Some(false),
            }),
        )
        .await;
        if activate_status != StatusCode::OK {
            return (activate_status, activate_body);
        }
        Some(activate_body.0)
    } else {
        None
    };

    let mut snapshot = state
        .auto_queue_service()
        .status_json_for_run(
            &run_id,
            crate::services::auto_queue::StatusInput {
                repo: body.repo.clone(),
                agent_id: body.agent_id.clone(),
                guild_id: None,
            },
        )
        .unwrap_or_else(|_| {
            json!({
                "run": null,
                "entries": [],
                "agents": {},
                "thread_groups": {},
            })
        });
    if let Some(obj) = snapshot.as_object_mut() {
        obj.insert("activated".to_string(), json!(activate_now));
        obj.insert(
            "requested".to_string(),
            json!({
                "groups": body.groups.len(),
                "issues": issue_numbers,
                "auto_assign_agent": auto_assign_agent,
            }),
        );
        if let Some(activation) = activation {
            obj.insert("dispatch".to_string(), activation);
        }
    }

    (StatusCode::OK, Json(snapshot))
}

/// GET /api/auto-queue/status
pub async fn status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let input = crate::services::auto_queue::StatusInput {
        repo: query.repo,
        agent_id: query.agent_id,
        guild_id: state.config.discord.guild_id.clone(),
    };

    let result = if let Some(pool) = state.pg_pool.as_ref() {
        state.auto_queue_service().status_with_pg(pool, input).await
    } else {
        state.auto_queue_service().status(input)
    };

    match result {
        Ok(response) => (StatusCode::OK, Json(json!(response))),
        Err(error) => error.into_json_response(),
    }
}

/// GET /api/auto-queue/history
pub async fn history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let limit = query.limit.unwrap_or(8).clamp(1, 20);
    let filter = crate::db::auto_queue::StatusFilter {
        repo: query.repo,
        agent_id: query.agent_id,
    };
    let records = if let Some(pool) = state.pg_pool.as_ref() {
        match crate::db::auto_queue::list_run_history_pg(pool, &filter, limit).await {
            Ok(records) => records,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("list run history: {error}")})),
                );
            }
        }
    } else {
        let conn = match state.db.separate_conn() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        match crate::db::auto_queue::list_run_history(&conn, &filter, limit) {
            Ok(records) => records,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("list run history: {error}")})),
                );
            }
        }
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    let runs: Vec<AutoQueueHistoryRun> = records
        .into_iter()
        .map(|record| {
            let entry_count = record.entry_count.max(0);
            let completed_count = record.done_count.max(0);
            let unresolved_count = (entry_count - completed_count).max(0) as f64;
            let total_entries = entry_count.max(1) as f64;
            let success_rate = if entry_count > 0 {
                completed_count as f64 / total_entries
            } else {
                0.0
            };
            let failure_rate = if entry_count > 0 {
                unresolved_count / total_entries
            } else {
                0.0
            };
            AutoQueueHistoryRun {
                id: record.id,
                repo: record.repo,
                agent_id: record.agent_id,
                status: record.status,
                created_at: record.created_at,
                completed_at: record.completed_at,
                duration_ms: record
                    .completed_at
                    .unwrap_or(now_ms)
                    .saturating_sub(record.created_at),
                entry_count,
                done_count: record.done_count,
                skipped_count: record.skipped_count,
                pending_count: record.pending_count,
                dispatched_count: record.dispatched_count,
                success_rate,
                failure_rate,
            }
        })
        .collect();

    let total_runs = runs.len();
    let completed_runs = runs.iter().filter(|run| run.status == "completed").count();
    let success_rate = if total_runs > 0 {
        runs.iter().map(|run| run.success_rate).sum::<f64>() / total_runs as f64
    } else {
        0.0
    };
    let failure_rate = if total_runs > 0 {
        runs.iter().map(|run| run.failure_rate).sum::<f64>() / total_runs as f64
    } else {
        0.0
    };

    (
        StatusCode::OK,
        Json(json!({
            "summary": AutoQueueHistorySummary {
                total_runs,
                completed_runs,
                success_rate,
                failure_rate,
            },
            "runs": runs,
        })),
    )
}

/// PATCH /api/auto-queue/entries/{id}
pub async fn update_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateEntryBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.thread_group.is_none()
        && body.priority_rank.is_none()
        && body.batch_phase.is_none()
        && body.status.is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }
    if let Some(thread_group) = body.thread_group {
        if thread_group < 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "thread_group must be >= 0"})),
            );
        }
    }
    if let Some(priority_rank) = body.priority_rank {
        if priority_rank < 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "priority_rank must be >= 0"})),
            );
        }
    }
    if let Some(batch_phase) = body.batch_phase {
        if batch_phase < 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "batch_phase must be >= 0"})),
            );
        }
    }
    let requested_status = match body.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(crate::db::auto_queue::ENTRY_STATUS_PENDING) => {
            Some(crate::db::auto_queue::ENTRY_STATUS_PENDING)
        }
        Some(crate::db::auto_queue::ENTRY_STATUS_SKIPPED) => {
            Some(crate::db::auto_queue::ENTRY_STATUS_SKIPPED)
        }
        Some(crate::db::auto_queue::ENTRY_STATUS_DISPATCHED)
        | Some(crate::db::auto_queue::ENTRY_STATUS_DONE) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "manual entry status updates only support pending or skipped"
                })),
            );
        }
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("unsupported entry status '{other}'")})),
            );
        }
    };

    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let entry_info: Option<(String, String)> = conn
        .query_row(
            "SELECT run_id, status
             FROM auto_queue_entries
             WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    let Some((run_id, status)) = entry_info else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "entry not found"})),
        );
    };

    let mut effective_status = status.clone();
    if let Some(new_status) = requested_status {
        match crate::db::auto_queue::update_entry_status_on_conn(
            &conn,
            &id,
            new_status,
            "manual_update",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        ) {
            Ok(result) => effective_status = result.to_status,
            Err(crate::db::auto_queue::EntryStatusUpdateError::NotFound { .. }) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "entry not found"})),
                );
            }
            Err(crate::db::auto_queue::EntryStatusUpdateError::InvalidTransition { .. }) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": format!(
                            "entry status transition not allowed: {} -> {}",
                            status, new_status
                        ),
                    })),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }
    }

    if body.thread_group.is_some() || body.priority_rank.is_some() || body.batch_phase.is_some() {
        if effective_status != crate::db::auto_queue::ENTRY_STATUS_PENDING {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "only pending entries can be reprioritized"})),
            );
        }

        let changed = conn
            .execute(
                "UPDATE auto_queue_entries
                 SET thread_group = COALESCE(?1, thread_group),
                     priority_rank = COALESCE(?2, priority_rank),
                     batch_phase = COALESCE(?3, batch_phase)
                 WHERE id = ?4
                   AND status = 'pending'",
                libsql_rusqlite::params![
                    body.thread_group,
                    body.priority_rank,
                    body.batch_phase,
                    id
                ],
            )
            .unwrap_or(0);
        if changed == 0 {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "entry not found or not pending"})),
            );
        }

        if body.thread_group.is_some() {
            if let Err(err) = crate::db::auto_queue::sync_run_group_metadata(&conn, &run_id) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{err}")})),
                );
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "entry": state
                .auto_queue_service()
                .entry_json(&id, None)
                .unwrap_or(serde_json::Value::Null),
        })),
    )
}

/// POST /api/auto-queue/runs/{id}/entries
async fn add_run_entry_with_pg(
    state: &AppState,
    run_id: &str,
    body: &AddRunEntryBody,
    batch_phase: i64,
    pool: &sqlx::PgPool,
) -> (StatusCode, Json<serde_json::Value>) {
    let run_row = match sqlx::query(
        "SELECT status, repo, agent_id
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load auto-queue run '{run_id}': {error}")})),
            );
        }
    };
    let Some(run_row) = run_row else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("auto-queue run '{run_id}' not found")})),
        );
    };

    let run_status: String = match run_row.try_get("status") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run status: {error}")})),
            );
        }
    };
    let run_repo: Option<String> = match run_row.try_get("repo") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run repo: {error}")})),
            );
        }
    };
    let run_agent_id: Option<String> = match run_row.try_get("agent_id") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run agent: {error}")})),
            );
        }
    };
    if run_status != "active" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("auto-queue run '{run_id}' is not active (status={run_status})"),
                "run_id": run_id,
                "status": run_status,
            })),
        );
    }

    let issue_numbers = [body.issue_number];
    let cards_by_issue =
        match resolve_dispatch_cards_with_pg(pool, run_repo.as_ref(), &issue_numbers).await {
            Ok(cards) => cards,
            Err(err) => {
                let status = if err.contains("not found") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_REQUEST
                };
                return (status, Json(json!({"error": err})));
            }
        };
    let Some(card) = cards_by_issue.get(&body.issue_number) else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": format!("kanban card not found for issue #{}", body.issue_number)}),
            ),
        );
    };
    if card.status != "ready" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "issue #{} must be in ready status to be added to an active run (current={})",
                    body.issue_number,
                    card.status
                )
            })),
        );
    }

    let run_agent = run_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let card_agent = card
        .assigned_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (run_agent, card_agent) {
        (_, None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("issue #{} has no assigned agent", body.issue_number)
                })),
            );
        }
        (Some(run_agent), Some(card_agent)) if run_agent != card_agent => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "issue #{} is assigned to {}, not the active run agent {}",
                        body.issue_number,
                        card_agent,
                        run_agent
                    )
                })),
            );
        }
        _ => {}
    }

    let inserted = match enqueue_entries_into_existing_run_with_pg(
        pool,
        run_id,
        &[GenerateEntryBody {
            issue_number: body.issue_number,
            batch_phase: Some(batch_phase),
            thread_group: body.thread_group,
        }],
        &cards_by_issue,
    )
    .await
    {
        Ok(entries) => entries,
        Err(err) => {
            let status = if err.contains("already queued") || err.contains("active dispatch") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            return (status, Json(json!({"error": err})));
        }
    };
    let Some(inserted_entry) = inserted.into_iter().next() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to create auto-queue entry"})),
        );
    };
    let entry = state
        .auto_queue_service()
        .entry_json_with_pg(pool, &inserted_entry.entry_id, None)
        .await
        .unwrap_or(serde_json::Value::Null);

    (
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "run_id": run_id,
            "thread_group": inserted_entry.thread_group,
            "priority_rank": inserted_entry.priority_rank,
            "entry": entry,
        })),
    )
}

pub async fn add_run_entry(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<AddRunEntryBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.issue_number <= 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "issue_number must be > 0"})),
        );
    }
    if let Some(thread_group) = body.thread_group {
        if thread_group < 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "thread_group must be >= 0"})),
            );
        }
    }
    let batch_phase = body.batch_phase.unwrap_or(0);
    if batch_phase < 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "batch_phase must be >= 0"})),
        );
    }
    if let Some(pg_pool) = state.pg_pool.clone() {
        return add_run_entry_with_pg(&state, &run_id, &body, batch_phase, &pg_pool).await;
    }

    let mut conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let run_info: Option<(String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT status, repo, agent_id
             FROM auto_queue_runs
             WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok();
    let Some((run_status, run_repo, run_agent_id)) = run_info else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("auto-queue run '{run_id}' not found")})),
        );
    };
    if run_status != "active" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("auto-queue run '{run_id}' is not active (status={run_status})"),
                "run_id": run_id,
                "status": run_status,
            })),
        );
    }

    let issue_numbers = [body.issue_number];
    let cards_by_issue = match resolve_dispatch_cards(&conn, run_repo.as_ref(), &issue_numbers) {
        Ok(cards) => cards,
        Err(err) => {
            let status = if err.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            return (status, Json(json!({"error": err})));
        }
    };
    let Some(card) = cards_by_issue.get(&body.issue_number) else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": format!("kanban card not found for issue #{}", body.issue_number)}),
            ),
        );
    };
    if card.status != "ready" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "issue #{} must be in ready status to be added to an active run (current={})",
                    body.issue_number,
                    card.status
                )
            })),
        );
    }

    let run_agent = run_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let card_agent = card
        .assigned_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (run_agent, card_agent) {
        (_, None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("issue #{} has no assigned agent", body.issue_number)
                })),
            );
        }
        (Some(run_agent), Some(card_agent)) if run_agent != card_agent => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "issue #{} is assigned to {}, not the active run agent {}",
                        body.issue_number,
                        card_agent,
                        run_agent
                    )
                })),
            );
        }
        _ => {}
    }

    let inserted = match enqueue_entries_into_existing_run(
        &mut conn,
        &run_id,
        &[GenerateEntryBody {
            issue_number: body.issue_number,
            batch_phase: Some(batch_phase),
            thread_group: body.thread_group,
        }],
        &cards_by_issue,
    ) {
        Ok(entries) => entries,
        Err(err) => {
            let status = if err.contains("already queued") || err.contains("active dispatch") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            return (status, Json(json!({"error": err})));
        }
    };
    let Some(inserted_entry) = inserted.into_iter().next() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to create auto-queue entry"})),
        );
    };

    (
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "run_id": run_id,
            "thread_group": inserted_entry.thread_group,
            "priority_rank": inserted_entry.priority_rank,
            "entry": state
                .auto_queue_service()
                .entry_json(&inserted_entry.entry_id, None)
                .unwrap_or(serde_json::Value::Null),
        })),
    )
}

/// POST /api/auto-queue/runs/{id}/restore
pub async fn restore_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
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
    let run_status: Option<String> = conn
        .query_row(
            "SELECT status
             FROM auto_queue_runs
             WHERE id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .ok();
    match run_status.as_deref() {
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("auto-queue run '{run_id}' not found")})),
            );
        }
        Some("cancelled") | Some(RUN_STATUS_RESTORING) => {}
        Some("active") => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("auto-queue run '{run_id}' is already active")})),
            );
        }
        Some(status) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "only cancelled or restoring runs can be restored (status={status})"
                    ),
                    "run_id": run_id,
                    "status": status,
                })),
            );
        }
    }

    let deps = AutoQueueActivateDeps::from_state(&state);
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut counts = RestoreRunCounts::default();
    let mut dispatch_candidates = Vec::new();

    match apply_restore_state_changes(&mut conn, &run_id, run_status.as_deref()) {
        Ok((applied_counts, candidates)) => {
            counts = applied_counts;
            dispatch_candidates = candidates;
        }
        Err(error) => errors.push(error),
    }

    if errors.is_empty() {
        for candidate in &dispatch_candidates {
            match attempt_restore_dispatch(&deps, &run_id, candidate) {
                Ok(result) => {
                    if result.dispatched {
                        counts.restored_pending = counts.restored_pending.saturating_sub(1);
                        counts.restored_dispatched += 1;
                    }
                    if result.created_dispatch {
                        counts.created_dispatches += 1;
                    }
                    if result.rebound_slot {
                        counts.rebound_slots += 1;
                    }
                    if result.unbound_dispatch {
                        counts.unbound_dispatches += 1;
                    }
                }
                Err(error) => warnings.push(error),
            }
        }

        if let Err(error) = finalize_restore_run(&conn, &run_id) {
            errors.push(error);
        }
    }
    let final_run_status = conn
        .query_row(
            "SELECT status
             FROM auto_queue_runs
             WHERE id = ?1",
            [&run_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "unknown".to_string());
    drop(conn);

    let mut payload = json!({
        "ok": errors.is_empty(),
        "run_id": run_id,
        "run_status": final_run_status,
        "restored_pending": counts.restored_pending,
        "restored_done": counts.restored_done,
        "restored_dispatched": counts.restored_dispatched,
        "rebound_slots": counts.rebound_slots,
        "created_dispatches": counts.created_dispatches,
        "unbound_dispatches": counts.unbound_dispatches,
    });
    if !errors.is_empty() {
        payload["errors"] = json!(errors);
    }
    if counts.unbound_dispatches > 0 {
        warnings.push(format!(
            "{} restored dispatch(es) still need slot rebind",
            counts.unbound_dispatches
        ));
    }
    if !warnings.is_empty() {
        payload["warning"] = json!(warnings.join("; "));
    }

    (StatusCode::OK, Json(payload))
}

/// POST /api/auto-queue/slots/{agent_id}/{slot_index}/rebind
pub async fn rebind_slot(
    State(state): State<AppState>,
    Path((agent_id, slot_index)): Path<(String, i64)>,
    Json(body): Json<RebindSlotBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if slot_index < 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "slot_index must be >= 0"})),
        );
    }
    if body.thread_group < 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "thread_group must be >= 0"})),
        );
    }
    let run_id = body.run_id.trim();
    if run_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "run_id is required"})),
        );
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
    let run_status: Option<String> = conn
        .query_row(
            "SELECT status
             FROM auto_queue_runs
             WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .ok();
    match run_status.as_deref() {
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("auto-queue run '{run_id}' not found")})),
            );
        }
        Some("active") | Some("paused") => {}
        Some(status) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("slot rebind requires an active or paused run (status={status})"),
                    "run_id": run_id,
                    "status": status,
                })),
            );
        }
    }

    let slot_pool_size = crate::db::auto_queue::run_slot_pool_size(&conn, run_id);
    if slot_index >= slot_pool_size {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "slot_index {} is outside the slot pool for run '{}' (size={})",
                    slot_index,
                    run_id,
                    slot_pool_size
                ),
            })),
        );
    }

    let current_binding: Option<(Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = ?1
               AND slot_index = ?2",
            libsql_rusqlite::params![&agent_id, slot_index],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let same_binding = current_binding
        .as_ref()
        .is_some_and(|(assigned_run_id, assigned_group)| {
            assigned_run_id.as_deref() == Some(run_id)
                && assigned_group.unwrap_or_default() == body.thread_group
        });
    if !same_binding
        && crate::db::auto_queue::slot_has_active_dispatch(&conn, &agent_id, slot_index)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "slot {} for {} has an active dispatch; reset or complete it before rebind",
                    slot_index, agent_id
                ),
            })),
        );
    }

    let updated_entries = match crate::db::auto_queue::rebind_slot_for_group_agent(
        &conn,
        run_id,
        body.thread_group,
        &agent_id,
        slot_index,
    ) {
        Ok(updated_entries) => updated_entries,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "agent_id": agent_id,
            "slot_index": slot_index,
            "run_id": run_id,
            "thread_group": body.thread_group,
            "rebound": !same_binding,
            "updated_entries": updated_entries,
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
    match crate::db::auto_queue::update_entry_status_on_conn(
        &conn,
        &id,
        crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
        "manual_skip",
        &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
    ) {
        Ok(result) if result.changed => {}
        Ok(_) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "entry not found or not pending"})),
            );
        }
        Err(crate::db::auto_queue::EntryStatusUpdateError::NotFound { .. }) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "entry not found"})),
            );
        }
        Err(crate::db::auto_queue::EntryStatusUpdateError::InvalidTransition { .. }) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "only pending entries can be skipped"})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// PATCH /api/auto-queue/runs/{id}
pub async fn update_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRunBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body
        .deploy_phases
        .as_ref()
        .is_some_and(|phases| !phases.is_empty())
        && !deploy_phase_api_enabled(&state)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "deploy_phases requires server.auth_token to be configured"
            })),
        );
    }

    if let Some(pool) = state.pg_pool.as_ref() {
        if let Some(max_concurrent_threads) = body.max_concurrent_threads {
            if max_concurrent_threads <= 0 {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "max_concurrent_threads must be > 0"})),
                );
            }
        }

        let ignored_unified_thread = body.unified_thread.is_some();
        if body.status.is_none()
            && body.deploy_phases.is_none()
            && body.max_concurrent_threads.is_none()
            && !ignored_unified_thread
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "no fields to update"})),
            );
        }

        return match update_run_with_pg(&id, &body, pool).await {
            Ok(_) => (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "ignored": ignored_unified_thread.then_some(vec!["unified_thread"]),
                })),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
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
                libsql_rusqlite::params![status, id],
            )
            .unwrap_or(0);
    }

    if let Some(ref deploy_phases) = body.deploy_phases {
        if let Ok(json_str) = serde_json::to_string(deploy_phases) {
            changed += conn
                .execute(
                    "UPDATE auto_queue_runs SET deploy_phases = ?1 WHERE id = ?2",
                    libsql_rusqlite::params![json_str, id],
                )
                .unwrap_or(0);
        }
    }

    if let Some(max_concurrent_threads) = body.max_concurrent_threads {
        if max_concurrent_threads <= 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "max_concurrent_threads must be > 0"})),
            );
        }
        changed += conn
            .execute(
                "UPDATE auto_queue_runs
                 SET max_concurrent_threads = ?1
                 WHERE id = ?2",
                libsql_rusqlite::params![max_concurrent_threads, id],
            )
            .unwrap_or(0);
    }

    let ignored_unified_thread = body.unified_thread.is_some();
    if changed == 0
        && body.status.is_none()
        && body.deploy_phases.is_none()
        && body.max_concurrent_threads.is_none()
        && !ignored_unified_thread
    {
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
    if let Some(pool) = state.pg_pool.as_ref() {
        return match crate::services::auto_queue::runtime::reset_slot_thread_bindings_pg(
            pool, &agent_id, slot_index,
        )
        .await
        {
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
        };
    }

    match crate::services::auto_queue::runtime::reset_slot_thread_bindings(
        &state.db, &agent_id, slot_index,
    )
    .await
    {
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

    if let Some(pool) = state.pg_pool.as_ref() {
        return match reset_with_pg(&body, pool).await {
            Ok(response) => (StatusCode::OK, Json(response)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
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
                libsql_rusqlite::params![agent_id],
            )
            .unwrap_or(0);
        let completed_runs = conn
                .execute(
                    "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') \
                     WHERE status IN ('generated', 'pending', 'active', 'paused') AND agent_id = ?1",
                    libsql_rusqlite::params![agent_id],
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
            crate::auto_queue_log!(
                warn,
                "reset_global_preserved_active_runs",
                AutoQueueLogContext::new(),
                "[auto-queue] Global reset requested without agent_id; preserving {protected_active_runs} active run(s)"
            );
        } else {
            crate::auto_queue_log!(
                warn,
                "reset_global_unscoped",
                AutoQueueLogContext::new(),
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
    if let Some(pool) = state.pg_pool.as_ref() {
        return match pause_with_pg(state.health_registry.clone(), pool).await {
            Ok(response) => (StatusCode::OK, Json(response)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
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
    let active_run_ids =
        crate::services::auto_queue::cancel_run::load_run_ids_with_status(&conn, &["active"])
            .unwrap_or_default();
    let cancelled_dispatches =
        crate::services::auto_queue::cancel_run::cancel_live_dispatches_for_runs(
            &conn,
            &active_run_ids,
            "auto_queue_pause",
        );
    let slot_cleanup = crate::services::auto_queue::cancel_run::clear_and_release_slots_for_runs(
        state.health_registry.clone(),
        &conn,
        &active_run_ids,
    );
    let paused = conn
        .execute(
            "UPDATE auto_queue_runs
             SET status = 'paused',
                 completed_at = NULL
             WHERE status = 'active'",
            [],
        )
        .unwrap_or(0);
    let mut response = json!({
        "ok": true,
        "paused_runs": paused,
        "cancelled_dispatches": cancelled_dispatches,
        "released_slots": slot_cleanup.released_slots,
        "cleared_slot_sessions": slot_cleanup.cleared_slot_sessions,
    });
    if let Some(warning) =
        crate::services::auto_queue::cancel_run::slot_cleanup_warning(&slot_cleanup.warnings)
    {
        response["warning"] = json!(warning);
    }
    (StatusCode::OK, Json(response))
}

fn cancel_route_error_response(
    error: crate::error::AppError,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut body = json!({ "error": error.message() });
    if let Some(run_id) = error.context().get("run_id") {
        body["run_id"] = run_id.clone();
    }
    if let Some(status) = error.context().get("status") {
        body["status"] = status.clone();
    }
    (error.status(), Json(body))
}

/// POST /api/auto-queue/resume — resume paused runs and dispatch next entry
pub async fn resume_run(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
        let blocked_runs = match sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)
             FROM auto_queue_runs r
             WHERE r.status = 'paused'
               AND EXISTS (
                   SELECT 1
                   FROM auto_queue_phase_gates pg
                   WHERE pg.run_id = r.id
                     AND pg.status IN ('pending', 'failed')
               )",
        )
        .fetch_one(pool)
        .await
        {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("count postgres blocked auto-queue runs: {error}")}),
                    ),
                );
            }
        };
        let resumed = match sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'active',
                 completed_at = NULL
             WHERE status = 'paused'
               AND NOT EXISTS (
                   SELECT 1
                   FROM auto_queue_phase_gates pg
                   WHERE pg.run_id = auto_queue_runs.id
                     AND pg.status IN ('pending', 'failed')
               )",
        )
        .execute(pool)
        .await
        {
            Ok(result) => result.rows_affected() as i64,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("resume postgres auto-queue runs: {error}")})),
                );
            }
        };

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
                Json(
                    json!({"ok": true, "resumed_runs": resumed, "blocked_runs": blocked_runs, "dispatched": dispatched}),
                ),
            );
        }

        return (
            StatusCode::OK,
            Json(
                json!({"ok": true, "resumed_runs": 0, "blocked_runs": blocked_runs, "message": "No resumable runs"}),
            ),
        );
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
    let blocked_runs: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM auto_queue_runs r
             WHERE r.status = 'paused'
               AND EXISTS (
                   SELECT 1
                   FROM auto_queue_phase_gates pg
                   WHERE pg.run_id = r.id
                     AND pg.status IN ('pending', 'failed')
               )",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let resumed = conn
        .execute(
            "UPDATE auto_queue_runs
             SET status = 'active'
             WHERE status = 'paused'
               AND NOT EXISTS (
                   SELECT 1
                   FROM auto_queue_phase_gates pg
                   WHERE pg.run_id = auto_queue_runs.id
                     AND pg.status IN ('pending', 'failed')
               )",
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
            Json(
                json!({"ok": true, "resumed_runs": resumed, "blocked_runs": blocked_runs, "dispatched": dispatched}),
            ),
        );
    }

    (
        StatusCode::OK,
        Json(
            json!({"ok": true, "resumed_runs": 0, "blocked_runs": blocked_runs, "message": "No resumable runs"}),
        ),
    )
}

/// POST /api/auto-queue/cancel — cancel all active/paused runs and pending entries
pub async fn cancel(
    State(state): State<AppState>,
    Query(query): Query<CancelQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
        let service = state.auto_queue_service();
        let result = if let Some(run_id) = query
            .run_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            service
                .cancel_run_with_pg(state.health_registry.clone(), pool, run_id)
                .await
        } else {
            service
                .cancel_runs_with_pg(state.health_registry.clone(), pool)
                .await
        };
        return match result {
            Ok(payload) => (StatusCode::OK, Json(payload)),
            Err(error) => cancel_route_error_response(error),
        };
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
    let service = state.auto_queue_service();
    let result = if let Some(run_id) = query
        .run_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        service.cancel_run(state.health_registry.clone(), &conn, run_id)
    } else {
        service.cancel_runs(state.health_registry.clone(), &conn)
    };
    match result {
        Ok(payload) => (StatusCode::OK, Json(payload)),
        Err(error) => cancel_route_error_response(error),
    }
}

/// PATCH /api/auto-queue/reorder
pub async fn reorder(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool.as_ref() {
        return match reorder_with_pg(&body, pool).await {
            Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
            Err(error) if error.starts_with("not_found:") => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": error.trim_start_matches("not_found:")})),
            ),
            Err(error)
                if error == "orderedIds cannot be empty"
                    || error == "no pending entries found for reorder scope"
                    || error == "orderedIds do not match any pending entries in scope"
                    || error == "replacement sequence exhausted"
                    || error == "replacement sequence was not fully consumed" =>
            {
                (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
            }
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

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
            libsql_rusqlite::params![rank as i64, id],
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

// ── Authenticated order submission callback ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OrderBody {
    /// Ordered list of GitHub issue numbers (or card IDs)
    pub order: Vec<serde_json::Value>,
    pub rationale: Option<String>,
    /// Alias for rationale (compatibility)
    pub reasoning: Option<String>,
}

/// POST /api/auto-queue/runs/:id/order
/// Authenticated callback: provides the ordered card list for a pending run.
async fn resolve_submit_order_card_with_pg(
    pool: &sqlx::PgPool,
    run_repo: Option<&str>,
    item: &serde_json::Value,
) -> Result<Option<ResolvedDispatchCard>, String> {
    let row = if let Some(issue_number) = item.as_i64() {
        sqlx::query(
            "SELECT id,
                    repo_id,
                    status,
                    assigned_agent_id,
                    github_issue_number::BIGINT AS github_issue_number
             FROM kanban_cards
             WHERE github_issue_number = $1
               AND ($2::TEXT IS NULL OR repo_id = $2)
             ORDER BY id ASC
             LIMIT 1",
        )
        .bind(issue_number)
        .bind(run_repo)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load kanban card for issue #{issue_number}: {error}"))?
    } else if let Some(card_id) = item.as_str() {
        sqlx::query(
            "SELECT id,
                    repo_id,
                    status,
                    assigned_agent_id,
                    github_issue_number::BIGINT AS github_issue_number
             FROM kanban_cards
             WHERE id = $1
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load kanban card {card_id}: {error}"))?
    } else {
        None
    };

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(ResolvedDispatchCard {
        issue_number: row
            .try_get("github_issue_number")
            .map_err(|error| format!("decode github_issue_number: {error}"))?,
        card_id: row
            .try_get("id")
            .map_err(|error| format!("decode card id: {error}"))?,
        repo_id: row
            .try_get("repo_id")
            .map_err(|error| format!("decode repo_id: {error}"))?,
        status: row
            .try_get("status")
            .map_err(|error| format!("decode status: {error}"))?,
        assigned_agent_id: row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id: {error}"))?,
    }))
}

async fn submit_order_with_pg(
    state: &AppState,
    run_id: &str,
    headers: &HeaderMap,
    body: &OrderBody,
    pool: &sqlx::PgPool,
) -> (StatusCode, Json<serde_json::Value>) {
    let caller_agent_id =
        crate::server::routes::kanban::resolve_requesting_agent_id_with_pg(pool, headers).await;
    let run_row = match sqlx::query(
        "SELECT status, repo
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load auto-queue run '{run_id}': {error}")})),
            );
        }
    };
    let Some(run_row) = run_row else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "run not found or not pending"})),
        );
    };
    let run_status: String = match run_row.try_get("status") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run status: {error}")})),
            );
        }
    };
    if run_status != "pending" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "run not found or not pending"})),
        );
    }
    let run_repo: Option<String> = match run_row.try_get("repo") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run repo: {error}")})),
            );
        }
    };
    let run_log_ctx = AutoQueueLogContext::new().run(run_id);

    let mut created = 0;
    for (rank, item) in body.order.iter().enumerate() {
        let card = match resolve_submit_order_card_with_pg(pool, run_repo.as_deref(), item).await {
            Ok(Some(card)) => card,
            Ok(None) => continue,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

        let dispatchable_check = crate::pipeline::try_get()
            .map(|pipeline| {
                pipeline
                    .dispatchable_states()
                    .iter()
                    .any(|state| *state == card.status)
            })
            .unwrap_or(card.status == "ready");
        if !dispatchable_check {
            crate::auto_queue_log!(
                info,
                "submit_order_card_not_dispatchable",
                run_log_ctx.clone().card(&card.card_id),
                "[auto-queue] Skipping card {} (status={}, not dispatchable)",
                card.card_id,
                card.status
            );
            continue;
        }

        let entry_id = uuid::Uuid::new_v4().to_string();
        if sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&entry_id)
        .bind(run_id)
        .bind(&card.card_id)
        .bind(card.assigned_agent_id.as_deref().unwrap_or(""))
        .bind(rank as i64)
        .execute(pool)
        .await
        .is_ok()
        {
            created += 1;
        }
    }

    let rationale = body
        .rationale
        .clone()
        .or(body.reasoning.clone())
        .unwrap_or_else(|| {
            caller_agent_id
                .as_deref()
                .map(|agent_id| format!("{agent_id} order submitted"))
                .unwrap_or_else(|| "API order submitted".to_string())
        });
    if created > 0 {
        if let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'active',
                 ai_rationale = $1
             WHERE id = $2",
        )
        .bind(&rationale)
        .bind(run_id)
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("activate auto-queue run '{run_id}': {error}")})),
            );
        }
    } else {
        crate::auto_queue_log!(
            warn,
            "submit_order_no_ready_cards",
            run_log_ctx.clone(),
            "[auto-queue] submit_order: no ready cards enqueued, run {run_id} stays pending"
        );
        if let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'completed',
                 ai_rationale = $1
             WHERE id = $2",
        )
        .bind(format!("{rationale} (no ready cards — auto-completed)"))
        .bind(run_id)
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("complete auto-queue run '{run_id}': {error}")})),
            );
        }
    }

    let _ = state;

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

pub async fn submit_order(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<OrderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) =
        crate::server::routes::kanban::require_explicit_bearer_token(&headers, "submit_order")
    {
        return response;
    }
    if let Some(pg_pool) = state.pg_pool.clone() {
        return submit_order_with_pg(&state, &run_id, &headers, &body, &pg_pool).await;
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
    let caller_agent_id =
        crate::server::routes::kanban::resolve_requesting_agent_id_on_conn(&conn, &headers);
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
    let run_log_ctx = AutoQueueLogContext::new().run(&run_id);

    // Create entries from the ordered list
    let mut created = 0;
    for (rank, item) in body.order.iter().enumerate() {
        // Item can be issue number (i64) or card_id (string)
        // When matching by issue number, filter by repo to prevent cross-repo collisions
        let card_id: Option<String> = if let Some(num) = item.as_i64() {
            if let Some(ref repo) = run_repo {
                conn.query_row(
                    "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
                    libsql_rusqlite::params![num, repo],
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
            crate::auto_queue_log!(
                info,
                "submit_order_card_not_dispatchable",
                run_log_ctx.clone().card(&card_id),
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
            libsql_rusqlite::params![entry_id, run_id, card_id, agent_id, rank as i64],
        )
        .ok();
        created += 1;
    }

    // Only activate if at least one card was enqueued; otherwise leave as pending
    // to prevent the activate() fallback from filling the run with unintended cards
    let rationale = body
        .rationale
        .clone()
        .or(body.reasoning.clone())
        .unwrap_or_else(|| {
            caller_agent_id
                .as_deref()
                .map(|agent_id| format!("{agent_id} order submitted"))
                .unwrap_or_else(|| "API order submitted".to_string())
        });
    if created > 0 {
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'active', ai_rationale = ?1 WHERE id = ?2",
            libsql_rusqlite::params![rationale, run_id],
        )
        .ok();
    } else {
        crate::auto_queue_log!(
            warn,
            "submit_order_no_ready_cards",
            run_log_ctx.clone(),
            "[auto-queue] submit_order: no ready cards enqueued, run {run_id} stays pending"
        );
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'completed', ai_rationale = ?1 WHERE id = ?2",
            libsql_rusqlite::params![
                format!("{rationale} (no ready cards — auto-completed)"),
                run_id
            ],
        )
        .ok();
    }

    // Queue created and activated — dispatch is a separate step via POST /api/auto-queue/activate
    // This allows the caller to review/adjust the order before dispatching begins.
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
    use super::{
        GenerateCandidate, QueueEntryOrder, build_group_plan, extract_dependency_numbers,
        extract_dependency_parse_result, reorder_entry_ids,
        slot_requires_thread_reset_before_reuse,
    };
    use libsql_rusqlite::Connection;
    use std::collections::HashMap;

    fn entry(id: &str, status: &str, agent_id: &str) -> QueueEntryOrder {
        QueueEntryOrder {
            id: id.to_string(),
            status: status.to_string(),
            agent_id: agent_id.to_string(),
        }
    }

    fn candidate(
        issue_number: i64,
        priority: &str,
        description: Option<&str>,
        metadata: Option<&str>,
    ) -> GenerateCandidate {
        GenerateCandidate {
            card_id: format!("card-{issue_number}"),
            agent_id: "agent-a".to_string(),
            priority: priority.to_string(),
            description: description.map(str::to_string),
            metadata: metadata.map(str::to_string),
            github_issue_number: Some(issue_number),
        }
    }

    fn slot_reset_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE auto_queue_slots (
                agent_id TEXT NOT NULL,
                slot_index INTEGER NOT NULL,
                thread_id_map TEXT,
                PRIMARY KEY (agent_id, slot_index)
            );
            CREATE TABLE task_dispatches (
                id TEXT PRIMARY KEY,
                to_agent_id TEXT,
                thread_id TEXT,
                context TEXT
            );",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn slot_thread_reset_requires_new_assignment() {
        let conn = slot_reset_conn();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-a', 0, '{\"123\":\"thread-1\"}')",
            [],
        )
        .expect("seed slot binding");

        assert!(
            !slot_requires_thread_reset_before_reuse(&conn, "agent-a", 0, false, false),
            "same-run slot rebind must keep the existing thread binding"
        );
        assert!(
            slot_requires_thread_reset_before_reuse(&conn, "agent-a", 0, true, false),
            "cross-run reclaim must reset preserved slot bindings"
        );
        assert!(
            slot_requires_thread_reset_before_reuse(&conn, "agent-a", 0, false, true),
            "different-group same-run reuse must also reset preserved slot bindings"
        );
    }

    #[test]
    fn extract_dependency_numbers_ignores_context_issue_references_in_description() {
        let card = candidate(
            497,
            "medium",
            Some("## 컨텍스트\n관련: #494\n이미 해결한 #493을 참고"),
            None,
        );

        assert_eq!(extract_dependency_numbers(&card), Vec::<i64>::new());
    }

    #[test]
    fn extract_dependency_numbers_parses_explicit_sections_and_json_metadata() {
        let card = candidate(
            497,
            "medium",
            Some("## 선행 작업\n- #494\n- #495\n## 컨텍스트\n관련: #493"),
            Some(r##"{"depends_on":[496,"#497","#498"]}"##),
        );

        let parsed = extract_dependency_parse_result(&card);
        assert_eq!(parsed.numbers, vec![494, 495, 496, 498]);
        assert!(
            parsed
                .signals
                .iter()
                .any(|signal| signal.contains("description:section:## 선행 작업")),
            "section-based dependency extraction should be recorded in signals"
        );
        assert!(
            parsed
                .signals
                .iter()
                .any(|signal| signal == "metadata:json:depends_on"),
            "json-based dependency extraction should be recorded in signals"
        );
    }

    #[test]
    fn extract_dependency_numbers_keeps_section_open_for_issue_ref_lines() {
        let card = candidate(
            497,
            "medium",
            Some("## 선행 작업\n#494\n- #495\n## 컨텍스트\n#493"),
            None,
        );

        let parsed = extract_dependency_parse_result(&card);
        assert_eq!(parsed.numbers, vec![494, 495]);
        assert!(
            parsed
                .signals
                .iter()
                .any(|signal| signal.contains("description:section:## 선행 작업")),
            "issue-ref lines inside dependency sections must remain section-scoped"
        );
    }

    #[test]
    fn extract_dependency_numbers_allows_bare_dependency_lists_in_metadata() {
        let card = candidate(202, "medium", None, Some("#201 #203"));

        assert_eq!(extract_dependency_numbers(&card), vec![201, 203]);
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

    #[test]
    fn build_group_plan_spreads_similarity_only_cards_across_groups() {
        let plan = build_group_plan(&[
            candidate(
                523,
                "high",
                Some("touches src/services/discord/tmux.rs"),
                None,
            ),
            candidate(
                545,
                "medium",
                Some("touches src/services/discord/tmux.rs"),
                None,
            ),
        ]);

        let entry_by_issue: HashMap<i64, (i64, i64)> = plan
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.card_idx as i64,
                    (entry.thread_group, entry.batch_phase),
                )
            })
            .collect();

        assert_eq!(plan.thread_group_count, 2);
        assert_eq!(plan.similarity_edges, 1);
        assert_eq!(entry_by_issue.get(&0).unwrap().0, 0);
        assert_eq!(entry_by_issue.get(&1).unwrap().0, 1);
        assert_eq!(entry_by_issue.get(&0).unwrap().1, 0);
        assert_eq!(entry_by_issue.get(&1).unwrap().1, 1);
    }

    #[test]
    fn build_group_plan_reuses_phases_for_non_conflicting_similarity_chain() {
        let plan = build_group_plan(&[
            candidate(101, "high", Some("touches src/a.rs"), None),
            candidate(102, "medium", Some("touches src/a.rs and src/b.rs"), None),
            candidate(103, "low", Some("touches src/b.rs"), None),
        ]);

        let phases_by_idx: HashMap<usize, i64> = plan
            .entries
            .iter()
            .map(|entry| (entry.card_idx, entry.batch_phase))
            .collect();

        assert_eq!(plan.thread_group_count, 3);
        assert_eq!(phases_by_idx.get(&0).copied(), Some(0));
        assert_eq!(phases_by_idx.get(&1).copied(), Some(1));
        assert_eq!(phases_by_idx.get(&2).copied(), Some(0));
    }

    #[test]
    fn build_group_plan_keeps_dependency_chain_in_one_group() {
        let plan = build_group_plan(&[
            candidate(201, "high", Some("base work"), None),
            candidate(202, "medium", Some("depends on #201"), None),
        ]);

        let entries_by_idx: HashMap<usize, (i64, i64)> = plan
            .entries
            .iter()
            .map(|entry| (entry.card_idx, (entry.thread_group, entry.batch_phase)))
            .collect();

        assert_eq!(plan.thread_group_count, 1);
        assert_eq!(entries_by_idx.get(&0).copied(), Some((0, 0)));
        assert_eq!(entries_by_idx.get(&1).copied(), Some((0, 1)));
    }
}
