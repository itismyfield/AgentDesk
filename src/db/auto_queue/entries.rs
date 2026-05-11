use sqlx::{PgPool, Row as SqlxRow};
use std::collections::BTreeSet;
use thiserror::Error;

use super::runs::{
    auto_queue_run_review_disabled_on_pg_tx, maybe_finalize_run_after_terminal_entry_pg,
    maybe_finalize_run_if_ready_pg,
};

pub const ENTRY_STATUS_PENDING: &str = "pending";
pub const ENTRY_STATUS_DISPATCHED: &str = "dispatched";
pub const ENTRY_STATUS_DONE: &str = "done";
pub const ENTRY_STATUS_SKIPPED: &str = "skipped";
pub const ENTRY_STATUS_FAILED: &str = "failed";
/// Non-dispatchable terminal state used when the operator explicitly stopped
/// the linked dispatch (#815). The auto-queue tick must NOT resurrect these
/// entries back to `pending`; only a deliberate operator action (re-activate,
/// pmd_reopen, etc.) should move them out of this state.
pub const ENTRY_STATUS_USER_CANCELLED: &str = "user_cancelled";

/// Returns true when an entry in `status` is eligible for the auto-queue
/// tick to pick up and dispatch. Exposed as a small shim so callers can
/// treat `user_cancelled` uniformly alongside other non-dispatchable states
/// (#815).
pub fn is_dispatchable_entry_status(status: &str) -> bool {
    matches!(status.trim(), ENTRY_STATUS_PENDING)
}

#[derive(Debug, Clone, Default)]
pub struct EntryStatusUpdateOptions {
    pub dispatch_id: Option<String>,
    pub slot_index: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct EntryStatusUpdateResult {
    pub run_id: String,
    pub from_status: String,
    pub to_status: String,
    pub changed: bool,
}

#[derive(Debug, Error)]
pub enum EntryStatusUpdateError {
    #[error("unsupported auto-queue entry status: {status}")]
    UnsupportedStatus { status: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryDispatchFailureResult {
    pub run_id: String,
    pub from_status: String,
    pub to_status: String,
    pub retry_count: i64,
    pub retry_limit: i64,
    pub changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchTerminalEntrySyncResult {
    pub changed_entries: usize,
    pub affected_run_ids: Vec<String>,
    pub finalized_run_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct EntryStatusRow {
    run_id: String,
    card_id: String,
    agent_id: String,
    status: String,
    dispatch_id: Option<String>,
    retry_count: i64,
    slot_index: Option<i64>,
    thread_group: i64,
    batch_phase: i64,
    completed_at: Option<String>,
}

pub async fn reactivate_done_entry_on_pg(
    pool: &PgPool,
    entry_id: &str,
    trigger_source: &str,
    options: &EntryStatusUpdateOptions,
) -> Result<EntryStatusUpdateResult, String> {
    let current = load_entry_status_row_pg(pool, entry_id).await?;
    if current.status != ENTRY_STATUS_DONE {
        return update_entry_status_on_pg(
            pool,
            entry_id,
            ENTRY_STATUS_DISPATCHED,
            trigger_source,
            options,
        )
        .await;
    }

    let effective_dispatch_id = options
        .dispatch_id
        .clone()
        .or_else(|| current.dispatch_id.clone());
    let effective_slot_index = options.slot_index.or(current.slot_index);

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("open postgres auto-queue done reactivation tx: {error}"))?;

    let rows_affected = sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'dispatched',
             dispatch_id = $1,
             slot_index = $2,
             dispatched_at = NOW(),
             completed_at = NULL
         WHERE id = $3
           AND status = 'done'",
    )
    .bind(&effective_dispatch_id)
    .bind(effective_slot_index)
    .bind(entry_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("reactivate postgres auto-queue entry {entry_id}: {error}"))?
    .rows_affected();

    if rows_affected == 0 {
        tx.rollback().await.map_err(|error| {
            format!("rollback postgres auto-queue reactivation {entry_id}: {error}")
        })?;
        let latest = load_entry_status_row_pg(pool, entry_id).await?;
        if entry_status_row_matches_target(
            &latest,
            ENTRY_STATUS_DISPATCHED,
            effective_dispatch_id.as_deref(),
            effective_slot_index,
        ) {
            return Ok(EntryStatusUpdateResult {
                run_id: latest.run_id,
                from_status: latest.status,
                to_status: ENTRY_STATUS_DISPATCHED.to_string(),
                changed: false,
            });
        }

        return Err(format!(
            "invalid auto-queue entry transition for {entry_id}: {} -> {}",
            latest.status, ENTRY_STATUS_DISPATCHED
        ));
    }

    sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE id = $1
           AND status = 'completed'",
    )
    .bind(&current.run_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        format!(
            "reactivate postgres auto-queue run {}: {error}",
            current.run_id
        )
    })?;

    if let Some(dispatch_id) = effective_dispatch_id.as_deref() {
        record_entry_dispatch_history_on_pg(&mut tx, entry_id, dispatch_id, trigger_source).await?;
    }

    record_entry_transition_on_pg(
        &mut tx,
        entry_id,
        ENTRY_STATUS_DONE,
        ENTRY_STATUS_DISPATCHED,
        trigger_source,
    )
    .await?;

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres auto-queue reactivation {entry_id}: {error}"))?;

