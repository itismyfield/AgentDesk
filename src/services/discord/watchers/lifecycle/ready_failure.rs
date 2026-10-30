use super::*;

pub(crate) async fn update_card_ready_failure_marker_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    reason: &str,
) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres ready marker for {card_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE kanban_cards
         SET blocked_reason = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(reason)
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("update postgres ready marker for {card_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        return Ok(false);
    }
    crate::db::kanban::set_card_labels_pg(
        &mut *tx,
        card_id,
        crate::db::kanban::CardLabelsUpdate::Append(READY_FOR_INPUT_STUCK_LABEL),
    )
    .await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres ready marker for {card_id}: {error}"))?;
    Ok(true)
}

pub(crate) fn load_dispatch_card_id(shared: &SharedData, dispatch_id: &str) -> Option<String> {
    if let Some(pool) = shared.pg_pool.as_ref() {
        let dispatch_id = dispatch_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT kanban_card_id FROM task_dispatches WHERE id = $1",
                )
                .bind(dispatch_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres dispatch card id: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = (shared, dispatch_id);
    None
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ReadyForInputFailureResult {
    pub dispatch_failed: bool,
    pub card_id: Option<String>,
    pub card_marked: bool,
}

pub(in crate::services::discord) async fn fail_dispatch_for_ready_for_input_stall(
    shared: &Arc<SharedData>,
    dispatch_id: &str,
    tmux_session_name: &str,
) -> Result<ReadyForInputFailureResult, String> {
    let payload = serde_json::json!({
        "reason": READY_FOR_INPUT_STUCK_REASON,
        "failure_kind": READY_FOR_INPUT_STUCK_LABEL,
        "tmux_session_name": tmux_session_name,
    });
    let changed = crate::dispatch::set_dispatch_status_with_backends(
        shared.pg_pool.as_ref(),
        dispatch_id,
        "failed",
        Some(&payload),
        "tmux_ready_for_input_stuck",
        Some(&["pending", "dispatched"]),
        false,
    )
    .map_err(|error| format!("mark dispatch {dispatch_id} failed for ready stall: {error}"))?;

    let card_id = load_dispatch_card_id(shared.as_ref(), dispatch_id);
    let mut card_marked = false;
    if let Some(card_id_ref) = card_id.as_deref() {
        card_marked = if let Some(pool) = shared.pg_pool.as_ref() {
            update_card_ready_failure_marker_pg(pool, card_id_ref, READY_FOR_INPUT_STUCK_REASON)
                .await?
        } else {
            false
        };
    }

    Ok(ReadyForInputFailureResult {
        dispatch_failed: changed > 0,
        card_id,
        card_marked,
    })
}

#[cfg(test)]
#[path = "ready_failure_pg_tests.rs"]
mod ready_failure_pg_tests;
