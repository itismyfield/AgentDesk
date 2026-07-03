/// #117: Update the canonical card_review_state record after a review-decision action.
/// #158: Routes through the unified review_state_sync entrypoint.
pub(super) fn update_card_review_state(
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
    decision: &str,
    _dispatch_id: Option<&str>,
) -> Result<(), String> {
    let (state, last_decision) = match decision {
        "accept" => ("rework_pending", "accept"),
        "dispute" => ("reviewing", "dispute"),
        "dismiss" => ("idle", "dismiss"),
        "dispute_scope_mismatch_closed" => ("dispute_scope_mismatch_closed", "dispute"),
        _ => return Ok(()),
    };
    let payload = serde_json::json!({
        "card_id": card_id,
        "state": state,
        "last_decision": last_decision,
    });
    let raw = crate::engine::ops::review_state_sync_with_backends(pg_pool, &payload.to_string());
    let parsed = serde_json::from_str::<serde_json::Value>(&raw).map_err(|error| {
        format!("parse review_state_sync response for {card_id}: {error}: {raw}")
    })?;
    if parsed.get("ok").and_then(|value| value.as_bool()) == Some(true) {
        return Ok(());
    }
    let error = parsed
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("review_state_sync returned a non-ok response");
    Err(format!("update review state for {card_id}: {error}"))
}
