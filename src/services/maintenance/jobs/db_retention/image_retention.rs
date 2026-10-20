//! Reuse the retention pass for terminal one-shot definition bytes.

use super::*;

pub(super) async fn retain(
    pool: &PgPool,
    dry_run: bool,
    report: &mut RetentionReport,
) -> Result<()> {
    super::retain_context_snapshots(pool, dry_run, report).await?;
    let before = chrono::Utc::now() - chrono::Duration::days(7);
    let rows =
        crate::db::scheduled_messages::reclaim_terminal_images_pg(pool, before, dry_run).await?;
    report.push(TableReport {
        table_name: "scheduled_messages",
        action: if dry_run {
            "reclaim_image_bytes_would"
        } else {
            "reclaim_image_bytes"
        },
        rows_affected: rows,
    });
    Ok(())
}
