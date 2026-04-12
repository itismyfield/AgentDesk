use rusqlite::{Connection, OptionalExtension, types::ToSql};
use thiserror::Error;

pub const ENTRY_STATUS_PENDING: &str = "pending";
pub const ENTRY_STATUS_DISPATCHED: &str = "dispatched";
pub const ENTRY_STATUS_DONE: &str = "done";
pub const ENTRY_STATUS_SKIPPED: &str = "skipped";

#[derive(Debug, Clone, Default)]
pub struct EntryStatusUpdateOptions {
    pub dispatch_id: Option<String>,
    pub slot_index: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct EntryStatusUpdateResult {
    pub run_id: String,
    pub from_status: String,
    pub to_status: String,
    pub changed: bool,
}

#[derive(Debug, Error)]
pub enum EntryStatusUpdateError {
    #[error("auto-queue entry not found: {entry_id}")]
    NotFound { entry_id: String },
    #[error("unsupported auto-queue entry status: {status}")]
    UnsupportedStatus { status: String },
    #[error("invalid auto-queue entry transition for {entry_id}: {from_status} -> {to_status}")]
    InvalidTransition {
        entry_id: String,
        from_status: String,
        to_status: String,
    },
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

#[derive(Debug, Clone)]
struct EntryStatusRow {
    run_id: String,
    status: String,
    dispatch_id: Option<String>,
    slot_index: Option<i64>,
    completed_at: Option<String>,
}

pub fn update_entry_status_on_conn(
    conn: &Connection,
    entry_id: &str,
    new_status: &str,
    trigger_source: &str,
    options: &EntryStatusUpdateOptions,
) -> Result<EntryStatusUpdateResult, EntryStatusUpdateError> {
    let current = load_entry_status_row(conn, entry_id)?;
    let normalized = normalize_entry_status(new_status)?;

    if !is_allowed_entry_transition(&current.status, normalized) {
        tracing::warn!(
            "[auto-queue] blocked invalid entry transition {} {} -> {} (source: {})",
            entry_id,
            current.status,
            normalized,
            trigger_source
        );
        return Err(EntryStatusUpdateError::InvalidTransition {
            entry_id: entry_id.to_string(),
            from_status: current.status,
            to_status: normalized.to_string(),
        });
    }

    let effective_dispatch_id = options
        .dispatch_id
        .clone()
        .or_else(|| current.dispatch_id.clone());
    let effective_slot_index = options.slot_index.or(current.slot_index);
    let metadata_change = match normalized {
        ENTRY_STATUS_PENDING => {
            current.dispatch_id.is_some()
                || current.slot_index.is_some()
                || current.completed_at.is_some()
        }
        ENTRY_STATUS_DISPATCHED => {
            effective_dispatch_id != current.dispatch_id
                || effective_slot_index != current.slot_index
                || current.completed_at.is_some()
        }
        ENTRY_STATUS_DONE | ENTRY_STATUS_SKIPPED => false,
        _ => false,
    };
    let changed = current.status != normalized || metadata_change;

    if !changed {
        return Ok(EntryStatusUpdateResult {
            run_id: current.run_id,
            from_status: current.status,
            to_status: normalized.to_string(),
            changed: false,
        });
    }

    match normalized {
        ENTRY_STATUS_PENDING => {
            conn.execute(
                "UPDATE auto_queue_entries
                 SET status = 'pending',
                     dispatch_id = NULL,
                     slot_index = NULL,
                     dispatched_at = NULL,
                     completed_at = NULL
                 WHERE id = ?1",
                [entry_id],
            )?;
        }
        ENTRY_STATUS_DISPATCHED => {
            conn.execute(
                "UPDATE auto_queue_entries
                 SET status = 'dispatched',
                     dispatch_id = ?1,
                     slot_index = ?2,
                     dispatched_at = datetime('now'),
                     completed_at = NULL
                 WHERE id = ?3",
                rusqlite::params![effective_dispatch_id, effective_slot_index, entry_id],
            )?;
        }
        ENTRY_STATUS_DONE => {
            conn.execute(
                "UPDATE auto_queue_entries
                 SET status = 'done',
                     completed_at = datetime('now')
                 WHERE id = ?1",
                [entry_id],
            )?;
        }
        ENTRY_STATUS_SKIPPED => {
            conn.execute(
                "UPDATE auto_queue_entries
                 SET status = 'skipped',
                     dispatch_id = NULL,
                     dispatched_at = NULL,
                     completed_at = datetime('now')
                 WHERE id = ?1",
                [entry_id],
            )?;
        }
        _ => unreachable!(),
    }

    if normalized == ENTRY_STATUS_DISPATCHED {
        if let Some(previous_dispatch_id) = current
            .dispatch_id
            .as_deref()
            .filter(|value| Some(*value) != effective_dispatch_id.as_deref())
        {
            record_entry_dispatch_history_on_conn(
                conn,
                entry_id,
                previous_dispatch_id,
                trigger_source,
            )?;
        }
        if let Some(dispatch_id) = effective_dispatch_id.as_deref() {
            record_entry_dispatch_history_on_conn(conn, entry_id, dispatch_id, trigger_source)?;
        }
    }

    record_entry_transition_on_conn(conn, entry_id, &current.status, normalized, trigger_source)?;

    if matches!(normalized, ENTRY_STATUS_DONE | ENTRY_STATUS_SKIPPED) {
        release_completed_group_slots_for_run(conn, &current.run_id);
        maybe_finalize_run_after_terminal_entry(conn, &current.run_id, normalized)?;
    }

    Ok(EntryStatusUpdateResult {
        run_id: current.run_id,
        from_status: current.status,
        to_status: normalized.to_string(),
        changed: true,
    })
}

fn record_entry_dispatch_history_on_conn(
    conn: &Connection,
    entry_id: &str,
    dispatch_id: &str,
    trigger_source: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO auto_queue_entry_dispatch_history (
            entry_id, dispatch_id, trigger_source
        ) VALUES (?1, ?2, ?3)",
        rusqlite::params![entry_id, dispatch_id, trigger_source],
    )?;
    Ok(())
}

pub fn list_entry_dispatch_history(
    conn: &Connection,
    entry_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT dispatch_id
         FROM auto_queue_entry_dispatch_history
         WHERE entry_id = ?1
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([entry_id], |row| row.get::<_, String>(0))?;
    rows.collect()
}

#[derive(Debug, Clone, Default)]
pub struct GenerateCardFilter {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub issue_numbers: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Default)]
pub struct StatusFilter {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BacklogCardRecord {
    pub card_id: String,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GenerateCandidateRecord {
    pub card_id: String,
    pub agent_id: String,
    pub priority: String,
    pub description: Option<String>,
    pub metadata: Option<String>,
    pub github_issue_number: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct AutoQueueRunRecord {
    pub id: String,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub status: String,
    pub timeout_minutes: i64,
    pub ai_model: Option<String>,
    pub ai_rationale: Option<String>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub max_concurrent_threads: i64,
    pub thread_group_count: i64,
}

#[derive(Debug, Clone)]
pub struct StatusEntryRecord {
    pub id: String,
    pub agent_id: String,
    pub card_id: String,
    pub priority_rank: i64,
    pub reason: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub card_title: Option<String>,
    pub github_issue_number: Option<i64>,
    pub github_repo: Option<String>,
    pub thread_group: i64,
    pub slot_index: Option<i64>,
    pub batch_phase: i64,
    pub channel_thread_map: Option<String>,
    pub active_thread_id: Option<String>,
    pub card_status: Option<String>,
    pub review_round: i64,
}

pub fn find_latest_run_id(
    conn: &Connection,
    filter: &StatusFilter,
) -> rusqlite::Result<Option<String>> {
    let mut run_filter = "1=1".to_string();
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    if let Some(repo) = filter.repo.as_ref() {
        params.push(Box::new(repo.clone()));
        run_filter.push_str(&format!(
            " AND (repo = ?{} OR repo IS NULL OR repo = '')",
            params.len()
        ));
    }
    if let Some(agent_id) = filter.agent_id.as_ref() {
        params.push(Box::new(agent_id.clone()));
        run_filter.push_str(&format!(
            " AND (agent_id = ?{} OR agent_id IS NULL OR agent_id = '')",
            params.len()
        ));
    }

    let param_refs: Vec<&dyn ToSql> = params.iter().map(|value| value.as_ref()).collect();
    conn.query_row(
        &format!(
            "SELECT id FROM auto_queue_runs WHERE {run_filter} ORDER BY created_at DESC LIMIT 1"
        ),
        param_refs.as_slice(),
        |row| row.get(0),
    )
    .optional()
}

pub fn get_run(conn: &Connection, run_id: &str) -> rusqlite::Result<Option<AutoQueueRunRecord>> {
    conn.query_row(
        "SELECT id, repo, agent_id, status, timeout_minutes,
                ai_model, ai_rationale,
                CAST(strftime('%s', created_at) AS INTEGER) * 1000,
                CASE WHEN completed_at IS NOT NULL THEN CAST(strftime('%s', completed_at) AS INTEGER) * 1000 END,
                COALESCE(max_concurrent_threads, 1),
                COALESCE(thread_group_count, 1)
         FROM auto_queue_runs
         WHERE id = ?1",
        [run_id],
        |row| {
            Ok(AutoQueueRunRecord {
                id: row.get(0)?,
                repo: row.get(1)?,
                agent_id: row.get(2)?,
                status: row.get(3)?,
                timeout_minutes: row.get(4)?,
                ai_model: row.get(5)?,
                ai_rationale: row.get(6)?,
                created_at: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                completed_at: row.get(8)?,
                max_concurrent_threads: row.get(9)?,
                thread_group_count: row.get(10)?,
            })
        },
    )
    .optional()
}

pub fn get_status_entry(
    conn: &Connection,
    entry_id: &str,
) -> rusqlite::Result<Option<StatusEntryRecord>> {
    conn.query_row(
        "SELECT e.id, e.agent_id, e.kanban_card_id, e.priority_rank, e.reason, e.status,
                CAST(strftime('%s', e.created_at) AS INTEGER) * 1000,
                CASE WHEN e.dispatched_at IS NOT NULL THEN CAST(strftime('%s', e.dispatched_at) AS INTEGER) * 1000 END,
                CASE WHEN e.completed_at IS NOT NULL THEN CAST(strftime('%s', e.completed_at) AS INTEGER) * 1000 END,
                kc.title, kc.github_issue_number, kc.github_issue_url,
                COALESCE(e.thread_group, 0), e.slot_index, COALESCE(e.batch_phase, 0),
                kc.channel_thread_map, kc.active_thread_id,
                kc.status, COALESCE(crs.review_round, kc.review_round, 0)
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id
         LEFT JOIN card_review_state crs ON e.kanban_card_id = crs.card_id
         WHERE e.id = ?1",
        [entry_id],
        map_status_entry_row,
    )
    .optional()
}

pub fn list_status_entries(
    conn: &Connection,
    run_id: &str,
    filter: &StatusFilter,
) -> rusqlite::Result<Vec<StatusEntryRecord>> {
    let mut sql = String::from(
        "SELECT e.id, e.agent_id, e.kanban_card_id, e.priority_rank, e.reason, e.status,
                CAST(strftime('%s', e.created_at) AS INTEGER) * 1000,
                CASE WHEN e.dispatched_at IS NOT NULL THEN CAST(strftime('%s', e.dispatched_at) AS INTEGER) * 1000 END,
                CASE WHEN e.completed_at IS NOT NULL THEN CAST(strftime('%s', e.completed_at) AS INTEGER) * 1000 END,
                kc.title, kc.github_issue_number, kc.github_issue_url,
                COALESCE(e.thread_group, 0), e.slot_index, COALESCE(e.batch_phase, 0),
                kc.channel_thread_map, kc.active_thread_id,
                kc.status, COALESCE(crs.review_round, kc.review_round, 0)
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id
         LEFT JOIN card_review_state crs ON e.kanban_card_id = crs.card_id
         WHERE e.run_id = ?1",
    );
    let mut params: Vec<Box<dyn ToSql>> = vec![Box::new(run_id.to_string())];

    if let Some(agent_id) = filter.agent_id.as_ref().filter(|value| !value.is_empty()) {
        params.push(Box::new(agent_id.clone()));
        sql.push_str(&format!(" AND e.agent_id = ?{}", params.len()));
    }
    if let Some(repo) = filter.repo.as_ref().filter(|value| !value.is_empty()) {
        params.push(Box::new(repo.clone()));
        sql.push_str(&format!(" AND kc.repo_id = ?{}", params.len()));
    }

    sql.push_str(" ORDER BY e.priority_rank ASC");

    let param_refs: Vec<&dyn ToSql> = params.iter().map(|value| value.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), map_status_entry_row)?;
    rows.collect()
}

pub fn list_backlog_cards(
    conn: &Connection,
    filter: &GenerateCardFilter,
) -> rusqlite::Result<Vec<BacklogCardRecord>> {
    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();
    append_card_filters("kc", filter, &mut conditions, &mut params);
    conditions.push("kc.status = 'backlog'".to_string());

    let sql = format!(
        "SELECT kc.id, kc.repo_id, kc.assigned_agent_id
         FROM kanban_cards kc
         WHERE {}",
        conditions.join(" AND ")
    );
    let param_refs: Vec<&dyn ToSql> = params.iter().map(|value| value.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(BacklogCardRecord {
            card_id: row.get(0)?,
            repo_id: row.get(1)?,
            assigned_agent_id: row.get(2)?,
        })
    })?;
    rows.collect()
}

pub fn list_generate_candidates(
    conn: &Connection,
    filter: &GenerateCardFilter,
    enqueueable_states: &[String],
) -> rusqlite::Result<Vec<GenerateCandidateRecord>> {
    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    let state_start = params.len() + 1;
    let state_placeholders = enqueueable_states
        .iter()
        .enumerate()
        .map(|(idx, _)| format!("?{}", state_start + idx))
        .collect::<Vec<_>>()
        .join(",");
    for state in enqueueable_states {
        params.push(Box::new(state.clone()));
    }
    conditions.push(format!("kc.status IN ({state_placeholders})"));
    append_card_filters("kc", filter, &mut conditions, &mut params);

    let sql = format!(
        "SELECT kc.id, kc.assigned_agent_id, kc.priority, kc.description, kc.metadata, kc.github_issue_number
         FROM kanban_cards kc
         WHERE {}
         ORDER BY
           CASE kc.priority
             WHEN 'urgent' THEN 0
             WHEN 'high' THEN 1
             WHEN 'medium' THEN 2
             WHEN 'low' THEN 3
             ELSE 4
           END,
           kc.created_at ASC",
        conditions.join(" AND ")
    );
    let param_refs: Vec<&dyn ToSql> = params.iter().map(|value| value.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(GenerateCandidateRecord {
            card_id: row.get::<_, String>(0)?,
            agent_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            priority: row
                .get::<_, Option<String>>(2)?
                .unwrap_or_else(|| "medium".to_string()),
            description: row.get::<_, Option<String>>(3)?,
            metadata: row.get::<_, Option<String>>(4)?,
            github_issue_number: row.get::<_, Option<i64>>(5)?,
        })
    })?;
    rows.collect()
}

