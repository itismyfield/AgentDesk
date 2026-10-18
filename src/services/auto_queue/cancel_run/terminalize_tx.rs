//! Transaction body of run terminalization (cancel and end). The caller owns the
//! transaction so tests can stop a real cancellation before its commit.

use sqlx::{Postgres, Row, Transaction};

use crate::dispatch::CancelTransitionMeta;

/// State changes applied by [`terminalize_selected_runs_on_pg_tx`] before commit.
pub(super) struct TerminalizeTxOutcome {
    pub(super) terminalized_entries: usize,
    pub(super) terminalized_runs: usize,
    pub(super) deleted_phase_gates: usize,
    pub(super) cancel_metas: Vec<CancelTransitionMeta>,
    pub(super) released_slots: usize,
    pub(super) cleanup_task_id: i64,
    pub(super) remaining_live_dispatches: i64,
    pub(super) rollback_candidate_card_ids: Vec<String>,
}

pub(super) async fn terminalize_selected_runs_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    target_run_ids: &[String],
    reason: &str,
    should_rollback_cards: bool,
) -> Result<TerminalizeTxOutcome, String> {
    let completes_run = reason == "auto_queue_end";
    let run_status_filter = if completes_run {
        "status IN ('active', 'paused', 'generated', 'pending')"
    } else {
        "status IN ('active', 'paused', 'restoring')"
    };
    let terminal_status = if completes_run {
        "completed"
    } else {
        "cancelled"
    };
    let lock_status_filter = if completes_run {
        run_status_filter
    } else {
        "TRUE"
    };
    let lock_runs_sql = format!(
        "SELECT id
         FROM auto_queue_runs
         WHERE id = ANY($1)
           AND {lock_status_filter}"
    );
    let lock_candidates = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(lock_runs_sql))
        .bind(target_run_ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| format!("load postgres auto_queue_runs for cancel: {error}"))?;
    let locked_run_ids =
        crate::db::auto_queue::acquire_run_advisory_xact_locks_on_pg_tx(tx, &lock_candidates)
            .await?;

    // P1-B: Capture the cards owned by the entries before terminalizing them. Their
    // generation markers are loaded from kanban_cards after cancellation has applied
    // all of its in-transaction state changes.
    let rollback_candidate_card_ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT kanban_card_id
         FROM auto_queue_entries
         WHERE run_id = ANY($1)
           AND status IN ('dispatched', 'user_cancelled')
           AND kanban_card_id IS NOT NULL
           AND BTRIM(kanban_card_id) <> ''
         ORDER BY kanban_card_id",
    )
    .bind(&locked_run_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load postgres cancellation card ids: {error}"))?;

    let entry_rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, status, dispatch_id
         FROM auto_queue_entries
         WHERE run_id = ANY($1)
           AND status IN ('pending', 'dispatched', 'user_cancelled')
         ORDER BY id ASC
         FOR UPDATE",
    )
    .bind(&locked_run_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load postgres cancel entries: {error}"))?;

    let cancel_metas = crate::dispatch::cancel_dispatches_for_runs_on_pg_tx_with_meta(
        tx,
        &locked_run_ids,
        Some(reason),
        false,
    )
    .await?;

    // Delete gate rows in the same commit as the terminal state changes. This
    // prevents terminal-dispatch reconciliation from manufacturing a failure
    // alert while an operator-requested run cancellation is in flight.
    let deleted_phase_gates =
        sqlx::query("DELETE FROM auto_queue_phase_gates WHERE run_id = ANY($1)")
            .bind(&locked_run_ids)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("delete postgres auto_queue_phase_gates: {error}"))?
            .rows_affected() as usize;

    let update_runs_sql = format!(
        "UPDATE auto_queue_runs
         SET status = $2,
             completed_at = NOW()
         WHERE id = ANY($1)
           AND {run_status_filter}"
    );
    let terminalized_runs = sqlx::query(sqlx::AssertSqlSafe(update_runs_sql))
        .bind(&locked_run_ids)
        .bind(terminal_status)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("cancel postgres auto_queue_runs: {error}"))?
        .rows_affected() as usize;

    let mut terminalized_entries = 0usize;
    for (entry_id, _entry_status, _dispatch_id) in entry_rows {
        let current_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_entries WHERE id = $1")
                .bind(&entry_id)
                .fetch_one(&mut **tx)
                .await
                .map_err(|error| format!("reload postgres cancel entry {entry_id}: {error}"))?;
        if matches!(
            current_status.as_str(),
            "pending" | "dispatched" | "user_cancelled"
        ) {
            crate::db::auto_queue::update_entry_status_on_pg_tx(
                tx,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
                "run_cancel",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            )
            .await?;
        }
        terminalized_entries += 1;
    }

    let released_slot_rows = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id = ANY($1)
         RETURNING agent_id, slot_index",
    )
    .bind(&locked_run_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("release postgres slots for cancelled runs: {error}"))?;
    let released_slots = released_slot_rows.len();
    let released_slot_keys = released_slot_rows
        .into_iter()
        .map(|row| {
            let agent_id = row
                .try_get::<String, _>("agent_id")
                .map_err(|error| format!("decode released slot agent: {error}"))?;
            let slot_index = row
                .try_get::<i64, _>("slot_index")
                .map_err(|error| format!("decode released slot index: {error}"))?;
            Ok(crate::services::auto_queue::cleanup_tasks::ReleasedSlot {
                agent_id,
                slot_index,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    if completes_run && terminalized_runs > 0 {
        for run_id in &locked_run_ids {
            crate::db::auto_queue::queue_run_completion_notify_on_pg(tx, run_id).await?;
        }
    }

    let cancelled_dispatch_ids = cancel_metas
        .iter()
        .map(|meta| meta.dispatch_id.clone())
        .collect::<Vec<_>>();
    // #5142: this transaction already released the slot rows, so the cleanup row
    // carries the slot keys directly; the steps that still have to run outside
    // the commit (emit, wake, session clear, slot-thread clear) become durable
    // here rather than living only on this stack.
    // #5357: P1-A rolls cards back only when cancelling; end operations do not
    // touch card state. Card generations are partitioned at enrollment because
    // a Some token is self-invalidating: rollback clears `latest_dispatch_id`,
    // so a surviving replay no longer matches. NULL cannot self-invalidate and
    // is therefore never a replay task. It is rolled back synchronously in this
    // cancel transaction; if the process crashes, the cancel itself is also
    // uncommitted, so there is no committed rollback obligation to recover.
    let card_rollback_tasks = if should_rollback_cards && !rollback_candidate_card_ids.is_empty() {
        let card_generations = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT id, latest_dispatch_id
     FROM kanban_cards
     WHERE id = ANY($1)
     ORDER BY id
     FOR UPDATE",
        )
        .bind(&rollback_candidate_card_ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| format!("load postgres cancellation card generations: {error}"))?;

        let mut replay_safe_tasks = Vec::with_capacity(card_generations.len());
        // This enrollment query returns and locks candidates in id order, so the
        // Some/NULL partition below uses only generations read while those rows
        // are locked. The shared rollback body repeats FOR UPDATE for a NULL row;
        // this transaction already owns that row lock, so reacquisition is a
        // no-op. NULL generations are rare and arise on paths such as
        // `clearLatestDispatch`. If any synchronous rollback fails, the entire
        // cancel aborts under the advisory locks and can be retried; run, entry,
        // dispatch, and card changes all roll back.
        for (card_id, dispatch_id) in card_generations {
            match dispatch_id {
                Some(dispatch_id) => {
                    replay_safe_tasks.push((card_id, Some(dispatch_id)));
                }
                None => {
                    crate::services::auto_queue::cleanup_tasks::perform_card_rollback_on_tx(
                        tx, &card_id, None, reason,
                    )
                    .await?;
                }
            }
        }
        replay_safe_tasks
    } else {
        Vec::new()
    };
    let cleanup_task_id = crate::services::auto_queue::cleanup_tasks::enqueue_run_cleanup_task_on_tx(
        tx,
        &locked_run_ids,
        &cancelled_dispatch_ids,
        &released_slot_keys,
        &cancel_metas,
        &card_rollback_tasks,
        Some(reason),
    )
    .await?;
    let remaining_live_dispatches =
        super::remaining_live_census::count_live_dispatches_associated_with_runs_on_pg_tx(
            tx,
            target_run_ids,
        )
        .await?;

    Ok(TerminalizeTxOutcome {
        terminalized_entries,
        terminalized_runs,
        deleted_phase_gates,
        cancel_metas,
        released_slots,
        cleanup_task_id,
        remaining_live_dispatches,
        rollback_candidate_card_ids,
    })
}
