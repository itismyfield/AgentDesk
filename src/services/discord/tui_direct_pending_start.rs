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

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::SharedData;

/// Conservative poll interval for the wait predicate.
pub(super) const PENDING_START_POLL: Duration = Duration::from_millis(100);

/// Backstop matching `turn_finalizer::GATE_BACKSTOP` (8s). After this single
/// wait window expires WITHOUT the prior turn finalizing, the worker does NOT
/// blindly claim (that would overwrite a still-LIVE prior inflight and resurrect
/// the original #3154 wrong-turn-finalize / `response_sent_offset` regression).
/// Instead it re-checks at the claim instant whether the prior inflight is truly
/// gone; if a foreign prior inflight is still live it keeps deferring under
/// bounded escalation (see [`PENDING_START_MAX_BACKSTOP_CYCLES`]).
pub(super) const PENDING_START_BACKSTOP: Duration = Duration::from_secs(8);

/// Bounded escalation cap. Each cycle is one `PENDING_START_BACKSTOP` wait
/// window during which the prior turn never finalized AND, at the claim instant,
/// a FOREIGN prior inflight was still live (so claiming would overwrite it).
/// After this many such cycles the worker ABORTS the synthetic start safely
/// (surfaces an observability event + deletes the durable record) rather than
/// either overwriting a live prior turn or leaking the record forever. The
/// provider prompt itself is never resubmitted; only the synthetic OWNERSHIP
/// claim is abandoned — the watcher/bridge still relays the provider's output.
pub(super) const PENDING_START_MAX_BACKSTOP_CYCLES: u32 = 4;

/// On a transient claim failure (`claimed == false`: another turn briefly owns
/// the mailbox, or an inflight save failed) the worker MUST NOT delete the
/// durable record (that would lose a Discord-submitted prompt — the original
/// turn-loss bug). It re-defers and retries, bounded by this cap, so a wedged
/// claim path cannot spin forever.
pub(super) const PENDING_START_MAX_CLAIM_ATTEMPTS: u32 = 5;

/// Backoff between claim retries after a transient `claimed == false`.
pub(super) const PENDING_START_CLAIM_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// Lifecycle state of a durable pending-start record. Kept tiny and
/// string-serialized so a forward/backward dcserver swap reads it tolerantly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(super) enum PendingStartState {
    /// Persisted; worker has not yet completed the claim.
    #[default]
    Waiting,
}

/// Durable record describing a TUI-direct synthetic turn-start that must be
/// claimed only AFTER the prior turn on the same channel finalizes.
///
/// All fields are primitives so the JSON survives a dcserver version swap; the
/// lease is rehydrated from these fields on restart
/// (`record_external_input_turn_lease`), never from a serialized lease struct.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TuiDirectPendingStart {
    pub provider: String,
    pub channel_id: u64,
    pub tmux_session_name: String,
    pub prompt_text: String,
    pub anchor_message_id: u64,
    /// Lease owner (`ExternalInputRelayOwner::as_str`) captured at persist time.
    pub lease_relay_owner: String,
    /// Lease runtime kind (`RuntimeHandoffKind::as_str`), if known.
    pub lease_runtime_kind: Option<String>,
    pub lease_turn_id: Option<String>,
    pub lease_session_key: Option<String>,
    /// Restart generation at persist time (the `turn_finalizer::TurnKey`
    /// generation the claim registers under).
    pub generation: u64,
    pub created_at_ms: u64,
    pub observed_at_ms: u64,
    #[serde(default)]
    pub state: PendingStartState,
    #[serde(default)]
    pub attempt_count: u32,
}

impl TuiDirectPendingStart {
    /// Stable filename key for the record (one record per anchor; a channel may
    /// briefly hold several queued anchors which all drain FIFO under the lock).
    fn file_stem(&self) -> String {
        format!(
            "{}_{}_{}",
            self.provider, self.channel_id, self.anchor_message_id
        )
    }
}

// ---------------------------------------------------------------------------
// Per-(provider, channel) serialization lock table
// ---------------------------------------------------------------------------

