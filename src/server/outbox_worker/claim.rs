//! Claim an id-ordered prefix holding at most one image, keeping the existing lease.

use crate::server::*;
use crate::services::cluster::intake_router_hook::owner_record::event as routing;

fn routing_reclaim_seconds() -> i64 {
    #[cfg(test)]
    if crate::services::cluster::intake_router_hook::owner_record::cleanup::tests::mutant(
        "notice-lease",
    ) {
        return MESSAGE_OUTBOX_CLAIM_STALE_SECS;
    }
    routing::RECLAIM_SECONDS
}

pub(in crate::server) async fn claim_pending_message_outbox_batch_pg(
    pool: &PgPool,
    claim_owner: &str,
) -> Vec<PendingMessageOutboxRow> {
    let rows = match sqlx::query(
        "WITH capability AS MATERIALIZED (
            SELECT set_config('agentdesk.outbox_image_consumer_v1', 'enabled', true)
        ), candidates AS (

            SELECT id, attachment_data IS NOT NULL AS image
              FROM message_outbox CROSS JOIN capability
             WHERE (
                    status = 'pending'
                    AND (
                        next_attempt_at IS NULL
                        OR next_attempt_at <= NOW()
                    )
                 )
                OR (
                    status = 'processing'
                    AND (
                        claimed_at IS NULL
                        OR claimed_at <= NOW() - ((CASE WHEN source = $3 THEN $4 ELSE $1 END)::bigint * INTERVAL '1 second')
                    )
                )
             ORDER BY id ASC
             FOR UPDATE SKIP LOCKED
             LIMIT 10
        ), claimed AS (
            -- Strict id-order prefix that stops before the second image.
            SELECT id FROM (
                SELECT id, COUNT(*) FILTER (WHERE image) OVER (ORDER BY id) AS images
                  FROM candidates
            ) ordered
             WHERE images <= 1
        )
        UPDATE message_outbox mo
           SET status = 'processing',
               claimed_at = NOW(),
               claim_owner = $2,
               error = NULL
          FROM claimed
         WHERE mo.id = claimed.id
        RETURNING mo.id, mo.target, mo.content, mo.bot, mo.source, mo.reason_code, mo.session_key, mo.retry_count, mo.claim_owner, mo.claimed_at,
                  mo.attachment_filename, mo.attachment_content_type, mo.attachment_data",
    )
    .bind(MESSAGE_OUTBOX_CLAIM_STALE_SECS)
    .bind(claim_owner)
    .bind(routing::SOURCE)
    .bind(routing_reclaim_seconds())
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!("[outbox-pg] failed to claim message_outbox rows: {error}");
            return Vec::new();
        }
    };

    let mut claimed = rows
        .into_iter()
        .filter_map(|row| {
            Some(PendingMessageOutboxRow {
                id: row.try_get::<i64, _>("id").ok()?,
                target: row.try_get::<String, _>("target").ok()?,
                content: row.try_get::<String, _>("content").ok()?,
                bot: row.try_get::<String, _>("bot").ok()?,
                source: row.try_get::<String, _>("source").ok()?,
                reason_code: row.try_get::<Option<String>, _>("reason_code").ok()?,
                session_key: row.try_get::<Option<String>, _>("session_key").ok()?,
                retry_count: row.try_get::<i64, _>("retry_count").unwrap_or(0),
                attachment: crate::services::message_outbox::decode_claimed_attachment(&row),
                claim_owner: row.try_get::<String, _>("claim_owner").ok()?,
                claimed_at: row
                    .try_get::<chrono::DateTime<chrono::Utc>, _>("claimed_at")
                    .ok()?,
            })
        })
        .collect::<Vec<_>>();
    claimed.sort_by_key(|row| row.id);
    claimed
}
