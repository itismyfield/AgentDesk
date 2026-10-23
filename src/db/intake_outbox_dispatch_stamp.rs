//! Read the lifecycle status of an intake handoff.

use crate::db::intake_outbox_status::IntakeOutboxStatus;
use sqlx::PgPool;

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
