//! Card delete stamps `deleted_at`; rows that point at the card keep a valid reference.

use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardDeleteOutcome {
    Deleted,
    NotFound,
    /// A pending or dispatched dispatch still works on the card.
    LiveDispatch(String),
}

/// Refuses while a dispatch is live; otherwise skips the card's pending auto-queue
/// entries and stamps the card in one transaction.
pub async fn soft_delete_card_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<CardDeleteOutcome, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin card delete {card_id}: {error}"))?;
    let found = sqlx::query_scalar::<_, String>(
        "SELECT id FROM kanban_cards WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(card_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| format!("lock card {card_id}: {error}"))?;
    if found.is_none() {
        return Ok(CardDeleteOutcome::NotFound);
    }

    let live = sqlx::query_scalar::<_, String>(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1 AND status IN ('pending', 'dispatched')
         ORDER BY created_at, id
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| format!("load live dispatch of card {card_id}: {error}"))?;
    if let Some(dispatch_id) = live {
        return Ok(CardDeleteOutcome::LiveDispatch(dispatch_id));
    }

    let pending_entries = sqlx::query_scalar::<_, String>(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = $1 AND status = 'pending'
         ORDER BY id",
    )
    .bind(card_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load pending auto-queue entries of card {card_id}: {error}"))?;
    for entry_id in pending_entries {
        crate::db::auto_queue::update_entry_status_on_pg_tx(
            &mut tx,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "card_deleted",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .await?;
    }

    // Clearing the issue number frees the issue for a fresh card; the URL keeps it.
    sqlx::query(
        "UPDATE kanban_cards
         SET deleted_at = NOW(), github_issue_number = NULL, updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("soft delete card {card_id}: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit card delete {card_id}: {error}"))?;
    Ok(CardDeleteOutcome::Deleted)
}