    Ok(EntryStatusUpdateResult {
        run_id: current.run_id,
        from_status: ENTRY_STATUS_DONE.to_string(),
        to_status: ENTRY_STATUS_DISPATCHED.to_string(),
        changed: true,
    })
}

pub async fn record_entry_dispatch_failure_on_pg(
    pool: &PgPool,
    entry_id: &str,
    max_retries: i64,
    trigger_source: &str,
) -> Result<EntryDispatchFailureResult, String> {
    let retry_limit = max_retries.max(1);
    loop {
        let current = load_entry_status_row_pg(pool, entry_id).await?;
        if current.status != ENTRY_STATUS_DISPATCHED {
            return Ok(EntryDispatchFailureResult {
                run_id: current.run_id,
                from_status: current.status.clone(),
                to_status: current.status,
                retry_count: current.retry_count,
                retry_limit,
                changed: false,
            });
        }

        let retry_count = current.retry_count.saturating_add(1);
        let target_status = if retry_count >= retry_limit {
            ENTRY_STATUS_FAILED
        } else {
            ENTRY_STATUS_PENDING
        };

        let mut tx = pool.begin().await.map_err(|error| {
            format!("begin postgres auto-queue dispatch failure transaction: {error}")
        })?;

        let rows_affected = sqlx::query(
            "UPDATE auto_queue_entries
             SET status = CASE
                     WHEN retry_count + 1 >= $1 THEN 'failed'
                     ELSE 'pending'
                 END,
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = CASE
                     WHEN retry_count + 1 >= $1 THEN NOW()
                     ELSE NULL
                 END,
                 retry_count = retry_count + 1
             WHERE id = $2
               AND status = 'dispatched'
               AND retry_count = $3",
        )
        .bind(retry_limit)
        .bind(entry_id)
        .bind(current.retry_count)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            format!("update postgres auto-queue dispatch failure {entry_id}: {error}")
        })?
        .rows_affected();

        if rows_affected == 0 {
            tx.rollback().await.map_err(|error| {
                format!("rollback stale postgres auto-queue dispatch failure {entry_id}: {error}")
            })?;

            let latest = load_entry_status_row_pg(pool, entry_id).await?;
            if latest.status != ENTRY_STATUS_DISPATCHED {
                return Ok(EntryDispatchFailureResult {
                    run_id: latest.run_id,
                    from_status: latest.status.clone(),
                    to_status: latest.status,
                    retry_count: latest.retry_count,
                    retry_limit,
                    changed: false,
                });
            }
            continue;
        }

        record_entry_transition_on_pg(
            &mut tx,
            entry_id,
            ENTRY_STATUS_DISPATCHED,
            target_status,
            trigger_source,
        )
        .await?;

        if target_status == ENTRY_STATUS_FAILED {
            maybe_finalize_run_after_terminal_entry_pg(&mut tx, &current.run_id, target_status)
                .await?;
        }

        tx.commit().await.map_err(|error| {
            format!("commit postgres auto-queue dispatch failure {entry_id}: {error}")
        })?;

        return Ok(EntryDispatchFailureResult {
            run_id: current.run_id,
            from_status: ENTRY_STATUS_DISPATCHED.to_string(),
            to_status: target_status.to_string(),
            retry_count,
            retry_limit,
            changed: true,
        });
    }
}

