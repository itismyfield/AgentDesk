//! Durable replay for the post-commit half of auto-queue run cancel/end (#5142).
//!
//! Both `cancel_live_dispatches_for_runs_pg` and `terminalize_selected_runs_with_pg`
//! commit the dispatch/run state change first and then owe four more steps:
//! observability emit, wait-queue wake, provider session clear, and slot release
//! (plus slot-thread clearing). Those steps used to live only on the caller's
//! stack, so a crash right after the commit left the cancel durable while the
//! slot token and provider session id survived, and a failing session clear only
//! appended a warning string.
//!
//! The fix is a transactional outbox. `enqueue_run_cleanup_task_on_tx` inserts a
//! row into `auto_queue_run_cleanup_tasks` inside the very transaction that
//! commits the state change, so "cleanup is owed" becomes durable at the same
//! instant. `drain_run_cleanup_task_pg` runs the steps and deletes the row only
//! when all of them succeeded; anything else leaves the row behind with
//! `attempts`/`last_error` set. `replay_pending_run_cleanup_tasks_pg` is what a
//! restarted process calls to pick the leftovers back up.
//!
//! Retry safety of each replayed step is analysed at its call site below.
//!
//! ## Delivery guarantee — read this before claiming "nothing is lost"
//!
//! This machinery makes the *database-visible* cleanup (provider session ids,
//! slot tokens, slot-thread sessions) crash-safe: every one of those steps is
//! re-derived from the durable row and retried until it succeeds, so they are
//! at-least-once and converge.
//!
//! The observability emit in step 1 is **not** covered by that guarantee and is
//! deliberately at-most-once. `CancelTransitionMeta::emit` hands the event to an
//! in-process worker channel and discards the result
//! (`observability/emit.rs`: `if let Some(sender) = worker_sender() { let _ =
//! sender.send(..) }`), so the event is silently dropped when the worker is not
//! running, when the channel send fails, or when the process dies before the
//! worker flushes its queue to PostgreSQL. Because `emitted = TRUE` is committed
//! straight after the send, a replay will never re-fire it. Losing an
//! observability row is the accepted trade for never double-counting one; it is
//! not a claim that no emit is ever lost.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use super::AutoQueueLogContext;
use super::cancel_run::{SlotCleanupResult, clear_sessions_for_dispatches_pg};
use crate::dispatch::CancelTransitionMeta;

/// A slot this task released (or found already released) and therefore still
/// owes a slot-thread clear for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReleasedSlot {
    pub(crate) agent_id: String,
    pub(crate) slot_index: i64,
}

/// One durable unit of post-commit cleanup.
pub(crate) struct RunCleanupTask {
    pub(crate) id: i64,
    pub(crate) run_ids: Vec<String>,
    pub(crate) dispatch_ids: Vec<String>,
    pub(crate) released_slots: Vec<ReleasedSlot>,
    pub(crate) pending_emits: Vec<CancelTransitionMeta>,
    pub(crate) emitted: bool,
}

/// Outcome of draining one task.
#[derive(Debug, Default)]
pub(crate) struct RunCleanupDrainOutcome {
    pub(crate) slot_cleanup: SlotCleanupResult,
    /// `false` when the row was deliberately left behind for a later retry.
    pub(crate) completed: bool,
}

/// Columns every drain path selects. Kept in one place so the batch sweep and
/// the single-row claim cannot drift apart.
const TASK_COLUMNS: &str = "id, run_ids, dispatch_ids, released_slots, pending_emits, emitted";

/// Rows drained per replay sweep.
const REPLAY_BATCH_LIMIT: i64 = 50;

/// Attempts a task gets before it is dead-lettered.
///
/// Without a cap a permanently failing row keeps the oldest `created_at` and
/// therefore the head of the drain order forever, so `REPLAY_BATCH_LIMIT`
/// unfixable rows would stop every newly queued cleanup from ever draining.
const MAX_CLEANUP_ATTEMPTS: i32 = 10;

/// Upper bound on the exponential retry delay, in seconds.
const MAX_BACKOFF_SECONDS: i64 = 300;

