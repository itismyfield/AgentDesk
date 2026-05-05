use sqlx::PgPool;

/// Reschedule an outbox row for retry with `backoff_secs` delay.
pub(crate) async fn schedule_outbox_retry_pg(
    pool: &PgPool,
    outbox_id: i64,
    error_message: &str,
    new_count: i64,
    backoff_secs: i64,
) {
    sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'pending',
                error = $1,
                retry_count = $2,
                next_attempt_at = NOW() + ($3::bigint * INTERVAL '1 second'),
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $4",
    )
    .bind(error_message)
    .bind(new_count)
    .bind(backoff_secs)
    .bind(outbox_id)
    .execute(pool)
    .await
    .ok();
}