fn dispatch_json_field(document: Option<&str>, field: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(document?).ok()?;
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn dispatch_completed_commit(result: Option<&str>, context: Option<&str>) -> Option<String> {
    dispatch_json_field(result, "completed_commit")
        .or_else(|| dispatch_json_field(context, "completed_commit"))
}

pub async fn reconcile_failed_entry_done_on_pg(
    pool: &PgPool,
    entry_id: &str,
    trigger_source: &str,
) -> Result<EntryStatusUpdateResult, String> {
    let row = sqlx::query(
        "SELECT e.status AS entry_status,
                c.status AS card_status,
                d.id AS dispatch_id,
                d.status AS dispatch_status,
                d.result AS dispatch_result,
                d.context AS dispatch_context
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards c ON c.id = e.kanban_card_id
         LEFT JOIN LATERAL (
             SELECT td.id, td.status, td.result, td.context, td.completed_at, td.created_at
             FROM task_dispatches td
             WHERE td.kanban_card_id = e.kanban_card_id
               AND (
                   td.id = e.dispatch_id
                   OR td.id = c.latest_dispatch_id
                   OR EXISTS (
                       SELECT 1
                       FROM auto_queue_entry_dispatch_history h
                       WHERE h.entry_id = e.id
                         AND h.dispatch_id = td.id
                   )
               )
             ORDER BY (td.status = 'completed') DESC,
                      td.completed_at DESC NULLS LAST,
                      td.created_at DESC
             LIMIT 1
         ) d ON TRUE
         WHERE e.id = $1",
    )
    .bind(entry_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load auto-queue entry {entry_id} reconciliation state: {error}"))?;

    let Some(row) = row else {
        return Err(format!("auto-queue entry not found: {entry_id}"));
    };

    let entry_status: String = row
        .try_get("entry_status")
        .map_err(|error| format!("decode auto-queue entry {entry_id} status: {error}"))?;
    if entry_status != ENTRY_STATUS_FAILED {
        return Err(format!(
            "cannot reconcile auto-queue entry {entry_id} as done from status {entry_status}"
        ));
    }

    let card_status: Option<String> = row
        .try_get("card_status")
        .map_err(|error| format!("decode auto-queue entry {entry_id} card status: {error}"))?;
    if card_status.as_deref() != Some(ENTRY_STATUS_DONE) {
        return Err(format!(
            "cannot reconcile auto-queue entry {entry_id} as done unless its card is done"
        ));
    }

    let dispatch_id: Option<String> = row
        .try_get("dispatch_id")
        .map_err(|error| format!("decode auto-queue entry {entry_id} dispatch id: {error}"))?;
    let dispatch_status: Option<String> = row
        .try_get("dispatch_status")
        .map_err(|error| format!("decode auto-queue entry {entry_id} dispatch status: {error}"))?;
    if dispatch_id.is_none() || dispatch_status.as_deref() != Some("completed") {
        return Err(format!(
            "cannot reconcile auto-queue entry {entry_id} as done without a completed dispatch"
        ));
    }

    let dispatch_result: Option<String> = row
        .try_get("dispatch_result")
        .map_err(|error| format!("decode auto-queue entry {entry_id} dispatch result: {error}"))?;
    let dispatch_context: Option<String> = row
        .try_get("dispatch_context")
        .map_err(|error| format!("decode auto-queue entry {entry_id} dispatch context: {error}"))?;
    if dispatch_completed_commit(dispatch_result.as_deref(), dispatch_context.as_deref()).is_none()
    {
        return Err(format!(
            "cannot reconcile auto-queue entry {entry_id} as done without completed_commit evidence"
        ));
    }

    update_entry_status_on_pg(
        pool,
        entry_id,
        ENTRY_STATUS_DONE,
        trigger_source,
        &EntryStatusUpdateOptions::default(),
    )
    .await
}

pub async fn update_entry_status_on_pg(
    pool: &PgPool,
    entry_id: &str,
    new_status: &str,
    trigger_source: &str,
    options: &EntryStatusUpdateOptions,
) -> Result<EntryStatusUpdateResult, String> {
    let normalized = normalize_entry_status(new_status).map_err(|error| error.to_string())?;
    let mut current = load_entry_status_row_pg(pool, entry_id).await?;

    // #2048 F11: bound the stale-retry loop. Without a cap, two concurrent
    // updaters on the same entry can keep losing the optimistic update and
    // re-reading without progress, livelocking the tokio task and starving
    // other queue work. 8 attempts is enough for legitimate ordering races
    // while still surfacing a hard failure on pathological contention.
    const MAX_STALE_RETRIES: usize = 8;
    let mut attempts: usize = 0;
    loop {
        attempts += 1;
        if attempts > MAX_STALE_RETRIES {
            return Err(format!(
                "auto-queue entry {entry_id} status update livelock: exceeded {MAX_STALE_RETRIES} stale retries (target={normalized})"
            ));
        }
        let log_ctx = crate::services::auto_queue::AutoQueueLogContext::new()
            .run(&current.run_id)
            .entry(entry_id)
            .card(&current.card_id)
            .maybe_dispatch(current.dispatch_id.as_deref())
            .agent(&current.agent_id)
            .thread_group(current.thread_group)
            .batch_phase(current.batch_phase)
            .maybe_slot_index(current.slot_index);

        if !is_allowed_entry_transition(&current.status, normalized, trigger_source) {
            crate::auto_queue_log!(
                warn,
                "entry_status_transition_blocked_pg",
                log_ctx.clone(),
                "[auto-queue] blocked invalid PG entry transition {} {} -> {} (source: {})",
                entry_id,
                current.status,
                normalized,
                trigger_source
            );
            return Err(format!(
                "invalid auto-queue entry transition for {entry_id}: {} -> {normalized}",
                current.status
            ));
        }

        let effective_dispatch_id = options
            .dispatch_id
            .clone()
            .or_else(|| current.dispatch_id.clone());
        let effective_slot_index = options.slot_index.or(current.slot_index);
        let metadata_change = match normalized {
            ENTRY_STATUS_PENDING => {
                current.dispatch_id.is_some()
                    || current.slot_index.is_some()
                    || current.completed_at.is_some()
            }
            ENTRY_STATUS_DISPATCHED => {
                effective_dispatch_id != current.dispatch_id
                    || effective_slot_index != current.slot_index
                    || current.completed_at.is_some()
            }
            ENTRY_STATUS_DONE
            | ENTRY_STATUS_SKIPPED
            | ENTRY_STATUS_FAILED
            | ENTRY_STATUS_USER_CANCELLED => false,
            _ => false,
        };
        let changed = current.status != normalized || metadata_change;

        if !changed {
            return Ok(EntryStatusUpdateResult {
                run_id: current.run_id,
                from_status: current.status,
                to_status: normalized.to_string(),
                changed: false,
            });
        }

        let mut tx = pool
            .begin()
            .await
            .map_err(|error| format!("open postgres entry transition transaction: {error}"))?;

        let rows_affected = match normalized {
            ENTRY_STATUS_PENDING => sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'pending',
                     dispatch_id = NULL,
                     slot_index = NULL,
                     dispatched_at = NULL,
                     completed_at = NULL,
                     retry_count = CASE
                         WHEN $3 = 'failed' THEN 0
                         ELSE retry_count
                     END
                 WHERE id = $1
                   AND status = $2",
            )
            .bind(entry_id)
            .bind(&current.status)
            .bind(&current.status)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update auto-queue entry {entry_id} -> pending: {error}"))?
            .rows_affected(),
            ENTRY_STATUS_DISPATCHED => sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'dispatched',
                     dispatch_id = $1,
                     slot_index = $2,
                     dispatched_at = NOW(),
                     completed_at = NULL
                 WHERE id = $3
                   AND status = $4",
            )
            .bind(effective_dispatch_id.as_deref())
            .bind(effective_slot_index)
            .bind(entry_id)
            .bind(&current.status)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update auto-queue entry {entry_id} -> dispatched: {error}"))?
            .rows_affected(),
            ENTRY_STATUS_DONE => sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'done',
                     completed_at = NOW()
                 WHERE id = $1
                   AND status = $2",
            )
            .bind(entry_id)
            .bind(&current.status)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update auto-queue entry {entry_id} -> done: {error}"))?
            .rows_affected(),
            ENTRY_STATUS_SKIPPED => sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'skipped',
                     dispatch_id = NULL,
                     slot_index = NULL,
                     dispatched_at = NULL,
                     completed_at = NOW()
                 WHERE id = $1
                   AND status = $2",
            )
            .bind(entry_id)
            .bind(&current.status)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update auto-queue entry {entry_id} -> skipped: {error}"))?
            .rows_affected(),
            ENTRY_STATUS_FAILED => sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'failed',
                     dispatch_id = NULL,
                     slot_index = NULL,
                     dispatched_at = NULL,
                     completed_at = NOW()
                 WHERE id = $1
                   AND status = $2",
            )
            .bind(entry_id)
            .bind(&current.status)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update auto-queue entry {entry_id} -> failed: {error}"))?
            .rows_affected(),
            ENTRY_STATUS_USER_CANCELLED => sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'user_cancelled',
                     dispatch_id = NULL,
                     slot_index = NULL,
                     dispatched_at = NULL,
                     completed_at = NOW()
                 WHERE id = $1
                   AND status = $2",
            )
            .bind(entry_id)
            .bind(&current.status)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("update auto-queue entry {entry_id} -> user_cancelled: {error}")
            })?
            .rows_affected(),
            _ => unreachable!(),
        };

        if rows_affected == 0 {
            drop(tx);
            let latest = load_entry_status_row_pg(pool, entry_id).await?;
            if entry_status_row_matches_target(
                &latest,
                normalized,
                effective_dispatch_id.as_deref(),
                effective_slot_index,
            ) {
                return Ok(EntryStatusUpdateResult {
                    run_id: latest.run_id,
                    from_status: latest.status,
                    to_status: normalized.to_string(),
                    changed: false,
                });
            }

            if !is_allowed_entry_transition(&latest.status, normalized, trigger_source) {
                let stale_log_ctx = crate::services::auto_queue::AutoQueueLogContext::new()
                    .run(&latest.run_id)
                    .entry(entry_id)
                    .card(&latest.card_id)
                    .maybe_dispatch(latest.dispatch_id.as_deref())
                    .agent(&latest.agent_id)
                    .thread_group(latest.thread_group)
                    .batch_phase(latest.batch_phase)
                    .maybe_slot_index(latest.slot_index);
                crate::auto_queue_log!(
                    warn,
                    "entry_status_stale_transition_blocked_pg",
                    stale_log_ctx,
                    "[auto-queue] stale PG entry transition blocked {} {} -> {} (source: {})",
                    entry_id,
                    latest.status,
                    normalized,
                    trigger_source
                );
                return Err(format!(
                    "invalid auto-queue entry transition for {entry_id}: {} -> {normalized}",
                    latest.status
                ));
            }

            current = latest;
            continue;
        }

        if normalized == ENTRY_STATUS_DISPATCHED {
            if let Some(previous_dispatch_id) = current
                .dispatch_id
                .as_deref()
                .filter(|value| Some(*value) != effective_dispatch_id.as_deref())
            {
                record_entry_dispatch_history_on_pg(
                    &mut tx,
                    entry_id,
                    previous_dispatch_id,
                    trigger_source,
                )
                .await?;
            }
            if let Some(dispatch_id) = effective_dispatch_id.as_deref() {
                record_entry_dispatch_history_on_pg(&mut tx, entry_id, dispatch_id, trigger_source)
                    .await?;
            }
        }

        record_entry_transition_on_pg(
            &mut tx,
            entry_id,
            &current.status,
            normalized,
            trigger_source,
        )
        .await?;

        // #815 P1: `user_cancelled` is a NON-run-finalizing terminal status.
        // The run must stay in its prior state (`active` / `paused`) so the
        // operator can flip the entry back to `pending` (e.g. via the API) and
        // a later tick can re-pick it up. Auto-completing the run would
        // strand the entry — `restore` only accepts cancelled/restoring,
        // `resume` only reopens paused, and `activate()` only promotes
        // generated/pending, so no path could re-open the entry.
        if matches!(
            normalized,
            ENTRY_STATUS_DONE | ENTRY_STATUS_SKIPPED | ENTRY_STATUS_FAILED
        ) {
            maybe_finalize_run_after_terminal_entry_pg(&mut tx, &current.run_id, normalized)
                .await?;
        }

        tx.commit()
            .await
            .map_err(|error| format!("commit postgres entry transition for {entry_id}: {error}"))?;

        return Ok(EntryStatusUpdateResult {
            run_id: current.run_id,
            from_status: current.status,
            to_status: normalized.to_string(),
            changed: true,
        });
    }
}

