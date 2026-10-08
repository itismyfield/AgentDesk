//! #3154 — durable pending synthetic-turn-start records + per-channel
//! serialization for the TUI-direct relay.
//!
//! ## Why this exists (the root cause it fixes)
//! A wakeup/loop (`ScheduleWakeup`, classified slash-command-control) turn can
//! start BEFORE the prior user turn's relay has drained. The synthetic claim
//! used to run INLINE inside the single per-provider observer loop
//! ([`super::tui_prompt_relay::relay_observed_prompt`]): it seeded
//! `turn_start_offset` from the prior relay cursor while the prior tail was
//! still undrained, colliding `response_sent_offset` bookkeeping
//! (`response_sent_offset_monotonic` violations), duplicate relay, or a
//! wrong-turn terminal commit. No claim-path offset manipulation can fix it —
//! the fix is TEMPORAL: defer the synthetic start until the prior turn
//! genuinely finalizes, detached from the shared observer loop.
//!
//! ## Mechanism (LOCKED design — Candidate 1, approach A)
//! 1. Persist a durable [`TuiDirectPendingStart`] under a new runtime_store
//!    root the instant the anchor/lease are created (BEFORE any wait).
//! 2. [`relay_observed_prompt`] returns to the observer loop immediately and a
//!    DETACHED per-`(provider, channel_id)` worker performs the claim — so a
//!    long wait on channel A never starves channel B.
//! 3. The worker serializes per channel ([`channel_lock`]); multiple pending
//!    prompts on the same channel drain FIFO.
//! 4. The worker polls [`prior_turn_finalized`] (~100ms) bounded by an 8s
//!    backstop, then claims with a FRESH `turn_start_offset = relay_last_offset()`
//!    (post-drain == EOF) and `response_sent_offset = 0`.
//! 5. While a pending start exists for a channel, the watcher no-inflight
//!    suppression keeps bytes buffered ([`pending_synthetic_start_present`]) and
//!    the idle queue is blocked for that channel.
//! 6. The record is deleted only AFTER the inflight save succeeds. A crash
//!    between save and delete is healed idempotently (the claim refreshes the
//!    matching anchor's existing inflight); a crash before save resumes waiting.
//!    The provider prompt is NEVER resubmitted.

use super::SharedData;
use crate::services::tui_prompt_dedupe::{TuiPromptAnchor, clear_prompt_anchor_for_response};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

mod watcher_cancel;

mod restore_gate;

mod state;
#[allow(unused_imports)]
pub(in crate::services::discord) use state::turn_retirement;

pub(super) use state::{
    AbortCleanupFn, ClaimFn, PENDING_START_BACKSTOP, PENDING_START_CLAIM_RETRY_BACKOFF,
    PENDING_START_MAX_BACKSTOP_CYCLES, PENDING_START_MAX_CLAIM_ATTEMPTS, PENDING_START_POLL,
    PendingStartState, PriorTurnObservation, PriorTurnView, ReclaimOrphanFn,
    ReclaimStaleForeignOutcome, STALE_FOREIGN_INFLIGHT_MIN_AGE_SECS, TuiDirectPendingStart, ViewFn,
    backstop_claim_is_safe, delete, load_all, mark_present_on_restore,
    pending_synthetic_start_blocks_idle_kickoff, pending_synthetic_start_present, persist,
    prior_turn_finalized, record_claim_marker_if_watcher_owned, retire_completed,
    save_row_unless_retired, should_defer_synthetic_turn_start,
};
#[allow(unused_imports)]
pub(super) use state::{
    RESTART_ORPHAN_COMMITTED_GRACE_SECS, clear_abandoned_synthetic_start_presence,
};
#[cfg(test)]
pub(super) use state::{pending_synthetic_start_abandoned, reset_present_for_tests};

