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

use serde_json::Value;
use sqlx::PgPool;

pub(crate) fn sandbox_preflight_suppresses_outbox(value: &Value) -> bool {
    value
        .get("sandbox_preflight")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && value
            .get("production_mutation_allowed")
            .and_then(Value::as_bool)
            == Some(false)
}

pub(crate) fn serialized_json_suppresses_outbox(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .as_ref()
        .is_some_and(sandbox_preflight_suppresses_outbox)
}

pub(crate) async fn dispatch_suppresses_outbox_pg(
    pg_pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool, String> {
    let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT context, result
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pg_pool)
    .await
    .map_err(|error| {
        format!("load dispatch outbox suppression marker for {dispatch_id}: {error}")
    })?;
    Ok(row.is_some_and(|(context, result)| {
        context
            .as_deref()
            .is_some_and(serialized_json_suppresses_outbox)
            || result
                .as_deref()
                .is_some_and(serialized_json_suppresses_outbox)
    }))
}

/// Queue a dispatch completion follow-up row on Postgres.
///
/// `ON CONFLICT DO NOTHING` preserves the single-finalize invariant for
/// manual/outbox/recovery callers.
pub async fn queue_dispatch_followup_pg(pg_pool: &PgPool, dispatch_id: &str) -> Result<(), String> {
    if dispatch_suppresses_outbox_pg(pg_pool, dispatch_id).await? {
        tracing::debug!(
            dispatch_id = %dispatch_id,
            "sandbox preflight dispatch suppressed followup outbox enqueue"
        );
        return Ok(());
    }

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
