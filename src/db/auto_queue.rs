use sqlx::{PgPool, Row as SqlxRow};
use std::collections::BTreeSet;
use thiserror::Error;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultationDispatchRecordResult {
    pub metadata_json: String,
    pub entry_status_changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchTerminalEntrySyncResult {
    pub changed_entries: usize,
    pub affected_run_ids: Vec<String>,
    pub finalized_run_ids: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ConsultationDispatchRecordError {
    #[error("consultation dispatch id is required")]
    MissingDispatchId,
    #[error("consultation trigger source is required")]
    MissingSource,
    #[error("consultation card not found: {card_id}")]
    CardNotFound { card_id: String },
}

const SLOT_ALLOCATION_MAX_RETRIES: usize = 16;
pub const SLOT_TERMINAL_DISPATCH_COOLDOWN_SECONDS: i64 = 45;

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

pub async fn update_entry_status_on_pg(
    pool: &PgPool,
    entry_id: &str,
    new_status: &str,
    trigger_source: &str,
    options: &EntryStatusUpdateOptions,
) -> Result<EntryStatusUpdateResult, String> {
    let normalized = normalize_entry_status(new_status).map_err(|error| error.to_string())?;
    let mut current = load_entry_status_row_pg(pool, entry_id).await?;

    loop {
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
    let rows = sqlx::query(
        "SELECT e.id, e.run_id, e.dispatch_id, e.slot_index
         FROM auto_queue_entries e
         WHERE e.status = 'dispatched'
           AND (
                 e.dispatch_id = $1
              OR (
                   e.kanban_card_id = (
                     SELECT kanban_card_id
                     FROM task_dispatches
                     WHERE id = $1
                   )
                   AND COALESCE(
                     (SELECT status FROM task_dispatches WHERE id = e.dispatch_id),
                     ''
                   ) IN ('cancelled', 'failed', 'superseded')
                 )
           )",
    )
    .bind(dispatch_id)
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
        let linked_dispatch_id: String = row.try_get("dispatch_id").map_err(|error| {
            format!("decode postgres auto-queue entry dispatch_id for {dispatch_id}: {error}")
        })?;
        let slot_index: Option<i64> = row.try_get("slot_index").map_err(|error| {
            format!("decode postgres auto-queue entry slot_index for {dispatch_id}: {error}")
        })?;
        let result = update_entry_status_on_pg_tx(
            tx,
            &entry_id,
            new_status,
            trigger_source,
            &EntryStatusUpdateOptions::default(),
        )
        .await?;
        if result.changed {
            if preserve_dispatch_link {
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

fn resume_session_id_from_context(context: Option<&str>) -> Option<String> {
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

pub async fn rebind_slot_for_group_agent_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
    slot_index: i64,
) -> Result<u64, String> {
    let slot_pool_size = run_slot_pool_size_pg(pool, run_id)
        .await
        .map_err(|error| format!("load postgres slot pool size for {run_id}: {error}"))?;
    ensure_agent_slot_pool_rows_pg(pool, agent_id, slot_pool_size)
        .await
        .map_err(|error| {
            format!("prepare postgres slot rows for run {run_id} agent {agent_id}: {error}")
        })?;

    let slot_updated = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = $1,
             assigned_thread_group = $2,
             updated_at = NOW()
         WHERE agent_id = $3
           AND slot_index = $4
           AND (assigned_run_id IS NULL OR assigned_run_id = $1)",
    )
    .bind(run_id)
    .bind(thread_group)
    .bind(agent_id)
    .bind(slot_index)
    .execute(pool)
    .await
    .map_err(|error| {
        format!(
            "rebind postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
        )
    })?
    .rows_affected();
    if slot_updated == 0 {
        return Ok(0);
    }

    bind_slot_index_for_group_entries_pg(pool, run_id, agent_id, thread_group, slot_index)
        .await
        .map_err(|error| {
            format!(
                "bind rebound postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
            )
        })
}

#[derive(Debug, Clone, Default)]
pub struct GenerateCardFilter {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub issue_numbers: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Default)]
pub struct StatusFilter {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BacklogCardRecord {
    pub card_id: String,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GenerateCandidateRecord {
    pub card_id: String,
    pub agent_id: String,
    pub priority: String,
    pub description: Option<String>,
    pub metadata: Option<String>,
    pub github_issue_number: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct AutoQueueRunRecord {
    pub id: String,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub review_mode: String,
    pub status: String,
    pub timeout_minutes: i64,
    pub ai_model: Option<String>,
    pub ai_rationale: Option<String>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub max_concurrent_threads: i64,
    pub thread_group_count: i64,
}

#[derive(Debug, Clone)]
pub struct StatusEntryRecord {
    pub id: String,
    pub agent_id: String,
    pub card_id: String,
    pub dispatch_id: Option<String>,
    pub priority_rank: i64,
    pub reason: Option<String>,
    pub status: String,
    pub retry_count: i64,
    pub created_at: i64,
    pub dispatched_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub card_title: Option<String>,
    pub github_issue_number: Option<i64>,
    pub github_repo: Option<String>,
    pub thread_group: i64,
    pub slot_index: Option<i64>,
    pub batch_phase: i64,
    pub channel_thread_map: Option<String>,
    pub active_thread_id: Option<String>,
    pub card_status: Option<String>,
    pub review_round: i64,
}

#[derive(Debug, Clone)]
pub struct AutoQueueRunHistoryRecord {
    pub id: String,
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub entry_count: i64,
    pub done_count: i64,
    pub skipped_count: i64,
    pub pending_count: i64,
    pub dispatched_count: i64,
}

pub async fn find_latest_run_id_pg(
    pool: &PgPool,
    filter: &StatusFilter,
) -> Result<Option<String>, sqlx::Error> {
    let repo = filter.repo.as_deref().filter(|value| !value.is_empty());
    let agent_id = filter.agent_id.as_deref().filter(|value| !value.is_empty());

    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_runs
         WHERE ($1::TEXT IS NULL OR repo = $1 OR repo IS NULL OR repo = '')
           AND ($2::TEXT IS NULL OR agent_id = $2 OR agent_id IS NULL OR agent_id = '')
         ORDER BY created_at DESC
         LIMIT 1",
    )
    .bind(repo)
    .bind(agent_id)
    .fetch_optional(pool)
    .await
}

pub async fn get_run_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<Option<AutoQueueRunRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id,
                repo,
                agent_id,
                COALESCE(review_mode, 'enabled') AS review_mode,
                status,
                timeout_minutes::BIGINT AS timeout_minutes,
                ai_model,
                ai_rationale,
                EXTRACT(EPOCH FROM created_at)::BIGINT * 1000 AS created_at,
                CASE WHEN completed_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM completed_at)::BIGINT * 1000
                END AS completed_at,
                COALESCE(max_concurrent_threads, 1)::BIGINT AS max_concurrent_threads,
                COALESCE(thread_group_count, 1)::BIGINT AS thread_group_count
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| auto_queue_run_record_from_pg_row(&row))
        .transpose()
}

pub async fn get_status_entry_pg(
    pool: &PgPool,
    entry_id: &str,
) -> Result<Option<StatusEntryRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT e.id,
                e.agent_id,
                COALESCE(e.kanban_card_id, '') AS kanban_card_id,
                e.dispatch_id,
                e.priority_rank::BIGINT AS priority_rank,
                e.reason,
                e.status,
                COALESCE(e.retry_count, 0)::BIGINT AS retry_count,
                EXTRACT(EPOCH FROM e.created_at)::BIGINT * 1000 AS created_at,
                CASE WHEN e.dispatched_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM e.dispatched_at)::BIGINT * 1000
                END AS dispatched_at,
                CASE WHEN e.completed_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM e.completed_at)::BIGINT * 1000
                END AS completed_at,
                kc.title,
                kc.github_issue_number::BIGINT AS github_issue_number,
                kc.github_issue_url AS github_repo,
                COALESCE(e.thread_group, 0)::BIGINT AS thread_group,
                e.slot_index::BIGINT AS slot_index,
                COALESCE(e.batch_phase, 0)::BIGINT AS batch_phase,
                kc.channel_thread_map::text AS channel_thread_map,
                kc.active_thread_id,
                kc.status AS card_status,
                GREATEST(COALESCE(crs.review_round, 0), COALESCE(kc.review_round, 0))::BIGINT AS review_round
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id
         LEFT JOIN card_review_state crs ON e.kanban_card_id = crs.card_id
         WHERE e.id = $1",
    )
    .bind(entry_id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| status_entry_record_from_pg_row(&row))
        .transpose()
}

pub async fn list_status_entries_pg(
    pool: &PgPool,
    run_id: &str,
    filter: &StatusFilter,
) -> Result<Vec<StatusEntryRecord>, sqlx::Error> {
    let agent_id = filter.agent_id.as_deref().filter(|value| !value.is_empty());
    let repo = filter.repo.as_deref().filter(|value| !value.is_empty());

    let rows = sqlx::query(
        "SELECT e.id,
                e.agent_id,
                COALESCE(e.kanban_card_id, '') AS kanban_card_id,
                e.dispatch_id,
                e.priority_rank::BIGINT AS priority_rank,
                e.reason,
                e.status,
                COALESCE(e.retry_count, 0)::BIGINT AS retry_count,
                EXTRACT(EPOCH FROM e.created_at)::BIGINT * 1000 AS created_at,
                CASE WHEN e.dispatched_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM e.dispatched_at)::BIGINT * 1000
                END AS dispatched_at,
                CASE WHEN e.completed_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM e.completed_at)::BIGINT * 1000
                END AS completed_at,
                kc.title,
                kc.github_issue_number::BIGINT AS github_issue_number,
                kc.github_issue_url AS github_repo,
                COALESCE(e.thread_group, 0)::BIGINT AS thread_group,
                e.slot_index::BIGINT AS slot_index,
                COALESCE(e.batch_phase, 0)::BIGINT AS batch_phase,
                kc.channel_thread_map::text AS channel_thread_map,
                kc.active_thread_id,
                kc.status AS card_status,
                GREATEST(COALESCE(crs.review_round, 0), COALESCE(kc.review_round, 0))::BIGINT AS review_round
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id
         LEFT JOIN card_review_state crs ON e.kanban_card_id = crs.card_id
         WHERE e.run_id = $1
           AND ($2::TEXT IS NULL OR e.agent_id = $2)
           AND ($3::TEXT IS NULL OR kc.repo_id = $3)
         ORDER BY e.priority_rank ASC",
    )
    .bind(run_id)
    .bind(agent_id)
    .bind(repo)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| status_entry_record_from_pg_row(&row))
        .collect()
}

pub async fn list_run_history_pg(
    pool: &PgPool,
    filter: &StatusFilter,
    limit: usize,
) -> Result<Vec<AutoQueueRunHistoryRecord>, sqlx::Error> {
    let repo = filter.repo.as_deref().filter(|value| !value.is_empty());
    let agent_id = filter.agent_id.as_deref().filter(|value| !value.is_empty());
    let limit = limit.clamp(1, 20) as i64;

    let rows = sqlx::query(
        "SELECT r.id,
                r.repo,
                r.agent_id,
                r.status,
                EXTRACT(EPOCH FROM r.created_at)::BIGINT * 1000 AS created_at,
                CASE WHEN r.completed_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM r.completed_at)::BIGINT * 1000
                END AS completed_at,
                COUNT(e.id)::BIGINT AS entry_count,
                COALESCE(SUM(CASE WHEN e.status = 'done' THEN 1 ELSE 0 END), 0)::BIGINT AS done_count,
                COALESCE(SUM(CASE WHEN e.status = 'skipped' THEN 1 ELSE 0 END), 0)::BIGINT AS skipped_count,
                COALESCE(SUM(CASE WHEN e.status = 'pending' THEN 1 ELSE 0 END), 0)::BIGINT AS pending_count,
                COALESCE(SUM(CASE WHEN e.status = 'dispatched' THEN 1 ELSE 0 END), 0)::BIGINT AS dispatched_count
         FROM auto_queue_runs r
         LEFT JOIN auto_queue_entries e ON e.run_id = r.id
         LEFT JOIN kanban_cards kc ON kc.id = e.kanban_card_id
         WHERE ($1::TEXT IS NULL OR COALESCE(kc.repo_id, r.repo, '') = $1)
           AND ($2::TEXT IS NULL OR COALESCE(e.agent_id, r.agent_id, '') = $2)
         GROUP BY r.id, r.repo, r.agent_id, r.status, r.created_at, r.completed_at
         ORDER BY r.created_at DESC
         LIMIT $3",
    )
    .bind(repo)
    .bind(agent_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| auto_queue_run_history_record_from_pg_row(&row))
        .collect()
}

pub async fn list_backlog_cards_pg(
    pool: &PgPool,
    filter: &GenerateCardFilter,
) -> Result<Vec<BacklogCardRecord>, sqlx::Error> {
    let repo = filter.repo.as_deref().filter(|value| !value.is_empty());
    let agent_id = filter.agent_id.as_deref().filter(|value| !value.is_empty());
    let issue_numbers = filter
        .issue_numbers
        .as_ref()
        .filter(|nums| !nums.is_empty())
        .cloned();

    let rows = sqlx::query(
        "SELECT kc.id,
                kc.repo_id,
                kc.assigned_agent_id
         FROM kanban_cards kc
         WHERE kc.status = 'backlog'
           AND ($1::TEXT IS NULL OR kc.repo_id = $1)
           AND ($2::TEXT IS NULL OR kc.assigned_agent_id = $2)
           AND ($3::BIGINT[] IS NULL OR kc.github_issue_number::BIGINT = ANY($3::BIGINT[]))",
    )
    .bind(repo)
    .bind(agent_id)
    .bind(issue_numbers)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(BacklogCardRecord {
                card_id: row.try_get("id")?,
                repo_id: row.try_get("repo_id")?,
                assigned_agent_id: row.try_get("assigned_agent_id")?,
            })
        })
        .collect()
}

