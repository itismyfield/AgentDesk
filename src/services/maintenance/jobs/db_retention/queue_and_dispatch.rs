//! Aged terminal auto-queue entries and completed dispatches leave the hot tables.

use super::*;

// Entries end as done/skipped/failed/user_cancelled; only a finished run's entries go,
// because an active or paused run still reads its own.
pub(super) async fn retain_auto_queue_entries(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    if dry_run {
        let would = sqlx::query(
            "SELECT COUNT(*)::BIGINT AS n FROM auto_queue_entries e \
             WHERE e.status IN ('done', 'skipped', 'failed', 'user_cancelled') \
               AND e.completed_at IS NOT NULL \
               AND e.completed_at < NOW() - ($1::INT || ' days')::INTERVAL \
               AND EXISTS (SELECT 1 FROM auto_queue_runs r \
                           WHERE r.id = e.run_id AND r.status IN ('completed', 'cancelled'))",
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
        "DELETE FROM auto_queue_entries e \
         WHERE e.status IN ('done', 'skipped', 'failed', 'user_cancelled') \
           AND e.completed_at IS NOT NULL \
           AND e.completed_at < NOW() - ($1::INT || ' days')::INTERVAL \
           AND EXISTS (SELECT 1 FROM auto_queue_runs r \
                       WHERE r.id = e.run_id AND r.status IN ('completed', 'cancelled'))",
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

pub(super) async fn retain_task_dispatches(
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

    // One statement: only the rows this pass actually deleted are added to their month,
    // so two passes racing on the same rows cannot count them twice.
    let row = sqlx::query(
        "WITH removed AS ( \
             DELETE FROM task_dispatches \
             WHERE status = 'completed' \
               AND completed_at IS NOT NULL \
               AND completed_at < NOW() - ($1::INT || ' days')::INTERVAL \
             RETURNING completed_at, status, dispatch_type \
         ), rolled AS ( \
             INSERT INTO task_dispatches_monthly_aggregate AS a \
                 (month, total_dispatches, completed_count, review_count, aggregated_at) \
             SELECT date_trunc('month', completed_at)::DATE, \
                    COUNT(*)::BIGINT, \
                    COUNT(*) FILTER (WHERE status = 'completed')::BIGINT, \
                    COUNT(*) FILTER (WHERE dispatch_type = 'review')::BIGINT, \
                    NOW() \
             FROM removed \
             GROUP BY date_trunc('month', completed_at) \
             ON CONFLICT (month) DO UPDATE \
             SET total_dispatches = a.total_dispatches + EXCLUDED.total_dispatches, \
                 completed_count = a.completed_count + EXCLUDED.completed_count, \
                 review_count = a.review_count + EXCLUDED.review_count, \
                 aggregated_at = EXCLUDED.aggregated_at \
             RETURNING 1 \
         ) \
         SELECT (SELECT COUNT(*) FROM rolled)::BIGINT AS months, \
                (SELECT COUNT(*) FROM removed)::BIGINT AS deleted",
    )
    .bind(DISPATCH_RETENTION_DAYS)
    .fetch_one(pool)
    .await?;
    report.push(TableReport {
        table_name: "task_dispatches_monthly_aggregate",
        action: "insert",
        rows_affected: row.try_get("months")?,
    });
    report.push(TableReport {
        table_name: "task_dispatches",
        action: "delete",
        rows_affected: row.try_get("deleted")?,
    });
    Ok(())
}
