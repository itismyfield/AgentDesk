//! Optional body-census column write inside the enqueue transaction's savepoint.

use serde_json::Value;
use sqlx::{Acquire, Postgres, Transaction};

/// Isolate optional-column failures without aborting the original enqueue transaction.
/// A failed savepoint BEGIN makes sqlx roll back the outer transaction, so that error ends the enqueue.
pub(crate) async fn store(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    census: Option<&Value>,
) -> Result<(), sqlx::Error> {
    let Some(census) = census else {
        return Ok(());
    };
    let mut observation = tx.begin().await?;
    if sqlx::query("UPDATE message_outbox SET body_census=$2 WHERE id=$1")
        .bind(id)
        .bind(census)
        .execute(&mut *observation)
        .await
        .is_err()
    {
        // A failed rollback or release still leaves sqlx's queued ROLLBACK TO SAVEPOINT at the
        // outer depth, so the caller's next statement or COMMIT reaches the server and reports.
        if observation.rollback().await.is_err() {
            tracing::warn!(outbox_id = id, "outbox observation rollback failed");
        }
        tracing::warn!(
            outbox_id = id,
            "outbox observation not stored; delivery will remain unknown"
        );
    } else if observation.commit().await.is_err() {
        tracing::warn!(
            outbox_id = id,
            "outbox observation savepoint release failed"
        );
    }
    Ok(())
}
