//! Card delete stamps `deleted_at`; rows that point at the card keep a valid reference.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardDeleteOutcome {
    Deleted,
    NotFound,
    /// A pending or dispatched dispatch still works on the card.
    LiveDispatch(String),
}

/// What a card delete does once the card row is locked.
pub(crate) enum CardDeletePlan {
    Refused(CardDeleteOutcome),
    /// Pending auto-queue entries to skip before the card is stamped.
    SkipPendingEntries(Vec<String>),
}

/// Locks the live card row and reads what refuses or precedes its delete.
pub(crate) async fn plan_card_delete_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> Result<CardDeletePlan, String> {
    let found = sqlx::query_scalar::<_, String>(
        "SELECT id FROM kanban_cards WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(card_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("lock card {card_id}: {error}"))?;
    if found.is_none() {
        return Ok(CardDeletePlan::Refused(CardDeleteOutcome::NotFound));
    }

    let live = sqlx::query_scalar::<_, String>(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1 AND status IN ('pending', 'dispatched')
         ORDER BY created_at, id
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load live dispatch of card {card_id}: {error}"))?;
    if let Some(dispatch_id) = live {
        return Ok(CardDeletePlan::Refused(CardDeleteOutcome::LiveDispatch(
            dispatch_id,
        )));
    }

    let pending_entries = sqlx::query_scalar::<_, String>(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = $1 AND status = 'pending'
         ORDER BY id",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load pending auto-queue entries of card {card_id}: {error}"))?;
    Ok(CardDeletePlan::SkipPendingEntries(pending_entries))
}

/// Clearing the issue number frees the issue for a fresh card; the URL keeps it.
pub(crate) async fn stamp_card_deleted_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE kanban_cards
         SET deleted_at = NOW(), github_issue_number = NULL, updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("soft delete card {card_id}: {error}"))?;
    Ok(())
}
