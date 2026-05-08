use sqlx::{Postgres, Transaction};

pub(crate) fn wait_reason_from_routing_diagnostics(
    diagnostics: &serde_json::Value,
) -> Option<String> {
    if !diagnostics
        .get("selected")
        .map(serde_json::Value::is_null)
        .unwrap_or(false)
    {
        return None;
    }

    diagnostics
        .get("constraint_results")
        .and_then(serde_json::Value::as_array)
        .and_then(|results| {
            results.iter().find_map(|result| {
                let outcome = result.get("final_outcome")?;
                let outcome_kind = outcome.get("outcome").and_then(serde_json::Value::as_str)?;
                if !matches!(outcome_kind, "wait" | "reject") {
                    return None;
                }
                outcome
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some(format!("routing constraint outcome: {outcome_kind}")))
            })
        })
        .or_else(|| {
            diagnostics
                .get("decision")
                .and_then(|decision| decision.get("reasons"))
                .and_then(serde_json::Value::as_array)
                .and_then(|reasons| reasons.iter().find_map(serde_json::Value::as_str))
                .map(str::to_string)
        })
        .or_else(|| Some("no route candidate is currently available".to_string()))
}

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
    let wait_reason = wait_reason_from_routing_diagnostics(diagnostics);

    if let Err(error) = sqlx::query(
        "UPDATE dispatch_outbox
            SET routing_diagnostics = $2,
                claim_owner = $3,
                constraint_results = $4,
                wait_reason = $5,
                wait_started_at = CASE
                    WHEN $5::TEXT IS NULL THEN NULL
                    ELSE COALESCE(wait_started_at, NOW())
                END,
                next_attempt_at = CASE
                    WHEN $5::TEXT IS NULL THEN NOW() + INTERVAL '5 seconds'
                    ELSE NULL
                END
          WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(diagnostics)
    .bind(preferred_owner)
    .bind(constraint_results)
    .bind(wait_reason.as_deref())
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

pub(crate) async fn record_task_dispatch_routing_diagnostics_pg(
    tx: &mut Transaction<'_, Postgres>,
    dispatch_id: &str,
    diagnostics: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    let constraint_results = diagnostics.get("constraint_results");
    sqlx::query(
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
    .await?;
    Ok(())
}
