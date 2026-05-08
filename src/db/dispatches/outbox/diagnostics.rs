use sqlx::{Postgres, Transaction};

pub(crate) async fn record_routing_diagnostics_pg(
    tx: &mut Transaction<'_, Postgres>,
    outbox_id: i64,
    dispatch_id: &str,
    diagnostics: &serde_json::Value,
) {
    let preferred_owner = diagnostics
        .get("selected")
        .and_then(|selected| selected.get("decision"))
        .and_then(|decision| decision.get("instance_id"))
        .and_then(|value| value.as_str());
    let constraint_results = diagnostics.get("constraint_results");

    if let Err(error) = sqlx::query(
        "UPDATE dispatch_outbox
            SET routing_diagnostics = $2,
                claim_owner = $3,
                constraint_results = $4,
                next_attempt_at = NOW() + INTERVAL '5 seconds'
          WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(diagnostics)
    .bind(preferred_owner)
    .bind(constraint_results)
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
                constraint_results = $3,
                updated_at = NOW()
          WHERE id = $1",
    )
    .bind(dispatch_id)
    .bind(diagnostics)
    .bind(constraint_results)
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
