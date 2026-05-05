use serde_json::Value;
use sqlx::{Postgres, Transaction};

pub(crate) fn required_capabilities_empty(required: Option<&Value>) -> bool {
    match required {
        None | Some(Value::Null) => true,
        Some(Value::Object(map)) => map.is_empty(),
        _ => false,
    }
}

pub(super) async fn record_routing_diagnostics_pg(
    tx: &mut Transaction<'_, Postgres>,
    outbox_id: i64,
    dispatch_id: &str,
    claim_owner: &str,
    decision: &crate::server::cluster::CapabilityRouteDecision,
    required_capabilities: &Value,
) {
    let diagnostics = serde_json::json!({
        "claim_owner": claim_owner,
        "decision": decision,
        "required_capabilities": required_capabilities,
        "checked_at": chrono::Utc::now(),
    });
    if let Err(error) = sqlx::query(
        "UPDATE dispatch_outbox
            SET routing_diagnostics = $2,
                next_attempt_at = NOW() + INTERVAL '5 seconds'
          WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(&diagnostics)
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
    .bind(&diagnostics)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn required_capabilities_empty_handles_null_and_empty_object() {
        assert!(required_capabilities_empty(None));
        assert!(required_capabilities_empty(Some(&Value::Null)));
        assert!(required_capabilities_empty(Some(&json!({}))));
        assert!(!required_capabilities_empty(Some(
            &json!({"provider": "codex"})
        )));
        assert!(!required_capabilities_empty(Some(&json!(["codex"]))));
    }
}