/// Module-static lock table (smaller surface than threading a field through
/// `SharedData`). One `tokio::Mutex` per `(provider, channel_id)`; the worker
/// holds it for the whole wait+claim so same-channel pending prompts serialize
/// FIFO while different channels run fully in parallel.
static CHANNEL_LOCKS: LazyLock<Mutex<HashMap<(String, u64), Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn channel_lock(provider: &str, channel_id: u64) -> Arc<tokio::sync::Mutex<()>> {
    let mut table = CHANNEL_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    table
        .entry((provider.to_string(), channel_id))
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

// ---------------------------------------------------------------------------
// In-memory presence index (cheap gate probe — avoids a filesystem scan on the
// hot watcher / idle-queue paths)
// ---------------------------------------------------------------------------

static PRESENT: LazyLock<Mutex<HashMap<(String, u64), u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn mark_present(provider: &str, channel_id: u64) {
    let mut map = PRESENT.lock().unwrap_or_else(|e| e.into_inner());
    *map.entry((provider.to_string(), channel_id)).or_insert(0) += 1;
}

fn mark_absent(provider: &str, channel_id: u64) {
    let mut map = PRESENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(count) = map.get_mut(&(provider.to_string(), channel_id)) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            map.remove(&(provider.to_string(), channel_id));
        }
    }
}

/// GATE probe consulted by the watcher no-inflight suppression and the idle
/// queue: is a synthetic turn-start pending (record persisted, inflight not yet
/// saved) for this provider/channel? While true, the watcher must LEAVE bytes
/// buffered and the idle queue must not kick normal work for this channel.
///
/// Cheap (in-memory) so it is safe to call inline on the hot paths. The durable
/// record is the source of truth on restart; this index is rebuilt by
/// `restore_pending_starts`.
pub(super) fn pending_synthetic_start_present(provider: &str, channel_id: u64) -> bool {
    let map = PRESENT.lock().unwrap_or_else(|e| e.into_inner());
    map.get(&(provider.to_string(), channel_id))
        .copied()
        .unwrap_or(0)
        > 0
}

/// Re-mark a record present during restart restore. [`load_all`] reads the
/// durable store but does not touch the in-memory index; this restores the gate
/// state before the respawned worker's first poll. The worker's terminal
/// [`delete`] balances it.
pub(super) fn mark_present_on_restore(provider: &str, channel_id: u64) {
    mark_present(provider, channel_id);
}

#[cfg(test)]
pub(super) fn reset_present_for_tests() {
    PRESENT.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

// ---------------------------------------------------------------------------
// Durable store
// ---------------------------------------------------------------------------

fn root() -> Option<std::path::PathBuf> {
    super::runtime_store::tui_direct_pending_start_root()
}

/// Persist (or update) a pending-start record and mark it present in the
/// in-memory index. Called BEFORE any wait, immediately after the anchor/lease
/// are created.
pub(super) fn persist(record: &TuiDirectPendingStart) -> Result<(), String> {
    mark_present(&record.provider, record.channel_id);
    let Some(root) = root() else {
        // No runtime root (tests / unconfigured): the in-memory presence index
        // still gates the watcher / idle queue for this process lifetime.
        return Ok(());
    };
    let path = root.join(format!("{}.json", record.file_stem()));
    let data = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    super::runtime_store::critical_atomic_write(
        &path,
        &data,
        super::runtime_store::AtomicWriteContext::new("tui_direct_pending_start")
            .provider(&record.provider)
            .channel_id(record.channel_id),
    )
}

/// Delete a pending-start record AFTER the inflight save succeeds (or when the
/// worker gives up). Idempotent.
pub(super) fn delete(record: &TuiDirectPendingStart) {
    mark_absent(&record.provider, record.channel_id);
    if let Some(root) = root() {
        let path = root.join(format!("{}.json", record.file_stem()));
        let _ = std::fs::remove_file(path);
    }
}

/// Load all durable pending-start records (restart restore).
pub(super) fn load_all() -> Vec<TuiDirectPendingStart> {
    let Some(root) = root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(record) = serde_json::from_str::<TuiDirectPendingStart>(&text)
        {
            out.push(record);
        }
    }
    // P2-1: `read_dir` yields entries in an arbitrary (filesystem) order. The
    // detached workers serialize per (provider, channel) under `channel_lock`,
    // so the ORDER in which we spawn same-channel records decides which acquires
    // the lock first — i.e. the FIFO drain order after a restart. Sort by the
    // persisted observed/creation timestamps so intra-channel FIFO matches the
    // original submission order (anchor_message_id as a final monotonic
    // tiebreak — Discord snowflakes are time-ordered).
    out.sort_by(|a, b| {
        a.observed_at_ms
            .cmp(&b.observed_at_ms)
            .then(a.created_at_ms.cmp(&b.created_at_ms))
            .then(a.anchor_message_id.cmp(&b.anchor_message_id))
    });
    out
}

// ---------------------------------------------------------------------------
// Pure decision functions (truth-table tested — no I/O, no clock)
// ---------------------------------------------------------------------------

