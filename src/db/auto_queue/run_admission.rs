//! Admission for writers that add work or a completion blocker to an existing run: each takes
//! `aq_run:<run_id>` first and then reads the run status in its own statement.

use sqlx::{Postgres, Transaction};

use super::entries::{
    ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_FAILED, ENTRY_STATUS_PENDING, ENTRY_STATUS_SKIPPED,
};
use super::run_status::is_live_run_status;
use super::runs::acquire_run_advisory_xact_lock_on_pg_tx;

/// Runs that may still gain a pending/failed gate or a reopened entry. Only `completed` (and a
/// missing row) refuses, so the operator steps between cancel and restore keep working.
pub(crate) const BLOCKER_ADMITTING_RUN_STATUSES: &[&str] = &[
    "active",
    "paused",
    "restoring",
    "generated",
    "pending",
    "cancelled",
];

/// Hold the run token for the rest of the caller's transaction without reading the run.
pub(crate) async fn lock_run_token_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
) -> Result<(), String> {
    acquire_run_advisory_xact_lock_on_pg_tx(tx, run_id).await
}

/// Token first, then a separate unlocked read: a read in the lock statement keeps its pre-wait
/// snapshot, and a row lock here deadlocks with gate reconciliation (phase key, then run row).
async fn run_status_under_token_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
    purpose: &str,
) -> Result<Option<String>, String> {
    acquire_run_advisory_xact_lock_on_pg_tx(tx, run_id).await?;
    sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("reload auto-queue run {run_id} {purpose}: {error}"))
}

/// Admit a writer only while its run is in `allowed`; otherwise refuse with
/// `run is <status>: refusing to <action>`, naming a missing row as `missing`.
pub(crate) async fn admit_run_writer_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
    allowed: &[&str],
    action: &str,
) -> Result<(), String> {
    let purpose = format!("before it may {action}");
    match run_status_under_token_on_pg_tx(tx, run_id, &purpose).await? {
        Some(status) if allowed.contains(&status.trim()) => Ok(()),
        status => Err(format!(
            "run is {}: refusing to {action} (run {run_id})",
            status.as_deref().unwrap_or("missing")
        )),
    }
}

/// Serializes a live attachment with terminal run writers, then derives the
/// run predicate from a fresh READ COMMITTED statement under that token.
///
/// Call sites invoke this only for the dispatched arm, before its entry write.
/// That arm does not call `maybe_finalize_run_if_ready_pg`; its non-blocking
/// finalizer protocol and the participant lock-order summary are documented by
/// that symbol in `runs`.
pub(super) async fn gate_dispatched_entry_run_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let run_status = run_status_under_token_on_pg_tx(tx, run_id, "before entry dispatch")
        .await?
        .ok_or_else(|| format!("auto-queue run not found before entry dispatch: {run_id}"))?;

    if !is_live_run_status(&run_status) {
        return Err(format!(
            "auto-queue run {run_id} is {run_status}: refusing to dispatch an entry"
        ));
    }

    Ok(())
}

/// Gate an entry write before it touches the row: dispatched needs a live run, and failed/skipped
/// reopened to `pending` is a new blocker. Other pending arms keep a blocker and add no wait.
pub(super) async fn admit_entry_transition_on_pg_tx(
    tx: &mut Transaction<'_, Postgres>,
    run_id: &str,
    entry_id: &str,
    from_status: &str,
    to_status: &str,
) -> Result<(), String> {
    if to_status == ENTRY_STATUS_DISPATCHED {
        return gate_dispatched_entry_run_on_pg_tx(tx, run_id).await;
    }
    if to_status == ENTRY_STATUS_PENDING
        && matches!(from_status, ENTRY_STATUS_FAILED | ENTRY_STATUS_SKIPPED)
    {
        let action = format!("reopen auto-queue entry {entry_id}");
        return admit_run_writer_on_pg_tx(tx, run_id, BLOCKER_ADMITTING_RUN_STATUSES, &action)
            .await;
    }
    Ok(())
}