pub async fn list_generate_candidates_pg(
    pool: &PgPool,
    filter: &GenerateCardFilter,
    enqueueable_states: &[String],
) -> Result<Vec<GenerateCandidateRecord>, sqlx::Error> {
    let repo = filter.repo.as_deref().filter(|value| !value.is_empty());
    let agent_id = filter.agent_id.as_deref().filter(|value| !value.is_empty());
    let issue_numbers = filter
        .issue_numbers
        .as_ref()
        .filter(|nums| !nums.is_empty())
        .cloned();

    let rows = sqlx::query(
        "SELECT kc.id,
                kc.assigned_agent_id,
                kc.priority,
                kc.description,
                kc.metadata::TEXT AS metadata,
                kc.github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards kc
         WHERE kc.status = ANY($1::TEXT[])
           AND ($2::TEXT IS NULL OR kc.repo_id = $2)
           AND ($3::TEXT IS NULL OR kc.assigned_agent_id = $3)
           AND ($4::BIGINT[] IS NULL OR kc.github_issue_number::BIGINT = ANY($4::BIGINT[]))
         ORDER BY
           CASE kc.priority
             WHEN 'urgent' THEN 0
             WHEN 'high' THEN 1
             WHEN 'medium' THEN 2
             WHEN 'low' THEN 3
             ELSE 4
           END,
           kc.created_at ASC",
    )
    .bind(enqueueable_states)
    .bind(repo)
    .bind(agent_id)
    .bind(issue_numbers)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(GenerateCandidateRecord {
                card_id: row.try_get("id")?,
                agent_id: row
                    .try_get::<Option<String>, _>("assigned_agent_id")?
                    .unwrap_or_default(),
                priority: row
                    .try_get::<Option<String>, _>("priority")?
                    .unwrap_or_else(|| "medium".to_string()),
                description: row.try_get("description")?,
                metadata: row.try_get("metadata")?,
                github_issue_number: row.try_get("github_issue_number")?,
            })
        })
        .collect()
}

pub async fn count_cards_by_status_pg(
    pool: &PgPool,
    repo: Option<&str>,
    agent_id: Option<&str>,
    status: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM kanban_cards
         WHERE status = $1
           AND ($2::TEXT IS NULL OR repo_id = $2)
           AND ($3::TEXT IS NULL OR assigned_agent_id = $3)",
    )
    .bind(status)
    .bind(repo.filter(|value| !value.is_empty()))
    .bind(agent_id.filter(|value| !value.is_empty()))
    .fetch_one(pool)
    .await
}

pub async fn run_slot_pool_size_pg(pool: &PgPool, run_id: &str) -> Result<i64, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COALESCE(max_concurrent_threads, 1)::BIGINT
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .unwrap_or(1)
    .clamp(1, 10))
}

pub async fn ensure_agent_slot_pool_rows_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_pool_size: i64,
) -> Result<(), sqlx::Error> {
    for slot_index in 0..slot_pool_size.clamp(1, 32) {
        sqlx::query(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ($1, $2, '{}'::jsonb)
             ON CONFLICT (agent_id, slot_index) DO NOTHING",
        )
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn clear_inactive_slot_assignments_pg(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id IS NOT NULL
           AND assigned_run_id NOT IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn release_run_slots_pg(pool: &PgPool, run_id: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id = $1",
    )
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn current_batch_phase_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(COALESCE(batch_phase, 0))::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
}

pub fn batch_phase_is_eligible(batch_phase: i64, current_phase: Option<i64>) -> bool {
    match current_phase {
        Some(phase) => batch_phase == phase,
        None => true,
    }
}

#[allow(dead_code)]
pub async fn run_has_blocking_phase_gate_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM auto_queue_phase_gates
         WHERE run_id = $1
           AND status IN ('pending', 'failed')",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
}

fn consultation_metadata_object(
    base_metadata_json: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let trimmed = base_metadata_json.trim();
    if trimmed.is_empty() {
        return serde_json::Map::new();
    }

    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

pub async fn record_consultation_dispatch_on_pg(
    pool: &PgPool,
    entry_id: &str,
    card_id: &str,
    dispatch_id: &str,
    trigger_source: &str,
    base_metadata_json: &str,
) -> Result<ConsultationDispatchRecordResult, String> {
    let dispatch_id = dispatch_id.trim();
    if dispatch_id.is_empty() {
        return Err(ConsultationDispatchRecordError::MissingDispatchId.to_string());
    }
    let trigger_source = trigger_source.trim();
    if trigger_source.is_empty() {
        return Err(ConsultationDispatchRecordError::MissingSource.to_string());
    }

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres consultation dispatch transaction: {error}"))?;
    let mut metadata = consultation_metadata_object(base_metadata_json);
    metadata.insert(
        "consultation_status".to_string(),
        serde_json::json!("pending"),
    );
    metadata.insert(
        "consultation_dispatch_id".to_string(),
        serde_json::json!(dispatch_id),
    );
    let metadata_json = serde_json::Value::Object(metadata).to_string();

    let updated = sqlx::query(
        "UPDATE kanban_cards
         SET metadata = $1::jsonb,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(&metadata_json)
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("update postgres consultation metadata for {card_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        tx.rollback().await.map_err(|error| {
            format!("rollback missing postgres consultation card {card_id}: {error}")
        })?;
        return Err(ConsultationDispatchRecordError::CardNotFound {
            card_id: card_id.to_string(),
        }
        .to_string());
    }

    let entry_result = update_entry_status_on_pg_tx(
        &mut tx,
        entry_id,
        ENTRY_STATUS_DISPATCHED,
        trigger_source,
        &EntryStatusUpdateOptions {
            dispatch_id: Some(dispatch_id.to_string()),
            slot_index: None,
        },
    )
    .await?;
    if !entry_result.changed {
        tx.rollback().await.map_err(|error| {
            format!("rollback stale postgres consultation dispatch entry {entry_id}: {error}")
        })?;
        return Err(format!(
            "stale postgres consultation dispatch entry {entry_id}: status update was not applied"
        ));
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres consultation dispatch transaction: {error}"))?;

    Ok(ConsultationDispatchRecordResult {
        metadata_json,
        entry_status_changed: entry_result.changed,
    })
}

#[derive(Debug, Clone, Default)]
pub struct PhaseGateStateWrite {
    pub status: String,
    pub verdict: Option<String>,
    pub dispatch_ids: Vec<String>,
    pub pass_verdict: String,
    pub next_phase: Option<i64>,
    pub final_phase: bool,
    pub anchor_card_id: Option<String>,
    pub failure_reason: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhaseGateSaveResult {
    pub persisted_dispatch_ids: Vec<String>,
    pub removed_stale_rows: usize,
}

fn normalize_phase_gate_status(status: &str) -> String {
    let trimmed = status.trim();
    if trimmed.is_empty() {
        "pending".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_phase_gate_pass_verdict(pass_verdict: &str) -> String {
    let trimmed = pass_verdict.trim();
    if trimmed.is_empty() {
        "phase_gate_passed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_optional_text(value: Option<&str>) -> Option<String> {
    value.and_then(|item| {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn dedupe_phase_gate_dispatch_ids(dispatch_ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for dispatch_id in dispatch_ids {
        let normalized = dispatch_id.trim();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.to_string()) {
            deduped.push(normalized.to_string());
        }
    }
    deduped
}

async fn lock_phase_gate_state_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase: i64,
) -> Result<(), String> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2::TEXT))")
        .bind(run_id)
        .bind(phase)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!("lock postgres phase-gate rows for run {run_id} phase {phase}: {error}")
        })?;
    Ok(())
}

async fn valid_phase_gate_dispatch_ids_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_ids: &[String],
) -> Result<Vec<String>, String> {
    if dispatch_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query("SELECT id FROM task_dispatches WHERE id = ANY($1)")
        .bind(dispatch_ids.to_vec())
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| format!("load postgres phase-gate dispatch ids: {error}"))?;

    let valid: std::collections::HashSet<String> = rows
        .into_iter()
        .filter_map(|row| row.try_get::<String, _>("id").ok())
        .collect();

    Ok(dispatch_ids
        .iter()
        .filter(|dispatch_id| valid.contains(dispatch_id.as_str()))
        .cloned()
        .collect())
}

async fn delete_stale_phase_gate_rows_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    phase: i64,
    dispatch_ids: &[String],
) -> Result<usize, String> {
    let rows_affected = if dispatch_ids.is_empty() {
        sqlx::query(
            "DELETE FROM auto_queue_phase_gates
             WHERE run_id = $1
               AND phase = $2
               AND dispatch_id IS NOT NULL",
        )
        .bind(run_id)
        .bind(phase)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!("delete postgres stale phase-gate rows for run {run_id} phase {phase}: {error}")
        })?
        .rows_affected()
    } else {
        sqlx::query(
            "DELETE FROM auto_queue_phase_gates
             WHERE run_id = $1
               AND phase = $2
               AND (dispatch_id IS NULL OR NOT (dispatch_id = ANY($3)))",
        )
        .bind(run_id)
        .bind(phase)
        .bind(dispatch_ids.to_vec())
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!("delete postgres stale phase-gate rows for run {run_id} phase {phase}: {error}")
        })?
        .rows_affected()
    };

    usize::try_from(rows_affected)
        .map_err(|error| format!("convert postgres phase-gate delete count for {run_id}: {error}"))
}

pub async fn save_phase_gate_state_on_pg(
    pool: &PgPool,
    run_id: &str,
    phase: i64,
    state: &PhaseGateStateWrite,
) -> Result<PhaseGateSaveResult, String> {
    let status = normalize_phase_gate_status(&state.status);
    let verdict = normalize_optional_text(state.verdict.as_deref());
    let pass_verdict = normalize_phase_gate_pass_verdict(&state.pass_verdict);
    let anchor_card_id = normalize_optional_text(state.anchor_card_id.as_deref());
    let failure_reason = normalize_optional_text(state.failure_reason.as_deref());
    let created_at = normalize_optional_text(state.created_at.as_deref());
    let deduped_dispatch_ids = dedupe_phase_gate_dispatch_ids(&state.dispatch_ids);

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres phase-gate save for run {run_id}: {error}"))?;
    lock_phase_gate_state_on_pg_tx(&mut tx, run_id, phase).await?;
    let dispatch_ids =
        valid_phase_gate_dispatch_ids_on_pg_tx(&mut tx, &deduped_dispatch_ids).await?;
    let removed_stale_rows =
        delete_stale_phase_gate_rows_on_pg_tx(&mut tx, run_id, phase, &dispatch_ids).await?;

    if dispatch_ids.is_empty() {
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, verdict, dispatch_id, pass_verdict, next_phase,
                final_phase, anchor_card_id, failure_reason, created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, NULL, $5, $6, $7, $8, $9,
                COALESCE($10::timestamptz, NOW()), NOW()
             )
             ON CONFLICT (run_id, phase, COALESCE(dispatch_id, ''))
             DO UPDATE SET
                status = EXCLUDED.status,
                verdict = EXCLUDED.verdict,
                pass_verdict = EXCLUDED.pass_verdict,
                next_phase = EXCLUDED.next_phase,
                final_phase = EXCLUDED.final_phase,
                anchor_card_id = EXCLUDED.anchor_card_id,
                failure_reason = EXCLUDED.failure_reason,
                created_at = COALESCE($10::timestamptz, auto_queue_phase_gates.created_at, NOW()),
                updated_at = NOW()",
        )
        .bind(run_id)
        .bind(phase)
        .bind(&status)
        .bind(verdict.as_deref())
        .bind(&pass_verdict)
        .bind(state.next_phase)
        .bind(state.final_phase)
        .bind(anchor_card_id.as_deref())
        .bind(failure_reason.as_deref())
        .bind(created_at.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            format!("upsert postgres phase-gate row for run {run_id} phase {phase}: {error}")
        })?;
    } else {
        for dispatch_id in &dispatch_ids {
            sqlx::query(
                "DELETE FROM auto_queue_phase_gates
                 WHERE dispatch_id = $1
                   AND NOT (run_id = $2 AND phase = $3)",
            )
            .bind(dispatch_id)
            .bind(run_id)
            .bind(phase)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!(
                    "delete existing postgres phase-gate row for dispatch {dispatch_id}: {error}"
                )
            })?;
            sqlx::query(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, verdict, dispatch_id, pass_verdict, next_phase,
                    final_phase, anchor_card_id, failure_reason, created_at, updated_at
                 ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                    COALESCE($11::timestamptz, NOW()), NOW()
                 )
                 ON CONFLICT (run_id, phase, COALESCE(dispatch_id, ''))
                 DO UPDATE SET
                    status = EXCLUDED.status,
                    verdict = EXCLUDED.verdict,
                    dispatch_id = EXCLUDED.dispatch_id,
                    pass_verdict = EXCLUDED.pass_verdict,
                    next_phase = EXCLUDED.next_phase,
                    final_phase = EXCLUDED.final_phase,
                    anchor_card_id = EXCLUDED.anchor_card_id,
                    failure_reason = EXCLUDED.failure_reason,
                    created_at = COALESCE($11::timestamptz, auto_queue_phase_gates.created_at, NOW()),
                    updated_at = NOW()",
            )
            .bind(run_id)
            .bind(phase)
            .bind(&status)
            .bind(verdict.as_deref())
            .bind(dispatch_id)
            .bind(&pass_verdict)
            .bind(state.next_phase)
            .bind(state.final_phase)
            .bind(anchor_card_id.as_deref())
            .bind(failure_reason.as_deref())
            .bind(created_at.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!(
                    "upsert postgres phase-gate row for run {run_id} phase {phase} dispatch {dispatch_id}: {error}"
                )
            })?;
        }
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres phase-gate save for run {run_id}: {error}"))?;

    Ok(PhaseGateSaveResult {
        persisted_dispatch_ids: dispatch_ids,
        removed_stale_rows,
    })
}