/// Inputs to [`prior_turn_finalized`]. Captured by the worker each poll from
/// inflight/mailbox/runtime-binding state so the decision is pure and testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PriorTurnView {
    /// An inflight row exists for this provider/channel.
    pub inflight_present: bool,
    /// The present inflight (if any) is THIS pending start's own anchor — a
    /// crash-after-save-before-delete restore, idempotently adoptable.
    pub inflight_is_own_anchor: bool,
    /// The mailbox has an active blocking (non-background) turn.
    pub mailbox_blocking_turn_present: bool,
    /// The mailbox's active turn (if any) is THIS pending start's own anchor.
    pub mailbox_turn_is_own_anchor: bool,
    /// A runtime binding resolves for the tmux session (needed to seed a fresh
    /// EOF offset at claim time).
    pub runtime_binding_present: bool,
}

/// The prior turn is finalized (relay drained) iff:
/// (a) there is no prior inflight for this provider/channel, OR the existing
///     inflight is THIS anchor (idempotent restore); AND
/// (b) the mailbox has no active blocking turn, OR it is THIS anchor; AND
/// (c) a runtime binding exists (so the claim can seed a fresh EOF offset).
///
/// "Prior" is the discriminator: an inflight/mailbox-turn that is OUR OWN anchor
/// is not a blocker — it is the partially-applied result of THIS pending start
/// (e.g. crash-recovery) and is adopted idempotently.
pub(super) fn prior_turn_finalized(view: PriorTurnView) -> bool {
    let inflight_ok = !view.inflight_present || view.inflight_is_own_anchor;
    let mailbox_ok = !view.mailbox_blocking_turn_present || view.mailbox_turn_is_own_anchor;
    inflight_ok && mailbox_ok && view.runtime_binding_present
}

/// Backstop-instant collision guard (P1-1). After a backstop wait window
/// expired without the prior turn finalizing, the worker re-reads the view at
/// the claim instant. It may ONLY proceed to claim if doing so would not
/// overwrite a still-LIVE FOREIGN prior inflight. A prior inflight that is OUR
/// OWN anchor (crash-restore) is adoptable, so it never blocks.
///
/// Returns `true` when claiming is safe at the backstop instant (the foreign
/// prior inflight is gone / was only ever our own). Returns `false` when a
/// foreign prior inflight is STILL live — claiming now would resurrect the
/// original #3154 overwrite bug, so the worker must keep deferring (bounded).
pub(super) fn backstop_claim_is_safe(view: PriorTurnView) -> bool {
    // The ONLY thing the backstop relaxes is the mailbox-blocking and
    // runtime-binding waits (a wedged-but-present prior turn / a transiently
    // missing binding). It must NEVER relax the live-foreign-inflight guard:
    // overwriting a live prior inflight is the exact regression this fixes.
    !view.inflight_present || view.inflight_is_own_anchor
}

/// Decide whether [`relay_observed_prompt`] must DEFER the synthetic turn-start
/// off the observer loop (persist a record + spawn the worker) instead of
/// claiming inline.
///
/// Defer when the prior turn is NOT finalized — i.e. claiming inline now would
/// reproduce the offset collision. When the prior turn is already finalized the
/// inline claim is safe and the deferral machinery is skipped entirely (keeps
/// the common no-interleave path on its existing fast path).
pub(super) fn should_defer_synthetic_turn_start(prior: PriorTurnView) -> bool {
    !prior_turn_finalized(prior)
}

// ---------------------------------------------------------------------------
// Detached worker
// ---------------------------------------------------------------------------

/// The claim action the worker runs once the prior turn is finalized. Provided
/// by [`super::tui_prompt_relay`] (where `claim_tui_direct_synthetic_turn` is
/// private). Returns `true` when an inflight was saved (claimed) AND the claim's
/// `relay_owner` was adopted into the in-memory lease (so the observer-side
/// BridgeAdapter tail stops once the watcher owns the turn — P1-3); `false` on a
/// transient failure (another turn briefly owns the mailbox, or an inflight save
/// failed), in which case the worker re-defers and retries WITHOUT deleting the
/// durable record (P1-2 — never lose a Discord-submitted prompt).
pub(super) type ClaimFn = Box<
    dyn for<'a> Fn(
            &'a Arc<SharedData>,
            &'a TuiDirectPendingStart,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
        + Send
        + Sync,
>;

/// Build the per-poll [`PriorTurnView`]. Provided by [`super::tui_prompt_relay`]
/// (it owns inflight/mailbox/runtime-binding access). Returns `None` when the
/// view cannot be computed yet (e.g. mailbox unavailable) — treated as "not
/// finalized" so the worker keeps waiting.
pub(super) type ViewFn = Box<
    dyn for<'a> Fn(
            &'a Arc<SharedData>,
            &'a TuiDirectPendingStart,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<PriorTurnView>> + Send + 'a>,
        > + Send
        + Sync,