#[cfg(test)]
use state::{
    ActiveWorkerGuard, claude_tui_output_path_missing, inflight_generation_precedes_current,
    mark_absent, mark_present, records_for_channel, restart_orphan_independent_pane_ready, root,
};
use state::{
    active_worker_guard_for_spawn, committed_foreign_complete_finalize_context,
    committed_foreign_inflight_is_finalize_clearable, output_capture_offset,
    restart_orphan_evidence_at, restart_orphan_pane_ready_for_input,
    stale_foreign_cancel_finalize_context, stale_foreign_inflight_is_reclaimable_at,
    stale_foreign_probe, take_retired, update_claim_attempt_count,
};

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DestructiveCancelHookPoint {
    /// The death/identity gate has allowed, before any pin is captured.
    PostGate,
    /// #5071 T3-A1: the flock-held row commit has returned and the registry CAS
    /// has not run yet. This is the window a late registry replacement or a
    /// respawn must fail closed in, and the only window in which `cancel` is
    /// still guaranteed unset.
    PreRegistryCas,
}
#[cfg(test)]
type DestructiveCancelPostGateHook =
    Arc<dyn Fn(DestructiveCancelHookPoint) + Send + Sync + 'static>;
#[cfg(test)]
static DESTRUCTIVE_CANCEL_POST_GATE_HOOK: LazyLock<Mutex<Option<DestructiveCancelPostGateHook>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
fn run_destructive_cancel_post_gate_hook_for_tests(point: DestructiveCancelHookPoint) {
    let hook = DESTRUCTIVE_CANCEL_POST_GATE_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(point);
    }
}

#[cfg(test)]
struct DestructiveCancelPostGateHookGuard;

