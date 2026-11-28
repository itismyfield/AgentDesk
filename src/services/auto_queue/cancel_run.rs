use super::{AutoQueueLogContext, AutoQueueService};
use crate::error::{AppError as ServiceError, AppResult as ServiceResult, ErrorCode};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

use crate::db::auto_queue::run_status::is_live_run_status;
use crate::services::engine_runtime::health_port::{SlotRowGuard, SlotRuntimeHealth};

mod remaining_live_census;
mod terminalize_tx;

use terminalize_tx::{TerminalizeTxOutcome, terminalize_selected_runs_on_pg_tx};

/// Runs the real terminalize body on the caller's transaction and leaves it uncommitted,
/// so run-owner contention tests can hold a cancellation at its commit point.
#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn terminalize_selected_runs_on_pg_tx_for_test(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    target_run_ids: &[String],
    reason: &str,
) -> Result<(), String> {
    terminalize_selected_runs_on_pg_tx(tx, target_run_ids, reason, true)
        .await
        .map(|_| ())
}

impl AutoQueueService {
    pub(crate) async fn cancel_run_with_pg(
        &self,
        row_guard: SlotRowGuard,
        health_registry: Option<SlotRuntimeHealth>,
        pool: &PgPool,
        run_id: &str,
    ) -> ServiceResult<Value> {
        let run_status = sqlx::query_scalar::<_, Option<String>>(
            "SELECT status
             FROM auto_queue_runs
             WHERE id = $1",
        )
        .bind(run_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            ServiceError::internal(format!("load auto-queue run '{run_id}': {error}"))
                .with_code(ErrorCode::Database)
                .with_context("run_id", run_id)
                .with_operation("auto_queue.cancel_run_with_pg.load_run")
        })?;

        match run_status.flatten() {
            Some(status) if is_live_run_status(&status) => cancel_selected_runs_with_pg(
                row_guard,
                health_registry,
                pool,
                &[run_id.to_string()],
                "auto_queue_cancel",
            )
            .await
            .map_err(|error| {
                ServiceError::internal(error)
                    .with_code(ErrorCode::Database)
                    .with_context("run_id", run_id)
                    .with_operation("auto_queue.cancel_run_with_pg.cancel_selected_runs_with_pg")
            }),
            Some(status) => Err(ServiceError::bad_request(format!(
                "auto-queue run '{run_id}' is not cancelable (status={status})"
            ))
            .with_code(ErrorCode::AutoQueue)
            .with_context("run_id", run_id)
            .with_context("status", status)),
            None => Err(
                ServiceError::not_found(format!("auto-queue run '{run_id}' not found"))
                    .with_code(ErrorCode::AutoQueue)
                    .with_context("run_id", run_id),
            ),
        }
    }

    pub(crate) async fn cancel_runs_with_pg(
        &self,
        row_guard: SlotRowGuard,
        health_registry: Option<SlotRuntimeHealth>,
        pool: &PgPool,
    ) -> ServiceResult<Value> {
        cancel_with_pg(row_guard, health_registry, pool)
            .await
            .map_err(|error| {
                ServiceError::internal(error)
                    .with_code(ErrorCode::Database)
                    .with_operation("auto_queue.cancel_runs_with_pg.cancel_with_pg")
            })
    }
}

#[derive(Debug, Default)]
pub(crate) struct SlotCleanupResult {
    pub(crate) released_slots: usize,
    pub(crate) cleared_slot_sessions: usize,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct LiveRunCleanupResult {
    pub(crate) cancelled_dispatches: usize,
    pub(crate) slot_cleanup: SlotCleanupResult,
}

pub(crate) fn slot_cleanup_warning(warnings: &[String]) -> Option<String> {
    (!warnings.is_empty()).then(|| warnings.join("; "))
}

pub(crate) async fn load_run_ids_with_status_pg(
    pool: &PgPool,
    statuses: &[&str],
) -> Result<Vec<String>, String> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }

    let mut query =
        QueryBuilder::<Postgres>::new("SELECT id FROM auto_queue_runs WHERE status IN (");
    let mut separated = query.separated(", ");
    for status in statuses {
        separated.push_bind(*status);
    }
    separated.push_unseparated(") ORDER BY created_at ASC, id ASC");
    query
        .build_query_scalar::<String>()
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load postgres auto_queue_runs by status: {error}"))
}

