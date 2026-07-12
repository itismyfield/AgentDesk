use sqlx::{PgPool, Row};

/// Final delivery state of the outbox rows a delivery handed off to, keyed by
/// outbox row id. Used to lazily enrich the deliveries API response.
pub async fn outbox_statuses_for_deliveries_pg(
    pool: &PgPool,
    outbox_ids: &[i64],
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    if outbox_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query("SELECT id, status FROM message_outbox WHERE id = ANY($1)")
        .bind(outbox_ids)
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get::<i64, _>("id")?,
                row.try_get::<String, _>("status")?,
            ))
        })
        .collect()
}
