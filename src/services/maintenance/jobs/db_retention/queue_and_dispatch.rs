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

    // Each pass adds the rows it deletes to their month; NOW() is fixed for the
    // transaction, so the rollup and the DELETE see the same rows.
    let mut tx = pool.begin().await?;
    let agg = sqlx::query(
        "INSERT INTO task_dispatches_monthly_aggregate AS a \
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
         ON CONFLICT (month) DO UPDATE \
         SET total_dispatches = a.total_dispatches + EXCLUDED.total_dispatches, \
             completed_count = a.completed_count + EXCLUDED.completed_count, \
             review_count = a.review_count + EXCLUDED.review_count, \
             aggregated_at = EXCLUDED.aggregated_at",
    )
    .bind(DISPATCH_RETENTION_DAYS)
    .execute(&mut *tx)
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
    .execute(&mut *tx)
    .await?;
    report.push(TableReport {
        table_name: "task_dispatches",
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    tx.commit().await?;
    Ok(())
}
