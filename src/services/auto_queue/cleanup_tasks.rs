//! Durable replay for the post-commit half of auto-queue run cancel/end (#5142).
//!
//! Both `cancel_live_dispatches_for_runs_pg` and `terminalize_selected_runs_with_pg`
//! commit the dispatch/run state change first and then owe five more steps:
//! observability emit, wait-queue wake, provider session clear, card rollback,
//! and slot release (plus slot-thread clearing). Those steps used to live only
//! on the caller's stack, so a crash right after the commit left the cancel
//! durable while the slot token and provider session id survived, and a failing
//! session clear only appended a warning string.
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
//! at-least-once and converge — up to the attempt cap described below.
//!
//! Card rollback has a narrower guarantee. Only rollbacks carrying a non-NULL
//! dispatch generation enter this outbox: a successful rollback clears
//! `latest_dispatch_id`, making that generation self-invalidating on replay.
//! NULL cannot self-invalidate, so cancellation applies those rare rollbacks
//! synchronously in the transaction that commits the cancel and never enrolls
//! them for replay. A crash therefore commits neither the cancel nor the NULL
//! rollback, removing the durability gap instead of trying to reject it later.
//!
//! The observability emit in step 1 is **not** covered by that guarantee and is
//! deliberately at-most-once. `CancelTransitionMeta::emit` hands the event to an
//! in-process worker channel and discards the result
//! (`observability/emit.rs`: `if let Some(sender) = worker_sender() { let _ =
//! sender.send(..) }`), so the event is silently dropped when the worker is not
//! running, when the channel send fails, or when the process dies before the
//! worker flushes its queue to PostgreSQL.
//!
//! `emitted = TRUE` is committed **before** `emit()` is called, and a failed
//! mark aborts the drain before any emit fires. That ordering is what makes the
//! at-most-once claim exact rather than aspirational. Here is every way a
//! drain's step 1 can end — the enumeration is meant to be exhaustive, so the
//! two shapes that are easy to leave out are spelled out rather than folded into
//! the happy paths:
//!
//! 1. the mark UPDATE fails and the failure is observed as a failure → nothing
//!    was emitted, the failure is recorded as an attempt, and the retry emits
//!    exactly once when the mark finally commits;
//! 2. the mark commits and `emit()` runs for every entry in `pending_emits` →
//!    the durable flag makes `!task.emitted` false for every later replay, so
//!    each event fires exactly once;
//! 3. the mark commits and the process dies before `emit()` runs (or before the
//!    observability worker flushes its queue) → those events are lost, and no
//!    replay will re-fire them. `pending_emits` can hold more than one event and
//!    the flag covers the whole vector, so this case includes a **partial** loss:
//!    the process can die after emitting entry 1 of 3, and entries 2 and 3 are
//!    then lost while the flag says the batch is done;
//! 4. **ambiguous commit** — the mark UPDATE actually commits in PostgreSQL but
//!    the response is lost, so `execute()` returns `Err`. This looks like case 1
//!    from inside the process and is handled as one (attempt recorded, retry
//!    scheduled), but nothing was emitted and the retry reads `emitted = TRUE`,
//!    so it skips step 1 **forever**: the events are never emitted at all. This
//!    is a loss, not a repeat, so the at-most-once guarantee still holds — but
//!    case 1's "the retry emits exactly once" is false here, which is why this
//!    is listed separately instead of being folded into it.
//!
//! Cases 3 and 4 are the accepted trade for never double-counting an event; they
//! are not a claim that no emit is ever lost.
//!
//! ## What happens when the cleanup cannot succeed at all
//!
//! Failure propagation is row-wide: one failed card or slot keeps the entire
//! row, including already completed and still-owed siblings, retry-eligible.
//! A row that fails `MAX_CLEANUP_ATTEMPTS` times is dead-lettered: it is parked
//! on disk with its `last_error` and leaves both drain queries permanently. Its
//! slot token and provider session id then stay in whatever state the last
//! failed attempt left them — the convergence guarantee above stops there. That
//! is a deliberate trade (one unfixable row must not block every cleanup queued
//! behind it), and it is only defensible because the outcome is *observable*:
//! `RunCleanupReplayStats::dead_lettered` counts the transition when the parking
//! UPDATE actually lands, and the standing `auto_queue_cleanup.dead_lettered`
//! backlog is carried on the credential-free `/api/health` (count-only) as well
//! as in full on `/api/health/detail`, until an operator clears it. Poison rows
//! that cannot be decoded are parked directly, before the attempt cap. The
//! active-dispatch card guard is a separate quiet permanent give-up: it returns
//! success, so the row can be deleted without a warning or dead-letter and that
//! card is never retried. The public
//! half is the one that matters here: `/api/health/detail` is behind
//! `protected_api_domain`, so without it a monitor with no token reads `ok: true`
//! over a cleanup queue that stopped converging.
//!
//! ## Bookkeeping writes can fail too
//!
//! Every terminal decision this module makes is itself a PostgreSQL write, so
//! each one can fail. There are exactly three such writes and none of them may
//! swallow its own failure, because a swallowed one produces a row that neither
//! converges nor ever dead-letters — it just re-runs at lease-expiry rate
//! forever, contradicting the paragraph above:
//!
//! 1. `record_task_failure_pg` — bumps `attempts`, arms the backoff, and parks
//!    the row at the cap. If this UPDATE fails, `attempts` never rises and the
//!    row can never reach the cap. It reports [`AttemptRecord::Unrecorded`]
//!    instead of returning `false`, and the sweep counts it.
//! 2. `dead_letter_task_pg` — parks an undecodable (poison) row. If this UPDATE
//!    fails the row is not parked, so it is re-claimed and rejected again
//!    forever.
//!    It returns whether it landed; when it did not, the sweep falls back to the
//!    ordinary attempt bookkeeping so the row still backs off and still reaches
//!    the terminal cap.
//! 3. the `DELETE` that retires a finished task — a failure here used to skip the
//!    attempt bookkeeping entirely (#5142 r3 P3-3). It now records an attempt
//!    like every other failure path.
//!
//! All three are the same class — a failed bookkeeping write — and all three are
//! now on the same backoff and the same terminal cap.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};

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
    pub(crate) card_rollback_tasks: Vec<(String, Option<String>)>, // (card_id, dispatch_id as generation marker)
    pub(crate) card_rollback_source: Option<String>,
}

