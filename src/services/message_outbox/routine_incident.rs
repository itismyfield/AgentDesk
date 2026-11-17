//! Same-row transport state of the routine failure incident: `posting` is committed before the
//! Discord post, then acked with the message id or left `unknown`, fenced by migration 0192.
use sqlx::PgPool;

const SOURCE: &str = "routine-runtime";
const REASON_CODE: &str = "routine_failure_incident";
/// A `posting` row whose worker is gone after this long is closed as `unknown`.
const ABANDONED_POSTING_SECS: i64 = 10 * 60;

pub(crate) fn is_incident(source: &str, reason_code: Option<&str>) -> bool {
    source == SOURCE && reason_code == Some(REASON_CODE)
}

/// One claimed row's lease; every transition is a CAS on it.
#[derive(Clone, Copy)]
pub(crate) struct Lease<'a> {
    pub id: i64,
    pub claim_owner: &'a str,
    pub claimed_at: chrono::DateTime<chrono::Utc>,
}

/// Moves a claimed incident row out of the worker queue before its post; false if the lease was lost.
pub(crate) async fn begin_posting(pool: &PgPool, lease: Lease<'_>) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE message_outbox SET status = 'held', transport_state = 'posting', error = NULL
          WHERE id = $1 AND status = 'processing' AND claim_owner = $2 AND claimed_at = $3
            AND transport_state IS NULL AND source = $4 AND reason_code = $5",
    )
    .bind(lease.id)
    .bind(lease.claim_owner)
    .bind(lease.claimed_at)
    .bind(SOURCE)
    .bind(REASON_CODE)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// Records the Discord message id on the same row and lease; a slow post the sweep already closed
/// as `unknown` may still be confirmed by the worker that holds its lease.
pub(crate) async fn ack(
    pool: &PgPool,
    lease: Lease<'_>,
    message_id: &str,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE message_outbox
            SET status = 'sent', sent_at = NOW(), transport_state = 'acked',
                transport_message_id = $4, error = NULL, claim_owner = NULL, claimed_at = NULL
          WHERE id = $1 AND transport_state IN ('posting', 'unknown')
            AND claim_owner = $2 AND claimed_at = $3",
    )
    .bind(lease.id)
    .bind(lease.claim_owner)
    .bind(lease.claimed_at)
    .bind(message_id)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// The post may have reached Discord: keep the held fence and never post again.
pub(crate) async fn mark_unknown(
    pool: &PgPool,
    lease: Lease<'_>,
    error: &str,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE message_outbox SET transport_state = 'unknown', error = $4
          WHERE id = $1 AND transport_state = 'posting' AND claim_owner = $2 AND claimed_at = $3",
    )
    .bind(lease.id)
    .bind(lease.claim_owner)
    .bind(lease.claimed_at)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// The send refused before posting: return the row to the ordinary retry or failure settle.
pub(crate) async fn release_not_sent(
    pool: &PgPool,
    lease: Lease<'_>,
    error: &str,
    retry: Option<(i64, i64)>,
) -> Result<bool, sqlx::Error> {
    let (status, retry_count, backoff_secs) = match retry {
        Some((retry_count, backoff_secs)) => ("pending", retry_count, backoff_secs),
        None => ("failed", 0, 0),
    };
    let changed = sqlx::query(
        "UPDATE message_outbox
            SET status = $4, transport_state = NULL, error = $5,
                retry_count = CASE WHEN $4 = 'pending' THEN $6 ELSE retry_count END,
                next_attempt_at = CASE WHEN $4 = 'pending'
                                       THEN NOW() + ($7::BIGINT * INTERVAL '1 second') END,
                claim_owner = NULL, claimed_at = NULL
          WHERE id = $1 AND transport_state = 'posting' AND claim_owner = $2 AND claimed_at = $3",
    )
    .bind(lease.id)
    .bind(lease.claim_owner)
    .bind(lease.claimed_at)
    .bind(status)
    .bind(error)
    .bind(retry_count)
    .bind(backoff_secs)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// A post whose outcome was lost to a restart or an unsaved ack terminates as `unknown`.
pub(crate) async fn close_abandoned_posting(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE message_outbox
            SET transport_state = 'unknown', error = 'post outcome lost before its ack was stored'
          WHERE reason_code = $1 AND transport_state = 'posting'
            AND claimed_at <= NOW() - ($2::BIGINT * INTERVAL '1 second')",
    )
    .bind(REASON_CODE)
    .bind(ABANDONED_POSTING_SECS)
    .execute(pool)
    .await?
    .rows_affected())
}