pub(crate) async fn delete_phase_gate_rows_for_runs_pg(
    pool: &PgPool,
    run_ids: &[String],
) -> Result<usize, String> {
    if run_ids.is_empty() {
        return Ok(0);
    }

    let mut query =
        QueryBuilder::<Postgres>::new("DELETE FROM auto_queue_phase_gates WHERE run_id IN (");
    let mut separated = query.separated(", ");
    for run_id in run_ids {
        separated.push_bind(run_id);
    }
    separated.push_unseparated(")");

    query
        .build()
        .execute(pool)
        .await
        .map(|result| result.rows_affected() as usize)
        .map_err(|error| format!("delete postgres auto_queue_phase_gates: {error}"))
}

pub(crate) async fn cancel_live_dispatches_for_runs_pg(
    pool: &PgPool,
    run_ids: &[String],
    reason: &str,
) -> Result<CancelledDispatchesWithCleanup, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres run-owned dispatch cancel: {error}"))?;
    // Same run tokens as terminalize, so an owner publication cannot interleave.
    crate::db::auto_queue::acquire_run_advisory_xact_locks_on_pg_tx(&mut tx, run_ids).await?;
    let transitions = crate::dispatch::cancel_dispatches_for_runs_on_pg_tx_with_meta(
        &mut tx,
        run_ids,
        Some(reason),
        false,
    )
    .await?;

    let dispatch_ids = transitions
        .iter()
        .map(|transition| transition.dispatch_id.clone())
        .collect::<Vec<_>>();
    // #5142: record the post-commit debt inside the same transaction. Before
    // this, the emit / wake / session clear / slot release that follow the
    // commit existed only on this stack, so a crash here left the cancel
    // durable and the cleanup lost.
    // Note: cancel_live_dispatches_for_runs_pg does not involve card rollback,
    // so we pass empty card_rollback_tasks.
    let cleanup_task_id = super::cleanup_tasks::enqueue_run_cleanup_task_on_tx(
        &mut tx,
        run_ids,
        &dispatch_ids,
        &[],
        &transitions,
        &[],
        None,
    )
    .await?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres run-owned dispatch cancel: {error}"))?;

    Ok(CancelledDispatchesWithCleanup {
        dispatch_ids,
        cleanup_task_id,
    })
}

/// Dispatches cancelled by a committed transaction plus the durable cleanup row
/// that still owes the post-commit steps for them (#5142).
#[derive(Debug)]
pub(crate) struct CancelledDispatchesWithCleanup {
    pub(crate) dispatch_ids: Vec<String>,
    pub(crate) cleanup_task_id: i64,
}

pub(crate) async fn clear_sessions_for_dispatches_pg(
    pool: &PgPool,
    dispatch_ids: &[String],
) -> Result<usize, String> {
    let mut cleared_sessions = 0usize;
    for dispatch_id in dispatch_ids {
        let result = sqlx::query(
            "UPDATE sessions
             SET status = 'idle',
                 active_dispatch_id = NULL,
                 session_info = $1,
                 claude_session_id = NULL,
                 tokens = 0,
                 last_heartbeat = NOW()
             WHERE active_dispatch_id = $2
               AND status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working', 'idle')",
        )
        .bind("Dispatch cancelled")
        .bind(dispatch_id)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("clear postgres sessions for cancelled dispatch {dispatch_id}: {error}")
        })?;
        cleared_sessions += result.rows_affected() as usize;
    }
    Ok(cleared_sessions)
}