/// How long a claim is honoured before another drainer may steal the row.
/// A process that dies mid-drain must not strand its claim permanently.
const CLAIM_LEASE_SECONDS: i64 = 300;

/// Identifies the claim holder in `claim_owner`. Only used for diagnostics —
/// correctness comes from the `FOR UPDATE SKIP LOCKED` claim itself.
fn claim_owner_tag() -> String {
    format!("pid:{}", std::process::id())
}

/// Insert the durable cleanup record. MUST be called on the same transaction
/// that commits the dispatch cancel / run terminalization, otherwise the record
/// and the state change can diverge.
///
/// "Same transaction" is the entire P0 argument: if this INSERT were moved to a
/// transaction of its own after the commit, a crash in between would leave the
/// cancel durable with no record that cleanup is owed, which is precisely the
/// defect this module exists to remove. `enqueue_is_atomic_with_the_state_change_pg`
/// pins that by failing the INSERT and asserting the state change rolls back
/// with it.
pub(crate) async fn enqueue_run_cleanup_task_on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_ids: &[String],
    dispatch_ids: &[String],
    released_slots: &[ReleasedSlot],
    pending_emits: &[CancelTransitionMeta],
) -> Result<i64, String> {
    let released_json = serde_json::to_value(released_slots)
        .map_err(|error| format!("serialize auto-queue cleanup released slots: {error}"))?;
    let emits_json = serde_json::to_value(pending_emits)
        .map_err(|error| format!("serialize auto-queue cleanup pending emits: {error}"))?;
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO auto_queue_run_cleanup_tasks
            (run_ids, dispatch_ids, released_slots, pending_emits)
         VALUES ($1, $2, $3, $4)
         RETURNING id",
    )
    .bind(run_ids)
    .bind(dispatch_ids)
    .bind(released_json)
    .bind(emits_json)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("enqueue auto-queue run cleanup task: {error}"))
}

fn task_from_row(row: &sqlx::postgres::PgRow) -> Result<RunCleanupTask, String> {
    let released_slots: serde_json::Value = row
        .try_get("released_slots")
        .map_err(|error| format!("decode auto-queue cleanup released slots: {error}"))?;
    let pending_emits: serde_json::Value = row
        .try_get("pending_emits")
        .map_err(|error| format!("decode auto-queue cleanup pending emits: {error}"))?;
    Ok(RunCleanupTask {
        id: row
            .try_get("id")
            .map_err(|error| format!("decode auto-queue cleanup id: {error}"))?,
        run_ids: row
            .try_get("run_ids")
            .map_err(|error| format!("decode auto-queue cleanup run ids: {error}"))?,
        dispatch_ids: row
            .try_get("dispatch_ids")
            .map_err(|error| format!("decode auto-queue cleanup dispatch ids: {error}"))?,
        released_slots: serde_json::from_value(released_slots)
            .map_err(|error| format!("parse auto-queue cleanup released slots: {error}"))?,
        pending_emits: serde_json::from_value(pending_emits)
            .map_err(|error| format!("parse auto-queue cleanup pending emits: {error}"))?,
        emitted: row
            .try_get("emitted")
            .map_err(|error| format!("decode auto-queue cleanup emitted flag: {error}"))?,
    })
}

/// Number of cleanup rows still owed. Tests use it to prove convergence.
#[cfg(test)]
pub(crate) async fn pending_run_cleanup_task_count_pg(pool: &PgPool) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM auto_queue_run_cleanup_tasks")
        .fetch_one(pool)
        .await
        .map_err(|error| format!("count auto-queue run cleanup tasks: {error}"))
}

/// True when the row is still on disk, whatever its claim/backoff state.
///
/// Used to tell "another drainer already finished this task" (row gone) apart
/// from "another drainer currently owns it, or it is backing off" (row present
/// but unclaimable). Reporting the second case as `completed` would make the
/// replay statistics lie.
async fn run_cleanup_task_exists_pg(pool: &PgPool, id: i64) -> Result<bool, String> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM auto_queue_run_cleanup_tasks WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .map(|count| count > 0)
        .map_err(|error| format!("probe auto-queue run cleanup task {id}: {error}"))
}