/// Outcome of draining one task.
#[derive(Debug, Default)]
pub(crate) struct RunCleanupDrainOutcome {
    pub(crate) slot_cleanup: SlotCleanupResult,
    /// `false` when the row was deliberately left behind for a later retry.
    pub(crate) completed: bool,
    /// `true` when *this* drain burned the row's last attempt and parked it for
    /// good. Propagated so the attempt-cap dead-letter reaches a counter instead
    /// of just vanishing from the drain query (#5142 r3).
    pub(crate) dead_lettered: bool,
    /// `true` when the drain failed *and* could not durably record that failure,
    /// i.e. `record_task_failure_pg`'s own UPDATE failed. The row keeps its old
    /// `attempts`, so it is neither closer to the cap nor backing off; the next
    /// drainer picks it straight back up once the claim lease expires. Reported
    /// rather than folded into "just another failed attempt", because those two
    /// have opposite convergence properties.
    pub(crate) attempt_unrecorded: bool,
}

/// Test-only footprint of the observability emit, re-exported from the emit
/// itself.
///
/// The recorder lives on `CancelTransitionMeta::emit` — the real boundary — and
/// not on a wrapper in this module. Round 4 recorded inside a
/// `fire_pending_emit` helper instead, which made the probe narrower than the
/// name it backed: a bare `meta.emit()` written in front of the `emitted = TRUE`
/// mark reintroduced the exact double-emit defect the ordering exists to
/// prevent, emitted the real events, and was invisible to
/// `a_failed_emit_mark_fires_no_emit_and_releases_no_slot_pg` because it never
/// went through the wrapper. Every route to an emit is now counted, so the test
/// name is true of the code rather than of one call site.
#[cfg(test)]
pub(crate) use crate::dispatch::emit_probe;

/// What `record_task_failure_pg` managed to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptRecord {
    /// The attempt landed. `dead_lettered` is `true` when it was the attempt
    /// that crossed [`MAX_CLEANUP_ATTEMPTS`] and parked the row.
    Recorded { dead_lettered: bool },
    /// The bookkeeping UPDATE itself failed, so nothing about this attempt is on
    /// disk. See the module header's "Bookkeeping writes can fail too".
    Unrecorded,
}

impl AttemptRecord {
    fn dead_lettered(self) -> bool {
        matches!(
            self,
            AttemptRecord::Recorded {
                dead_lettered: true
            }
        )
    }

    fn unrecorded(self) -> bool {
        matches!(self, AttemptRecord::Unrecorded)
    }
}

/// Columns every drain path selects. Kept in one place so the batch sweep and
/// the single-row claim cannot drift apart.
const TASK_COLUMNS: &str = "id, run_ids, dispatch_ids, released_slots, pending_emits, emitted, card_rollback_tasks, card_rollback_source";

/// Rows drained per replay sweep.
const REPLAY_BATCH_LIMIT: i64 = 50;