async fn self_heal_orphan_dispatched_entries_without_slot_pg(
    pool: &PgPool,
    run_ids: &[String],
    trigger_source: &str,
) -> Result<usize, String> {
    if run_ids.is_empty() {
        return Ok(0);
    }

    let entry_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE run_id = ANY($1)
           AND status = 'dispatched'
           AND dispatch_id IS NULL
           AND slot_index IS NULL
         ORDER BY id ASC",
    )
    .bind(run_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        format!(
            "load postgres orphan dispatched entry ids {:?}: {error}",
            run_ids
        )
    })?;

    let mut healed = 0usize;
    for entry_id in entry_ids {
        let mut tx = pool.begin().await.map_err(|error| {
            format!("begin postgres orphan dispatched repair transaction {entry_id}: {error}")
        })?;
        let changed = sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'pending',
                 dispatched_at = NULL
             WHERE id = $1
               AND status = 'dispatched'
               AND dispatch_id IS NULL
               AND slot_index IS NULL",
        )
        .bind(&entry_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("repair postgres orphan dispatched entry {entry_id}: {error}"))?
        .rows_affected() as usize;
        if changed == 0 {
            tx.rollback().await.map_err(|error| {
                format!("rollback unchanged postgres orphan dispatched entry {entry_id}: {error}")
            })?;
            continue;
        }
        // A failed statement aborts the PG transaction and COMMIT then rolls back
        // silently, so the audit row's error must fail the repair.
        sqlx::query(
            "INSERT INTO auto_queue_entry_transitions (
                entry_id,
                from_status,
                to_status,
                trigger_source
            ) VALUES ($1, 'dispatched', 'pending', $2)",
        )
        .bind(&entry_id)
        .bind(trigger_source)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            format!("record postgres orphan dispatched repair transition {entry_id}: {error}")
        })?;
        tx.commit().await.map_err(|error| {
            format!("commit postgres orphan dispatched repair {entry_id}: {error}")
        })?;
        healed += 1;
    }

    Ok(healed)
}

pub(crate) async fn cancel_and_release_runs_with_pg(
    row_guard: SlotRowGuard,
    health_registry: Option<SlotRuntimeHealth>,
    pool: &PgPool,
    run_ids: &[String],
    reason: &str,
    orphan_trigger_source: Option<&str>,
) -> Result<LiveRunCleanupResult, String> {
    let cancelled = cancel_live_dispatches_for_runs_pg(pool, run_ids, reason).await?;
    let cancelled_dispatches = cancelled.dispatch_ids.len();
    let _self_healed_orphan_entries = match orphan_trigger_source {
        Some(trigger_source) => {
            self_heal_orphan_dispatched_entries_without_slot_pg(pool, run_ids, trigger_source)
                .await?
        }
        None => 0,
    };
    // #5142: the emit / wake / session clear / slot release below used to run as
    // three loose transactions whose progress was invisible after a crash. They
    // now run out of the durable cleanup row committed with the cancel, and a
    // step that fails leaves that row behind for the replay sweep instead of
    // degrading into a warning string.
    let slot_cleanup = super::cleanup_tasks::drain_run_cleanup_task_by_id_pg(
        row_guard,
        health_registry,
        pool,
        cancelled.cleanup_task_id,
    )
    .await
    .slot_cleanup;

    Ok(LiveRunCleanupResult {
        cancelled_dispatches,
        slot_cleanup,
    })
}