/// Transaction-scoped variant of [`update_entry_status_on_pg`].
///
/// Mirrors the pool-scoped helper's semantics — transition validation,
/// dispatch-history bookkeeping, transition recording, and conditional run
/// finalization — but operates inside a caller-owned transaction.
///
/// Unlike the pool-scoped helper this is single-shot: on stale-row mismatch
/// it returns `changed: false` rather than re-reading state and looping. The
/// caller composes this into a wider atomic operation, so observed state is
/// already a stable snapshot inside the transaction.
pub async fn update_entry_status_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
    new_status: &str,
    trigger_source: &str,
    options: &EntryStatusUpdateOptions,
) -> Result<EntryStatusUpdateResult, String> {
    let normalized = normalize_entry_status(new_status).map_err(|error| error.to_string())?;
    let current = load_entry_status_row_pg_tx(tx, entry_id).await?;

    let log_ctx = crate::services::auto_queue::AutoQueueLogContext::new()
        .run(&current.run_id)
        .entry(entry_id)
        .card(&current.card_id)
        .maybe_dispatch(current.dispatch_id.as_deref())
        .agent(&current.agent_id)
        .thread_group(current.thread_group)
        .batch_phase(current.batch_phase)
        .maybe_slot_index(current.slot_index);

    if !is_allowed_entry_transition(&current.status, normalized, trigger_source) {
        crate::auto_queue_log!(
            warn,
            "entry_status_transition_blocked_pg_tx",
            log_ctx,
            "[auto-queue] blocked invalid PG entry transition (tx) {} {} -> {} (source: {})",
            entry_id,
            current.status,
            normalized,
            trigger_source
        );
        return Err(format!(
            "invalid auto-queue entry transition for {entry_id}: {} -> {normalized}",
            current.status
        ));
    }

    let effective_dispatch_id = options
        .dispatch_id
        .clone()
        .or_else(|| current.dispatch_id.clone());
    let effective_slot_index = options.slot_index.or(current.slot_index);
    let metadata_change = match normalized {
        ENTRY_STATUS_PENDING => {
            current.dispatch_id.is_some()
                || current.slot_index.is_some()
                || current.completed_at.is_some()
        }
        ENTRY_STATUS_DISPATCHED => {
            effective_dispatch_id != current.dispatch_id
                || effective_slot_index != current.slot_index
                || current.completed_at.is_some()
        }
        ENTRY_STATUS_DONE
        | ENTRY_STATUS_SKIPPED
        | ENTRY_STATUS_FAILED
        | ENTRY_STATUS_USER_CANCELLED => false,
        _ => false,
    };
    let changed = current.status != normalized || metadata_change;

    if !changed {
        return Ok(EntryStatusUpdateResult {
            run_id: current.run_id,
            from_status: current.status,
            to_status: normalized.to_string(),
            changed: false,
        });
    }

    let rows_affected = match normalized {
        ENTRY_STATUS_PENDING => sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'pending',
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = NULL,
                 retry_count = CASE
                     WHEN $3 = 'failed' THEN 0
                     ELSE retry_count
                 END
             WHERE id = $1
               AND status = $2",
        )
        .bind(entry_id)
        .bind(&current.status)
        .bind(&current.status)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("update auto-queue entry {entry_id} -> pending: {error}"))?
        .rows_affected(),
        ENTRY_STATUS_DISPATCHED => sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'dispatched',
                 dispatch_id = $1,
                 slot_index = $2,
                 dispatched_at = NOW(),
                 completed_at = NULL
             WHERE id = $3
               AND status = $4",
        )
        .bind(effective_dispatch_id.as_deref())
        .bind(effective_slot_index)
        .bind(entry_id)
        .bind(&current.status)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("update auto-queue entry {entry_id} -> dispatched: {error}"))?
        .rows_affected(),
        ENTRY_STATUS_DONE => sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'done',
                 completed_at = NOW()
             WHERE id = $1
               AND status = $2",
        )
        .bind(entry_id)
        .bind(&current.status)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("update auto-queue entry {entry_id} -> done: {error}"))?
        .rows_affected(),
        ENTRY_STATUS_SKIPPED => sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'skipped',
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = NOW()
             WHERE id = $1
               AND status = $2",
        )
        .bind(entry_id)
        .bind(&current.status)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("update auto-queue entry {entry_id} -> skipped: {error}"))?
        .rows_affected(),
        ENTRY_STATUS_FAILED => sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'failed',
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = NOW()
             WHERE id = $1
               AND status = $2",
        )
        .bind(entry_id)
        .bind(&current.status)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("update auto-queue entry {entry_id} -> failed: {error}"))?
        .rows_affected(),
        ENTRY_STATUS_USER_CANCELLED => sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'user_cancelled',
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = NOW()
             WHERE id = $1
               AND status = $2",
        )
        .bind(entry_id)
        .bind(&current.status)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("update auto-queue entry {entry_id} -> user_cancelled: {error}"))?
        .rows_affected(),
        _ => unreachable!(),
    };

    if rows_affected == 0 {
        // Stale snapshot — the row mutated between our load and update inside
        // the same tx. Surface as a no-op; the caller already owns the tx
        // boundary and decides whether to roll back.
        return Ok(EntryStatusUpdateResult {
            run_id: current.run_id,
            from_status: current.status.clone(),
            to_status: current.status,
            changed: false,
        });
    }

    if normalized == ENTRY_STATUS_DISPATCHED {
        if let Some(previous_dispatch_id) = current
            .dispatch_id
            .as_deref()
            .filter(|value| Some(*value) != effective_dispatch_id.as_deref())
        {
            record_entry_dispatch_history_on_pg(tx, entry_id, previous_dispatch_id, trigger_source)
                .await?;
        }
        if let Some(dispatch_id) = effective_dispatch_id.as_deref() {
            record_entry_dispatch_history_on_pg(tx, entry_id, dispatch_id, trigger_source).await?;
        }
    }

    record_entry_transition_on_pg(tx, entry_id, &current.status, normalized, trigger_source)
        .await?;

    // #815 P1: `user_cancelled` is intentionally NOT in this list — the run
    // must stay in its prior state so the operator can flip the entry back to
    // `pending` and a later tick can re-pick it up.
    if matches!(
        normalized,
        ENTRY_STATUS_DONE | ENTRY_STATUS_SKIPPED | ENTRY_STATUS_FAILED
    ) {
        maybe_finalize_run_after_terminal_entry_pg(tx, &current.run_id, normalized).await?;
    }

    Ok(EntryStatusUpdateResult {
        run_id: current.run_id,
        from_status: current.status,
        to_status: normalized.to_string(),
        changed: true,
    })
}