#[cfg(test)]
impl Drop for DestructiveCancelPostGateHookGuard {
    fn drop(&mut self) {
        *DESTRUCTIVE_CANCEL_POST_GATE_HOOK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

#[cfg(test)]
fn set_destructive_cancel_post_gate_hook_for_tests(
    hook: DestructiveCancelPostGateHook,
) -> DestructiveCancelPostGateHookGuard {
    *DESTRUCTIVE_CANCEL_POST_GATE_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hook);
    DestructiveCancelPostGateHookGuard
}

// ---------------------------------------------------------------------------
// Per-(provider, channel) serialization lock table
// ---------------------------------------------------------------------------

/// Module-static lock table (smaller surface than threading a field through
/// `SharedData`). One `tokio::Mutex` per `(provider, channel_id)`; the worker
/// holds it for the whole wait+claim so same-channel pending prompts serialize
/// FIFO while different channels run fully in parallel.
#[allow(clippy::type_complexity)]
static CHANNEL_LOCKS: LazyLock<Mutex<HashMap<(String, u64), Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn channel_lock(provider: &str, channel_id: u64) -> Arc<tokio::sync::Mutex<()>> {
    let mut table = CHANNEL_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    table
        .entry((provider.to_string(), channel_id))
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

mod foreign_recovery;
#[cfg(test)]
use foreign_recovery::{ForeignRecoverySource, submit_stale_foreign_inflight_cancel};
pub(in crate::services::discord) use foreign_recovery::{
    demote_leaked_foreign_inflight_if_current, demote_stale_foreign_inflight_if_current,
};

#[cfg(test)]
#[path = "tui_direct_pending_start/retirement_recheck_tests.rs"]
pub(in crate::services::discord) mod retirement_recheck_tests;

/// #3296 codex r3: choose the foreign identity an aborted-anchor marker pins.
/// The worker's LAST-VIEW identity is PRIMARY — that row was observed LIVE
/// during the backstop window, so it is definitionally the turn the ABORT
/// deferred on. The cleanup-instant inflight row is read (lazily) ONLY when
/// no poll ever captured an identity: between the final backstop view and the
/// cleanup's read, the foreign row may terminal-commit (tombstone + clear)
/// and a SUCCESSOR row may already hold the `(provider, channel)` slot —
/// preferring the current row pinned that WRONG turn (the genuine prior
/// commit's tombstone then never matched the marker, and the successor's own
/// commit could false-`✅` a possibly-unanswered anchor). The no-view fallback
/// is deliberately kept conservative-best-effort: with no observed identity
/// the cleanup-instant row is the only evidence available (a successor there
/// would need the never-observed prior row to clear AND a new claim to land
/// inside the same µs window), while pinning nothing forfeits drain coverage
/// outright — a guaranteed bounded `⚠` even on an answered anchor.
pub(super) fn pin_abort_foreign_identity(
    last_view_foreign: Option<(u64, String)>,
    read_cleanup_instant_row: impl FnOnce() -> Option<(u64, String)>,
) -> Option<(u64, String)> {
    last_view_foreign.or_else(read_cleanup_instant_row)
}

/// Spawn the DETACHED per-channel worker. Acquires the channel lock (FIFO
/// serialization), polls the wait predicate until the prior turn finalizes (or
/// the 8s backstop fires), runs the claim, and deletes the record. On the
/// terminal backstop ABORT it runs `abort_cleanup_fn` (the aborted-anchor
/// marker record — #3282/#3296) before dropping the record. Returns immediately
/// so the observer loop is never blocked.
pub(super) fn spawn_worker(
    shared: Arc<SharedData>,
    record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
    abort_cleanup_fn: AbortCleanupFn,
    reclaim_orphan_fn: ReclaimOrphanFn,
) {
    let effect = restore_gate::input_effect(&record);
    let active_guard = active_worker_guard_for_spawn(&record.provider, record.channel_id);
    let native_turn = WorkerNativeTurn {
        provider: record.provider.clone(),
        tmux_session_name: record.tmux_session_name.clone(),
        anchor: record.anchor_message_id,
        claimed: false,
    };
    restore_gate::spawn_admitted(effect, async move {
        let _active_guard = active_guard;
        // Bind the whole guard so the block owns it, not only its `claimed` field.
        let mut native_turn = native_turn;
        native_turn.claimed = run_worker_inner(
            shared,
            record,
            view_fn,
            claim_fn,
            abort_cleanup_fn,
            reclaim_orphan_fn,
        )
        .await;
    });
}

/// Settles the native turn a relay handed to this worker once the worker ends or is never run.
struct WorkerNativeTurn {
    provider: String,
    tmux_session_name: String,
    anchor: u64,
    claimed: bool,
}

impl Drop for WorkerNativeTurn {
    fn drop(&mut self) {
        crate::services::tui_prompt_dedupe::settle_worker_native_turn(
            &self.provider,
            &self.tmux_session_name,
            self.anchor,
            self.claimed,
        );
    }
}

/// Why the worker's wait loop ended this cycle.
enum WaitOutcome {
    /// The prior turn genuinely finalized — claiming is safe.
    Finalized,
    /// The backstop expired AND, at the claim instant, the prior inflight is
    /// gone / only ever our own anchor — claiming is safe (a wedged-but-cleared
    /// or binding-transient prior). Carries the final view for observability.
    BackstopClaimSafe,
    /// The backstop expired but a FOREIGN prior inflight is STILL live —
    /// claiming would overwrite it (the #3154 regression). Keep deferring.
    BackstopForeignInflightLive,
}

#[cfg(test)]
async fn run_worker(
    shared: Arc<SharedData>,
    record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
    abort_cleanup_fn: AbortCleanupFn,
    reclaim_orphan_fn: ReclaimOrphanFn,
) {
    let _active_guard = ActiveWorkerGuard::new(&record.provider, record.channel_id);
    run_worker_inner(
        shared,
        record,
        view_fn,
        claim_fn,
        abort_cleanup_fn,
        reclaim_orphan_fn,
    )
    .await;
}

/// Waits for the prior turn and claims; `true` only when the claim succeeded.
async fn run_worker_inner(
    shared: Arc<SharedData>,
    mut record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
    abort_cleanup_fn: AbortCleanupFn,
    reclaim_orphan_fn: ReclaimOrphanFn,
) -> bool {
    let lock = channel_lock(&record.provider, record.channel_id);
    let _guard = lock.lock().await;

    let mut backstop_cycles: u32 = 0;
    let mut claim_attempts: u32 = 0;
    let worker_start = tokio::time::Instant::now();
    // codex r2: the most recent poll's live FOREIGN inflight identity. Handed
    // to the ABORT cleanup so the aborted-anchor marker pins WHICH turn it was
    // deferring on even when that row vanishes before the cleanup's own read.
    let mut last_foreign_identity: Option<(u64, String)> = None;

    loop {
        // ---- Wait window: poll until finalized or backstop expiry. ----
        let cycle_start = tokio::time::Instant::now();
        let outcome = loop {
            if let Some(obs) = view_fn(&shared, &record).await {
                if obs.foreign_inflight_identity.is_some() {
                    last_foreign_identity = obs.foreign_inflight_identity;
                }
                if prior_turn_finalized(obs.view) {
                    break WaitOutcome::Finalized;
                }
            }
            if cycle_start.elapsed() >= PENDING_START_BACKSTOP {
                break match view_fn(&shared, &record).await {
                    Some(obs) => {
                        if obs.foreign_inflight_identity.is_some() {
                            last_foreign_identity = obs.foreign_inflight_identity;
                        }
                        if backstop_claim_is_safe(obs.view) {
                            WaitOutcome::BackstopClaimSafe
                        } else {
                            WaitOutcome::BackstopForeignInflightLive
                        }
                    }
                    None => WaitOutcome::BackstopForeignInflightLive,
                };
            }
            tokio::time::sleep(PENDING_START_POLL).await;
        };
        if take_retired(&record) {
            return false;
        }

        match outcome {
            WaitOutcome::Finalized => {}
            WaitOutcome::BackstopClaimSafe => {
                tracing::warn!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    backstop_ms = PENDING_START_BACKSTOP.as_millis(),
                    backstop_cycle = backstop_cycles,
                    "tui_direct_pending_start: prior turn did not finalize within backstop, but the prior inflight is gone at the claim instant; claiming with fresh EOF offset"
                );
            }
            WaitOutcome::BackstopForeignInflightLive => {
                // #4030 + #3982: before escalating, try the bounded stale-foreign
                // recovery hook. It first demotes a FOREIGN inflight whose
                // `updated_at` crossed the #4020 120s positive-stale gate AND
                // whose relay frontier never advanced despite captured output;
                // then it falls back to the #3982 producer-dead SessionBoundRelay
                // orphan downgrade. Either success only causes an immediate
                // re-evaluation; the worker never claims on this stale view.
                let reclaim_outcome = reclaim_orphan_fn(&shared, &record).await;
                if reclaim_outcome.is_reclaimed() {
                    tracing::warn!(
                        provider = %record.provider,
                        channel_id = record.channel_id,
                        tmux_session_name = %record.tmux_session_name,
                        anchor_message_id = record.anchor_message_id,
                        backstop_cycle = backstop_cycles,
                        event = reclaim_outcome.event_key(),
                        "tui_direct_pending_start: reclaimed/demoted a stale FOREIGN inflight blocking this synthetic start; re-evaluating immediately before any claim (#4030/#3982)"
                    );
                    continue;
                }
                // No stale/demotable row matched. Keep the existing bounded
                // escalation/abort behavior; a failed recovery attempt must not
                // turn into an infinite spin or an unsafe overwrite.
                backstop_cycles = backstop_cycles.saturating_add(1);
                if backstop_cycles >= PENDING_START_MAX_BACKSTOP_CYCLES {
                    if record.captured_source.is_some() {
                        // A failed inline save captured undelivered bytes before this
                        // foreign owner appeared. Keep that restart obligation; a
                        // predecessor's completion cannot settle this source range.
                        update_claim_attempt_count(&mut record, PENDING_START_MAX_CLAIM_ATTEMPTS);
                        release_prompt_anchor_slot(&record);
                        tracing::warn!(
                            channel_id = record.channel_id,
                            anchor_message_id = record.anchor_message_id,
                            "captured TUI source still blocked by foreign owner; retaining pending restart obligation"
                        );
                        return false;
                    }
                    // ABORT SAFELY (P1-1): a foreign prior inflight stayed live
                    // across the escalation budget. We refuse to overwrite it.
                    // Surface an observability event and drop only the synthetic
                    // OWNERSHIP claim (the provider prompt was already submitted;
                    // the watcher/bridge still relays its output).
                    // #3296: WARN, not ERROR — this branch fires by definition
                    // only when a FOREIGN inflight is live on the SAME channel,
                    // i.e. the input was already submitted and usually merges
                    // into the prior owner's turn (a normal outcome, not a
                    // failure). The event key is load-bearing — never change it.
                    // P2-G: this abandon leaks the shared prompt-anchor slot
                    // exactly like the exhaustion branch below — the prior
                    // owner's identity-guarded completion can never clear OUR
                    // anchor out of it. Release it here (process-local only).
                    if take_retired(&record) {
                        return false;
                    }
                    let anchor_slot_released = release_prompt_anchor_slot(&record);
                    tracing::warn!(
                        provider = %record.provider,
                        channel_id = record.channel_id,
                        tmux_session_name = %record.tmux_session_name,
                        anchor_message_id = record.anchor_message_id,
                        backstop_cycles,
                        anchor_slot_released,
                        waited_ms = worker_start.elapsed().as_millis(),
                        event = "tui_direct_pending_start.backstop_abort_foreign_inflight_live",
                        "tui_direct_pending_start: prior inflight stayed LIVE across the backstop escalation budget; ABORTING the synthetic turn-start claim without overwriting the live prior turn — input already submitted; abort marker recorded, reconcile lands ✅ via prior-owner completion or ⚠ via TTL fallback (#3296)"
                    );
                    // #3282/#3296: no claim will ever run for this anchor, so
                    // the normal `⏳ → ✅` completion never fires — record the
                    // durable aborted-anchor marker here (the anchor keeps its
                    // ⏳; the watcher drain / TTL sweep own the reconcile),
                    // pinning the last-view foreign identity (codex r2).
                    abort_cleanup_fn(&shared, &record, last_foreign_identity.clone()).await;
                    delete(&record);
                    // #3540 (B′ — defense-in-depth, NO EVICT): the pending gate is
                    // now released (`delete` above), but a follow-up the user sent
                    // while this synthetic start was deferring is still parked in
                    // the mailbox queue behind a QUEUE-ACK. If the FOREIGN inflight
                    // we were deferring on is a phantom (#3540 root cause: a
                    // watermark-reset re-claim whose commit will never arrive), the
                    // queued follow-up would otherwise stay parked until the
                    // ABORT_MARKER_TTL sweep. Kick the EXISTING mailbox dispatch
                    // path once so the follow-up promotes promptly. This clears /
                    // resets / deletes NO inflight — `kickoff_idle_queues` routes
                    // through `mailbox_try_start_turn_kinded`, which (a) starts a
                    // fresh turn if the slot is genuinely free, or (b) MERGES into a
                    // still-live prior turn (worst case = normal merge, zero live
                    // loss). The phantom row, if any, is reaped later by its own
                    // commit/finalize or the bounded ⏳ sweep — never evicted here.
                    promote_queued_follow_up_after_abort(&shared, &record);
                    return false;
                }
                tracing::warn!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    backstop_cycle = backstop_cycles,
                    max_cycles = PENDING_START_MAX_BACKSTOP_CYCLES,
                    "tui_direct_pending_start: backstop expired but a FOREIGN prior inflight is still live; refusing to overwrite, re-deferring (bounded escalation)"
                );
                // Re-defer: another full wait window.
                continue;
            }
        }

        // ---- Claim. Only delete the durable record on a SUCCESSFUL claim. ----
        if restore_gate::already_finished(&shared, &record).await && take_retired(&record) {
            return false;
        }
        let claimed = claim_fn(&shared, &record).await;
        if claimed {
            tracing::info!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                waited_ms = worker_start.elapsed().as_millis(),
                backstop_cycles,
                claim_attempts,
                "tui_direct_pending_start: deferred synthetic turn-start claimed after prior turn finalized"
            );
            // #3303: record the own-identity DeferredClaim marker BEFORE the
            // durable record delete (a crash between the two re-claims on
            // restart and re-records idempotently — the marker stem
            // overwrites). Fail-open: nothing in there can fail the claim.
            record_deferred_claim_marker_if_watcher_owned(&record);
            // Delete only AFTER a successful claim (P1-2). A crash between the
            // inflight save and this delete is healed on restart: the worker
            // re-runs and the claim adopts the matching anchor's existing
            // inflight idempotently, then deletes.
            delete(&record);
            return true;
        }

