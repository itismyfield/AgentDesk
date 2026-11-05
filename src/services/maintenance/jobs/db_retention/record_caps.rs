//! Caps the postmortem record tables (transcript archive, supervisor audit log, SLO
//! trend rows) so they keep a bounded window instead of growing forever.

use super::*;

// Runs after `retain_session_transcripts`, so a transcript archived in this pass is
// judged by the archive window, not the hot one.
pub(super) async fn retain(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    for (table, days) in [
        (
            "session_transcripts_archive",
            TRANSCRIPT_ARCHIVE_RETENTION_DAYS,
        ),
        ("runtime_decisions", RUNTIME_DECISIONS_RETENTION_DAYS),
        ("slo_aggregates", SLO_AGGREGATES_RETENTION_DAYS),
    ] {
        delete_aged_by_created_at(pool, dry_run, report, table, days).await?;
    }
    Ok(())
}

// `table` comes only from the fixed list above. Rows with NULL `created_at` are kept.
async fn delete_aged_by_created_at(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
    table: &'static str,
    days: i32,
) -> Result<()> {
    const AGED: &str = "created_at < NOW() - ($1::INT || ' days')::INTERVAL";
    if dry_run {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*)::BIGINT FROM {table} WHERE {AGED}"
        )))
        .bind(days)
        .fetch_one(pool)
        .await?;
        report.push(TableReport {
            table_name: table,
            action: "delete_would",
            rows_affected: n,
        });
        return Ok(());
    }

    let del = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {table} WHERE {AGED}"
    )))
    .bind(days)
    .execute(pool)
    .await?;
    report.push(TableReport {
        table_name: table,
        action: "delete",
        rows_affected: del.rows_affected() as i64,
    });
    Ok(())
}
