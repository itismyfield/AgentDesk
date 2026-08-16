use sqlx::{PgPool, Row as SqlxRow};
use std::collections::BTreeSet;
use thiserror::Error;

use super::run_status::is_live_run_status;
use super::runs::{
    acquire_run_advisory_xact_lock_on_pg_tx, acquire_run_advisory_xact_locks_on_pg_tx,
    auto_queue_run_review_disabled_on_pg_tx, maybe_finalize_run_after_terminal_entry_pg,
    maybe_finalize_run_if_ready_pg,
};

mod dispatch_failure;
pub use dispatch_failure::{
    EntryDispatchFailureAlert, record_entry_dispatch_failure_on_pg,
    record_entry_dispatch_failure_with_alert_on_pg,
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
// reason: auto-queue dispatchability shim wired on select tick paths; current
// callers live in the dispatch-cancel test surface. See #3034.
#[allow(dead_code)]
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
    pub failure_transition_id: Option<i64>,
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

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("open postgres auto-queue done reactivation tx: {error}"))?;
    acquire_run_advisory_xact_lock_on_pg_tx(&mut tx, &current.run_id).await?;

    let expected_dispatch_id = options
        .dispatch_id
        .clone()
        .or_else(|| current.dispatch_id.clone());
    let expected_slot_index = options.slot_index.or(current.slot_index);
    let (reloaded_run_id, reloaded_status, reloaded_dispatch_id, reloaded_slot_index) =
        lock_entry_dispatch_identity_on_pg_tx(&mut tx, entry_id).await?;
    if reloaded_run_id != current.run_id {
        tx.rollback().await.map_err(|error| {
            format!("rollback stale postgres auto-queue reactivation {entry_id}: {error}")
        })?;
        return Err(format!(
            "auto-queue entry {entry_id} run identity changed during reactivation"
        ));
    }
    if reloaded_status == ENTRY_STATUS_DISPATCHED {
        if reloaded_dispatch_id != expected_dispatch_id
            || reloaded_slot_index != expected_slot_index
        {
            tx.rollback().await.map_err(|error| {
                format!("rollback stale postgres auto-queue reactivation {entry_id}: {error}")
            })?;
            return Err(format!(
                "auto-queue entry {entry_id} dispatch identity changed during reactivation"
            ));
        }
        ensure_done_entry_run_live_on_pg_tx(&mut tx, &reloaded_run_id, entry_id).await?;
        tx.commit().await.map_err(|error| {
            format!("commit accepted postgres auto-queue reactivation {entry_id}: {error}")
        })?;
        return Ok(EntryStatusUpdateResult {
            run_id: reloaded_run_id,
            from_status: ENTRY_STATUS_DISPATCHED.to_string(),
            to_status: ENTRY_STATUS_DISPATCHED.to_string(),
            changed: false,
        });
    }
    if reloaded_status != ENTRY_STATUS_DONE
        || reloaded_dispatch_id != current.dispatch_id
        || reloaded_slot_index != current.slot_index
    {
        tx.rollback().await.map_err(|error| {
            format!("rollback stale postgres auto-queue reactivation {entry_id}: {error}")
        })?;
        return Err(format!(
            "auto-queue entry {entry_id} dispatch identity changed during reactivation"
        ));
    }

    ensure_done_entry_run_live_on_pg_tx(&mut tx, &reloaded_run_id, entry_id).await?;

    let result = update_entry_status_on_pg_tx(
        &mut tx,
        entry_id,
        ENTRY_STATUS_DISPATCHED,
        trigger_source,
        options,
    )
    .await?;
    if !result.changed {
        tx.rollback().await.map_err(|error| {
            format!("rollback stale postgres auto-queue reactivation {entry_id}: {error}")
        })?;
        return Err(format!(
            "auto-queue entry {entry_id} dispatch identity changed during reactivation"
        ));
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit postgres auto-queue reactivation {entry_id}: {error}"))?;

    Ok(result)
}

/// Keep done-entry reactivation symmetric across the dedicated reactivate and
/// kanban-reopen paths after their run token has been acquired.
pub(crate) async fn ensure_done_entry_run_live_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    entry_id: &str,
) -> Result<(), String> {
    let run_status = sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| {
        format!("reload postgres auto-queue run {run_id} before entry reactivation: {error}")
    })?
    .ok_or_else(|| format!("auto-queue run not found before entry reactivation: {run_id}"))?;

    if run_status == "completed" {
        let reactivated_run = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'active',
                 completed_at = NULL
             WHERE id = $1
               AND status = 'completed'",
        )
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("reactivate postgres auto-queue run {run_id}: {error}"))?
        .rows_affected();
        if reactivated_run != 1 {
            return Err(format!(
                "auto-queue run {run_id} changed while reactivating completed entry {entry_id}"
            ));
        }

        let (agent_id, slot_index, thread_group) =
            sqlx::query_as::<_, (Option<String>, Option<i64>, i64)>(
                "SELECT agent_id,
                        slot_index::BIGINT,
                        COALESCE(thread_group, 0)::BIGINT
                 FROM auto_queue_entries
                 WHERE id = $1
                   AND run_id = $2",
            )
            .bind(entry_id)
            .bind(run_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                format!("load postgres auto-queue entry {entry_id} slot identity: {error}")
            })?
            .ok_or_else(|| {
                format!("auto-queue entry {entry_id} disappeared while reactivating run {run_id}")
            })?;
        if let Some(slot_index) = slot_index {
            let reacquired_slot = sqlx::query(
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
            .bind(agent_id.as_deref())
            .bind(slot_index)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                format!(
                    "reacquire postgres auto-queue slot {slot_index} for revived run {run_id}: {error}"
                )
            })?
            .rows_affected();
            if reacquired_slot != 1 {
                return Err(format!(
                    "auto-queue slot {slot_index} for entry {entry_id} is owned by another run: refusing to revive {run_id}"
                ));
            }
        }

        // A completion notification cannot be recalled. If reopen revives a
        // run after that notification was published, resumed activity may be
        // observed after the completion notification.
    } else if !is_live_run_status(&run_status) {
        return Err(format!(
            "auto-queue run {run_id} is {run_status}: refusing to reactivate done entry {entry_id}"
        ));
    }

    Ok(())
}