        if take_retired(&record) {
            return false;
        }
        // Transient claim failure: do NOT delete (P1-2). Retry, bounded.
        claim_attempts = claim_attempts.saturating_add(1);
        update_claim_attempt_count(&mut record, claim_attempts);
        if claim_attempts >= PENDING_START_MAX_CLAIM_ATTEMPTS {
            // #5833 E11: no lifecycle will ever complete for this anchor in
            // THIS process, so release the shared prompt-anchor slot before the
            // early return (the durable record still stays for restart retry).
            let anchor_slot_released = release_prompt_anchor_slot(&record);
            tracing::error!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                claim_attempts,
                anchor_slot_released,
                waited_ms = worker_start.elapsed().as_millis(),
                event = "tui_direct_pending_start.claim_retry_exhausted",
                "tui_direct_pending_start: claim returned false across the retry budget (another turn owns the mailbox or saves keep failing); abandoning the synthetic ownership claim to avoid an unbounded spin (record retained for restart re-attempt)"
            );
            // Leave the durable record in place: a later restart restore will
            // re-attempt idempotently rather than silently lose the prompt.
            return false;
        }
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            claim_attempt = claim_attempts,
            max_attempts = PENDING_START_MAX_CLAIM_ATTEMPTS,
            "tui_direct_pending_start: claim returned false (transient); retaining durable record and retrying"
        );
        tokio::time::sleep(PENDING_START_CLAIM_RETRY_BACKOFF).await;
        // Loop back: re-confirm the prior turn is still finalized, then re-claim.
    }
}