>;

/// Spawn the DETACHED per-channel worker. Acquires the channel lock (FIFO
/// serialization), polls the wait predicate until the prior turn finalizes (or
/// the 8s backstop fires), runs the claim, and deletes the record. Returns
/// immediately so the observer loop is never blocked.
pub(super) fn spawn_worker(
    shared: Arc<SharedData>,
    record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
) {
    super::task_supervisor::spawn_observed("tui_direct_pending_start_worker", async move {
        run_worker(shared, record, view_fn, claim_fn).await;
    });
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

async fn run_worker(
    shared: Arc<SharedData>,
    record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
) {
    let lock = channel_lock(&record.provider, record.channel_id);
    let _guard = lock.lock().await;

    let mut backstop_cycles: u32 = 0;
    let mut claim_attempts: u32 = 0;
    let worker_start = tokio::time::Instant::now();

    loop {
        // ---- Wait window: poll until finalized or backstop expiry. ----
        let cycle_start = tokio::time::Instant::now();
        let outcome = loop {
            if let Some(view) = view_fn(&shared, &record).await
                && prior_turn_finalized(view)
            {
                break WaitOutcome::Finalized;
            }
            if cycle_start.elapsed() >= PENDING_START_BACKSTOP {
                let view = view_fn(&shared, &record).await;
                break match view {
                    Some(view) if backstop_claim_is_safe(view) => WaitOutcome::BackstopClaimSafe,
                    _ => WaitOutcome::BackstopForeignInflightLive,
                };
            }
            tokio::time::sleep(PENDING_START_POLL).await;
        };

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
                backstop_cycles = backstop_cycles.saturating_add(1);
                if backstop_cycles >= PENDING_START_MAX_BACKSTOP_CYCLES {
                    // ABORT SAFELY (P1-1): a foreign prior inflight stayed live
                    // across the escalation budget. We refuse to overwrite it.
                    // Surface an observability event and drop only the synthetic
                    // OWNERSHIP claim (the provider prompt was already submitted;
                    // the watcher/bridge still relays its output).
                    tracing::error!(
                        provider = %record.provider,
                        channel_id = record.channel_id,
                        tmux_session_name = %record.tmux_session_name,
                        anchor_message_id = record.anchor_message_id,
                        backstop_cycles,
                        waited_ms = worker_start.elapsed().as_millis(),
                        event = "tui_direct_pending_start.backstop_abort_foreign_inflight_live",
                        "tui_direct_pending_start: prior inflight stayed LIVE across the backstop escalation budget; ABORTING the synthetic turn-start claim without overwriting the live prior turn (provider output still relays via the prior turn's owner)"
                    );
                    delete(&record);
                    return;
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
            // Delete only AFTER a successful claim (P1-2). A crash between the
            // inflight save and this delete is healed on restart: the worker
            // re-runs and the claim adopts the matching anchor's existing
            // inflight idempotently, then deletes.
            delete(&record);
            return;
        }

        // Transient claim failure: do NOT delete (P1-2). Retry, bounded.
        claim_attempts = claim_attempts.saturating_add(1);
        if claim_attempts >= PENDING_START_MAX_CLAIM_ATTEMPTS {
            tracing::error!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                claim_attempts,
                waited_ms = worker_start.elapsed().as_millis(),
                event = "tui_direct_pending_start.claim_retry_exhausted",
                "tui_direct_pending_start: claim returned false across the retry budget (another turn owns the mailbox or saves keep failing); abandoning the synthetic ownership claim to avoid an unbounded spin (record retained for restart re-attempt)"
            );
            // Leave the durable record in place: a later restart restore will
            // re-attempt idempotently rather than silently lose the prompt.
            return;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The presence index (`PRESENT`) and the durable store root are PROCESS-WIDE
    /// statics. Any test that calls `persist` / `delete` / `reset_present_for_tests`
    /// / drives `run_worker` mutates them, so concurrent tests would stomp each
    /// other (e.g. one test's `reset_present_for_tests` clearing another's gate).
    /// Serialize all such tests on this module-local lock.
    fn worker_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn base_view() -> PriorTurnView {
        PriorTurnView {
            inflight_present: false,
            inflight_is_own_anchor: false,
            mailbox_blocking_turn_present: false,
            mailbox_turn_is_own_anchor: false,
            runtime_binding_present: true,
        }
    }

    #[test]
    fn finalized_when_no_prior_inflight_and_no_blocking_turn_and_binding() {
        assert!(prior_turn_finalized(base_view()));
        assert!(!should_defer_synthetic_turn_start(base_view()));
    }

    #[test]
    fn not_finalized_while_prior_inflight_undrained() {
        let view = PriorTurnView {
            inflight_present: true,
            ..base_view()
        };
        assert!(!prior_turn_finalized(view));
        assert!(
            should_defer_synthetic_turn_start(view),
            "an undrained prior inflight (the interleave bug) MUST defer"
        );
    }

    #[test]
    fn own_anchor_inflight_does_not_block_idempotent_restore() {
        let view = PriorTurnView {
            inflight_present: true,
            inflight_is_own_anchor: true,
            ..base_view()
        };
        assert!(
            prior_turn_finalized(view),
            "a crash-restored inflight for OUR OWN anchor is adopted, not waited on"
        );
    }

    #[test]
    fn not_finalized_while_mailbox_blocking_turn_present() {
        let view = PriorTurnView {
            mailbox_blocking_turn_present: true,
            ..base_view()
        };
        assert!(!prior_turn_finalized(view));
        assert!(should_defer_synthetic_turn_start(view));
    }

    #[test]
    fn own_anchor_mailbox_turn_does_not_block() {
        let view = PriorTurnView {
            mailbox_blocking_turn_present: true,
            mailbox_turn_is_own_anchor: true,
            ..base_view()
        };
        assert!(prior_turn_finalized(view));
    }

    #[test]
    fn not_finalized_without_runtime_binding() {
        let view = PriorTurnView {
            runtime_binding_present: false,
            ..base_view()
        };
        assert!(
            !prior_turn_finalized(view),
            "no runtime binding → cannot seed a fresh EOF offset → keep waiting"
        );
    }

    #[test]
    fn presence_index_marks_and_clears() {
        let _guard = worker_test_lock();
        reset_present_for_tests();
        let provider = "claude";
        let channel = 777u64;
        assert!(!pending_synthetic_start_present(provider, channel));
        mark_present(provider, channel);
        assert!(pending_synthetic_start_present(provider, channel));
        mark_present(provider, channel);
        mark_absent(provider, channel);
        assert!(
            pending_synthetic_start_present(provider, channel),
            "two pending starts on a channel: still present after one clears"
        );
        mark_absent(provider, channel);
        assert!(!pending_synthetic_start_present(provider, channel));
        reset_present_for_tests();
    }

    #[test]
    fn record_roundtrips_through_json() {
        let record = TuiDirectPendingStart {
            provider: "claude".to_string(),
            channel_id: 42,
            tmux_session_name: "tmux-abc".to_string(),
            prompt_text: "/loop do the thing".to_string(),
            anchor_message_id: 9001,
            lease_relay_owner: "bridge_adapter".to_string(),
            lease_runtime_kind: Some("claude_tui".to_string()),
            lease_turn_id: Some("turn-1".to_string()),
            lease_session_key: Some("sess-1".to_string()),
            generation: 7,
            created_at_ms: 1234,
            observed_at_ms: 1230,
            state: PendingStartState::Waiting,
            attempt_count: 0,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: TuiDirectPendingStart = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    fn record(provider: &str, channel_id: u64, anchor: u64) -> TuiDirectPendingStart {
        TuiDirectPendingStart {
            provider: provider.to_string(),
            channel_id,
            tmux_session_name: format!("tmux-{channel_id}"),
            prompt_text: "/loop tick".to_string(),
            anchor_message_id: anchor,
            lease_relay_owner: "bridge_adapter".to_string(),
            lease_runtime_kind: Some("claude_tui".to_string()),
            lease_turn_id: None,
            lease_session_key: None,
            generation: 0,
            created_at_ms: 0,
            observed_at_ms: 0,
            state: PendingStartState::Waiting,
            attempt_count: 0,
        }
    }

    /// #3154 interleave integration test (design point: tokio interleave with
    /// `tokio::time::pause()`):
    ///   - channel A's wakeup DEFERS while a seeded turn1 inflight is undrained;
    ///   - channel B relays FIRST (no cross-channel starvation: B's worker is on
    ///     a different channel lock and finishes immediately);
    ///   - A claims ONLY after turn1's inflight clears, and the EOF offset the
    ///     claim reads at THAT moment is recorded (asserting the claim is seeded
    ///     post-drain, never from the stale prior cursor).
    #[tokio::test(start_paused = true)]
    async fn channel_a_defers_until_prior_clears_while_channel_b_does_not_starve() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        let _guard = worker_test_lock();
        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();

        // ---- Channel A: prior turn1 inflight is UNDRAINED at first. ----
        let a_prior_undrained = Arc::new(AtomicBool::new(true));
        // The "EOF offset" the claim would read: starts at the stale prior
        // cursor (100) and only advances to the post-drain frontier (250) once
        // turn1's inflight clears. The claim must capture 250, not 100.
        let a_eof_when_claimed = Arc::new(AtomicU64::new(0));

        let a_undrained_for_view = a_prior_undrained.clone();
        let a_view: ViewFn = Box::new(move |_shared, _record| {
            let undrained = a_undrained_for_view.clone();
            Box::pin(async move {
                Some(PriorTurnView {
                    // turn1 inflight present until drained.
                    inflight_present: undrained.load(Ordering::SeqCst),
                    inflight_is_own_anchor: false,
                    mailbox_blocking_turn_present: false,
                    mailbox_turn_is_own_anchor: false,
                    runtime_binding_present: true,
                })
            })
        });
        let a_undrained_for_claim = a_prior_undrained.clone();
        let a_eof_for_claim = a_eof_when_claimed.clone();
        let a_claim: ClaimFn = Box::new(move |_shared, _record| {
            let undrained = a_undrained_for_claim.clone();
            let eof = a_eof_for_claim.clone();
            Box::pin(async move {
                // The relay cursor is the stale 100 while undrained, EOF 250 once
                // drained. The claim reads it FRESH at claim time.
                let offset = if undrained.load(Ordering::SeqCst) {
                    100
                } else {
                    250
                };
                eof.store(offset, Ordering::SeqCst);
                true
            })
        });

        let rec_a = record("claude", 1, 11);
        persist(&rec_a).unwrap();
        assert!(
            pending_synthetic_start_present("claude", 1),
            "A's pending start gates the watcher/idle-queue immediately"
        );
        let a_handle = tokio::spawn(run_worker(shared.clone(), rec_a, a_view, a_claim));

        // ---- Channel B: prior turn already finalized → relays immediately. ----
        let b_claimed = Arc::new(AtomicBool::new(false));
        let b_view: ViewFn = Box::new(move |_shared, _record| {
            Box::pin(async move {
                Some(PriorTurnView {
                    inflight_present: false,
                    inflight_is_own_anchor: false,
                    mailbox_blocking_turn_present: false,
                    mailbox_turn_is_own_anchor: false,
                    runtime_binding_present: true,
                })
            })
        });
        let b_claimed_for_claim = b_claimed.clone();
        let b_claim: ClaimFn = Box::new(move |_shared, _record| {
            let claimed = b_claimed_for_claim.clone();
            Box::pin(async move {
                claimed.store(true, Ordering::SeqCst);
                true
            })
        });
        let rec_b = record("claude", 2, 22);
        persist(&rec_b).unwrap();
        let b_handle = tokio::spawn(run_worker(shared.clone(), rec_b, b_view, b_claim));

        // B is on a DIFFERENT channel lock; it must finish without waiting for A.
        b_handle.await.unwrap();
        assert!(
            b_claimed.load(Ordering::SeqCst),
            "channel B must NOT be starved by channel A's deferral (no inline cross-channel wait)"
        );
        assert!(
            a_eof_when_claimed.load(Ordering::SeqCst) == 0,
            "channel A must STILL be waiting (its prior turn1 inflight has not drained)"
        );
        assert!(
            pending_synthetic_start_present("claude", 1),
            "A's pending start still gates while it waits"
        );

        // Now drain turn1 (the prior user turn's relay completes).
        a_prior_undrained.store(false, Ordering::SeqCst);
        // Let the ~100ms poll elapse under paused time.
        tokio::time::advance(PENDING_START_POLL * 2).await;
        a_handle.await.unwrap();

        assert_eq!(
            a_eof_when_claimed.load(Ordering::SeqCst),
            250,
            "channel A claimed ONLY after turn1 drained, seeding the FRESH post-drain EOF (250), \
             never the stale prior cursor (100) — this is what prevents the response_sent_offset \
             collision"
        );
        assert!(
            !pending_synthetic_start_present("claude", 1),
            "A's pending start cleared after the claim (gate releases)"
        );
        reset_present_for_tests();
    }

    // ====================================================================
    // #3154 P2-2 — codex P1/P2 regression coverage for the deferred-claim
    // safety properties. Each test drives the REAL `run_worker` (or the REAL
    // durable `load_all` restore path) and is RED→GREEN: a comment on each
    // assertion names the neutralization that makes it fail.
    // ====================================================================

    /// Pure-fn coverage for the P1-1 backstop collision guard. The guard must
    /// relax ONLY the mailbox-blocking / runtime-binding waits — never the
    /// live-foreign-inflight guard (that overwrite is the exact #3154 bug).
    #[test]
    fn backstop_claim_safe_only_when_foreign_inflight_gone() {
        // Foreign inflight still live → NOT safe (claiming would overwrite it).
        // RED if `backstop_claim_is_safe` were `true` (the old "claim anyway").
        let foreign_live = PriorTurnView {
            inflight_present: true,
            inflight_is_own_anchor: false,
            ..base_view()
        };
        assert!(
            !backstop_claim_is_safe(foreign_live),
            "a live FOREIGN inflight must block the backstop claim"
        );

        // No inflight → safe (wedged-but-cleared / binding-transient prior).
        assert!(backstop_claim_is_safe(PriorTurnView {
            inflight_present: false,
            ..base_view()
        }));

        // Our own anchor inflight → safe (idempotent crash-restore adoption).
        assert!(backstop_claim_is_safe(PriorTurnView {
            inflight_present: true,
            inflight_is_own_anchor: true,
            ..base_view()
        }));

        // A wedged-but-present mailbox turn with NO inflight is relaxed by the
        // backstop (this is what the backstop is FOR), so it claims-safe.
        assert!(backstop_claim_is_safe(PriorTurnView {
            inflight_present: false,
            mailbox_blocking_turn_present: true,
            ..base_view()
        }));
    }

    /// P2-2 (a): backstop expires while a FOREIGN prior inflight stays live
    /// across the WHOLE escalation budget. The worker must NEVER claim (no
    /// overwrite) and, after the budget, ABORT safely WITHOUT resubmitting —
    /// proven by the claim closure never running.
    #[tokio::test(start_paused = true)]
    async fn backstop_foreign_inflight_live_aborts_without_claim() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let _guard = worker_test_lock();
        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();

        // A foreign prior inflight is live FOREVER (never drains, never ours).
        let view: ViewFn = Box::new(move |_shared, _record| {
            Box::pin(async move {
                Some(PriorTurnView {
                    inflight_present: true,
                    inflight_is_own_anchor: false,
                    mailbox_blocking_turn_present: true,
                    mailbox_turn_is_own_anchor: false,
                    runtime_binding_present: true,
                })
            })
        });

        let claim_calls = Arc::new(AtomicU32::new(0));
        let claim_calls_for_fn = claim_calls.clone();
        let claim: ClaimFn = Box::new(move |_shared, _record| {
            let calls = claim_calls_for_fn.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                true
            })
        });

        let rec = record("claude", 10, 100);
        persist(&rec).unwrap();
        assert!(pending_synthetic_start_present("claude", 10));

        let handle = tokio::spawn(run_worker(shared.clone(), rec, view, claim));

        // Advance through the full escalation budget of backstop windows.
        for _ in 0..(PENDING_START_MAX_BACKSTOP_CYCLES + 1) {
            tokio::time::advance(PENDING_START_BACKSTOP + PENDING_START_POLL * 2).await;
            tokio::task::yield_now().await;
        }
        handle.await.unwrap();

        assert_eq!(
            claim_calls.load(Ordering::SeqCst),
            0,
            "the claim must NEVER run while a foreign inflight is live — claiming \
             would overwrite the live prior turn (the #3154 regression). RED if the \
             backstop reverts to 'claim anyway' on expiry."
        );
        assert!(
            !pending_synthetic_start_present("claude", 10),
            "after the escalation budget the worker ABORTS and drops only the \
             ownership record (no prompt resubmit). RED if abort leaks the record \
             or never fires."
        );
        reset_present_for_tests();
    }

    /// P2-2 (b): the claim returns `false` (transient — another turn briefly
    /// owns the mailbox). The worker MUST retain the durable record (never lose a
    /// Discord-submitted prompt) and retry; once the claim later succeeds it
    /// deletes. Proves the record is RETAINED across the false returns.
    #[tokio::test(start_paused = true)]
    async fn claim_false_retains_record_and_retries() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let _guard = worker_test_lock();
        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();

        // Prior turn is finalized immediately — the wait window is not the point.
        let view: ViewFn =
            Box::new(move |_shared, _record| Box::pin(async move { Some(base_view()) }));

        // First two claims fail (transient), the third succeeds.
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_for_fn = attempts.clone();
        let claim: ClaimFn = Box::new(move |_shared, _record| {
            let attempts = attempts_for_fn.clone();
            Box::pin(async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst);
                n >= 2
            })
        });

        let rec = record("claude", 11, 111);
        persist(&rec).unwrap();
        let handle = tokio::spawn(run_worker(shared.clone(), rec, view, claim));

        // Drive the retry backoffs. After the first false, assert the record is
        // STILL present (RETAINED) before the eventual success deletes it.
        tokio::task::yield_now().await;
        tokio::time::advance(PENDING_START_CLAIM_RETRY_BACKOFF + PENDING_START_POLL).await;
        tokio::task::yield_now().await;
        assert!(
            pending_synthetic_start_present("claude", 11),
            "a transient claim==false MUST NOT delete the durable record (the \
             turn-loss bug). RED if the worker deletes on claim==false."
        );

        // Let the remaining retries elapse and the third claim succeed.
        for _ in 0..PENDING_START_MAX_CLAIM_ATTEMPTS {
            tokio::time::advance(PENDING_START_CLAIM_RETRY_BACKOFF + PENDING_START_POLL).await;
            tokio::task::yield_now().await;
        }
        handle.await.unwrap();

        assert!(
            attempts.load(Ordering::SeqCst) >= 3,
            "the worker retried the claim after the false returns (did not bail)"
        );
        assert!(
            !pending_synthetic_start_present("claude", 11),
            "after the claim finally succeeded the record is deleted (gate releases)"
        );
        reset_present_for_tests();
    }

    /// P2-2 (b'): the claim returns `false` ACROSS THE WHOLE retry budget. The
    /// worker exhausts attempts but STILL must NOT delete the record (it is left
    /// for a restart re-attempt — never silently lose the prompt).
    #[tokio::test(start_paused = true)]
    async fn claim_false_exhausted_still_retains_record() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let _guard = worker_test_lock();
        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();
        let view: ViewFn =
            Box::new(move |_shared, _record| Box::pin(async move { Some(base_view()) }));

        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_for_fn = attempts.clone();
        let claim: ClaimFn = Box::new(move |_shared, _record| {
            let attempts = attempts_for_fn.clone();
            Box::pin(async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                false // never succeeds
            })
        });

        let rec = record("claude", 12, 122);
        persist(&rec).unwrap();
        let handle = tokio::spawn(run_worker(shared.clone(), rec, view, claim));

        for _ in 0..(PENDING_START_MAX_CLAIM_ATTEMPTS + 2) {
            tokio::time::advance(PENDING_START_CLAIM_RETRY_BACKOFF + PENDING_START_POLL).await;
            tokio::task::yield_now().await;
        }
        handle.await.unwrap();

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            PENDING_START_MAX_CLAIM_ATTEMPTS,
            "the worker bounds the retries at PENDING_START_MAX_CLAIM_ATTEMPTS (no spin)"
        );
        assert!(
            pending_synthetic_start_present("claude", 12),
            "on retry exhaustion the record is RETAINED for restart re-attempt — \
             RED if the worker deletes after exhausting claims (turn-loss)."
        );
        reset_present_for_tests();
    }

    /// P2-2 (d): durable restore roundtrip. Several same-channel records are
    /// persisted out of order on disk; `load_all` must return them in FIFO order
    /// (observed_at, created_at, anchor tiebreak) so the respawned workers drain
    /// in submission order. Drives the REAL durable store under a temp root.
    #[test]
    fn durable_restore_roundtrip_loads_fifo_order() {
        let _guard = worker_test_lock();
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        std::fs::create_dir_all(root().expect("durable root configured under temp")).unwrap();

        reset_present_for_tests();

        // Persist three same-channel records whose observed_at order is the
        // REVERSE of their filesystem-key order, so an unsorted read_dir would
        // not yield FIFO by accident.
        let mut first = record("claude", 50, 9003);
        first.observed_at_ms = 100;
        first.created_at_ms = 100;
        let mut second = record("claude", 50, 9002);
        second.observed_at_ms = 200;
        second.created_at_ms = 200;
        let mut third = record("claude", 50, 9001);
        third.observed_at_ms = 300;
        third.created_at_ms = 300;
        // Persist in a scrambled order.
        persist(&second).unwrap();
        persist(&third).unwrap();
        persist(&first).unwrap();

        let loaded = load_all();
        let same_channel: Vec<u64> = loaded
            .iter()
            .filter(|r| r.channel_id == 50)
            .map(|r| r.observed_at_ms)
            .collect();
        assert_eq!(
            same_channel,
            vec![100, 200, 300],
            "load_all must return same-channel records in FIFO (observed_at) order \
             so respawned workers drain in submission order — RED if the sort is \
             removed (filesystem order would scramble them)."
        );

        // The roundtrip preserves field fidelity (no resubmit-losing the prompt).
        let restored_first = loaded
            .iter()
            .find(|r| r.anchor_message_id == 9003)
            .expect("first record survives the durable roundtrip");
        assert_eq!(restored_first.prompt_text, first.prompt_text);
        assert_eq!(restored_first.channel_id, 50);

        reset_present_for_tests();
    }
}
