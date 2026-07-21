//! DB retention job (#1093 / 909-4; extended in #3865).
//!
//! Nine retention policies across the AgentDesk postgres backbone:
//!
//! | Table                                   | Retention | Strategy                          |
//! |-----------------------------------------|-----------|-----------------------------------|
//! | `agent_quality_event`                   | 90 days   | Monthly aggregate, then DELETE    |
//! | `session_transcripts`                   | 90 days   | Archive-table copy, then DELETE   |
//! | `message_outbox` (sent)                 | 7 days    | DELETE (durable sentinels exempt) |
//! | `auto_queue_entries`                    | 30 days   | DELETE (status='completed')       |
//! | `task_dispatches`                       | 90 days   | Monthly aggregate, then DELETE    |
//! | `turn_lifecycle_events`                 | 30 days   | DELETE (on `created_at`)          |
//! | `skill_usage`                           | 90 days   | DELETE (on `used_at`)             |
//! | `turns`                                 | 90 days   | Archive-table copy, then DELETE   |
//! | `scheduled_message_context_snapshots`   | 30 days   | DELETE (unreferenced by any def)  |
//!
//! `kanban_cards` is explicitly **not** touched — done cards are permanent
//! history. See `docs/source-of-truth.md` §retention for the policy rationale.
//!
//! Each operation returns a [`TableReport`] logging action taken and rows
//! affected, so `/api/cron-jobs` and observability dashboards can diff
//! retention pressure week-over-week.
//!
//! ## Dry-run mode
//! When `dry_run = true`, every DELETE is rewritten as a `SELECT COUNT(*)` and
//! every aggregate INSERT is skipped. The returned [`RetentionReport`] is
//! populated with the would-be counts but the DB is untouched. Used by CI and
//! staging verification pipelines.

use anyhow::Result;
use serde::Serialize;
use sqlx::{PgPool, Row};

/// Per-table outcome of a single retention pass.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TableReport {
    pub table_name: &'static str,
    pub action: &'static str,
    pub rows_affected: i64,
}

/// Full report for one run of [`db_retention_job`]. Eight table entries plus
/// any aggregate-write / archive-write entries (turn_analytics, task_dispatches,
/// session_transcripts_archive, turns_archive).
#[derive(Debug, Clone, Serialize, Default)]
pub struct RetentionReport {
    pub dry_run: bool,
    pub tables: Vec<TableReport>,
}

impl RetentionReport {
    fn push(&mut self, entry: TableReport) {
        self.tables.push(entry);
    }

    /// Flat summary for log lines: `"tbl:action=N"` pairs.
    pub fn summary(&self) -> Vec<String> {
        self.tables
            .iter()
            .map(|t| format!("{}:{}={}", t.table_name, t.action, t.rows_affected))
            .collect()
    }

    /// Total rows deleted across all operations (excludes aggregate inserts).
    pub fn total_deleted(&self) -> i64 {
        self.tables
            .iter()
            .filter(|t| t.action == "delete" || t.action == "delete_would")
            .map(|t| t.rows_affected)
            .sum()
    }

    pub fn get(&self, table: &str, action: &str) -> Option<&TableReport> {
        self.tables
            .iter()
            .find(|t| t.table_name == table && t.action == action)
    }
}

const TURN_RETENTION_DAYS: i32 = 90;
const TRANSCRIPT_RETENTION_DAYS: i32 = 90;
const OUTBOX_RETENTION_DAYS: i32 = 7;
const AUTO_QUEUE_RETENTION_DAYS: i32 = 30;
const DISPATCH_RETENTION_DAYS: i32 = 90;
// #3865 — three INSERT-only tables with no prior prune. These named windows are
// the configurable retention boundaries for the policies added below.
const TURN_LIFECYCLE_RETENTION_DAYS: i32 = 30; // pure operational telemetry, highest volume (multi-row/turn)
const SKILL_USAGE_RETENTION_DAYS: i32 = 90; // dashboard analytics (used_at DESC fast-path)
const TURNS_RETENTION_DAYS: i32 = 90; // token/cost analytics → archive before delete
// #4658 — immutable scheduled-message context snapshots. Deleted only once no
// active (non-terminal) definition still references them AND they are older than
// this window, so a live recurring reservation's snapshot is never reclaimed.
const CONTEXT_SNAPSHOT_RETENTION_DAYS: i32 = 30;