/// #5833 (E11) + review P2-G: release the process-local `(provider, tmux)`
/// PROMPT ANCHOR slot when this deferred synthetic start is ABANDONED (terminal
/// backstop ABORT or claim-retry exhaustion). `record_prompt_anchor` stamps that
/// single per-pane slot for BOTH the inline and deferred submit paths, but only
/// a COMPLETED lifecycle ever clears it — so an abandoned start parks its anchor
/// id there forever and the next response with no pinned anchor of its own
/// inherits that stale placeholder (E11: a 93s-old abandoned anchor was edited
/// by a later turn's adapter). Only PROCESS-LOCAL state is released; the durable
/// record / abort-marker lifecycle is untouched. [`clear_prompt_anchor_for_response`]
/// is identity-guarded (it removes the slot ONLY while it still equals the anchor
/// passed here), so a NEWER turn that already overwrote the slot keeps its anchor.
fn release_prompt_anchor_slot(record: &TuiDirectPendingStart) -> bool {
    clear_prompt_anchor_for_response(
        &record.provider,
        &record.tmux_session_name,
        TuiPromptAnchor {
            channel_id: record.channel_id,
            message_id: record.anchor_message_id,
        },
    )
}

fn record_deferred_claim_marker_if_watcher_owned(record: &TuiDirectPendingStart) {
    record_claim_marker_if_watcher_owned(
        &record.provider,
        record.channel_id,
        record.anchor_message_id,
        &record.tmux_session_name,
    );
}

