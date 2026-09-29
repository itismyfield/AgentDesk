//! DB retention pass: archives, aggregates or deletes aged rows per table, recording each
//! step as a [`TableReport`]. Windows are the `*_RETENTION_DAYS` constants; the policy is
//! described in `docs/storage-retention.md`. `kanban_cards` is never touched: done cards
//! are permanent history. `intake_outbox` candidates are only counted, never deleted.
//!
//! With `dry_run = true` every DELETE becomes a `SELECT COUNT(*)` and no archive or
//! aggregate write runs, so the DB is untouched.

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

/// Full report for one run of [`db_retention_job`]: one entry per table plus
/// aggregate/archive writes and the `intake_outbox` `candidates` counts.
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

    /// Retention candidates counted but never deleted (the `intake_outbox` policy).
    pub fn total_candidates(&self) -> i64 {
        self.tables
            .iter()
            .filter(|t| t.action == "candidates")
            .map(|t| t.rows_affected)
            .sum()
    }

    /// Total rows deleted across all operations (excludes aggregate inserts).
    pub fn total_deleted(&self) -> i64 {
        self.tables
            .iter()
            .filter(|t| t.action == "delete" || t.action == "delete_would")
            .map(|t| t.rows_affected)
            .sum()
    }

    #[cfg(test)]
    pub fn get(&self, table: &str, action: &str) -> Option<&TableReport> {
        self.tables
            .iter()
            .find(|t| t.table_name == table && t.action == action)
    }
}

const TRANSCRIPT_RETENTION_DAYS: i32 = 90;
const OUTBOX_RETENTION_DAYS: i32 = 7;
const AUTO_QUEUE_RETENTION_DAYS: i32 = 30;
const DISPATCH_RETENTION_DAYS: i32 = 90;
const TURN_LIFECYCLE_RETENTION_DAYS: i32 = 30; // operational telemetry, several rows per turn
const SKILL_USAGE_RETENTION_DAYS: i32 = 90; // dashboard analytics
const TURNS_RETENTION_DAYS: i32 = 90; // token/cost analytics, archived before delete
// Applies to both the snapshot's age and its referencing definitions' terminal age.
const CONTEXT_SNAPSHOT_RETENTION_DAYS: i32 = 30;
const INTAKE_OUTBOX_DONE_RETENTION_DAYS: i32 = 7;
// unknown/failed rows are evidence for relay and intake loss investigations.
const INTAKE_OUTBOX_FAILED_RETENTION_DAYS: i32 = 30;
// Server-side cap on the candidate COUNT so a large backlog cannot stall the serial scheduler.
const INTAKE_OUTBOX_COUNT_STATEMENT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Runs every retention policy once. With `dry_run = true` it runs only
/// `SELECT COUNT(*)` probes.
pub async fn db_retention_job(pool: &PgPool, dry_run: bool) -> Result<RetentionReport> {
    let mut report = RetentionReport {
        dry_run,
        tables: Vec::with_capacity(12),
    };

    // `agent_quality_event` is owned by the hourly observability retention sweep.
    retain_session_transcripts(pool, dry_run, &mut report).await?;
    retain_message_outbox(pool, dry_run, &mut report).await?;
    retain_auto_queue_entries(pool, dry_run, &mut report).await?;
    retain_task_dispatches(pool, dry_run, &mut report).await?;
    retain_turn_lifecycle_events(pool, dry_run, &mut report).await?;
    retain_skill_usage(pool, dry_run, &mut report).await?;
    retain_turns(pool, dry_run, &mut report).await?;
    retain_context_snapshots(pool, dry_run, &mut report).await?;
    count_intake_outbox_candidates_bounded(
        pool,
        INTAKE_OUTBOX_COUNT_STATEMENT_TIMEOUT,
        &mut report,
    )
    .await?;

    tracing::info!(
        dry_run,
        total_deleted = report.total_deleted(),
        total_candidates = report.total_candidates(),
        table_count = report.tables.len(),
        "[db_retention] pass complete"
    );
    Ok(report)
}

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

