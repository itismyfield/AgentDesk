//! Entry status write results and the dispatch-terminal entry SQL. A changed terminal entry
//! is marked here; the service attempts run completion on the same tx at that point.

use sqlx::Row as SqlxRow;
use sqlx::postgres::PgRow;

use super::{
    ENTRY_STATUS_DONE, ENTRY_STATUS_FAILED, ENTRY_STATUS_SKIPPED, EntryStatusUpdateResult,
    record_entry_dispatch_history_on_pg,
};

/// Whether a changed entry now in `status` owes a run-completion attempt on its tx. `done` goes
/// through the completed-dispatch finalizer; `user_cancelled` keeps the run resumable.
pub(super) fn run_finalize_due(status: &str) -> bool {
    matches!(status, ENTRY_STATUS_SKIPPED | ENTRY_STATUS_FAILED)
}

/// Result of the tx-scoped entry status writer.
pub(crate) struct EntryStatusWrite {
    pub(crate) result: EntryStatusUpdateResult,
    pub(crate) run_finalize_due: bool,
}

impl EntryStatusWrite {
    pub(super) fn unchanged(result: EntryStatusUpdateResult) -> Self {
        Self {
            result,
            run_finalize_due: false,
        }
    }

    /// For DB callers whose target status never owes run completion.
    pub(crate) fn without_finalize(self) -> EntryStatusUpdateResult {
        debug_assert!(!self.run_finalize_due);
        self.result
    }
}

/// Result of the pool-scoped entry status writer.
pub(crate) enum EntryStatusStage {
    Unchanged(EntryStatusUpdateResult),
    Staged(StagedEntryStatus),
}

impl EntryStatusStage {
    /// For DB callers whose target status never owes run completion.
    pub(crate) async fn commit_without_finalize(self) -> Result<EntryStatusUpdateResult, String> {
        match self {
            Self::Unchanged(result) => Ok(result),
            Self::Staged(staged) => {
                debug_assert!(!staged.run_finalize_due);
                staged.commit().await
            }
        }
    }
}

/// A changed entry row on its still-open pool transaction.
pub(crate) struct StagedEntryStatus {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
    entry_id: String,
    result: EntryStatusUpdateResult,
    run_finalize_due: bool,
}

impl StagedEntryStatus {
    pub(super) fn new(
        tx: sqlx::Transaction<'static, sqlx::Postgres>,
        entry_id: &str,
        result: EntryStatusUpdateResult,
        run_finalize_due: bool,
    ) -> Self {
        Self {
            tx,
            entry_id: entry_id.to_string(),
            result,
            run_finalize_due,
        }
    }

    pub(crate) fn run_finalize_due(&self) -> bool {
        self.run_finalize_due
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.result.run_id
    }

    pub(crate) fn transaction(&mut self) -> &mut sqlx::Transaction<'static, sqlx::Postgres> {
        &mut self.tx
    }

    pub(crate) async fn commit(self) -> Result<EntryStatusUpdateResult, String> {
        let Self {
            tx,
            entry_id,
            result,
            ..
        } = self;
        tx.commit()
            .await
            .map_err(|error| format!("commit postgres entry transition for {entry_id}: {error}"))?;
        Ok(result)
    }
}

