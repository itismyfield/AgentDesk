//! DB retention job (#1093 / 909-4; extended in #3865).
//!
//! Eight retention policies across the AgentDesk postgres backbone:
//!
//! | Table                    | Retention | Strategy                          |
//! |--------------------------|-----------|-----------------------------------|
//! | `agent_quality_event`    | 90 days   | Monthly aggregate, then DELETE    |
//! | `session_transcripts`    | 90 days   | Archive-table copy, then DELETE   |
//! | `message_outbox` (sent)  | 7 days    | DELETE                            |
//! | `auto_queue_entries`     | 30 days   | DELETE (status='completed')       |
//! | `task_dispatches`        | 90 days   | Monthly aggregate, then DELETE    |
//! | `turn_lifecycle_events`  | 30 days   | DELETE (on `created_at`)          |
//! | `skill_usage`            | 90 days   | DELETE (on `used_at`)             |
//! | `turns`                  | 90 days   | Archive-table copy, then DELETE   |
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
// 3. message_outbox: delete sent rows older than 7 days.
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
               AND sent_at < NOW() - ($1::INT || ' days')::INTERVAL",
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
           AND sent_at < NOW() - ($1::INT || ' days')::INTERVAL",
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
// Mirrors `retain_session_transcripts`: copy into `turns_archive` (idempotent
// via WHERE NOT EXISTS) so historical token/cost totals stay queryable, then
// DELETE from the hot table. `finished_at` is NOT NULL → no NULL edge cases.
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
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "turns_archive",
        action: "insert",
        rows_affected: archived.rows_affected() as i64,
    });

    let del = sqlx::query(
        "DELETE FROM turns \
         WHERE finished_at < NOW() - ($1::INT || ' days')::INTERVAL",
    )
    .bind(TURNS_RETENTION_DAYS)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: "turns",
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

    async fn count(pool: &PgPool, sql: &str) -> i64 {
        sqlx::query(sql)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|err| panic!("count query `{sql}`: {err}"))
            .try_get::<i64, _>("n")
            .unwrap_or(0)
    }

    /// Seed one stale row (older than the policy window) and one fresh row in
    /// each of the three tables. `stale_days` puts the stale row safely past
    /// the largest (90d) window.
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

        // turns: stale + fresh (windowed on finished_at).
        for (turn_id, age) in [("turn-old", stale_days), ("turn-new", 0)] {
            sqlx::query(
                "INSERT INTO turns \
                     (turn_id, channel_id, started_at, finished_at, input_tokens, output_tokens) \
                 VALUES ($1, 'chan', \
                         NOW() - ($2::INT || ' days')::INTERVAL, \
                         NOW() - ($2::INT || ' days')::INTERVAL, 10, 20)",
            )
            .bind(turn_id)
            .bind(age)
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("seed turns {turn_id}: {err}"));
        }
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
}
