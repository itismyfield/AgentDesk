use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use clap::Args;
use libsql_rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use sqlx::{PgPool, Row};

use crate::config::Config;
use crate::utils::format::expand_tilde_path;

#[derive(Clone, Debug, Args)]
pub struct PostgresCutoverArgs {
    /// Preview counts and blockers without writing files or importing into PostgreSQL
    #[arg(long)]
    pub dry_run: bool,
    /// Optional directory for JSONL archive snapshots
    #[arg(long = "archive-dir", value_name = "PATH")]
    pub archive_dir: Option<String>,
    /// Skip PostgreSQL import and only report/export the SQLite history
    #[arg(long)]
    pub skip_pg_import: bool,
    /// Acknowledge and proceed even when SQLite still has unsent message_outbox
    /// rows. By default cutover refuses so Discord messages are not silently
    /// dropped — pass this only after confirming the pending rows are known
    /// stale and will not need to be re-delivered.
    #[arg(long = "allow-unsent-messages")]
    pub allow_unsent_messages: bool,
}

#[derive(Debug, Default, Serialize)]
struct SqliteCutoverCounts {
    audit_logs: i64,
    session_transcripts: i64,
    active_dispatches: i64,
    working_sessions: i64,
    open_dispatch_outbox: i64,
    /// Unsent rows in `message_outbox` (status = 'pending'). These are Discord
    /// messages enqueued by the policy engine that have not yet been
    /// delivered, so cutover would silently drop them if ignored.
    pending_message_outbox: i64,
}

impl SqliteCutoverCounts {
    fn has_live_state(&self) -> bool {
        self.active_dispatches > 0
            || self.working_sessions > 0
            || self.open_dispatch_outbox > 0
            || self.pending_message_outbox > 0
    }
}

#[derive(Debug, Default, Serialize)]
struct PgCutoverCounts {
    audit_logs: i64,
    session_transcripts: i64,
    active_dispatches: i64,
    working_sessions: i64,
    open_dispatch_outbox: i64,
    pending_message_outbox: i64,
}