/// Attempts a task gets before it is dead-lettered.
///
/// Without a cap a permanently failing row keeps the oldest `created_at` and
/// therefore the head of the drain order forever, so `REPLAY_BATCH_LIMIT`
/// unfixable rows would stop every newly queued cleanup from ever draining.
///
/// See [`MAX_BACKOFF_SECONDS`] for what this cap costs in wall clock: a cleanup
/// step that keeps failing for roughly a quarter of an hour is parked for good.
const MAX_CLEANUP_ATTEMPTS: i32 = 10;

/// Upper bound on the exponential retry delay, in seconds.
///
/// The delay is `POWER(2, LEAST(attempts + 1, 8))`, so the exponent cap already
/// tops the series out at `2^8 = 256`. This constant is the belt-and-braces
/// clamp beside it, set to the same 256 so the two bounds cannot disagree — at
/// 300 it was simply unreachable and the doc comment described a ceiling the
/// code could never hit.
///
/// The full series a task gets before [`MAX_CLEANUP_ATTEMPTS`] dead-letters it
/// is therefore `2 + 4 + 8 + 16 + 32 + 64 + 128 + 256 + 256 = 766` seconds of
/// backoff. The replay sweep only runs on the 30-second policy tick, so each
/// delay rounds up to the next tick: **about 13–17 minutes of wall clock**.
/// Read that as the operational contract — *a cleanup step that keeps failing
/// for ~15 minutes is parked permanently* — and note that the only thing which
/// will say so afterwards is the `/api/health` `auto_queue_cleanup` backlog.
const MAX_BACKOFF_SECONDS: i64 = 256;

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
    card_rollback_tasks: &[(String, Option<String>)],
    card_rollback_source: Option<&str>,
) -> Result<i64, String> {
    let released_json = serde_json::to_value(released_slots)
        .map_err(|error| format!("serialize auto-queue cleanup released slots: {error}"))?;
    let emits_json = serde_json::to_value(pending_emits)
        .map_err(|error| format!("serialize auto-queue cleanup pending emits: {error}"))?;
    let card_tasks_json = serde_json::to_value(
        card_rollback_tasks
            .iter()
            .map(|(card_id, dispatch_id)| {
                serde_json::json!({
                    "card_id": card_id,
                    "dispatch_id": dispatch_id,
                })
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|error| format!("serialize auto-queue cleanup card rollback tasks: {error}"))?;
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO auto_queue_run_cleanup_tasks
            (run_ids, dispatch_ids, released_slots, pending_emits, card_rollback_tasks, card_rollback_source)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(run_ids)
    .bind(dispatch_ids)
    .bind(released_json)
    .bind(emits_json)
    .bind(card_tasks_json)
    .bind(card_rollback_source)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("enqueue auto-queue run cleanup task: {error}"))
}

fn parse_card_rollback_dispatch_id(item: &serde_json::Value) -> Result<Option<String>, String> {
    match item.get("dispatch_id") {
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(dispatch_id)) => Ok(Some(dispatch_id.clone())),
        None => Err("missing dispatch_id".to_string()),
        Some(_) => Err("invalid dispatch_id: expected null or string".to_string()),
    }
}

