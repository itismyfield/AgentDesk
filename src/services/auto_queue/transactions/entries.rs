//! Entry status writes for engine, dispatch, kanban, GitHub, route and runtime terminal paths.
//! A changed `skipped`/`failed` entry attempts run completion on the same tx before commit.

use super::completion::maybe_finalize_run_if_ready_pg;
use crate::db::auto_queue::entries::status_write::{
    DispatchTerminalEntryRow, EntryStatusStage, lock_dispatch_terminal_entry_rows_on_pg_tx,
};
use crate::db::auto_queue::{
    DispatchTerminalEntrySyncResult, ENTRY_STATUS_DONE, EntryStatusUpdateOptions,
    EntryStatusUpdateResult, auto_queue_run_review_disabled_on_pg_tx, stage_entry_status_on_pg,
    write_entry_status_on_pg_tx,
};
use sqlx::PgPool;
use std::collections::BTreeSet;

pub(crate) use crate::db::auto_queue::{
    EntryDispatchFailureResult, reconcile_failed_entry_done_on_pg,
};

/// Pool-scoped writer: stale-retry loop and one tx per changed attempt, committed here.
pub(crate) async fn update_entry_status_on_pg(
    pool: &PgPool,
    entry_id: &str,
    new_status: &str,
    trigger_source: &str,
    options: &EntryStatusUpdateOptions,
) -> Result<EntryStatusUpdateResult, String> {
    match stage_entry_status_on_pg(pool, entry_id, new_status, trigger_source, options).await? {
        EntryStatusStage::Unchanged(result) => Ok(result),
        EntryStatusStage::Staged(mut staged) => {
            if staged.run_finalize_due() {
                let run_id = staged.run_id().to_string();
                maybe_finalize_run_if_ready_pg(staged.transaction(), &run_id).await?;
            }
            staged.commit().await
        }
    }
}

/// Caller-owned tx variant of [`update_entry_status_on_pg`].
pub(crate) async fn update_entry_status_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
    new_status: &str,
    trigger_source: &str,
    options: &EntryStatusUpdateOptions,
) -> Result<EntryStatusUpdateResult, String> {
    let written =
        write_entry_status_on_pg_tx(tx, entry_id, new_status, trigger_source, options).await?;
    if written.run_finalize_due {
        maybe_finalize_run_if_ready_pg(tx, &written.result.run_id).await?;
    }
    Ok(written.result)
}

/// Route dispatch-linked terminal transitions through the canonical entry
/// writer so PG transition bookkeeping and run finalization stay consistent.
pub(crate) async fn sync_dispatch_terminal_entries_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    new_status: &str,
    trigger_source: &str,
    preserve_dispatch_link: bool,
) -> Result<usize, String> {
    Ok(sync_dispatch_terminal_entries(
        tx,
        dispatch_id,
        new_status,
        trigger_source,
        preserve_dispatch_link,
    )
    .await?
    .changed_entries)
}

async fn sync_dispatch_terminal_entries(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    new_status: &str,
    trigger_source: &str,
    preserve_dispatch_link: bool,
) -> Result<DispatchTerminalEntrySyncResult, String> {
    let rows = lock_dispatch_terminal_entry_rows_on_pg_tx(tx, dispatch_id, new_status).await?;
    let mut changed = 0usize;
    let mut affected_run_ids = BTreeSet::new();
    for row in rows {
        let entry = DispatchTerminalEntryRow::decode(&row, dispatch_id)?;
        let result = update_entry_status_on_pg_tx(
            tx,
            &entry.entry_id,
            new_status,
            entry.update_trigger_source(new_status, trigger_source),
            &EntryStatusUpdateOptions::default(),
        )
        .await?;
        if result.changed {
            entry
                .record_lineage_on_pg_tx(
                    tx,
                    dispatch_id,
                    new_status,
                    trigger_source,
                    preserve_dispatch_link,
                )
                .await?;
            affected_run_ids.insert(entry.run_id);
            changed += 1;
        }
    }

    Ok(DispatchTerminalEntrySyncResult {
        changed_entries: changed,
        affected_run_ids: affected_run_ids.into_iter().collect(),
        finalized_run_ids: Vec::new(),
    })
}

/// Canonical completed-dispatch entry finalizer.
///
/// Normal dispatch completion reaches `task_dispatches.status = completed`
/// first, then derives the linked auto-queue entry terminal state here. Runs
/// with review disabled have no review/card-terminal hook left to close them,
/// so this helper is also responsible for invoking the only run completion
/// writer, `maybe_finalize_run_if_ready_pg`, after the entry reaches `done`.
pub(crate) async fn finalize_completed_dispatch_terminal_entry_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    trigger_source: &str,
    preserve_dispatch_link: bool,
) -> Result<DispatchTerminalEntrySyncResult, String> {
    let mut result = sync_dispatch_terminal_entries(
        tx,
        dispatch_id,
        ENTRY_STATUS_DONE,
        trigger_source,
        preserve_dispatch_link,
    )
    .await?;

    for run_id in &result.affected_run_ids {
        if auto_queue_run_review_disabled_on_pg_tx(tx, run_id).await?
            && maybe_finalize_run_if_ready_pg(tx, run_id).await?
        {
            result.finalized_run_ids.push(run_id.clone());
        }
    }

    Ok(result)
}
