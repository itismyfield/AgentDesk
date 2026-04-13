use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
}

#[derive(Debug, Deserialize)]
pub struct UpdateEntryBody {
    pub thread_group: Option<i64>,
    pub priority_rank: Option<i64>,
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
}

#[derive(Debug, Clone)]
struct PlannedEntry {
    card_idx: usize,
    thread_group: i64,
    priority_rank: i64,
    batch_phase: i64,
    reason: String,
}

fn load_run_ids_with_status(
    conn: &rusqlite::Connection,
    statuses: &[&str],
) -> rusqlite::Result<Vec<String>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat("?")
        .take(statuses.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id FROM auto_queue_runs WHERE status IN ({placeholders}) ORDER BY rowid ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(rusqlite::params_from_iter(statuses.iter()), |row| {
        row.get(0)
    })?
    .collect::<Result<Vec<_>, _>>()
}

fn load_slot_bindings_for_runs(
    conn: &rusqlite::Connection,
    run_ids: &[String],
) -> rusqlite::Result<Vec<(String, i64)>> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat("?")
        .take(run_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT DISTINCT agent_id, slot_index
         FROM auto_queue_slots
         WHERE assigned_run_id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(rusqlite::params_from_iter(run_ids.iter()), |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?
    .collect::<Result<Vec<_>, _>>()
}

fn load_live_dispatch_ids_for_runs(
    conn: &rusqlite::Connection,
    run_ids: &[String],
) -> rusqlite::Result<Vec<String>> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat("?")
        .take(run_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT DISTINCT td.id
         FROM task_dispatches td
         JOIN auto_queue_entries e ON e.dispatch_id = td.id
         WHERE e.run_id IN ({placeholders})
           AND td.status IN ('pending', 'dispatched')"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(rusqlite::params_from_iter(run_ids.iter()), |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()
}

fn cancel_live_dispatches_for_runs(
    conn: &rusqlite::Connection,
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
    conn: &rusqlite::Connection,
    run_ids: &[String],
) -> (usize, usize) {
    let mut released_slots: HashSet<(String, i64)> = HashSet::new();
    let mut cleared_sessions = 0usize;

    for (agent_id, slot_index) in load_slot_bindings_for_runs(conn, run_ids).unwrap_or_default() {
        if released_slots.insert((agent_id.clone(), slot_index)) {
            cleared_sessions += crate::services::auto_queue::runtime::clear_slot_threads_for_slot(
                health_registry.clone(),
                conn,
                &agent_id,
                slot_index,
            );
        }
    }

    for run_id in run_ids {
        crate::db::auto_queue::release_run_slots(conn, run_id);
    }

    (released_slots.len(), cleared_sessions)
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
}

impl ActivateCardState {
    fn has_active_dispatch(&self) -> bool {
        self.latest_dispatch_id.is_some()
            && matches!(
                self.latest_dispatch_status.as_deref(),
                Some("pending") | Some("dispatched")
            )
    }
}

fn load_activate_card_state(
    conn: &rusqlite::Connection,
    card_id: &str,
    entry_id: &str,
) -> rusqlite::Result<ActivateCardState> {
    let (status, title, metadata, latest_dispatch_id): (
        String,
        String,
        Option<String>,
        Option<String>,
    ) = conn.query_row(
        "SELECT status, title, metadata, latest_dispatch_id FROM kanban_cards WHERE id = ?1",
        [card_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
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
    })
}

#[derive(Clone)]
pub(crate) struct AutoQueueActivateDeps {
    db: crate::db::Db,
    engine: crate::engine::PolicyEngine,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    guild_id: Option<String>,
}

impl AutoQueueActivateDeps {
    fn from_state(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            engine: state.engine.clone(),
            health_registry: state.health_registry.clone(),
            guild_id: state.config.discord.guild_id.clone(),
        }
    }

    pub(crate) fn for_bridge(db: crate::db::Db, engine: crate::engine::PolicyEngine) -> Self {
        Self {
            db,
            engine,
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
}

enum ActivatePreflightOutcome {
    Continue,
    Dispatched(serde_json::Value),
    Skipped,
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

fn handle_activate_preflight_metadata(
    deps: &AutoQueueActivateDeps,
    entry_id: &str,
    card_id: &str,
    agent_id: &str,
    group: i64,
    title: &str,
    metadata: Option<&str>,
) -> ActivatePreflightOutcome {
    let Some(metadata) = metadata else {
        return ActivatePreflightOutcome::Continue;
    };
    let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return ActivatePreflightOutcome::Continue;
    };
    let log_ctx = AutoQueueLogContext::new()
        .entry(entry_id)
        .card(card_id)
        .agent(agent_id)
        .thread_group(group);

    match parsed.get("preflight_status").and_then(|v| v.as_str()) {
        Some("consult_required") => {
            let consult_agent_id = {
                let conn = deps.db.separate_conn().unwrap();
                let provider = conn
                    .query_row(
                        "SELECT COALESCE(provider, 'claude') FROM agents WHERE id = ?1",
                        [agent_id],
                        |row| row.get::<_, String>(0),
                    )
                    .map(|raw| ProviderKind::from_str_or_unsupported(&raw))
                    .unwrap_or_else(|_| {
                        ProviderKind::default_channel_provider().unwrap_or(ProviderKind::Claude)
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
                    .query_map([agent_id], |row| {
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
                        available_agents
                            .iter()
                            .map(|(_, candidate_provider)| candidate_provider.clone()),
                    )
                    .and_then(|counterpart| {
                        available_agents
                            .iter()
                            .find_map(|(candidate_id, candidate_provider)| {
                                (*candidate_provider == counterpart).then_some(candidate_id.clone())
                            })
                    })
                    .unwrap_or_else(|| agent_id.to_string())
            };

            let dispatch_result = run_activate_blocking(|| {
                crate::dispatch::create_dispatch(
                    &deps.db,
                    &deps.engine,
                    card_id,
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
                crate::auto_queue_log!(
                    warn,
                    "activate_preflight_consultation_dispatch_failed",
                    log_ctx.clone(),
                    "[auto-queue] consultation dispatch failed for entry {entry_id} (group {group})"
                );
                return ActivatePreflightOutcome::Continue;
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

            let conn = deps.db.separate_conn().unwrap();
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
            crate::auto_queue_log!(
                info,
                "activate_preflight_consultation_dispatch_created",
                log_ctx.clone().dispatch(&dispatch_id),
                "[auto-queue] created consultation dispatch for entry {entry_id} (group {group})"
            );
            ActivatePreflightOutcome::Dispatched(deps.entry_json(entry_id))
        }
        Some("invalid") | Some("already_applied") => {
            let conn = deps.db.separate_conn().unwrap();
            conn.execute(
                "UPDATE auto_queue_entries
                 SET status = 'skipped',
                     completed_at = datetime('now')
                 WHERE id = ?1 AND status = 'pending'",
                [entry_id],
            )
            .ok();
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
    conn: &rusqlite::Connection,
    repo: Option<&String>,
    issue_numbers: &[i64],
) -> Result<HashMap<i64, ResolvedDispatchCard>, String> {
    if issue_numbers.is_empty() {
        return Ok(HashMap::new());
    }

    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
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
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
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

fn apply_dispatch_agent_assignments(
    conn: &rusqlite::Connection,
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
                    rusqlite::params![target, card.card_id],
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
    conn: &rusqlite::Connection,
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
    conn: &rusqlite::Connection,
    repo: Option<&str>,
    agent_id: Option<&str>,
) -> Result<Option<String>, String> {
    let mut sql = String::from("SELECT id FROM auto_queue_runs WHERE status = 'active'");
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

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
    sql.push_str(" ORDER BY created_at DESC LIMIT 1");

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    match conn.query_row(&sql, param_refs.as_slice(), |row| row.get::<_, String>(0)) {
        Ok(run_id) => Ok(Some(run_id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(format!("lookup active run: {err}")),
    }
}

fn enqueue_dispatch_entries_into_run(
    conn: &mut rusqlite::Connection,
    run_id: &str,
    requested_entries: &[GenerateEntryBody],
    cards_by_issue: &HashMap<i64, ResolvedDispatchCard>,
) -> Result<usize, String> {
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
    let mut existing_live_cards = existing_live_cards;
    let tx = conn
        .transaction()
        .map_err(|err| format!("begin enqueue transaction: {err}"))?;
    let mut inserted = 0usize;

    for entry in requested_entries {
        let Some(card) = cards_by_issue.get(&entry.issue_number) else {
            continue;
        };
        if existing_live_cards.contains(&card.card_id) {
            continue;
        }

        let thread_group = entry.thread_group.unwrap_or(0);
        let priority_rank = *next_rank_by_group.entry(thread_group).or_insert(0);
        next_rank_by_group.insert(thread_group, priority_rank + 1);

        tx.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, priority_rank, thread_group, batch_phase
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7
             )",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                run_id,
                card.card_id,
                card.assigned_agent_id.as_deref().unwrap_or(""),
                priority_rank,
                thread_group,
                entry.batch_phase.unwrap_or(0)
            ],
        )
        .map_err(|err| format!("insert auto-queue entry: {err}"))?;
        existing_live_cards.insert(card.card_id.clone());
        inserted += 1;
    }

    if inserted > 0 {
        crate::db::auto_queue::sync_run_group_metadata(&tx, run_id)
            .map_err(|err| format!("sync run group metadata: {err}"))?;
    }

    tx.commit()
        .map_err(|err| format!("commit enqueue transaction: {err}"))?;
    Ok(inserted)
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

    let conn = match state.db.separate_conn() {
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
    let mut excluded_count = 0usize;
    for card in &cards {
        let dep_numbers = extract_dependency_numbers(card);
        let has_unresolved_external_dependency = dep_numbers.iter().any(|dep_num| {
            if issue_to_idx.contains_key(dep_num) {
                return false;
            }
            let dep_status: Option<String> = conn
                .query_row(
                    "SELECT status FROM kanban_cards WHERE github_issue_number = ?1",
                    [dep_num],
                    |row| row.get(0),
                )
                .ok();
            dep_status.as_deref() != Some("done")
        });

        if has_unresolved_external_dependency {
            excluded_count += 1;
        } else {
            filtered_cards.push(card.clone());
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

    // Create run
    let run_id = uuid::Uuid::new_v4().to_string();
    let ai_model_str = "smart-planner".to_string();
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
                planned.batch_phase
            ],
        )
        .ok();
        entries.push(
            state
                .auto_queue_service()
                .entry_json(&entry_id, guild_id)
                .unwrap_or(serde_json::Value::Null),
        );
    }

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
    activate_with_deps(&AutoQueueActivateDeps::from_state(&state), body)
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
    let completed_slots = crate::db::auto_queue::completed_group_slots(&conn, &run_id);
    let mut cleared_slots: HashSet<(String, i64)> = HashSet::new();
    for (agent_id, slot_index) in &completed_slots {
        let cleared = crate::services::auto_queue::runtime::clear_slot_threads_for_slot(
            deps.health_registry.clone(),
            &conn,
            agent_id,
            *slot_index,
        );
        if cleared > 0 {
            crate::auto_queue_log!(
                info,
                "activate_release_completed_slot",
                run_log_ctx.clone().agent(agent_id).slot_index(*slot_index),
                "[auto-queue] cleared {cleared} slot thread session(s) before releasing {agent_id} slot {slot_index}"
            );
        }
        cleared_slots.insert((agent_id.clone(), *slot_index));
    }
    crate::db::auto_queue::release_group_slots(&conn, &completed_slots);

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
            crate::auto_queue_log!(
                info,
                "activate_inflight_guard_blocked",
                run_log_ctx.clone().agent(agt),
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
    let mut occupied_agents: HashSet<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT agent_id
                 FROM auto_queue_entries
                 WHERE run_id = ?1 AND status = 'dispatched'",
            )
            .unwrap();
        stmt.query_map([&run_id], |row| row.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|row| row.ok()).collect())
            .unwrap_or_default()
    };

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
                rusqlite::params![run_id, group],
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

        if occupied_agents.contains(&agent_id) {
            crate::auto_queue_log!(
                info,
                "activate_same_agent_guard_blocked",
                entry_log_ctx.clone(),
                "[auto-queue] Skipping group {group} for {agent_id}: agent already dispatched in this activate cycle or run"
            );
            continue;
        }

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
                rusqlite::params![agent_id, card_id, run_id],
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
                &entry_id,
                &card_id,
                &agent_id,
                *group,
                &initial_state.title,
                initial_state.metadata.as_deref(),
            ) {
                ActivatePreflightOutcome::Continue => {}
                ActivatePreflightOutcome::Dispatched(entry_json) => {
                    occupied_agents.insert(agent_id.clone());
                    dispatched_groups_this_activate += 1;
                    dispatched.push(entry_json);
                    continue;
                }
                ActivatePreflightOutcome::Skipped => continue,
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
                if let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(&metadata) {
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
                                crate::dispatch::create_dispatch(
                                    &deps.db,
                                    &deps.engine,
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
                                let conn = deps.db.separate_conn().unwrap();
                                if let Err(error) =
                                    crate::db::auto_queue::update_entry_status_on_conn(
                                        &conn,
                                        &entry_id,
                                        crate::db::auto_queue::ENTRY_STATUS_PENDING,
                                        "activate_consultation_reserve_revert",
                                        &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                                    )
                                {
                                    crate::auto_queue_log!(
                                        warn,
                                        "activate_consultation_reserve_revert_failed",
                                        entry_log_ctx.clone(),
                                        "[auto-queue] failed to revert consultation reservation for entry {}: {}",
                                        entry_id,
                                        error
                                    );
                                }
                                drop(conn);
                                crate::auto_queue_log!(
                                    warn,
                                    "activate_consultation_dispatch_failed",
                                    entry_log_ctx.clone(),
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

                            let conn = deps.db.separate_conn().unwrap();
                            conn.execute(
                                "UPDATE kanban_cards SET metadata = ?1 WHERE id = ?2",
                                rusqlite::params![parsed.to_string(), card_id],
                            )
                            .ok();
                            if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
                                &conn,
                                &entry_id,
                                crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                                "activate_consultation_dispatch",
                                &crate::db::auto_queue::EntryStatusUpdateOptions {
                                    dispatch_id: Some(dispatch_id.clone()),
                                    slot_index: None,
                                },
                            ) {
                                crate::auto_queue_log!(
                                    warn,
                                    "activate_consultation_mark_dispatched_failed",
                                    entry_log_ctx.clone().dispatch(&dispatch_id),
                                    "[auto-queue] failed to mark consultation entry {} dispatched: {}",
                                    entry_id,
                                    error
                                );
                            }
                            occupied_agents.insert(agent_id.clone());
                            dispatched_groups_this_activate += 1;
                            dispatched.push(deps.entry_json(&entry_id));
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
                occupied_agents.insert(agent_id.clone());
                dispatched_groups_this_activate += 1;
                dispatched.push(deps.entry_json(&entry_id));
            }
            continue;
        }

        if post_walk.status == "done" {
            let conn = deps.db.separate_conn().unwrap();
            conn.execute(
                "UPDATE auto_queue_entries
                 SET status = 'skipped',
                     completed_at = COALESCE(completed_at, datetime('now'))
                 WHERE id = ?1 AND status = 'pending'",
                [&entry_id],
            )
            .ok();
            drop(conn);
            continue;
        }

        if post_walk.has_active_dispatch() {
            let dispatch_id = post_walk
                .latest_dispatch_id
                .as_ref()
                .expect("active dispatch state requires dispatch id");
            let conn = deps.db.separate_conn().unwrap();
            conn.execute(
                "UPDATE auto_queue_entries
                 SET status = 'dispatched',
                     dispatch_id = ?1,
                     dispatched_at = COALESCE(dispatched_at, datetime('now'))
                 WHERE id = ?2 AND status = 'pending'",
                rusqlite::params![dispatch_id, entry_id],
            )
            .ok();
            drop(conn);
            occupied_agents.insert(agent_id.clone());
            dispatched_groups_this_activate += 1;
            dispatched.push(deps.entry_json(&entry_id));
            continue;
        }

        match handle_activate_preflight_metadata(
            deps,
            &entry_id,
            &card_id,
            &agent_id,
            *group,
            &post_walk.title,
            post_walk.metadata.as_deref(),
        ) {
            ActivatePreflightOutcome::Continue => {}
            ActivatePreflightOutcome::Dispatched(entry_json) => {
                occupied_agents.insert(agent_id.clone());
                dispatched_groups_this_activate += 1;
                dispatched.push(entry_json);
                continue;
            }
            ActivatePreflightOutcome::Skipped => continue,
        }

        // Create dispatch
        let conn = deps.db.separate_conn().unwrap();
        let slot_allocation =
            crate::db::auto_queue::allocate_slot_for_group_agent(&conn, &run_id, *group, &agent_id);
        let slot_index = slot_allocation.as_ref().map(|(slot_index, _)| *slot_index);
        if slot_allocation.is_none() {
            crate::auto_queue_log!(
                info,
                "activate_slot_pool_exhausted",
                entry_log_ctx.clone(),
                "[auto-queue] Skipping group {group} for {agent_id}: no free slot in pool"
            );
            continue;
        }
        if let Some((assigned_slot, _newly_assigned)) = slot_allocation {
            let slot_key = (agent_id.clone(), assigned_slot);
            if !cleared_slots.contains(&slot_key) {
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
        }

        let conn = deps.db.separate_conn().unwrap();
        if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
            &conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
            "activate_dispatch_reserve",
            &crate::db::auto_queue::EntryStatusUpdateOptions {
                dispatch_id: None,
                slot_index,
            },
        ) {
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
        drop(conn);

        let dispatch_result = run_activate_blocking(|| {
            crate::dispatch::create_dispatch(
                &deps.db,
                &deps.engine,
                &card_id,
                &agent_id,
                "implementation",
                &post_walk.title,
                &json!({
                    "auto_queue": true,
                    "entry_id": entry_id,
                    "thread_group": group,
                    "slot_index": slot_index,
                }),
            )
        });

        if dispatch_result.is_err() {
            let conn = deps.db.separate_conn().unwrap();
            if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
                &conn,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_PENDING,
                "activate_dispatch_reserve_revert",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            ) {
                crate::auto_queue_log!(
                    warn,
                    "activate_dispatch_reserve_revert_failed",
                    entry_log_ctx.clone().maybe_slot_index(slot_index),
                    "[auto-queue] failed to revert reservation for entry {} after create_dispatch error: {}",
                    entry_id,
                    error
                );
            }
            drop(conn);
            crate::auto_queue_log!(
                error,
                "activate_dispatch_create_failed",
                entry_log_ctx.clone().maybe_slot_index(slot_index),
                "[auto-queue] create_dispatch failed for entry {entry_id} (group {group}), leaving as pending for retry"
            );
            continue;
        }

        // Mark entry with dispatch_id (#145)
        let dispatch_id = dispatch_result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap_or("")
            .to_string();
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

        occupied_agents.insert(agent_id.clone());
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
        crate::db::auto_queue::release_run_slots(&conn, &run_id);
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

    let existing_active_run_id =
        match find_matching_active_run_id(&conn, body.repo.as_deref(), body.agent_id.as_deref()) {
            Ok(run_id) => run_id,
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": err})),
                );
            }
        };
    drop(conn);

    if let Some(run_id) = existing_active_run_id {
        let prepare_result = state.auto_queue_service().prepare_generate_cards(
            &crate::services::auto_queue::PrepareGenerateInput {
                repo: body.repo.clone(),
                agent_id: body.agent_id.clone(),
                issue_numbers: Some(issue_numbers.clone()),
            },
        );
        if let Err(error) = prepare_result {
            return error.into_json_response();
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
        if let Err(err) = enqueue_dispatch_entries_into_run(
            &mut conn,
            &run_id,
            &requested_entries,
            &cards_by_issue,
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": err})),
            );
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
                    active_only: Some(true),
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

        return (StatusCode::OK, Json(snapshot));
    }

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
            rusqlite::params![thread_group, *priority_rank, run_id, card.card_id],
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
    match state
        .auto_queue_service()
        .status(crate::services::auto_queue::StatusInput {
            repo: query.repo,
            agent_id: query.agent_id,
            guild_id: state.config.discord.guild_id.clone(),
        }) {
        Ok(response) => (StatusCode::OK, Json(json!(response))),
        Err(error) => error.into_json_response(),
    }
}

/// PATCH /api/auto-queue/entries/{id}
pub async fn update_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateEntryBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.thread_group.is_none() && body.priority_rank.is_none() {
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
    if status != "pending" {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "only pending entries can be updated"})),
        );
    }

    let changed = conn
        .execute(
            "UPDATE auto_queue_entries
             SET thread_group = COALESCE(?1, thread_group),
                 priority_rank = COALESCE(?2, priority_rank)
             WHERE id = ?3
               AND status = 'pending'",
            rusqlite::params![body.thread_group, body.priority_rank, id],
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
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    let active_run_ids = load_run_ids_with_status(&conn, &["active"]).unwrap_or_default();
    let cancelled_dispatches =
        cancel_live_dispatches_for_runs(&conn, &active_run_ids, "auto_queue_pause");
    let (released_slots, cleared_slot_sessions) =
        clear_and_release_slots_for_runs(state.health_registry.clone(), &conn, &active_run_ids);
    let paused = conn
        .execute(
            "UPDATE auto_queue_runs
             SET status = 'paused',
                 completed_at = NULL
             WHERE status = 'active'",
            [],
        )
        .unwrap_or(0);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "paused_runs": paused,
            "cancelled_dispatches": cancelled_dispatches,
            "released_slots": released_slots,
            "cleared_slot_sessions": cleared_slot_sessions,
        })),
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
    let blocked_runs: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM auto_queue_runs r
             WHERE r.status = 'paused'
               AND EXISTS (
                   SELECT 1
                   FROM kv_meta km
                   WHERE km.key LIKE 'aq_phase_gate:' || r.id || ':%'
                     AND json_extract(COALESCE(km.value, '{}'), '$.status') IN ('pending', 'failed')
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
                   FROM kv_meta km
                   WHERE km.key LIKE 'aq_phase_gate:' || auto_queue_runs.id || ':%'
                     AND json_extract(COALESCE(km.value, '{}'), '$.status') IN ('pending', 'failed')
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
    let target_run_ids = load_run_ids_with_status(&conn, &["active", "paused"]).unwrap_or_default();
    let cancelled_dispatches =
        cancel_live_dispatches_for_runs(&conn, &target_run_ids, "auto_queue_cancel");
    let (released_slots, cleared_slot_sessions) =
        clear_and_release_slots_for_runs(state.health_registry.clone(), &conn, &target_run_ids);
    let cancelled_runs = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'cancelled', completed_at = datetime('now') WHERE status IN ('active', 'paused')",
            [],
        )
        .unwrap_or(0);
    let entry_ids: Vec<String> = if target_run_ids.is_empty() {
        Vec::new()
    } else {
        let placeholders = std::iter::repeat("?")
            .take(target_run_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id FROM auto_queue_entries
             WHERE run_id IN ({placeholders})
               AND status IN ('pending', 'dispatched')"
        );
        conn.prepare(&sql)
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params_from_iter(target_run_ids.iter()), |row| {
                    row.get::<_, String>(0)
                })
                .ok()
                .map(|rows| rows.filter_map(|row| row.ok()).collect())
            })
            .unwrap_or_default()
    };
    let mut cancelled_entries = 0usize;
    for entry_id in entry_ids {
        match crate::db::auto_queue::update_entry_status_on_conn(
            &conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "run_cancel",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        ) {
            Ok(result) if result.changed => cancelled_entries += 1,
            Ok(_) => {}
            Err(error) => crate::auto_queue_log!(
                warn,
                "cancel_entry_skip_failed",
                AutoQueueLogContext::new().entry(&entry_id),
                "[auto-queue] failed to cancel entry {} during run cancel: {}",
                entry_id,
                error
            ),
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "cancelled_entries": cancelled_entries,
            "cancelled_runs": cancelled_runs,
            "cancelled_dispatches": cancelled_dispatches,
            "released_slots": released_slots,
            "cleared_slot_sessions": cleared_slot_sessions,
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
        crate::auto_queue_log!(
            warn,
            "submit_order_no_ready_cards",
            run_log_ctx.clone(),
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
    use super::{GenerateCandidate, QueueEntryOrder, build_group_plan, reorder_entry_ids};
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
