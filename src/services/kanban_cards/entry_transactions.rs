//! Card writes that move auto-queue entries in the same tx as the run completion notice.
//! The kanban delete, rereview and reopen routes reach those writes only through these names.

use crate::db::auto_queue::{ENTRY_STATUS_SKIPPED, EntryStatusUpdateOptions};
use crate::db::kanban_cards::{
    CardDeleteOutcome, CardDeletePlan, plan_card_delete_on_pg_tx, stamp_card_deleted_on_pg_tx,
};
use crate::services::auto_queue::transactions::update_entry_status_on_pg_tx;
use sqlx::PgPool;

/// Rereview and reopen only move entries to `dispatched`, which never owes run completion.
pub(crate) use crate::db::kanban_cards::{
    move_auto_queue_entry_to_dispatched_on_pg, reactivate_done_auto_queue_entries_pg,
};

/// Refuses while a dispatch is live; otherwise skips the card's pending auto-queue
/// entries and stamps the card in one transaction.
pub(crate) async fn soft_delete_card_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<CardDeleteOutcome, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin card delete {card_id}: {error}"))?;
    let pending_entries = match plan_card_delete_on_pg_tx(&mut tx, card_id).await? {
        CardDeletePlan::Refused(outcome) => return Ok(outcome),
        CardDeletePlan::SkipPendingEntries(entry_ids) => entry_ids,
    };
    for entry_id in pending_entries {
        update_entry_status_on_pg_tx(
            &mut tx,
            &entry_id,
            ENTRY_STATUS_SKIPPED,
            "card_deleted",
            &EntryStatusUpdateOptions::default(),
        )
        .await?;
    }
    stamp_card_deleted_on_pg_tx(&mut tx, card_id).await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit card delete {card_id}: {error}"))?;
    Ok(CardDeleteOutcome::Deleted)
}
