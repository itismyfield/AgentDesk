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

/// Insert the durable cleanup record. MUST be called on the same transaction
/// that commits the dispatch cancel / run terminalization, otherwise the record
/// and the state change can diverge.
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

pub(crate) async fn load_run_cleanup_task_pg(
    pool: &PgPool,
    id: i64,
) -> Result<Option<RunCleanupTask>, String> {
    let row = sqlx::query(
        "SELECT id, run_ids, dispatch_ids, released_slots, pending_emits, emitted
         FROM auto_queue_run_cleanup_tasks
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load auto-queue run cleanup task {id}: {error}"))?;
    row.as_ref().map(task_from_row).transpose()
}

/// Number of cleanup rows still owed. Tests use it to prove convergence.
pub(crate) async fn pending_run_cleanup_task_count_pg(pool: &PgPool) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM auto_queue_run_cleanup_tasks")
        .fetch_one(pool)
        .await
        .map_err(|error| format!("count auto-queue run cleanup tasks: {error}"))
}

async fn record_task_failure_pg(pool: &PgPool, id: i64, error: &str) {
    if let Err(update_error) = sqlx::query(
        "UPDATE auto_queue_run_cleanup_tasks
         SET attempts = attempts + 1,
             last_error = $2,
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
            "[auto-queue] failed to record cleanup task retry state"
        );
    }
}

async fn persist_released_slots_pg(
    pool: &PgPool,
    id: i64,
    released_slots: &[ReleasedSlot],
) -> Result<(), String> {
    let payload = serde_json::to_value(released_slots)
        .map_err(|error| format!("serialize auto-queue cleanup released slots: {error}"))?;
    sqlx::query(
        "UPDATE auto_queue_run_cleanup_tasks
         SET released_slots = $2,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(id)
    .bind(payload)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|error| format!("persist auto-queue cleanup released slots for task {id}: {error}"))
}

/// Release every slot still held by this task's runs, merging the result into
/// the durably recorded set.
///
/// Retry safety: the CAS predicate `assigned_run_id = ANY($1)` means a replay
/// that arrives after the slot was handed to a different run matches no row, so
/// the slot is never stolen back. A replay that arrives after this task already
/// released the slot also matches no row — which is exactly why the released set
/// is persisted before the slot-thread clear runs.
async fn release_slots_for_task_pg(
    pool: &PgPool,
    task: &RunCleanupTask,
) -> Result<(Vec<ReleasedSlot>, usize), String> {
    let rows = sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id = ANY($1)
         RETURNING agent_id, slot_index",
    )
    .bind(&task.run_ids)
    .fetch_all(pool)
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
    // that stops a replay from repeating it. The flag is set after the emit, so
    // a crash inside that window can still duplicate — at-least-once, never
    // lost.
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

    // Step 3 — slot release, persisted before the slot-thread clear so a crash
    // between the two still leaves the slot keys on disk.
    let (released_slots, newly_released) = match release_slots_for_task_pg(pool, &task).await {
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
    if released_slots != task.released_slots
        && let Err(error) = persist_released_slots_pg(pool, task.id, &released_slots).await
    {
        record_task_failure_pg(pool, task.id, &error).await;
        warnings.push(error);
        return RunCleanupDrainOutcome {
            slot_cleanup: SlotCleanupResult {
                released_slots: newly_released,
                cleared_slot_sessions: cleared_dispatch_sessions,
                warnings,
            },
            completed: false,
        };
    }

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

/// Drain the task identified by `task_id`, tolerating a row that another drain
/// already removed.
pub(crate) async fn drain_run_cleanup_task_by_id_pg(
    health_registry: Option<std::sync::Arc<crate::services::discord::health::HealthRegistry>>,
    pool: &PgPool,
    task_id: i64,
) -> RunCleanupDrainOutcome {
    match load_run_cleanup_task_pg(pool, task_id).await {
        Ok(Some(task)) => drain_run_cleanup_task_pg(health_registry, pool, task).await,
        Ok(None) => RunCleanupDrainOutcome {
            slot_cleanup: SlotCleanupResult::default(),
            completed: true,
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
}

impl RunCleanupReplayStats {
    pub(crate) fn touched(&self) -> bool {
        self.drained > 0
    }
}

const REPLAY_BATCH_LIMIT: i64 = 50;

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
    let rows = sqlx::query(
        "SELECT id, run_ids, dispatch_ids, released_slots, pending_emits, emitted
         FROM auto_queue_run_cleanup_tasks
         ORDER BY created_at ASC, id ASC
         LIMIT $1",
    )
    .bind(REPLAY_BATCH_LIMIT)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load pending auto-queue run cleanup tasks: {error}"))?;

    let mut stats = RunCleanupReplayStats::default();
    for row in &rows {
        let task = match task_from_row(row) {
            Ok(task) => task,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "[auto-queue] skipping undecodable run cleanup task"
                );
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
