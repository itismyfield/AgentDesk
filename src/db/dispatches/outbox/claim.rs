use sqlx::{Postgres, Row as SqlxRow, Transaction};

use super::model::DispatchOutboxClaimCandidate;

const DISPATCH_OUTBOX_CLAIM_STALE_SECS: i64 = 300;

pub(crate) async fn select_pending_dispatch_outbox_claim_candidates_pg(
    tx: &mut Transaction<'_, Postgres>,
    claim_owner: &str,
) -> Result<Vec<DispatchOutboxClaimCandidate>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT
            o.id,
            o.dispatch_id,
            o.action,
            o.agent_id,
            o.card_id,
            o.title,
            o.retry_count,
            COALESCE(o.required_capabilities, td.required_capabilities) AS required_capabilities
         FROM dispatch_outbox o
         LEFT JOIN task_dispatches td ON td.id = o.dispatch_id
         WHERE (
                o.status = 'pending'
                AND (o.next_attempt_at IS NULL OR o.next_attempt_at <= NOW())
                AND (o.claim_owner IS NULL OR o.claim_owner = $2)
             )
            OR (
                o.status = 'processing'
                AND (
                    o.claimed_at IS NULL
                    OR o.claimed_at <= NOW() - ($1::bigint * INTERVAL '1 second')
                )
            )
         ORDER BY o.id ASC
         FOR UPDATE OF o SKIP LOCKED
         LIMIT 20",
    )
    .bind(DISPATCH_OUTBOX_CLAIM_STALE_SECS)
    .bind(claim_owner)
    .fetch_all(&mut **tx)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(DispatchOutboxClaimCandidate {
                id: row.try_get("id")?,
                dispatch_id: row.try_get("dispatch_id")?,
                action: row.try_get("action")?,
                agent_id: row.try_get("agent_id")?,
                card_id: row.try_get("card_id")?,
                title: row.try_get("title")?,
                retry_count: row.try_get("retry_count")?,
                required_capabilities: row.try_get("required_capabilities")?,
            })
        })
        .collect()
}

pub(crate) async fn mark_dispatch_outbox_claimed_pg(
    tx: &mut Transaction<'_, Postgres>,
    outbox_id: i64,
    claim_owner: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'processing',
                claimed_at = NOW(),
                claim_owner = $2
          WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(claim_owner)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