pub async fn clear_phase_gate_state_on_pg(
    pool: &PgPool,
    run_id: &str,
    phase: i64,
) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres phase-gate clear for run {run_id}: {error}"))?;
    lock_phase_gate_state_on_pg_tx(&mut tx, run_id, phase).await?;
    let deleted =
        sqlx::query("DELETE FROM auto_queue_phase_gates WHERE run_id = $1 AND phase = $2")
            .bind(run_id)
            .bind(phase)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("clear postgres phase-gate rows for run {run_id} phase {phase}: {error}")
            })?
            .rows_affected();
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres phase-gate clear for run {run_id}: {error}"))?;
    Ok(deleted > 0)
}

pub async fn group_has_pending_entries_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    current_phase: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(batch_phase, 0)::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
           AND COALESCE(thread_group, 0) = $2
           AND status = 'pending'
         ORDER BY priority_rank ASC",
    )
    .bind(run_id)
    .bind(thread_group)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .any(|batch_phase| batch_phase_is_eligible(batch_phase, current_phase)))
}

pub async fn first_pending_entry_for_group_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    current_phase: Option<i64>,
) -> Result<Option<(String, String, String, i64, i64)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT e.id,
                COALESCE(e.kanban_card_id, '') AS kanban_card_id,
                e.agent_id,
                COALESCE(e.batch_phase, 0)::BIGINT AS batch_phase,
                COALESCE(e.retry_count, 0)::BIGINT AS retry_count
         FROM auto_queue_entries e
         WHERE e.run_id = $1
           AND COALESCE(e.thread_group, 0) = $2
           AND e.status = 'pending'
         ORDER BY e.priority_rank ASC",
    )
    .bind(run_id)
    .bind(thread_group)
    .fetch_all(pool)
    .await?;

    for row in rows {
        let batch_phase = row.try_get::<i64, _>("batch_phase")?;
        if batch_phase_is_eligible(batch_phase, current_phase) {
            return Ok(Some((
                row.try_get("id")?,
                row.try_get("kanban_card_id")?,
                row.try_get("agent_id")?,
                batch_phase,
                row.try_get("retry_count")?,
            )));
        }
    }

    Ok(None)
}

pub async fn assigned_groups_with_pending_entries_pg(
    pool: &PgPool,
    run_id: &str,
    current_phase: Option<i64>,
) -> Result<Vec<i64>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT s.assigned_thread_group, COALESCE(e.batch_phase, 0)::BIGINT AS batch_phase
         FROM auto_queue_slots s
         JOIN auto_queue_entries e
           ON e.run_id = $1
          AND e.agent_id = s.agent_id
          AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
         WHERE s.assigned_run_id = $1
           AND s.assigned_thread_group IS NOT NULL
           AND EXISTS (
               SELECT 1
               FROM auto_queue_entries e
               WHERE e.run_id = $1
                 AND e.agent_id = s.agent_id
                 AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
                 AND e.status = 'pending'
           )
           AND NOT EXISTS (
               SELECT 1
               FROM auto_queue_entries e
               WHERE e.run_id = $1
                 AND e.agent_id = s.agent_id
                 AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
                 AND e.status = 'dispatched'
           )
         ORDER BY s.assigned_thread_group ASC, s.slot_index ASC, COALESCE(e.batch_phase, 0) ASC",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;

    let mut seen = std::collections::HashSet::new();
    let mut groups = Vec::new();
    for row in rows {
        let thread_group = row.try_get::<i64, _>("assigned_thread_group")?;
        let batch_phase = row.try_get::<i64, _>("batch_phase")?;
        if batch_phase_is_eligible(batch_phase, current_phase) && seen.insert(thread_group) {
            groups.push(thread_group);
        }
    }
    Ok(groups)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotAllocation {
    pub slot_index: i64,
    pub newly_assigned: bool,
    pub reassigned_from_other_group: bool,
}

async fn bind_slot_index_for_group_entries_pg(
    pool: &PgPool,
    run_id: &str,
    agent_id: &str,
    thread_group: i64,
    slot_index: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE auto_queue_entries
         SET slot_index = $1
         WHERE run_id = $2
           AND agent_id = $3
           AND COALESCE(thread_group, 0) = $4
           AND status IN ('pending', 'dispatched')
           AND (slot_index IS NULL OR slot_index != $1)",
    )
    .bind(slot_index)
    .bind(run_id)
    .bind(agent_id)
    .bind(thread_group)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn slot_has_recent_terminal_auto_queue_dispatch_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1
             FROM task_dispatches d
             WHERE d.to_agent_id = $1
               AND d.status IN ('completed', 'failed', 'cancelled')
               AND COALESCE(NULLIF((COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->>'slot_index', '')::BIGINT, -1) = $2
               AND COALESCE(((COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->>'auto_queue')::BOOLEAN, FALSE) = TRUE
               AND COALESCE(((COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
               AND (COALESCE(NULLIF(d.context, ''), '{}')::jsonb)->'phase_gate' IS NULL
               AND COALESCE(d.completed_at, d.updated_at, d.created_at)
                   >= NOW() - make_interval(secs => $3::INT)
         )",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(SLOT_TERMINAL_DISPATCH_COOLDOWN_SECONDS)
    .fetch_one(pool)
    .await
}

fn active_dispatch_slot_guard_sql(agent_expr: &str, slot_expr: &str) -> String {
    format!(
        "NOT EXISTS (
             SELECT 1
             FROM task_dispatches d
             WHERE d.to_agent_id = {agent_expr}
               AND d.status IN ('pending', 'dispatched')
               AND COALESCE(NULLIF((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'slot_index', '')::BIGINT, -1) = {slot_expr}
               AND COALESCE(((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
               AND (COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->'phase_gate' IS NULL
         )"
    )
}

fn active_dispatch_slot_exists_sql(agent_expr: &str, slot_expr: &str) -> String {
    format!(
        "EXISTS (
             SELECT 1
             FROM task_dispatches d
             WHERE d.to_agent_id = {agent_expr}
               AND d.status IN ('pending', 'dispatched')
               AND COALESCE(NULLIF((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'slot_index', '')::BIGINT, -1) = {slot_expr}
               AND COALESCE(((COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
               AND (COALESCE(NULLIF(d.context, ''), '{{}}')::jsonb)->'phase_gate' IS NULL
         )"
    )
}

async fn first_free_slot_blocked_by_active_dispatch_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let active_dispatch_exists =
        active_dispatch_slot_exists_sql("auto_queue_slots.agent_id", "auto_queue_slots.slot_index");
    let query = format!(
        "SELECT slot_index::BIGINT
         FROM auto_queue_slots
         WHERE agent_id = $1
           AND assigned_run_id IS NULL
           AND {active_dispatch_exists}
         ORDER BY slot_index ASC
         LIMIT 1"
    );

    sqlx::query_scalar::<_, i64>(&query)
        .bind(agent_id)
        .fetch_optional(pool)
        .await
}

pub async fn allocate_slot_for_group_agent_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
) -> Result<Option<SlotAllocation>, String> {
    let slot_pool_size = run_slot_pool_size_pg(pool, run_id)
        .await
        .map_err(|error| format!("load postgres slot pool size for {run_id}: {error}"))?;
    ensure_agent_slot_pool_rows_pg(pool, agent_id, slot_pool_size)
        .await
        .map_err(|error| {
            format!("prepare postgres slot rows for run {run_id} agent {agent_id}: {error}")
        })?;

    for attempt in 1..=SLOT_ALLOCATION_MAX_RETRIES {
        let existing = sqlx::query_scalar::<_, i64>(
            "SELECT slot_index::BIGINT
             FROM auto_queue_slots
             WHERE agent_id = $1
               AND assigned_run_id = $2
               AND COALESCE(assigned_thread_group, 0) = $3
             LIMIT 1",
        )
        .bind(agent_id)
        .bind(run_id)
        .bind(thread_group)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!(
                "inspect existing postgres slot for run {run_id} agent {agent_id} group {thread_group}: {error}"
            )
        })?;
        if let Some(slot_index) = existing {
            let slot_busy = slot_has_active_dispatch_pg(pool, agent_id, slot_index)
                .await
                .map_err(|error| {
                    format!(
                        "inspect existing postgres slot {slot_index} active dispatch for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    )
                })?;
            if slot_busy {
                return Ok(None);
            }

            bind_slot_index_for_group_entries_pg(pool, run_id, agent_id, thread_group, slot_index)
                .await
                .map_err(|error| {
                    format!(
                        "bind existing postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    )
                })?;
            return Ok(Some(SlotAllocation {
                slot_index,
                newly_assigned: false,
                reassigned_from_other_group: false,
            }));
        }

        let reusable_slot_guard = active_dispatch_slot_guard_sql("s.agent_id", "s.slot_index");
        let reusable_slot_query = format!(
            "SELECT s.slot_index::BIGINT
             FROM auto_queue_slots s
             WHERE s.agent_id = $1
               AND s.assigned_run_id = $2
               AND COALESCE(s.assigned_thread_group, -1) != $3
               AND NOT EXISTS (
                   SELECT 1
                   FROM auto_queue_entries e
                   WHERE e.run_id = $2
                     AND e.agent_id = s.agent_id
                     AND COALESCE(e.thread_group, 0) = COALESCE(s.assigned_thread_group, 0)
                     AND e.status IN ('pending', 'dispatched')
               )
               AND {reusable_slot_guard}
             ORDER BY s.slot_index ASC
             LIMIT 1"
        );
        let reusable_slot = sqlx::query_scalar::<_, i64>(&reusable_slot_query)
        .bind(agent_id)
        .bind(run_id)
        .bind(thread_group)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!(
                "inspect reusable postgres slot for run {run_id} agent {agent_id} group {thread_group}: {error}"
            )
        })?;
        if let Some(slot_index) = reusable_slot {
            let rebound_slot_guard = active_dispatch_slot_guard_sql(
                "auto_queue_slots.agent_id",
                "auto_queue_slots.slot_index",
            );
            let rebound_query = format!(
                "UPDATE auto_queue_slots
                 SET assigned_thread_group = $1,
                     updated_at = NOW()
                 WHERE agent_id = $2
                   AND slot_index = $3
                   AND assigned_run_id = $4
                   AND COALESCE(assigned_thread_group, -1) != $1
                   AND NOT EXISTS (
                       SELECT 1
                       FROM auto_queue_entries e
                       WHERE e.run_id = $4
                         AND e.agent_id = auto_queue_slots.agent_id
                         AND COALESCE(e.thread_group, 0) = COALESCE(auto_queue_slots.assigned_thread_group, 0)
                         AND e.status IN ('pending', 'dispatched')
                   )
                   AND {rebound_slot_guard}"
            );
            let rebound = sqlx::query(&rebound_query)
            .bind(thread_group)
            .bind(agent_id)
            .bind(slot_index)
            .bind(run_id)
            .execute(pool)
            .await
            .map_err(|error| {
                format!(
                    "rebind postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                )
            })?
            .rows_affected();
            if rebound == 0 {
                if attempt == SLOT_ALLOCATION_MAX_RETRIES {
                    return Err(format!(
                        "slot allocation retry limit exceeded for run {run_id} agent {agent_id} group {thread_group} after {attempt} attempts"
                    ));
                }
                continue;
            }

            bind_slot_index_for_group_entries_pg(pool, run_id, agent_id, thread_group, slot_index)
                .await
                .map_err(|error| {
                    format!(
                        "bind rebound postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                    )
                })?;
            return Ok(Some(SlotAllocation {
                slot_index,
                newly_assigned: false,
                reassigned_from_other_group: true,
            }));
        }

        let free_slot_guard = active_dispatch_slot_guard_sql(
            "auto_queue_slots.agent_id",
            "auto_queue_slots.slot_index",
        );
        let free_slot_query = format!(
            "SELECT slot_index::BIGINT
             FROM auto_queue_slots
             WHERE agent_id = $1
               AND assigned_run_id IS NULL
               AND {free_slot_guard}
             ORDER BY slot_index ASC
             LIMIT 1"
        );
        let free_slot = sqlx::query_scalar::<_, i64>(&free_slot_query)
        .bind(agent_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!(
                "inspect free postgres slot for run {run_id} agent {agent_id} group {thread_group}: {error}"
            )
        })?;
        let Some(slot_index) = free_slot else {
            match first_free_slot_blocked_by_active_dispatch_pg(pool, agent_id).await {
                Ok(Some(blocked_slot_index)) => tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    slot_index = blocked_slot_index,
                    "[auto-queue] free-slot fallback refused slot with active dispatch"
                ),
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    error = %error,
                    "[auto-queue] failed to inspect active-dispatch-blocked free slots"
                ),
            }
            return Ok(None);
        };

        let claim_slot_guard = active_dispatch_slot_guard_sql(
            "auto_queue_slots.agent_id",
            "auto_queue_slots.slot_index",
        );
        let claim_query = format!(
            "UPDATE auto_queue_slots
             SET assigned_run_id = $1,
                 assigned_thread_group = $2,
                 updated_at = NOW()
             WHERE agent_id = $3
               AND slot_index = $4
               AND assigned_run_id IS NULL
               AND {claim_slot_guard}"
        );
        let claimed = sqlx::query(&claim_query)
        .bind(run_id)
        .bind(thread_group)
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await
        .map_err(|error| {
            format!(
                "claim postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
            )
        })?
        .rows_affected();
        if claimed == 0 {
            match slot_has_active_dispatch_pg(pool, agent_id, slot_index).await {
                Ok(true) => tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    slot_index,
                    "[auto-queue] free-slot claim refused slot with active dispatch"
                ),
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    run_id,
                    agent_id,
                    thread_group,
                    slot_index,
                    error = %error,
                    "[auto-queue] failed to inspect active dispatch after free-slot claim refusal"
                ),
            }
            if attempt == SLOT_ALLOCATION_MAX_RETRIES {
                return Err(format!(
                    "slot allocation retry limit exceeded for run {run_id} agent {agent_id} group {thread_group} after {attempt} attempts"
                ));
            }
            continue;
        }

        bind_slot_index_for_group_entries_pg(pool, run_id, agent_id, thread_group, slot_index)
            .await
            .map_err(|error| {
                format!(
                    "bind claimed postgres slot {slot_index} for run {run_id} agent {agent_id} group {thread_group}: {error}"
                )
            })?;
        return Ok(Some(SlotAllocation {
            slot_index,
            newly_assigned: true,
            reassigned_from_other_group: false,
        }));
    }

    unreachable!("slot allocation loop must return within bounded retries");
}

pub async fn release_slot_for_group_agent_pg(
    pool: &PgPool,
    run_id: &str,
    thread_group: i64,
    agent_id: &str,
    slot_index: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE agent_id = $1
           AND slot_index = $2
           AND assigned_run_id = $3
           AND COALESCE(assigned_thread_group, 0) = $4",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(run_id)
    .bind(thread_group)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn slot_has_active_dispatch_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<bool, sqlx::Error> {
    slot_has_active_dispatch_excluding_pg(pool, agent_id, slot_index, None).await
}

pub async fn slot_has_active_dispatch_excluding_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    exclude_dispatch_id: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let exclude_id = exclude_dispatch_id.unwrap_or("");
    let auto_queue_active = sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM auto_queue_entries
         WHERE agent_id = $1
           AND slot_index = $2
           AND status = 'dispatched'
           AND COALESCE(dispatch_id, '') != $3",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(exclude_id)
    .fetch_one(pool)
    .await?;
    if auto_queue_active {
        return Ok(true);
    }

    sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM task_dispatches
         WHERE to_agent_id = $1
           AND status IN ('pending', 'dispatched')
           AND COALESCE(NULLIF((COALESCE(NULLIF(context, ''), '{}')::jsonb)->>'slot_index', '')::BIGINT, -1) = $2
           AND COALESCE(((COALESCE(NULLIF(context, ''), '{}')::jsonb)->>'sidecar_dispatch')::BOOLEAN, FALSE) = FALSE
           AND (COALESCE(NULLIF(context, ''), '{}')::jsonb)->'phase_gate' IS NULL
           AND id != $3",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(exclude_id)
    .fetch_one(pool)
    .await
}

#[allow(dead_code)]
pub async fn sync_run_group_metadata_pg(pool: &PgPool, run_id: &str) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres sync run group metadata {run_id}: {error}"))?;
    sync_run_group_metadata_pg_tx(&mut tx, run_id).await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres sync run group metadata {run_id}: {error}"))?;
    Ok(())
}

#[allow(dead_code)]
pub async fn sync_run_group_metadata_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let thread_group_count = sqlx::query_scalar::<_, i64>(
        "SELECT GREATEST(
                COALESCE(COUNT(DISTINCT COALESCE(thread_group, 0)), 0),
                1
            )::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count postgres thread groups for run {run_id}: {error}"))?;

    sqlx::query(
        "UPDATE auto_queue_runs
         SET thread_group_count = $1,
             max_concurrent_threads = $1
         WHERE id = $2",
    )
    .bind(thread_group_count)
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("update postgres run group metadata for {run_id}: {error}"))?;

    Ok(())
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

async fn maybe_finalize_run_after_terminal_entry_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    new_status: &str,
) -> Result<bool, String> {
    if new_status == ENTRY_STATUS_DONE {
        return Ok(false);
    }
    // #815 P1: never finalize on `user_cancelled` — it must leave the run in a
    // resumable state so the operator can flip the entry back to `pending`.
    if new_status == ENTRY_STATUS_USER_CANCELLED {
        return Ok(false);
    }

    maybe_finalize_run_if_ready_pg(tx, run_id).await
}

