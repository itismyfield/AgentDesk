/// #117: Update the canonical card_review_state record after a review-decision action.
/// #158: Routes through the unified review_state_sync entrypoint.
pub(super) fn update_card_review_state(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
    decision: &str,
    _dispatch_id: Option<&str>,
) {
    let state = match decision {
        "accept" => "rework_pending",
        "dispute" => "reviewing",
        "dismiss" => "idle",
        _ => return,
    };
    let payload = serde_json::json!({
        "card_id": card_id,
        "state": state,
        "last_decision": decision,
    });
    crate::engine::ops::review_state_sync_with_backends(db, pg_pool, &payload.to_string());
}