// `sent_at` is stamped together with status='sent'. Permanent dedupe sentinels
// (`dedupe_key` set, `dedupe_expires_at` NULL) are never deleted.
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

// `used_at` is nullable; a row with NULL `used_at` is kept.
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

// Both steps share one transaction, so `NOW()` gives them the same cutoff, and the
// DELETE's EXISTS guard only removes rows already copied to `turns_archive`.
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

// Aged snapshots whose every referencing definition is terminal with an aged `updated_at`
// (bumped on each terminal transition). A re-armed recurring definition is never terminal.
const CONTEXT_SNAPSHOT_RECLAIMABLE_PREDICATE: &str = "created_at < NOW() - ($1::INT || ' days')::INTERVAL \
       AND NOT EXISTS ( \
           SELECT 1 FROM scheduled_messages m \
           WHERE m.context_snapshot_id = scheduled_message_context_snapshots.id \
             AND NOT ( \
                 m.status IN ('sent', 'failed', 'canceled', 'expired') \
                 AND m.updated_at < NOW() - ($1::INT || ' days')::INTERVAL \
             ) \
       )";

async fn retain_context_snapshots(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(&format!(
            "SELECT COUNT(*)::BIGINT AS n FROM scheduled_message_context_snapshots \
             WHERE {CONTEXT_SNAPSHOT_RECLAIMABLE_PREDICATE}"
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

    // Null the FK pointers (provenance stays in `context_snapshot_reclaimed_at`) and
    // delete the snapshots in one transaction so no definition can re-arm in between.
    let mut tx = pool.begin().await?;

    let unref = sqlx::query(&format!(
        "UPDATE scheduled_messages \
         SET context_snapshot_id = NULL, context_snapshot_reclaimed_at = NOW() \
         WHERE context_snapshot_id IN ( \
             SELECT id FROM scheduled_message_context_snapshots \
             WHERE {CONTEXT_SNAPSHOT_RECLAIMABLE_PREDICATE} \
         )"
    ))
    .bind(CONTEXT_SNAPSHOT_RETENTION_DAYS)
    .execute(&mut *tx)
    .await?;
    report.push(TableReport {
        table_name: "scheduled_messages",
        action: "reclaim_snapshot_ref",
        rows_affected: unref.rows_affected() as i64,
    });

    let del = sqlx::query(&format!(
        "DELETE FROM scheduled_message_context_snapshots \
         WHERE {CONTEXT_SNAPSHOT_RECLAIMABLE_PREDICATE}"
    ))
    .bind(CONTEXT_SNAPSHOT_RETENTION_DAYS)
    .execute(&mut *tx)
    .await?;
    report.push(TableReport {
        table_name: "scheduled_message_context_snapshots",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });

    tx.commit().await?;
    Ok(())
}

// An intake_outbox row is a candidate only when it, its whole (channel_id, user_msg_id)
// attempt family, and every child are allowlisted terminal and aged.
fn intake_outbox_aged_terminal(alias: &str) -> String {
    format!(
        "({alias}.status IN ('done', 'unknown', 'failed_pre_accept', 'failed_post_accept') \
          AND (({alias}.status = 'done' \
                AND {alias}.updated_at < NOW() - ($1::INT || ' days')::INTERVAL) \
               OR ({alias}.status IN ('unknown', 'failed_pre_accept', 'failed_post_accept') \
                AND {alias}.updated_at < NOW() - ($2::INT || ' days')::INTERVAL)))"
    )
}

fn intake_outbox_candidate_predicate() -> String {
    let (row, family, child) = (
        intake_outbox_aged_terminal("io"),
        intake_outbox_aged_terminal("f"),
        intake_outbox_aged_terminal("c"),
    );
    format!(
        "{row} \
         AND NOT EXISTS (SELECT 1 FROM intake_outbox f \
             WHERE f.channel_id = io.channel_id AND f.user_msg_id = io.user_msg_id \
               AND NOT {family}) \
         AND NOT EXISTS (SELECT 1 FROM intake_outbox c \
             WHERE c.parent_outbox_id = io.id AND NOT {child})"
    )
}

/// Runs the candidate COUNT under a transaction-local `statement_timeout`. Any
/// cancel (timeout or operator) is reported as `candidates_cancelled`, not an error.
async fn count_intake_outbox_candidates_bounded(
    pool: &PgPool,
    statement_timeout: std::time::Duration,
    report: &mut RetentionReport,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(format!("{}ms", statement_timeout.as_millis()))
        .execute(&mut *tx)
        .await?;
    let counted = count_intake_outbox_candidates(&mut *tx, report).await;
    tx.rollback().await?;
    match counted {
        Err(error) if is_query_canceled(&error) => {
            tracing::warn!(
                timeout_ms = statement_timeout.as_millis() as u64,
                "[db_retention] intake_outbox candidate count not completed \
                 (statement timeout or cancellation); backlog unknown"
            );
            report.push(TableReport {
                table_name: "intake_outbox",
                action: "candidates_cancelled",
                rows_affected: 0,
            });
            Ok(())
        }
        other => other,
    }
}

// 57014 means timeout or pg_cancel_backend; the two cannot be told apart reliably.
fn is_query_canceled(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<sqlx::Error>(),
        Some(sqlx::Error::Database(db)) if db.code().as_deref() == Some("57014")
    )
}

/// Reports the uncapped candidate backlog, in total and per status, as seen by
/// one statement snapshot. Always runs, dry-run or not, and never deletes.
async fn count_intake_outbox_candidates<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    report: &mut RetentionReport,
) -> Result<()> {
    let row = sqlx::query(&format!(
        "SELECT COUNT(*)::BIGINT AS total, \
                COUNT(*) FILTER (WHERE io.status = 'done')::BIGINT AS done, \
                COUNT(*) FILTER (WHERE io.status = 'unknown')::BIGINT AS unknown, \
                COUNT(*) FILTER (WHERE io.status = 'failed_pre_accept')::BIGINT AS failed_pre, \
                COUNT(*) FILTER (WHERE io.status = 'failed_post_accept')::BIGINT AS failed_post \
         FROM intake_outbox io WHERE {}",
        intake_outbox_candidate_predicate()
    ))
    .bind(INTAKE_OUTBOX_DONE_RETENTION_DAYS)
    .bind(INTAKE_OUTBOX_FAILED_RETENTION_DAYS)
    .fetch_one(executor)
    .await?;
    for (table_name, action, column) in [
        ("intake_outbox", "candidates", "total"),
        ("intake_outbox.done", "status_candidates", "done"),
        ("intake_outbox.unknown", "status_candidates", "unknown"),
        (
            "intake_outbox.failed_pre_accept",
            "status_candidates",
            "failed_pre",
        ),
        (
            "intake_outbox.failed_post_accept",
            "status_candidates",
            "failed_post",
        ),
    ] {
        report.push(TableReport {
            table_name,
            action,
            rows_affected: row.try_get(column)?,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Exceeds `i32::MAX`; archiving it fails unless the archive token columns are BIGINT.
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

    /// Inserts one `turns` row aged on `finished_at`, with `tokens` in the duration and
    /// token columns.
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

    /// Seeds one stale and one fresh row in each of the three tables; the stale
    /// `turns` row carries [`BIG_TOKENS`].
    async fn seed_fixtures(pool: &PgPool, stale_days: i32) {
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

    /// An aged snapshot is kept while an active definition references it and deleted
    /// once none does; an unreferenced aged snapshot goes on the first pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_context_snapshot_pg_reference_gate() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_4658_snapshots",
            "db_retention #4658 context snapshot referenced/aged policy",
        )
        .await;
        let pool = db.connect_and_migrate().await;

        let hex64 = "0".repeat(64);
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
        // `push` delivery avoids the agents FK.
        sqlx::query(
            "INSERT INTO scheduled_messages
                (id, content, target_channel_id, delivery_kind, scheduled_at, status,
                 context_strategy, context_snapshot_id)
             VALUES ('smsg_ref', 'c', '1', 'push', NOW(), 'scheduled', 'snapshot', 'smcs_active')",
        )
        .execute(&pool)
        .await
        .expect("seed referencing active definition");

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

    /// Only a snapshot whose referencing definitions are all terminal and aged is reclaimed;
    /// `smcs_active` surviving catches that condition being dropped from the predicate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_context_snapshot_terminal_reclaim() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_4723_reclaim",
            "db_retention #4723 terminal-definition snapshot reclaim",
        )
        .await;
        let pool = db.connect_and_migrate().await;

        let hex64 = "0".repeat(64);
        for id in ["smcs_term_aged", "smcs_term_fresh", "smcs_active"] {
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

        // `push` delivery avoids the agents FK.
        for (def_id, snap_id, status, updated) in [
            (
                "smsg_term_aged",
                "smcs_term_aged",
                "canceled",
                "NOW() - INTERVAL '40 days'",
            ),
            ("smsg_term_fresh", "smcs_term_fresh", "sent", "NOW()"),
            ("smsg_active", "smcs_active", "scheduled", "NOW()"),
        ] {
            sqlx::query(&format!(
                "INSERT INTO scheduled_messages
                    (id, content, target_channel_id, delivery_kind, scheduled_at,
                     status, context_strategy, context_snapshot_id, updated_at)
                 VALUES ($1, 'c', '1', 'push', NOW(), $2, 'snapshot', $3, {updated})"
            ))
            .bind(def_id)
            .bind(status)
            .bind(snap_id)
            .execute(&pool)
            .await
            .expect("seed referencing definition");
        }

        db_retention_job(&pool, false)
            .await
            .expect("retention pass");

        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM scheduled_message_context_snapshots WHERE id = 'smcs_term_aged'"
            )
            .await,
            0,
            "snapshot whose every referencing definition is terminal + aged must be reclaimed"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM scheduled_messages \
                 WHERE id = 'smsg_term_aged' AND context_strategy = 'snapshot' \
                   AND context_snapshot_id IS NULL \
                   AND context_snapshot_reclaimed_at IS NOT NULL"
            )
            .await,
            1,
            "reclaim must keep the definition row, null the FK, preserve 'snapshot' strategy, and stamp reclaimed_at"
        );

        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM scheduled_message_context_snapshots WHERE id = 'smcs_term_fresh'"
            )
            .await,
            1,
            "terminal-but-not-yet-aged definition still pins its snapshot"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM scheduled_messages \
                 WHERE id = 'smsg_term_fresh' AND context_snapshot_id = 'smcs_term_fresh'"
            )
            .await,
            1,
            "not-yet-aged terminal definition keeps its FK reference"
        );

        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*)::BIGINT AS n FROM scheduled_message_context_snapshots WHERE id = 'smcs_active'"
            )
            .await,
            1,
            "snapshot referenced by an active/pending definition must never be reclaimed (AC-9)"
        );

        pool.close().await;
        db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_keeps_skill_usage_rows_with_null_used_at() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_3865_nullusedat",
            "db_retention #3865 NULL used_at survival",
        )
        .await;
        let pool = db.connect_and_migrate().await;

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

    /// A `turns` row just inside the 90-day window survives; one just outside is archived
    /// and deleted, with equal archive and delete counts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_turns_window_is_strict_with_no_archive_less_delete() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_3865_boundary",
            "db_retention #3865 turns strict boundary + atomic archive",
        )
        .await;
        let pool = db.connect_and_migrate().await;

        // 2-hour margins absorb the ms-level drift between seed and job NOW().
        sqlx::query(
            "INSERT INTO turns (turn_id, channel_id, started_at, finished_at) \
             VALUES ('turn-inside', 'chan', NOW(), NOW() - (INTERVAL '90 days' - INTERVAL '2 hours'))",
        )
        .execute(&pool)
        .await
        .expect("seed inside-window turn");
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

    /// Seeds one intake_outbox row whose created/updated_at is `NOW() - age` (set
    /// at INSERT because the BEFORE UPDATE trigger would reset `updated_at`).
    async fn seed_intake<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        (channel, msg, attempt): (&str, &str, i32),
        status: &str,
        age: &str,
        parent: Option<i64>,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO intake_outbox \
                 (target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id, \
                  request_owner_id, user_text, turn_kind, agent_id, status, attempt_no, \
                  parent_outbox_id, dispatched_at, created_at, updated_at) \
             VALUES ('node', 'leader', $1, $2, 'owner', 'text', 'normal', 'agent', $3, $4, $5, \
                     CASE WHEN $3 = 'dispatched' THEN NOW() END, \
                     NOW() - $6::INTERVAL, NOW() - $6::INTERVAL) \
             RETURNING id",
        )
        .bind(channel)
        .bind(msg)
        .bind(status)
        .bind(attempt)
        .bind(parent)
        .bind(age)
        .fetch_one(executor)
        .await
        .unwrap_or_else(|err| panic!("seed intake_outbox {channel}/{msg}: {err}"))
    }

    fn candidates(report: &RetentionReport, table: &str) -> Option<i64> {
        let action = if table.contains('.') {
            "status_candidates"
        } else {
            "candidates"
        };
        report.get(table, action).map(|e| e.rows_affected)
    }

    /// Only allowlisted terminal rows strictly older than their own status
    /// window count; seeding and counting share one transaction, so one NOW().
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_candidates_use_exact_status_windows_and_allowlist() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_intake_outbox_window",
            "db_retention intake_outbox status windows",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let mut tx = pool.begin().await.expect("begin");
        // Stand-in for a future migration adding statuses the allowlist must not match.
        sqlx::query("ALTER TABLE intake_outbox DROP CONSTRAINT intake_outbox_status_check")
            .execute(&mut *tx)
            .await
            .expect("drop status check");

        let (over_7, over_30) = ("7 days 1 microsecond", "30 days 1 microsecond");
        let rows = [
            ("done-over", "done", over_7),
            ("done-at", "done", "7 days"),
            ("unknown-over", "unknown", over_30),
            ("unknown-at", "unknown", "30 days"),
            ("fpre-over", "failed_pre_accept", over_30),
            ("fpre-at", "failed_pre_accept", "30 days"),
            ("fpost-over", "failed_post_accept", over_30),
            ("fpost-at", "failed_post_accept", "30 days"),
            ("fpost-8d", "failed_post_accept", "8 days"),
            ("pending", "pending", "60 days"),
            ("claimed", "claimed", "60 days"),
            ("accepted", "accepted", "60 days"),
            ("spawned", "spawned", "60 days"),
            ("dispatched", "dispatched", "60 days"),
            ("upper-done", "DONE", "60 days"),
            ("padded-done", "done ", "60 days"),
            ("future", "archived", "60 days"),
        ];
        for (key, status, age) in rows {
            seed_intake(&mut *tx, (key, key, 1), status, age, None).await;
        }

        let mut report = RetentionReport::default();
        count_intake_outbox_candidates(&mut *tx, &mut report)
            .await
            .expect("count intake_outbox candidates");

        assert_eq!(candidates(&report, "intake_outbox"), Some(4));
        for status in ["done", "unknown", "failed_pre_accept", "failed_post_accept"] {
            let table = format!("intake_outbox.{status}");
            assert_eq!(candidates(&report, &table), Some(1), "{table}");
        }

        tx.rollback().await.expect("rollback");
        pool.close().await;
        db.drop().await;
    }

    /// A row is not a candidate while any attempt in its family, or any row
    /// naming it as parent, is non-terminal or still inside its window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_candidates_exclude_rows_with_live_family_or_children() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_intake_outbox_family",
            "db_retention intake_outbox parent/child preservation",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let (old, pre, post) = ("60 days", "failed_pre_accept", "failed_post_accept");

        let live = seed_intake(&pool, ("c-live", "m", 1), pre, old, None).await;
        seed_intake(&pool, ("c-live", "m", 2), "pending", "0 days", Some(live)).await;
        let young = seed_intake(&pool, ("c-young", "m", 1), pre, old, None).await;
        seed_intake(&pool, ("c-young", "m", 2), "done", "1 day", Some(young)).await;
        let cross = seed_intake(&pool, ("c-cross", "m", 1), post, old, None).await;
        seed_intake(&pool, ("c-other", "m", 1), "claimed", "0 days", Some(cross)).await;
        let a1 = seed_intake(&pool, ("c-chain", "m", 1), pre, old, None).await;
        let a2 = seed_intake(&pool, ("c-chain", "m", 2), pre, old, Some(a1)).await;
        seed_intake(&pool, ("c-chain", "m", 3), "pending", "0 days", Some(a2)).await;
        let xyoung = seed_intake(&pool, ("c-xyoung", "m", 1), pre, old, None).await;
        seed_intake(
            &pool,
            ("c-xyoung-other", "m", 1),
            "done",
            "1 day",
            Some(xyoung),
        )
        .await;
        let g1 = seed_intake(&pool, ("c-grand", "m", 1), pre, old, None).await;
        let g2 = seed_intake(&pool, ("c-grand", "m", 2), pre, old, Some(g1)).await;
        seed_intake(&pool, ("c-grand", "m", 3), "done", "1 day", Some(g2)).await;
        let gone = seed_intake(&pool, ("c-gone", "m", 1), pre, old, None).await;
        seed_intake(&pool, ("c-gone", "m", 2), post, old, Some(gone)).await;

        let mut report = RetentionReport::default();
        count_intake_outbox_candidates(&pool, &mut report)
            .await
            .expect("count intake_outbox candidates");

        assert_eq!(candidates(&report, "intake_outbox"), Some(2));
        assert_eq!(
            candidates(&report, "intake_outbox.failed_pre_accept"),
            Some(1)
        );
        assert_eq!(
            candidates(&report, "intake_outbox.failed_post_accept"),
            Some(1)
        );

        pool.close().await;
        db.drop().await;
    }

    /// Asserts the neutral "count not completed" outcome with no count and no timeout claim.
    fn assert_count_not_completed(report: &RetentionReport) {
        assert!(
            report
                .get("intake_outbox", "candidates_cancelled")
                .is_some()
        );
        assert_eq!(candidates(report, "intake_outbox"), None);
        assert_eq!(report.total_candidates(), 0);
        assert!(
            report
                .tables
                .iter()
                .all(|t| !t.action.contains("timed_out"))
        );
    }

    /// A COUNT that exceeds its statement_timeout is reported as not completed
    /// and the pass continues; the same call counts normally once unblocked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_candidate_count_timeout_is_reported_not_fatal() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_intake_outbox_timeout",
            "db_retention intake_outbox count timeout",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(2).await;
        seed_intake(&pool, ("t-done", "m", 1), "done", "10 days", None).await;
        let mut blocker = pool.begin().await.expect("begin blocker");
        sqlx::query("LOCK TABLE intake_outbox IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .expect("lock intake_outbox");

        let mut report = RetentionReport::default();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            count_intake_outbox_candidates_bounded(&pool, Duration::from_millis(200), &mut report),
        )
        .await
        .expect("statement_timeout must bound the blocked COUNT")
        .expect("a timed-out count is not fatal");
        assert_count_not_completed(&report);

        blocker.rollback().await.expect("release lock");
        let mut report = RetentionReport::default();
        count_intake_outbox_candidates_bounded(&pool, Duration::from_secs(60), &mut report)
            .await
            .expect("unblocked count");
        assert_eq!(candidates(&report, "intake_outbox"), Some(1));
        assert!(
            report
                .get("intake_outbox", "candidates_cancelled")
                .is_none()
        );

        pool.close().await;
        db.drop().await;
    }

    /// An operator's pg_cancel_backend on the running COUNT, well before the
    /// timeout, gets the same neutral not-completed outcome and is not fatal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_candidate_count_operator_abort_is_reported_not_completed() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_intake_outbox_cancel",
            "db_retention intake_outbox count cancel",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(3).await;
        seed_intake(&pool, ("u-done", "m", 1), "done", "10 days", None).await;
        let mut blocker = pool.begin().await.expect("begin blocker");
        sqlx::query("LOCK TABLE intake_outbox IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .expect("lock intake_outbox");

        let mut report = RetentionReport::default();
        // Polls from autocommit pool connections: pg_stat_activity is snapshotted per transaction.
        let cancel = async {
            loop {
                let cancelled: bool = sqlx::query_scalar(
                    "SELECT COALESCE(bool_or(pg_cancel_backend(pid)), false) FROM pg_stat_activity \
                     WHERE datname = current_database() AND pid <> pg_backend_pid() \
                       AND wait_event_type = 'Lock' \
                       AND query LIKE '%FROM intake_outbox io%'",
                )
                .fetch_one(&pool)
                .await
                .expect("cancel waiting count");
                if cancelled {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        };
        let (counted, ()) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            tokio::join!(
                count_intake_outbox_candidates_bounded(&pool, Duration::from_secs(60), &mut report),
                cancel
            )
        })
        .await
        .expect("cancel must end the blocked COUNT");
        counted.expect("an aborted count is not fatal");
        assert_count_not_completed(&report);

        blocker.rollback().await.expect("release lock");
        pool.close().await;
        db.drop().await;
    }

    /// A COUNT failure other than SQLSTATE 57014 still fails the pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_candidate_count_other_db_error_surfaces() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_intake_outbox_db_error",
            "db_retention intake_outbox count db error",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        sqlx::query("ALTER TABLE intake_outbox RENAME COLUMN updated_at TO updated_at_moved")
            .execute(&pool)
            .await
            .expect("rename updated_at");

        let mut report = RetentionReport::default();
        let error =
            count_intake_outbox_candidates_bounded(&pool, Duration::from_secs(60), &mut report)
                .await
                .expect_err("undefined column must fail the count");
        let code = match error.downcast_ref::<sqlx::Error>() {
            Some(sqlx::Error::Database(db)) => db.code().map(|code| code.into_owned()),
            _ => None,
        };
        assert_eq!(code.as_deref(), Some("42703"), "{error:#}");
        assert!(report.tables.is_empty());

        pool.close().await;
        db.drop().await;
    }

    /// The scheduled job reports intake_outbox candidates apart from deletions
    /// and leaves every row in place.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_retention_job_reports_intake_outbox_candidates_without_deleting() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_db_retention_intake_outbox_count_only",
            "db_retention intake_outbox count-only wiring",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let post = "failed_post_accept";
        seed_intake(&pool, ("j-done", "m", 1), "done", "10 days", None).await;
        seed_intake(&pool, ("j-fpost1", "m", 1), post, "40 days", None).await;
        seed_intake(&pool, ("j-fpost2", "m", 1), post, "40 days", None).await;
        seed_intake(&pool, ("j-new", "m", 1), "done", "1 hour", None).await;

        let report = db_retention_job(&pool, false)
            .await
            .expect("retention pass");

        assert_eq!(report.total_candidates(), 3);
        assert_eq!(report.total_deleted(), 0);
        assert_eq!(candidates(&report, "intake_outbox.done"), Some(1));
        assert_eq!(
            candidates(&report, "intake_outbox.failed_post_accept"),
            Some(2)
        );
        let remaining = count(&pool, "SELECT COUNT(*)::BIGINT AS n FROM intake_outbox").await;
        assert_eq!(remaining, 4);

        pool.close().await;
        db.drop().await;
    }
}
