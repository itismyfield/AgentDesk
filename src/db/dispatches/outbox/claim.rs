use serde_json::Value;
use sqlx::{PgPool, Row as SqlxRow};

use super::diagnostics::{record_routing_diagnostics_pg, required_capabilities_empty};
use super::model::DispatchOutboxRow;

const DISPATCH_OUTBOX_CLAIM_STALE_SECS: i64 = 300;

pub(crate) async fn claim_pending_dispatch_outbox_batch_pg(
    pool: &PgPool,
    claim_owner: &str,
) -> Vec<DispatchOutboxRow> {
    let owner_node =
        match crate::server::cluster::worker_node_snapshot_by_instance(pool, claim_owner, 60).await
        {
            Ok(node) => node,
            Err(error) => {
                tracing::warn!(
                    claim_owner,
                    error,
                    "[dispatch-outbox] failed to load claim owner capabilities"
                );
                None
            }
        };
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::warn!("[dispatch-outbox] failed to begin postgres claim transaction: {error}");
            return Vec::new();
        }
    };

    let rows = match sqlx::query(
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
    .fetch_all(&mut *tx)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!("[dispatch-outbox] failed to select postgres outbox rows: {error}");
            let _ = tx.rollback().await;
            return Vec::new();
        }
    };

    let mut pending = Vec::new();
    for row in rows {
        let id = match row.try_get::<i64, _>("id") {
            Ok(id) => id,
            Err(_) => continue,
        };
        let dispatch_id = match row.try_get::<String, _>("dispatch_id") {
            Ok(dispatch_id) => dispatch_id,
            Err(_) => continue,
        };
        let required_capabilities = row
            .try_get::<Option<Value>, _>("required_capabilities")
            .ok()
            .flatten();

        if !required_capabilities_empty(required_capabilities.as_ref()) {
            let required = required_capabilities
                .as_ref()
                .expect("required capabilities checked above");
            let decision = owner_node
                .as_ref()
                .map(|node| crate::server::cluster::explain_capability_match(node, required))
                .unwrap_or_else(|| crate::server::cluster::CapabilityRouteDecision {
                    instance_id: Some(claim_owner.to_string()),
                    eligible: false,
                    reasons: vec!["claim owner is not registered in worker_nodes".to_string()],
                });
            if !decision.eligible {
                record_routing_diagnostics_pg(
                    &mut tx,
                    id,
                    &dispatch_id,
                    claim_owner,
                    &decision,
                    required,
                )
                .await;
                continue;
            }
        }

        if let Err(error) = sqlx::query(
            "UPDATE dispatch_outbox
                SET status = 'processing',
                    claimed_at = NOW(),
                    claim_owner = $2
              WHERE id = $1",
        )
        .bind(id)
        .bind(claim_owner)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(
                outbox_id = id,
                dispatch_id,
                error = %error,
                "[dispatch-outbox] failed to claim postgres outbox row"
            );
            continue;
        }

        pending.push((
            id,
            dispatch_id,
            row.try_get::<String, _>("action").ok().unwrap_or_default(),
            row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
            row.try_get::<Option<String>, _>("card_id").ok().flatten(),
            row.try_get::<Option<String>, _>("title").ok().flatten(),
            row.try_get::<i64, _>("retry_count")
                .ok()
                .unwrap_or_default(),
            required_capabilities,
        ));
        if pending.len() >= 5 {
            break;
        }
    }

    if let Err(error) = tx.commit().await {
        tracing::warn!("[dispatch-outbox] failed to commit postgres outbox claims: {error}");
        return Vec::new();
    }

    pending.sort_by_key(|row| row.0);
    pending
}