/// Route dispatch-linked terminal transitions through the canonical entry
/// helper so PG transition bookkeeping and run finalization stay consistent.
pub async fn sync_dispatch_terminal_entries_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    new_status: &str,
    trigger_source: &str,
    preserve_dispatch_link: bool,
) -> Result<usize, String> {
    Ok(sync_dispatch_terminal_entries_on_pg_tx_result(
        tx,
        dispatch_id,
        new_status,
        trigger_source,
        preserve_dispatch_link,
    )
    .await?
    .changed_entries)
}

async fn sync_dispatch_terminal_entries_on_pg_tx_result(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    new_status: &str,
    trigger_source: &str,
    preserve_dispatch_link: bool,
) -> Result<DispatchTerminalEntrySyncResult, String> {
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
    let rows = sqlx::query(
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
                    (SELECT status FROM task_dispatches WHERE id = e.dispatch_id),
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
                    (SELECT status FROM task_dispatches WHERE id = e.dispatch_id),
                    ''
                  ) IN ('cancelled', 'failed', 'superseded')
               OR EXISTS (
                    SELECT 1
                    FROM auto_queue_entry_dispatch_history h
                    WHERE h.entry_id = e.id
                      AND h.dispatch_id = $1
               )
            )
         )",
    )
    .bind(dispatch_id)
    .bind(new_status)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        format!("load postgres auto-queue entries for dispatch {dispatch_id}: {error}")
    })?;

    let mut changed = 0usize;
    let mut affected_run_ids = BTreeSet::new();
    for row in rows {
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
        let update_trigger_source =
            if entry_status == ENTRY_STATUS_FAILED && new_status == ENTRY_STATUS_DONE {
                "dispatch_terminal_reconcile"
            } else {
                trigger_source
            };
        let result = update_entry_status_on_pg_tx(
            tx,
            &entry_id,
            new_status,
            update_trigger_source,
            &EntryStatusUpdateOptions::default(),
        )
        .await?;
        if result.changed {
            if entry_status == ENTRY_STATUS_FAILED && new_status == ENTRY_STATUS_DONE {
                record_entry_dispatch_history_on_pg(tx, &entry_id, dispatch_id, trigger_source)
                    .await?;
            }
            if preserve_dispatch_link {
                if let Some(linked_dispatch_id) = linked_dispatch_id {
                    sqlx::query(
                        "UPDATE auto_queue_entries
                         SET dispatch_id = $1,
                             slot_index = $2
                         WHERE id = $3",
                    )
                    .bind(&linked_dispatch_id)
                    .bind(slot_index)
                    .bind(&entry_id)
                    .execute(&mut **tx)
                    .await
                    .map_err(|error| {
                        format!("restore postgres auto-queue entry lineage for {entry_id}: {error}")
                    })?;
                }
            }
            affected_run_ids.insert(run_id);
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
pub async fn finalize_completed_dispatch_terminal_entry_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
    trigger_source: &str,
    preserve_dispatch_link: bool,
) -> Result<DispatchTerminalEntrySyncResult, String> {
    let mut result = sync_dispatch_terminal_entries_on_pg_tx_result(
        tx,
        dispatch_id,
        ENTRY_STATUS_DONE,
        trigger_source,
        preserve_dispatch_link,
    )
    .await?;

    for run_id in result.affected_run_ids.clone() {
        if auto_queue_run_review_disabled_on_pg_tx(tx, &run_id).await?
            && maybe_finalize_run_if_ready_pg(tx, &run_id).await?
        {
            result.finalized_run_ids.push(run_id);
        }
    }

    Ok(result)
}

/// Transaction-scoped equivalent of [`load_entry_status_row_pg`] used by
/// [`update_entry_status_on_pg_tx`].
///
/// Note: `agent_id` is nullable in the PG schema (see
/// `migrations/postgres/0001_initial_schema.sql`) — older fixtures and
/// mid-migration rows can carry NULL. The pool variant decodes it strictly,
/// but this tx variant fans out to broader callers (the dispatch cancel path
/// included), so we coalesce NULL to an empty string to avoid spuriously
/// failing the cancel just because the entry was seeded without an agent.
async fn load_entry_status_row_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
) -> Result<EntryStatusRow, String> {
    let row = sqlx::query(
        "SELECT run_id,
                COALESCE(kanban_card_id, '') AS kanban_card_id,
                agent_id,
                status,
                dispatch_id,
                COALESCE(retry_count, 0)::BIGINT AS retry_count,
                slot_index::BIGINT AS slot_index,
                COALESCE(thread_group, 0)::BIGINT AS thread_group,
                COALESCE(batch_phase, 0)::BIGINT AS batch_phase,
                completed_at::text AS completed_at
         FROM auto_queue_entries
         WHERE id = $1",
    )
    .bind(entry_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load postgres auto-queue entry {entry_id}: {error}"))?
    .ok_or_else(|| format!("auto-queue entry not found: {entry_id}"))?;

    let agent_id_opt: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode auto-queue entry {entry_id} agent_id: {error}"))?;

    Ok(EntryStatusRow {
        run_id: row
            .try_get("run_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} run_id: {error}"))?,
        card_id: row.try_get("kanban_card_id").map_err(|error| {
            format!("decode auto-queue entry {entry_id} kanban_card_id: {error}")
        })?,
        agent_id: agent_id_opt.unwrap_or_default(),
        status: row
            .try_get("status")
            .map_err(|error| format!("decode auto-queue entry {entry_id} status: {error}"))?,
        dispatch_id: row
            .try_get("dispatch_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} dispatch_id: {error}"))?,
        retry_count: row
            .try_get("retry_count")
            .map_err(|error| format!("decode auto-queue entry {entry_id} retry_count: {error}"))?,
        slot_index: row
            .try_get("slot_index")
            .map_err(|error| format!("decode auto-queue entry {entry_id} slot_index: {error}"))?,
        thread_group: row
            .try_get("thread_group")
            .map_err(|error| format!("decode auto-queue entry {entry_id} thread_group: {error}"))?,
        batch_phase: row
            .try_get("batch_phase")
            .map_err(|error| format!("decode auto-queue entry {entry_id} batch_phase: {error}"))?,
        completed_at: row
            .try_get("completed_at")
            .map_err(|error| format!("decode auto-queue entry {entry_id} completed_at: {error}"))?,
    })
}