async fn transition_entry_to_skipped_pg(
    pool: &PgPool,
    entry_id: &str,
    trigger_source: &str,
) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres entry skip transaction {entry_id}: {error}"))?;

    let current_row = sqlx::query(
        "SELECT status, dispatch_id
         FROM auto_queue_entries
         WHERE id = $1",
    )
    .bind(entry_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| format!("load postgres entry status {entry_id}: {error}"))?;
    let Some(current_row) = current_row else {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback missing postgres entry {entry_id}: {error}"))?;
        return Ok(false);
    };
    let current_status: Option<String> = current_row
        .try_get("status")
        .map_err(|error| format!("decode postgres entry status {entry_id}: {error}"))?;
    let previous_dispatch_id: Option<String> = current_row
        .try_get("dispatch_id")
        .map_err(|error| format!("decode postgres entry dispatch_id {entry_id}: {error}"))?;
    let Some(current_status) = current_status else {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback missing postgres entry {entry_id}: {error}"))?;
        return Ok(false);
    };
    if !matches!(
        current_status.as_str(),
        "pending" | "dispatched" | "user_cancelled"
    ) {
        tx.rollback().await.map_err(|error| {
            format!("rollback non-skippable postgres entry {entry_id}: {error}")
        })?;
        return Ok(false);
    }

    // #2048 F15: preserve dispatch history before nulling the pointer.
    // `auto_queue_entry_dispatch_history` is the canonical join table for
    // entry↔dispatch correlation in audits/dashboards. Clearing dispatch_id
    // without inserting here makes the cancelled entry's dispatch lineage
    // unrecoverable post-cancel.
    if let Some(previous_dispatch_id) = previous_dispatch_id.as_deref() {
        sqlx::query(
            "INSERT INTO auto_queue_entry_dispatch_history (
                 entry_id, dispatch_id, trigger_source
             )
             SELECT $1, $2, $3
             WHERE EXISTS (SELECT 1 FROM task_dispatches WHERE id = $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(entry_id)
        .bind(previous_dispatch_id)
        .bind(trigger_source)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            format!("record postgres dispatch history for skipped entry {entry_id}: {error}")
        })?;
    }

    let changed = sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'skipped',
             dispatch_id = NULL,
             dispatched_at = NULL,
             completed_at = NOW()
         WHERE id = $1
           AND status = $2",
    )
    .bind(entry_id)
    .bind(&current_status)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("skip postgres entry {entry_id}: {error}"))?
    .rows_affected() as usize;
    if changed == 0 {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback unchanged postgres entry {entry_id}: {error}"))?;
        return Ok(false);
    }

    sqlx::query(
        "INSERT INTO auto_queue_entry_transitions (
            entry_id,
            from_status,
            to_status,
            trigger_source
        ) VALUES ($1, $2, 'skipped', $3)",
    )
    .bind(entry_id)
    .bind(&current_status)
    .bind(trigger_source)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("record postgres entry skip transition {entry_id}: {error}"))?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres entry skip {entry_id}: {error}"))?;
    Ok(true)
}

async fn terminalize_selected_runs_with_pg(
    row_guard: SlotRowGuard,
    health_registry: Option<SlotRuntimeHealth>,
    pool: &PgPool,
    target_run_ids: &[String],
    reason: &str,
    should_rollback_cards: bool,
) -> Result<(Value, Vec<String>), String> {
    if target_run_ids.is_empty() {
        return Ok((
            json!({
                "ok": true,
                "cancelled_entries": 0usize,
                "cancelled_runs": 0usize,
                "cancelled_dispatches": 0usize,
                "deleted_phase_gates": 0usize,
                "remaining_live_dispatches": 0usize,
                "released_slots": 0usize,
                "cleared_slot_sessions": 0usize,
            }),
            Vec::new(),
        ));
    }

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres run cancel transaction: {error}"))?;

    let TerminalizeTxOutcome {
        terminalized_entries,
        terminalized_runs,
        deleted_phase_gates,
        cancel_metas,
        released_slots,
        cleanup_task_id,
        remaining_live_dispatches,
        rollback_candidate_card_ids,
    } = terminalize_selected_runs_on_pg_tx(&mut tx, target_run_ids, reason, should_rollback_cards)
        .await?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres run cancel transaction: {error}"))?;

    let drained = super::cleanup_tasks::drain_run_cleanup_task_by_id_pg(
        row_guard,
        health_registry,
        pool,
        cleanup_task_id,
    )
    .await;
    let cleanup = LiveRunCleanupResult {
        cancelled_dispatches: cancel_metas.len(),
        slot_cleanup: SlotCleanupResult {
            released_slots,
            cleared_slot_sessions: drained.slot_cleanup.cleared_slot_sessions,
            warnings: drained.slot_cleanup.warnings,
        },
    };
    if remaining_live_dispatches > 0 {
        let log_ctx = target_run_ids
            .first()
            .map(|run_id| AutoQueueLogContext::new().run(run_id))
            .unwrap_or_default();
        crate::auto_queue_log!(
            warn,
            "run_cancel_remaining_live_dispatches_pg",
            log_ctx,
            "[auto-queue] postgres cancel left {} non-terminal dispatches for runs {:?}",
            remaining_live_dispatches,
            target_run_ids
        );
    }

    let mut response = json!({
        "ok": true,
        "cancelled_entries": terminalized_entries,
        "cancelled_runs": terminalized_runs,
        "cancelled_dispatches": cleanup.cancelled_dispatches,
        "deleted_phase_gates": deleted_phase_gates,
        "remaining_live_dispatches": remaining_live_dispatches,
        "released_slots": cleanup.slot_cleanup.released_slots,
        "cleared_slot_sessions": cleanup.slot_cleanup.cleared_slot_sessions,
    });
    if let Some(warning) = slot_cleanup_warning(&cleanup.slot_cleanup.warnings) {
        response["warning"] = json!(warning);
    }
    Ok((response, rollback_candidate_card_ids))
}