#[derive(Debug, Default, Serialize)]
struct ArchiveOutput {
    directory: String,
    audit_logs_file: Option<String>,
    session_transcripts_file: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct ImportSummary {
    agents_inserted: i64,
    cards_upserted: i64,
    task_dispatches_upserted: i64,
    sessions_upserted: i64,
    dispatch_outbox_upserted: i64,
    audit_logs_inserted: i64,
    session_transcripts_upserted: i64,
}

#[derive(Debug, Default, Serialize)]
struct PostgresCutoverReport {
    ok: bool,
    sqlite: SqliteCutoverCounts,
    postgres_before: Option<PgCutoverCounts>,
    postgres_after: Option<PgCutoverCounts>,
    archive: Option<ArchiveOutput>,
    imported: Option<ImportSummary>,
    blocker: Option<String>,
}

#[derive(Debug, Default)]
struct SqliteCutoverSnapshot {
    counts: SqliteCutoverCounts,
    audit_logs: Vec<AuditLogRow>,
    session_transcripts: Vec<SessionTranscriptRow>,
    task_dispatches: Vec<TaskDispatchRow>,
    sessions: Vec<SessionRow>,
    dispatch_outbox: Vec<DispatchOutboxRow>,
    referenced_cards: Vec<KanbanCardRow>,
    referenced_agents: Vec<AgentRow>,
}

#[derive(Debug, Clone, Serialize)]
struct AuditLogRow {
    entity_type: Option<String>,
    entity_id: Option<String>,
    action: Option<String>,
    timestamp: Option<String>,
    actor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionTranscriptRow {
    turn_id: String,
    session_key: Option<String>,
    channel_id: Option<String>,
    agent_id: Option<String>,
    provider: Option<String>,
    dispatch_id: Option<String>,
    user_message: String,
    assistant_message: String,
    events_json: String,
    duration_ms: Option<i64>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentRow {
    id: String,
    name: String,
    name_ko: Option<String>,
    department: Option<String>,
    provider: Option<String>,
    discord_channel_id: Option<String>,
    discord_channel_alt: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
    avatar_emoji: Option<String>,
    status: Option<String>,
    xp: Option<i64>,
    skills: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct KanbanCardRow {
    id: String,
    repo_id: Option<String>,
    title: String,
    status: Option<String>,
    priority: Option<String>,
    assigned_agent_id: Option<String>,
    github_issue_url: Option<String>,
    github_issue_number: Option<i64>,
    latest_dispatch_id: Option<String>,
    review_round: Option<i64>,
    metadata: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    blocked_reason: Option<String>,
    pipeline_stage_id: Option<String>,
    review_notes: Option<String>,
    review_status: Option<String>,
    requested_at: Option<String>,
    owner_agent_id: Option<String>,
    requester_agent_id: Option<String>,
    parent_card_id: Option<String>,
    depth: Option<i64>,
    sort_order: Option<i64>,
    description: Option<String>,
    active_thread_id: Option<String>,
    channel_thread_map: Option<String>,
    suggestion_pending_at: Option<String>,
    review_entered_at: Option<String>,
    awaiting_dod_at: Option<String>,
    deferred_dod_json: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct TaskDispatchRow {
    id: String,
    kanban_card_id: Option<String>,
    from_agent_id: Option<String>,
    to_agent_id: Option<String>,
    dispatch_type: Option<String>,
    status: Option<String>,
    title: Option<String>,
    context: Option<String>,
    result: Option<String>,
    parent_dispatch_id: Option<String>,
    chain_depth: Option<i64>,
    thread_id: Option<String>,
    retry_count: Option<i64>,
    created_at: Option<String>,
    updated_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionRow {
    session_key: String,
    agent_id: Option<String>,
    provider: Option<String>,
    status: Option<String>,
    active_dispatch_id: Option<String>,
    model: Option<String>,
    session_info: Option<String>,
    tokens: Option<i64>,
    cwd: Option<String>,
    last_heartbeat: Option<String>,
    thread_channel_id: Option<String>,
    claude_session_id: Option<String>,
    raw_provider_session_id: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Clone)]
struct DispatchOutboxRow {
    id: i64,
    dispatch_id: String,
    action: String,
    agent_id: Option<String>,
    card_id: Option<String>,
    title: Option<String>,
    status: String,
    retry_count: Option<i64>,
    next_attempt_at: Option<String>,
    created_at: Option<String>,
    processed_at: Option<String>,
    error: Option<String>,
}

pub async fn cmd_migrate_postgres_cutover(args: PostgresCutoverArgs) -> Result<(), String> {
    if !args.dry_run && args.skip_pg_import && args.archive_dir.is_none() {
        return Err(
            "postgres-cutover needs at least one action: omit --skip-pg-import or pass --archive-dir"
                .to_string(),
        );
    }

    let config = load_effective_config()?;
    let need_history_rows = args.archive_dir.is_some() || !args.skip_pg_import;
    let need_live_rows = !args.skip_pg_import;
    let pg_pool = if args.skip_pg_import {
        None
    } else {
        Some(connect_postgres_for_cutover(&config).await?)
    };
    let sqlite_path = config.data.dir.join(&config.data.db_name);
    let sqlite = if !args.dry_run && !args.skip_pg_import {
        crate::db::open_write_connection(&sqlite_path)
    } else {
        crate::db::open_read_only_connection(&sqlite_path)
    }
    .map_err(|e| {
        format!(
            "open sqlite cutover connection {}: {e}",
            sqlite_path.display()
        )
    })?;
    let barrier_active = if !args.dry_run && !args.skip_pg_import {
        sqlite
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| format!("acquire sqlite cutover write barrier: {e}"))?;
        true
    } else {
        false
    };
    let result: Result<PostgresCutoverReport, String> = async {
        let snapshot = load_sqlite_cutover_snapshot(&sqlite, need_history_rows, need_live_rows)?;

        let pg_before = if let Some(pool) = pg_pool.as_ref() {
            Some(load_pg_cutover_counts(pool).await?)
        } else {
            None
        };

        let mut report = PostgresCutoverReport {
            ok: false,
            sqlite: snapshot.counts,
            postgres_before: pg_before,
            postgres_after: None,
            archive: None,
            imported: None,
            blocker: None,
        };

        report.blocker = cutover_blocker(&args, &report.sqlite);

        if args.dry_run || report.blocker.is_some() {
            report.ok = report.blocker.is_none();
            return Ok(report);
        }

        if let Some(dir) = args.archive_dir.as_deref() {
            report.archive = Some(write_archive_files(
                dir,
                &snapshot.audit_logs,
                &snapshot.session_transcripts,
            )?);
        }

        if let Some(pool) = pg_pool.as_ref() {
            let mut import_summary = import_live_state_into_pg(
                pool,
                &snapshot.referenced_agents,
                &snapshot.referenced_cards,
                &snapshot.task_dispatches,
                &snapshot.sessions,
                &snapshot.dispatch_outbox,
            )
            .await?;
            let history_summary =
                import_history_into_pg(pool, &snapshot.audit_logs, &snapshot.session_transcripts)
                    .await?;
            import_summary.audit_logs_inserted = history_summary.audit_logs_inserted;
            import_summary.session_transcripts_upserted =
                history_summary.session_transcripts_upserted;
            report.imported = Some(import_summary);
            report.postgres_after = Some(load_pg_cutover_counts(pool).await?);
        }

        report.ok = report.blocker.is_none();
        Ok(report)
    }
    .await;

    if barrier_active {
        sqlite
            .execute_batch("ROLLBACK")
            .map_err(|e| format!("release sqlite cutover write barrier: {e}"))?;
    }

    let report = result?;
    print_report(&report)?;
    if let Some(blocker) = report.blocker {
        return Err(blocker);
    }
    Ok(())
}

fn cutover_blocker(
    args: &PostgresCutoverArgs,
    sqlite_counts: &SqliteCutoverCounts,
) -> Option<String> {
    if args.skip_pg_import && sqlite_counts.has_live_state() {
        return Some(
            "sqlite still has in-flight dispatch/session/outbox/message state; archive-only cutover would lose it. Omit --skip-pg-import or drain runtime to idle first."
                .to_string(),
        );
    }

    if !args.skip_pg_import && sqlite_counts.open_dispatch_outbox > 0 {
        return Some(
            "sqlite still has open dispatch_outbox rows; drain outbox before PG cutover to avoid duplicate delivery."
                .to_string(),
        );
    }

    if !args.skip_pg_import
        && sqlite_counts.pending_message_outbox > 0
        && !args.allow_unsent_messages
    {
        return Some(format!(
            "sqlite still has {count} pending message_outbox row(s); these Discord messages would be lost on cutover. \
Drain by letting the message-outbox worker settle (restart dcserver if it is stalled) or pass --allow-unsent-messages \
after confirming the rows are stale and safe to drop.",
            count = sqlite_counts.pending_message_outbox,
        ));
    }

    None
}

fn load_sqlite_cutover_snapshot(
    sqlite: &Connection,
    need_history_rows: bool,
    need_live_rows: bool,
) -> Result<SqliteCutoverSnapshot, String> {
    let counts = sqlite_cutover_counts(sqlite)?;
    let audit_logs = if need_history_rows {
        load_audit_logs(sqlite)?
    } else {
        Vec::new()
    };
    let session_transcripts = if need_history_rows {
        load_session_transcripts(sqlite)?
    } else {
        Vec::new()
    };
    let task_dispatches = if need_live_rows {
        load_active_task_dispatches(sqlite)?
    } else {
        Vec::new()
    };
    let sessions = if need_live_rows {
        load_live_sessions(sqlite)?
    } else {
        Vec::new()
    };
    let dispatch_outbox = if need_live_rows {
        load_open_dispatch_outbox(sqlite)?
    } else {
        Vec::new()
    };
    let referenced_cards = if need_live_rows {
        load_referenced_kanban_cards(
            sqlite,
            &referenced_card_ids(&task_dispatches, &dispatch_outbox),
        )?
    } else {
        Vec::new()
    };
    let referenced_agents = if need_live_rows {
        load_referenced_agents(
            sqlite,
            &referenced_agent_ids(
                &task_dispatches,
                &sessions,
                &dispatch_outbox,
                &referenced_cards,
            ),
        )?
    } else {
        Vec::new()
    };

    Ok(SqliteCutoverSnapshot {
        counts,
        audit_logs,
        session_transcripts,
        task_dispatches,
        sessions,
        dispatch_outbox,
        referenced_cards,
        referenced_agents,
    })
}

fn print_report(report: &PostgresCutoverReport) -> Result<(), String> {
    let rendered = serde_json::to_string_pretty(report)
        .map_err(|e| format!("serialize postgres cutover report: {e}"))?;
    println!("{rendered}");
    Ok(())
}

fn load_effective_config() -> Result<Config, String> {
    if let Some(root) = crate::config::runtime_root() {
        return crate::services::discord::config_audit::load_runtime_config(&root)
            .map(|loaded| loaded.config)
            .map_err(|e| format!("load runtime config: {e}"));
    }

    crate::config::load().map_err(|e| format!("load config: {e}"))
}

async fn connect_postgres_for_cutover(config: &Config) -> Result<PgPool, String> {
    crate::db::postgres::connect_and_migrate(config)
        .await
        .and_then(|pool| {
            pool.ok_or_else(|| {
                "postgres is disabled; enable config.database or set DATABASE_URL before cutover"
                    .to_string()
            })
        })
}

fn sqlite_cutover_counts(conn: &Connection) -> Result<SqliteCutoverCounts, String> {
    Ok(SqliteCutoverCounts {
        audit_logs: query_count(conn, "SELECT COUNT(*) FROM audit_logs")?,
        session_transcripts: query_count(conn, "SELECT COUNT(*) FROM session_transcripts")?,
        active_dispatches: query_count(
            conn,
            "SELECT COUNT(*) FROM task_dispatches WHERE status IN ('pending', 'dispatched')",
        )?,
        working_sessions: query_count(
            conn,
            "SELECT COUNT(*) FROM sessions WHERE status = 'working'",
        )?,
        open_dispatch_outbox: query_count(
            conn,
            "SELECT COUNT(*) FROM dispatch_outbox WHERE status <> 'done' OR processed_at IS NULL",
        )?,
        // SQLite message_outbox uses status = 'pending' for queued-but-unsent
        // rows; once delivered the worker flips to 'sent' (success) or
        // 'failed' (permanent). 'failed' rows are not retried, so they do not
        // need to block cutover — only true pending entries do.
        pending_message_outbox: query_count(
            conn,
            "SELECT COUNT(*) FROM message_outbox WHERE status = 'pending'",
        )?,
    })
}

fn query_count(conn: &Connection, sql: &str) -> Result<i64, String> {
    conn.query_row(sql, [], |row| row.get(0))
        .map_err(|e| format!("sqlite count query failed: {e}"))
}

fn load_audit_logs(conn: &Connection) -> Result<Vec<AuditLogRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT entity_type, entity_id, action, timestamp, actor
             FROM audit_logs
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare audit_logs export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AuditLogRow {
                entity_type: row.get(0)?,
                entity_id: row.get(1)?,
                action: row.get(2)?,
                timestamp: row.get(3)?,
                actor: row.get(4)?,
            })
        })
        .map_err(|e| format!("query audit_logs export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect audit_logs export: {e}"))
}

fn load_session_transcripts(conn: &Connection) -> Result<Vec<SessionTranscriptRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT turn_id,
                    session_key,
                    channel_id,
                    agent_id,
                    provider,
                    dispatch_id,
                    user_message,
                    assistant_message,
                    COALESCE(events_json, '[]') AS events_json,
                    duration_ms,
                    created_at
             FROM session_transcripts
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare session_transcripts export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionTranscriptRow {
                turn_id: row.get(0)?,
                session_key: row.get(1)?,
                channel_id: row.get(2)?,
                agent_id: row.get(3)?,
                provider: row.get(4)?,
                dispatch_id: row.get(5)?,
                user_message: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                assistant_message: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                events_json: row
                    .get::<_, Option<String>>(8)?
                    .unwrap_or_else(|| "[]".to_string()),
                duration_ms: row.get(9)?,
                created_at: row.get(10)?,
            })
        })
        .map_err(|e| format!("query session_transcripts export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect session_transcripts export: {e}"))
}

fn load_active_task_dispatches(conn: &Connection) -> Result<Vec<TaskDispatchRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    kanban_card_id,
                    from_agent_id,
                    to_agent_id,
                    dispatch_type,
                    status,
                    title,
                    context,
                    result,
                    parent_dispatch_id,
                    chain_depth,
                    thread_id,
                    retry_count,
                    created_at,
                    updated_at,
                    completed_at
             FROM task_dispatches
             WHERE status IN ('pending', 'dispatched')
             ORDER BY created_at ASC, id ASC",
        )
        .map_err(|e| format!("prepare task_dispatches export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(TaskDispatchRow {
                id: row.get(0)?,
                kanban_card_id: row.get(1)?,
                from_agent_id: row.get(2)?,
                to_agent_id: row.get(3)?,
                dispatch_type: row.get(4)?,
                status: row.get(5)?,
                title: row.get(6)?,
                context: row.get(7)?,
                result: row.get(8)?,
                parent_dispatch_id: row.get(9)?,
                chain_depth: row.get(10)?,
                thread_id: row.get(11)?,
                retry_count: row.get(12)?,
                created_at: row.get(13)?,
                updated_at: row.get(14)?,
                completed_at: row.get(15)?,
            })
        })
        .map_err(|e| format!("query task_dispatches export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect task_dispatches export: {e}"))
}