async fn record_entry_dispatch_history_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
    dispatch_id: &str,
    trigger_source: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO auto_queue_entry_dispatch_history (
             entry_id, dispatch_id, trigger_source
         )
         SELECT $1, $2, $3
         WHERE EXISTS (
             SELECT 1 FROM task_dispatches WHERE id = $2
         )
         ON CONFLICT DO NOTHING",
    )
    .bind(entry_id)
    .bind(dispatch_id)
    .bind(trigger_source)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        format!("record dispatch history for auto-queue entry {entry_id} ({dispatch_id}): {error}")
    })?;
    Ok(())
}

pub async fn list_entry_dispatch_history_pg(
    pool: &PgPool,
    entry_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT dispatch_id
         FROM auto_queue_entry_dispatch_history
         WHERE entry_id = $1
         ORDER BY id ASC",
    )
    .bind(entry_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| row.try_get("dispatch_id"))
        .collect()
}

fn normalized_optional_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(super) fn resume_session_id_from_context(context: Option<&str>) -> Option<String> {
    let context = context
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(|value| value.is_object())?;
    context
        .get("auto_queue_retry_resume_session_id")
        .or_else(|| context.get("resume_session_id"))
        .and_then(|value| value.as_str())
        .and_then(|value| normalized_optional_text(Some(value)))
}