pub(crate) async fn cancel_selected_runs_with_pg(
    row_guard: SlotRowGuard,
    health_registry: Option<SlotRuntimeHealth>,
    pool: &PgPool,
    target_run_ids: &[String],
    reason: &str,
) -> Result<Value, String> {
    let (response, _rollback_candidate_card_ids) = terminalize_selected_runs_with_pg(
        row_guard,
        health_registry,
        pool,
        target_run_ids,
        reason,
        true,
    )
    .await?;
    // #5357: Some-generation card rollback candidates are queued in the same
    // transaction as the entry changes and are retried by the replay sweep if
    // the synchronous drain fails. NULL-generation candidates are instead
    // rolled back inside that transaction, so failure aborts the entire cancel
    // and leaves it retryable without creating an unsafe replay obligation.
    Ok(response)
}

pub(crate) async fn end_run_with_pg(
    row_guard: SlotRowGuard,
    health_registry: Option<SlotRuntimeHealth>,
    pool: &PgPool,
    run_id: &str,
) -> Result<bool, String> {
    let (response, _) = terminalize_selected_runs_with_pg(
        row_guard,
        health_registry,
        pool,
        &[run_id.to_string()],
        "auto_queue_end",
        false, // P1-A: end operations do not roll back cards
    )
    .await?;
    Ok(response
        .get("cancelled_runs")
        .and_then(Value::as_u64)
        .is_some_and(|count| count > 0))
}

pub(crate) async fn skip_dispatched_entries_for_runs_pg(
    pool: &PgPool,
    run_ids: &[String],
    trigger_source: &str,
) -> Result<usize, String> {
    if run_ids.is_empty() {
        return Ok(0);
    }

    let entry_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE run_id = ANY($1)
           AND status = 'dispatched'
         ORDER BY id ASC",
    )
    .bind(run_ids)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres dispatched entries for pause: {error}"))?;

    let mut skipped = 0usize;
    for entry_id in entry_ids {
        match transition_entry_to_skipped_pg(pool, &entry_id, trigger_source).await {
            Ok(true) => skipped += 1,
            Ok(false) => {}
            Err(error) => crate::auto_queue_log!(
                warn,
                "pause_skip_entry_pg_failed",
                AutoQueueLogContext::new().entry(&entry_id),
                "[auto-queue] failed to skip postgres dispatched entry {}: {}",
                entry_id,
                error
            ),
        }
    }

    Ok(skipped)
}

pub(crate) async fn cancel_with_pg(
    row_guard: SlotRowGuard,
    health_registry: Option<SlotRuntimeHealth>,
    pool: &PgPool,
) -> Result<Value, String> {
    let target_run_ids =
        load_run_ids_with_status_pg(pool, crate::db::auto_queue::run_status::LIVE_RUN_STATUSES)
            .await?;
    cancel_selected_runs_with_pg(
        row_guard,
        health_registry,
        pool,
        &target_run_ids,
        "auto_queue_cancel",
    )
    .await
}