/// Claim one specific row for this drainer.
///
/// The inline post-commit drain and the policy-tick replay sweep both target
/// live rows, so without a claim they can run the same task concurrently and
/// fire its observability emits twice. Claiming under `FOR UPDATE SKIP LOCKED`
/// makes exactly one of them the owner and lets the loser skip immediately
/// instead of blocking on a row lock.
async fn claim_run_cleanup_task_pg(
    pool: &PgPool,
    id: i64,
) -> Result<Option<RunCleanupTask>, String> {
    let sql = format!(
        "UPDATE auto_queue_run_cleanup_tasks AS t
         SET claim_owner = $2,
             claimed_at = NOW(),
             updated_at = NOW()
         FROM (
             SELECT id
             FROM auto_queue_run_cleanup_tasks
             WHERE id = $1
               AND dead_lettered_at IS NULL
               AND next_attempt_at <= NOW()
               AND (claimed_at IS NULL
                    OR claimed_at < NOW() - ($3::BIGINT * INTERVAL '1 second'))
             FOR UPDATE SKIP LOCKED
         ) AS c
         WHERE t.id = c.id
         RETURNING {}",
        TASK_COLUMNS
            .split(", ")
            .map(|column| format!("t.{column}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let row = sqlx::query(&sql)
        .bind(id)
        .bind(claim_owner_tag())
        .bind(CLAIM_LEASE_SECONDS)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("claim auto-queue run cleanup task {id}: {error}"))?;
    row.as_ref().map(task_from_row).transpose()
}

/// Claim up to `REPLAY_BATCH_LIMIT` drainable rows for the replay sweep.
///
/// Ordering by `next_attempt_at` first (not `created_at`) is what makes the
/// backoff effective: a row that just failed sorts to the back until its delay
/// elapses, so it stops occupying a batch slot that newer work needs.
async fn claim_run_cleanup_task_batch_pg(
    pool: &PgPool,
) -> Result<Vec<sqlx::postgres::PgRow>, String> {
    let sql = format!(
        "UPDATE auto_queue_run_cleanup_tasks AS t
         SET claim_owner = $1,
             claimed_at = NOW(),
             updated_at = NOW()
         FROM (
             SELECT id
             FROM auto_queue_run_cleanup_tasks
             WHERE dead_lettered_at IS NULL
               AND next_attempt_at <= NOW()
               AND (claimed_at IS NULL
                    OR claimed_at < NOW() - ($2::BIGINT * INTERVAL '1 second'))
             ORDER BY next_attempt_at ASC, created_at ASC, id ASC
             LIMIT $3
             FOR UPDATE SKIP LOCKED
         ) AS c
         WHERE t.id = c.id
         RETURNING {}",
        TASK_COLUMNS
            .split(", ")
            .map(|column| format!("t.{column}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    sqlx::query(&sql)
        .bind(claim_owner_tag())
        .bind(CLAIM_LEASE_SECONDS)
        .bind(REPLAY_BATCH_LIMIT)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("claim pending auto-queue run cleanup tasks: {error}"))
}

/// Record a failed attempt: bump `attempts`, apply exponential backoff, release
/// the claim, and dead-letter the row once it has burned through the cap.
///
/// The row is never deleted on dead-letter — the operator keeps the evidence and
/// `last_error` — but it drops out of the drain query so it can no longer block
/// the queue behind it.
async fn record_task_failure_pg(pool: &PgPool, id: i64, error: &str) {
    if let Err(update_error) = sqlx::query(
        "UPDATE auto_queue_run_cleanup_tasks
         SET attempts = attempts + 1,
             last_error = $2,
             next_attempt_at = NOW()
                 + (LEAST(
                        $3::BIGINT,
                        POWER(2::NUMERIC, LEAST(attempts + 1, 8))::BIGINT
                    ) * INTERVAL '1 second'),
             dead_lettered_at = CASE
                 WHEN attempts + 1 >= $4 THEN NOW()
                 ELSE dead_lettered_at
             END,
             claim_owner = NULL,
             claimed_at = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .bind(MAX_BACKOFF_SECONDS)
    .bind(MAX_CLEANUP_ATTEMPTS)
    .execute(pool)
    .await
    {
        tracing::warn!(
            task_id = id,
            error = %update_error,
            "[auto-queue] failed to record cleanup task retry state"
        );
    }
}

/// Dead-letter a row whose payload cannot be decoded at all.
///
/// A poison row can never succeed, so retrying it forever would starve the
/// queue; deleting it would destroy the only evidence of the corruption. It is
/// parked instead, and counted, so the sweep reports it rather than silently
/// skipping it.
async fn dead_letter_task_pg(pool: &PgPool, id: i64, error: &str) {
    if let Err(update_error) = sqlx::query(
        "UPDATE auto_queue_run_cleanup_tasks
         SET dead_lettered_at = NOW(),
             last_error = $2,
             claim_owner = NULL,
             claimed_at = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(pool)
    .await
    {
        tracing::warn!(
            task_id = id,
            error = %update_error,
            "[auto-queue] failed to dead-letter undecodable cleanup task"
        );
    }
}

/// Release every slot still held by this task's runs AND persist the resulting
/// set on the task row — in one transaction.
///
/// ## Why the single transaction is load-bearing (#5142 D-1)
///
/// These were two separate statements against the pool, so each committed on its
/// own. A crash in between left the slots released on disk with the task row
/// still carrying an empty `released_slots`. The replay then re-ran the slot
/// UPDATE, matched zero rows (the slots were already `NULL`), merged that into an
/// empty persisted set, found nothing to iterate in step 4, and **deleted the row
/// while reporting `completed`** — skipping the slot-thread clear, leaving the
/// residual provider session id behind, and destroying the retry evidence. That
/// is the exact defect this module was written to remove, one layer down.
///
/// Committing both writes together closes it: either the slots are still held and
/// the whole thing is retried from scratch, or they are released and the durable
/// row already names them.
///
/// Retry safety: the CAS predicate `assigned_run_id = ANY($1)` means a replay
/// that arrives after the slot was handed to a different run matches no row, so
/// the slot is never stolen back. A replay that arrives after this task already
/// released the slot also matches no row — which is exactly why the released set
/// is persisted in the same commit as the release.
async fn release_and_persist_slots_for_task_pg(
    pool: &PgPool,
    task: &RunCleanupTask,
) -> Result<(Vec<ReleasedSlot>, usize), String> {
    let mut tx = pool.begin().await.map_err(|error| {
        format!(
            "begin postgres slot release for cleanup task {}: {error}",
            task.id
        )
    })?;

    let rows = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id = ANY($1)
         RETURNING agent_id, slot_index",
    )
    .bind(&task.run_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| {
        format!(
            "release postgres slots for cleanup task {}: {error}",
            task.id
        )
    })?;

    let newly_released = rows.len();
    let mut merged = task.released_slots.clone();
    for row in rows {
        let agent_id = row
            .try_get::<String, _>("agent_id")
            .map_err(|error| format!("decode released slot agent: {error}"))?;
        let slot_index = row
            .try_get::<i64, _>("slot_index")
            .map_err(|error| format!("decode released slot index: {error}"))?;
        let slot = ReleasedSlot {
            agent_id,
            slot_index,
        };
        if !merged.contains(&slot) {
            merged.push(slot);
        }
    }

    if merged != task.released_slots {
        let payload = serde_json::to_value(&merged)
            .map_err(|error| format!("serialize auto-queue cleanup released slots: {error}"))?;
        sqlx::query(
            "UPDATE auto_queue_run_cleanup_tasks
             SET released_slots = $2,
                 updated_at = NOW()
             WHERE id = $1",
        )
        .bind(task.id)
        .bind(payload)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            format!(
                "persist auto-queue cleanup released slots for task {}: {error}",
                task.id
            )
        })?;
    }

    tx.commit().await.map_err(|error| {
        format!(
            "commit postgres slot release for cleanup task {}: {error}",
            task.id
        )
    })?;

    Ok((merged, newly_released))
}

/// True when the slot now belongs to a run outside this task.
///
/// Without this guard a late replay would clear the slot threads of whichever
/// run picked the slot up in the meantime — the A-B-A hazard that
/// `clear_slot_threads_for_slot_pg` cannot see, because it keys on
/// `(agent_id, slot_index)` and carries no run identity.
async fn slot_taken_by_foreign_run_pg(
    pool: &PgPool,
    run_ids: &[String],
    slot: &ReleasedSlot,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_slots
         WHERE agent_id = $1
           AND slot_index = $2
           AND assigned_run_id IS NOT NULL
           AND NOT (assigned_run_id = ANY($3))",
    )
    .bind(&slot.agent_id)
    .bind(slot.slot_index)
    .bind(run_ids)
    .fetch_one(pool)
    .await
    .map(|count| count > 0)
    .map_err(|error| {
        format!(
            "check slot ownership for {}:{}: {error}",
            slot.agent_id, slot.slot_index
        )
    })
}

/// Run every owed post-commit step for one task and delete the row when they all
/// succeed. A step that fails leaves the row in place, which is what keeps a
/// failed `clear_sessions_for_dispatches_pg` retry-eligible instead of reducing
/// it to a warning string.
///
/// The caller must already hold the row's claim.
pub(crate) async fn drain_run_cleanup_task_pg(
    health_registry: Option<std::sync::Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
    task: RunCleanupTask,
) -> RunCleanupDrainOutcome {
    let mut warnings = Vec::new();

    // Step 1 — observability emit + wait-queue wake.
    //
    // Retry safety: `emit()` appends observability rows and has no dedup key, so
    // it is NOT idempotent; the durable `emitted` flag is the idempotency key
    // that stops a replay from repeating it. See the module header for why that
    // makes the emit at-most-once rather than lossless.
    //
    // `spawn_cached_constraint_release_wake` ignores the dispatch id except for
    // logging (`wait_queue.rs:69`) and re-evaluates every waiting
    // `dispatch_outbox` row, so it is a reconciliation sweep: running it twice
    // re-reads rows the first sweep already cleared and needs no dedup key.
    if !task.emitted && !task.pending_emits.is_empty() {
        for meta in &task.pending_emits {
            meta.emit();
        }
        if let Err(error) = sqlx::query(
            "UPDATE auto_queue_run_cleanup_tasks
             SET emitted = TRUE,
                 updated_at = NOW()
             WHERE id = $1",
        )
        .bind(task.id)
        .execute(pool)
        .await
        {
            warnings.push(format!(
                "failed to mark auto-queue cleanup emits for task {}: {error}",
                task.id
            ));
        }
    }
    for meta in &task.pending_emits {
        crate::services::dispatches::wait_queue::spawn_cached_constraint_release_wake(
            pool.clone(),
            "constraint_release",
            meta.dispatch_id.clone(),
            "cancel_dispatch",
        );
    }

    // Step 2 — provider session clear.
    //
    // Retry safety: the UPDATE is scoped to `active_dispatch_id = $2` and to
    // non-terminal statuses, so a second run matches nothing and changes
    // nothing.
    //
    // #5142 D-3 — this step is a structural no-op on both production paths, and
    // that is deliberate rather than accidental. The transaction that cancels the
    // dispatch already runs `UPDATE sessions SET active_dispatch_id = NULL WHERE
    // active_dispatch_id = $2` (`dispatch_cancel.rs`), so by the time this
    // post-commit call runs, its `WHERE active_dispatch_id = ANY(..)` predicate
    // can never match. It is kept as the retry gate for the case where that
    // in-transaction clear is ever narrowed, and because a failure here (PG
    // unreachable) must still stop the drain before it releases slot tokens.
    // `session_clear_is_a_structural_no_op_after_the_cancel_commit_pg` pins the
    // zero-row fact so nobody mistakes it for the step that clears
    // `claude_session_id` — that is step 4, via the slot's threads.
    let cleared_dispatch_sessions = match clear_sessions_for_dispatches_pg(pool, &task.dispatch_ids)
        .await
    {
        Ok(cleared) => cleared,
        Err(error) => {
            crate::auto_queue_log!(
                warn,
                "run_cleanup_dispatch_session_clear_pg_failed",
                task.run_ids
                    .first()
                    .map(|run_id| AutoQueueLogContext::new().run(run_id))
                    .unwrap_or_default(),
                "[auto-queue] failed to clear postgres sessions for cleanup task {} dispatches {:?}: {}",
                task.id,
                task.dispatch_ids,
                error
            );
            record_task_failure_pg(pool, task.id, &error).await;
            warnings.push(format!(
                "failed to clear postgres sessions for run cleanup dispatches {:?}: {}",
                task.dispatch_ids, error
            ));
            return RunCleanupDrainOutcome {
                slot_cleanup: SlotCleanupResult {
                    released_slots: 0,
                    cleared_slot_sessions: 0,
                    warnings,
                },
                completed: false,
            };
        }
    };

    // Step 3 — slot release, committed together with the durable record of which
    // slots were released so a crash between the two is impossible.
    let (released_slots, newly_released) =
        match release_and_persist_slots_for_task_pg(pool, &task).await {
            Ok(value) => value,
            Err(error) => {
                record_task_failure_pg(pool, task.id, &error).await;
                warnings.push(error);
                return RunCleanupDrainOutcome {
                    slot_cleanup: SlotCleanupResult {
                        released_slots: 0,
                        cleared_slot_sessions: cleared_dispatch_sessions,
                        warnings,
                    },
                    completed: false,
                };
            }
        };

    // Step 4 — slot-thread clear, guarded against the A-B-A hazard above.
    //
    // Retry safety: `clear_slot_threads_for_slot_pg` resets sessions bound to the
    // slot's threads and is naturally repeatable, but only while the slot still
    // belongs to this task's runs.
    let mut cleared_slot_sessions = cleared_dispatch_sessions;
    let mut all_slots_handled = true;
    for slot in &released_slots {
        match slot_taken_by_foreign_run_pg(pool, &task.run_ids, slot).await {
            Ok(true) => {
                tracing::warn!(
                    agent_id = %slot.agent_id,
                    slot_index = slot.slot_index,
                    task_id = task.id,
                    "[auto-queue] skipping slot thread clear: slot already reassigned to another run"
                );
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                all_slots_handled = false;
                warnings.push(error);
                continue;
            }
        }
        match super::runtime::clear_slot_threads_for_slot_pg(
            health_registry.clone(),
            pool,
            &slot.agent_id,
            slot.slot_index,
        )
        .await
        {
            Ok(cleared) => cleared_slot_sessions += cleared,
            Err(error) => {
                all_slots_handled = false;
                crate::auto_queue_log!(
                    warn,
                    "clear_slot_threads_pg_failed",
                    AutoQueueLogContext::new().agent(&slot.agent_id),
                    "[auto-queue] failed to clear postgres slot thread sessions for {}:{}: {}",
                    slot.agent_id,
                    slot.slot_index,
                    error
                );
                warnings.push(format!(
                    "failed to clear slot thread sessions for {}:{}: {}",
                    slot.agent_id, slot.slot_index, error
                ));
            }
        }
    }

    let slot_cleanup = SlotCleanupResult {
        released_slots: newly_released,
        cleared_slot_sessions,
        warnings,
    };
    if !all_slots_handled {
        let summary = slot_cleanup.warnings.join("; ");
        record_task_failure_pg(pool, task.id, &summary).await;
        return RunCleanupDrainOutcome {
            slot_cleanup,
            completed: false,
        };
    }

    if let Err(error) = sqlx::query("DELETE FROM auto_queue_run_cleanup_tasks WHERE id = $1")
        .bind(task.id)
        .execute(pool)
        .await
    {
        tracing::warn!(
            task_id = task.id,
            error = %error,
            "[auto-queue] cleanup task finished but could not be deleted; a replay will repeat it"
        );
        return RunCleanupDrainOutcome {
            slot_cleanup,
            completed: false,
        };
    }

    RunCleanupDrainOutcome {
        slot_cleanup,
        completed: true,
    }
}

/// Claim and drain the task identified by `task_id`.
///
/// `completed` is reported honestly: `true` only when this call finished every
/// step, or when the row is already gone (another drainer finished it). A row
/// that exists but could not be claimed — someone else owns it, it is backing
/// off, or it is dead-lettered — is reported as *not* completed.
pub(crate) async fn drain_run_cleanup_task_by_id_pg(
    health_registry: Option<std::sync::Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
    task_id: i64,
) -> RunCleanupDrainOutcome {
    match claim_run_cleanup_task_pg(pool, task_id).await {
        Ok(Some(task)) => drain_run_cleanup_task_pg(health_registry, pool, task).await,
        Ok(None) => match run_cleanup_task_exists_pg(pool, task_id).await {
            // Row gone: a concurrent drain already carried it to completion.
            Ok(false) => RunCleanupDrainOutcome {
                slot_cleanup: SlotCleanupResult::default(),
                completed: true,
            },
            // Row present but unclaimable: still owed, just not by us.
            Ok(true) => RunCleanupDrainOutcome {
                slot_cleanup: SlotCleanupResult {
                    released_slots: 0,
                    cleared_slot_sessions: 0,
                    warnings: vec![format!(
                        "auto-queue cleanup task {task_id} is claimed elsewhere, backing off, or dead-lettered"
                    )],
                },
                completed: false,
            },
            Err(error) => RunCleanupDrainOutcome {
                slot_cleanup: SlotCleanupResult {
                    released_slots: 0,
                    cleared_slot_sessions: 0,
                    warnings: vec![error],
                },
                completed: false,
            },
        },
        Err(error) => RunCleanupDrainOutcome {
            slot_cleanup: SlotCleanupResult {
                released_slots: 0,
                cleared_slot_sessions: 0,
                warnings: vec![error],
            },
            completed: false,
        },
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RunCleanupReplayStats {
    pub(crate) drained: usize,
    pub(crate) completed: usize,
    /// Rows parked because their payload could not be decoded. Counted rather
    /// than silently skipped so an undecodable row is visible in the logs.
    pub(crate) dead_lettered: usize,
}

impl RunCleanupReplayStats {
    pub(crate) fn touched(&self) -> bool {
        self.drained > 0 || self.dead_lettered > 0
    }
}

/// Resume every cleanup task a previous process left behind.
///
/// This is the replay entry point: a restarted process reads
/// `auto_queue_run_cleanup_tasks` and continues from whichever step still owes
/// work, because each step re-derives its own remaining work from the durable
/// row rather than from anything the dead process held in memory.
pub(crate) async fn replay_pending_run_cleanup_tasks_pg(
    health_registry: Option<std::sync::Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
) -> Result<RunCleanupReplayStats, String> {
    let rows = claim_run_cleanup_task_batch_pg(pool).await?;

    let mut stats = RunCleanupReplayStats::default();
    for row in &rows {
        let task = match task_from_row(row) {
            Ok(task) => task,
            Err(error) => {
                let id: Option<i64> = row.try_get("id").ok();
                tracing::warn!(
                    task_id = ?id,
                    error = %error,
                    "[auto-queue] dead-lettering undecodable run cleanup task"
                );
                if let Some(id) = id {
                    dead_letter_task_pg(pool, id, &error).await;
                }
                stats.dead_lettered += 1;
                continue;
            }
        };
        stats.drained += 1;
        if drain_run_cleanup_task_pg(health_registry.clone(), pool, task)
            .await
            .completed
        {
            stats.completed += 1;
        }
    }
    Ok(stats)
}

#[cfg(test)]
#[path = "cleanup_tasks_pg_tests.rs"]
mod cleanup_tasks_pg_tests;