pub async fn latest_entry_phase_codex_session_id_pg(
    pool: &PgPool,
    entry_id: &str,
    dispatch_type: &str,
) -> Result<Option<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT
             h.dispatch_id,
             d.context,
             session_state.claude_session_id,
             session_state.raw_provider_session_id,
             turn_state.session_id AS turn_session_id
         FROM auto_queue_entry_dispatch_history h
         JOIN task_dispatches d ON d.id = h.dispatch_id
         LEFT JOIN LATERAL (
             SELECT claude_session_id, raw_provider_session_id
             FROM sessions
             WHERE active_dispatch_id = h.dispatch_id
               AND provider = 'codex'
             ORDER BY last_heartbeat DESC NULLS LAST, created_at DESC NULLS LAST
             LIMIT 1
         ) session_state ON TRUE
         LEFT JOIN LATERAL (
             SELECT session_id
             FROM turns
             WHERE dispatch_id = h.dispatch_id
               AND provider = 'codex'
               AND session_id IS NOT NULL
               AND BTRIM(session_id) != ''
             ORDER BY finished_at DESC NULLS LAST, started_at DESC NULLS LAST
             LIMIT 1
         ) turn_state ON TRUE
         WHERE h.entry_id = $1
           AND d.dispatch_type = $2
         ORDER BY h.id DESC
         LIMIT 10",
    )
    .bind(entry_id)
    .bind(dispatch_type)
    .fetch_all(pool)
    .await?;

    for row in rows {
        let claude_session_id: Option<String> = row.try_get("claude_session_id")?;
        if let Some(session_id) = normalized_optional_text(claude_session_id.as_deref()) {
            return Ok(Some(session_id));
        }

        let raw_provider_session_id: Option<String> = row.try_get("raw_provider_session_id")?;
        if let Some(session_id) = normalized_optional_text(raw_provider_session_id.as_deref()) {
            return Ok(Some(session_id));
        }

        let turn_session_id: Option<String> = row.try_get("turn_session_id")?;
        if let Some(session_id) = normalized_optional_text(turn_session_id.as_deref()) {
            return Ok(Some(session_id));
        }

        let context: Option<String> = row.try_get("context")?;
        if let Some(session_id) = resume_session_id_from_context(context.as_deref()) {
            return Ok(Some(session_id));
        }
    }

    Ok(None)
}