/// Lock and return the full entry identity used to accept a concurrent
/// done-to-dispatched transition. Status alone is not sufficient: a different
/// dispatch or slot is a competing owner, not an idempotent target state.
pub(crate) async fn lock_entry_dispatch_identity_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
) -> Result<(String, String, Option<String>, Option<i64>), String> {
    sqlx::query_as::<_, (String, String, Option<String>, Option<i64>)>(
        "SELECT run_id, status, dispatch_id, slot_index::BIGINT
         FROM auto_queue_entries
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(entry_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| {
        format!("lock postgres auto-queue entry {entry_id} dispatch identity: {error}")
    })?
    .ok_or_else(|| format!("auto-queue entry not found while locking identity: {entry_id}"))
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
    // #5356 S2: the write target is fixed once, before the retry loop, so a
    // stale reload cannot silently redirect what this call attaches. The pin is
    // defensive rather than load-bearing — the 0-row drift branch below only
    // `continue`s while the row still carries the identity this attempt
    // observed, so recomputing `options.or(current.*)` per iteration would
    // yield the same value. Its declaration site is locked structurally by
    // `pinned_dispatch_identity_is_declared_before_the_stale_retry_loop`.
    let pinned_dispatch_id = options
        .dispatch_id
        .clone()
        .or_else(|| current.dispatch_id.clone());
    let pinned_slot_index = options.slot_index.or(current.slot_index);

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

        let metadata_change = match normalized {
            ENTRY_STATUS_PENDING => {
                current.dispatch_id.is_some()
                    || current.slot_index.is_some()
                    || current.completed_at.is_some()
            }
            ENTRY_STATUS_DISPATCHED => {
                pinned_dispatch_id != current.dispatch_id
                    || pinned_slot_index != current.slot_index
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

        if normalized == ENTRY_STATUS_DISPATCHED {
            gate_dispatched_entry_run_on_pg_tx(&mut tx, &current.run_id).await?;
            validate_new_dispatch_attachment_on_pg_tx(
                &mut tx,
                entry_id,
                &current.status,
                current.dispatch_id.as_deref(),
                pinned_dispatch_id.as_deref(),
            )
            .await?;
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
                   AND status = $4
                   AND dispatch_id IS NOT DISTINCT FROM $5
                   AND slot_index IS NOT DISTINCT FROM $6",
            )
            .bind(pinned_dispatch_id.as_deref())
            .bind(pinned_slot_index)
            .bind(entry_id)
            .bind(&current.status)
            .bind(current.dispatch_id.as_deref())
            .bind(current.slot_index)
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
                pinned_dispatch_id.as_deref(),
                pinned_slot_index,
            ) {
                return Ok(EntryStatusUpdateResult {
                    run_id: latest.run_id,
                    from_status: latest.status,
                    to_status: normalized.to_string(),
                    changed: false,
                });
            }
            // #5356 S2: the relatch defense is this branch and the CAS above
            // together, not either alone — and neither of them is the hoisted
            // pin. The dispatched arm's CAS carries the identity condition
            // (`dispatch_id`/`slot_index IS NOT DISTINCT FROM` the observed
            // values), so a competing owner's row fails to match and this
            // attempt writes nothing; this branch is what then stops the retry
            // from re-reading the winner's row and overwriting its identity,
            // returning `changed: true` with the requested dispatch attached.
            // On the other five arms, whose CAS matches on id + status alone,
            // this branch is the only identity check the 0-row path has: it
            // keeps a stale retry of any status update — not just of a new
            // attachment — off a row a new owner claimed after the reload.
            //
            // Drift is judged against the observed pre-state (`current.*`), not
            // against the write target (`pinned_*`); on the dispatched arm
            // those observed values are also the ones that arm's CAS bound in
            // its WHERE. A new attachment (`options.dispatch_id = Some(..)`)
            // differs from the row by construction, so comparing the target
            // would also reject pure-status races — the very reloads this retry
            // loop exists to absorb.
            //
            // `reactivate_done_entry_on_pg` splits acceptance and drift the
            // same way (acceptance against the expected identity, drift against
            // the observed one), yet the siblings deliberately disagree on one
            // race: with the run live and a competitor moving the entry
            // `done -> dispatched` while leaving the identity NULL, this helper
            // absorbs the reload and attaches, while that helper's dispatched
            // branch measures the still-NULL identity against the requested one
            // and errors. That asymmetry is the intended contract, not drift to
            // unify — reactivation refuses to revive an entry another actor
            // concurrently dispatched and takes only an already-identical
            // re-dispatch as idempotent, whereas a status update exists to
            // absorb its own stale reloads.
            if latest.dispatch_id != current.dispatch_id || latest.slot_index != current.slot_index
            {
                return Err(format!(
                    "auto-queue entry {entry_id} dispatch identity changed during status update"
                ));
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
                .filter(|value| Some(*value) != pinned_dispatch_id.as_deref())
            {
                record_entry_dispatch_history_on_pg(
                    &mut tx,
                    entry_id,
                    previous_dispatch_id,
                    trigger_source,
                )
                .await?;
            }
            if let Some(dispatch_id) = pinned_dispatch_id.as_deref() {
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
/// Unlike the pool-scoped helper this is single-shot: on stale-row mismatch it
/// returns `changed: false`. The pool-scoped helper re-reads and loops only
/// while the reloaded row still carries the dispatch identity it observed; once
/// that identity drifts it returns an error instead of looping. The caller
/// composes this into a wider atomic operation, so observed state is already a
/// stable snapshot inside the transaction.
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

    if normalized == ENTRY_STATUS_DISPATCHED {
        gate_dispatched_entry_run_on_pg_tx(tx, &current.run_id).await?;
        validate_new_dispatch_attachment_on_pg_tx(
            tx,
            entry_id,
            &current.status,
            current.dispatch_id.as_deref(),
            effective_dispatch_id.as_deref(),
        )
        .await?;
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
               AND status = $4
               AND dispatch_id IS NOT DISTINCT FROM $5
               AND slot_index IS NOT DISTINCT FROM $6",
        )
        .bind(effective_dispatch_id.as_deref())
        .bind(effective_slot_index)
        .bind(entry_id)
        .bind(&current.status)
        .bind(current.dispatch_id.as_deref())
        .bind(current.slot_index)
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

/// Serializes a live attachment with terminal run writers, then derives the
/// run predicate from a fresh READ COMMITTED statement under that token.
///
/// Call sites invoke this only for the dispatched arm, before its entry write.
/// That arm does not call `maybe_finalize_run_if_ready_pg`; its non-blocking
/// finalizer protocol and the participant lock-order summary are documented by
/// that symbol in `runs`.
async fn gate_dispatched_entry_run_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    acquire_run_advisory_xact_lock_on_pg_tx(tx, run_id).await?;
    let run_status = sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("reload auto-queue run {run_id} before entry dispatch: {error}"))?
    .ok_or_else(|| format!("auto-queue run not found before entry dispatch: {run_id}"))?;

    if !is_live_run_status(&run_status) {
        return Err(format!(
            "auto-queue run {run_id} is {run_status}: refusing to dispatch an entry"
        ));
    }

    Ok(())
}

/// Acquire every run token before a caller-owned transaction writes any row.
/// Sorting and deduplication keep multi-run attachment transactions on one
/// advisory-lock order; the dispatched choke point re-enters these tokens.
pub(crate) async fn acquire_dispatched_entry_run_tokens_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_ids: &[String],
) -> Result<(), String> {
    acquire_run_advisory_xact_locks_on_pg_tx(tx, run_ids).await?;
    Ok(())
}

/// A newly linked dispatch remains stable through the entry write and history
/// insert. This closes the window where an attacher observed a live dispatch,
/// then linked it after another transaction terminalized it. A missing
/// dispatch row is rejected because attachment ownership cannot be validated,
/// so the guard fails closed rather than creating an unverifiable link.
async fn validate_new_dispatch_attachment_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
    current_status: &str,
    current_dispatch_id: Option<&str>,
    effective_dispatch_id: Option<&str>,
) -> Result<(), String> {
    let retains_existing_link = effective_dispatch_id == current_dispatch_id
        && matches!(current_status, ENTRY_STATUS_DONE | ENTRY_STATUS_DISPATCHED);
    let Some(dispatch_id) = effective_dispatch_id.filter(|_| !retains_existing_link) else {
        return Ok(());
    };

    let dispatch_status = sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM task_dispatches
         WHERE id = $1
         FOR SHARE",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| {
        format!(
            "reload dispatch {dispatch_id} before attaching auto-queue entry {entry_id}: {error}"
        )
    })?
    .ok_or_else(|| {
        format!("dispatch {dispatch_id} not found before attaching auto-queue entry {entry_id}")
    })?;

    if !matches!(dispatch_status.as_str(), "pending" | "dispatched") {
        return Err(format!(
            "dispatch {dispatch_id} is {dispatch_status}: refusing to attach auto-queue entry {entry_id}"
        ));
    }

    Ok(())
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
                         WHERE id = $3
                           AND dispatch_id IS NULL
                           AND status = $4",
                    )
                    .bind(&linked_dispatch_id)
                    .bind(slot_index)
                    .bind(&entry_id)
                    .bind(new_status)
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

    for run_id in &result.affected_run_ids {
        if auto_queue_run_review_disabled_on_pg_tx(tx, &run_id).await?
            && maybe_finalize_run_if_ready_pg(tx, &run_id).await?
        {
            result.finalized_run_ids.push(run_id.clone());
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
        // The API reopen route is an existing caller of the same done-entry
        // reactivation contract as pmd/rereview; retaining it here preserves
        // that public behavior even though the design transition table names
        // the internal sources.
        && matches!(
            trigger_source,
            "api_reopen" | "pmd_reopen" | "rereview_dispatch"
        )
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
) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO auto_queue_entry_transitions (
             entry_id,
             from_status,
             to_status,
             trigger_source
         )
         VALUES ($1, $2, $3, $4)
         RETURNING id",
    )
    .bind(entry_id)
    .bind(from_status)
    .bind(to_status)
    .bind(trigger_source)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("record auto-queue transition for {entry_id}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use sqlx::{Connection, PgConnection, PgPool};

    /// This module's own source, read by the declaration-site lock on the
    /// pinned dispatch identity.
    const MODULE_SOURCE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", file!()));

    async fn setup_entry(pool: &PgPool, run_status: &str, entry_status: &str) {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('gate-agent', 'Gate Agent', 'claude', 'gate-channel')",
        )
        .execute(pool)
        .await
        .expect("seed gate agent");
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('gate-run', 'gate-repo', 'gate-agent', $1)",
        )
        .bind(run_status)
        .execute(pool)
        .await
        .expect("seed gate run");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('gate-card', 'Gate Card', 'in_progress', 'gate-agent')",
        )
        .execute(pool)
        .await
        .expect("seed gate card");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, completed_at)
             VALUES ('gate-entry', 'gate-run', 'gate-card', 'gate-agent', $1,
                     CASE WHEN $1 = 'done' THEN NOW() END)",
        )
        .bind(entry_status)
        .execute(pool)
        .await
        .expect("seed gate entry");
    }

    async fn seed_dispatch(pool: &PgPool, dispatch_id: &str) {
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status)
             VALUES ($1, 'gate-card', 'gate-agent', 'implementation', 'dispatched')",
        )
        .bind(dispatch_id)
        .execute(pool)
        .await
        .expect("seed gate dispatch");
    }

    async fn entry_status(pool: &PgPool) -> String {
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'gate-entry'")
            .fetch_one(pool)
            .await
            .expect("load gate entry status")
    }

    async fn run_status(pool: &PgPool) -> String {
        sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'gate-run'")
            .fetch_one(pool)
            .await
            .expect("load gate run status")
    }

    async fn transition_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_entry_transitions
             WHERE entry_id = 'gate-entry'",
        )
        .fetch_one(pool)
        .await
        .expect("count gate entry transitions")
    }

    async fn begin_run_token_holder(database_url: &str) -> PgConnection {
        let mut conn = PgConnection::connect(database_url)
            .await
            .expect("connect run-token holder");
        sqlx::query("BEGIN")
            .execute(&mut conn)
            .await
            .expect("begin run-token holder");
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || 'gate-run'))")
            .execute(&mut conn)
            .await
            .expect("hold gate run token");
        conn
    }

    async fn wait_for_run_token_waiter(conn: &mut PgConnection) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            sqlx::query("SELECT pg_stat_clear_snapshot()")
                .execute(&mut *conn)
                .await
                .expect("clear backend-status snapshot");
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1
                     FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND wait_event_type = 'Lock'
                       AND query LIKE '%pg_advisory_xact_lock%aq_run:%'
                 )",
            )
            .fetch_one(&mut *conn)
            .await
            .expect("inspect gate run-token waiter");
            if waiting {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "entry transition did not wait for the run token"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn cancelled_run_rejects_dispatched_tx_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "cancelled", ENTRY_STATUS_PENDING).await;

        let mut tx = pool.begin().await.expect("begin entry update tx");
        let error = update_entry_status_on_pg_tx(
            &mut tx,
            "gate-entry",
            ENTRY_STATUS_DISPATCHED,
            "test_tx_gate",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect_err("cancelled run must reject tx-scoped attach");
        assert!(error.contains("gate-run is cancelled"), "{error}");
        tx.rollback().await.expect("rollback rejected tx attach");
        assert_eq!(entry_status(&pool).await, ENTRY_STATUS_PENDING);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancelled_run_rejects_dispatched_pool_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "cancelled", ENTRY_STATUS_PENDING).await;

        let error = update_entry_status_on_pg(
            &pool,
            "gate-entry",
            ENTRY_STATUS_DISPATCHED,
            "test_pool_gate",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect_err("cancelled run must reject pool-scoped attach");
        assert!(error.contains("gate-run is cancelled"), "{error}");
        assert_eq!(entry_status(&pool).await, ENTRY_STATUS_PENDING);

        pool.close().await;
        pg_db.drop().await;
    }

    async fn assert_terminal_transition_does_not_wait(initial_status: &str, terminal_status: &str) {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "cancelled", initial_status).await;
        let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            update_entry_status_on_pg(
                &pool,
                "gate-entry",
                terminal_status,
                "test_terminal_arm",
                &EntryStatusUpdateOptions::default(),
            ),
        )
        .await
        .expect("terminal arm must not wait for the dispatched gate")
        .expect("terminalize cancelled-run entry");
        assert!(result.changed);
        assert_eq!(entry_status(&pool).await, terminal_status);
        sqlx::query("COMMIT")
            .execute(&mut token_holder)
            .await
            .expect("release gate run token");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn cancelled_run_allows_skipped_without_waiting_for_gate_pg() {
        assert_terminal_transition_does_not_wait(ENTRY_STATUS_PENDING, ENTRY_STATUS_SKIPPED).await;
    }

    #[tokio::test]
    async fn cancelled_run_allows_failed_without_waiting_for_gate_pg() {
        assert_terminal_transition_does_not_wait(ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_FAILED)
            .await;
    }

    #[tokio::test]
    async fn cancelled_run_allows_cancelled_without_waiting_for_gate_pg() {
        assert_terminal_transition_does_not_wait(ENTRY_STATUS_PENDING, ENTRY_STATUS_USER_CANCELLED)
            .await;
    }

    #[tokio::test]
    async fn cancelled_run_allows_done_without_waiting_for_gate_pg() {
        assert_terminal_transition_does_not_wait(ENTRY_STATUS_PENDING, ENTRY_STATUS_DONE).await;
    }

    async fn assert_live_run_allows_dispatched_transition(run_status: &str, dispatch_id: &str) {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, run_status, ENTRY_STATUS_PENDING).await;
        seed_dispatch(&pool, dispatch_id).await;

        let result = update_entry_status_on_pg(
            &pool,
            "gate-entry",
            ENTRY_STATUS_DISPATCHED,
            "test_live_gate",
            &EntryStatusUpdateOptions {
                dispatch_id: Some(dispatch_id.to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("live run remains eligible for attachment");
        assert!(result.changed);
        assert_eq!(entry_status(&pool).await, ENTRY_STATUS_DISPATCHED);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn active_run_allows_dispatched_transition_pg() {
        assert_live_run_allows_dispatched_transition("active", "dispatch-active").await;
    }

    #[tokio::test]
    async fn paused_run_allows_dispatched_transition_pg() {
        assert_live_run_allows_dispatched_transition("paused", "dispatch-paused").await;
    }

    #[tokio::test]
    async fn restoring_run_allows_dispatched_transition_pg() {
        assert_live_run_allows_dispatched_transition("restoring", "dispatch-restoring").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_status_retry_rejects_competing_dispatch_identity_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
        seed_dispatch(&pool, "dispatch-requested-status-update").await;
        seed_dispatch(&pool, "dispatch-competing-status-update").await;
        let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

        let update_pool = pool.clone();
        let update = tokio::spawn(async move {
            update_entry_status_on_pg(
                &update_pool,
                "gate-entry",
                ENTRY_STATUS_DISPATCHED,
                "test_pool_status_retry",
                &EntryStatusUpdateOptions {
                    dispatch_id: Some("dispatch-requested-status-update".to_string()),
                    slot_index: Some(0),
                },
            )
            .await
        });
        wait_for_run_token_waiter(&mut token_holder).await;
        sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'dispatched',
                 dispatch_id = 'dispatch-competing-status-update',
                 slot_index = 1,
                 completed_at = NULL
             WHERE id = 'gate-entry'",
        )
        .execute(&mut token_holder)
        .await
        .expect("attach competing dispatch during pool status update");
        sqlx::query("COMMIT")
            .execute(&mut token_holder)
            .await
            .expect("commit competing dispatch and release run token");

        let error = tokio::time::timeout(std::time::Duration::from_secs(5), update)
            .await
            .expect("pool status update finishes after competing attachment")
            .expect("join pool status update")
            .expect_err("pool status retry must reject a competing dispatch identity");
        assert!(
            error.contains("dispatch identity changed during status update"),
            "{error}"
        );
        let identity = load_entry_status_row_pg(&pool, "gate-entry")
            .await
            .expect("load preserved competing dispatch identity");
        assert_eq!(
            identity.dispatch_id.as_deref(),
            Some("dispatch-competing-status-update")
        );
        assert_eq!(identity.slot_index, Some(1));

        drop(token_holder);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_status_retry_distinguishes_null_and_empty_dispatch_identity_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
        let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

        let update_pool = pool.clone();
        let update = tokio::spawn(async move {
            update_entry_status_on_pg(
                &update_pool,
                "gate-entry",
                ENTRY_STATUS_DISPATCHED,
                "test_pool_status_retry",
                &EntryStatusUpdateOptions::default(),
            )
            .await
        });
        wait_for_run_token_waiter(&mut token_holder).await;
        sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'dispatched',
                 dispatch_id = '',
                 completed_at = NULL
             WHERE id = 'gate-entry'",
        )
        .execute(&mut token_holder)
        .await
        .expect("replace NULL dispatch identity with empty text");
        sqlx::query("COMMIT")
            .execute(&mut token_holder)
            .await
            .expect("commit empty dispatch identity and release run token");

        let error = tokio::time::timeout(std::time::Duration::from_secs(5), update)
            .await
            .expect("pool status update finishes after empty identity wins")
            .expect("join pool status update")
            .expect_err("pool status retry must not accept empty text as the pinned NULL identity");
        assert!(
            error.contains("dispatch identity changed during status update"),
            "{error}"
        );
        let identity = load_entry_status_row_pg(&pool, "gate-entry")
            .await
            .expect("load preserved empty dispatch identity");
        assert_eq!(identity.dispatch_id.as_deref(), Some(""));

        drop(token_holder);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_status_retry_absorbs_competing_status_change_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
        seed_dispatch(&pool, "dispatch-requested-status-race").await;
        let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

        let update_pool = pool.clone();
        let update = tokio::spawn(async move {
            update_entry_status_on_pg(
                &update_pool,
                "gate-entry",
                ENTRY_STATUS_DISPATCHED,
                "test_pool_status_retry",
                &EntryStatusUpdateOptions {
                    dispatch_id: Some("dispatch-requested-status-race".to_string()),
                    slot_index: Some(0),
                },
            )
            .await
        });
        wait_for_run_token_waiter(&mut token_holder).await;
        // Only the status moves; the row keeps the NULL dispatch identity this
        // attempt observed, so the retry must converge instead of aborting.
        sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'skipped',
                 completed_at = NOW()
             WHERE id = 'gate-entry'",
        )
        .execute(&mut token_holder)
        .await
        .expect("race the status only during pool status update");
        sqlx::query("COMMIT")
            .execute(&mut token_holder)
            .await
            .expect("commit racing status and release run token");

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), update)
            .await
            .expect("pool status update finishes after the racing status commits")
            .expect("join pool status update")
            .expect("pool status retry must absorb a pure status race");
        assert!(result.changed);
        assert_eq!(result.from_status, ENTRY_STATUS_SKIPPED);
        let identity = load_entry_status_row_pg(&pool, "gate-entry")
            .await
            .expect("load attached dispatch identity");
        assert_eq!(identity.status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(
            identity.dispatch_id.as_deref(),
            Some("dispatch-requested-status-race")
        );
        assert_eq!(identity.slot_index, Some(0));

        drop(token_holder);
        pool.close().await;
        pg_db.drop().await;
    }

    struct RacedDoneAttachment {
        result: Result<EntryStatusUpdateResult, String>,
        row: EntryStatusRow,
        transitions: Vec<(String, String, String)>,
        dispatch_history: Vec<String>,
    }

    /// Stage the reattachment race #5356 S2 opened: a new attachment is in
    /// flight when a competitor moves the entry to `done` without touching the
    /// dispatch identity the attempt observed. Judging drift against the write
    /// target aborted this; judging it against the observed pre-state lets the
    /// retry converge from `done`.
    async fn race_entry_to_done_during_new_attachment(
        trigger_source: &str,
        requested_dispatch_status: &str,
    ) -> RacedDoneAttachment {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
        seed_dispatch(&pool, "dispatch-requested-done-race").await;
        sqlx::query(
            "UPDATE task_dispatches
             SET status = $1, updated_at = NOW()
             WHERE id = 'dispatch-requested-done-race'",
        )
        .bind(requested_dispatch_status)
        .execute(&pool)
        .await
        .expect("seed requested dispatch status");
        let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

        let trigger_source = trigger_source.to_string();
        let update_pool = pool.clone();
        let update = tokio::spawn(async move {
            update_entry_status_on_pg(
                &update_pool,
                "gate-entry",
                ENTRY_STATUS_DISPATCHED,
                &trigger_source,
                &EntryStatusUpdateOptions {
                    dispatch_id: Some("dispatch-requested-done-race".to_string()),
                    slot_index: Some(0),
                },
            )
            .await
        });
        // Waiting here is the run-token gate itself: the dispatched arm cannot
        // reach its CAS until this holder commits, so the racing `done` is
        // guaranteed to land between the attempt's reload and its write.
        wait_for_run_token_waiter(&mut token_holder).await;
        sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'done',
                 completed_at = NOW()
             WHERE id = 'gate-entry'",
        )
        .execute(&mut token_holder)
        .await
        .expect("race the entry to done during the new attachment");
        sqlx::query("COMMIT")
            .execute(&mut token_holder)
            .await
            .expect("commit racing done status and release run token");

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), update)
            .await
            .expect("pool status update finishes after the racing done commits")
            .expect("join pool status update");
        let row = load_entry_status_row_pg(&pool, "gate-entry")
            .await
            .expect("load entry state after the racing done");
        let transitions = sqlx::query_as::<_, (String, String, String)>(
            "SELECT from_status, to_status, trigger_source
             FROM auto_queue_entry_transitions
             WHERE entry_id = 'gate-entry'
             ORDER BY id ASC",
        )
        .fetch_all(&pool)
        .await
        .expect("load transitions recorded during the reattachment race");
        let dispatch_history = list_entry_dispatch_history_pg(&pool, "gate-entry")
            .await
            .expect("load dispatch history recorded during the reattachment race");

        drop(token_holder);
        pool.close().await;
        pg_db.drop().await;
        RacedDoneAttachment {
            result,
            row,
            transitions,
            dispatch_history,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_status_retry_reattaches_after_racing_done_pg() {
        let reattached = race_entry_to_done_during_new_attachment("pmd_reopen", "dispatched").await;
        let result = reattached
            .result
            .expect("pool status retry must converge onto the raced done row");
        assert!(result.changed);
        assert_eq!(result.from_status, ENTRY_STATUS_DONE);
        assert_eq!(reattached.row.status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(
            reattached.row.dispatch_id.as_deref(),
            Some("dispatch-requested-done-race")
        );
        assert_eq!(reattached.row.slot_index, Some(0));
        assert!(reattached.row.completed_at.is_none());
        // Convergence runs through the canonical bookkeeping, not around it.
        assert_eq!(
            reattached.transitions,
            vec![(
                ENTRY_STATUS_DONE.to_string(),
                ENTRY_STATUS_DISPATCHED.to_string(),
                "pmd_reopen".to_string()
            )]
        );
        assert_eq!(
            reattached.dispatch_history,
            vec!["dispatch-requested-done-race".to_string()]
        );

        // The approved-source gate is real: the same race under a source the
        // done -> dispatched transition table does not name is refused, and the
        // competitor's row is left untouched.
        let unapproved =
            race_entry_to_done_during_new_attachment("test_pool_status_retry", "dispatched").await;
        let error = unapproved
            .result
            .expect_err("an unapproved source must not reattach a raced done entry");
        assert!(
            error
                .contains("invalid auto-queue entry transition for gate-entry: done -> dispatched"),
            "{error}"
        );
        assert_eq!(unapproved.row.status, ENTRY_STATUS_DONE);
        assert_eq!(unapproved.row.dispatch_id, None);
        assert_eq!(unapproved.row.slot_index, None);
        assert!(unapproved.transitions.is_empty());
        assert!(unapproved.dispatch_history.is_empty());

        // The attachment guard is real too. It runs before the CAS, so this
        // leg is refused on the first attempt rather than at the retry — which
        // is what shows the converging leg above was granted by a live
        // dispatch, not by an absent liveness check.
        let terminal_dispatch =
            race_entry_to_done_during_new_attachment("pmd_reopen", "failed").await;
        let error = terminal_dispatch
            .result
            .expect_err("a terminal dispatch must not be attached to a raced done entry");
        assert!(error.contains("is failed: refusing to attach"), "{error}");
        assert_eq!(terminal_dispatch.row.status, ENTRY_STATUS_DONE);
        assert_eq!(terminal_dispatch.row.dispatch_id, None);
        assert!(terminal_dispatch.transitions.is_empty());
        assert!(terminal_dispatch.dispatch_history.is_empty());
    }

    fn pool_status_update_body(source: &str) -> &str {
        source
            .split_once("pub async fn update_entry_status_on_pg(")
            .expect("module must declare update_entry_status_on_pg")
            .1
            .split_once("\n}\n")
            .expect("update_entry_status_on_pg must close at column zero")
            .0
    }

    fn pins_declared_before_retry_loop(source: &str) -> bool {
        let body = pool_status_update_body(source);
        let Some(retry_loop) = body.find("\n    loop {") else {
            return false;
        };
        let declarations: Vec<_> = body.match_indices("let pinned_").collect();
        declarations.len() == 2 && declarations.iter().all(|(offset, _)| *offset < retry_loop)
    }

    #[test]
    fn pinned_dispatch_identity_is_declared_before_the_stale_retry_loop() {
        assert!(
            pins_declared_before_retry_loop(MODULE_SOURCE),
            "both pinned_* bindings must be declared before the stale-retry loop"
        );

        // The shape this lock rejects: the pin recomputed per iteration. It is
        // behaviorally indistinguishable from the hoist, because the drift
        // branch only lets an iteration `continue` while the reloaded identity
        // still equals the observed one — so the recomputed value would always
        // match. The declaration site is therefore the only checkable surface.
        const RECOMPUTED_INSIDE_LOOP: &str = "\
pub async fn update_entry_status_on_pg(
) -> Result<EntryStatusUpdateResult, String> {
    let mut current = load_entry_status_row_pg(pool, entry_id).await?;
    loop {
        let pinned_dispatch_id = options.dispatch_id.clone();
        let pinned_slot_index = options.slot_index;
    }
}
";
        assert!(
            !pins_declared_before_retry_loop(RECOMPUTED_INSIDE_LOOP),
            "the lock must reject pins recomputed inside the retry loop"
        );
    }

    #[tokio::test]
    async fn reactivate_completed_run_waits_for_token_and_records_history_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "completed", ENTRY_STATUS_DONE).await;
        seed_dispatch(&pool, "dispatch-reactivated").await;
        let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

        let reactivate_pool = pool.clone();
        let reactivation = tokio::spawn(async move {
            reactivate_done_entry_on_pg(
                &reactivate_pool,
                "gate-entry",
                "pmd_reopen",
                &EntryStatusUpdateOptions {
                    dispatch_id: Some("dispatch-reactivated".to_string()),
                    slot_index: Some(0),
                },
            )
            .await
        });
        wait_for_run_token_waiter(&mut token_holder).await;
        sqlx::query("COMMIT")
            .execute(&mut token_holder)
            .await
            .expect("release gate run token");

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), reactivation)
            .await
            .expect("reactivation completes after token release")
            .expect("join reactivation task")
            .expect("reactivate completed entry");
        assert!(result.changed);
        assert_eq!(entry_status(&pool).await, ENTRY_STATUS_DISPATCHED);
        assert_eq!(run_status(&pool).await, "active");
        assert_eq!(
            list_entry_dispatch_history_pg(&pool, "gate-entry")
                .await
                .expect("load history-only ownership evidence"),
            vec!["dispatch-reactivated".to_string()]
        );
        let transition = sqlx::query_as::<_, (String, String, String)>(
            "SELECT from_status, to_status, trigger_source
             FROM auto_queue_entry_transitions
             WHERE entry_id = 'gate-entry'",
        )
        .fetch_one(&pool)
        .await
        .expect("load canonical done-entry reactivation transition");
        assert_eq!(
            transition,
            (
                ENTRY_STATUS_DONE.to_string(),
                ENTRY_STATUS_DISPATCHED.to_string(),
                "pmd_reopen".to_string()
            )
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reactivate_reentry_rejects_competing_dispatch_identity_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "completed", ENTRY_STATUS_DONE).await;
        seed_dispatch(&pool, "dispatch-requested-reactivation").await;
        seed_dispatch(&pool, "dispatch-competing-reactivation").await;
        let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

        let reactivate_pool = pool.clone();
        let reactivation = tokio::spawn(async move {
            reactivate_done_entry_on_pg(
                &reactivate_pool,
                "gate-entry",
                "pmd_reopen",
                &EntryStatusUpdateOptions {
                    dispatch_id: Some("dispatch-requested-reactivation".to_string()),
                    slot_index: Some(0),
                },
            )
            .await
        });
        wait_for_run_token_waiter(&mut token_holder).await;
        sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'active', completed_at = NULL
             WHERE id = 'gate-run'",
        )
        .execute(&mut token_holder)
        .await
        .expect("revive run in competing reactivator");
        sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'dispatched',
                 dispatch_id = 'dispatch-competing-reactivation',
                 slot_index = 0,
                 completed_at = NULL
             WHERE id = 'gate-entry'",
        )
        .execute(&mut token_holder)
        .await
        .expect("attach competing dispatch while owning the run token");
        sqlx::query("COMMIT")
            .execute(&mut token_holder)
            .await
            .expect("commit competing reactivation and release token");

        let error = tokio::time::timeout(std::time::Duration::from_secs(5), reactivation)
            .await
            .expect("reactivation finishes after competing attachment")
            .expect("join competing reactivation task")
            .expect_err("a different dispatch identity must reject reactivation reentry");
        assert!(error.contains("dispatch identity"), "{error}");
        let identity = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'gate-entry'",
        )
        .fetch_one(&pool)
        .await
        .expect("load preserved competing entry identity");
        assert_eq!(
            identity,
            (
                ENTRY_STATUS_DISPATCHED.to_string(),
                Some("dispatch-competing-reactivation".to_string()),
                Some(0)
            )
        );
        assert_eq!(transition_count(&pool).await, 0);

        drop(token_holder);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn reactivate_done_entry_rejects_unapproved_transition_source_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "completed", ENTRY_STATUS_DONE).await;
        seed_dispatch(&pool, "dispatch-invalid-reactivation").await;

        let error = reactivate_done_entry_on_pg(
            &pool,
            "gate-entry",
            "unapproved_reactivation",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-invalid-reactivation".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect_err("canonical transition validation must reject an unapproved done reactivation");
        assert!(
            error.contains("invalid auto-queue entry transition"),
            "{error}"
        );
        assert_eq!(entry_status(&pool).await, ENTRY_STATUS_DONE);
        assert_eq!(run_status(&pool).await, "completed");
        assert_eq!(transition_count(&pool).await, 0);
        assert!(
            list_entry_dispatch_history_pg(&pool, "gate-entry")
                .await
                .expect("load rejected reactivation history")
                .is_empty()
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn failed_dispatch_is_rechecked_before_late_attachment_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
        seed_dispatch(&pool, "dispatch-failed-before-attach").await;
        sqlx::query(
            "UPDATE auto_queue_entries
             SET dispatch_id = 'dispatch-failed-before-attach'
             WHERE id = 'gate-entry'",
        )
        .execute(&pool)
        .await
        .expect("seed stale pending-entry dispatch pointer");
        sqlx::query(
            "UPDATE task_dispatches
             SET status = 'failed', updated_at = NOW()
             WHERE id = 'dispatch-failed-before-attach'",
        )
        .execute(&pool)
        .await
        .expect("terminalize dispatch before late attachment");

        let error = update_entry_status_on_pg(
            &pool,
            "gate-entry",
            ENTRY_STATUS_DISPATCHED,
            "restore_run_attach_existing_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-failed-before-attach".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect_err("failed dispatch must not be attached after the restore-path observation");
        assert!(error.contains("is failed: refusing to attach"), "{error}");
        assert_eq!(entry_status(&pool).await, ENTRY_STATUS_PENDING);
        assert_eq!(transition_count(&pool).await, 0);
        assert!(
            list_entry_dispatch_history_pg(&pool, "gate-entry")
                .await
                .expect("load rejected late-attach history")
                .is_empty()
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn terminal_dispatch_commit_blocks_and_rejects_late_attachment_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "active", ENTRY_STATUS_PENDING).await;
        seed_dispatch(&pool, "dispatch-terminalizing-during-attach").await;

        let mut terminalizer = pool.acquire().await.expect("acquire terminalizer");
        sqlx::query("BEGIN")
            .execute(&mut *terminalizer)
            .await
            .expect("begin terminalizer");
        sqlx::query(
            "UPDATE task_dispatches
             SET status = 'failed', updated_at = NOW()
             WHERE id = 'dispatch-terminalizing-during-attach'",
        )
        .execute(&mut *terminalizer)
        .await
        .expect("terminalize dispatch without committing");

        let attach_pool = pool.clone();
        let attachment = tokio::spawn(async move {
            update_entry_status_on_pg(
                &attach_pool,
                "gate-entry",
                ENTRY_STATUS_DISPATCHED,
                "restore_run_attach_existing_dispatch",
                &EntryStatusUpdateOptions {
                    dispatch_id: Some("dispatch-terminalizing-during-attach".to_string()),
                    slot_index: Some(0),
                },
            )
            .await
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            sqlx::query("SELECT pg_stat_clear_snapshot()")
                .execute(&mut *terminalizer)
                .await
                .expect("clear late-attach waiter snapshot");
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1
                     FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND wait_event_type = 'Lock'
                       AND query LIKE '%FOR SHARE%'
                 )",
            )
            .fetch_one(&mut *terminalizer)
            .await
            .expect("inspect dispatch FOR SHARE waiter");
            if waiting {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "late attachment did not wait on the terminal dispatch update"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        sqlx::query("COMMIT")
            .execute(&mut *terminalizer)
            .await
            .expect("commit terminal dispatch update");

        let error = tokio::time::timeout(std::time::Duration::from_secs(5), attachment)
            .await
            .expect("late attachment finishes after terminal commit")
            .expect("join late attachment")
            .expect_err("committed terminal dispatch must reject late attachment");
        assert!(error.contains("is failed: refusing to attach"), "{error}");
        assert_eq!(entry_status(&pool).await, ENTRY_STATUS_PENDING);
        assert_eq!(transition_count(&pool).await, 0);

        drop(terminalizer);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn reactivate_rechecks_cancelled_run_after_waiting_for_token_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        setup_entry(&pool, "completed", ENTRY_STATUS_DONE).await;
        seed_dispatch(&pool, "dispatch-rejected-reactivation").await;
        let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;

        let reactivate_pool = pool.clone();
        let reactivation = tokio::spawn(async move {
            reactivate_done_entry_on_pg(
                &reactivate_pool,
                "gate-entry",
                "pmd_reopen",
                &EntryStatusUpdateOptions {
                    dispatch_id: Some("dispatch-rejected-reactivation".to_string()),
                    slot_index: Some(0),
                },
            )
            .await
        });
        wait_for_run_token_waiter(&mut token_holder).await;
        sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'cancelled', completed_at = NOW()
             WHERE id = 'gate-run'",
        )
        .execute(&mut token_holder)
        .await
        .expect("cancel run while owning its token");
        sqlx::query("COMMIT")
            .execute(&mut token_holder)
            .await
            .expect("commit run cancellation and release token");

        let error = tokio::time::timeout(std::time::Duration::from_secs(5), reactivation)
            .await
            .expect("reactivation finishes after cancellation commits")
            .expect("join rejected reactivation task")
            .expect_err("cancelled run must reject done-entry reactivation");
        assert!(error.contains("gate-run is cancelled"), "{error}");
        assert_eq!(entry_status(&pool).await, ENTRY_STATUS_DONE);
        assert_eq!(run_status(&pool).await, "cancelled");
        assert!(
            list_entry_dispatch_history_pg(&pool, "gate-entry")
                .await
                .expect("load rejected reactivation history")
                .is_empty()
        );
        assert_eq!(transition_count(&pool).await, 0);

        pool.close().await;
        pg_db.drop().await;
    }
}
