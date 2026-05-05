use sqlx::{Postgres, Transaction};

pub(crate) async fn record_routing_diagnostics_pg(
    tx: &mut Transaction<'_, Postgres>,
    outbox_id: i64,
    dispatch_id: &str,
    diagnostics: &serde_json::Value,
) {
    if let Err(error) = sqlx::query(
        "UPDATE dispatch_outbox
            SET routing_diagnostics = $2,
                next_attempt_at = NOW() + INTERVAL '5 seconds'
          WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(diagnostics)
    .execute(&mut **tx)
    .await
    {
        tracing::warn!(
            outbox_id,
            dispatch_id,
            error = %error,
            "[dispatch-outbox] failed to record routing diagnostics"
        );
    }
    if let Err(error) = sqlx::query(
        "UPDATE task_dispatches
            SET routing_diagnostics = $2,
                updated_at = NOW()
          WHERE id = $1",
    )
    .bind(dispatch_id)
    .bind(diagnostics)
    .execute(&mut **tx)
    .await
    {
        tracing::warn!(
            dispatch_id,
            error = %error,
            "[dispatch-outbox] failed to record dispatch routing diagnostics"
        );
    }
}
