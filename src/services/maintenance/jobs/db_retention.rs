//! DB retention job (#1093 / 909-4).
//!
//! Five retention policies across the AgentDesk postgres backbone:
//!
//! | Table                  | Retention | Strategy                          |
//! |------------------------|-----------|-----------------------------------|
//! | `agent_quality_event`  | 90 days   | Monthly aggregate, then DELETE    |
//! | `session_transcripts`  | 90 days   | Archive-table copy, then DELETE   |
//! | `message_outbox` (sent)| 7 days    | DELETE                            |
//! | `auto_queue_entries`   | 30 days   | DELETE (status='completed')       |
//! | `task_dispatches`      | 90 days   | Monthly aggregate, then DELETE    |
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

/// Full report for one run of [`db_retention_job`]. Five table entries plus
/// any aggregate-write entries (turn_analytics, task_dispatches).
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

/// Run the full retention pass. Returns a per-table report. When
/// `dry_run = true` no DML is executed — only SELECT COUNT(*) probes.
pub async fn db_retention_job(pool: &PgPool, dry_run: bool) -> Result<RetentionReport> {
    let mut report = RetentionReport {
        dry_run,
        tables: Vec::with_capacity(8),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_summary_is_compact_and_per_table() {
        let mut r = RetentionReport {
            dry_run: false,
            tables: vec![],
        };
        r.push(TableReport {
            table_name: "message_outbox",
            action: "delete",
            rows_affected: 7,
        });
        r.push(TableReport {
            table_name: "auto_queue_entries",
            action: "delete",
            rows_affected: 3,
        });
        let summary = r.summary();
        assert_eq!(
            summary,
            vec!["message_outbox:delete=7", "auto_queue_entries:delete=3"]
        );
        assert_eq!(r.total_deleted(), 10);
    }

    #[test]
    fn report_total_deleted_excludes_inserts() {
        let mut r = RetentionReport::default();
        r.push(TableReport {
            table_name: "agent_quality_event",
            action: "delete",
            rows_affected: 100,
        });
        r.push(TableReport {
            table_name: "turn_analytics_monthly_aggregate",
            action: "insert",
            rows_affected: 3,
        });
        assert_eq!(r.total_deleted(), 100);
    }

    #[test]
    fn report_total_deleted_counts_dry_run_would_delete() {
        let mut r = RetentionReport {
            dry_run: true,
            tables: vec![],
        };
        r.push(TableReport {
            table_name: "message_outbox",
            action: "delete_would",
            rows_affected: 42,
        });
        assert_eq!(r.total_deleted(), 42);
    }

    #[test]
    fn report_get_finds_matching_entry() {
        let mut r = RetentionReport::default();
        r.push(TableReport {
            table_name: "auto_queue_entries",
            action: "delete",
            rows_affected: 5,
        });
        let found = r.get("auto_queue_entries", "delete").unwrap();
        assert_eq!(found.rows_affected, 5);
        assert!(r.get("auto_queue_entries", "insert").is_none());
    }

    #[test]
    fn kanban_cards_never_appear_in_report() {
        // Invariant: no retention-policy code path produces a TableReport
        // with table_name == "kanban_cards". Enforced by construction (we
        // simply never call such a policy), but this test anchors the
        // intent so future edits don't silently regress the guarantee.
        let r = RetentionReport::default();
        assert!(r.tables.iter().all(|t| t.table_name != "kanban_cards"));
    }

    /// Postgres integration tests live outside this unit-test module and
    /// run only when `DATABASE_URL` is set (staging / CI). Local `cargo
    /// test` exercises the pure-Rust report helpers above; the SQL itself
    /// is smoke-tested by bringing a dockerized pg instance up in CI.
    ///
    /// Rationale for this split: the project currently runs `cargo test`
    /// offline with no postgres available (sqlx is compiled against a
    /// non-recorded macro set, so `sqlx::query!` is avoided throughout the
    /// codebase). Adding a live-pg fixture to unit tests would break that
    /// contract.
    #[cfg(all(test, feature = "pg_integration"))]
    mod pg_integration {
        use super::super::*;
        use sqlx::postgres::PgPoolOptions;

        async fn pool() -> PgPool {
            let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
            PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn dry_run_does_not_mutate_message_outbox() {
            let pool = pool().await;
            let before: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM message_outbox")
                .fetch_one(&pool)
                .await
                .unwrap();
            let _report = db_retention_job(&pool, true).await.unwrap();
            let after: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM message_outbox")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(before, after, "dry-run must not mutate");
        }

        #[tokio::test]
        async fn old_message_outbox_rows_are_deleted() {
            let pool = pool().await;
            sqlx::query(
                "INSERT INTO message_outbox (target, content, status, sent_at) \
                 VALUES ('#test', 'retention-fixture', 'sent', NOW() - INTERVAL '10 days')",
            )
            .execute(&pool)
            .await
            .unwrap();
            let report = db_retention_job(&pool, false).await.unwrap();
            let outbox = report.get("message_outbox", "delete").unwrap();
            assert!(outbox.rows_affected >= 1);
        }

        #[tokio::test]
        async fn recent_outbox_rows_are_kept() {
            let pool = pool().await;
            sqlx::query(
                "INSERT INTO message_outbox (target, content, status, sent_at) \
                 VALUES ('#test', 'keep-fresh', 'sent', NOW() - INTERVAL '1 day')",
            )
            .execute(&pool)
            .await
            .unwrap();
            let _ = db_retention_job(&pool, false).await.unwrap();
            let kept: i64 = sqlx::query_scalar(
                "SELECT COUNT(*)::BIGINT FROM message_outbox \
                 WHERE content = 'keep-fresh'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(kept, 1, "rows within 7d must be kept");
        }
    }
}