/// Locks the entries a terminal dispatch moves. Rows come back raw; the caller decodes and
/// writes them one at a time.
pub(crate) async fn lock_dispatch_terminal_entry_rows_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    new_status: &str,
) -> Result<Vec<PgRow>, String> {
    // #1562 RC8: also match entries via kanban_card_id when the agent has
    // performed a self-recovery (replaced a cancelled dispatch with a fresh
    // one on the same card). The entry's `dispatch_id` pointer still
    // references the cancelled original, so direct dispatch_id match would
    // miss the completion. Card-id fallback only fires when the entry's
    // tracked dispatch is NOT itself the dispatch being completed (avoids
    // cross-row updates when both pointers happen to align) AND when the
    // entry's previously-tracked dispatch is in a terminal non-completed
    // state — i.e. genuine self-recovery, not normal lifecycle.
    //
    // #1970: retryable transport failures can briefly push the entry to
    // `failed` before a later retry dispatch succeeds for the same card. Treat
    // the completed retry as authoritative and reconcile that stale failed
    // entry to `done` by card id.
    sqlx::query(
        "WITH target_dispatch AS (
             SELECT kanban_card_id
             FROM task_dispatches
             WHERE id = $1
         )
         SELECT e.id, e.run_id, e.dispatch_id, e.slot_index, e.status
         FROM auto_queue_entries e
         JOIN target_dispatch d ON d.kanban_card_id = e.kanban_card_id
         WHERE (
                e.status = 'dispatched'
            AND (
                  e.dispatch_id = $1
               OR COALESCE(
                    (
                        SELECT status
                        FROM task_dispatches
                        WHERE id = e.dispatch_id
                        FOR SHARE
                    ),
                    ''
                  ) IN ('cancelled', 'failed', 'superseded')
            )
         )
         OR (
                $2 = 'done'
            AND e.status = 'failed'
            AND (
                  e.dispatch_id = $1
               OR COALESCE(
                    (
                        SELECT status
                        FROM task_dispatches
                        WHERE id = e.dispatch_id
                        FOR SHARE
                    ),
                    ''
                  ) IN ('cancelled', 'failed', 'superseded')
               OR EXISTS (
                    SELECT 1
                    FROM auto_queue_entry_dispatch_history h
                    WHERE h.entry_id = e.id
                      AND h.dispatch_id = $1
               )
            )
         )
         FOR UPDATE OF e",
    )
    .bind(dispatch_id)
    .bind(new_status)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        format!("load postgres auto-queue entries for dispatch {dispatch_id}: {error}")
    })
}

/// One locked entry of a terminal dispatch.
pub(crate) struct DispatchTerminalEntryRow {
    pub(crate) entry_id: String,
    pub(crate) run_id: String,
    linked_dispatch_id: Option<String>,
    slot_index: Option<i64>,
    entry_status: String,
}

impl DispatchTerminalEntryRow {
    pub(crate) fn decode(row: &PgRow, dispatch_id: &str) -> Result<Self, String> {
        let entry_id: String = row.try_get("id").map_err(|error| {
            format!("decode postgres auto-queue entry id for {dispatch_id}: {error}")
        })?;
        let run_id: String = row.try_get("run_id").map_err(|error| {
            format!("decode postgres auto-queue entry run_id for {dispatch_id}: {error}")
        })?;
        let linked_dispatch_id: Option<String> = row.try_get("dispatch_id").map_err(|error| {
            format!("decode postgres auto-queue entry dispatch_id for {dispatch_id}: {error}")
        })?;
        let slot_index: Option<i64> = row.try_get("slot_index").map_err(|error| {
            format!("decode postgres auto-queue entry slot_index for {dispatch_id}: {error}")
        })?;
        let entry_status: String = row.try_get("status").map_err(|error| {
            format!("decode postgres auto-queue entry status for {dispatch_id}: {error}")
        })?;
        Ok(Self {
            entry_id,
            run_id,
            linked_dispatch_id,
            slot_index,
            entry_status,
        })
    }

    /// A stale `failed` entry that a completed retry reconciles to `done`.
    fn reconciles_failed_to_done(&self, new_status: &str) -> bool {
        self.entry_status == ENTRY_STATUS_FAILED && new_status == ENTRY_STATUS_DONE
    }

    pub(crate) fn update_trigger_source<'a>(
        &self,
        new_status: &str,
        trigger_source: &'a str,
    ) -> &'a str {
        if self.reconciles_failed_to_done(new_status) {
            "dispatch_terminal_reconcile"
        } else {
            trigger_source
        }
    }

    /// History and dispatch-link writes after this entry's status changed.
    pub(crate) async fn record_lineage_on_pg_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        dispatch_id: &str,
        new_status: &str,
        trigger_source: &str,
        preserve_dispatch_link: bool,
    ) -> Result<(), String> {
        let entry_id = &self.entry_id;
        if self.reconciles_failed_to_done(new_status) {
            record_entry_dispatch_history_on_pg(tx, entry_id, dispatch_id, trigger_source).await?;
        }
        if preserve_dispatch_link {
            if let Some(linked_dispatch_id) = self.linked_dispatch_id.as_deref() {
                sqlx::query(
                    "UPDATE auto_queue_entries
                     SET dispatch_id = $1,
                         slot_index = $2
                     WHERE id = $3
                       AND dispatch_id IS NULL
                       AND status = $4",
                )
                .bind(linked_dispatch_id)
                .bind(self.slot_index)
                .bind(entry_id)
                .bind(new_status)
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    format!("restore postgres auto-queue entry lineage for {entry_id}: {error}")
                })?;
            }
        }
        Ok(())
    }
}
