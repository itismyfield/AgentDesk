use sqlx::{Postgres, Row as SqlxRow, Transaction};

use super::model::{DispatchOutboxClaimCandidate, StaleDispatchOutboxClaimOwnerCandidate};

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
                AND o.wait_reason IS NULL
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
                claim_owner = $2,
                wait_reason = NULL,
                wait_started_at = NULL
          WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(claim_owner)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn select_stale_dispatch_outbox_claim_owner_candidates_pg(
    tx: &mut Transaction<'_, Postgres>,
    stale_threshold_secs: i64,
    limit: i64,
) -> Result<Vec<StaleDispatchOutboxClaimOwnerCandidate>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT
            o.id,
            o.dispatch_id,
            o.action,
            COALESCE(o.required_capabilities, td.required_capabilities) AS required_capabilities,
            o.claim_owner AS stale_claim_owner,
            wn.last_heartbeat_at AS stale_owner_last_heartbeat_at
         FROM dispatch_outbox o
         LEFT JOIN task_dispatches td ON td.id = o.dispatch_id
         LEFT JOIN worker_nodes wn ON wn.instance_id = o.claim_owner
         WHERE o.status = 'pending'
           AND o.claim_owner IS NOT NULL
           AND (
                wn.instance_id IS NULL
             OR wn.status <> 'online'
             OR wn.last_heartbeat_at IS NULL
             OR wn.last_heartbeat_at < NOW() - ($1::bigint * INTERVAL '1 second')
           )
         ORDER BY o.id ASC
         FOR UPDATE OF o SKIP LOCKED
         LIMIT $2",
    )
    .bind(stale_threshold_secs.max(1))
    .bind(limit.clamp(1, 500))
    .fetch_all(&mut **tx)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(StaleDispatchOutboxClaimOwnerCandidate {
                id: row.try_get("id")?,
                dispatch_id: row.try_get("dispatch_id")?,
                action: row.try_get("action")?,
                required_capabilities: row.try_get("required_capabilities")?,
                stale_claim_owner: row.try_get("stale_claim_owner")?,
                stale_owner_last_heartbeat_at: row.try_get("stale_owner_last_heartbeat_at")?,
            })
        })
        .collect()
}

pub(crate) async fn update_dispatch_outbox_claim_owner_pg(
    tx: &mut Transaction<'_, Postgres>,
    outbox_id: i64,
    claim_owner: Option<&str>,
    diagnostics: &serde_json::Value,
) -> Result<u64, sqlx::Error> {
    let constraint_results = diagnostics.get("constraint_results");
    let result = sqlx::query(
        "UPDATE dispatch_outbox
            SET claim_owner = $2,
                routing_diagnostics = $3,
                constraint_results = $4
          WHERE id = $1
            AND status = 'pending'",
    )
    .bind(outbox_id)
    .bind(claim_owner)
    .bind(diagnostics)
    .bind(constraint_results)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}
