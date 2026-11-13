//! Retry dispatch creation for the session force-kill path. Failing the origin dispatch runs
//! its terminal entry and gate writes, including run completion, on the retry tx.

use crate::db::dispatched_sessions::{
    RetryDispatchMeta, attach_retry_dispatch_on_pg_tx, claim_retry_dispatch_on_pg_tx,
};
use serde_json::json;
use sqlx::PgPool;

pub(crate) async fn create_retry_dispatch_pg(
    pool: &PgPool,
    meta: &RetryDispatchMeta,
) -> Result<String, String> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();
    let dispatch_type = meta
        .dispatch_type
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("implementation");
    let title = meta
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("retry dispatch");

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres retry dispatch transaction: {error}"))?;
    let claim = claim_retry_dispatch_on_pg_tx(&mut tx, meta).await?;
    let reason = json!({"reason": "force_kill_session", "retry": true});
    let mut deferred_observability = Vec::new();
    let changed = crate::dispatch::set_dispatch_status_on_pg_tx_async(
        &mut tx,
        &meta.origin_dispatch_id,
        "failed",
        Some(&reason),
        "force_kill_session",
        Some(&["pending", "dispatched"]),
        true,
        true,
        Some(&mut deferred_observability),
    )
    .await
    .map_err(|error| {
        format!(
            "canonical fail postgres dispatch {} during retry force-kill: {error}",
            meta.origin_dispatch_id
        )
    })?;
    if changed == 0 {
        return Err(format!(
            "origin dispatch {} is no longer retryable",
            meta.origin_dispatch_id
        ));
    }

    attach_retry_dispatch_on_pg_tx(&mut tx, meta, claim, &dispatch_id, dispatch_type, title)
        .await?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres retry dispatch {dispatch_id}: {error}"))?;

    // Best-effort delivery: a process crash between the commit above and these
    // emits loses the buffered events. The database rows committed above stay
    // authoritative; only the observability copies are lost.
    for event in deferred_observability {
        event.emit();
    }

    Ok(dispatch_id)
}