/// Run the full retention pass. Returns a per-table report. When
/// `dry_run = true` no DML is executed — only SELECT COUNT(*) probes.
pub async fn db_retention_job(pool: &PgPool, dry_run: bool) -> Result<RetentionReport> {
    let mut report = RetentionReport {
        dry_run,
        tables: Vec::with_capacity(12),
    };

    // 1. turn analytics (agent_quality_event).
    retain_turn_analytics(pool, dry_run, &mut report).await?;
    // 2. session_transcripts archive.
    retain_session_transcripts(pool, dry_run, &mut report).await?;
    // 3. message_outbox (sent rows).
    retain_message_outbox(pool, dry_run, &mut report).await?;
    // 4. auto_queue_entries.
    retain_auto_queue_entries(pool, dry_run, &mut report).await?;
    // 5. task_dispatches.
    retain_task_dispatches(pool, dry_run, &mut report).await?;
    // 6. turn_lifecycle_events (time-window DELETE on created_at). #3865
    retain_turn_lifecycle_events(pool, dry_run, &mut report).await?;
    // 7. skill_usage (time-window DELETE on used_at). #3865
    retain_skill_usage(pool, dry_run, &mut report).await?;
    // 8. turns (archive-then-delete on finished_at). #3865
    retain_turns(pool, dry_run, &mut report).await?;
    // 9. scheduled_message_context_snapshots (unreferenced, aged out). #4658
    retain_context_snapshots(pool, dry_run, &mut report).await?;

    tracing::info!(
        dry_run,
        total_deleted = report.total_deleted(),
        table_count = report.tables.len(),
        "[db_retention] pass complete"
    );
    Ok(report)
}