fn task_from_row(row: &sqlx::postgres::PgRow) -> Result<RunCleanupTask, String> {
    let released_slots: serde_json::Value = row
        .try_get("released_slots")
        .map_err(|error| format!("decode auto-queue cleanup released slots: {error}"))?;
    let pending_emits: serde_json::Value = row
        .try_get("pending_emits")
        .map_err(|error| format!("decode auto-queue cleanup pending emits: {error}"))?;
    let card_rollback_tasks_json: serde_json::Value = row
        .try_get("card_rollback_tasks")
        .map_err(|error| format!("decode auto-queue cleanup card rollback tasks: {error}"))?;
    let card_rollback_tasks: Vec<(String, Option<String>)> = card_rollback_tasks_json
        .as_array()
        .ok_or_else(|| "card_rollback_tasks is not an array".to_string())?
        .iter()
        .map(|item| {
            let card_id = item
                .get("card_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "missing or invalid card_id".to_string())?
                .to_string();
            let dispatch_id = parse_card_rollback_dispatch_id(item)?;
            Ok((card_id, dispatch_id))
        })
        .collect::<Result<Vec<_>, String>>()
        .map_err(|error| format!("parse auto-queue cleanup card rollback tasks: {error}"))?;
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
        card_rollback_tasks,
        card_rollback_source: row
            .try_get("card_rollback_source")
            .map_err(|error| format!("decode auto-queue cleanup card rollback source: {error}"))?,
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
/// Returns [`AttemptRecord::Recorded`] with `dead_lettered` set when the row is
/// dead-lettered as a result, so the caller can count it. That return value is
/// the whole point of this signature: before it existed the attempt-cap
/// dead-letter was the one failure mode with no counter at all —
/// `stats.dead_lettered` only ever saw undecodable payloads, and a row that
/// burned its ten attempts simply stopped appearing in the drain query with
/// nothing anywhere to say why.
///
/// [`AttemptRecord::Unrecorded`] means this UPDATE *itself* failed. That used to
/// be a `tracing::warn!` and a bare `false`, which is indistinguishable from "the
/// attempt was recorded and the row is not at the cap yet" — and the two have
/// opposite convergence properties, because an unrecorded attempt leaves
/// `attempts` where it was and therefore never reaches the cap at all. The caller
/// counts it instead.
///
/// The row is never deleted on dead-letter — the operator keeps the evidence and
/// `last_error` — but it drops out of the drain query so it can no longer block
/// the queue behind it.
///
/// ## Why this does not write to `db::relay_dead_letter`
///
/// That table is the relay's *message-content* sink (#4260): a
/// `kind`/`channel_id`/`content`/`reason` row that preserves text which was
/// already lost, written fire-and-forget and **auto-pruned after
/// `relay_dead_letter::RETENTION_DAYS`**. None of that shape fits here. This row
/// is not a copy of something lost — it *is* the work item, and it carries the
/// `run_ids`/`dispatch_ids`/`released_slots`/`pending_emits` that a later code
/// fix or an operator needs in order to resume the cleanup. Flattening that into
/// a `content` TEXT column would make it unresumable, the 30-day retention sweep
/// would delete the evidence the paragraph above promises to keep, and the copy
/// plus the original would become two sources of truth for one task. Parking the
/// row where it already is, and giving it a counter plus a health gauge, buys
/// the same observability without any of that.
async fn record_task_failure_pg(pool: &PgPool, id: i64, error: &str) -> AttemptRecord {
    match sqlx::query_scalar::<_, bool>(
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
         WHERE id = $1
         RETURNING dead_lettered_at IS NOT NULL",
    )
    .bind(id)
    .bind(error)
    .bind(MAX_BACKOFF_SECONDS)
    .bind(MAX_CLEANUP_ATTEMPTS)
    .fetch_optional(pool)
    .await
    {
        // `None` means the row is already gone (another drainer deleted it), so
        // there is nothing left to park: that is a recorded non-dead-letter, not
        // a bookkeeping failure.
        Ok(dead_lettered) => AttemptRecord::Recorded {
            dead_lettered: dead_lettered.unwrap_or(false),
        },
        Err(update_error) => {
            tracing::warn!(
                task_id = id,
                error = %update_error,
                "[auto-queue] failed to record cleanup task retry state"
            );
            AttemptRecord::Unrecorded
        }
    }
}

/// Rows that are parked and will never be drained again, i.e. the standing
/// dead-letter backlog. `/api/health` reports this so a task that burned its
/// attempt cap cannot sit in the table unnoticed; see the module header.
pub(crate) async fn dead_lettered_run_cleanup_task_count_pg(pool: &PgPool) -> Result<i64, String> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_run_cleanup_tasks
         WHERE dead_lettered_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("count dead-lettered auto-queue run cleanup tasks: {error}"))
}