async fn load_entry_status_row_pg(pool: &PgPool, entry_id: &str) -> Result<EntryStatusRow, String> {
    let row = sqlx::query(
        "SELECT run_id,
                COALESCE(kanban_card_id, '') AS kanban_card_id,
                COALESCE(agent_id, '') AS agent_id,
                status,
                dispatch_id,
                COALESCE(retry_count, 0)::BIGINT AS retry_count,
                slot_index::BIGINT AS slot_index,
                COALESCE(thread_group, 0)::BIGINT AS thread_group,
                COALESCE(batch_phase, 0)::BIGINT AS batch_phase,
                completed_at::text AS completed_at
         FROM auto_queue_entries
         WHERE id = $1",
    )
    .bind(entry_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres auto-queue entry {entry_id}: {error}"))?
    .ok_or_else(|| format!("auto-queue entry not found: {entry_id}"))?;

    Ok(EntryStatusRow {
        run_id: row
            .try_get("run_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} run_id: {error}"))?,
        card_id: row.try_get("kanban_card_id").map_err(|error| {
            format!("decode auto-queue entry {entry_id} kanban_card_id: {error}")
        })?,
        agent_id: row
            .try_get("agent_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} agent_id: {error}"))?,
        status: row
            .try_get("status")
            .map_err(|error| format!("decode auto-queue entry {entry_id} status: {error}"))?,
        dispatch_id: row
            .try_get("dispatch_id")
            .map_err(|error| format!("decode auto-queue entry {entry_id} dispatch_id: {error}"))?,
        retry_count: row
            .try_get("retry_count")
            .map_err(|error| format!("decode auto-queue entry {entry_id} retry_count: {error}"))?,
        slot_index: row
            .try_get("slot_index")
            .map_err(|error| format!("decode auto-queue entry {entry_id} slot_index: {error}"))?,
        thread_group: row
            .try_get("thread_group")
            .map_err(|error| format!("decode auto-queue entry {entry_id} thread_group: {error}"))?,
        batch_phase: row
            .try_get("batch_phase")
            .map_err(|error| format!("decode auto-queue entry {entry_id} batch_phase: {error}"))?,
        completed_at: row
            .try_get("completed_at")
            .map_err(|error| format!("decode auto-queue entry {entry_id} completed_at: {error}"))?,
    })
}

fn normalize_entry_status(status: &str) -> Result<&str, EntryStatusUpdateError> {
    match status.trim() {
        ENTRY_STATUS_PENDING => Ok(ENTRY_STATUS_PENDING),
        ENTRY_STATUS_DISPATCHED => Ok(ENTRY_STATUS_DISPATCHED),
        ENTRY_STATUS_DONE => Ok(ENTRY_STATUS_DONE),
        ENTRY_STATUS_SKIPPED => Ok(ENTRY_STATUS_SKIPPED),
        ENTRY_STATUS_FAILED => Ok(ENTRY_STATUS_FAILED),
        ENTRY_STATUS_USER_CANCELLED => Ok(ENTRY_STATUS_USER_CANCELLED),
        other => Err(EntryStatusUpdateError::UnsupportedStatus {
            status: other.to_string(),
        }),
    }
}

fn is_allowed_entry_transition(from_status: &str, to_status: &str, trigger_source: &str) -> bool {
    if from_status == to_status {
        return true;
    }

    if from_status == ENTRY_STATUS_DONE
        && to_status == ENTRY_STATUS_DISPATCHED
        && matches!(trigger_source, "pmd_reopen" | "rereview_dispatch")
    {
        return true;
    }
    if from_status == ENTRY_STATUS_FAILED
        && to_status == ENTRY_STATUS_DONE
        && matches!(
            trigger_source,
            "manual_terminal_reconcile" | "dispatch_terminal_reconcile" | "card_terminal"
        )
    {
        return true;
    }

    matches!(
        (from_status, to_status),
        (ENTRY_STATUS_PENDING, ENTRY_STATUS_DISPATCHED)
            | (ENTRY_STATUS_PENDING, ENTRY_STATUS_DONE)
            | (ENTRY_STATUS_PENDING, ENTRY_STATUS_SKIPPED)
            | (ENTRY_STATUS_PENDING, ENTRY_STATUS_USER_CANCELLED)
            | (ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_FAILED)
            | (ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_PENDING)
            | (ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_DONE)
            | (ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_SKIPPED)
            | (ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_USER_CANCELLED)
            | (ENTRY_STATUS_FAILED, ENTRY_STATUS_PENDING)
            | (ENTRY_STATUS_FAILED, ENTRY_STATUS_SKIPPED)
            | (ENTRY_STATUS_SKIPPED, ENTRY_STATUS_PENDING)
            | (ENTRY_STATUS_SKIPPED, ENTRY_STATUS_DISPATCHED)
            | (ENTRY_STATUS_SKIPPED, ENTRY_STATUS_DONE)
            | (ENTRY_STATUS_USER_CANCELLED, ENTRY_STATUS_PENDING)
            | (ENTRY_STATUS_USER_CANCELLED, ENTRY_STATUS_SKIPPED)
    )
}

fn entry_status_row_matches_target(
    row: &EntryStatusRow,
    normalized: &str,
    effective_dispatch_id: Option<&str>,
    effective_slot_index: Option<i64>,
) -> bool {
    if row.status != normalized {
        return false;
    }

    match normalized {
        ENTRY_STATUS_PENDING => {
            row.dispatch_id.is_none() && row.slot_index.is_none() && row.completed_at.is_none()
        }
        ENTRY_STATUS_DISPATCHED => {
            row.dispatch_id.as_deref() == effective_dispatch_id
                && row.slot_index == effective_slot_index
                && row.completed_at.is_none()
        }
        ENTRY_STATUS_DONE | ENTRY_STATUS_SKIPPED => true,
        ENTRY_STATUS_FAILED | ENTRY_STATUS_USER_CANCELLED => {
            row.dispatch_id.is_none() && row.slot_index.is_none() && row.completed_at.is_some()
        }
        _ => false,
    }
}

async fn record_entry_transition_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
    from_status: &str,
    to_status: &str,
    trigger_source: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO auto_queue_entry_transitions (
             entry_id,
             from_status,
             to_status,
             trigger_source
         )
         VALUES ($1, $2, $3, $4)",
    )
    .bind(entry_id)
    .bind(from_status)
    .bind(to_status)
    .bind(trigger_source)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("record auto-queue transition for {entry_id}: {error}"))?;
    Ok(())
}
