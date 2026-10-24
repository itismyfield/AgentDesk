//! Image metadata projections and terminal byte retention.

use super::*;

pub async fn get_scheduled_message_metadata_pg(
    pool: &PgPool,
    id: &str,
) -> Result<Option<ScheduledMessageRow>, sqlx::Error> {
    sqlx::query_as(AssertSqlSafe(format!(
        "SELECT {DEFINITION_COLUMNS}, {LIST_IMAGE_COLUMNS} FROM scheduled_messages WHERE id=$1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn reclaim_terminal_images_pg(
    pool: &PgPool,
    before: DateTime<Utc>,
    dry_run: bool,
) -> Result<i64, sqlx::Error> {
    let predicate = "image_data IS NOT NULL AND schedule IS NULL
        AND status IN ('sent', 'failed', 'expired', 'canceled') AND updated_at < $1";
    if dry_run {
        return sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT COUNT(*)::bigint FROM scheduled_messages WHERE {predicate}"
        )))
        .bind(before)
        .fetch_one(pool)
        .await;
    }
    let result = sqlx::query(AssertSqlSafe(format!(
        "UPDATE scheduled_messages SET image_filename=NULL, image_content_type=NULL, image_data=NULL
         WHERE {predicate}"
    )))
    .bind(before)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() as i64)
}

#[cfg(test)]
#[path = "image_lifecycle_tests.rs"]
mod tests;