pub(crate) async fn maybe_finalize_run_if_ready_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    let blocking_phase_gate_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_phase_gates
         WHERE run_id = $1
           AND status IN ('pending', 'failed')",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count blocking phase gates for run {run_id}: {error}"))?;
    if blocking_phase_gate_count > 0 {
        return Ok(false);
    }

    let remaining = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count remaining auto-queue entries for run {run_id}: {error}"))?;
    if remaining > 0 {
        return Ok(false);
    }

    sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id = $1",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("release auto-queue slots for run {run_id}: {error}"))?;

    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = NOW()
         WHERE id = $1
           AND status IN ('active', 'paused', 'generated', 'pending')",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("complete auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        return Ok(false);
    }

    queue_run_completion_notify_on_pg(tx, run_id).await?;
    Ok(true)
}

async fn auto_queue_run_review_disabled_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    let review_mode = sqlx::query_scalar::<_, Option<String>>(
        "SELECT review_mode FROM auto_queue_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load auto-queue review mode for run {run_id}: {error}"))?
    .flatten();

    Ok(review_mode.as_deref().unwrap_or("enabled") == "disabled")
}

pub async fn pause_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres pause auto-queue run {run_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'paused',
             completed_at = NULL
         WHERE id = $1
           AND status = 'active'",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("pause postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated > 0 {
        sqlx::query(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL,
                 updated_at = NOW()
             WHERE assigned_run_id = $1",
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            format!("release postgres auto-queue slots for paused run {run_id}: {error}")
        })?;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres pause auto-queue run {run_id}: {error}"))?;
    Ok(updated > 0)
}

pub async fn resume_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE id = $1
           AND status = 'paused'",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .map_err(|error| format!("resume postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    Ok(updated > 0)
}

pub async fn complete_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres complete auto-queue run {run_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = NOW()
         WHERE id = $1
           AND status IN ('active', 'paused', 'generated', 'pending')",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("complete postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        tx.rollback().await.map_err(|error| {
            format!("rollback stale postgres complete auto-queue run {run_id}: {error}")
        })?;
        return Ok(false);
    }

    queue_run_completion_notify_on_pg(&mut tx, run_id).await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres complete auto-queue run {run_id}: {error}"))?;
    Ok(true)
}

async fn queue_run_completion_notify_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let row = sqlx::query("SELECT repo, agent_id FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| format!("load completion notify targets for run {run_id}: {error}"))?;
    let repo: Option<String> = row
        .try_get("repo")
        .map_err(|error| format!("decode completion notify repo for run {run_id}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode completion notify agent_id for run {run_id}: {error}"))?;
    let targets = completion_notify_targets_on_pg(tx, run_id, agent_id.as_deref()).await?;
    if targets.is_empty() {
        return Ok(());
    }

    let entry_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| format!("count auto-queue entries for run {run_id}: {error}"))?;
    let repo_label = repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("(global)");
    let short_run_id = &run_id[..8.min(run_id.len())];
    let content = format!("자동큐 완료: {repo_label} / run {short_run_id} / {entry_count}개");

    for channel_id in targets {
        sqlx::query(
            "INSERT INTO message_outbox (target, content, bot, source)
             VALUES ($1, $2, 'notify', 'system')",
        )
        .bind(format!("channel:{channel_id}"))
        .bind(&content)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            format!(
                "queue auto-queue completion notify for run {run_id} channel {channel_id}: {error}"
            )
        })?;
    }

    Ok(())
}

async fn completion_notify_targets_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    run_agent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut targets = Vec::new();

    if let Some(agent_id) = run_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let channel_id = sqlx::query("SELECT discord_channel_id FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                format!("load completion notify agent channel for run {run_id}: {error}")
            })?
            .map(|row| {
                row.try_get::<Option<String>, _>("discord_channel_id")
                    .map_err(|error| {
                        format!("decode completion notify agent channel for run {run_id}: {error}")
                    })
            })
            .transpose()?
            .flatten();
        if let Some(channel_id) = channel_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            targets.push(channel_id);
        }
    }

    if targets.is_empty() {
        let rows = sqlx::query(
            "SELECT DISTINCT a.discord_channel_id
             FROM auto_queue_entries e
             JOIN agents a ON a.id = e.agent_id
             WHERE e.run_id = $1
               AND a.discord_channel_id IS NOT NULL
               AND TRIM(a.discord_channel_id) != ''",
        )
        .bind(run_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| {
            format!("load completion notify fallback channels for run {run_id}: {error}")
        })?;
        for row in rows {
            let channel_id: String = row.try_get("discord_channel_id").map_err(|error| {
                format!("decode completion notify fallback channel for run {run_id}: {error}")
            })?;
            targets.push(channel_id);
        }
    }

    targets.sort();
    targets.dedup();
    Ok(targets)
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

fn auto_queue_run_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AutoQueueRunRecord, sqlx::Error> {
    Ok(AutoQueueRunRecord {
        id: row.try_get("id")?,
        repo: row.try_get("repo")?,
        agent_id: row.try_get("agent_id")?,
        review_mode: row.try_get("review_mode")?,
        status: row.try_get("status")?,
        timeout_minutes: row.try_get("timeout_minutes")?,
        ai_model: row.try_get("ai_model")?,
        ai_rationale: row.try_get("ai_rationale")?,
        created_at: row.try_get("created_at")?,
        completed_at: row.try_get("completed_at")?,
        max_concurrent_threads: row.try_get("max_concurrent_threads")?,
        thread_group_count: row.try_get("thread_group_count")?,
    })
}

fn status_entry_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<StatusEntryRecord, sqlx::Error> {
    Ok(StatusEntryRecord {
        id: row.try_get("id")?,
        agent_id: row.try_get("agent_id")?,
        card_id: row.try_get("kanban_card_id")?,
        dispatch_id: row.try_get("dispatch_id")?,
        priority_rank: row.try_get("priority_rank")?,
        reason: row.try_get("reason")?,
        status: row.try_get("status")?,
        retry_count: row.try_get("retry_count")?,
        created_at: row.try_get("created_at")?,
        dispatched_at: row.try_get("dispatched_at")?,
        completed_at: row.try_get("completed_at")?,
        card_title: row.try_get("title")?,
        github_issue_number: row.try_get("github_issue_number")?,
        github_repo: row.try_get("github_repo")?,
        thread_group: row.try_get("thread_group")?,
        slot_index: row.try_get("slot_index")?,
        batch_phase: row.try_get("batch_phase")?,
        channel_thread_map: row.try_get("channel_thread_map")?,
        active_thread_id: row.try_get("active_thread_id")?,
        card_status: row.try_get("card_status")?,
        review_round: row.try_get("review_round")?,
    })
}

fn auto_queue_run_history_record_from_pg_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AutoQueueRunHistoryRecord, sqlx::Error> {
    Ok(AutoQueueRunHistoryRecord {
        id: row.try_get("id")?,
        repo: row.try_get("repo")?,
        agent_id: row.try_get("agent_id")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        completed_at: row.try_get("completed_at")?,
        entry_count: row.try_get("entry_count")?,
        done_count: row.try_get("done_count")?,
        skipped_count: row.try_get("skipped_count")?,
        pending_count: row.try_get("pending_count")?,
        dispatched_count: row.try_get("dispatched_count")?,
    })
}

#[cfg(test)]
mod resume_session_context_tests {
    use super::resume_session_id_from_context;