/// #3540 (B′): after the terminal backstop ABORT has run `abort_cleanup_fn` and
/// `delete(&record)` (pending gate released), kick the EXISTING mailbox dispatch
/// path ONCE so a follow-up parked behind a QUEUE-ACK promotes promptly instead
/// of waiting out the bounded ⏳ sweep when the deferred-on FOREIGN inflight was
/// a phantom.
///
/// NO-EVICT INVARIANT (load-bearing): this function does NOT clear / reset /
/// `save_inflight(empty)` / delete ANY inflight row. It only schedules
/// [`super::schedule_deferred_idle_queue_kickoff`], the same idempotent
/// queued-dispatch entrypoint the post-turn / catch-up paths already use. That
/// kickoff routes through `mailbox_try_start_turn_kinded`, which either starts a
/// fresh turn (slot genuinely free) or MERGES the follow-up into a still-live
/// prior turn — so even if the deferred-on row is in fact a live turn, the worst
/// case is a normal merge with ZERO live-turn loss. The serialization is the
/// channel lock the worker already holds (this runs before its `return`, under
/// `_guard`); the kickoff's own work is detached, so no new lock-order risk.
/// Fail-soft: an unparseable provider only warns — the ABORT path is otherwise
/// unchanged (pre-#3540 behavior: the follow-up waits for the sweep).
fn promote_queued_follow_up_after_abort(shared: &Arc<SharedData>, record: &TuiDirectPendingStart) {
    let Some(provider) = crate::services::provider::ProviderKind::from_str(&record.provider) else {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            anchor_message_id = record.anchor_message_id,
            "tui_direct_pending_start: unparseable provider; skipping post-abort queue promote (fail-open — follow-up still drains via the bounded sweep) (#3540)"
        );
        return;
    };
    let channel_id = poise::serenity_prelude::ChannelId::new(record.channel_id);
    tracing::info!(
        provider = provider.as_str(),
        channel_id = record.channel_id,
        anchor_message_id = record.anchor_message_id,
        "tui_direct_pending_start: post-abort queue promote — kicking the existing mailbox dispatch once so a queued follow-up is not parked until the ⏳ sweep; NO inflight is cleared/reset/deleted (#3540 B′)"
    );
    #[cfg(test)]
    {
        // Test seam: record that the promote fired exactly once without spawning
        // the real detached kickoff task (which would leak past the test and, with
        // a test `shared`, has no cached ctx/token to act on anyway). Production
        // (below) takes the real path.
        POST_ABORT_PROMOTE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = (shared, channel_id, &provider);
        return;
    }
    #[cfg(not(test))]
    super::schedule_deferred_idle_queue_kickoff(
        shared.clone(),
        provider,
        channel_id,
        "tui_direct_pending_start backstop abort follow-up promote (#3540)",
    );
}