/// Dead-letter a row whose payload cannot be decoded or cannot safely replay.
///
/// A poison row can never succeed, and a policy-rejected row must not be
/// re-applied. Retrying either forever would starve the queue; deleting it would
/// destroy the only evidence. It is parked instead, and counted, so the sweep
/// reports it rather than silently skipping it.
///
/// Returns whether the park actually landed. This UPDATE used to swallow its own
/// failure in a `tracing::warn!`, which made the sweep report a dead-letter that
/// had not happened while the row stayed drainable and was re-decoded on every
/// lease expiry — the same "bookkeeping write failed and nothing says so" shape
/// the `DELETE` path was fixed for in r3. The caller uses the return value both
/// to decide whether to count the transition and to fall back to the ordinary
/// attempt bookkeeping.
async fn dead_letter_task_pg(pool: &PgPool, id: i64, error: &str) -> bool {
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
            "[auto-queue] failed to dead-letter cleanup task"
        );
        return false;
    }
    true
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
///
/// ## Residual TOCTOU window (#5142 D-7) — sized honestly
///
/// This check and the clear it guards are separate statements, so a run that
/// takes the slot *between* them is still cleared. That window is not "one
/// database round-trip": the caller's path from here to the write is
/// `slot_taken_by_foreign_run_pg` → `slot_has_active_dispatch_excluding_pg` →
/// `build_slot_clear_target_pg` → `filter_safe_slot_thread_reset_targets` →
/// `archive_slot_threads` → `clear_slot_sessions_pg`, and `archive_slot_threads`
/// makes **Discord HTTP calls**, so a full round of provider API latency sits
/// inside the window. Order the seconds, not the milliseconds.
///
/// That sequence is base code; this module only added the ownership probe in
/// front of it. The damage stays P3 because
/// `filter_safe_slot_thread_reset_targets` already excludes threads with a live
/// dispatch, so the worst outcome is that a freshly-arrived run loses provider
/// session continuity and re-establishes it on its next turn. Closing it
/// properly means moving the ownership predicate into the clearing statement
/// itself (or fencing on a slot generation counter), which is a change to the
/// base slot-clearing path and belongs in its own issue rather than here.
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
        // The mark is committed BEFORE `emit()` fires, and a failed mark returns
        // immediately. The other order — emit, then mark — is what made the
        // module header's "never double-counting" broader than the code: a mark
        // that failed while the events were already out left the row at
        // `emitted = FALSE`, so the next replay sent the same events a second
        // time. In this order a failed mark means nothing was emitted yet, so
        // abandoning the attempt here costs nothing and the retry emits exactly
        // once. The residuals are cases 3 and 4 in the header (mark commits and
        // the events are lost anyway, or the mark commits ambiguously and is
        // never re-attempted) — both losses, never repeats.
        //
        // Nothing in the emit path is observable at runtime, so this ordering is
        // pinned by the `#[cfg(test)]` probe on `CancelTransitionMeta::emit`
        // itself rather than by inference; any emit reaching this row before the
        // mark commits — through this loop or written directly — is counted. See
        // `a_failed_emit_mark_fires_no_emit_and_releases_no_slot_pg`.
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
            let error = format!(
                "failed to mark auto-queue cleanup emits for task {}: {error}",
                task.id
            );
            let record = record_task_failure_pg(pool, task.id, &error).await;
            warnings.push(error);
            return RunCleanupDrainOutcome {
                slot_cleanup: SlotCleanupResult {
                    released_slots: 0,
                    cleared_slot_sessions: 0,
                    warnings,
                },
                completed: false,
                dead_lettered: record.dead_lettered(),
                attempt_unrecorded: record.unrecorded(),
            };
        }
        for meta in &task.pending_emits {
            meta.emit();
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
            let record = record_task_failure_pg(pool, task.id, &error).await;
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
                dead_lettered: record.dead_lettered(),
                attempt_unrecorded: record.unrecorded(),
            };
        }
    };

    // Step 3 — slot release, committed together with the durable record of which
    // slots were released so a crash between the two is impossible.
    let (released_slots, newly_released) =
        match release_and_persist_slots_for_task_pg(pool, &task).await {
            Ok(value) => value,
            Err(error) => {
                let record = record_task_failure_pg(pool, task.id, &error).await;
                warnings.push(error);
                return RunCleanupDrainOutcome {
                    slot_cleanup: SlotCleanupResult {
                        released_slots: 0,
                        cleared_slot_sessions: cleared_dispatch_sessions,
                        warnings,
                    },
                    completed: false,
                    dead_lettered: record.dead_lettered(),
                    attempt_unrecorded: record.unrecorded(),
                };
            }
        };

    // Step 3.5 — card rollback.
    //
    // Retry safety: every enrolled generation is Some and self-invalidating: a
    // successful rollback clears `latest_dispatch_id` to NULL, so a replay sees
    // Some != NULL and skips. NULL is forbidden at enrollment because it cannot
    // distinguish the cancelled lifecycle from a later manual NULL lifecycle.
    // Seeing one here means manual corruption/tampering; skip only that card so
    // a valid Some sibling in the same row is not lost with it.
    // The active-dispatch guard is a quiet permanent skip: it returns Ok, so the
    // outbox row can complete and be deleted, and that rollback is not retried.
    let mut all_cards_handled = true;
    if !task.card_rollback_tasks.is_empty() {
        let source = task.card_rollback_source.as_deref().unwrap_or("auto_queue");
        for (card_id, expected_dispatch_id) in &task.card_rollback_tasks {
            if expected_dispatch_id.is_none() {
                let reason = format!(
                    "skipped forbidden NULL dispatch_id generation in card rollback outbox: card_id={card_id}"
                );
                crate::auto_queue_log!(
                    warn,
                    "card_rollback_null_generation_outbox_invariant_violation",
                    crate::services::auto_queue::AutoQueueLogContext::new().card(card_id),
                    "[auto-queue] skipping corrupted postgres card rollback during cleanup task {}: {}",
                    task.id,
                    reason
                );
                warnings.push(reason);
                continue;
            }
            match perform_card_rollback_on_pg(
                pool,
                card_id,
                expected_dispatch_id.as_deref(),
                source,
            )
            .await
            {
                Ok(_) => {}
                Err(error) => {
                    all_cards_handled = false;
                    crate::auto_queue_log!(
                        warn,
                        "card_rollback_pg_failed",
                        crate::services::auto_queue::AutoQueueLogContext::new().card(card_id),
                        "[auto-queue] failed to roll back postgres card {card_id} during run cleanup: {error}"
                    );
                    warnings.push(format!("failed to roll back card {card_id}: {error}"));
                }
            }
        }
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
    if !all_slots_handled || !all_cards_handled {
        let summary = slot_cleanup.warnings.join("; ");
        let record = record_task_failure_pg(pool, task.id, &summary).await;
        return RunCleanupDrainOutcome {
            slot_cleanup,
            completed: false,
            dead_lettered: record.dead_lettered(),
            attempt_unrecorded: record.unrecorded(),
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
        // #5142 r3 P3-3: every other failure path records an attempt; this one
        // used to skip it, which made an undeletable row a task that neither
        // converged nor ever dead-lettered — it just re-ran the whole drain at
        // lease-expiry rate (`CLAIM_LEASE_SECONDS`) forever, contradicting "no
        // row occupies the queue indefinitely". The repeated steps are all
        // idempotent, so putting it on the same backoff and the same terminal cap
        // as everything else costs nothing but a parked row, and the parked row
        // is now counted and reported.
        //
        // r3 called this "the single shape" with that property. That was a guess
        // dressed as an enumeration, and it was wrong: the same shape also lived
        // in `record_task_failure_pg` (a failed attempt UPDATE returned a bare
        // `false`, so `attempts` never rose and the cap was never reached) and in
        // `dead_letter_task_pg` (a failed park UPDATE was swallowed by a
        // `tracing::warn!`, so a poison row was re-claimed forever). The true
        // enumeration is the three bookkeeping writes listed in the module
        // header, and all three are now handled the same way.
        let error = format!(
            "delete finished auto-queue cleanup task {}: {error}",
            task.id
        );
        let record = record_task_failure_pg(pool, task.id, &error).await;
        return RunCleanupDrainOutcome {
            slot_cleanup,
            completed: false,
            dead_lettered: record.dead_lettered(),
            attempt_unrecorded: record.unrecorded(),
        };
    }

    RunCleanupDrainOutcome {
        slot_cleanup,
        completed: true,
        dead_lettered: false,
        attempt_unrecorded: false,
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
                dead_lettered: false,
                attempt_unrecorded: false,
            },
            // Row present but unclaimable: still owed, just not by us.
            //
            // `dead_lettered` stays false here even though one of the three
            // reasons is "already dead-lettered": this field counts the
            // *transition*, and the transition was counted by whichever drain
            // performed it. The standing backlog is the health gauge's job
            // (`dead_lettered_run_cleanup_task_count_pg`), not this counter's.
            Ok(true) => RunCleanupDrainOutcome {
                slot_cleanup: SlotCleanupResult {
                    released_slots: 0,
                    cleared_slot_sessions: 0,
                    warnings: vec![format!(
                        "auto-queue cleanup task {task_id} is claimed elsewhere, backing off, or dead-lettered"
                    )],
                },
                completed: false,
                dead_lettered: false,
                attempt_unrecorded: false,
            },
            Err(error) => RunCleanupDrainOutcome {
                slot_cleanup: SlotCleanupResult {
                    released_slots: 0,
                    cleared_slot_sessions: 0,
                    warnings: vec![error],
                },
                completed: false,
                dead_lettered: false,
                attempt_unrecorded: false,
            },
        },
        Err(error) => RunCleanupDrainOutcome {
            slot_cleanup: SlotCleanupResult {
                released_slots: 0,
                cleared_slot_sessions: 0,
                warnings: vec![error],
            },
            completed: false,
            dead_lettered: false,
            attempt_unrecorded: false,
        },
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RunCleanupReplayStats {
    pub(crate) drained: usize,
    pub(crate) completed: usize,
    /// Rows this sweep parked and removed from the drain order for good — the
    /// undecodable payloads (poison) and rows that burned through
    /// `MAX_CLEANUP_ATTEMPTS`. Counted
    /// rather than silently skipped because a
    /// dead-letter is the one outcome the retry loop can never repair on its
    /// own, so it has to reach a human; the standing backlog is on `/api/health`
    /// and `/api/health/detail` under `auto_queue_cleanup.dead_lettered`.
    ///
    /// Only counted when the parking UPDATE actually landed. It used to be
    /// incremented for every undecodable row, including rows whose `id` could not
    /// be read (so nothing was addressed at all) and rows whose dead-letter
    /// UPDATE failed — i.e. it reported a transition that had not happened.
    pub(crate) dead_lettered: usize,
    /// Outcomes this sweep decided on but could not durably write: a dead-letter
    /// UPDATE that failed, a failed attempt whose bookkeeping UPDATE failed, or a
    /// row whose `id` could not be decoded so nothing could be addressed at all.
    ///
    /// These rows are not lost — they stay claimable and the next sweep
    /// re-derives the same decision — but nothing this sweep decided reached
    /// disk, so it is counted here instead of being reported as if it had landed.
    pub(crate) unrecorded_failures: usize,
}

impl RunCleanupReplayStats {
    pub(crate) fn touched(&self) -> bool {
        self.drained > 0 || self.dead_lettered > 0 || self.unrecorded_failures > 0
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
                let Some(id) = id else {
                    // Without an id nothing can be addressed: the row cannot be
                    // parked, cannot be given an attempt, and will be re-claimed
                    // on the next sweep. Counting it as `dead_lettered` (which is
                    // what this used to do) reports a transition that did not
                    // happen.
                    stats.unrecorded_failures += 1;
                    continue;
                };
                if dead_letter_task_pg(pool, id, &error).await {
                    stats.dead_lettered += 1;
                } else {
                    // The park did not land. Fall back to the ordinary attempt
                    // bookkeeping so the row at least backs off and still reaches
                    // the terminal cap, instead of being re-decoded forever at
                    // lease-expiry rate. If that fallback happens to be the
                    // attempt that crosses the cap, the row really is parked and
                    // the transition is counted as one; otherwise the sweep's
                    // decision left no trace and is counted as unrecorded.
                    if record_task_failure_pg(pool, id, &error)
                        .await
                        .dead_lettered()
                    {
                        stats.dead_lettered += 1;
                    } else {
                        stats.unrecorded_failures += 1;
                    }
                }
                continue;
            }
        };
        stats.drained += 1;
        let outcome = drain_run_cleanup_task_pg(health_registry.clone(), pool, task).await;
        if outcome.completed {
            stats.completed += 1;
        }
        if outcome.dead_lettered {
            // The attempt-cap dead-letter. It used to be counted nowhere: the row
            // just stopped matching the drain predicate and the sweep reported it
            // as one more incomplete drain.
            stats.dead_lettered += 1;
        }
        if outcome.attempt_unrecorded {
            stats.unrecorded_failures += 1;
        }
    }
    Ok(stats)
}