fn load_live_sessions(conn: &Connection) -> Result<Vec<SessionRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT session_key,
                    agent_id,
                    provider,
                    status,
                    active_dispatch_id,
                    model,
                    session_info,
                    tokens,
                    cwd,
                    last_heartbeat,
                    thread_channel_id,
                    claude_session_id,
                    raw_provider_session_id,
                    created_at
             FROM sessions
             WHERE status = 'working' OR active_dispatch_id IS NOT NULL
             ORDER BY created_at ASC, session_key ASC",
        )
        .map_err(|e| format!("prepare sessions export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionRow {
                session_key: row.get(0)?,
                agent_id: row.get(1)?,
                provider: row.get(2)?,
                status: row.get(3)?,
                active_dispatch_id: row.get(4)?,
                model: row.get(5)?,
                session_info: row.get(6)?,
                tokens: row.get(7)?,
                cwd: row.get(8)?,
                last_heartbeat: row.get(9)?,
                thread_channel_id: row.get(10)?,
                claude_session_id: row.get(11)?,
                raw_provider_session_id: row.get(12)?,
                created_at: row.get(13)?,
            })
        })
        .map_err(|e| format!("query sessions export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect sessions export: {e}"))
}

fn load_open_dispatch_outbox(conn: &Connection) -> Result<Vec<DispatchOutboxRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    dispatch_id,
                    action,
                    agent_id,
                    card_id,
                    title,
                    status,
                    retry_count,
                    next_attempt_at,
                    created_at,
                    processed_at,
                    error
             FROM dispatch_outbox
             WHERE status <> 'done' OR processed_at IS NULL
             ORDER BY id ASC",
        )
        .map_err(|e| format!("prepare dispatch_outbox export: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DispatchOutboxRow {
                id: row.get(0)?,
                dispatch_id: row.get(1)?,
                action: row.get(2)?,
                agent_id: row.get(3)?,
                card_id: row.get(4)?,
                title: row.get(5)?,
                status: row
                    .get::<_, Option<String>>(6)?
                    .unwrap_or_else(|| "pending".to_string()),
                retry_count: row.get(7)?,
                next_attempt_at: row.get(8)?,
                created_at: row.get(9)?,
                processed_at: row.get(10)?,
                error: row.get(11)?,
            })
        })
        .map_err(|e| format!("query dispatch_outbox export: {e}"))?;
    rows.collect::<libsql_rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect dispatch_outbox export: {e}"))
}

fn referenced_card_ids(
    task_dispatches: &[TaskDispatchRow],
    dispatch_outbox: &[DispatchOutboxRow],
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for row in task_dispatches {
        if let Some(card_id) = row.kanban_card_id.as_deref().map(str::trim) {
            if !card_id.is_empty() {
                ids.insert(card_id.to_string());
            }
        }
    }
    for row in dispatch_outbox {
        if let Some(card_id) = row.card_id.as_deref().map(str::trim) {
            if !card_id.is_empty() {
                ids.insert(card_id.to_string());
            }
        }
    }
    ids.into_iter().collect()
}

fn load_referenced_kanban_cards(
    conn: &Connection,
    card_ids: &[String],
) -> Result<Vec<KanbanCardRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    repo_id,
                    title,
                    status,
                    priority,
                    assigned_agent_id,
                    github_issue_url,
                    github_issue_number,
                    latest_dispatch_id,
                    review_round,
                    metadata,
                    started_at,
                    completed_at,
                    blocked_reason,
                    pipeline_stage_id,
                    review_notes,
                    review_status,
                    requested_at,
                    owner_agent_id,
                    requester_agent_id,
                    parent_card_id,
                    depth,
                    sort_order,
                    description,
                    active_thread_id,
                    channel_thread_map,
                    suggestion_pending_at,
                    review_entered_at,
                    awaiting_dod_at,
                    deferred_dod_json,
                    created_at,
                    updated_at
             FROM kanban_cards
             WHERE id = ?1",
        )
        .map_err(|e| format!("prepare kanban_cards export: {e}"))?;

    let mut rows = Vec::with_capacity(card_ids.len());
    for card_id in card_ids {
        if let Some(row) = stmt
            .query_row([card_id], |row| {
                Ok(KanbanCardRow {
                    id: row.get(0)?,
                    repo_id: row.get(1)?,
                    title: row.get(2)?,
                    status: row.get(3)?,
                    priority: row.get(4)?,
                    assigned_agent_id: row.get(5)?,
                    github_issue_url: row.get(6)?,
                    github_issue_number: row.get(7)?,
                    latest_dispatch_id: row.get(8)?,
                    review_round: row.get(9)?,
                    metadata: normalize_optional_json(row.get(10)?),
                    started_at: row.get(11)?,
                    completed_at: row.get(12)?,
                    blocked_reason: row.get(13)?,
                    pipeline_stage_id: row.get(14)?,
                    review_notes: row.get(15)?,
                    review_status: row.get(16)?,
                    requested_at: row.get(17)?,
                    owner_agent_id: row.get(18)?,
                    requester_agent_id: row.get(19)?,
                    parent_card_id: row.get(20)?,
                    depth: row.get(21)?,
                    sort_order: row.get(22)?,
                    description: row.get(23)?,
                    active_thread_id: row.get(24)?,
                    channel_thread_map: normalize_optional_json(row.get(25)?),
                    suggestion_pending_at: row.get(26)?,
                    review_entered_at: row.get(27)?,
                    awaiting_dod_at: row.get(28)?,
                    deferred_dod_json: normalize_optional_json(row.get(29)?),
                    created_at: row.get(30)?,
                    updated_at: row.get(31)?,
                })
            })
            .optional()
            .map_err(|e| format!("load kanban card {card_id}: {e}"))?
        {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn referenced_agent_ids(
    task_dispatches: &[TaskDispatchRow],
    sessions: &[SessionRow],
    dispatch_outbox: &[DispatchOutboxRow],
    cards: &[KanbanCardRow],
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for row in task_dispatches {
        for agent_id in [&row.from_agent_id, &row.to_agent_id] {
            if let Some(agent_id) = agent_id.as_deref().map(str::trim) {
                if !agent_id.is_empty() {
                    ids.insert(agent_id.to_string());
                }
            }
        }
    }
    for row in sessions {
        if let Some(agent_id) = row.agent_id.as_deref().map(str::trim) {
            if !agent_id.is_empty() {
                ids.insert(agent_id.to_string());
            }
        }
    }
    for row in dispatch_outbox {
        if let Some(agent_id) = row.agent_id.as_deref().map(str::trim) {
            if !agent_id.is_empty() {
                ids.insert(agent_id.to_string());
            }
        }
    }
    for row in cards {
        if let Some(agent_id) = row.assigned_agent_id.as_deref().map(str::trim) {
            if !agent_id.is_empty() {
                ids.insert(agent_id.to_string());
            }
        }
    }
    ids.into_iter().collect()
}

fn load_referenced_agents(
    conn: &Connection,
    agent_ids: &[String],
) -> Result<Vec<AgentRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id,
                    name,
                    name_ko,
                    department,
                    provider,
                    discord_channel_id,
                    discord_channel_alt,
                    discord_channel_cc,
                    discord_channel_cdx,
                    avatar_emoji,
                    status,
                    xp,
                    skills,
                    created_at,
                    updated_at
             FROM agents
             WHERE id = ?1",
        )
        .map_err(|e| format!("prepare agents export: {e}"))?;

    let mut rows = Vec::with_capacity(agent_ids.len());
    for agent_id in agent_ids {
        if let Some(row) = stmt
            .query_row([agent_id], |row| {
                Ok(AgentRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    name_ko: row.get(2)?,
                    department: row.get(3)?,
                    provider: row.get(4)?,
                    discord_channel_id: row.get(5)?,
                    discord_channel_alt: row.get(6)?,
                    discord_channel_cc: row.get(7)?,
                    discord_channel_cdx: row.get(8)?,
                    avatar_emoji: row.get(9)?,
                    status: row.get(10)?,
                    xp: row.get(11)?,
                    skills: row.get(12)?,
                    created_at: row.get(13)?,
                    updated_at: row.get(14)?,
                })
            })
            .optional()
            .map_err(|e| format!("load agent {agent_id}: {e}"))?
        {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn normalize_optional_json(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn write_archive_files(
    archive_dir: &str,
    audit_logs: &[AuditLogRow],
    session_transcripts: &[SessionTranscriptRow],
) -> Result<ArchiveOutput, String> {
    let dir = normalize_archive_dir(archive_dir)?;
    fs::create_dir_all(&dir).map_err(|e| format!("create archive dir {}: {e}", dir.display()))?;

    let audit_path = dir.join("audit_logs.jsonl");
    let transcript_path = dir.join("session_transcripts.jsonl");
    write_jsonl(&audit_path, audit_logs)?;
    write_jsonl(&transcript_path, session_transcripts)?;

    Ok(ArchiveOutput {
        directory: dir.display().to_string(),
        audit_logs_file: Some(audit_path.display().to_string()),
        session_transcripts_file: Some(transcript_path.display().to_string()),
    })
}

fn normalize_archive_dir(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("archive dir cannot be empty".to_string());
    }
    let expanded = expand_tilde_path(trimmed);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(expanded))
            .map_err(|e| format!("resolve archive dir: {e}"))
    }
}

fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for row in rows {
        let line =
            serde_json::to_string(row).map_err(|e| format!("serialize {}: {e}", path.display()))?;
        writer
            .write_all(line.as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    writer
        .flush()
        .map_err(|e| format!("flush {}: {e}", path.display()))
}

async fn load_pg_cutover_counts(pool: &PgPool) -> Result<PgCutoverCounts, String> {
    let audit_logs = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_logs")
        .fetch_one(pool)
        .await
        .map_err(|e| format!("count postgres audit_logs: {e}"))?;
    let session_transcripts =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM session_transcripts")
            .fetch_one(pool)
            .await
            .map_err(|e| format!("count postgres session_transcripts: {e}"))?;
    let active_dispatches = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM task_dispatches WHERE status IN ('pending', 'dispatched')",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count postgres active task_dispatches: {e}"))?;
    let working_sessions = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sessions WHERE status = 'working' OR active_dispatch_id IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count postgres live sessions: {e}"))?;
    let open_dispatch_outbox = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM dispatch_outbox WHERE status <> 'done' OR processed_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count postgres open dispatch_outbox: {e}"))?;
    // PG message_outbox additionally uses 'processing' while a worker is mid-
    // delivery (claimed_at set). Treat anything other than terminal states
    // ('sent'/'failed') as still in flight so the report mirrors the SQLite
    // surface for operators verifying drain.
    let pending_message_outbox = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM message_outbox WHERE status NOT IN ('sent', 'failed')",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("count postgres pending message_outbox: {e}"))?;
    Ok(PgCutoverCounts {
        audit_logs,
        session_transcripts,
        active_dispatches,
        working_sessions,
        open_dispatch_outbox,
        pending_message_outbox,
    })
}

