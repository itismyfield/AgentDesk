use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::delivery::DispatchOutboxLeaseUpdateError;

/// Reschedule an outbox row for retry with `backoff_secs` delay.
pub(crate) async fn schedule_outbox_retry_pg(
    pool: &PgPool,
    outbox_id: i64,
    error_message: &str,
    new_count: i64,
    backoff_secs: i64,
    claim_owner: &str,
    claimed_at: DateTime<Utc>,
) -> Result<(), DispatchOutboxLeaseUpdateError> {
    let result = sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'pending',
                error = $1,
                retry_count = $2,
                next_attempt_at = NOW() + ($3::bigint * INTERVAL '1 second'),
                claimed_at = NULL,
                claim_owner = NULL
          WHERE id = $4
            AND claim_owner = $5
            AND claimed_at = $6",
    )
    .bind(error_message)
    .bind(new_count)
    .bind(backoff_secs)
    .bind(outbox_id)
    .bind(claim_owner)
    .bind(claimed_at)
    .execute(pool)
    .await
    .map_err(|error| {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            %error,
            "[dispatch-outbox] db failure while scheduling outbox row retry"
        );
        DispatchOutboxLeaseUpdateError::Db(error)
    })?;
    if result.rows_affected() == 0 {
        tracing::warn!(
            outbox_id,
            claim_owner,
            %claimed_at,
            "[dispatch-outbox] stale lease no-op while scheduling outbox row retry"
        );
        return Err(DispatchOutboxLeaseUpdateError::StaleLeaseLost {
            outbox_id,
            claim_owner: claim_owner.to_string(),
            claimed_at,
        });
    }
    Ok(())
}