    #[test]
    fn resume_session_id_from_context_prefers_retry_field_and_trims() {
        assert_eq!(
            resume_session_id_from_context(Some(
                r#"{"auto_queue_retry_resume_session_id":" thread-1585 ","resume_session_id":"old"}"#,
            ))
            .as_deref(),
            Some("thread-1585")
        );
        assert_eq!(
            resume_session_id_from_context(Some(r#"{"resume_session_id":" fallback-thread "}"#))
                .as_deref(),
            Some("fallback-thread")
        );
        assert_eq!(
            resume_session_id_from_context(Some(r#"{"auto_queue_retry_resume_session_id":"   "}"#)),
            None
        );
    }
}

#[cfg(test)]
mod dispatch_terminal_sync_pg_tests {
    use super::{
        ENTRY_STATUS_DONE, ENTRY_STATUS_SKIPPED, ENTRY_STATUS_USER_CANCELLED,
        EntryStatusUpdateOptions, PhaseGateStateWrite, SlotAllocation,
        allocate_slot_for_group_agent_pg, clear_phase_gate_state_on_pg,
        finalize_completed_dispatch_terminal_entry_on_pg_tx, save_phase_gate_state_on_pg,
        slot_has_recent_terminal_auto_queue_dispatch_pg, sync_dispatch_terminal_entries_on_pg_tx,
        update_entry_status_on_pg,
    };
    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
        cleanup_armed: bool,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_dispatch_terminal_sync_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dispatch terminal sync pg tests",
            )
            .await
            .expect("create dispatch terminal sync test db");

            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
                cleanup_armed: true,
            }
        }

        async fn connect_and_migrate(&self) -> PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "dispatch terminal sync pg tests",
            )
            .await
            .expect("connect + migrate dispatch terminal sync test db")
        }

        async fn drop(mut self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "dispatch terminal sync pg tests",
            )
            .await
            .expect("drop dispatch terminal sync test db");
            self.cleanup_armed = false;
        }
    }

    impl Drop for TestPostgresDb {
        fn drop(&mut self) {
            if !self.cleanup_armed {
                return;
            }
            let admin_url = self.admin_url.clone();
            let database_name = self.database_name.clone();
            let _ = std::thread::Builder::new()
                .name(format!("dispatch terminal sync pg cleanup {database_name}"))
                .spawn(move || {
                    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return;
                    };
                    let _ = runtime.block_on(crate::db::postgres::drop_test_database(
                        &admin_url,
                        &database_name,
                        "dispatch terminal sync pg tests cleanup",
                    ));
                })
                .map(|handle| handle.join());
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| std::env::var("USER").ok())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());
        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    async fn setup_pool(pg_db: &TestPostgresDb) -> PgPool {
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-1', 'repo-1', 'agent-1', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-1', 'Agent 1', 'claude', '123')",
        )
        .execute(&pool)
        .await
        .expect("seed agent");
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-1', 0, 'run-1', 0, CAST('{}' AS jsonb))",
        )
        .execute(&pool)
        .await
        .expect("seed slot");
        pool
    }

    async fn entry_row_status_dispatch_completed(
        pool: &PgPool,
        entry_id: &str,
    ) -> (String, Option<String>, Option<DateTime<Utc>>) {
        let row = sqlx::query(
            "SELECT status, dispatch_id, completed_at
             FROM auto_queue_entries
             WHERE id = $1",
        )
        .bind(entry_id)
        .fetch_one(pool)
        .await
        .expect("entry row");
        (
            row.try_get::<String, _>("status").expect("status"),
            row.try_get::<Option<String>, _>("dispatch_id")
                .expect("dispatch_id"),
            row.try_get::<Option<DateTime<Utc>>, _>("completed_at")
                .expect("completed_at"),
        )
    }

    async fn run_status(pool: &PgPool, run_id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("run row")
    }

    async fn slot_run(pool: &PgPool, agent_id: &str, slot_index: i64) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT assigned_run_id
             FROM auto_queue_slots
             WHERE agent_id = $1 AND slot_index = $2",
        )
        .bind(agent_id)
        .bind(slot_index)
        .fetch_one(pool)
        .await
        .expect("slot row")
    }

    async fn count_transitions(pool: &PgPool, entry_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_entry_transitions
             WHERE entry_id = $1",
        )
        .bind(entry_id)
        .fetch_one(pool)
        .await
        .expect("transition count")
    }

    async fn count_message_outbox(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM message_outbox")
            .fetch_one(pool)
            .await
            .expect("message outbox count")
    }

    async fn seed_phase_gate_dispatches(pool: &PgPool, dispatch_ids: &[&str]) {
        for dispatch_id in dispatch_ids {
            sqlx::query(
                "INSERT INTO task_dispatches (id, to_agent_id, status, context)
                 VALUES ($1, 'agent-1', 'dispatched', '{}')",
            )
            .bind(dispatch_id)
            .execute(pool)
            .await
            .expect("seed phase gate dispatch");
        }
    }

    async fn phase_gate_row_ids(pool: &PgPool, phase: i64) -> Vec<i64> {
        sqlx::query(
            "SELECT id
             FROM auto_queue_phase_gates
             WHERE run_id = 'run-1' AND phase = $1
             ORDER BY COALESCE(dispatch_id, '') ASC",
        )
        .bind(phase)
        .fetch_all(pool)
        .await
        .expect("phase gate row ids")
        .into_iter()
        .map(|row| row.try_get::<i64, _>("id").expect("phase gate id"))
        .collect()
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_skips_reusable_slot_with_active_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET max_concurrent_threads = 2 WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("expand slot pool");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, slot_index, thread_group,
                 batch_phase, completed_at)
             VALUES ('entry-complete', 'run-1', NULL, 'agent-1', 'done', 0, 0, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed completed slot entry");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-review-slot-0', 'agent-1', 'dispatched', $1)",
        )
        .bind(serde_json::json!({"slot_index": 0}).to_string())
        .execute(&pool)
        .await
        .expect("seed active dispatch in reusable slot");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-next', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed next phase entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 1, "agent-1")
            .await
            .expect("allocation must succeed via a different free slot");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 1,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );

        let slot_index: Option<i64> =
            sqlx::query_scalar("SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-next'")
                .fetch_one(&pool)
                .await
                .expect("next entry slot");
        assert_eq!(slot_index, Some(1));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn phase_gate_state_save_is_idempotent_for_dispatch_rows_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        seed_phase_gate_dispatches(&pool, &["dispatch-gate-1", "dispatch-gate-2"]).await;
        let state = PhaseGateStateWrite {
            status: "pending".to_string(),
            verdict: None,
            dispatch_ids: vec!["dispatch-gate-1".to_string(), "dispatch-gate-2".to_string()],
            pass_verdict: "phase_gate_passed".to_string(),
            next_phase: Some(6),
            final_phase: false,
            anchor_card_id: None,
            failure_reason: None,
            created_at: Some("2026-05-05 00:00:00+00".to_string()),
        };

        let first = save_phase_gate_state_on_pg(&pool, "run-1", 5, &state)
            .await
            .expect("first save phase gate state");
        let first_row_ids = phase_gate_row_ids(&pool, 5).await;
        let second = save_phase_gate_state_on_pg(&pool, "run-1", 5, &state)
            .await
            .expect("second save phase gate state");
        let second_row_ids = phase_gate_row_ids(&pool, 5).await;

        assert_eq!(
            first.persisted_dispatch_ids,
            vec!["dispatch-gate-1".to_string(), "dispatch-gate-2".to_string()]
        );
        assert_eq!(first.removed_stale_rows, 0);
        assert_eq!(second, first);
        assert_eq!(first_row_ids.len(), 2);
        assert_eq!(second_row_ids, first_row_ids);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn phase_gate_state_save_is_idempotent_for_empty_dispatch_set_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        let state = PhaseGateStateWrite {
            status: "pending".to_string(),
            verdict: None,
            dispatch_ids: Vec::new(),
            pass_verdict: "phase_gate_passed".to_string(),
            next_phase: None,
            final_phase: true,
            anchor_card_id: None,
            failure_reason: None,
            created_at: Some("2026-05-05 00:00:00+00".to_string()),
        };

        let first = save_phase_gate_state_on_pg(&pool, "run-1", 6, &state)
            .await
            .expect("first save empty phase gate state");
        let first_row_ids = phase_gate_row_ids(&pool, 6).await;
        let second = save_phase_gate_state_on_pg(&pool, "run-1", 6, &state)
            .await
            .expect("second save empty phase gate state");
        let second_row_ids = phase_gate_row_ids(&pool, 6).await;

        assert_eq!(first.persisted_dispatch_ids, Vec::<String>::new());
        assert_eq!(first.removed_stale_rows, 0);
        assert_eq!(second, first);
        assert_eq!(first_row_ids.len(), 1);
        assert_eq!(second_row_ids, first_row_ids);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn phase_gate_state_save_rolls_back_stale_cleanup_when_write_fails_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        seed_phase_gate_dispatches(&pool, &["dispatch-valid", "dispatch-stale"]).await;
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, dispatch_id, pass_verdict)
             VALUES ('run-1', 7, 'pending', 'dispatch-stale', 'phase_gate_passed')",
        )
        .execute(&pool)
        .await
        .expect("seed stale phase gate row");

        let error = save_phase_gate_state_on_pg(
            &pool,
            "run-1",
            7,
            &PhaseGateStateWrite {
                status: "pending".to_string(),
                verdict: None,
                dispatch_ids: vec!["dispatch-valid".to_string()],
                pass_verdict: "phase_gate_passed".to_string(),
                next_phase: None,
                final_phase: false,
                anchor_card_id: None,
                failure_reason: None,
                created_at: Some("not-a-timestamp".to_string()),
            },
        )
        .await
        .expect_err("invalid timestamp must fail the write");
        assert!(
            error.contains("upsert postgres phase-gate row"),
            "unexpected error: {error}"
        );

        let stale_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = 'run-1' AND phase = 7 AND dispatch_id = 'dispatch-stale'",
        )
        .fetch_one(&pool)
        .await
        .expect("stale count");
        let valid_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = 'run-1' AND phase = 7 AND dispatch_id = 'dispatch-valid'",
        )
        .fetch_one(&pool)
        .await
        .expect("valid count");
        assert_eq!(stale_count, 1);
        assert_eq!(valid_count, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn phase_gate_state_concurrent_clear_waits_for_atomic_save_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        seed_phase_gate_dispatches(&pool, &["dispatch-slow-1", "dispatch-slow-2"]).await;
        sqlx::query(
            r#"
            CREATE OR REPLACE FUNCTION slow_phase_gate_insert_for_test()
            RETURNS trigger AS $$
            BEGIN
                PERFORM pg_sleep(0.08);
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .execute(&pool)
        .await
        .expect("install slow phase gate insert function");
        sqlx::query(
            "CREATE TRIGGER slow_phase_gate_insert_for_test
             BEFORE INSERT ON auto_queue_phase_gates
             FOR EACH ROW EXECUTE FUNCTION slow_phase_gate_insert_for_test()",
        )
        .execute(&pool)
        .await
        .expect("install slow phase gate insert trigger");

        let pool_for_save = pool.clone();
        let save_state = PhaseGateStateWrite {
            status: "pending".to_string(),
            verdict: None,
            dispatch_ids: vec!["dispatch-slow-1".to_string(), "dispatch-slow-2".to_string()],
            pass_verdict: "phase_gate_passed".to_string(),
            next_phase: Some(10),
            final_phase: false,
            anchor_card_id: None,
            failure_reason: None,
            created_at: Some("2026-05-05 00:00:00+00".to_string()),
        };
        let save_task = tokio::spawn(async move {
            save_phase_gate_state_on_pg(&pool_for_save, "run-1", 9, &save_state).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let pool_for_clear = pool.clone();
        let clear_task =
            tokio::spawn(
                async move { clear_phase_gate_state_on_pg(&pool_for_clear, "run-1", 9).await },
            );

        let save_result = save_task
            .await
            .expect("save task join")
            .expect("save phase gate state");
        let cleared = clear_task
            .await
            .expect("clear task join")
            .expect("clear phase gate state");
        assert_eq!(
            save_result.persisted_dispatch_ids,
            vec!["dispatch-slow-1".to_string(), "dispatch-slow-2".to_string()]
        );
        assert!(cleared);
        let remaining = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = 'run-1' AND phase = 9",
        )
        .fetch_one(&pool)
        .await
        .expect("remaining phase gate count");
        assert_eq!(remaining, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn slot_has_recent_terminal_auto_queue_dispatch_pg_respects_cooldown() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context, created_at, updated_at, completed_at)
             VALUES ('dispatch-recent-terminal-slot-0', 'agent-1', 'completed', $1, NOW(), NOW(), NOW())",
        )
        .bind(
            serde_json::json!({
                "auto_queue": true,
                "entry_id": "entry-recent-terminal-slot-0",
                "thread_group": 0,
                "slot_index": 0
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed recent terminal dispatch");

        assert!(
            slot_has_recent_terminal_auto_queue_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("recent terminal cooldown probe"),
            "recent same-slot terminal auto-queue dispatch must trigger cooldown"
        );
        assert!(
            !slot_has_recent_terminal_auto_queue_dispatch_pg(&pool, "agent-1", 1)
                .await
                .expect("other slot cooldown probe"),
            "dispatches in other slots must not trigger cooldown"
        );

        sqlx::query(
            "UPDATE task_dispatches
             SET completed_at = NOW() - INTERVAL '2 minutes',
                 updated_at = NOW() - INTERVAL '2 minutes'
             WHERE id = 'dispatch-recent-terminal-slot-0'",
        )
        .execute(&pool)
        .await
        .expect("age terminal dispatch");
        assert!(
            !slot_has_recent_terminal_auto_queue_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("aged terminal cooldown probe"),
            "aged terminal dispatches should be eligible for the next tick"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_does_not_reuse_busy_existing_group_slot() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group,
                 batch_phase)
             VALUES ('entry-active', 'run-1', NULL, 'agent-1', 'dispatched', 'dispatch-active', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed active same-group entry");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-active', 'agent-1', 'pending', $1)",
        )
        .bind(
            serde_json::json!({
                "auto_queue": true,
                "entry_id": "entry-active",
                "slot_index": 0,
                "thread_group": 0
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed pending same-slot dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-next', 'run-1', NULL, 'agent-1', 'pending', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed next same-group entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 0, "agent-1")
            .await
            .expect("busy same-group slot probe must succeed");
        assert_eq!(
            allocation, None,
            "a group must not receive its existing slot while that slot has a pending dispatch"
        );

        let next_slot: Option<i64> =
            sqlx::query_scalar("SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-next'")
                .fetch_one(&pool)
                .await
                .expect("next entry slot");
        assert_eq!(next_slot, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_terminal_sync_marks_entry_done_without_finalizing_active_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-sync-done', 'Card Sync Done', 'in_progress', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-sync-done', 'card-sync-done', 'agent-1',
                     'implementation', 'completed', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-sync-done', 'run-1', 'card-sync-done', 'agent-1',
                     'dispatched', 'dispatch-sync-done', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let mut tx = pool.begin().await.expect("begin tx");
        let changed = sync_dispatch_terminal_entries_on_pg_tx(
            &mut tx,
            "dispatch-sync-done",
            ENTRY_STATUS_DONE,
            "test_runtime_finalizer",
            true,
        )
        .await
        .expect("sync dispatch terminal");
        tx.commit().await.expect("commit tx");

        assert_eq!(changed, 1);
        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-sync-done").await;
        assert_eq!(status, ENTRY_STATUS_DONE);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-sync-done"));
        assert!(completed_at.is_some());
        assert_eq!(run_status(&pool, "run-1").await, "active");
        assert_eq!(
            slot_run(&pool, "agent-1", 0).await.as_deref(),
            Some("run-1")
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn completed_dispatch_terminal_finalizer_completes_review_disabled_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET review_mode = 'disabled' WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("disable review mode");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-finalizer-done', 'Card Finalizer Done', 'in_progress', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-finalizer-done', 'card-finalizer-done', 'agent-1',
                     'implementation', 'completed', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-finalizer-done', 'run-1', 'card-finalizer-done', 'agent-1',
                     'dispatched', 'dispatch-finalizer-done', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let mut tx = pool.begin().await.expect("begin tx");
        let result = finalize_completed_dispatch_terminal_entry_on_pg_tx(
            &mut tx,
            "dispatch-finalizer-done",
            "watcher_streaming_final",
            true,
        )
        .await
        .expect("finalize completed dispatch entry");
        tx.commit().await.expect("commit tx");

        assert_eq!(result.changed_entries, 1);
        assert_eq!(result.affected_run_ids, vec!["run-1".to_string()]);
        assert_eq!(result.finalized_run_ids, vec!["run-1".to_string()]);
        assert_eq!(
            entry_row_status_dispatch_completed(&pool, "entry-finalizer-done")
                .await
                .0,
            ENTRY_STATUS_DONE
        );
        assert_eq!(run_status(&pool, "run-1").await, "completed");
        assert_eq!(slot_run(&pool, "agent-1", 0).await, None);
        assert_eq!(count_transitions(&pool, "entry-finalizer-done").await, 1);
        assert_eq!(count_message_outbox(&pool).await, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn completed_dispatch_terminal_finalizer_is_idempotent_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET review_mode = 'disabled' WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("disable review mode");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-finalizer-repeat', 'Card Finalizer Repeat', 'in_progress', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-finalizer-repeat', 'card-finalizer-repeat', 'agent-1',
                     'implementation', 'completed', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-finalizer-repeat', 'run-1', 'card-finalizer-repeat', 'agent-1',
                     'dispatched', 'dispatch-finalizer-repeat', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let mut tx = pool.begin().await.expect("begin first tx");
        let first = finalize_completed_dispatch_terminal_entry_on_pg_tx(
            &mut tx,
            "dispatch-finalizer-repeat",
            "watcher_streaming_final",
            true,
        )
        .await
        .expect("first finalize");
        tx.commit().await.expect("commit first tx");
        assert_eq!(first.changed_entries, 1);
        assert_eq!(first.finalized_run_ids, vec!["run-1".to_string()]);

        let transition_count = count_transitions(&pool, "entry-finalizer-repeat").await;
        let outbox_count = count_message_outbox(&pool).await;

        let mut tx = pool.begin().await.expect("begin second tx");
        let second = finalize_completed_dispatch_terminal_entry_on_pg_tx(
            &mut tx,
            "dispatch-finalizer-repeat",
            "watcher_streaming_final",
            true,
        )
        .await
        .expect("second finalize");
        tx.commit().await.expect("commit second tx");

        assert_eq!(second.changed_entries, 0);
        assert!(second.affected_run_ids.is_empty());
        assert!(second.finalized_run_ids.is_empty());
        assert_eq!(
            count_transitions(&pool, "entry-finalizer-repeat").await,
            transition_count
        );
        assert_eq!(count_message_outbox(&pool).await, outbox_count);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn user_cancelled_entry_does_not_finalize_review_disabled_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET review_mode = 'disabled' WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("disable review mode");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-user-cancelled', 'run-1', NULL, 'agent-1',
                     'dispatched', NULL, 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let result = update_entry_status_on_pg(
            &pool,
            "entry-user-cancelled",
            ENTRY_STATUS_USER_CANCELLED,
            "dispatch_cancel_user",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("user cancel entry");

        assert!(result.changed);
        assert_eq!(result.to_status, ENTRY_STATUS_USER_CANCELLED);
        assert_eq!(run_status(&pool, "run-1").await, "active");
        assert_eq!(
            slot_run(&pool, "agent-1", 0).await.as_deref(),
            Some("run-1")
        );
        assert_eq!(count_message_outbox(&pool).await, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_terminal_sync_respects_blocking_phase_gate_on_paused_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET status = 'paused' WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("pause run");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-sync-skip', 'Card Sync Skip', 'in_progress', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-sync-skip', 'card-sync-skip', 'agent-1',
                     'implementation', 'cancelled', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-sync-skip', 'run-1', 'card-sync-skip', 'agent-1',
                     'dispatched', 'dispatch-sync-skip', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, verdict, pass_verdict, next_phase,
                 final_phase, anchor_card_id)
             VALUES ('run-1', 0, 'pending', NULL, 'phase_gate_passed',
                     NULL, TRUE, 'card-sync-skip')",
        )
        .execute(&pool)
        .await
        .expect("seed blocking phase gate");

        let mut tx = pool.begin().await.expect("begin tx");
        let changed = sync_dispatch_terminal_entries_on_pg_tx(
            &mut tx,
            "dispatch-sync-skip",
            ENTRY_STATUS_SKIPPED,
            "test_phase_gate_finalizer",
            true,
        )
        .await
        .expect("sync dispatch terminal");
        tx.commit().await.expect("commit tx");

        assert_eq!(changed, 1);
        assert_eq!(
            entry_row_status_dispatch_completed(&pool, "entry-sync-skip")
                .await
                .0,
            ENTRY_STATUS_SKIPPED
        );
        assert_eq!(run_status(&pool, "run-1").await, "paused");

        pool.close().await;
        pg_db.drop().await;
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_DONE, ENTRY_STATUS_PENDING, ENTRY_STATUS_SKIPPED,
        EntryStatusUpdateOptions, PhaseGateStateWrite, SlotAllocation,
        allocate_slot_for_group_agent_pg, clear_phase_gate_state_on_pg,
        latest_entry_phase_codex_session_id_pg, list_entry_dispatch_history_pg,
        reactivate_done_entry_on_pg, record_consultation_dispatch_on_pg, release_run_slots_pg,
        release_slot_for_group_agent_pg, save_phase_gate_state_on_pg, slot_has_active_dispatch_pg,
        update_entry_status_on_pg,
    };
    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};

    /// Per-test PG fixture. Mirrors the canonical lifecycle from
    /// `src/server/routes/routes_tests.rs` lines 121-220 — every test spins up
    /// an isolated `agentdesk_db_auto_queue_*` database, runs all migrations,
    /// then drops the database in `drop()` so repeat runs don't leak state
    /// (PR-C in the #843 Step 2 sequence; see #1342).
    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
        cleanup_armed: bool,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name =
                format!("agentdesk_db_auto_queue_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "db::auto_queue tests",
            )
            .await
            .unwrap();

            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
                cleanup_armed: true,
            }
        }

        async fn connect_and_migrate(&self) -> PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "db::auto_queue tests",
            )
            .await
            .unwrap()
        }

        async fn drop(mut self) {
            let drop_result = crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "db::auto_queue tests",
            )
            .await;
            if drop_result.is_ok() {
                self.cleanup_armed = false;
            }
            drop_result.expect("drop postgres test db");
        }
    }

    impl Drop for TestPostgresDb {
        fn drop(&mut self) {
            if !self.cleanup_armed {
                return;
            }
            cleanup_test_postgres_db_from_drop(self.admin_url.clone(), self.database_name.clone());
        }
    }

    fn cleanup_test_postgres_db_from_drop(admin_url: String, database_name: String) {
        let cleanup_database_name = database_name.clone();
        let thread_name = format!("db::auto_queue tests cleanup {cleanup_database_name}");
        let spawn_result = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        eprintln!(
                            "db::auto_queue tests cleanup runtime failed for {database_name}: {error}"
                        );
                        return;
                    }
                };
                if let Err(error) = runtime.block_on(crate::db::postgres::drop_test_database(
                    &admin_url,
                    &database_name,
                    "db::auto_queue tests",
                )) {
                    eprintln!(
                        "db::auto_queue tests cleanup failed for {database_name}: {error}"
                    );
                }
            });

        match spawn_result {
            Ok(handle) => {
                if handle.join().is_err() {
                    eprintln!(
                        "db::auto_queue tests cleanup thread panicked for {cleanup_database_name}"
                    );
                }
            }
            Err(error) => {
                eprintln!(
                    "db::auto_queue tests cleanup thread spawn failed for {cleanup_database_name}: {error}"
                );
            }
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    /// Seed the canonical `setup_conn` baseline against PG: one active run
    /// (`run-1` / `agent-1`), one agent row, and one slot row pre-bound to
    /// `run-1` group 0. Returns the freshly-migrated pool.
    async fn setup_pool(pg_db: &TestPostgresDb) -> PgPool {
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-1', 'repo-1', 'agent-1', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-1', 'Agent 1', 'claude', '123')",
        )
        .execute(&pool)
        .await
        .expect("seed agent");
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-1', 0, 'run-1', 0, CAST('{}' AS jsonb))",
        )
        .execute(&pool)
        .await
        .expect("seed slot");
        pool
    }

    /// Seed the shared-slot harness against PG: one active run with
    /// `max_concurrent_threads = 1` and two `pending` entries in different
    /// thread groups. Used by the concurrency test for
    /// `allocate_slot_for_group_agent_pg`.
    async fn setup_shared_slot_pool(pg_db: &TestPostgresDb) -> PgPool {
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO auto_queue_runs
                (id, repo, agent_id, status, max_concurrent_threads)
             VALUES ('run-shared', 'repo-1', 'agent-1', 'active', 1)",
        )
        .execute(&pool)
        .await
        .expect("seed shared run");
        sqlx::query("INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '999')")
            .execute(&pool)
            .await
            .expect("seed agent");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-shared-0', 'run-shared', NULL, 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed shared entry 0");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-shared-1', 'run-shared', NULL, 'agent-1', 'pending', 1)",
        )
        .execute(&pool)
        .await
        .expect("seed shared entry 1");
        pool
    }

    async fn entry_row_status_dispatch_completed(
        pool: &PgPool,
        entry_id: &str,
    ) -> (String, Option<String>, Option<DateTime<Utc>>) {
        let row = sqlx::query(
            "SELECT status, dispatch_id, completed_at
             FROM auto_queue_entries
             WHERE id = $1",
        )
        .bind(entry_id)
        .fetch_one(pool)
        .await
        .expect("entry row");
        (
            row.try_get::<String, _>("status").expect("status"),
            row.try_get::<Option<String>, _>("dispatch_id")
                .expect("dispatch_id"),
            row.try_get::<Option<DateTime<Utc>>, _>("completed_at")
                .expect("completed_at"),
        )
    }

    async fn run_status(pool: &PgPool, run_id: &str) -> String {
        let row = sqlx::query("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("run row");
        row.try_get::<String, _>("status").expect("status")
    }

    async fn slot_assignment(
        pool: &PgPool,
        agent_id: &str,
        slot_index: i64,
    ) -> (Option<String>, Option<i64>) {
        let row = sqlx::query(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = $1 AND slot_index = $2",
        )
        .bind(agent_id)
        .bind(slot_index)
        .fetch_one(pool)
        .await
        .expect("slot row");
        (
            row.try_get::<Option<String>, _>("assigned_run_id")
                .expect("assigned_run_id"),
            row.try_get::<Option<i64>, _>("assigned_thread_group")
                .expect("assigned_thread_group"),
        )
    }

    async fn count_transitions(pool: &PgPool, entry_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_entry_transitions
             WHERE entry_id = $1",
        )
        .bind(entry_id)
        .fetch_one(pool)
        .await
        .expect("transition count")
    }

    async fn count_outbox(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM message_outbox")
            .fetch_one(pool)
            .await
            .expect("outbox count")
    }

    #[tokio::test]
    async fn entry_transition_done_defers_run_completion_until_policy_hook_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group)
             VALUES ('entry-1', 'run-1', NULL, 'agent-1', 'pending', NULL, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let dispatched = update_entry_status_on_pg(
            &pool,
            "entry-1",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-1".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("dispatch transition");
        assert_eq!(dispatched.from_status, ENTRY_STATUS_PENDING);
        assert_eq!(dispatched.to_status, ENTRY_STATUS_DISPATCHED);

        update_entry_status_on_pg(
            &pool,
            "entry-1",
            ENTRY_STATUS_DONE,
            "test_done",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("done transition");

        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-1").await;
        assert_eq!(status, ENTRY_STATUS_DONE);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-1"));
        assert!(completed_at.is_some());
        assert_eq!(run_status(&pool, "run-1").await, "active");
        let slot = slot_assignment(&pool, "agent-1", 0).await;
        assert_eq!(slot.0.as_deref(), Some("run-1"));
        assert_eq!(count_transitions(&pool, "entry-1").await, 2);
        assert_eq!(
            count_outbox(&pool).await,
            0,
            "done transition must wait for policy-side completion before notifying"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_done_keeps_slot_assignment_until_multi_phase_run_finishes_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-phase-0', 'run-1', NULL, 'agent-1', 'pending', NULL, 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed phase 0 entry");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-phase-1', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed phase 1 entry");

        update_entry_status_on_pg(
            &pool,
            "entry-phase-0",
            ENTRY_STATUS_DISPATCHED,
            "test_phase_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-phase-0".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("dispatch phase 0 entry");
        update_entry_status_on_pg(
            &pool,
            "entry-phase-0",
            ENTRY_STATUS_DONE,
            "test_phase_done",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("complete phase 0 entry");

        assert_eq!(run_status(&pool, "run-1").await, "active");
        let slot = slot_assignment(&pool, "agent-1", 0).await;
        assert_eq!(slot.0.as_deref(), Some("run-1"));
        assert_eq!(slot.1, Some(0));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_done_is_idempotent_without_duplicate_side_effects_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group)
             VALUES ('entry-idempotent', 'run-1', NULL, 'agent-1', 'dispatched',
                     'dispatch-idempotent', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let first = update_entry_status_on_pg(
            &pool,
            "entry-idempotent",
            ENTRY_STATUS_DONE,
            "test_done_first",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("first completion");
        assert!(first.changed);

        let transition_count_before = count_transitions(&pool, "entry-idempotent").await;
        let outbox_count_before = count_outbox(&pool).await;

        let second = update_entry_status_on_pg(
            &pool,
            "entry-idempotent",
            ENTRY_STATUS_DONE,
            "test_done_second",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("second completion");
        assert!(
            !second.changed,
            "repeated terminal completion must become a no-op"
        );

        assert_eq!(
            count_transitions(&pool, "entry-idempotent").await,
            transition_count_before,
            "repeated completion must not append duplicate transition audit rows"
        );
        assert_eq!(
            count_outbox(&pool).await,
            outbox_count_before,
            "repeated completion must not emit duplicate completion notifications"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_pending_clears_dispatch_binding_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, completed_at)
             VALUES ('entry-2', 'run-1', NULL, 'agent-1', 'dispatched', 'dispatch-2', 0, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        update_entry_status_on_pg(
            &pool,
            "entry-2",
            ENTRY_STATUS_PENDING,
            "test_reset",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("pending reset");

        let row = sqlx::query(
            "SELECT status, dispatch_id, slot_index, completed_at
             FROM auto_queue_entries
             WHERE id = 'entry-2'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        let status: String = row.try_get("status").expect("status");
        let dispatch_id: Option<String> = row.try_get("dispatch_id").expect("dispatch_id");
        let slot_index: Option<i64> = row.try_get("slot_index").expect("slot_index");
        let completed_at: Option<DateTime<Utc>> =
            row.try_get("completed_at").expect("completed_at");
        assert_eq!(status, ENTRY_STATUS_PENDING);
        assert!(dispatch_id.is_none());
        assert!(slot_index.is_none());
        assert!(completed_at.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_dispatch_history_preserves_previous_dispatch_ids_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        // The dispatch-history FK requires task_dispatches rows.
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-consult', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch consult");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-impl', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch impl");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-history', 'run-1', NULL, 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        update_entry_status_on_pg(
            &pool,
            "entry-history",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch_initial",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-consult".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("initial dispatch");
        update_entry_status_on_pg(
            &pool,
            "entry-history",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch_resume",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-impl".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("resumed dispatch");

        let history = list_entry_dispatch_history_pg(&pool, "entry-history")
            .await
            .expect("history");
        assert_eq!(history, vec!["dispatch-consult", "dispatch-impl"]);

        let current_dispatch_id: Option<String> = sqlx::query_scalar(
            "SELECT dispatch_id FROM auto_queue_entries WHERE id = 'entry-history'",
        )
        .fetch_one(&pool)
        .await
        .expect("current dispatch");
        assert_eq!(current_dispatch_id.as_deref(), Some("dispatch-impl"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn latest_entry_phase_codex_session_id_uses_same_phase_history_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-resume', 'run-1', NULL, 'agent-1', 'pending', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, dispatch_type, status, context)
             VALUES
                ('dispatch-turn', 'agent-1', 'implementation', 'failed', '{}'),
                ('dispatch-review', 'agent-1', 'review', 'failed', '{}'),
                ('dispatch-context', 'agent-1', 'implementation', 'failed',
                    '{\"auto_queue_retry_resume_session_id\":\"context-session\"}'),
                ('dispatch-live', 'agent-1', 'implementation', 'failed', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatches");
        sqlx::query(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
             VALUES
                ('entry-resume', 'dispatch-turn', 'test'),
                ('entry-resume', 'dispatch-review', 'test'),
                ('entry-resume', 'dispatch-context', 'test'),
                ('entry-resume', 'dispatch-live', 'test')",
        )
        .execute(&pool)
        .await
        .expect("seed history");
        sqlx::query(
            "INSERT INTO turns
                (turn_id, channel_id, provider, session_id, dispatch_id, started_at, finished_at)
             VALUES
                ('turn-1', '123', 'codex', 'turn-session', 'dispatch-turn', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed turn");
        sqlx::query(
            "INSERT INTO sessions
                (session_key, agent_id, provider, status, active_dispatch_id, claude_session_id)
             VALUES
                ('codex/test/live', 'agent-1', 'codex', 'turn_active',
                 'dispatch-live', 'live-session')",
        )
        .execute(&pool)
        .await
        .expect("seed live session");

        assert_eq!(
            latest_entry_phase_codex_session_id_pg(&pool, "entry-resume", "implementation")
                .await
                .expect("lookup session")
                .as_deref(),
            Some("live-session")
        );
        sqlx::query("DELETE FROM sessions WHERE active_dispatch_id = 'dispatch-live'")
            .execute(&pool)
            .await
            .expect("remove live session");
        assert_eq!(
            latest_entry_phase_codex_session_id_pg(&pool, "entry-resume", "implementation")
                .await
                .expect("lookup context fallback")
                .as_deref(),
            Some("context-session")
        );
        sqlx::query(
            "DELETE FROM auto_queue_entry_dispatch_history
             WHERE entry_id = 'entry-resume' AND dispatch_id = 'dispatch-context'",
        )
        .execute(&pool)
        .await
        .expect("remove context fallback");
        assert_eq!(
            latest_entry_phase_codex_session_id_pg(&pool, "entry-resume", "implementation")
                .await
                .expect("lookup turn fallback")
                .as_deref(),
            Some("turn-session")
        );
        assert_eq!(
            latest_entry_phase_codex_session_id_pg(&pool, "entry-resume", "review")
                .await
                .expect("lookup review session"),
            None,
            "review phase history must not leak into implementation retries"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// PG variant of the SQLite stale-current-row retry test. The PG twin
    /// `update_entry_status_on_pg` does not expose a
    /// `..._with_current_on_conn`-style entry point — instead it carries the
    /// retry loop internally, re-loading the row when the optimistic UPDATE
    /// matches zero rows. This test simulates concurrent dispatch by writing
    /// a `dispatched` row through the helper and then asking the helper to
    /// transition straight to `skipped`, exercising the same allowed
    /// `dispatched -> skipped` path the SQLite test ultimately verified.
    #[tokio::test]
    async fn stale_allowed_transition_retries_from_latest_status_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-live', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-stale', 'run-1', NULL, 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        // Move the entry to dispatched first, then ask the shared helper to
        // skip — the PG helper resolves the latest status before retrying.
        update_entry_status_on_pg(
            &pool,
            "entry-stale",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-live".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("simulate concurrent dispatch");

        let result = update_entry_status_on_pg(
            &pool,
            "entry-stale",
            ENTRY_STATUS_SKIPPED,
            "test_cancel_retry",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("stale cancel should succeed");
        assert!(result.changed);
        assert_eq!(result.from_status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(result.to_status, ENTRY_STATUS_SKIPPED);

        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-stale").await;
        assert_eq!(status, ENTRY_STATUS_SKIPPED);
        assert!(dispatch_id.is_none());
        assert!(completed_at.is_some());

        let row = sqlx::query(
            "SELECT from_status, to_status
             FROM auto_queue_entry_transitions
             WHERE entry_id = 'entry-stale'
             ORDER BY id DESC
             LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("transition row");
        let from_status: String = row.try_get("from_status").expect("from_status");
        let to_status: String = row.try_get("to_status").expect("to_status");
        assert_eq!(from_status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(to_status, ENTRY_STATUS_SKIPPED);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_allows_skipped_restore_to_dispatched_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-restored', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-3', 'run-1', NULL, 'agent-1', 'skipped', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let restored = update_entry_status_on_pg(
            &pool,
            "entry-3",
            ENTRY_STATUS_DISPATCHED,
            "test_restore_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-restored".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("restore transition");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_SKIPPED);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        let row = sqlx::query(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-3'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        let status: String = row.try_get("status").expect("status");
        let dispatch_id: Option<String> = row.try_get("dispatch_id").expect("dispatch_id");
        let slot_index: Option<i64> = row.try_get("slot_index").expect("slot_index");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-restored"));
        assert_eq!(slot_index, Some(0));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_allows_done_restore_to_dispatched_for_recovery_sources_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-rereview', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, completed_at)
             VALUES ('entry-3b', 'run-1', NULL, 'agent-1', 'done', 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let restored = update_entry_status_on_pg(
            &pool,
            "entry-3b",
            ENTRY_STATUS_DISPATCHED,
            "rereview_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-rereview".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("recovery transition");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_DONE);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        let row = sqlx::query(
            "SELECT status, dispatch_id, slot_index, completed_at
             FROM auto_queue_entries
             WHERE id = 'entry-3b'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        let status: String = row.try_get("status").expect("status");
        let dispatch_id: Option<String> = row.try_get("dispatch_id").expect("dispatch_id");
        let slot_index: Option<i64> = row.try_get("slot_index").expect("slot_index");
        let completed_at: Option<DateTime<Utc>> =
            row.try_get("completed_at").expect("completed_at");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-rereview"));
        assert_eq!(slot_index, Some(0));
        assert!(completed_at.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_blocks_invalid_done_to_pending_restore_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-4', 'run-1', NULL, 'agent-1', 'done', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let error = update_entry_status_on_pg(
            &pool,
            "entry-4",
            ENTRY_STATUS_PENDING,
            "test_invalid",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect_err("invalid transition must fail");
        assert!(
            error.contains("invalid auto-queue entry transition"),
            "expected invalid-transition error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_blocks_invalid_done_to_dispatched_restore_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-retry', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-4b', 'run-1', NULL, 'agent-1', 'done', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let error = update_entry_status_on_pg(
            &pool,
            "entry-4b",
            ENTRY_STATUS_DISPATCHED,
            "test_invalid_done_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-retry".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect_err("done -> dispatched transition must fail");
        assert!(
            error.contains("invalid auto-queue entry transition"),
            "expected invalid-transition error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_blocks_invalid_done_to_skipped_restore_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-4c', 'run-1', NULL, 'agent-1', 'done', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let error = update_entry_status_on_pg(
            &pool,
            "entry-4c",
            ENTRY_STATUS_SKIPPED,
            "test_invalid_done_skip",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect_err("done -> skipped transition must fail");
        assert!(
            error.contains("invalid auto-queue entry transition"),
            "expected invalid-transition error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn reactivate_done_entry_allows_admin_restore_to_dispatched_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-reactivate', 'repo-1', 'agent-1', 'completed')",
        )
        .execute(&pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, completed_at)
             VALUES ('entry-reactivate', 'run-reactivate', NULL, 'agent-1', 'done', 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let restored = reactivate_done_entry_on_pg(
            &pool,
            "entry-reactivate",
            "test_reactivate_done",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("reactivate done entry");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_DONE);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-reactivate").await;
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert!(dispatch_id.is_none());
        assert!(completed_at.is_none());

        assert_eq!(run_status(&pool, "run-reactivate").await, "active");

        pool.close().await;
        pg_db.drop().await;
    }

    /// Two concurrent allocations against a single-slot pool must succeed at
    /// most once. The PG twin's optimistic CAS retry loop handles concurrent
    /// claims; a second tokio task racing the same pool must observe either
    /// `None` (no slot available) or a successful allocation, but never both
    /// claims at the same time.
    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_never_double_assigns_single_slot_under_concurrency() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_shared_slot_pool(&pg_db).await;

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let task_a = tokio::spawn(async move {
            allocate_slot_for_group_agent_pg(&pool_a, "run-shared", 0, "agent-1").await
        });
        let task_b = tokio::spawn(async move {
            allocate_slot_for_group_agent_pg(&pool_b, "run-shared", 1, "agent-1").await
        });
        let first = task_a.await.unwrap().expect("first allocation");
        let second = task_b.await.unwrap().expect("second allocation");

        let successful: Vec<SlotAllocation> = [first, second].into_iter().flatten().collect();
        assert_eq!(
            successful.len(),
            1,
            "single-slot pool must allow only one concurrent group allocation"
        );

        let assignments: Vec<(Option<String>, Option<i64>)> = sqlx::query(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-1'
             ORDER BY slot_index ASC",
        )
        .fetch_all(&pool)
        .await
        .expect("slot rows")
        .into_iter()
        .map(|row| {
            (
                row.try_get::<Option<String>, _>("assigned_run_id")
                    .expect("assigned_run_id"),
                row.try_get::<Option<i64>, _>("assigned_thread_group")
                    .expect("assigned_thread_group"),
            )
        })
        .collect();
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].0.as_deref(),
            Some("run-shared"),
            "the slot must remain assigned to exactly one run"
        );

        let slotted_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_entries
             WHERE slot_index IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .expect("slotted count");
        assert_eq!(
            slotted_count, 1,
            "only one group entry must receive the single slot"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_rebinds_completed_same_run_slot_without_reset() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "UPDATE auto_queue_slots
             SET thread_id_map = CAST($1 AS jsonb)
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .bind(r#"{"123":"thread-slot-0"}"#)
        .execute(&pool)
        .await
        .expect("seed slot thread map");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, slot_index, thread_group,
                 batch_phase, completed_at)
             VALUES ('entry-complete', 'run-1', NULL, 'agent-1', 'done', 0, 0, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed completed entry");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-next', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed next phase entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 1, "agent-1")
            .await
            .expect("same-run rebind must succeed");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 0,
                newly_assigned: false,
                reassigned_from_other_group: true,
            })
        );

        let row = sqlx::query(
            "SELECT assigned_run_id, assigned_thread_group, thread_id_map::TEXT AS thread_id_map
             FROM auto_queue_slots
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("slot row");
        let assigned_run_id: Option<String> =
            row.try_get("assigned_run_id").expect("assigned_run_id");
        let assigned_thread_group: Option<i64> = row
            .try_get("assigned_thread_group")
            .expect("assigned_thread_group");
        let thread_id_map: Option<String> = row.try_get("thread_id_map").expect("thread_id_map");
        assert_eq!(assigned_run_id.as_deref(), Some("run-1"));
        assert_eq!(assigned_thread_group, Some(1));
        let parsed: serde_json::Value =
            serde_json::from_str(thread_id_map.as_deref().unwrap_or("{}"))
                .expect("thread_id_map json");
        assert_eq!(parsed["123"], "thread-slot-0");

        let slot_index: Option<i64> =
            sqlx::query_scalar("SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-next'")
                .fetch_one(&pool)
                .await
                .expect("next entry slot");
        assert_eq!(slot_index, Some(0));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_marks_cross_run_reclaim_as_new_assignment() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "UPDATE auto_queue_slots
             SET thread_id_map = CAST($1 AS jsonb)
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .bind(r#"{"123":"thread-slot-0"}"#)
        .execute(&pool)
        .await
        .expect("seed slot thread map");
        release_run_slots_pg(&pool, "run-1")
            .await
            .expect("release first run slots");
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-2', 'repo-1', 'agent-1', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed second run");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-run-2', 'run-2', NULL, 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed second run entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-2", 0, "agent-1")
            .await
            .expect("cross-run claim must succeed");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 0,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );

        let row = sqlx::query(
            "SELECT assigned_run_id, assigned_thread_group, thread_id_map::TEXT AS thread_id_map
             FROM auto_queue_slots
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("slot row");
        let assigned_run_id: Option<String> =
            row.try_get("assigned_run_id").expect("assigned_run_id");
        let assigned_thread_group: Option<i64> = row
            .try_get("assigned_thread_group")
            .expect("assigned_thread_group");
        let thread_id_map: Option<String> = row.try_get("thread_id_map").expect("thread_id_map");
        assert_eq!(assigned_run_id.as_deref(), Some("run-2"));
        assert_eq!(assigned_thread_group, Some(0));
        let parsed: serde_json::Value =
            serde_json::from_str(thread_id_map.as_deref().unwrap_or("{}"))
                .expect("thread_id_map json");
        assert_eq!(parsed["123"], "thread-slot-0");

        pool.close().await;
        pg_db.drop().await;
    }

    /// Force the bounded-retry exit path by attaching a PG trigger that
    /// silently rejects any UPDATE that would CAS the slot from `NULL ->
    /// run-1`. Mirrors the SQLite `RAISE(IGNORE)` trigger from the previous
    /// in-memory test.
    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_fails_after_bounded_cas_retries() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .execute(&pool)
        .await
        .expect("free seed slot");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-cas-retry', 'run-1', NULL, 'agent-1', 'pending', 1)",
        )
        .execute(&pool)
        .await
        .expect("seed retry entry");

        // Suppress the CAS update via a BEFORE-UPDATE trigger that returns
        // NULL when the helper attempts to claim the slot for `run-1` from a
        // currently-unassigned slot. Returning NULL from a BEFORE trigger
        // skips the row update, so `rows_affected` stays 0 and the helper's
        // bounded retry loop eventually exits with an error.
        sqlx::query(
            "CREATE OR REPLACE FUNCTION test_ignore_slot_claim()
             RETURNS TRIGGER AS $$
             BEGIN
                 IF NEW.assigned_run_id = 'run-1' AND OLD.assigned_run_id IS NULL THEN
                     RETURN NULL;
                 END IF;
                 RETURN NEW;
             END;
             $$ LANGUAGE plpgsql",
        )
        .execute(&pool)
        .await
        .expect("create trigger function");
        sqlx::query(
            "CREATE TRIGGER ignore_slot_claim
             BEFORE UPDATE ON auto_queue_slots
             FOR EACH ROW EXECUTE FUNCTION test_ignore_slot_claim()",
        )
        .execute(&pool)
        .await
        .expect("attach trigger");

        let error = allocate_slot_for_group_agent_pg(&pool, "run-1", 1, "agent-1")
            .await
            .expect_err("forced claim race must terminate with bounded retry error");
        assert!(
            error.contains("slot allocation retry limit exceeded"),
            "expected bounded-retry error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn release_slot_for_group_agent_pg_clears_only_matching_assignment() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;

        let released = release_slot_for_group_agent_pg(&pool, "run-1", 0, "agent-1", 0)
            .await
            .expect("release matching slot");
        assert_eq!(released, 1);

        let slot = slot_assignment(&pool, "agent-1", 0).await;
        assert_eq!(slot, (None, None));

        let released_again = release_slot_for_group_agent_pg(&pool, "run-1", 0, "agent-1", 0)
            .await
            .expect("release already cleared slot");
        assert_eq!(released_again, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    /// In the PG flow, `update_entry_status_on_pg(..., done)` does NOT touch
    /// `auto_queue_slots` — the slot stays assigned until a downstream
    /// policy hook calls `release_slot_for_group_agent_pg`. This test
    /// verifies that invariant directly: the done transition records the
    /// audit row, completes the entry, and leaves the slot assignment
    /// intact, mirroring what the SQLite test asserted via a synthetic
    /// trigger.
    #[tokio::test]
    async fn terminal_transition_done_defers_slot_release_failures_until_policy_hook_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-rollback', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group)
             VALUES ('entry-rollback', 'run-1', NULL, 'agent-1', 'pending', NULL, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        update_entry_status_on_pg(
            &pool,
            "entry-rollback",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-rollback".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("dispatch transition");

        update_entry_status_on_pg(
            &pool,
            "entry-rollback",
            ENTRY_STATUS_DONE,
            "test_done_rollback",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("done transition should defer slot release until policy hook");

        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-rollback").await;
        assert_eq!(status, ENTRY_STATUS_DONE);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-rollback"));
        assert!(completed_at.is_some());

        assert_eq!(run_status(&pool, "run-1").await, "active");
        let slot = slot_assignment(&pool, "agent-1", 0).await;
        assert_eq!(slot.0.as_deref(), Some("run-1"));
        assert_eq!(
            count_transitions(&pool, "entry-rollback").await,
            2,
            "done transition audit must still be recorded"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn slot_has_active_dispatch_ignores_sidecar_dispatches_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ($1, $2, 'dispatched', $3)",
        )
        .bind("dispatch-sidecar")
        .bind("agent-1")
        .bind(
            serde_json::json!({
                "slot_index": 0,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-1",
                }
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed sidecar dispatch");

        assert!(
            !slot_has_active_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("query active sidecar dispatch"),
            "sidecar phase-gate dispatches must not keep a slot occupied"
        );

        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ($1, $2, 'dispatched', $3)",
        )
        .bind("dispatch-primary")
        .bind("agent-1")
        .bind(serde_json::json!({"slot_index": 0}).to_string())
        .execute(&pool)
        .await
        .expect("seed primary dispatch");

        assert!(
            slot_has_active_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("query active primary dispatch"),
            "primary dispatches must still block slot reuse"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn record_consultation_dispatch_preserves_metadata_and_marks_entry_dispatched_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, metadata)
             VALUES ('card-consult', 'Card Consult', 'requested', CAST($1 AS jsonb))",
        )
        .bind(
            serde_json::json!({
                "keep": "yes",
                "preflight_status": "consult_required"
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed kanban card");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-consult', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-consult', 'run-1', 'card-consult', 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let result = record_consultation_dispatch_on_pg(
            &pool,
            "entry-consult",
            "card-consult",
            "dispatch-consult",
            "test_consultation_dispatch",
            r#"{"keep":"yes","preflight_status":"consult_required"}"#,
        )
        .await
        .expect("consultation dispatch");
        assert!(result.entry_status_changed);

        let metadata: serde_json::Value =
            sqlx::query_scalar("SELECT metadata::TEXT FROM kanban_cards WHERE id = 'card-consult'")
                .fetch_one(&pool)
                .await
                .ok()
                .and_then(|raw: String| serde_json::from_str(&raw).ok())
                .expect("metadata json");
        assert_eq!(metadata["keep"], "yes");
        assert_eq!(metadata["preflight_status"], "consult_required");
        assert_eq!(metadata["consultation_status"], "pending");
        assert_eq!(metadata["consultation_dispatch_id"], "dispatch-consult");

        let row = sqlx::query(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-consult'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        let status: String = row.try_get("status").expect("status");
        let dispatch_id: Option<String> = row.try_get("dispatch_id").expect("dispatch_id");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-consult"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn record_consultation_dispatch_requires_dispatch_id_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        let error = record_consultation_dispatch_on_pg(
            &pool,
            "entry-missing",
            "card-missing",
            "   ",
            "test_consultation_dispatch",
            "{}",
        )
        .await
        .expect_err("missing dispatch id must fail");
        assert!(
            error.contains("consultation dispatch id is required"),
            "expected missing-dispatch-id error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn save_phase_gate_state_filters_invalid_dispatches_and_removes_stale_rows_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        for dispatch_id in &["dispatch-valid-1", "dispatch-valid-2", "dispatch-stale"] {
            sqlx::query(
                "INSERT INTO task_dispatches (id, to_agent_id, status, context)
                 VALUES ($1, 'agent-1', 'dispatched', '{}')",
            )
            .bind(dispatch_id)
            .execute(&pool)
            .await
            .expect("seed dispatch");
        }
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, dispatch_id, pass_verdict)
             VALUES ('run-1', 2, 'pending', 'dispatch-stale', 'phase_gate_passed')",
        )
        .execute(&pool)
        .await
        .expect("seed stale phase gate row");

        let result = save_phase_gate_state_on_pg(
            &pool,
            "run-1",
            2,
            &PhaseGateStateWrite {
                status: "failed".to_string(),
                verdict: Some("phase_gate_failed".to_string()),
                dispatch_ids: vec![
                    "dispatch-valid-1".to_string(),
                    "dispatch-valid-1".to_string(),
                    "dispatch-missing".to_string(),
                    "dispatch-valid-2".to_string(),
                ],
                pass_verdict: "phase_gate_passed".to_string(),
                next_phase: Some(3),
                final_phase: true,
                anchor_card_id: None,
                failure_reason: Some("phase gate failed".to_string()),
                created_at: Some("2026-04-15 00:00:00+00".to_string()),
            },
        )
        .await
        .expect("save phase gate state");

        assert_eq!(
            result.persisted_dispatch_ids,
            vec![
                "dispatch-valid-1".to_string(),
                "dispatch-valid-2".to_string()
            ]
        );
        assert_eq!(result.removed_stale_rows, 1);

        let rows = sqlx::query(
            "SELECT dispatch_id, status, verdict, next_phase, final_phase, failure_reason
             FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2
             ORDER BY dispatch_id ASC",
        )
        .bind("run-1")
        .bind(2_i64)
        .fetch_all(&pool)
        .await
        .expect("phase gate rows");
        assert_eq!(rows.len(), 2);
        let dispatch_id_0: Option<String> = rows[0].try_get("dispatch_id").expect("dispatch_id 0");
        let dispatch_id_1: Option<String> = rows[1].try_get("dispatch_id").expect("dispatch_id 1");
        let status_0: String = rows[0].try_get("status").expect("status 0");
        let verdict_0: Option<String> = rows[0].try_get("verdict").expect("verdict 0");
        let next_phase_0: Option<i64> = rows[0].try_get("next_phase").expect("next_phase 0");
        let final_phase_0: bool = rows[0].try_get("final_phase").expect("final_phase 0");
        let failure_reason_0: Option<String> =
            rows[0].try_get("failure_reason").expect("failure_reason 0");
        assert_eq!(dispatch_id_0.as_deref(), Some("dispatch-valid-1"));
        assert_eq!(dispatch_id_1.as_deref(), Some("dispatch-valid-2"));
        assert_eq!(status_0, "failed");
        assert_eq!(verdict_0.as_deref(), Some("phase_gate_failed"));
        assert_eq!(next_phase_0, Some(3));
        assert!(final_phase_0);
        assert_eq!(failure_reason_0.as_deref(), Some("phase gate failed"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn clear_phase_gate_state_removes_phase_rows_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, dispatch_id, pass_verdict)
             VALUES ('run-1', 2, 'pending', NULL, 'phase_gate_passed')",
        )
        .execute(&pool)
        .await
        .expect("seed phase 2");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, dispatch_id, pass_verdict)
             VALUES ('run-1', 3, 'pending', NULL, 'phase_gate_passed')",
        )
        .execute(&pool)
        .await
        .expect("seed phase 3");

        assert!(
            clear_phase_gate_state_on_pg(&pool, "run-1", 2)
                .await
                .expect("clear phase 2")
        );

        let phase_two_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2",
        )
        .bind("run-1")
        .bind(2_i64)
        .fetch_one(&pool)
        .await
        .expect("phase 2 count");
        let phase_three_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2",
        )
        .bind("run-1")
        .bind(3_i64)
        .fetch_one(&pool)
        .await
        .expect("phase 3 count");
        assert_eq!(phase_two_count, 0);
        assert_eq!(phase_three_count, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #821 (4): `done` entries must not reactivate without an explicit
    /// operator rerun. The shared `update_entry_status_on_pg` helper gates
    /// `done -> dispatched` behind the `pmd_reopen` / `rereview_dispatch`
    /// trigger sources (see `is_allowed_entry_transition`), and `done ->
    /// pending` is simply not in the allowlist. The only authorized entry
    /// point that can legally flip a `done` row back to `dispatched` is
    /// `reactivate_done_entry_on_pg` itself, invoked from the PMD reopen
    /// route in `src/server/routes/kanban.rs` (the `pmd_reopen` /
    /// `rereview_dispatch` call sites).
    #[tokio::test]
    async fn done_entry_cannot_reactivate_without_explicit_operator_rerun_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-821-rea', 'repo-1', 'agent-1', 'completed')",
        )
        .execute(&pool)
        .await
        .expect("seed completed run");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-sneak', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed sneak dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, completed_at)
             VALUES ('entry-821-rea', 'run-821-rea', NULL, 'agent-1', 'done', 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        for bogus_source in &[
            "auto_queue_tick",
            "tick",
            "policy_hook",
            "onDispatchCompleted",
            "review_automation",
        ] {
            let err = update_entry_status_on_pg(
                &pool,
                "entry-821-rea",
                ENTRY_STATUS_DISPATCHED,
                bogus_source,
                &EntryStatusUpdateOptions {
                    dispatch_id: Some("dispatch-sneak".to_string()),
                    slot_index: Some(0),
                },
            )
            .await
            .expect_err("non-operator source must not resurrect a done entry");
            assert!(
                err.contains("invalid auto-queue entry transition"),
                "source `{bogus_source}` unexpectedly permitted done -> dispatched (got {err})"
            );
        }

        let err = update_entry_status_on_pg(
            &pool,
            "entry-821-rea",
            ENTRY_STATUS_PENDING,
            "pmd_reopen",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect_err("done -> pending must not be a valid transition at all");
        assert!(
            err.contains("invalid auto-queue entry transition"),
            "expected invalid-transition error, got: {err}"
        );

        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-821-rea'")
                .fetch_one(&pool)
                .await
                .expect("entry row");
        assert_eq!(entry_status, "done");
        assert_eq!(run_status(&pool, "run-821-rea").await, "completed");

        let restored = reactivate_done_entry_on_pg(
            &pool,
            "entry-821-rea",
            "pmd_reopen",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("operator-authorized reactivate must succeed");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_DONE);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        pool.close().await;
        pg_db.drop().await;
    }
}