// ─────────────────────────────────────────────────────────────────────────
// 1. agent_quality_event (turn_analytics): monthly aggregate then DELETE.
// ─────────────────────────────────────────────────────────────────────────
async fn retain_turn_analytics(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM agent_quality_event \
             WHERE created_at < NOW() - ($1::INT || ' days')::INTERVAL \
               AND event_type IN ('turn_start','turn_complete','turn_error')",
        )
        .bind(TURN_RETENTION_DAYS)
        .fetch_one(pool)
        .await?;
        let n: i64 = would.try_get("n").unwrap_or(0);
        report.push(TableReport {
            table_name: "agent_quality_event",
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    // Aggregate-into first. ON CONFLICT DO NOTHING keeps prior months stable
    // (we only backfill *new* month buckets; the most recent month is still
    // inside the 90d window so its row is never written here).
    let agg = sqlx::query(
        "INSERT INTO turn_analytics_monthly_aggregate \
             (month, total_turns, success_count, error_count, start_count, aggregated_at) \
         SELECT date_trunc('month', created_at)::DATE AS month, \
                COUNT(*) FILTER (WHERE event_type IN ('turn_start','turn_complete','turn_error'))::BIGINT, \
                COUNT(*) FILTER (WHERE event_type = 'turn_complete')::BIGINT, \
                COUNT(*) FILTER (WHERE event_type = 'turn_error')::BIGINT, \
                COUNT(*) FILTER (WHERE event_type = 'turn_start')::BIGINT, \
                NOW() \
         FROM agent_quality_event \
         WHERE created_at < NOW() - ($1::INT || ' days')::INTERVAL \
           AND event_type IN ('turn_start','turn_complete','turn_error') \
         GROUP BY date_trunc('month', created_at) \
         ON CONFLICT (month) DO NOTHING",
    )
    .bind(TURN_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "turn_analytics_monthly_aggregate",
        action: "insert",
        rows_affected: agg.rows_affected() as i64,
    });

    let del = sqlx::query(
        "DELETE FROM agent_quality_event \
         WHERE created_at < NOW() - ($1::INT || ' days')::INTERVAL \
           AND event_type IN ('turn_start','turn_complete','turn_error')",
    )
    .bind(TURN_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "agent_quality_event",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// 2. session_transcripts: archive-then-delete.
// ─────────────────────────────────────────────────────────────────────────
async fn retain_session_transcripts(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM session_transcripts \
             WHERE created_at < NOW() - ($1::INT || ' days')::INTERVAL",
        )
        .bind(TRANSCRIPT_RETENTION_DAYS)
        .fetch_one(pool)
        .await?;
        let n: i64 = would.try_get("n").unwrap_or(0);
        report.push(TableReport {
            table_name: "session_transcripts",
            action: "archive_would",
            rows_affected: n,
        });
        return Ok(());
    }

    // INSERT … SELECT … WHERE NOT EXISTS keeps re-runs idempotent.
    let archived = sqlx::query(
        "INSERT INTO session_transcripts_archive \
             (id, turn_id, session_key, channel_id, agent_id, provider, dispatch_id, \
              user_message, assistant_message, events_json, duration_ms, created_at) \
         SELECT s.id, s.turn_id, s.session_key, s.channel_id, s.agent_id, s.provider, \
                s.dispatch_id, s.user_message, s.assistant_message, s.events_json, \
                s.duration_ms, s.created_at \
         FROM session_transcripts s \
         WHERE s.created_at < NOW() - ($1::INT || ' days')::INTERVAL \
           AND NOT EXISTS ( \
               SELECT 1 FROM session_transcripts_archive a WHERE a.id = s.id \
           )",
    )
    .bind(TRANSCRIPT_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "session_transcripts_archive",
        action: "insert",
        rows_affected: archived.rows_affected() as i64,
    });

    let del = sqlx::query(
        "DELETE FROM session_transcripts \
         WHERE created_at < NOW() - ($1::INT || ' days')::INTERVAL",
    )
    .bind(TRANSCRIPT_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "session_transcripts",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// 3. message_outbox: delete sent rows older than 7 days, except permanent
// dedupe sentinels (`dedupe_key IS NOT NULL AND dedupe_expires_at IS NULL`).
//
// Schema uses `sent_at` (not `delivered_at`) — the DoD's "delivered" maps to
// status='sent' + sent_at set. Treat both as interchangeable here.
// ─────────────────────────────────────────────────────────────────────────
async fn retain_message_outbox(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM message_outbox \
             WHERE sent_at IS NOT NULL \
               AND sent_at < NOW() - ($1::INT || ' days')::INTERVAL \
               AND NOT (dedupe_key IS NOT NULL AND dedupe_expires_at IS NULL)",
        )
        .bind(OUTBOX_RETENTION_DAYS)
        .fetch_one(pool)
        .await?;
        let n: i64 = would.try_get("n").unwrap_or(0);
        report.push(TableReport {
            table_name: "message_outbox",
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let del = sqlx::query(
        "DELETE FROM message_outbox \
         WHERE sent_at IS NOT NULL \
           AND sent_at < NOW() - ($1::INT || ' days')::INTERVAL \
           AND NOT (dedupe_key IS NOT NULL AND dedupe_expires_at IS NULL)",
    )
    .bind(OUTBOX_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "message_outbox",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// 4. auto_queue_entries: delete completed rows older than 30 days.
// ─────────────────────────────────────────────────────────────────────────
async fn retain_auto_queue_entries(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM auto_queue_entries \
             WHERE status = 'completed' \
               AND completed_at IS NOT NULL \
               AND completed_at < NOW() - ($1::INT || ' days')::INTERVAL",
        )
        .bind(AUTO_QUEUE_RETENTION_DAYS)
        .fetch_one(pool)
        .await?;
        let n: i64 = would.try_get("n").unwrap_or(0);
        report.push(TableReport {
            table_name: "auto_queue_entries",
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let del = sqlx::query(
        "DELETE FROM auto_queue_entries \
         WHERE status = 'completed' \
           AND completed_at IS NOT NULL \
           AND completed_at < NOW() - ($1::INT || ' days')::INTERVAL",
    )
    .bind(AUTO_QUEUE_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "auto_queue_entries",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// 5. task_dispatches: monthly aggregate + delete completed rows older than 90d.
// ─────────────────────────────────────────────────────────────────────────
async fn retain_task_dispatches(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM task_dispatches \
             WHERE status = 'completed' \
               AND completed_at IS NOT NULL \
               AND completed_at < NOW() - ($1::INT || ' days')::INTERVAL",
        )
        .bind(DISPATCH_RETENTION_DAYS)
        .fetch_one(pool)
        .await?;
        let n: i64 = would.try_get("n").unwrap_or(0);
        report.push(TableReport {
            table_name: "task_dispatches",
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let agg = sqlx::query(
        "INSERT INTO task_dispatches_monthly_aggregate \
             (month, total_dispatches, completed_count, review_count, aggregated_at) \
         SELECT date_trunc('month', completed_at)::DATE AS month, \
                COUNT(*)::BIGINT, \
                COUNT(*) FILTER (WHERE status = 'completed')::BIGINT, \
                COUNT(*) FILTER (WHERE dispatch_type = 'review')::BIGINT, \
                NOW() \
         FROM task_dispatches \
         WHERE status = 'completed' \
           AND completed_at IS NOT NULL \
           AND completed_at < NOW() - ($1::INT || ' days')::INTERVAL \
         GROUP BY date_trunc('month', completed_at) \
         ON CONFLICT (month) DO NOTHING",
    )
    .bind(DISPATCH_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "task_dispatches_monthly_aggregate",
        action: "insert",
        rows_affected: agg.rows_affected() as i64,
    });

    let del = sqlx::query(
        "DELETE FROM task_dispatches \
         WHERE status = 'completed' \
           AND completed_at IS NOT NULL \
           AND completed_at < NOW() - ($1::INT || ' days')::INTERVAL",
    )
    .bind(DISPATCH_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "task_dispatches",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// 6. turn_lifecycle_events: delete telemetry rows older than 30 days. #3865
//
// Pure operational telemetry (multiple rows per turn) with no downstream
// aggregate — a plain time-window DELETE on the indexed `created_at` column.
// ─────────────────────────────────────────────────────────────────────────
async fn retain_turn_lifecycle_events(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM turn_lifecycle_events \
             WHERE created_at < NOW() - ($1::INT || ' days')::INTERVAL",
        )
        .bind(TURN_LIFECYCLE_RETENTION_DAYS)
        .fetch_one(pool)
        .await?;
        let n: i64 = would.try_get("n").unwrap_or(0);
        report.push(TableReport {
            table_name: "turn_lifecycle_events",
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let del = sqlx::query(
        "DELETE FROM turn_lifecycle_events \
         WHERE created_at < NOW() - ($1::INT || ' days')::INTERVAL",
    )
    .bind(TURN_LIFECYCLE_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "turn_lifecycle_events",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// 7. skill_usage: delete usage rows older than 90 days. #3865
//
// `used_at` is nullable (DEFAULT NOW()); rows are never inserted with NULL, but
// the `used_at IS NOT NULL` guard mirrors the message_outbox `sent_at` guard so
// a stray NULL is retained rather than mis-windowed.
// ─────────────────────────────────────────────────────────────────────────
async fn retain_skill_usage(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM skill_usage \
             WHERE used_at IS NOT NULL \
               AND used_at < NOW() - ($1::INT || ' days')::INTERVAL",
        )
        .bind(SKILL_USAGE_RETENTION_DAYS)
        .fetch_one(pool)
        .await?;
        let n: i64 = would.try_get("n").unwrap_or(0);
        report.push(TableReport {
            table_name: "skill_usage",
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let del = sqlx::query(
        "DELETE FROM skill_usage \
         WHERE used_at IS NOT NULL \
           AND used_at < NOW() - ($1::INT || ' days')::INTERVAL",
    )
    .bind(SKILL_USAGE_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "skill_usage",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// 8. turns: archive-then-delete on finished_at older than 90 days. #3865
//
// Like `retain_session_transcripts` this copies into `turns_archive` (idempotent
// via WHERE NOT EXISTS) before deleting, but `turns` is high-value token/cost
// data so the two steps are hardened against an archive-less delete:
//
//   * Both statements run inside ONE transaction. Postgres `NOW()` resolves to
//     `transaction_timestamp()`, which is fixed for the life of the transaction,
//     so the archive and delete predicates share the *identical* cutoff — a row
//     can never cross the 90-day boundary between the two steps. (This is
//     stronger than computing the cutoff app-side, which would add DB/app clock
//     skew.)
//   * The DELETE carries an `EXISTS (… turns_archive …)` guard, so it can only
//     remove rows that are already in the archive — a delete-without-archive is
//     impossible even if the reasoning above were ever violated.
//
// `finished_at` is NOT NULL → no NULL edge cases.
// ─────────────────────────────────────────────────────────────────────────
async fn retain_turns(pool: &PgPool, dry_run: bool, report: &mut RetentionReport) -> Result<()> {
    if dry_run {
        let would = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM turns \
             WHERE finished_at < NOW() - ($1::INT || ' days')::INTERVAL",
        )
        .bind(TURNS_RETENTION_DAYS)
        .fetch_one(pool)
        .await?;
        let n: i64 = would.try_get("n").unwrap_or(0);
        report.push(TableReport {
            table_name: "turns",
            action: "archive_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let mut tx = pool.begin().await?;

    // INSERT … SELECT … WHERE NOT EXISTS keeps re-runs idempotent.
    let archived = sqlx::query(
        "INSERT INTO turns_archive \
             (turn_id, session_key, thread_id, thread_title, channel_id, agent_id, \
              provider, session_id, dispatch_id, started_at, finished_at, duration_ms, \
              input_tokens, cache_create_tokens, cache_read_tokens, output_tokens, created_at) \
         SELECT t.turn_id, t.session_key, t.thread_id, t.thread_title, t.channel_id, \
                t.agent_id, t.provider, t.session_id, t.dispatch_id, t.started_at, \
                t.finished_at, t.duration_ms, t.input_tokens, t.cache_create_tokens, \
                t.cache_read_tokens, t.output_tokens, t.created_at \
         FROM turns t \
         WHERE t.finished_at < NOW() - ($1::INT || ' days')::INTERVAL \
           AND NOT EXISTS ( \
               SELECT 1 FROM turns_archive a WHERE a.turn_id = t.turn_id \
           )",
    )
    .bind(TURNS_RETENTION_DAYS)
    .execute(&mut *tx)
    .await?;
    report.push(TableReport {
        table_name: "turns_archive",
        action: "insert",
        rows_affected: archived.rows_affected() as i64,
    });

    // EXISTS guard: only delete rows that are already archived. Combined with the
    // shared transaction cutoff, every old row was archived by the INSERT above,
    // so this deletes exactly the archived set and nothing more.
    let del = sqlx::query(
        "DELETE FROM turns t \
         WHERE t.finished_at < NOW() - ($1::INT || ' days')::INTERVAL \
           AND EXISTS ( \
               SELECT 1 FROM turns_archive a WHERE a.turn_id = t.turn_id \
           )",
    )
    .bind(TURNS_RETENTION_DAYS)
    .execute(&mut *tx)
    .await?;
    report.push(TableReport {
        table_name: "turns",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });

    tx.commit().await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// 9. scheduled_message_context_snapshots: delete rows older than 30 days that
// NO definition still references. #4658
//
// The `fk_smsg_context_snapshot` foreign key pins a snapshot for as long as any
// definition (active OR terminal) references it, so retention must only reclaim
// fully-unreferenced rows — deleting a still-referenced snapshot would violate
// the FK. This makes AC-9 ("retention never deletes an active repeating
// definition's snapshot") structural: an active definition is a reference, so it
// is never a delete candidate.
//
// KNOWN LIMITATION (#4658 F3, follow-up #4723): in the current
// system nothing ever deletes a `scheduled_messages` row — cancel sets
// status='canceled' but keeps the row, and capture is same-transaction as the
// definition insert (so orphaned snapshots are never produced). A snapshot
// therefore stays referenced for the definition's entire lifetime, and this
// policy in practice only reclaims rows a FUTURE definition-hard-delete path
// would orphan. Consequence: each terminal snapshot definition retains its
// rendered_context (≤32KB/row) indefinitely. This is BOUNDED (one small row per
// snapshot definition ever created) and FK-safe, deliberately preferred over a
// history-mutating reclaim (nulling context_snapshot_id / flipping a terminal
// definition to 'fresh') which would need its own design review. A real reclaim
// trigger (terminal-definition lifecycle cleanup) is deferred to the follow-up.
// ─────────────────────────────────────────────────────────────────────────
const CONTEXT_SNAPSHOT_UNREFERENCED_PREDICATE: &str = "created_at < NOW() - ($1::INT || ' days')::INTERVAL \
       AND NOT EXISTS ( \
           SELECT 1 FROM scheduled_messages m \
           WHERE m.context_snapshot_id = scheduled_message_context_snapshots.id \
       )";

async fn retain_context_snapshots(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(&format!(
            "SELECT COUNT(*)::BIGINT AS n FROM scheduled_message_context_snapshots \
             WHERE {CONTEXT_SNAPSHOT_UNREFERENCED_PREDICATE}"
        ))
        .bind(CONTEXT_SNAPSHOT_RETENTION_DAYS)
        .fetch_one(pool)
        .await?;
        let n: i64 = would.try_get("n").unwrap_or(0);
        report.push(TableReport {
            table_name: "scheduled_message_context_snapshots",
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let del = sqlx::query(&format!(
        "DELETE FROM scheduled_message_context_snapshots \
         WHERE {CONTEXT_SNAPSHOT_UNREFERENCED_PREDICATE}"
    ))
    .bind(CONTEXT_SNAPSHOT_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "scheduled_message_context_snapshots",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// #3865 — regression coverage for the three new retention policies.
//
// Uses the shared `DispatchPostgresTestDb` harness (same pattern as
// `engine::ops::kanban_ops` tests): create an ephemeral DB, run all migrations
// (incl. 0075_turns_archive), seed one stale + one fresh row per table, run the
// job, and assert old rows are pruned, fresh rows survive, `turns` rows are
// archived, the report is shaped correctly, dry-run is a no-op, and re-runs are
// idempotent. Skipped automatically when no local Postgres is reachable.
// ─────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    /// Token value larger than INT32::MAX (2_147_483_647). The `turns` token
    /// columns were widened to BIGINT in 0008; the stale row carries this so the
    /// archive copy exercises BIGINT fidelity (an INTEGER archive column would
    /// overflow here and fail the whole pass — guards #3865 review finding #1).
    const BIG_TOKENS: i64 = 3_000_000_000;

    async fn count(pool: &PgPool, sql: &str) -> i64 {
        sqlx::query(sql)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|err| panic!("count query `{sql}`: {err}"))
            .try_get::<i64, _>("n")
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_preserves_permanent_outbox_dedupe_sentinels() {
        let Some(db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_db_retention_outbox_persistent_dedupe",
            "db_retention persistent outbox dedupe contract",
        )
        .await
        else {
            return;
        };
        let pool = db.connect_and_migrate().await;

        use crate::services::message_outbox::{
            OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe,
            enqueue_outbox_pg_returning_id_with_ttl,
        };
        let persistent_id = enqueue_outbox_pg_returning_id_with_persistent_dedupe(
            &pool,
            OutboxMessage {
                target: "channel:1",
                content: "persistent",
                bot: "notify",
                source: "scheduled_message",
                reason_code: Some("scheduled_message:v1:retention-test:slot"),
                session_key: None,
            },
        )
        .await
        .expect("enqueue permanent sentinel");
        let ordinary_id = enqueue_outbox_pg_returning_id_with_ttl(
            &pool,
            OutboxMessage {
                target: "channel:1",
                content: "ordinary",
                bot: "notify",
                source: "system",
                reason_code: None,
                session_key: None,
            },
            0,
        )
        .await
        .expect("enqueue ordinary row")
        .expect("ordinary row inserted");
        let ttl_id = enqueue_outbox_pg_returning_id_with_ttl(
            &pool,
            OutboxMessage {
                target: "channel:1",
                content: "ttl-expired",
                bot: "notify",
                source: "system",
                reason_code: Some("retention-test-ttl"),
                session_key: None,
            },
            60,
        )
        .await
        .expect("enqueue TTL row")
        .expect("TTL row inserted");
        sqlx::query(
            "UPDATE message_outbox
             SET status = 'sent', sent_at = NOW() - INTERVAL '8 days',
                 created_at = NOW() - INTERVAL '8 days',
                 dedupe_expires_at = CASE WHEN id = $2
                     THEN NOW() - INTERVAL '7 days' ELSE dedupe_expires_at END
             WHERE id = ANY($1)",
        )
        .bind(vec![persistent_id, ordinary_id, ttl_id])
        .bind(ttl_id)
        .execute(&pool)
        .await
        .expect("age stale sent outbox rows");

        let dry = db_retention_job(&pool, true)
            .await
            .expect("dry-run retention pass");
        assert_eq!(
            dry.get("message_outbox", "delete_would")
                .map(|entry| entry.rows_affected),
            Some(2),
            "dry-run must exclude the permanent sentinel"
        );

        let report = db_retention_job(&pool, false)
            .await
            .expect("retention pass");
        assert_eq!(
            report
                .get("message_outbox", "delete")
                .map(|entry| entry.rows_affected),
            Some(2)
        );
        let survivors: Vec<String> =
            sqlx::query_scalar("SELECT content FROM message_outbox ORDER BY content")
                .fetch_all(&pool)
                .await
                .expect("read outbox survivors");
        assert_eq!(survivors, vec!["persistent"]);

        pool.close().await;
        db.drop().await;
    }

    /// Insert one `turns` row windowed on `finished_at`, with explicit BIGINT
    /// token/duration values so archive fidelity can be asserted.
    async fn seed_turn(pool: &PgPool, turn_id: &str, age_days: i32, tokens: i64) {
        sqlx::query(
            "INSERT INTO turns \
                 (turn_id, channel_id, started_at, finished_at, duration_ms, \
                  input_tokens, cache_create_tokens, cache_read_tokens, output_tokens) \
             VALUES ($1, 'chan', \
                     NOW() - ($2::INT || ' days')::INTERVAL, \
                     NOW() - ($2::INT || ' days')::INTERVAL, $3, $3, $3, $3, $3)",
        )
        .bind(turn_id)
        .bind(age_days)
        .bind(tokens)
        .execute(pool)
        .await
        .unwrap_or_else(|err| panic!("seed turns {turn_id}: {err}"));
    }

    /// Seed one stale row (older than the policy window) and one fresh row in
    /// each of the three tables. `stale_days` puts the stale row safely past
    /// the largest (90d) window. The stale `turns` row carries [`BIG_TOKENS`].
    async fn seed_fixtures(pool: &PgPool, stale_days: i32) {
        // turn_lifecycle_events: stale + fresh.
        for (turn_id, age) in [("tle-old", stale_days), ("tle-new", 0)] {
            sqlx::query(
                "INSERT INTO turn_lifecycle_events \
                     (turn_id, channel_id, kind, severity, summary, created_at) \
                 VALUES ($1, 'chan', 'turn_start', 'info', 'seed', \
                         NOW() - ($2::INT || ' days')::INTERVAL)",
            )
            .bind(turn_id)
            .bind(age)
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("seed turn_lifecycle_events {turn_id}: {err}"));
        }

        // skill_usage: stale + fresh.
        for (skill_id, age) in [("sk-old", stale_days), ("sk-new", 0)] {
            sqlx::query(
                "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at) \
                 VALUES ($1, 'agent', 'sess', NOW() - ($2::INT || ' days')::INTERVAL)",
            )
            .bind(skill_id)
            .bind(age)
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("seed skill_usage {skill_id}: {err}"));
        }

        // turns: stale (>INT32 tokens) + fresh (windowed on finished_at).
        seed_turn(pool, "turn-old", stale_days, BIG_TOKENS).await;
        seed_turn(pool, "turn-new", 0, 20).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_prunes_old_rows_archives_turns_and_is_idempotent() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_3865",
            "db_retention #3865 lifecycle/skill_usage/turns coverage",
        )
        .await;
        let pool = db.connect_and_migrate().await;

        // 91 days is past every policy window (max is 90d).
        seed_fixtures(&pool, 91).await;

        // ── Dry-run is a no-op: nothing deleted, would-counts == 1 each. ──
        let dry = db_retention_job(&pool, true)
            .await
            .expect("dry-run retention pass");
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM turn_lifecycle_events"
            )
            .await,
            2,
            "dry-run must not delete turn_lifecycle_events rows"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*)::BIGINT AS n FROM skill_usage").await,
            2,
            "dry-run must not delete skill_usage rows"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*)::BIGINT AS n FROM turns").await,
            2,
            "dry-run must not delete turns rows"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*)::BIGINT AS n FROM turns_archive").await,
            0,
            "dry-run must not archive turns rows"
        );
        assert_eq!(
            dry.get("turn_lifecycle_events", "delete_would")
                .map(|t| t.rows_affected),
            Some(1)
        );
        assert_eq!(
            dry.get("skill_usage", "delete_would")
                .map(|t| t.rows_affected),
            Some(1)
        );
        assert_eq!(
            dry.get("turns", "archive_would").map(|t| t.rows_affected),
            Some(1)
        );

        // ── Live run: old rows pruned, fresh rows kept, turn archived. ──
        let report = db_retention_job(&pool, false)
            .await
            .expect("live retention pass");

        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM turn_lifecycle_events"
            )
            .await,
            1,
            "stale turn_lifecycle_events row must be deleted"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM turn_lifecycle_events WHERE turn_id = 'tle-new'"
            )
            .await,
            1,
            "fresh turn_lifecycle_events row must survive"
        );

        assert_eq!(
            count(&pool, "SELECT COUNT(*)::BIGINT AS n FROM skill_usage").await,
            1,
            "stale skill_usage row must be deleted"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM skill_usage WHERE skill_id = 'sk-new'"
            )
            .await,
            1,
            "fresh skill_usage row must survive"
        );

        assert_eq!(
            count(&pool, "SELECT COUNT(*)::BIGINT AS n FROM turns").await,
            1,
            "stale turns row must be deleted"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM turns WHERE turn_id = 'turn-new'"
            )
            .await,
            1,
            "fresh turns row must survive"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM turns_archive WHERE turn_id = 'turn-old'"
            )
            .await,
            1,
            "stale turns row must be copied into turns_archive before deletion"
        );

        // BIGINT fidelity: the >INT32 token/duration values survive the archive
        // copy intact (an INTEGER archive column would have overflowed).
        let archived_tokens: i64 =
            sqlx::query("SELECT input_tokens AS n FROM turns_archive WHERE turn_id = 'turn-old'")
                .fetch_one(&pool)
                .await
                .expect("read archived input_tokens")
                .try_get::<i64, _>("n")
                .expect("input_tokens is BIGINT");
        assert_eq!(
            archived_tokens, BIG_TOKENS,
            "archived input_tokens must preserve the >INT32 value without overflow"
        );

        // Report entries reflect the new policies.
        assert_eq!(
            report
                .get("turn_lifecycle_events", "delete")
                .map(|t| t.rows_affected),
            Some(1)
        );
        assert_eq!(
            report.get("skill_usage", "delete").map(|t| t.rows_affected),
            Some(1)
        );
        assert_eq!(
            report
                .get("turns_archive", "insert")
                .map(|t| t.rows_affected),
            Some(1)
        );
        assert_eq!(
            report.get("turns", "delete").map(|t| t.rows_affected),
            Some(1)
        );

        // ── Idempotency: a second run deletes nothing new and creates no
        //    duplicate archive rows (NOT EXISTS guard). ──
        let rerun = db_retention_job(&pool, false)
            .await
            .expect("second retention pass");
        assert_eq!(
            rerun
                .get("turns_archive", "insert")
                .map(|t| t.rows_affected),
            Some(0),
            "re-run must not duplicate turns_archive rows"
        );
        assert_eq!(
            rerun.get("turns", "delete").map(|t| t.rows_affected),
            Some(0),
            "re-run must delete no additional turns rows"
        );
        assert_eq!(
            count(&pool, "SELECT COUNT(*)::BIGINT AS n FROM turns_archive").await,
            1,
            "turns_archive must hold exactly one row after a double run"
        );

        pool.close().await;
        db.drop().await;
    }

    /// #4658 AC-9: a snapshot referenced by an active definition is never
    /// reclaimed, even when aged past the window; once no definition references
    /// it (the referencing row is gone) the aged snapshot is deleted. A second
    /// aged snapshot with no reference is deleted immediately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_context_snapshot_pg_reference_gate() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_4658_snapshots",
            "db_retention #4658 context snapshot referenced/aged policy",
        )
        .await;
        let pool = db.connect_and_migrate().await;

        let hex64 = "0".repeat(64);
        // Two aged snapshots: one referenced by an active definition, one orphan.
        for id in ["smcs_active", "smcs_orphan"] {
            sqlx::query(
                "INSERT INTO scheduled_message_context_snapshots
                    (id, source_channel_id, transcript_frontier, rendered_context,
                     pair_count, content_digest, created_at)
                 VALUES ($1, '1', 0, 'ctx', 1, $2, NOW() - INTERVAL '40 days')",
            )
            .bind(id)
            .bind(&hex64)
            .execute(&pool)
            .await
            .expect("seed aged snapshot");
        }
        // Active push definition referencing smcs_active (push avoids the agents FK;
        // the retention guard keys only on status + context_snapshot_id).
        sqlx::query(
            "INSERT INTO scheduled_messages
                (id, content, target_channel_id, delivery_kind, scheduled_at, status,
                 context_strategy, context_snapshot_id)
             VALUES ('smsg_ref', 'c', '1', 'push', NOW(), 'scheduled', 'snapshot', 'smcs_active')",
        )
        .execute(&pool)
        .await
        .expect("seed referencing active definition");

        // First pass: orphan deleted, referenced-active survives.
        db_retention_job(&pool, false)
            .await
            .expect("retention pass 1");
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM scheduled_message_context_snapshots WHERE id = 'smcs_orphan'"
            )
            .await,
            0,
            "unreferenced aged snapshot must be deleted"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM scheduled_message_context_snapshots WHERE id = 'smcs_active'"
            )
            .await,
            1,
            "snapshot of an active definition must never be deleted (AC-9)"
        );

        // Remove the referencing definition; now the snapshot is reclaimable
        // (the FK no longer pins it).
        sqlx::query("DELETE FROM scheduled_messages WHERE id = 'smsg_ref'")
            .execute(&pool)
            .await
            .expect("delete referencing definition");
        db_retention_job(&pool, false)
            .await
            .expect("retention pass 2");
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM scheduled_message_context_snapshots WHERE id = 'smcs_active'"
            )
            .await,
            0,
            "once no definition references it and it has aged, the snapshot is reclaimed"
        );

        pool.close().await;
        db.drop().await;
    }

    /// The `used_at IS NOT NULL` guard must keep rows whose timestamp is NULL —
    /// a NULL `used_at` is never older-than the window, so it survives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_keeps_skill_usage_rows_with_null_used_at() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_3865_nullusedat",
            "db_retention #3865 NULL used_at survival",
        )
        .await;
        let pool = db.connect_and_migrate().await;

        // NULL used_at (must survive), stale (must delete), fresh (must keep).
        sqlx::query("INSERT INTO skill_usage (skill_id, used_at) VALUES ('sk-null', NULL)")
            .execute(&pool)
            .await
            .expect("seed NULL used_at row");
        sqlx::query(
            "INSERT INTO skill_usage (skill_id, used_at) \
             VALUES ('sk-stale', NOW() - INTERVAL '120 days')",
        )
        .execute(&pool)
        .await
        .expect("seed stale row");
        sqlx::query("INSERT INTO skill_usage (skill_id, used_at) VALUES ('sk-fresh', NOW())")
            .execute(&pool)
            .await
            .expect("seed fresh row");

        db_retention_job(&pool, false)
            .await
            .expect("retention pass");

        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM skill_usage WHERE skill_id = 'sk-null'"
            )
            .await,
            1,
            "NULL used_at row must never be deleted by the time-window prune"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM skill_usage WHERE skill_id = 'sk-stale'"
            )
            .await,
            0,
            "stale skill_usage row must be deleted"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM skill_usage WHERE skill_id = 'sk-fresh'"
            )
            .await,
            1,
            "fresh skill_usage row must survive"
        );

        pool.close().await;
        db.drop().await;
    }

    /// The `turns` window is strict (`finished_at < cutoff`): a row just inside
    /// the 90d window survives, a row just outside is archived then deleted, and
    /// the archived row count always equals the deleted row count (no
    /// delete-without-archive), including on a re-run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_turns_window_is_strict_with_no_archive_less_delete() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_3865_boundary",
            "db_retention #3865 turns strict boundary + atomic archive",
        )
        .await;
        let pool = db.connect_and_migrate().await;

        // 2-hour margins absorb the ms-level drift between seed and job NOW().
        // inside: 89d22h old → younger than 90d → kept.
        sqlx::query(
            "INSERT INTO turns (turn_id, channel_id, started_at, finished_at) \
             VALUES ('turn-inside', 'chan', NOW(), NOW() - (INTERVAL '90 days' - INTERVAL '2 hours'))",
        )
        .execute(&pool)
        .await
        .expect("seed inside-window turn");
        // outside: 90d02h old → older than 90d → archived + deleted.
        sqlx::query(
            "INSERT INTO turns (turn_id, channel_id, started_at, finished_at) \
             VALUES ('turn-outside', 'chan', NOW(), NOW() - (INTERVAL '90 days' + INTERVAL '2 hours'))",
        )
        .execute(&pool)
        .await
        .expect("seed outside-window turn");

        let report = db_retention_job(&pool, false)
            .await
            .expect("retention pass");

        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM turns WHERE turn_id = 'turn-inside'"
            )
            .await,
            1,
            "row just inside the 90d window must survive (strict `<`)"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM turns WHERE turn_id = 'turn-outside'"
            )
            .await,
            0,
            "row just outside the window must be deleted"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM turns_archive WHERE turn_id = 'turn-outside'"
            )
            .await,
            1,
            "the deleted row must have been archived first"
        );

        // Archive insert count == delete count: no delete-without-archive.
        let archived = report
            .get("turns_archive", "insert")
            .map(|t| t.rows_affected);
        let deleted = report.get("turns", "delete").map(|t| t.rows_affected);
        assert_eq!(archived, Some(1));
        assert_eq!(deleted, Some(1));
        assert_eq!(
            archived, deleted,
            "every deleted turns row must be archived in the same pass"
        );

        pool.close().await;
        db.drop().await;
    }
}