/// #3540 (B′) test seam: counts `promote_queued_follow_up_after_abort` firings so
/// the ABORT-path regression test can assert it ran EXACTLY ONCE while the claim
/// (inflight write) NEVER ran — proving the queue is promoted without evicting or
/// clearing any inflight row.
#[cfg(test)]
static POST_ABORT_PROMOTE_CALLS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// #3350 issue-3: the observer INLINE-claim wiring, separated so a unit test
/// can pin it — `relay_observed_prompt` must record the #3303 DeferredClaim
/// marker IFF the inline synthetic claim actually claimed, forwarding the
/// prompt's exact `(provider, channel, anchor, tmux)` identity. `recorder` is
/// injected (`FnOnce` flavor of the `ClaimFn` injection convention);
/// production passes [`record_claim_marker_if_watcher_owned`] itself, so the
/// signature match is compiler-pinned at the call site.
pub(in crate::services::discord) fn record_inline_claim_marker_if_claimed(
    claimed: bool,
    provider: &str,
    channel_id: u64,
    anchor_message_id: u64,
    tmux_session_name: &str,
    recorder: impl FnOnce(&str, u64, u64, &str),
) {
    if claimed {
        recorder(provider, channel_id, anchor_message_id, tmux_session_name);
    }
}

#[cfg(test)]
mod tests;
