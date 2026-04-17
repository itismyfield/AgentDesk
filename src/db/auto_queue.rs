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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultationDispatchRecordResult {
    pub metadata_json: String,
    pub entry_status_changed: bool,
}

#[derive(Debug, Error)]
pub enum ConsultationDispatchRecordError {
    #[error("consultation dispatch id is required")]
    MissingDispatchId,
    #[error("consultation trigger source is required")]
    MissingSource,
    #[error("consultation card not found: {card_id}")]
    CardNotFound { card_id: String },
    #[error(transparent)]
    EntryStatus(#[from] EntryStatusUpdateError),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

const SLOT_ALLOCATION_MAX_RETRIES: usize = 16;

#[derive(Debug, Error)]
pub enum SlotAllocationError {
    #[error(
        "slot allocation retry limit exceeded for run {run_id} agent {agent_id} group {thread_group} after {attempts} attempts"
    )]
    RetryLimitExceeded {
        run_id: String,
        agent_id: String,
        thread_group: i64,
        attempts: usize,
    },
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

#[derive(Debug, Clone)]
struct EntryStatusRow {
    run_id: String,
    card_id: String,
    agent_id: String,
    status: String,
    dispatch_id: Option<String>,
    slot_index: Option<i64>,
    thread_group: i64,
    batch_phase: i64,
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
    update_entry_status_with_current_on_conn(
        conn,
        entry_id,
        normalized,
        trigger_source,
        options,
        current,
    )
}

pub fn reactivate_done_entry_on_conn(
    conn: &Connection,
    entry_id: &str,
    trigger_source: &str,
    options: &EntryStatusUpdateOptions,
) -> Result<EntryStatusUpdateResult, EntryStatusUpdateError> {
    let current = load_entry_status_row(conn, entry_id)?;
    if current.status != ENTRY_STATUS_DONE {
        return update_entry_status_with_current_on_conn(
            conn,
            entry_id,
            ENTRY_STATUS_DISPATCHED,
            trigger_source,
            options,
            current,
        );
    }

    let effective_dispatch_id = options
        .dispatch_id
        .clone()
        .or_else(|| current.dispatch_id.clone());
    let effective_slot_index = options.slot_index.or(current.slot_index);

    conn.execute_batch("SAVEPOINT auto_queue_entry_done_reactivate")?;
    let restore_result = (|| -> rusqlite::Result<usize> {
        let rows_affected = conn.execute(
            "UPDATE auto_queue_entries
                 SET status = 'dispatched',
                     dispatch_id = ?1,
                     slot_index = ?2,
                     dispatched_at = datetime('now'),
                     completed_at = NULL
                 WHERE id = ?3
                   AND status = 'done'",
            rusqlite::params![effective_dispatch_id, effective_slot_index, entry_id,],
        )?;

        if rows_affected == 0 {
            return Ok(0);
        }

        conn.execute(
            "UPDATE auto_queue_runs
                 SET status = 'active',
                     completed_at = NULL
                 WHERE id = ?1
                   AND status = 'completed'",
            [&current.run_id],
        )?;

        if let Some(dispatch_id) = effective_dispatch_id.as_deref() {
            record_entry_dispatch_history_on_conn(conn, entry_id, dispatch_id, trigger_source)?;
        }

        record_entry_transition_on_conn(
            conn,
            entry_id,
            ENTRY_STATUS_DONE,
            ENTRY_STATUS_DISPATCHED,
            trigger_source,
        )?;

        Ok(rows_affected)
    })();

    let rows_affected = match restore_result {
        Ok(rows_affected) => {
            conn.execute_batch("RELEASE SAVEPOINT auto_queue_entry_done_reactivate")?;
            rows_affected
        }
        Err(error) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO SAVEPOINT auto_queue_entry_done_reactivate; \
                 RELEASE SAVEPOINT auto_queue_entry_done_reactivate",
            );
            return Err(EntryStatusUpdateError::Sql(error));
        }
    };

    if rows_affected == 0 {
        let latest = load_entry_status_row(conn, entry_id)?;
        if entry_status_row_matches_target(
            &latest,
            ENTRY_STATUS_DISPATCHED,
            effective_dispatch_id.as_deref(),
            effective_slot_index,
        ) {
            return Ok(EntryStatusUpdateResult {
                run_id: latest.run_id,
                from_status: latest.status,
                to_status: ENTRY_STATUS_DISPATCHED.to_string(),
                changed: false,
            });
        }

        return Err(EntryStatusUpdateError::InvalidTransition {
            entry_id: entry_id.to_string(),
            from_status: latest.status,
            to_status: ENTRY_STATUS_DISPATCHED.to_string(),
        });
    }

    Ok(EntryStatusUpdateResult {
        run_id: current.run_id,
        from_status: ENTRY_STATUS_DONE.to_string(),
        to_status: ENTRY_STATUS_DISPATCHED.to_string(),
        changed: true,
    })
}

