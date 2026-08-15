//! Monotonic intake-outbox handoff stamp for the Discord turn bridge.

use crate::db::intake_outbox_status::IntakeOutboxStatus;
use sqlx::PgPool;

/// Transitions `spawned -> dispatched` immediately before bridge handoff.
///
/// The bridge site does not own the worker's claim token, so the monotonic
/// status CAS is the authority boundary. Dispatch audit fields are retained.
pub(crate) async fn mark_dispatched(pool: &PgPool, outbox_id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE public.intake_outbox
         SET status = $2, dispatched_at = NOW()
         WHERE id = $1 AND status = $3",
    )
    .bind(outbox_id)
    .bind(IntakeOutboxStatus::Dispatched)
    .bind(IntakeOutboxStatus::Spawned)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Reads the current lifecycle status without taking transition authority.
pub(crate) async fn observe_status(
    pool: &PgPool,
    outbox_id: i64,
) -> Result<Option<IntakeOutboxStatus>, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM public.intake_outbox WHERE id = $1")
        .bind(outbox_id)
        .fetch_optional(pool)
        .await
}

#[cfg(test)]
#[path = "intake_outbox_dispatch_stamp/tests.rs"]
mod tests;