/// Open and commit a transaction around the shared rollback body for an outbox
/// card. Only Some generation tokens are permitted to reach this wrapper.
async fn perform_card_rollback_on_pg(
    pool: &PgPool,
    card_id: &str,
    expected_dispatch_id: Option<&str>,
    source: &str,
) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin card rollback transaction for {card_id}: {error}"))?;
    perform_card_rollback_on_tx(&mut tx, card_id, expected_dispatch_id, source).await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit card rollback for {card_id}: {error}"))
}

/// Apply the complete card rollback inside the caller's transaction.
///
/// The cancel path uses this directly for a NULL generation, while the outbox
/// wrapper uses it for a replay-safe Some generation. Keeping the status and
/// generation guards, active-dispatch check, state transition, review/clock/PM
/// reset, audit, and worktree scrub in this single body prevents the two paths
/// from drifting.
pub(crate) async fn perform_card_rollback_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    card_id: &str,
    expected_dispatch_id: Option<&str>,
    source: &str,
) -> Result<(), String> {
    let (current_status, current_dispatch_id): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = $1 FOR UPDATE",
    )
    .bind(card_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load locked card status and dispatch id {card_id}: {error}"))?
    .unwrap_or_default();

    let Some(from_status) = current_status else {
        return Ok(());
    };
    if !matches!(from_status.as_str(), "requested" | "in_progress") {
        return Ok(());
    }

    // A Some token that already rolled back now sees NULL and self-invalidates;
    // any other mismatch skips without claiming what lifecycle produced it.
    if expected_dispatch_id != current_dispatch_id.as_deref() {
        return Ok(());
    }

    let has_active_dispatch = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1
             FROM task_dispatches
             WHERE kanban_card_id = $1 AND status IN ('pending', 'dispatched')
         )",
    )
    .bind(card_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("check active dispatches for card {card_id}: {error}"))?;
    if has_active_dispatch {
        // Quiet permanent give-up: the outbox caller treats this as success and
        // deletes the row, so this card rollback receives no retry.
        return Ok(());
    }

    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        tx,
        &crate::engine::transition::TransitionIntent::UpdateStatus {
            card_id: card_id.to_string(),
            from: from_status.clone(),
            to: "ready".to_string(),
        },
    )
    .await
    .map_err(|error| format!("update card status for {card_id}: {error}"))?;

    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        tx,
        &crate::engine::transition::TransitionIntent::SetLatestDispatchId {
            card_id: card_id.to_string(),
            dispatch_id: None,
        },
    )
    .await
    .map_err(|error| format!("clear dispatch id for {card_id}: {error}"))?;

    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        tx,
        &crate::engine::transition::TransitionIntent::SetReviewStatus {
            card_id: card_id.to_string(),
            review_status: None,
        },
    )
    .await
    .map_err(|error| format!("clear review status for {card_id}: {error}"))?;

    sqlx::query(
        "UPDATE kanban_cards
         SET review_round = 0,
             review_notes = NULL,
             suggestion_pending_at = NULL,
             review_entered_at = NULL,
             awaiting_dod_at = NULL,
             blocked_reason = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("reset card cleanup fields {card_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision,
            decided_by, decided_at, approach_change_round, session_reset_round, review_entered_at, updated_at
         ) VALUES (
            $1, 0, 'idle', NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NOW()
         )
         ON CONFLICT (card_id) DO UPDATE SET
            review_round = 0,
            state = 'idle',
            pending_dispatch_id = NULL,
            last_verdict = NULL,
            last_decision = NULL,
            decided_by = NULL,
            decided_at = NULL,
            approach_change_round = NULL,
            session_reset_round = NULL,
            review_entered_at = NULL,
            updated_at = NOW()",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("reset card review state {card_id}: {error}"))?;

    sqlx::query("DELETE FROM kv_meta WHERE key = $1 OR key = $2")
        .bind(format!("pm_pending:{card_id}"))
        .bind(format!("pm_decision_sent:{card_id}"))
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("clear card escalation state {card_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result)
         VALUES ($1, $2, 'ready', $3, 'OK (run cleanup card rollback)')",
    )
    .bind(card_id)
    .bind(&from_status)
    .bind(source)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("insert kanban audit log {card_id}: {error}"))?;

    sqlx::query(
        "UPDATE task_dispatches
         SET context = CASE
                 WHEN context IS NULL OR context = '' THEN context
                 ELSE NULLIF(
                     (context::jsonb
                         - 'worktree_path'
                         - 'worktree_branch'
                         - 'completed_worktree_path'
                         - 'completed_branch'
                     )::text,
                     '{}'
                 )
             END,
             result = CASE
                 WHEN result IS NULL OR result = '' THEN result
                 ELSE NULLIF(
                     (result::jsonb
                         - 'worktree_path'
                         - 'worktree_branch'
                         - 'completed_worktree_path'
                         - 'completed_branch'
                     )::text,
                     '{}'
                 )
             END
         WHERE kanban_card_id = $1
           AND (
               (context IS NOT NULL AND context <> '' AND (
                   (context::jsonb) ? 'worktree_path'
                   OR (context::jsonb) ? 'worktree_branch'
                   OR (context::jsonb) ? 'completed_worktree_path'
                   OR (context::jsonb) ? 'completed_branch'
               ))
               OR (result IS NOT NULL AND result <> '' AND (
                   (result::jsonb) ? 'worktree_path'
                   OR (result::jsonb) ? 'worktree_branch'
                   OR (result::jsonb) ? 'completed_worktree_path'
                   OR (result::jsonb) ? 'completed_branch'
               ))
           )",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("scrub dispatch worktree metadata {card_id}: {error}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_card_rollback_dispatch_id;

    #[test]
    fn card_rollback_dispatch_id_accepts_only_null_or_string() {
        assert_eq!(
            parse_card_rollback_dispatch_id(&serde_json::json!({"dispatch_id": null})),
            Ok(None)
        );
        assert_eq!(
            parse_card_rollback_dispatch_id(&serde_json::json!({"dispatch_id": "dispatch-1"})),
            Ok(Some("dispatch-1".to_string()))
        );

        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"dispatch_id": 7}),
            serde_json::json!({"dispatch_id": {"id": "dispatch-1"}}),
        ] {
            assert!(parse_card_rollback_dispatch_id(&invalid).is_err());
        }
    }
}

#[cfg(test)]
#[path = "cleanup_tasks_pg_tests.rs"]
mod cleanup_tasks_pg_tests;