async fn import_live_state_into_pg(
    pool: &PgPool,
    agents: &[AgentRow],
    cards: &[KanbanCardRow],
    task_dispatches: &[TaskDispatchRow],
    sessions: &[SessionRow],
    dispatch_outbox: &[DispatchOutboxRow],
) -> Result<ImportSummary, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres live-state transaction: {e}"))?;

    let mut inserted_agents = 0i64;
    for row in agents {
        let result = sqlx::query(
            "INSERT INTO agents (
                id,
                name,
                name_ko,
                department,
                provider,
                discord_channel_id,
                discord_channel_alt,
                discord_channel_cc,
                discord_channel_cdx,
                avatar_emoji,
                status,
                xp,
                skills,
                created_at,
                updated_at
             )
             VALUES (
                $1,
                $2,
                $3,
                $4,
                COALESCE($5, 'claude'),
                $6,
                $7,
                $8,
                $9,
                $10,
                COALESCE($11, 'idle'),
                COALESCE($12, 0),
                $13,
                COALESCE(CAST($14 AS timestamptz), NOW()),
                COALESCE(CAST($15 AS timestamptz), NOW())
             )
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&row.id)
        .bind(&row.name)
        .bind(&row.name_ko)
        .bind(&row.department)
        .bind(&row.provider)
        .bind(&row.discord_channel_id)
        .bind(&row.discord_channel_alt)
        .bind(&row.discord_channel_cc)
        .bind(&row.discord_channel_cdx)
        .bind(&row.avatar_emoji)
        .bind(&row.status)
        .bind(row.xp)
        .bind(&row.skills)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres agent {}: {e}", row.id))?;
        inserted_agents += result.rows_affected() as i64;
    }

    let mut upserted_cards = 0i64;
    for row in cards {
        let result = sqlx::query(
            "INSERT INTO kanban_cards (
                id,
                repo_id,
                title,
                status,
                priority,
                assigned_agent_id,
                github_issue_url,
                github_issue_number,
                latest_dispatch_id,
                review_round,
                metadata,
                started_at,
                completed_at,
                blocked_reason,
                pipeline_stage_id,
                review_notes,
                review_status,
                requested_at,
                owner_agent_id,
                requester_agent_id,
                parent_card_id,
                depth,
                sort_order,
                description,
                active_thread_id,
                channel_thread_map,
                suggestion_pending_at,
                review_entered_at,
                awaiting_dod_at,
                deferred_dod_json,
                created_at,
                updated_at
             )
             VALUES (
                $1,
                $2,
                $3,
                COALESCE($4, 'backlog'),
                COALESCE($5, 'medium'),
                $6,
                $7,
                $8,
                $9,
                COALESCE($10, 0),
                CAST($11 AS jsonb),
                CAST($12 AS timestamptz),
                CAST($13 AS timestamptz),
                $14,
                $15,
                $16,
                $17,
                CAST($18 AS timestamptz),
                $19,
                $20,
                $21,
                COALESCE($22, 0),
                COALESCE($23, 0),
                $24,
                $25,
                CAST($26 AS jsonb),
                CAST($27 AS timestamptz),
                CAST($28 AS timestamptz),
                CAST($29 AS timestamptz),
                CAST($30 AS jsonb),
                COALESCE(CAST($31 AS timestamptz), NOW()),
                COALESCE(CAST($32 AS timestamptz), NOW())
             )
             ON CONFLICT (id) DO UPDATE
             SET repo_id = EXCLUDED.repo_id,
                 title = EXCLUDED.title,
                 status = EXCLUDED.status,
                 priority = EXCLUDED.priority,
                 assigned_agent_id = EXCLUDED.assigned_agent_id,
                 github_issue_url = EXCLUDED.github_issue_url,
                 github_issue_number = EXCLUDED.github_issue_number,
                 latest_dispatch_id = EXCLUDED.latest_dispatch_id,
                 review_round = EXCLUDED.review_round,
                 metadata = EXCLUDED.metadata,
                 started_at = EXCLUDED.started_at,
                 completed_at = EXCLUDED.completed_at,
                 blocked_reason = EXCLUDED.blocked_reason,
                 pipeline_stage_id = EXCLUDED.pipeline_stage_id,
                 review_notes = EXCLUDED.review_notes,
                 review_status = EXCLUDED.review_status,
                 requested_at = EXCLUDED.requested_at,
                 owner_agent_id = EXCLUDED.owner_agent_id,
                 requester_agent_id = EXCLUDED.requester_agent_id,
                 parent_card_id = EXCLUDED.parent_card_id,
                 depth = EXCLUDED.depth,
                 sort_order = EXCLUDED.sort_order,
                 description = EXCLUDED.description,
                 active_thread_id = EXCLUDED.active_thread_id,
                 channel_thread_map = EXCLUDED.channel_thread_map,
                 suggestion_pending_at = EXCLUDED.suggestion_pending_at,
                 review_entered_at = EXCLUDED.review_entered_at,
                 awaiting_dod_at = EXCLUDED.awaiting_dod_at,
                 deferred_dod_json = EXCLUDED.deferred_dod_json,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.id)
        .bind(&row.repo_id)
        .bind(&row.title)
        .bind(&row.status)
        .bind(&row.priority)
        .bind(&row.assigned_agent_id)
        .bind(&row.github_issue_url)
        .bind(row.github_issue_number)
        .bind(&row.latest_dispatch_id)
        .bind(row.review_round)
        .bind(&row.metadata)
        .bind(&row.started_at)
        .bind(&row.completed_at)
        .bind(&row.blocked_reason)
        .bind(&row.pipeline_stage_id)
        .bind(&row.review_notes)
        .bind(&row.review_status)
        .bind(&row.requested_at)
        .bind(&row.owner_agent_id)
        .bind(&row.requester_agent_id)
        .bind(&row.parent_card_id)
        .bind(row.depth)
        .bind(row.sort_order)
        .bind(&row.description)
        .bind(&row.active_thread_id)
        .bind(&row.channel_thread_map)
        .bind(&row.suggestion_pending_at)
        .bind(&row.review_entered_at)
        .bind(&row.awaiting_dod_at)
        .bind(&row.deferred_dod_json)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres kanban_card {}: {e}", row.id))?;
        upserted_cards += result.rows_affected() as i64;
    }

    let mut upserted_dispatches = 0i64;
    for row in task_dispatches {
        let result = sqlx::query(
            "INSERT INTO task_dispatches (
                id,
                kanban_card_id,
                from_agent_id,
                to_agent_id,
                dispatch_type,
                status,
                title,
                context,
                result,
                parent_dispatch_id,
                chain_depth,
                thread_id,
                retry_count,
                created_at,
                updated_at,
                completed_at
             )
             VALUES (
                $1,
                $2,
                $3,
                $4,
                $5,
                $6,
                $7,
                $8,
                $9,
                $10,
                COALESCE($11, 0),
                $12,
                COALESCE($13, 0),
                COALESCE(CAST($14 AS timestamptz), NOW()),
                COALESCE(CAST($15 AS timestamptz), NOW()),
                CAST($16 AS timestamptz)
             )
             ON CONFLICT (id) DO UPDATE
             SET kanban_card_id = EXCLUDED.kanban_card_id,
                 from_agent_id = EXCLUDED.from_agent_id,
                 to_agent_id = EXCLUDED.to_agent_id,
                 dispatch_type = EXCLUDED.dispatch_type,
                 status = EXCLUDED.status,
                 title = EXCLUDED.title,
                 context = EXCLUDED.context,
                 result = EXCLUDED.result,
                 parent_dispatch_id = EXCLUDED.parent_dispatch_id,
                 chain_depth = EXCLUDED.chain_depth,
                 thread_id = EXCLUDED.thread_id,
                 retry_count = EXCLUDED.retry_count,
                 created_at = EXCLUDED.created_at,
                 updated_at = EXCLUDED.updated_at,
                 completed_at = EXCLUDED.completed_at",
        )
        .bind(&row.id)
        .bind(&row.kanban_card_id)
        .bind(&row.from_agent_id)
        .bind(&row.to_agent_id)
        .bind(&row.dispatch_type)
        .bind(&row.status)
        .bind(&row.title)
        .bind(&row.context)
        .bind(&row.result)
        .bind(&row.parent_dispatch_id)
        .bind(row.chain_depth)
        .bind(&row.thread_id)
        .bind(row.retry_count)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .bind(&row.completed_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres task_dispatches {}: {e}", row.id))?;
        upserted_dispatches += result.rows_affected() as i64;
    }

    let mut upserted_sessions = 0i64;
    for row in sessions {
        let result = sqlx::query(
            "INSERT INTO sessions (
                session_key,
                agent_id,
                provider,
                status,
                active_dispatch_id,
                model,
                session_info,
                tokens,
                cwd,
                last_heartbeat,
                thread_channel_id,
                claude_session_id,
                raw_provider_session_id,
                created_at
             )
             VALUES (
                $1,
                $2,
                COALESCE($3, 'claude'),
                COALESCE($4, 'disconnected'),
                $5,
                $6,
                $7,
                COALESCE($8, 0),
                $9,
                CAST($10 AS timestamptz),
                $11,
                $12,
                $13,
                COALESCE(CAST($14 AS timestamptz), NOW())
             )
             ON CONFLICT (session_key) DO UPDATE
             SET agent_id = EXCLUDED.agent_id,
                 provider = EXCLUDED.provider,
                 status = EXCLUDED.status,
                 active_dispatch_id = EXCLUDED.active_dispatch_id,
                 model = EXCLUDED.model,
                 session_info = EXCLUDED.session_info,
                 tokens = EXCLUDED.tokens,
                 cwd = EXCLUDED.cwd,
                 last_heartbeat = EXCLUDED.last_heartbeat,
                 thread_channel_id = EXCLUDED.thread_channel_id,
                 claude_session_id = EXCLUDED.claude_session_id,
                 raw_provider_session_id = EXCLUDED.raw_provider_session_id",
        )
        .bind(&row.session_key)
        .bind(&row.agent_id)
        .bind(&row.provider)
        .bind(&row.status)
        .bind(&row.active_dispatch_id)
        .bind(&row.model)
        .bind(&row.session_info)
        .bind(row.tokens)
        .bind(&row.cwd)
        .bind(&row.last_heartbeat)
        .bind(&row.thread_channel_id)
        .bind(&row.claude_session_id)
        .bind(&row.raw_provider_session_id)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres sessions {}: {e}", row.session_key))?;
        upserted_sessions += result.rows_affected() as i64;
    }

    let mut upserted_outbox = 0i64;
    for row in dispatch_outbox {
        let result = sqlx::query(
            "INSERT INTO dispatch_outbox (
                id,
                dispatch_id,
                action,
                agent_id,
                card_id,
                title,
                status,
                retry_count,
                next_attempt_at,
                created_at,
                processed_at,
                error
             )
             VALUES (
                $1,
                $2,
                $3,
                $4,
                $5,
                $6,
                $7,
                COALESCE($8, 0),
                CAST($9 AS timestamptz),
                COALESCE(CAST($10 AS timestamptz), NOW()),
                CAST($11 AS timestamptz),
                $12
             )
             ON CONFLICT (id) DO UPDATE
             SET dispatch_id = EXCLUDED.dispatch_id,
                 action = EXCLUDED.action,
                 agent_id = EXCLUDED.agent_id,
                 card_id = EXCLUDED.card_id,
                 title = EXCLUDED.title,
                 status = EXCLUDED.status,
                 retry_count = EXCLUDED.retry_count,
                 next_attempt_at = EXCLUDED.next_attempt_at,
                 created_at = EXCLUDED.created_at,
                 processed_at = EXCLUDED.processed_at,
                 error = EXCLUDED.error",
        )
        .bind(row.id)
        .bind(&row.dispatch_id)
        .bind(&row.action)
        .bind(&row.agent_id)
        .bind(&row.card_id)
        .bind(&row.title)
        .bind(&row.status)
        .bind(row.retry_count)
        .bind(&row.next_attempt_at)
        .bind(&row.created_at)
        .bind(&row.processed_at)
        .bind(&row.error)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres dispatch_outbox {}: {e}", row.id))?;
        upserted_outbox += result.rows_affected() as i64;
    }

    advance_pg_serial_sequences(&mut tx).await?;

    tx.commit()
        .await
        .map_err(|e| format!("commit postgres live-state transaction: {e}"))?;

    Ok(ImportSummary {
        agents_inserted: inserted_agents,
        cards_upserted: upserted_cards,
        task_dispatches_upserted: upserted_dispatches,
        sessions_upserted: upserted_sessions,
        dispatch_outbox_upserted: upserted_outbox,
        audit_logs_inserted: 0,
        session_transcripts_upserted: 0,
    })
}