fn update_entry_status_with_current_on_conn(
    conn: &Connection,
    entry_id: &str,
    normalized: &str,
    trigger_source: &str,
    options: &EntryStatusUpdateOptions,
    mut current: EntryStatusRow,
) -> Result<EntryStatusUpdateResult, EntryStatusUpdateError> {
    loop {
        let log_ctx = crate::services::auto_queue::AutoQueueLogContext::new()
            .run(&current.run_id)
            .entry(entry_id)
            .card(&current.card_id)
            .maybe_dispatch(current.dispatch_id.as_deref())
            .agent(&current.agent_id)
            .thread_group(current.thread_group)
            .batch_phase(current.batch_phase)
            .maybe_slot_index(current.slot_index);

        if !is_allowed_entry_transition(&current.status, normalized, trigger_source) {
            crate::auto_queue_log!(
                warn,
                "entry_status_transition_blocked",
                log_ctx.clone(),
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

        conn.execute_batch("SAVEPOINT auto_queue_entry_status_transition")?;
        let transition_result = (|| -> rusqlite::Result<usize> {
            let rows_affected = match normalized {
                ENTRY_STATUS_PENDING => conn.execute(
                    "UPDATE auto_queue_entries
                         SET status = 'pending',
                             dispatch_id = NULL,
                             slot_index = NULL,
                             dispatched_at = NULL,
                             completed_at = NULL
                         WHERE id = ?1
                           AND status = ?2",
                    rusqlite::params![entry_id, current.status],
                )?,
                ENTRY_STATUS_DISPATCHED => conn.execute(
                    "UPDATE auto_queue_entries
                         SET status = 'dispatched',
                             dispatch_id = ?1,
                             slot_index = ?2,
                             dispatched_at = datetime('now'),
                             completed_at = NULL
                         WHERE id = ?3
                           AND status = ?4",
                    rusqlite::params![
                        effective_dispatch_id,
                        effective_slot_index,
                        entry_id,
                        current.status
                    ],
                )?,
                ENTRY_STATUS_DONE => conn.execute(
                    "UPDATE auto_queue_entries
                         SET status = 'done',
                             completed_at = datetime('now')
                         WHERE id = ?1
                           AND status = ?2",
                    rusqlite::params![entry_id, current.status],
                )?,
                ENTRY_STATUS_SKIPPED => conn.execute(
                    "UPDATE auto_queue_entries
                         SET status = 'skipped',
                             dispatch_id = NULL,
                             dispatched_at = NULL,
                             completed_at = datetime('now')
                         WHERE id = ?1
                           AND status = ?2",
                    rusqlite::params![entry_id, current.status],
                )?,
                _ => unreachable!(),
            };

            if rows_affected == 0 {
                return Ok(0);
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
                    record_entry_dispatch_history_on_conn(
                        conn,
                        entry_id,
                        dispatch_id,
                        trigger_source,
                    )?;
                }
            }

            record_entry_transition_on_conn(
                conn,
                entry_id,
                &current.status,
                normalized,
                trigger_source,
            )?;

            if matches!(normalized, ENTRY_STATUS_DONE | ENTRY_STATUS_SKIPPED) {
                maybe_finalize_run_after_terminal_entry(conn, &current.run_id, normalized)?;
            }

            Ok(rows_affected)
        })();
        let rows_affected = match transition_result {
            Ok(rows_affected) => {
                conn.execute_batch("RELEASE SAVEPOINT auto_queue_entry_status_transition")?;
                rows_affected
            }
            Err(error) => {
                let _ = conn.execute_batch(
                    "ROLLBACK TO SAVEPOINT auto_queue_entry_status_transition; \
                     RELEASE SAVEPOINT auto_queue_entry_status_transition",
                );
                return Err(EntryStatusUpdateError::Sql(error));
            }
        };

        if rows_affected == 0 {
            let latest = load_entry_status_row(conn, entry_id)?;
            if entry_status_row_matches_target(
                &latest,
                normalized,
                effective_dispatch_id.as_deref(),
                effective_slot_index,
            ) {
                return Ok(EntryStatusUpdateResult {
                    run_id: latest.run_id,
                    from_status: latest.status,
                    to_status: normalized.to_string(),
                    changed: false,
                });
            }

            if !is_allowed_entry_transition(&latest.status, normalized, trigger_source) {
                let stale_log_ctx = crate::services::auto_queue::AutoQueueLogContext::new()
                    .run(&latest.run_id)
                    .entry(entry_id)
                    .card(&latest.card_id)
                    .maybe_dispatch(latest.dispatch_id.as_deref())
                    .agent(&latest.agent_id)
                    .thread_group(latest.thread_group)
                    .batch_phase(latest.batch_phase)
                    .maybe_slot_index(latest.slot_index);
                crate::auto_queue_log!(
                    warn,
                    "entry_status_stale_transition_blocked",
                    stale_log_ctx,
                    "[auto-queue] stale entry transition blocked {} {} -> {} (source: {})",
                    entry_id,
                    latest.status,
                    normalized,
                    trigger_source
                );
                return Err(EntryStatusUpdateError::InvalidTransition {
                    entry_id: entry_id.to_string(),
                    from_status: latest.status,
                    to_status: normalized.to_string(),
                });
            }

            current = latest;
            continue;
        }

        return Ok(EntryStatusUpdateResult {
            run_id: current.run_id,
            from_status: current.status,
            to_status: normalized.to_string(),
            changed: true,
        });
    }
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

pub fn rebind_slot_for_group_agent(
    conn: &Connection,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
    slot_index: i64,
) -> rusqlite::Result<usize> {
    ensure_agent_slot_rows(conn, run_id, agent_id)?;

    let slot_updated = conn.execute(
        "UPDATE auto_queue_slots
         SET assigned_run_id = ?1,
             assigned_thread_group = ?2,
             updated_at = datetime('now')
         WHERE agent_id = ?3
           AND slot_index = ?4
           AND (assigned_run_id IS NULL OR assigned_run_id = ?1)",
        rusqlite::params![run_id, thread_group, agent_id, slot_index],
    )?;
    if slot_updated == 0 {
        return Ok(0);
    }

    bind_slot_index_for_group_entries(conn, run_id, agent_id, thread_group, slot_index)
}

fn bind_slot_index_for_group_entries(
    conn: &Connection,
    run_id: &str,
    agent_id: &str,
    thread_group: i64,
    slot_index: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE auto_queue_entries
         SET slot_index = ?1
         WHERE run_id = ?2
           AND agent_id = ?3
           AND COALESCE(thread_group, 0) = ?4
           AND status IN ('pending', 'dispatched')
           AND (slot_index IS NULL OR slot_index != ?1)",
        rusqlite::params![slot_index, run_id, agent_id, thread_group],
    )
}

pub fn release_slot_for_group_agent(
    conn: &Connection,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
    slot_index: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = datetime('now')
         WHERE agent_id = ?1
           AND slot_index = ?2
           AND assigned_run_id = ?3
           AND COALESCE(assigned_thread_group, 0) = ?4",
        rusqlite::params![agent_id, slot_index, run_id, thread_group],
    )
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
    pub deploy_phases: Option<String>,
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

#[derive(Debug, Clone)]
pub struct AutoQueueRunHistoryRecord {
    pub id: String,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub entry_count: i64,
    pub done_count: i64,
    pub skipped_count: i64,
    pub pending_count: i64,
    pub dispatched_count: i64,
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
                COALESCE(thread_group_count, 1),
                deploy_phases
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
                deploy_phases: row.get(11)?,
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

pub fn list_run_history(
    conn: &Connection,
    filter: &StatusFilter,
    limit: usize,
) -> rusqlite::Result<Vec<AutoQueueRunHistoryRecord>> {
    let mut sql = String::from(
        "SELECT r.id, r.repo, r.agent_id, r.status,
                CAST(strftime('%s', r.created_at) AS INTEGER) * 1000,
                CASE WHEN r.completed_at IS NOT NULL THEN CAST(strftime('%s', r.completed_at) AS INTEGER) * 1000 END,
                COUNT(e.id),
                COALESCE(SUM(CASE WHEN e.status = 'done' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN e.status = 'skipped' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN e.status = 'pending' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN e.status = 'dispatched' THEN 1 ELSE 0 END), 0)
         FROM auto_queue_runs r
         LEFT JOIN auto_queue_entries e ON e.run_id = r.id
         LEFT JOIN kanban_cards kc ON kc.id = e.kanban_card_id
         WHERE 1 = 1",
    );
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    if let Some(repo) = filter.repo.as_ref().filter(|value| !value.is_empty()) {
        params.push(Box::new(repo.clone()));
        sql.push_str(&format!(
            " AND COALESCE(kc.repo_id, r.repo, '') = ?{}",
            params.len()
        ));
    }
    if let Some(agent_id) = filter.agent_id.as_ref().filter(|value| !value.is_empty()) {
        params.push(Box::new(agent_id.clone()));
        sql.push_str(&format!(
            " AND COALESCE(e.agent_id, r.agent_id, '') = ?{}",
            params.len()
        ));
    }

    sql.push_str(
        " GROUP BY r.id, r.repo, r.agent_id, r.status, r.created_at, r.completed_at
          ORDER BY r.created_at DESC",
    );
    params.push(Box::new(limit as i64));
    sql.push_str(&format!(" LIMIT ?{}", params.len()));

    let param_refs: Vec<&dyn ToSql> = params.iter().map(|value| value.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(AutoQueueRunHistoryRecord {
            id: row.get(0)?,
            repo: row.get(1)?,
            agent_id: row.get(2)?,
            status: row.get(3)?,
            created_at: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            completed_at: row.get(5)?,
            entry_count: row.get(6)?,
            done_count: row.get(7)?,
            skipped_count: row.get(8)?,
            pending_count: row.get(9)?,
            dispatched_count: row.get(10)?,
        })
    })?;
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

pub fn release_run_slots(conn: &Connection, run_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = datetime('now')
         WHERE assigned_run_id = ?1",
        [run_id],
    )?;
    Ok(())
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
    conn.query_row(
        "SELECT COUNT(*) > 0
         FROM auto_queue_phase_gates
         WHERE run_id = ?1
           AND status IN ('pending', 'failed')",
        [run_id],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn consultation_metadata_object(
    base_metadata_json: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let trimmed = base_metadata_json.trim();
    if trimmed.is_empty() {
        return serde_json::Map::new();
    }

    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

pub fn record_consultation_dispatch_on_conn(
    conn: &mut Connection,
    entry_id: &str,
    card_id: &str,
    dispatch_id: &str,
    trigger_source: &str,
    base_metadata_json: &str,
) -> Result<ConsultationDispatchRecordResult, ConsultationDispatchRecordError> {
    let dispatch_id = dispatch_id.trim();
    if dispatch_id.is_empty() {
        return Err(ConsultationDispatchRecordError::MissingDispatchId);
    }
    let trigger_source = trigger_source.trim();
    if trigger_source.is_empty() {
        return Err(ConsultationDispatchRecordError::MissingSource);
    }

    let tx = conn.transaction()?;
    let mut metadata = consultation_metadata_object(base_metadata_json);
    metadata.insert(
        "consultation_status".to_string(),
        serde_json::json!("pending"),
    );
    metadata.insert(
        "consultation_dispatch_id".to_string(),
        serde_json::json!(dispatch_id),
    );
    let metadata_json = serde_json::Value::Object(metadata).to_string();

    let updated = tx.execute(
        "UPDATE kanban_cards
         SET metadata = ?1,
             updated_at = datetime('now')
         WHERE id = ?2",
        rusqlite::params![&metadata_json, card_id],
    )?;
    if updated == 0 {
        return Err(ConsultationDispatchRecordError::CardNotFound {
            card_id: card_id.to_string(),
        });
    }

    let entry_result = update_entry_status_on_conn(
        &tx,
        entry_id,
        ENTRY_STATUS_DISPATCHED,
        trigger_source,
        &EntryStatusUpdateOptions {
            dispatch_id: Some(dispatch_id.to_string()),
            slot_index: None,
        },
    )?;

    tx.commit()?;
    Ok(ConsultationDispatchRecordResult {
        metadata_json,
        entry_status_changed: entry_result.changed,
    })
}

#[derive(Debug, Clone, Default)]
pub struct PhaseGateStateWrite {
    pub status: String,
    pub verdict: Option<String>,
    pub dispatch_ids: Vec<String>,
    pub pass_verdict: String,
    pub next_phase: Option<i64>,
    pub final_phase: bool,
    pub anchor_card_id: Option<String>,
    pub failure_reason: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhaseGateSaveResult {
    pub persisted_dispatch_ids: Vec<String>,
    pub removed_stale_rows: usize,
}

fn normalize_phase_gate_status(status: &str) -> String {
    let trimmed = status.trim();
    if trimmed.is_empty() {
        "pending".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_phase_gate_pass_verdict(pass_verdict: &str) -> String {
    let trimmed = pass_verdict.trim();
    if trimmed.is_empty() {
        "phase_gate_passed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_optional_text(value: Option<&str>) -> Option<String> {
    value.and_then(|item| {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn dedupe_phase_gate_dispatch_ids(dispatch_ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for dispatch_id in dispatch_ids {
        let normalized = dispatch_id.trim();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.to_string()) {
            deduped.push(normalized.to_string());
        }
    }
    deduped
}

fn valid_phase_gate_dispatch_ids(
    conn: &Connection,
    dispatch_ids: &[String],
) -> rusqlite::Result<Vec<String>> {
    if dispatch_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat_n("?", dispatch_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id FROM task_dispatches WHERE id IN ({})",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let params = rusqlite::params_from_iter(dispatch_ids.iter().map(String::as_str));
    let mut rows = stmt.query(params)?;
    let mut valid = std::collections::HashSet::new();
    while let Some(row) = rows.next()? {
        let dispatch_id: String = row.get(0)?;
        valid.insert(dispatch_id);
    }

    Ok(dispatch_ids
        .iter()
        .filter(|dispatch_id| valid.contains(dispatch_id.as_str()))
        .cloned()
        .collect())
}

fn delete_stale_phase_gate_rows(
    conn: &Connection,
    run_id: &str,
    phase: i64,
    dispatch_ids: &[String],
) -> rusqlite::Result<usize> {
    if dispatch_ids.is_empty() {
        return conn.execute(
            "DELETE FROM auto_queue_phase_gates WHERE run_id = ?1 AND phase = ?2",
            rusqlite::params![run_id, phase],
        );
    }

    let placeholders = std::iter::repeat_n("?", dispatch_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "DELETE FROM auto_queue_phase_gates
         WHERE run_id = ?1
           AND phase = ?2
           AND (dispatch_id IS NULL OR dispatch_id NOT IN ({}))",
        placeholders
    );
    let mut values = vec![rusqlite::types::Value::from(run_id.to_string())];
    values.push(rusqlite::types::Value::from(phase));
    values.extend(
        dispatch_ids
            .iter()
            .cloned()
            .map(rusqlite::types::Value::from),
    );
    conn.execute(&sql, rusqlite::params_from_iter(values))
}

pub fn save_phase_gate_state_on_conn(
    conn: &Connection,
    run_id: &str,
    phase: i64,
    state: &PhaseGateStateWrite,
) -> rusqlite::Result<PhaseGateSaveResult> {
    let dispatch_ids =
        valid_phase_gate_dispatch_ids(conn, &dedupe_phase_gate_dispatch_ids(&state.dispatch_ids))?;
    let removed_stale_rows = delete_stale_phase_gate_rows(conn, run_id, phase, &dispatch_ids)?;
    let status = normalize_phase_gate_status(&state.status);
    let verdict = normalize_optional_text(state.verdict.as_deref());
    let pass_verdict = normalize_phase_gate_pass_verdict(&state.pass_verdict);
    let anchor_card_id = normalize_optional_text(state.anchor_card_id.as_deref());
    let failure_reason = normalize_optional_text(state.failure_reason.as_deref());
    let created_at = normalize_optional_text(state.created_at.as_deref());

    if dispatch_ids.is_empty() {
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, verdict, dispatch_id, pass_verdict, next_phase,
                final_phase, anchor_card_id, failure_reason, created_at, updated_at
             ) VALUES (
                ?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9,
                COALESCE(?10, CURRENT_TIMESTAMP), datetime('now')
             )",
            rusqlite::params![
                run_id,
                phase,
                status,
                verdict,
                pass_verdict,
                state.next_phase,
                if state.final_phase { 1 } else { 0 },
                anchor_card_id,
                failure_reason,
                created_at
            ],
        )?;
    } else {
        for dispatch_id in &dispatch_ids {
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, verdict, dispatch_id, pass_verdict, next_phase,
                    final_phase, anchor_card_id, failure_reason, created_at, updated_at
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                    COALESCE(?11, CURRENT_TIMESTAMP), datetime('now')
                 )
                 ON CONFLICT(dispatch_id) DO UPDATE SET
                    run_id = excluded.run_id,
                    phase = excluded.phase,
                    status = excluded.status,
                    verdict = excluded.verdict,
                    pass_verdict = excluded.pass_verdict,
                    next_phase = excluded.next_phase,
                    final_phase = excluded.final_phase,
                    anchor_card_id = excluded.anchor_card_id,
                    failure_reason = excluded.failure_reason,
                    updated_at = datetime('now')",
                rusqlite::params![
                    run_id,
                    phase,
                    status,
                    verdict,
                    dispatch_id,
                    pass_verdict,
                    state.next_phase,
                    if state.final_phase { 1 } else { 0 },
                    anchor_card_id,
                    failure_reason,
                    created_at
                ],
            )?;
        }
    }

    Ok(PhaseGateSaveResult {
        persisted_dispatch_ids: dispatch_ids,
        removed_stale_rows,
    })
}

pub fn clear_phase_gate_state_on_conn(
    conn: &Connection,
    run_id: &str,
    phase: i64,
) -> rusqlite::Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM auto_queue_phase_gates WHERE run_id = ?1 AND phase = ?2",
        rusqlite::params![run_id, phase],
    )?;
    Ok(deleted > 0)
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
) -> Option<(String, String, String, i64)> {
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
                batch_phase_is_eligible(batch_phase, current_phase).then_some((
                    entry_id,
                    card_id,
                    agent_id,
                    batch_phase,
                ))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotAllocation {
    pub slot_index: i64,
    pub newly_assigned: bool,
    pub reassigned_from_other_group: bool,
}

pub fn allocate_slot_for_group_agent(
    conn: &Connection,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
) -> Result<Option<SlotAllocation>, SlotAllocationError> {
    let log_ctx = crate::services::auto_queue::AutoQueueLogContext::new()
        .run(run_id)
        .agent(agent_id)
        .thread_group(thread_group);
    ensure_agent_slot_rows(conn, run_id, agent_id).map_err(|error| {
        crate::auto_queue_log!(
            warn,
            "slot_allocate_prepare_failed",
            log_ctx.clone(),
            "[auto-queue] failed to prepare slot rows for run {run_id} agent {agent_id} group {thread_group}: {error}"
        );
        SlotAllocationError::Sql(error)
    })?;

    for attempt in 1..=SLOT_ALLOCATION_MAX_RETRIES {
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
            .optional()
            .map_err(|error| {
                crate::auto_queue_log!(
                    warn,
                    "slot_allocate_existing_lookup_failed",
                    log_ctx.clone(),
                    "[auto-queue] failed to inspect existing slot for run {run_id} agent {agent_id} group {thread_group}: {error}"
                );
                SlotAllocationError::Sql(error)
            })?;
        if let Some(slot_index) = existing {
            bind_slot_index_for_group_entries(conn, run_id, agent_id, thread_group, slot_index)
            .map_err(|error| {
                crate::auto_queue_log!(
                    warn,
                    "slot_allocate_existing_bind_failed",
                    log_ctx.clone().slot_index(slot_index),
                    "[auto-queue] failed to bind existing slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                );
                SlotAllocationError::Sql(error)
            })?;
            return Ok(Some(SlotAllocation {
                slot_index,
                newly_assigned: false,
                reassigned_from_other_group: false,
            }));
        }

        let reusable_slot: Option<i64> = conn
            .query_row(
                "SELECT s.slot_index
                 FROM auto_queue_slots s
                 WHERE s.agent_id = ?1
                   AND s.assigned_run_id = ?2
                   AND COALESCE(s.assigned_thread_group, -1) != ?3
                   AND NOT EXISTS (
                       SELECT 1
                       FROM auto_queue_entries e
                       WHERE e.run_id = ?2
                         AND e.agent_id = s.agent_id
                         AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
                         AND e.status IN ('pending', 'dispatched')
                   )
                 ORDER BY s.slot_index ASC
                 LIMIT 1",
                rusqlite::params![agent_id, run_id, thread_group],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                crate::auto_queue_log!(
                    warn,
                    "slot_allocate_reusable_lookup_failed",
                    log_ctx.clone(),
                    "[auto-queue] failed to inspect reusable slot for run {run_id} agent {agent_id} group {thread_group}: {error}"
                );
                SlotAllocationError::Sql(error)
            })?;
        if let Some(slot_index) = reusable_slot {
            let rebound = conn
                .execute(
                    "UPDATE auto_queue_slots
                     SET assigned_thread_group = ?1,
                         updated_at = datetime('now')
                     WHERE agent_id = ?2
                       AND slot_index = ?3
                       AND assigned_run_id = ?4
                       AND COALESCE(assigned_thread_group, -1) != ?1
                       AND NOT EXISTS (
                           SELECT 1
                           FROM auto_queue_entries e
                           WHERE e.run_id = ?4
                             AND e.agent_id = auto_queue_slots.agent_id
                             AND COALESCE(e.thread_group, 0) = COALESCE(auto_queue_slots.assigned_thread_group, 0)
                             AND e.status IN ('pending', 'dispatched')
                       )",
                    rusqlite::params![thread_group, agent_id, slot_index, run_id],
                )
                .map_err(|error| {
                    crate::auto_queue_log!(
                        warn,
                        "slot_allocate_rebind_failed",
                        log_ctx.clone().slot_index(slot_index),
                        "[auto-queue] failed to rebind slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    );
                    SlotAllocationError::Sql(error)
                })?;
            if rebound == 0 {
                if attempt == SLOT_ALLOCATION_MAX_RETRIES {
                    crate::auto_queue_log!(
                        warn,
                        "slot_allocate_rebind_retry_limit_reached",
                        log_ctx.clone().slot_index(slot_index),
                        "[auto-queue] slot rebind retry limit reached for run {run_id} agent {agent_id} group {thread_group} after {attempt} attempts"
                    );
                    return Err(SlotAllocationError::RetryLimitExceeded {
                        run_id: run_id.to_string(),
                        agent_id: agent_id.to_string(),
                        thread_group,
                        attempts: attempt,
                    });
                }
                continue;
            }

            bind_slot_index_for_group_entries(conn, run_id, agent_id, thread_group, slot_index)
                .map_err(|error| {
                    crate::auto_queue_log!(
                        warn,
                        "slot_allocate_rebind_bind_failed",
                        log_ctx.clone().slot_index(slot_index),
                        "[auto-queue] failed to bind rebound slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    );
                    SlotAllocationError::Sql(error)
                })?;
            return Ok(Some(SlotAllocation {
                slot_index,
                newly_assigned: false,
                reassigned_from_other_group: true,
            }));
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
            .optional()
            .map_err(|error| {
                crate::auto_queue_log!(
                    warn,
                    "slot_allocate_free_lookup_failed",
                    log_ctx.clone(),
                    "[auto-queue] failed to inspect free slot for run {run_id} agent {agent_id} group {thread_group}: {error}"
                );
                SlotAllocationError::Sql(error)
            })?;
        let Some(slot_index) = free_slot else {
            return Ok(None);
        };

        let claimed = conn
            .execute(
                "UPDATE auto_queue_slots
                 SET assigned_run_id = ?1,
                     assigned_thread_group = ?2,
                     updated_at = datetime('now')
                 WHERE agent_id = ?3
                   AND slot_index = ?4
                   AND assigned_run_id IS NULL",
                rusqlite::params![run_id, thread_group, agent_id, slot_index],
            )
            .map_err(|error| {
                crate::auto_queue_log!(
                    warn,
                    "slot_allocate_claim_failed",
                    log_ctx.clone().slot_index(slot_index),
                    "[auto-queue] failed to claim slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                );
                SlotAllocationError::Sql(error)
            })?;
        if claimed == 0 {
            if attempt == SLOT_ALLOCATION_MAX_RETRIES {
                crate::auto_queue_log!(
                    warn,
                    "slot_allocate_retry_limit_reached",
                    log_ctx.clone().slot_index(slot_index),
                    "[auto-queue] slot allocation CAS retry limit reached for run {run_id} agent {agent_id} group {thread_group} after {attempt} attempts"
                );
                return Err(SlotAllocationError::RetryLimitExceeded {
                    run_id: run_id.to_string(),
                    agent_id: agent_id.to_string(),
                    thread_group,
                    attempts: attempt,
                });
            }
            continue;
        }

        bind_slot_index_for_group_entries(conn, run_id, agent_id, thread_group, slot_index)
        .map_err(|error| {
            crate::auto_queue_log!(
                warn,
                "slot_allocate_bind_failed",
                log_ctx.clone().slot_index(slot_index),
                "[auto-queue] failed to bind claimed slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
            );
            SlotAllocationError::Sql(error)
        })?;
        return Ok(Some(SlotAllocation {
            slot_index,
            newly_assigned: true,
            reassigned_from_other_group: false,
        }));
    }

    unreachable!("slot allocation loop must return within bounded retries");
}

pub fn slot_has_active_dispatch(conn: &Connection, agent_id: &str, slot_index: i64) -> bool {
    slot_has_active_dispatch_excluding(conn, agent_id, slot_index, None)
}

pub fn slot_has_active_dispatch_excluding(
    conn: &Connection,
    agent_id: &str,
    slot_index: i64,
    exclude_dispatch_id: Option<&str>,
) -> bool {
    let exclude_id = exclude_dispatch_id.unwrap_or("");
    let auto_queue_active: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0
             FROM auto_queue_entries
             WHERE agent_id = ?1
               AND slot_index = ?2
               AND status = 'dispatched'
               AND COALESCE(dispatch_id, '') != ?3",
            rusqlite::params![agent_id, slot_index, exclude_id],
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
           AND CAST(json_extract(COALESCE(context, '{}'), '$.slot_index') AS INTEGER) = ?2
           AND COALESCE(CAST(json_extract(COALESCE(context, '{}'), '$.sidecar_dispatch') AS INTEGER), 0) = 0
           AND json_type(COALESCE(context, '{}'), '$.phase_gate') IS NULL
           AND id != ?3",
        rusqlite::params![agent_id, slot_index, exclude_id],
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
        "SELECT run_id,
                kanban_card_id,
                agent_id,
                status,
                dispatch_id,
                slot_index,
                COALESCE(thread_group, 0),
                COALESCE(batch_phase, 0),
                completed_at
         FROM auto_queue_entries
         WHERE id = ?1",
        [entry_id],
        |row| {
            Ok(EntryStatusRow {
                run_id: row.get(0)?,
                card_id: row.get(1)?,
                agent_id: row.get(2)?,
                status: row.get(3)?,
                dispatch_id: row.get(4)?,
                slot_index: row.get(5)?,
                thread_group: row.get(6)?,
                batch_phase: row.get(7)?,
                completed_at: row.get(8)?,
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

fn is_allowed_entry_transition(from_status: &str, to_status: &str, trigger_source: &str) -> bool {
    if from_status == to_status {
        return true;
    }

    if from_status == ENTRY_STATUS_DONE
        && to_status == ENTRY_STATUS_DISPATCHED
        && matches!(trigger_source, "pmd_reopen" | "rereview_dispatch")
    {
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
            | (ENTRY_STATUS_SKIPPED, ENTRY_STATUS_PENDING)
            | (ENTRY_STATUS_SKIPPED, ENTRY_STATUS_DISPATCHED)
            | (ENTRY_STATUS_SKIPPED, ENTRY_STATUS_DONE)
    )
}

fn entry_status_row_matches_target(
    row: &EntryStatusRow,
    normalized: &str,
    effective_dispatch_id: Option<&str>,
    effective_slot_index: Option<i64>,
) -> bool {
    if row.status != normalized {
        return false;
    }

    match normalized {
        ENTRY_STATUS_PENDING => {
            row.dispatch_id.is_none() && row.slot_index.is_none() && row.completed_at.is_none()
        }
        ENTRY_STATUS_DISPATCHED => {
            row.dispatch_id.as_deref() == effective_dispatch_id
                && row.slot_index == effective_slot_index
                && row.completed_at.is_none()
        }
        ENTRY_STATUS_DONE | ENTRY_STATUS_SKIPPED => true,
        _ => false,
    }
}

fn maybe_finalize_run_after_terminal_entry(
    conn: &Connection,
    run_id: &str,
    new_status: &str,
) -> rusqlite::Result<bool> {
    // `done` completion is finalized by the policy-side OnCardTerminal flow so it
    // can always create or pass through a phase gate, even for single-phase runs.
    if new_status == ENTRY_STATUS_DONE {
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

    release_run_slots(conn, run_id)?;
    complete_run_on_conn(conn, run_id)
}

pub fn pause_run_on_conn(conn: &Connection, run_id: &str) -> rusqlite::Result<bool> {
    let updated = conn.execute(
        "UPDATE auto_queue_runs
         SET status = 'paused',
             completed_at = NULL
         WHERE id = ?1 AND status = 'active'",
        [run_id],
    )?;
    if updated > 0 {
        release_run_slots(conn, run_id)?;
    }
    Ok(updated > 0)
}

pub fn resume_run_on_conn(conn: &Connection, run_id: &str) -> rusqlite::Result<bool> {
    let updated = conn.execute(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE id = ?1 AND status = 'paused'",
        [run_id],
    )?;
    Ok(updated > 0)
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
        crate::auto_queue_log!(
            warn,
            "run_completion_notify_failed",
            crate::services::auto_queue::AutoQueueLogContext::new().run(run_id),
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
        ConsultationDispatchRecordError, ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_DONE,
        ENTRY_STATUS_PENDING, ENTRY_STATUS_SKIPPED, EntryStatusUpdateError,
        EntryStatusUpdateOptions, PhaseGateStateWrite, SlotAllocation, SlotAllocationError,
        allocate_slot_for_group_agent, clear_phase_gate_state_on_conn, list_entry_dispatch_history,
        reactivate_done_entry_on_conn, record_consultation_dispatch_on_conn, release_run_slots,
        release_slot_for_group_agent, save_phase_gate_state_on_conn, update_entry_status_on_conn,
    };
    use rusqlite::{Connection, OpenFlags};
    use std::sync::{Arc, Barrier};

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
            );
            CREATE TABLE task_dispatches (
                id TEXT PRIMARY KEY,
                to_agent_id TEXT,
                status TEXT,
                context TEXT
            );
            CREATE TABLE kanban_cards (
                id TEXT PRIMARY KEY,
                title TEXT,
                status TEXT,
                metadata TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE auto_queue_phase_gates (
                run_id TEXT NOT NULL,
                phase INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                verdict TEXT,
                dispatch_id TEXT UNIQUE,
                pass_verdict TEXT NOT NULL DEFAULT 'phase_gate_passed',
                next_phase INTEGER,
                final_phase INTEGER NOT NULL DEFAULT 0,
                anchor_card_id TEXT REFERENCES kanban_cards(id) ON DELETE SET NULL,
                failure_reason TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
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

    fn setup_shared_slot_conn() -> (tempfile::TempDir, String) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let path = tempdir.path().join("auto-queue-slot-test.sqlite");
        let path_str = path.to_string_lossy().to_string();
        let conn = Connection::open(&path_str).expect("slot db");
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE auto_queue_runs (
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
             CREATE TABLE auto_queue_slots (
                agent_id TEXT NOT NULL,
                slot_index INTEGER NOT NULL,
                assigned_run_id TEXT,
                assigned_thread_group INTEGER,
                thread_id_map TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (agent_id, slot_index)
             );",
        )
        .expect("shared schema");
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads
            ) VALUES (
                'run-shared', 'repo-1', 'agent-1', 'active', 1
            )",
            [],
        )
        .expect("seed shared run");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, thread_group
            ) VALUES (
                'entry-shared-0', 'run-shared', 'card-shared-0', 'agent-1', 'pending', 0
            )",
            [],
        )
        .expect("seed shared entry 0");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, thread_group
            ) VALUES (
                'entry-shared-1', 'run-shared', 'card-shared-1', 'agent-1', 'pending', 1
            )",
            [],
        )
        .expect("seed shared entry 1");
        drop(conn);
        (tempdir, path_str)
    }

    #[test]
    fn entry_transition_done_defers_run_completion_until_policy_hook() {
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
        assert_eq!(run_status, "active");

        let slot_run: Option<String> = conn
            .query_row(
                "SELECT assigned_run_id FROM auto_queue_slots WHERE agent_id = 'agent-1' AND slot_index = 0",
                [],
                |row| row.get(0),
            )
            .expect("slot row");
        assert_eq!(slot_run.as_deref(), Some("run-1"));

        let audit_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = 'entry-1'",
                [],
                |row| row.get(0),
            )
            .expect("audit count");
        assert_eq!(audit_rows, 2);

        let outbox_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
            .expect("outbox count");
        assert_eq!(
            outbox_count, 0,
            "done transition must wait for policy-side completion before notifying"
        );
    }

    #[test]
    fn entry_transition_done_keeps_slot_assignment_until_multi_phase_run_finishes() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group, batch_phase
             ) VALUES ('entry-phase-0', 'run-1', 'card-phase-0', 'agent-1', 'pending', NULL, 0, 0, 0)",
            [],
        )
        .expect("seed phase 0 entry");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase
             ) VALUES ('entry-phase-1', 'run-1', 'card-phase-1', 'agent-1', 'pending', 1, 1)",
            [],
        )
        .expect("seed phase 1 entry");

        update_entry_status_on_conn(
            &conn,
            "entry-phase-0",
            ENTRY_STATUS_DISPATCHED,
            "test_phase_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-phase-0".to_string()),
                slot_index: Some(0),
            },
        )
        .expect("dispatch phase 0 entry");
        update_entry_status_on_conn(
            &conn,
            "entry-phase-0",
            ENTRY_STATUS_DONE,
            "test_phase_done",
            &EntryStatusUpdateOptions::default(),
        )
        .expect("complete phase 0 entry");

        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("run status");
        assert_eq!(run_status, "active");

        let slot: (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT assigned_run_id, assigned_thread_group
                 FROM auto_queue_slots
                 WHERE agent_id = 'agent-1' AND slot_index = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("slot row");
        assert_eq!(slot.0.as_deref(), Some("run-1"));
        assert_eq!(slot.1, Some(0));
    }

    #[test]
    fn entry_transition_done_is_idempotent_without_duplicate_side_effects() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group
             ) VALUES ('entry-idempotent', 'run-1', 'card-idempotent', 'agent-1', 'dispatched', 'dispatch-idempotent', 0, 0)",
            [],
        )
        .expect("seed entry");

        let first = update_entry_status_on_conn(
            &conn,
            "entry-idempotent",
            ENTRY_STATUS_DONE,
            "test_done_first",
            &EntryStatusUpdateOptions::default(),
        )
        .expect("first completion");
        assert!(first.changed);

        let transition_count_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = 'entry-idempotent'",
                [],
                |row| row.get(0),
            )
            .expect("transition count before");
        let outbox_count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
            .expect("outbox count before");

        let second = update_entry_status_on_conn(
            &conn,
            "entry-idempotent",
            ENTRY_STATUS_DONE,
            "test_done_second",
            &EntryStatusUpdateOptions::default(),
        )
        .expect("second completion");
        assert!(
            !second.changed,
            "repeated terminal completion must become a no-op"
        );

        let transition_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = 'entry-idempotent'",
                [],
                |row| row.get(0),
            )
            .expect("transition count after");
        let outbox_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
            .expect("outbox count after");
        assert_eq!(
            transition_count_after, transition_count_before,
            "repeated completion must not append duplicate transition audit rows"
        );
        assert_eq!(
            outbox_count_after, outbox_count_before,
            "repeated completion must not emit duplicate completion notifications"
        );
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
    fn stale_allowed_transition_retries_from_latest_status() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-stale', 'run-1', 'card-stale', 'agent-1', 'pending', 0)",
            [],
        )
        .expect("seed entry");

        let stale_current = super::load_entry_status_row(&conn, "entry-stale").expect("stale row");
        conn.execute(
            "UPDATE auto_queue_entries
             SET status = 'dispatched',
                 dispatch_id = 'dispatch-live',
                 slot_index = 0,
                 dispatched_at = datetime('now')
             WHERE id = 'entry-stale'",
            [],
        )
        .expect("simulate concurrent dispatch");

        let result = super::update_entry_status_with_current_on_conn(
            &conn,
            "entry-stale",
            ENTRY_STATUS_SKIPPED,
            "test_cancel_retry",
            &EntryStatusUpdateOptions::default(),
            stale_current,
        )
        .expect("stale cancel should retry");
        assert!(result.changed);
        assert_eq!(result.from_status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(result.to_status, ENTRY_STATUS_SKIPPED);

        let (status, dispatch_id, completed_at): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, dispatch_id, completed_at FROM auto_queue_entries WHERE id = 'entry-stale'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("entry row");
        assert_eq!(status, ENTRY_STATUS_SKIPPED);
        assert!(dispatch_id.is_none());
        assert!(completed_at.is_some());

        let transition: (String, String) = conn
            .query_row(
                "SELECT from_status, to_status
                 FROM auto_queue_entry_transitions
                 WHERE entry_id = 'entry-stale'
                 ORDER BY id DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("transition row");
        assert_eq!(transition.0, ENTRY_STATUS_DISPATCHED);
        assert_eq!(transition.1, ENTRY_STATUS_SKIPPED);
    }

    #[test]
    fn entry_transition_allows_skipped_restore_to_dispatched() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-3', 'run-1', 'card-3', 'agent-1', 'skipped', 0)",
            [],
        )
        .expect("seed entry");

        let restored = update_entry_status_on_conn(
            &conn,
            "entry-3",
            ENTRY_STATUS_DISPATCHED,
            "test_restore_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-restored".to_string()),
                slot_index: Some(0),
            },
        )
        .expect("restore transition");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_SKIPPED);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        let (status, dispatch_id, slot_index): (String, Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT status, dispatch_id, slot_index FROM auto_queue_entries WHERE id = 'entry-3'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("entry row");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-restored"));
        assert_eq!(slot_index, Some(0));
    }

    #[test]
    fn entry_transition_allows_done_restore_to_dispatched_for_recovery_sources() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group, completed_at
             ) VALUES ('entry-3b', 'run-1', 'card-3b', 'agent-1', 'done', 0, datetime('now'))",
            [],
        )
        .expect("seed done entry");

        let restored = update_entry_status_on_conn(
            &conn,
            "entry-3b",
            ENTRY_STATUS_DISPATCHED,
            "rereview_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-rereview".to_string()),
                slot_index: Some(0),
            },
        )
        .expect("recovery transition");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_DONE);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        let (status, dispatch_id, slot_index, completed_at): (
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, dispatch_id, slot_index, completed_at
                 FROM auto_queue_entries
                 WHERE id = 'entry-3b'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("entry row");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-rereview"));
        assert_eq!(slot_index, Some(0));
        assert!(completed_at.is_none());
    }

    #[test]
    fn entry_transition_blocks_invalid_done_to_pending_restore() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-4', 'run-1', 'card-4', 'agent-1', 'done', 0)",
            [],
        )
        .expect("seed done entry");

        let error = update_entry_status_on_conn(
            &conn,
            "entry-4",
            ENTRY_STATUS_PENDING,
            "test_invalid",
            &EntryStatusUpdateOptions::default(),
        )
        .expect_err("invalid transition must fail");
        assert!(matches!(
            error,
            EntryStatusUpdateError::InvalidTransition { .. }
        ));
    }

    #[test]
    fn entry_transition_blocks_invalid_done_to_dispatched_restore() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-4b', 'run-1', 'card-4b', 'agent-1', 'done', 0)",
            [],
        )
        .expect("seed done entry");

        let error = update_entry_status_on_conn(
            &conn,
            "entry-4b",
            ENTRY_STATUS_DISPATCHED,
            "test_invalid_done_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-retry".to_string()),
                slot_index: Some(0),
            },
        )
        .expect_err("done -> dispatched transition must fail");
        assert!(matches!(
            error,
            EntryStatusUpdateError::InvalidTransition { .. }
        ));
    }

    #[test]
    fn entry_transition_blocks_invalid_done_to_skipped_restore() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-4c', 'run-1', 'card-4c', 'agent-1', 'done', 0)",
            [],
        )
        .expect("seed done entry");

        let error = update_entry_status_on_conn(
            &conn,
            "entry-4c",
            ENTRY_STATUS_SKIPPED,
            "test_invalid_done_skip",
            &EntryStatusUpdateOptions::default(),
        )
        .expect_err("done -> skipped transition must fail");
        assert!(matches!(
            error,
            EntryStatusUpdateError::InvalidTransition { .. }
        ));
    }

    #[test]
    fn reactivate_done_entry_allows_admin_restore_to_dispatched() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-reactivate', 'repo-1', 'agent-1', 'completed')",
            [],
        )
        .expect("seed run");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group, completed_at
             ) VALUES ('entry-reactivate', 'run-reactivate', 'card-reactivate', 'agent-1', 'done', 0, datetime('now'))",
            [],
        )
        .expect("seed done entry");

        let restored = reactivate_done_entry_on_conn(
            &conn,
            "entry-reactivate",
            "test_reactivate_done",
            &EntryStatusUpdateOptions::default(),
        )
        .expect("reactivate done entry");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_DONE);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        let (status, dispatch_id, completed_at): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, dispatch_id, completed_at
                 FROM auto_queue_entries
                 WHERE id = 'entry-reactivate'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("entry row");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert!(dispatch_id.is_none());
        assert!(completed_at.is_none());

        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-reactivate'",
                [],
                |row| row.get(0),
            )
            .expect("run row");
        assert_eq!(run_status, "active");
    }

    #[test]
    fn allocate_slot_for_group_agent_never_double_assigns_single_slot_under_concurrency() {
        let (_tempdir, path) = setup_shared_slot_conn();
        let barrier = Arc::new(Barrier::new(2));
        let make_worker = |group: i64| {
            let barrier = barrier.clone();
            let path = path.clone();
            std::thread::spawn(move || {
                let conn = Connection::open_with_flags(
                    &path,
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
                )
                .expect("open shared db");
                conn.busy_timeout(std::time::Duration::from_secs(2))
                    .expect("busy timeout");
                barrier.wait();
                allocate_slot_for_group_agent(&conn, "run-shared", group, "agent-1")
            })
        };

        let first_handle = make_worker(0);
        let second_handle = make_worker(1);
        let first = first_handle.join().unwrap().expect("first allocation");
        let second = second_handle.join().unwrap().expect("second allocation");

        let successful = [first, second].into_iter().flatten().collect::<Vec<_>>();
        assert_eq!(
            successful.len(),
            1,
            "single-slot pool must allow only one concurrent group allocation"
        );

        let conn = Connection::open(&path).expect("verify db");
        let slot_assignments: Vec<(Option<String>, Option<i64>)> = conn
            .prepare(
                "SELECT assigned_run_id, assigned_thread_group
                 FROM auto_queue_slots
                 WHERE agent_id = 'agent-1'
                 ORDER BY slot_index ASC",
            )
            .expect("slot stmt")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("slot rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect slot rows");
        assert_eq!(slot_assignments.len(), 1);
        assert_eq!(
            slot_assignments[0].0.as_deref(),
            Some("run-shared"),
            "the slot must remain assigned to exactly one run"
        );

        let slotted_entries: Vec<(String, Option<i64>)> = conn
            .prepare(
                "SELECT id, slot_index
                 FROM auto_queue_entries
                 ORDER BY id ASC",
            )
            .expect("entry stmt")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("entry rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect entry rows");
        assert_eq!(
            slotted_entries
                .iter()
                .filter(|(_, slot_index)| slot_index.is_some())
                .count(),
            1,
            "only one group entry must receive the single slot"
        );
    }

    #[test]
    fn allocate_slot_for_group_agent_rebinds_completed_same_run_slot_without_reset() {
        let conn = setup_conn();
        conn.execute(
            "UPDATE auto_queue_slots
             SET thread_id_map = '{\"123\":\"thread-slot-0\"}'
             WHERE agent_id = 'agent-1' AND slot_index = 0",
            [],
        )
        .expect("seed slot thread map");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, slot_index, thread_group, batch_phase, completed_at
             ) VALUES ('entry-complete', 'run-1', 'card-complete', 'agent-1', 'done', 0, 0, 0, datetime('now'))",
            [],
        )
        .expect("seed completed entry");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase
             ) VALUES ('entry-next', 'run-1', 'card-next', 'agent-1', 'pending', 1, 1)",
            [],
        )
        .expect("seed next phase entry");

        let allocation = allocate_slot_for_group_agent(&conn, "run-1", 1, "agent-1")
            .expect("same-run rebind must succeed");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 0,
                newly_assigned: false,
                reassigned_from_other_group: true,
            })
        );

        let slot: (Option<String>, Option<i64>, String) = conn
            .query_row(
                "SELECT assigned_run_id, assigned_thread_group, thread_id_map
                 FROM auto_queue_slots
                 WHERE agent_id = 'agent-1' AND slot_index = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("slot row");
        assert_eq!(slot.0.as_deref(), Some("run-1"));
        assert_eq!(slot.1, Some(1));
        assert_eq!(slot.2, "{\"123\":\"thread-slot-0\"}");

        let slot_index: Option<i64> = conn
            .query_row(
                "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-next'",
                [],
                |row| row.get(0),
            )
            .expect("next entry slot");
        assert_eq!(slot_index, Some(0));
    }

    #[test]
    fn allocate_slot_for_group_agent_marks_cross_run_reclaim_as_new_assignment() {
        let conn = setup_conn();
        conn.execute(
            "UPDATE auto_queue_slots
             SET thread_id_map = '{\"123\":\"thread-slot-0\"}'
             WHERE agent_id = 'agent-1' AND slot_index = 0",
            [],
        )
        .expect("seed slot thread map");
        release_run_slots(&conn, "run-1").expect("release first run slots");
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-2', 'repo-1', 'agent-1', 'active')",
            [],
        )
        .expect("seed second run");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-run-2', 'run-2', 'card-run-2', 'agent-1', 'pending', 0)",
            [],
        )
        .expect("seed second run entry");

        let allocation = allocate_slot_for_group_agent(&conn, "run-2", 0, "agent-1")
            .expect("cross-run claim must succeed");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 0,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );

        let slot: (Option<String>, Option<i64>, String) = conn
            .query_row(
                "SELECT assigned_run_id, assigned_thread_group, thread_id_map
                 FROM auto_queue_slots
                 WHERE agent_id = 'agent-1' AND slot_index = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("slot row");
        assert_eq!(slot.0.as_deref(), Some("run-2"));
        assert_eq!(slot.1, Some(0));
        assert_eq!(slot.2, "{\"123\":\"thread-slot-0\"}");
    }

    #[test]
    fn allocate_slot_for_group_agent_fails_after_bounded_cas_retries() {
        let conn = setup_conn();
        conn.execute(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL
             WHERE agent_id = 'agent-1' AND slot_index = 0",
            [],
        )
        .expect("free seed slot");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-cas-retry', 'run-1', 'card-cas-retry', 'agent-1', 'pending', 1)",
            [],
        )
        .expect("seed retry entry");
        conn.execute_batch(
            "CREATE TRIGGER ignore_slot_claim
             BEFORE UPDATE OF assigned_run_id ON auto_queue_slots
             WHEN NEW.assigned_run_id = 'run-1' AND OLD.assigned_run_id IS NULL
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .expect("create trigger");

        let error = allocate_slot_for_group_agent(&conn, "run-1", 1, "agent-1")
            .expect_err("forced claim race must terminate with bounded retry error");
        assert!(matches!(
            error,
            SlotAllocationError::RetryLimitExceeded { attempts, .. }
                if attempts == super::SLOT_ALLOCATION_MAX_RETRIES
        ));
    }

    #[test]
    fn release_slot_for_group_agent_clears_only_matching_assignment() {
        let conn = setup_conn();

        let released = release_slot_for_group_agent(&conn, "run-1", 0, "agent-1", 0)
            .expect("release matching slot");
        assert_eq!(released, 1);

        let slot: (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT assigned_run_id, assigned_thread_group
                 FROM auto_queue_slots
                 WHERE agent_id = 'agent-1' AND slot_index = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("slot row");
        assert_eq!(slot, (None, None));

        let released_again = release_slot_for_group_agent(&conn, "run-1", 0, "agent-1", 0)
            .expect("release already cleared slot");
        assert_eq!(released_again, 0);
    }

    #[test]
    fn terminal_transition_done_defers_slot_release_failures_until_policy_hook() {
        let conn = setup_conn();
        conn.execute_batch(
            "CREATE TRIGGER fail_slot_release
             BEFORE UPDATE OF assigned_run_id ON auto_queue_slots
             WHEN OLD.assigned_run_id IS NOT NULL AND NEW.assigned_run_id IS NULL
             BEGIN
                 SELECT RAISE(ABORT, 'slot release blocked');
             END;",
        )
        .expect("create trigger");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group
             ) VALUES ('entry-rollback', 'run-1', 'card-rollback', 'agent-1', 'pending', NULL, 0, 0)",
            [],
        )
        .expect("seed entry");

        update_entry_status_on_conn(
            &conn,
            "entry-rollback",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-rollback".to_string()),
                slot_index: Some(0),
            },
        )
        .expect("dispatch transition");

        update_entry_status_on_conn(
            &conn,
            "entry-rollback",
            ENTRY_STATUS_DONE,
            "test_done_rollback",
            &EntryStatusUpdateOptions::default(),
        )
        .expect("done transition should defer slot release until policy hook");

        let (status, dispatch_id, completed_at): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, dispatch_id, completed_at
                 FROM auto_queue_entries
                 WHERE id = 'entry-rollback'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("entry row");
        assert_eq!(status, ENTRY_STATUS_DONE);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-rollback"));
        assert!(completed_at.is_some());

        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("run status");
        assert_eq!(run_status, "active");

        let slot_run: Option<String> = conn
            .query_row(
                "SELECT assigned_run_id FROM auto_queue_slots WHERE agent_id = 'agent-1' AND slot_index = 0",
                [],
                |row| row.get(0),
            )
            .expect("slot row");
        assert_eq!(slot_run.as_deref(), Some("run-1"));

        let audit_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entry_transitions WHERE entry_id = 'entry-rollback'",
                [],
                |row| row.get(0),
            )
            .expect("audit count");
        assert_eq!(
            audit_rows, 2,
            "done transition audit must still be recorded"
        );
    }

    #[test]
    fn slot_has_active_dispatch_ignores_sidecar_dispatches() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES (?1, ?2, 'dispatched', ?3)",
            rusqlite::params![
                "dispatch-sidecar",
                "agent-1",
                serde_json::json!({
                    "slot_index": 0,
                    "sidecar_dispatch": true,
                    "phase_gate": {
                        "run_id": "run-1",
                    }
                })
                .to_string()
            ],
        )
        .expect("seed sidecar dispatch");

        assert!(
            !super::slot_has_active_dispatch(&conn, "agent-1", 0),
            "sidecar phase-gate dispatches must not keep a slot occupied"
        );

        conn.execute(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES (?1, ?2, 'dispatched', ?3)",
            rusqlite::params![
                "dispatch-primary",
                "agent-1",
                serde_json::json!({
                    "slot_index": 0
                })
                .to_string()
            ],
        )
        .expect("seed primary dispatch");

        assert!(
            super::slot_has_active_dispatch(&conn, "agent-1", 0),
            "primary dispatches must still block slot reuse"
        );
    }

    #[test]
    fn record_consultation_dispatch_preserves_metadata_and_marks_entry_dispatched() {
        let mut conn = setup_conn();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, metadata)
             VALUES ('card-consult', 'Card Consult', 'requested', ?1)",
            [serde_json::json!({
                "keep": "yes",
                "preflight_status": "consult_required"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                 id, run_id, kanban_card_id, agent_id, status, thread_group
             ) VALUES ('entry-consult', 'run-1', 'card-consult', 'agent-1', 'pending', 0)",
            [],
        )
        .unwrap();

        let result = record_consultation_dispatch_on_conn(
            &mut conn,
            "entry-consult",
            "card-consult",
            "dispatch-consult",
            "test_consultation_dispatch",
            r#"{"keep":"yes","preflight_status":"consult_required"}"#,
        )
        .unwrap();
        assert!(result.entry_status_changed);

        let metadata_raw: String = conn
            .query_row(
                "SELECT metadata FROM kanban_cards WHERE id = 'card-consult'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&metadata_raw).unwrap();
        assert_eq!(metadata["keep"], "yes");
        assert_eq!(metadata["preflight_status"], "consult_required");
        assert_eq!(metadata["consultation_status"], "pending");
        assert_eq!(metadata["consultation_dispatch_id"], "dispatch-consult");

        let (status, dispatch_id): (String, Option<String>) = conn
            .query_row(
                "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-consult'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-consult"));
    }

    #[test]
    fn record_consultation_dispatch_requires_dispatch_id() {
        let mut conn = setup_conn();
        let error = record_consultation_dispatch_on_conn(
            &mut conn,
            "entry-missing",
            "card-missing",
            "   ",
            "test_consultation_dispatch",
            "{}",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConsultationDispatchRecordError::MissingDispatchId
        ));
    }

    #[test]
    fn save_phase_gate_state_filters_invalid_dispatches_and_removes_stale_rows() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-valid-1', 'agent-1', 'dispatched', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-valid-2', 'agent-1', 'dispatched', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-stale', 'agent-1', 'dispatched', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES ('run-1', 2, 'pending', 'dispatch-stale', 'phase_gate_passed')",
            [],
        )
        .unwrap();

        let result = save_phase_gate_state_on_conn(
            &conn,
            "run-1",
            2,
            &PhaseGateStateWrite {
                status: "failed".to_string(),
                verdict: Some("deploy_failed".to_string()),
                dispatch_ids: vec![
                    "dispatch-valid-1".to_string(),
                    "dispatch-valid-1".to_string(),
                    "dispatch-missing".to_string(),
                    "dispatch-valid-2".to_string(),
                ],
                pass_verdict: "phase_gate_passed".to_string(),
                next_phase: Some(3),
                final_phase: true,
                anchor_card_id: None,
                failure_reason: Some("deploy-dev failed".to_string()),
                created_at: Some("2026-04-15 00:00:00".to_string()),
            },
        )
        .unwrap();

        assert_eq!(
            result.persisted_dispatch_ids,
            vec![
                "dispatch-valid-1".to_string(),
                "dispatch-valid-2".to_string()
            ]
        );
        assert_eq!(result.removed_stale_rows, 1);

        let mut stmt = conn
            .prepare(
                "SELECT dispatch_id, status, verdict, next_phase, final_phase, failure_reason
                 FROM auto_queue_phase_gates
                 WHERE run_id = ?1 AND phase = ?2
                 ORDER BY dispatch_id ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params!["run-1", 2], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.as_deref(), Some("dispatch-valid-1"));
        assert_eq!(rows[1].0.as_deref(), Some("dispatch-valid-2"));
        assert_eq!(rows[0].1, "failed");
        assert_eq!(rows[0].2.as_deref(), Some("deploy_failed"));
        assert_eq!(rows[0].3, Some(3));
        assert_eq!(rows[0].4, 1);
        assert_eq!(rows[0].5.as_deref(), Some("deploy-dev failed"));
    }

    #[test]
    fn clear_phase_gate_state_removes_phase_rows() {
        let conn = setup_conn();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES ('run-1', 2, 'pending', NULL, 'phase_gate_passed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES ('run-1', 3, 'pending', NULL, 'phase_gate_passed')",
            [],
        )
        .unwrap();

        assert!(clear_phase_gate_state_on_conn(&conn, "run-1", 2).unwrap());

        let phase_two_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_phase_gates WHERE run_id = ?1 AND phase = ?2",
                rusqlite::params!["run-1", 2],
                |row| row.get(0),
            )
            .unwrap();
        let phase_three_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_phase_gates WHERE run_id = ?1 AND phase = ?2",
                rusqlite::params!["run-1", 3],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(phase_two_count, 0);
        assert_eq!(phase_three_count, 1);
    }
}