pub fn count_cards_by_status(
    conn: &Connection,
    repo: Option<&str>,
    agent_id: Option<&str>,
    status: &str,
) -> rusqlite::Result<i64> {
    let mut sql = "SELECT COUNT(*) FROM kanban_cards WHERE status = ?1".to_string();
    let mut params: Vec<Box<dyn ToSql>> = vec![Box::new(status.to_string())];

    if let Some(repo) = repo.filter(|value| !value.is_empty()) {
        params.push(Box::new(repo.to_string()));
        sql.push_str(&format!(" AND repo_id = ?{}", params.len()));
    }
    if let Some(agent_id) = agent_id.filter(|value| !value.is_empty()) {
        params.push(Box::new(agent_id.to_string()));
        sql.push_str(&format!(" AND assigned_agent_id = ?{}", params.len()));
    }

    let param_refs: Vec<&dyn ToSql> = params.iter().map(|value| value.as_ref()).collect();
    conn.query_row(&sql, param_refs.as_slice(), |row| row.get(0))
}

pub fn run_slot_pool_size(conn: &Connection, run_id: &str) -> i64 {
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

pub fn ensure_agent_slot_pool_rows(
    conn: &Connection,
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

pub fn clear_inactive_slot_assignments(conn: &Connection) {
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

pub fn completed_group_slots(conn: &Connection, run_id: &str) -> Vec<(String, i64)> {
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

pub fn release_group_slots(conn: &Connection, slots: &[(String, i64)]) {
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

pub fn release_run_slots(conn: &Connection, run_id: &str) {
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

pub fn current_batch_phase(conn: &Connection, run_id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT MIN(COALESCE(batch_phase, 0))
         FROM auto_queue_entries
         WHERE run_id = ?1
           AND status IN ('pending', 'dispatched')",
        [run_id],
        |row| row.get::<_, Option<i64>>(0),
    )
    .ok()
    .flatten()
}

pub fn batch_phase_is_eligible(batch_phase: i64, current_phase: Option<i64>) -> bool {
    match current_phase {
        Some(phase) => batch_phase == phase,
        None => true,
    }
}

pub fn run_has_blocking_phase_gate(conn: &Connection, run_id: &str) -> bool {
    let key_pattern = format!("aq_phase_gate:{run_id}:%");
    conn.query_row(
        "SELECT COUNT(*) > 0
         FROM kv_meta
         WHERE key LIKE ?1
           AND json_extract(COALESCE(value, '{}'), '$.status') IN ('pending', 'failed')",
        [key_pattern],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

pub fn group_has_pending_entries(
    conn: &Connection,
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

pub fn first_pending_entry_for_group(
    conn: &Connection,
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

pub fn assigned_groups_with_pending_entries(
    conn: &Connection,
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
    let mut seen = std::collections::HashSet::new();
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

pub fn allocate_slot_for_group_agent(
    conn: &Connection,
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

pub fn slot_has_active_dispatch(conn: &Connection, agent_id: &str, slot_index: i64) -> bool {
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

pub fn sync_run_group_metadata(conn: &Connection, run_id: &str) -> rusqlite::Result<()> {
    let thread_group_count: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT COALESCE(thread_group, 0))
             FROM auto_queue_entries
             WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap_or(0)
        .max(1);

    conn.execute(
        "UPDATE auto_queue_runs
         SET thread_group_count = ?1,
             max_concurrent_threads = ?1
         WHERE id = ?2",
        rusqlite::params![thread_group_count, run_id],
    )?;
    Ok(())
}

fn load_entry_status_row(
    conn: &Connection,
    entry_id: &str,
) -> Result<EntryStatusRow, EntryStatusUpdateError> {
    conn.query_row(
        "SELECT run_id, status, dispatch_id, slot_index, completed_at
         FROM auto_queue_entries
         WHERE id = ?1",
        [entry_id],
        |row| {
            Ok(EntryStatusRow {
                run_id: row.get(0)?,
                status: row.get(1)?,
                dispatch_id: row.get(2)?,
                slot_index: row.get(3)?,
                completed_at: row.get(4)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| EntryStatusUpdateError::NotFound {
        entry_id: entry_id.to_string(),
    })
}

fn normalize_entry_status(status: &str) -> Result<&str, EntryStatusUpdateError> {
    match status.trim() {
        ENTRY_STATUS_PENDING => Ok(ENTRY_STATUS_PENDING),
        ENTRY_STATUS_DISPATCHED => Ok(ENTRY_STATUS_DISPATCHED),
        ENTRY_STATUS_DONE => Ok(ENTRY_STATUS_DONE),
        ENTRY_STATUS_SKIPPED => Ok(ENTRY_STATUS_SKIPPED),
        other => Err(EntryStatusUpdateError::UnsupportedStatus {
            status: other.to_string(),
        }),
    }
}

fn is_allowed_entry_transition(from_status: &str, to_status: &str) -> bool {
    if from_status == to_status {
        return true;
    }

    matches!(
        (from_status, to_status),
        (ENTRY_STATUS_PENDING, ENTRY_STATUS_DISPATCHED)
            | (ENTRY_STATUS_PENDING, ENTRY_STATUS_DONE)
            | (ENTRY_STATUS_PENDING, ENTRY_STATUS_SKIPPED)
            | (ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_PENDING)
            | (ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_DONE)
            | (ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_SKIPPED)
            | (ENTRY_STATUS_DONE, ENTRY_STATUS_DISPATCHED)
    )
}

fn release_completed_group_slots_for_run(conn: &Connection, run_id: &str) {
    let completed_slots = completed_group_slots(conn, run_id);
    if !completed_slots.is_empty() {
        release_group_slots(conn, &completed_slots);
    }
}

fn maybe_finalize_run_after_terminal_entry(
    conn: &Connection,
    run_id: &str,
    new_status: &str,
) -> rusqlite::Result<bool> {
    if new_status == ENTRY_STATUS_DONE && distinct_batch_phase_count(conn, run_id) > 1 {
        return Ok(false);
    }
    if run_has_blocking_phase_gate(conn, run_id) {
        return Ok(false);
    }

    let remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM auto_queue_entries
         WHERE run_id = ?1 AND status IN ('pending', 'dispatched')",
        [run_id],
        |row| row.get(0),
    )?;
    if remaining > 0 {
        return Ok(false);
    }

    release_run_slots(conn, run_id);
    complete_run_on_conn(conn, run_id)
}

pub fn complete_run_on_conn(conn: &Connection, run_id: &str) -> rusqlite::Result<bool> {
    let updated = conn.execute(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = datetime('now')
         WHERE id = ?1 AND status IN ('active', 'paused', 'generated', 'pending')",
        [run_id],
    )?;
    if updated == 0 {
        return Ok(false);
    }

    if let Err(error) = queue_run_completion_notify_on_conn(conn, run_id) {
        tracing::warn!(
            "[auto-queue] failed to queue completion notify for run {}: {}",
            run_id,
            error
        );
    }

    Ok(true)
}

fn queue_run_completion_notify_on_conn(conn: &Connection, run_id: &str) -> rusqlite::Result<()> {
    let (repo, agent_id): (Option<String>, Option<String>) = conn.query_row(
        "SELECT repo, agent_id FROM auto_queue_runs WHERE id = ?1",
        [run_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let targets = completion_notify_targets_on_conn(conn, run_id, agent_id.as_deref());
    if targets.is_empty() {
        return Ok(());
    }

    let entry_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let repo_label = repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("(global)");
    let short_run_id = &run_id[..8.min(run_id.len())];
    let content = format!("자동큐 완료: {repo_label} / run {short_run_id} / {entry_count}개");

    for channel_id in targets {
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, 'notify', 'system')",
            rusqlite::params![format!("channel:{channel_id}"), &content],
        )?;
    }

    Ok(())
}

fn completion_notify_targets_on_conn(
    conn: &Connection,
    run_id: &str,
    run_agent_id: Option<&str>,
) -> Vec<String> {
    let mut targets = Vec::new();

    if let Some(agent_id) = run_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && let Ok(channel_id) = conn.query_row(
            "SELECT discord_channel_id FROM agents WHERE id = ?1",
            [agent_id],
            |row| row.get::<_, Option<String>>(0),
        )
        && let Some(channel_id) = channel_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    {
        targets.push(channel_id);
    }

    if targets.is_empty()
        && let Ok(mut stmt) = conn.prepare(
            "SELECT DISTINCT a.discord_channel_id
             FROM auto_queue_entries e
             JOIN agents a ON a.id = e.agent_id
             WHERE e.run_id = ?1
               AND a.discord_channel_id IS NOT NULL
               AND TRIM(a.discord_channel_id) != ''",
        )
        && let Ok(rows) = stmt.query_map([run_id], |row| row.get::<_, String>(0))
    {
        targets.extend(rows.filter_map(|row| row.ok()));
    }

    targets.sort();
    targets.dedup();
    targets
}

fn distinct_batch_phase_count(conn: &Connection, run_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(DISTINCT COALESCE(batch_phase, 0))
         FROM auto_queue_entries
         WHERE run_id = ?1",
        [run_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

fn record_entry_transition_on_conn(
    conn: &Connection,
    entry_id: &str,
    from_status: &str,
    to_status: &str,
    trigger_source: &str,
) -> rusqlite::Result<()> {
    ensure_entry_transition_audit_schema(conn)?;
    conn.execute(
        "INSERT INTO auto_queue_entry_transitions (
             entry_id,
             from_status,
             to_status,
             trigger_source
         )
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![entry_id, from_status, to_status, trigger_source],
    )?;
    Ok(())
}

fn ensure_entry_transition_audit_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS auto_queue_entry_transitions (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id       TEXT NOT NULL,
            from_status    TEXT,
            to_status      TEXT NOT NULL,
            trigger_source TEXT NOT NULL,
            created_at     DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_aq_entry_transitions_entry
            ON auto_queue_entry_transitions(entry_id);
        CREATE INDEX IF NOT EXISTS idx_aq_entry_transitions_created
            ON auto_queue_entry_transitions(created_at);",
    )
}

fn map_status_entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StatusEntryRecord> {
    Ok(StatusEntryRecord {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        card_id: row.get(2)?,
        priority_rank: row.get(3)?,
        reason: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
        dispatched_at: row.get(7)?,
        completed_at: row.get(8)?,
        card_title: row.get(9)?,
        github_issue_number: row.get(10)?,
        github_repo: row.get(11)?,
        thread_group: row.get(12)?,
        slot_index: row.get(13)?,
        batch_phase: row.get(14)?,
        channel_thread_map: row.get(15)?,
        active_thread_id: row.get(16)?,
        card_status: row.get(17)?,
        review_round: row.get::<_, Option<i64>>(18)?.unwrap_or(0),
    })
}

fn ensure_agent_slot_rows(conn: &Connection, run_id: &str, agent_id: &str) -> rusqlite::Result<()> {
    ensure_agent_slot_pool_rows(conn, agent_id, run_slot_pool_size(conn, run_id))
}

fn append_card_filters(
    alias: &str,
    filter: &GenerateCardFilter,
    conditions: &mut Vec<String>,
    params: &mut Vec<Box<dyn ToSql>>,
) {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };

    if let Some(repo) = filter.repo.as_ref() {
        params.push(Box::new(repo.clone()));
        conditions.push(format!("{}repo_id = ?{}", prefix, params.len()));
    }
    if let Some(agent_id) = filter.agent_id.as_ref() {
        params.push(Box::new(agent_id.clone()));
        conditions.push(format!("{}assigned_agent_id = ?{}", prefix, params.len()));
    }
    if let Some(issue_numbers) = filter
        .issue_numbers
        .as_ref()
        .filter(|nums| !nums.is_empty())
    {
        let start = params.len() + 1;
        let placeholders = issue_numbers
            .iter()
            .enumerate()
            .map(|(idx, _)| format!("?{}", start + idx))
            .collect::<Vec<_>>()
            .join(",");
        for issue_number in issue_numbers {
            params.push(Box::new(*issue_number));
        }
        conditions.push(format!("{}github_issue_number IN ({placeholders})", prefix));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_DONE, ENTRY_STATUS_PENDING, EntryStatusUpdateError,
        EntryStatusUpdateOptions, list_entry_dispatch_history, update_entry_status_on_conn,
    };
    use rusqlite::Connection;

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE auto_queue_runs (
                id TEXT PRIMARY KEY,
                repo TEXT,
                agent_id TEXT,
                status TEXT,
                completed_at DATETIME,
                max_concurrent_threads INTEGER DEFAULT 1
            );
            CREATE TABLE auto_queue_entries (
                id TEXT PRIMARY KEY,
                run_id TEXT,
                kanban_card_id TEXT,
                agent_id TEXT,
                status TEXT,
                dispatch_id TEXT,
                slot_index INTEGER,
                thread_group INTEGER DEFAULT 0,
                batch_phase INTEGER DEFAULT 0,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                dispatched_at DATETIME,
                completed_at DATETIME
            );
            CREATE TABLE auto_queue_entry_dispatch_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id TEXT NOT NULL,
                dispatch_id TEXT NOT NULL,
                trigger_source TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(entry_id, dispatch_id)
            );
            CREATE TABLE auto_queue_slots (
                agent_id TEXT NOT NULL,
                slot_index INTEGER NOT NULL,
                assigned_run_id TEXT,
                assigned_thread_group INTEGER,
                thread_id_map TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (agent_id, slot_index)
            );
            CREATE TABLE kv_meta (
                key TEXT PRIMARY KEY,
                value TEXT
            );
            CREATE TABLE agents (
                id TEXT PRIMARY KEY,
                discord_channel_id TEXT
            );
            CREATE TABLE message_outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                target TEXT,
                content TEXT,
                bot TEXT,
                source TEXT
            );",
        )
        .expect("schema");
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('run-1', 'repo-1', 'agent-1', 'active')",
            [],
        )
        .expect("seed run");
        conn.execute(
            "INSERT INTO agents (id, discord_channel_id) VALUES ('agent-1', '123')",
            [],
        )
        .expect("seed agent");
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-1', 0, 'run-1', 0, '{}')",
            [],
        )
        .expect("seed slot");
        conn
    }

    #[test]
    fn entry_transition_done_releases_slots_and_completes_run() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group
             ) VALUES ('entry-1', 'run-1', 'card-1', 'agent-1', 'pending', NULL, 0, 0)",
            [],
        )
        .expect("seed entry");

        let dispatched = update_entry_status_on_conn(
            &conn,
            "entry-1",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-1".to_string()),
                slot_index: Some(0),
            },
        )
        .expect("dispatch transition");
        assert_eq!(dispatched.from_status, ENTRY_STATUS_PENDING);
        assert_eq!(dispatched.to_status, ENTRY_STATUS_DISPATCHED);

        update_entry_status_on_conn(
            &conn,
            "entry-1",
            ENTRY_STATUS_DONE,
            "test_done",
            &EntryStatusUpdateOptions::default(),
        )
        .expect("done transition");

        let (status, dispatch_id, completed_at): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, dispatch_id, completed_at FROM auto_queue_entries WHERE id = 'entry-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("entry row");
        assert_eq!(status, ENTRY_STATUS_DONE);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-1"));
        assert!(completed_at.is_some());

        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("run status");
        assert_eq!(run_status, "completed");

        let slot_run: Option<String> = conn
            .query_row(
                "SELECT assigned_run_id FROM auto_queue_slots WHERE agent_id = 'agent-1' AND slot_index = 0",
                [],
                |row| row.get(0),
            )
            .expect("slot row");
        assert!(slot_run.is_none());

        let audit_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = 'entry-1'",
                [],
                |row| row.get(0),
            )
            .expect("audit count");
        assert_eq!(audit_rows, 2);

        let (target, bot, content): (String, String, String) = conn
            .query_row(
                "SELECT target, bot, content FROM message_outbox ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("completion notify");
        assert_eq!(target, "channel:123");
        assert_eq!(bot, "notify");
        assert!(content.contains("자동큐 완료: repo-1 / run run-1 / 1개"));
    }

    #[test]
    fn entry_transition_pending_clears_dispatch_binding() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group, completed_at
             ) VALUES ('entry-2', 'run-1', 'card-2', 'agent-1', 'dispatched', 'dispatch-2', 0, 0, datetime('now'))",
            [],
        )
        .expect("seed entry");

        update_entry_status_on_conn(
            &conn,
            "entry-2",
            ENTRY_STATUS_PENDING,
            "test_reset",
            &EntryStatusUpdateOptions::default(),
        )
        .expect("pending reset");

        let (status, dispatch_id, slot_index, completed_at): (
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, dispatch_id, slot_index, completed_at FROM auto_queue_entries WHERE id = 'entry-2'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("entry row");
        assert_eq!(status, ENTRY_STATUS_PENDING);
        assert!(dispatch_id.is_none());
        assert!(slot_index.is_none());
        assert!(completed_at.is_none());
    }

    #[test]
    fn entry_dispatch_history_preserves_previous_dispatch_ids() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-history', 'run-1', 'card-history', 'agent-1', 'pending', 0)",
            [],
        )
        .expect("seed entry");

        update_entry_status_on_conn(
            &conn,
            "entry-history",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch_initial",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-consult".to_string()),
                slot_index: Some(0),
            },
        )
        .expect("initial dispatch");
        update_entry_status_on_conn(
            &conn,
            "entry-history",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch_resume",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-impl".to_string()),
                slot_index: Some(0),
            },
        )
        .expect("resumed dispatch");

        let history = list_entry_dispatch_history(&conn, "entry-history").expect("history");
        assert_eq!(history, vec!["dispatch-consult", "dispatch-impl"]);

        let current_dispatch_id: Option<String> = conn
            .query_row(
                "SELECT dispatch_id FROM auto_queue_entries WHERE id = 'entry-history'",
                [],
                |row| row.get(0),
            )
            .expect("current dispatch");
        assert_eq!(current_dispatch_id.as_deref(), Some("dispatch-impl"));
    }

    #[test]
    fn entry_transition_blocks_invalid_skipped_reactivation() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-3', 'run-1', 'card-3', 'agent-1', 'skipped', 0)",
            [],
        )
        .expect("seed entry");

        let error = update_entry_status_on_conn(
            &conn,
            "entry-3",
            ENTRY_STATUS_DISPATCHED,
            "test_invalid",
            &EntryStatusUpdateOptions::default(),
        )
        .expect_err("invalid transition must fail");
        assert!(matches!(
            error,
            EntryStatusUpdateError::InvalidTransition { .. }
        ));
    }
}