async fn advance_pg_serial_sequences(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), String> {
    let serial_columns = sqlx::query(
        "SELECT table_name, column_name
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND data_type IN ('bigint', 'integer')
           AND (
                column_default LIKE 'nextval(%'
                OR is_identity = 'YES'
           )
         ORDER BY table_name, ordinal_position",
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| format!("list postgres serial columns: {e}"))?;

    for column in serial_columns {
        let table_name = column
            .try_get::<String, _>("table_name")
            .map_err(|e| format!("decode postgres serial table name: {e}"))?;
        let column_name = column
            .try_get::<String, _>("column_name")
            .map_err(|e| format!("decode postgres serial column name: {e}"))?;

        let sequence_name =
            sqlx::query_scalar::<_, Option<String>>("SELECT pg_get_serial_sequence($1, $2)")
                .bind(format!("public.{table_name}"))
                .bind(&column_name)
                .fetch_one(&mut **tx)
                .await
                .map_err(|e| {
                    format!("resolve postgres serial sequence for {table_name}.{column_name}: {e}")
                })?;

        let Some(sequence_name) = sequence_name else {
            continue;
        };

        let quoted_table = quote_ident(&table_name);
        let quoted_column = quote_ident(&column_name);
        let max_query = format!(
            "SELECT COALESCE(MAX({quoted_column}), 0)::BIGINT AS max_id FROM public.{quoted_table}"
        );
        let max_id = sqlx::query_scalar::<_, i64>(&max_query)
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| format!("load max id for {table_name}.{column_name}: {e}"))?;

        sqlx::query("SELECT setval($1, $2, $3)")
            .bind(&sequence_name)
            .bind(if max_id > 0 { max_id } else { 1 })
            .bind(max_id > 0)
            .execute(&mut **tx)
            .await
            .map_err(|e| {
                format!(
                    "advance postgres serial sequence {sequence_name} for {table_name}.{column_name}: {e}"
                )
            })?;
    }

    Ok(())
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn import_history_into_pg(
    pool: &PgPool,
    audit_logs: &[AuditLogRow],
    session_transcripts: &[SessionTranscriptRow],
) -> Result<ImportSummary, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("begin postgres cutover transaction: {e}"))?;

    let mut inserted_audit_logs = 0i64;
    for row in audit_logs {
        let result = sqlx::query(
            "INSERT INTO audit_logs (entity_type, entity_id, action, timestamp, actor)
             SELECT $1, $2, $3, COALESCE(CAST($4 AS timestamptz), NOW()), $5
             WHERE NOT EXISTS (
                 SELECT 1
                   FROM audit_logs
                  WHERE entity_type IS NOT DISTINCT FROM $1
                    AND entity_id IS NOT DISTINCT FROM $2
                    AND action IS NOT DISTINCT FROM $3
                    AND actor IS NOT DISTINCT FROM $5
                    AND timestamp = COALESCE(CAST($4 AS timestamptz), NOW())
             )",
        )
        .bind(&row.entity_type)
        .bind(&row.entity_id)
        .bind(&row.action)
        .bind(&row.timestamp)
        .bind(&row.actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres audit_logs: {e}"))?;
        inserted_audit_logs += result.rows_affected() as i64;
    }

    let mut upserted_session_transcripts = 0i64;
    for row in session_transcripts {
        let result = sqlx::query(
            "INSERT INTO session_transcripts (
                turn_id,
                session_key,
                channel_id,
                agent_id,
                provider,
                dispatch_id,
                user_message,
                assistant_message,
                events_json,
                duration_ms,
                created_at
             )
             VALUES (
                $1,
                $2,
                $3,
                $4,
                $5,
                $6,
                $7,
                $8,
                CAST($9 AS jsonb),
                $10,
                COALESCE(CAST($11 AS timestamptz), NOW())
             )
             ON CONFLICT (turn_id) DO UPDATE
             SET session_key = EXCLUDED.session_key,
                 channel_id = EXCLUDED.channel_id,
                 agent_id = COALESCE(EXCLUDED.agent_id, session_transcripts.agent_id),
                 provider = EXCLUDED.provider,
                 dispatch_id = EXCLUDED.dispatch_id,
                 user_message = EXCLUDED.user_message,
                 assistant_message = EXCLUDED.assistant_message,
                 events_json = EXCLUDED.events_json,
                 duration_ms = EXCLUDED.duration_ms",
        )
        .bind(&row.turn_id)
        .bind(&row.session_key)
        .bind(&row.channel_id)
        .bind(&row.agent_id)
        .bind(&row.provider)
        .bind(&row.dispatch_id)
        .bind(&row.user_message)
        .bind(&row.assistant_message)
        .bind(&row.events_json)
        .bind(row.duration_ms)
        .bind(&row.created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("import postgres session_transcripts {}: {e}", row.turn_id))?;
        upserted_session_transcripts += result.rows_affected() as i64;
    }

    tx.commit()
        .await
        .map_err(|e| format!("commit postgres cutover transaction: {e}"))?;

    Ok(ImportSummary {
        agents_inserted: 0,
        cards_upserted: 0,
        task_dispatches_upserted: 0,
        sessions_upserted: 0,
        dispatch_outbox_upserted: 0,
        audit_logs_inserted: inserted_audit_logs,
        session_transcripts_upserted: upserted_session_transcripts,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AgentRow, AuditLogRow, DispatchOutboxRow, KanbanCardRow, PostgresCutoverArgs, SessionRow,
        SessionTranscriptRow, SqliteCutoverCounts, TaskDispatchRow, advance_pg_serial_sequences,
        cutover_blocker, import_history_into_pg, import_live_state_into_pg, load_pg_cutover_counts,
        load_session_transcripts, load_sqlite_cutover_snapshot, sqlite_cutover_counts,
        write_archive_files,
    };
    use libsql_rusqlite::Connection;
    use sqlx::{PgPool, Row};
    use std::path::Path;
    use tempfile::TempDir;

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = admin_database_url();
            let database_name = format!("agentdesk_cutover_{}", uuid::Uuid::new_v4().simple());
            let admin_pool = PgPool::connect(&admin_url)
                .await
                .expect("connect postgres admin db");
            sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
                .expect("create postgres test db");
            admin_pool.close().await;
            Self {
                admin_url,
                database_name,
            }
        }

        async fn connect_and_migrate(&self) -> PgPool {
            let pool = PgPool::connect(&format!("{}/{}", base_database_url(), self.database_name))
                .await
                .expect("connect postgres test db");
            crate::db::postgres::migrate(&pool)
                .await
                .expect("migrate postgres test db");
            pool
        }

        async fn drop(self) {
            let admin_pool = PgPool::connect(&self.admin_url)
                .await
                .expect("reconnect postgres admin db");
            sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await
            .expect("terminate postgres test db sessions");
            sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await
            .expect("drop postgres test db");
            admin_pool.close().await;
        }
    }

    fn base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", base_database_url(), admin_db)
    }

    #[test]
    fn sqlite_cutover_counts_detects_live_state() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        conn.execute(
            "INSERT INTO task_dispatches (id, status) VALUES ('dispatch-cutover', 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, status) VALUES ('session-cutover', 'working')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status) VALUES ('dispatch-cutover', 'notify', 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source, status) VALUES ('thread-cutover', 'hello', 'announce', 'system', 'pending')",
            [],
        )
        .unwrap();
        // Already-delivered and permanently-failed rows must not inflate the
        // pending counter — only true unsent rows should block cutover.
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source, status) VALUES ('thread-cutover', 'sent already', 'announce', 'system', 'sent')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source, status) VALUES ('thread-cutover', 'failed perm', 'announce', 'system', 'failed')",
            [],
        )
        .unwrap();

        let counts = sqlite_cutover_counts(&conn).expect("count sqlite cutover state");
        assert_eq!(counts.active_dispatches, 1);
        assert_eq!(counts.working_sessions, 1);
        assert_eq!(counts.open_dispatch_outbox, 1);
        assert_eq!(counts.pending_message_outbox, 1);
        assert!(counts.has_live_state());
    }

    #[test]
    fn archive_only_cutover_blocks_when_live_state_exists() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: false,
        };
        let counts = SqliteCutoverCounts {
            active_dispatches: 1,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts).expect("live state blocker");
        assert!(blocker.contains("archive-only cutover would lose it"));
    }

    #[test]
    fn archive_only_cutover_allows_idle_sqlite() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: false,
        };

        assert!(cutover_blocker(&args, &SqliteCutoverCounts::default()).is_none());
    }

    #[test]
    fn archive_only_cutover_blocks_when_only_pending_messages_remain() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: false,
        };
        let counts = SqliteCutoverCounts {
            pending_message_outbox: 3,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts).expect("message_outbox blocker");
        assert!(blocker.contains("archive-only cutover would lose it"));
    }

    #[test]
    fn pg_cutover_blocks_when_dispatch_outbox_is_not_drained() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: false,
        };
        let counts = SqliteCutoverCounts {
            open_dispatch_outbox: 1,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts).expect("dispatch_outbox blocker");
        assert!(blocker.contains("drain outbox"));
    }

    #[test]
    fn pg_cutover_blocks_when_message_outbox_has_pending_rows() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: false,
        };
        let counts = SqliteCutoverCounts {
            pending_message_outbox: 4,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts).expect("message_outbox blocker");
        assert!(
            blocker.contains("4 pending message_outbox row(s)"),
            "operator-facing blocker should surface the pending count, got: {blocker}"
        );
        assert!(
            blocker.contains("--allow-unsent-messages"),
            "blocker should advertise the opt-out flag, got: {blocker}"
        );
    }

    #[test]
    fn pg_cutover_dry_run_blocks_message_outbox_same_as_real_run() {
        // Dry-run must show the same blocker so operators see the gate before
        // attempting a real cutover.
        let dry_args = PostgresCutoverArgs {
            dry_run: true,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: false,
        };
        let real_args = PostgresCutoverArgs {
            dry_run: false,
            ..dry_args.clone()
        };
        let counts = SqliteCutoverCounts {
            pending_message_outbox: 2,
            ..Default::default()
        };

        let dry_blocker = cutover_blocker(&dry_args, &counts).expect("dry-run blocker");
        let real_blocker = cutover_blocker(&real_args, &counts).expect("real-run blocker");
        assert_eq!(dry_blocker, real_blocker);
    }

    #[test]
    fn pg_cutover_proceeds_when_operator_acknowledges_unsent_messages() {
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: true,
        };
        let counts = SqliteCutoverCounts {
            pending_message_outbox: 7,
            ..Default::default()
        };

        assert!(
            cutover_blocker(&args, &counts).is_none(),
            "--allow-unsent-messages must release the message_outbox blocker"
        );
    }

    #[test]
    fn archive_only_cutover_still_blocks_pending_messages_even_with_override() {
        // --allow-unsent-messages is for the PG-import path. Archive-only
        // cutover would still drop the messages because there is no PG to
        // carry them over to, so we keep blocking via has_live_state().
        let args = PostgresCutoverArgs {
            dry_run: false,
            archive_dir: Some("/tmp/cutover-archive".to_string()),
            skip_pg_import: true,
            allow_unsent_messages: true,
        };
        let counts = SqliteCutoverCounts {
            pending_message_outbox: 1,
            ..Default::default()
        };

        let blocker = cutover_blocker(&args, &counts).expect("archive-only ignores the override");
        assert!(blocker.contains("archive-only cutover would lose it"));
    }

    #[test]
    fn load_sqlite_cutover_snapshot_preserves_live_references() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        crate::db::schema::migrate(&conn).expect("sqlite migrate");
        conn.execute(
            "INSERT INTO agents (id, name, provider, status) VALUES ('project-agentdesk', 'AgentDesk', 'codex', 'idle')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id) VALUES ('card-cutover', 'Cutover card', 'in_progress', 'project-agentdesk')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, from_agent_id, to_agent_id, status) VALUES ('dispatch-cutover', 'card-cutover', 'project-agentdesk', 'project-agentdesk', 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id) VALUES ('session-cutover', 'project-agentdesk', 'working', 'dispatch-cutover')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status, agent_id, card_id) VALUES ('dispatch-cutover', 'notify', 'pending', 'project-agentdesk', 'card-cutover')",
            [],
        )
        .unwrap();

        let snapshot = load_sqlite_cutover_snapshot(&conn, false, true).expect("sqlite snapshot");
        assert_eq!(snapshot.counts.active_dispatches, 1);
        assert_eq!(snapshot.counts.working_sessions, 1);
        assert_eq!(snapshot.counts.open_dispatch_outbox, 1);
        assert_eq!(snapshot.task_dispatches.len(), 1);
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.dispatch_outbox.len(), 1);
        assert_eq!(snapshot.referenced_cards.len(), 1);
        assert_eq!(snapshot.referenced_agents.len(), 1);
        assert_eq!(snapshot.referenced_cards[0].id, "card-cutover");
        assert_eq!(snapshot.referenced_agents[0].id, "project-agentdesk");
    }

    #[tokio::test]
    async fn import_live_state_into_pg_copies_active_dispatches_sessions_and_outbox() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let agents = vec![AgentRow {
            id: "project-agentdesk".to_string(),
            name: "AgentDesk".to_string(),
            name_ko: Some("에이전트데스크".to_string()),
            department: Some("platform".to_string()),
            provider: Some("codex".to_string()),
            discord_channel_id: Some("123456789".to_string()),
            discord_channel_alt: Some("987654321".to_string()),
            discord_channel_cc: Some("123456789".to_string()),
            discord_channel_cdx: Some("987654321".to_string()),
            avatar_emoji: Some(":robot:".to_string()),
            status: Some("idle".to_string()),
            xp: Some(42),
            skills: Some("[\"postgres-cutover\"]".to_string()),
            created_at: Some("2026-04-18 09:59:59".to_string()),
            updated_at: Some("2026-04-18 10:00:00".to_string()),
        }];
        let cards = vec![KanbanCardRow {
            id: "card-cutover-live".to_string(),
            repo_id: Some("itismyfield/AgentDesk".to_string()),
            title: "Carry in-flight cutover card".to_string(),
            status: Some("in_progress".to_string()),
            priority: Some("high".to_string()),
            assigned_agent_id: Some("project-agentdesk".to_string()),
            github_issue_url: Some(
                "https://github.com/itismyfield/AgentDesk/issues/479".to_string(),
            ),
            github_issue_number: Some(479),
            latest_dispatch_id: Some("dispatch-cutover-live".to_string()),
            review_round: Some(0),
            metadata: Some("{\"cutover\":true}".to_string()),
            started_at: Some("2026-04-18 10:00:00".to_string()),
            completed_at: None,
            blocked_reason: None,
            pipeline_stage_id: Some("pg-cutover".to_string()),
            review_notes: None,
            review_status: None,
            requested_at: Some("2026-04-18 10:00:00".to_string()),
            owner_agent_id: Some("project-agentdesk".to_string()),
            requester_agent_id: Some("project-agentdesk".to_string()),
            parent_card_id: None,
            depth: Some(0),
            sort_order: Some(0),
            description: Some("Preserve the live card during PG cutover".to_string()),
            active_thread_id: Some("thread-123".to_string()),
            channel_thread_map: Some("{\"primary\":\"thread-123\"}".to_string()),
            suggestion_pending_at: None,
            review_entered_at: None,
            awaiting_dod_at: None,
            deferred_dod_json: None,
            created_at: Some("2026-04-18 10:00:00".to_string()),
            updated_at: Some("2026-04-18 10:00:01".to_string()),
        }];
        let dispatches = vec![TaskDispatchRow {
            id: "dispatch-cutover-live".to_string(),
            kanban_card_id: Some("card-cutover-live".to_string()),
            from_agent_id: Some("project-agentdesk".to_string()),
            to_agent_id: Some("project-agentdesk".to_string()),
            dispatch_type: Some("implementation".to_string()),
            status: Some("dispatched".to_string()),
            title: Some("Carry in-flight dispatch".to_string()),
            context: Some("{\"cutover\":true}".to_string()),
            result: None,
            parent_dispatch_id: None,
            chain_depth: Some(0),
            thread_id: Some("thread-123".to_string()),
            retry_count: Some(1),
            created_at: Some("2026-04-18 10:00:00".to_string()),
            updated_at: Some("2026-04-18 10:00:01".to_string()),
            completed_at: None,
        }];
        let sessions = vec![SessionRow {
            session_key: "codex/live-cutover".to_string(),
            agent_id: Some("project-agentdesk".to_string()),
            provider: Some("codex".to_string()),
            status: Some("working".to_string()),
            active_dispatch_id: Some("dispatch-cutover-live".to_string()),
            model: Some("gpt-5-codex".to_string()),
            session_info: Some("{\"source\":\"cutover\"}".to_string()),
            tokens: Some(321),
            cwd: Some("/tmp/agentdesk".to_string()),
            last_heartbeat: Some("2026-04-18 10:00:02".to_string()),
            thread_channel_id: Some("123456789".to_string()),
            claude_session_id: None,
            raw_provider_session_id: Some("provider-session-1".to_string()),
            created_at: Some("2026-04-18 10:00:00".to_string()),
        }];
        let outbox = vec![DispatchOutboxRow {
            id: 42,
            dispatch_id: "dispatch-cutover-live".to_string(),
            action: "notify".to_string(),
            agent_id: Some("project-agentdesk".to_string()),
            card_id: Some("card-cutover-live".to_string()),
            title: Some("Carry in-flight outbox".to_string()),
            status: "pending".to_string(),
            retry_count: Some(2),
            next_attempt_at: Some("2026-04-18 10:00:03".to_string()),
            created_at: Some("2026-04-18 10:00:00".to_string()),
            processed_at: None,
            error: None,
        }];

        let summary =
            import_live_state_into_pg(&pool, &agents, &cards, &dispatches, &sessions, &outbox)
                .await
                .expect("import live state");
        assert_eq!(summary.agents_inserted, 1);
        assert_eq!(summary.cards_upserted, 1);
        assert_eq!(summary.task_dispatches_upserted, 1);
        assert_eq!(summary.sessions_upserted, 1);
        assert_eq!(summary.dispatch_outbox_upserted, 1);

        let counts = load_pg_cutover_counts(&pool)
            .await
            .expect("pg cutover counts");
        assert_eq!(counts.active_dispatches, 1);
        assert_eq!(counts.working_sessions, 1);
        assert_eq!(counts.open_dispatch_outbox, 1);
        assert_eq!(
            counts.pending_message_outbox, 0,
            "no message_outbox rows seeded yet — count must be zero"
        );

        // #767 regression guard: PG-side pending_message_outbox must reflect
        // any non-terminal message_outbox rows so post-import audits surface
        // drain progress. Seed pending + processing rows alongside terminal
        // rows and assert only the non-terminal ones contribute.
        sqlx::query(
            "INSERT INTO message_outbox (target, content, bot, source, status)
             VALUES ('thread-pending', 'pending body', 'announce', 'test', 'pending'),
                    ('thread-processing', 'mid-flight body', 'announce', 'test', 'processing'),
                    ('thread-sent', 'already delivered', 'announce', 'test', 'sent'),
                    ('thread-failed', 'permanent failure', 'announce', 'test', 'failed')",
        )
        .execute(&pool)
        .await
        .expect("seed message_outbox rows for pending count");
        let counts_after_seed = load_pg_cutover_counts(&pool)
            .await
            .expect("pg cutover counts after message_outbox seed");
        assert_eq!(
            counts_after_seed.pending_message_outbox, 2,
            "PG count must include 'pending' + 'processing' but exclude 'sent' / 'failed'"
        );

        let session = sqlx::query(
            "SELECT status, active_dispatch_id, raw_provider_session_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind("codex/live-cutover")
        .fetch_one(&pool)
        .await
        .expect("load imported session");
        assert_eq!(session.get::<String, _>("status"), "working");
        assert_eq!(
            session
                .get::<Option<String>, _>("active_dispatch_id")
                .as_deref(),
            Some("dispatch-cutover-live")
        );
        assert_eq!(
            session
                .get::<Option<String>, _>("raw_provider_session_id")
                .as_deref(),
            Some("provider-session-1")
        );

        let card = sqlx::query(
            "SELECT status, assigned_agent_id, latest_dispatch_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind("card-cutover-live")
        .fetch_one(&pool)
        .await
        .expect("load imported card");
        assert_eq!(card.get::<String, _>("status"), "in_progress");
        assert_eq!(
            card.get::<Option<String>, _>("assigned_agent_id")
                .as_deref(),
            Some("project-agentdesk")
        );
        assert_eq!(
            card.get::<Option<String>, _>("latest_dispatch_id")
                .as_deref(),
            Some("dispatch-cutover-live")
        );

        let second =
            import_live_state_into_pg(&pool, &agents, &cards, &dispatches, &sessions, &outbox)
                .await
                .expect("re-import live state");
        assert_eq!(second.agents_inserted, 0);

        let outbox_row = sqlx::query(
            "SELECT status, retry_count
             FROM dispatch_outbox
             WHERE id = 42",
        )
        .fetch_one(&pool)
        .await
        .expect("load imported outbox");
        assert_eq!(outbox_row.get::<String, _>("status"), "pending");
        assert_eq!(outbox_row.get::<i32, _>("retry_count"), 2);

        let next_outbox_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO dispatch_outbox (dispatch_id, action, status)
             VALUES ('dispatch-cutover-next', 'notify', 'pending')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert next outbox row after sequence advance");
        assert_eq!(next_outbox_id, 43);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn advance_pg_serial_sequences_updates_all_bigserial_tables() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO message_outbox (id, target, content, bot, source, status)
             VALUES (41, 'thread-1', 'hello', 'announce', 'test', 'pending')",
        )
        .execute(&pool)
        .await
        .expect("seed message_outbox");

        let mut tx = pool.begin().await.expect("begin sequence advance tx");
        advance_pg_serial_sequences(&mut tx)
            .await
            .expect("advance all serial sequences");
        tx.commit().await.expect("commit sequence advance tx");

        let next_message_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO message_outbox (target, content, bot, source)
             VALUES ('thread-2', 'world', 'announce', 'test')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("insert next message_outbox row");
        assert_eq!(next_message_id, 42);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_cutover_schema_includes_pr_tracking_create_pr_support() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let columns = sqlx::query_scalar::<_, String>(
            "SELECT column_name
             FROM information_schema.columns
             WHERE table_schema = 'public'
               AND table_name = 'pr_tracking'
               AND column_name IN ('dispatch_generation', 'review_round', 'retry_count')
             ORDER BY column_name",
        )
        .fetch_all(&pool)
        .await
        .expect("load pr_tracking columns");
        assert_eq!(
            columns,
            vec![
                "dispatch_generation".to_string(),
                "retry_count".to_string(),
                "review_round".to_string(),
            ]
        );

        let has_index = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                SELECT 1
                FROM pg_indexes
                WHERE schemaname = 'public'
                  AND tablename = 'task_dispatches'
                  AND indexname = 'idx_single_active_create_pr'
             )",
        )
        .fetch_one(&pool)
        .await
        .expect("check create-pr partial index");
        assert!(has_index);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn import_history_into_pg_is_idempotent() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let audit_logs = vec![AuditLogRow {
            entity_type: Some("card".to_string()),
            entity_id: Some("card-1".to_string()),
            action: Some("advance".to_string()),
            timestamp: Some("2026-04-18 10:00:00".to_string()),
            actor: Some("project-agentdesk".to_string()),
        }];
        let session_transcripts = vec![SessionTranscriptRow {
            turn_id: "discord:cutover:1".to_string(),
            session_key: Some("session-cutover".to_string()),
            channel_id: Some("123456".to_string()),
            agent_id: Some("project-agentdesk".to_string()),
            provider: Some("codex".to_string()),
            dispatch_id: None,
            user_message: "hello".to_string(),
            assistant_message: "world".to_string(),
            events_json: "[]".to_string(),
            duration_ms: Some(1234),
            created_at: Some("2026-04-18 10:01:02".to_string()),
        }];

        let first = import_history_into_pg(&pool, &audit_logs, &session_transcripts)
            .await
            .expect("first import");
        let second = import_history_into_pg(&pool, &audit_logs, &session_transcripts)
            .await
            .expect("second import");

        assert_eq!(first.audit_logs_inserted, 1);
        assert_eq!(first.session_transcripts_upserted, 1);
        assert_eq!(second.audit_logs_inserted, 0);
        assert_eq!(second.session_transcripts_upserted, 1);

        let counts = load_pg_cutover_counts(&pool).await.expect("pg counts");
        assert_eq!(counts.audit_logs, 1);
        assert_eq!(counts.session_transcripts, 1);

        let transcript = sqlx::query(
            "SELECT user_message, assistant_message, duration_ms
             FROM session_transcripts
             WHERE turn_id = $1",
        )
        .bind("discord:cutover:1")
        .fetch_one(&pool)
        .await
        .expect("load imported transcript");
        assert_eq!(transcript.get::<String, _>("user_message"), "hello");
        assert_eq!(transcript.get::<String, _>("assistant_message"), "world");
        assert_eq!(transcript.get::<Option<i32>, _>("duration_ms"), Some(1234));

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn write_archive_files_emits_jsonl_pairs() {
        let temp_dir = TempDir::new().expect("tempdir");
        let output = write_archive_files(
            temp_dir.path().to_str().unwrap(),
            &[AuditLogRow {
                entity_type: Some("card".to_string()),
                entity_id: Some("card-1".to_string()),
                action: Some("advance".to_string()),
                timestamp: Some("2026-04-18 10:00:00".to_string()),
                actor: Some("tester".to_string()),
            }],
            &[SessionTranscriptRow {
                turn_id: "discord:test:1".to_string(),
                session_key: None,
                channel_id: None,
                agent_id: None,
                provider: Some("codex".to_string()),
                dispatch_id: None,
                user_message: "hello".to_string(),
                assistant_message: "world".to_string(),
                events_json: "[]".to_string(),
                duration_ms: None,
                created_at: Some("2026-04-18 10:01:02".to_string()),
            }],
        )
        .expect("write archive files");

        assert!(Path::new(output.audit_logs_file.as_deref().unwrap()).exists());
        assert!(Path::new(output.session_transcripts_file.as_deref().unwrap()).exists());
    }

    #[test]
    fn postgres_cutover_args_default_to_pg_import() {
        let args = PostgresCutoverArgs {
            dry_run: true,
            archive_dir: None,
            skip_pg_import: false,
            allow_unsent_messages: false,
        };
        assert!(args.dry_run);
        assert!(!args.skip_pg_import);
        assert!(!args.allow_unsent_messages);
    }

    #[test]
    fn load_session_transcripts_handles_null_messages() {
        let conn = Connection::open_in_memory().expect("sqlite in memory");
        conn.execute_batch(
            "CREATE TABLE session_transcripts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_id TEXT NOT NULL,
                session_key TEXT,
                channel_id TEXT,
                agent_id TEXT,
                provider TEXT,
                dispatch_id TEXT,
                user_message TEXT,
                assistant_message TEXT,
                events_json TEXT,
                duration_ms INTEGER,
                created_at TEXT
            );",
        )
        .expect("create legacy-compatible session_transcripts table");
        conn.execute(
            "INSERT INTO session_transcripts (
                turn_id, session_key, channel_id, provider, user_message, assistant_message, events_json
             ) VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5)",
            libsql_rusqlite::params!["discord:null:1", "session-null", "123", "codex", "[]"],
        )
        .unwrap();

        let rows = load_session_transcripts(&conn).expect("load session transcripts");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_message, "");
        assert_eq!(rows[0].assistant_message, "");
    }
}
