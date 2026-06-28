//! Dispatch follow-up outbox enqueue — service-layer helpers.
//!
//! Previously these helpers lived in `server/routes/dispatches/outbox.rs` and
//! were called back into from service code (`services::dispatches`,
//! `services::discord::turn_bridge`, `server::routes::review_verdict`). That
//! produced a service→route reverse edge in the dispatch call graph.
//!
//! The helpers themselves are pure DB writes (insert into `dispatch_outbox`)
//! with no HTTP/Axum surface, so their correct home is in the service layer.
//! The outbox worker loop still lives under `server::routes::dispatches::outbox`
//! because it owns the Discord side-effect transport — but the *enqueue* side
//! that callers need is now here.
//!
//! Manual dispatch completion (PATCH /api/dispatches/:id), outbox-driven
//! follow-up, review-verdict completion, and recovery/turn-bridge completion
//! all funnel through the same `queue_dispatch_followup_sync` / `_pg` pair,
//! giving the call graph a single finalize guard shape.

use sqlx::PgPool;

/// Queue a dispatch completion follow-up row on Postgres.
///
/// `ON CONFLICT DO NOTHING` preserves the single-finalize invariant for
/// manual/outbox/recovery callers.
pub async fn queue_dispatch_followup_pg(pg_pool: &PgPool, dispatch_id: &str) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action)
         VALUES ($1, 'followup')
         ON CONFLICT DO NOTHING",
    )
    .bind(dispatch_id)
    .execute(pg_pool)
    .await
    .map_err(|error| format!("enqueue postgres followup for {dispatch_id}: {error}"))?;
    Ok(())
}

/// Sync wrapper over `queue_dispatch_followup_pg`.
///
/// This is the single entry point used by callers that don't want to deal
/// with the async/sync boundary directly (service update_dispatch path,
/// verdict route, etc.). All delivery finalize paths go through this
/// function or through `queue_dispatch_followup_pg` directly.
pub fn queue_dispatch_followup_sync(pg_pool: Option<&PgPool>, dispatch_id: &str) {
    if let Some(pool) = pg_pool {
        let dispatch_id_owned = dispatch_id.to_string();
        if let Err(error) = crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                queue_dispatch_followup_pg(&bridge_pool, &dispatch_id_owned).await
            },
            |error| error,
        ) {
            tracing::warn!(
                dispatch_id = %dispatch_id,
                "failed to enqueue postgres followup: {error}"
            );
        }
        return;
    }

    tracing::warn!(
        dispatch_id = %dispatch_id,
        "no postgres pool available to enqueue dispatch followup"
    );
}
