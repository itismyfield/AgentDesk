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

static ACTIVE_WORKERS: LazyLock<Mutex<HashMap<(String, u64), u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PRECLAIMED_ACTIVE_WORKERS: LazyLock<Mutex<HashMap<(String, u64), u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static PRESENCE_RECONCILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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

fn clear_present(provider: &str, channel_id: u64) {
    PRESENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(provider.to_string(), channel_id));
}

struct ActiveWorkerGuard {
    provider: String,
    channel_id: u64,
}

impl ActiveWorkerGuard {
    fn new(provider: &str, channel_id: u64) -> Self {
        let mut workers = ACTIVE_WORKERS.lock().unwrap_or_else(|e| e.into_inner());
        *workers
            .entry((provider.to_string(), channel_id))
            .or_insert(0) += 1;
        Self {
            provider: provider.to_string(),
            channel_id,
        }
    }

    fn from_preclaimed(provider: &str, channel_id: u64) -> Self {
        Self {
            provider: provider.to_string(),
            channel_id,
        }
    }
}

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        let mut workers = ACTIVE_WORKERS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = workers.get_mut(&(self.provider.clone(), self.channel_id)) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                workers.remove(&(self.provider.clone(), self.channel_id));
            }
        }
    }
}

fn active_worker_present(provider: &str, channel_id: u64) -> bool {
    ACTIVE_WORKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(provider.to_string(), channel_id))
        .copied()
        .unwrap_or(0)
        > 0
}

fn preclaim_active_worker(provider: &str, channel_id: u64) {
    {
        let mut workers = ACTIVE_WORKERS.lock().unwrap_or_else(|e| e.into_inner());
        *workers
            .entry((provider.to_string(), channel_id))
            .or_insert(0) += 1;
    }
    let mut preclaimed = PRECLAIMED_ACTIVE_WORKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *preclaimed
        .entry((provider.to_string(), channel_id))
        .or_insert(0) += 1;
}

fn take_preclaimed_active_worker(provider: &str, channel_id: u64) -> Option<ActiveWorkerGuard> {
    let mut preclaimed = PRECLAIMED_ACTIVE_WORKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let count = preclaimed.get_mut(&(provider.to_string(), channel_id))?;
    *count = count.saturating_sub(1);
    if *count == 0 {
        preclaimed.remove(&(provider.to_string(), channel_id));
    }
    Some(ActiveWorkerGuard::from_preclaimed(provider, channel_id))
}

fn active_worker_guard_for_spawn(provider: &str, channel_id: u64) -> ActiveWorkerGuard {
    take_preclaimed_active_worker(provider, channel_id)
        .unwrap_or_else(|| ActiveWorkerGuard::new(provider, channel_id))
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
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    mark_present(provider, channel_id);
    preclaim_active_worker(provider, channel_id);
}

#[cfg(test)]
pub(super) fn reset_present_for_tests() {
    PRESENT.lock().unwrap_or_else(|e| e.into_inner()).clear();
    ACTIVE_WORKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    PRECLAIMED_ACTIVE_WORKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

// ---------------------------------------------------------------------------
// Durable store
// ---------------------------------------------------------------------------

fn root() -> Option<std::path::PathBuf> {
    super::runtime_store::tui_direct_pending_start_root()
}

fn write_record(record: &TuiDirectPendingStart) -> Result<(), String> {
    let Some(root) = root() else {
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

/// Persist (or update) a pending-start record and mark it present in the
/// in-memory index. Called BEFORE any wait, immediately after the anchor/lease
/// are created.
pub(super) fn persist(record: &TuiDirectPendingStart) -> Result<(), String> {
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    mark_present(&record.provider, record.channel_id);
    write_record(record)?;
    Ok(())
}

/// Delete a pending-start record AFTER the inflight save succeeds (or when the
/// worker gives up). Idempotent.
pub(super) fn delete(record: &TuiDirectPendingStart) {
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    mark_absent(&record.provider, record.channel_id);
    if let Some(root) = root() {
        let path = root.join(format!("{}.json", record.file_stem()));
        let _ = std::fs::remove_file(path);
    }
}

fn update_claim_attempt_count(record: &mut TuiDirectPendingStart, claim_attempts: u32) {
    record.attempt_count = claim_attempts;
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Err(error) = write_record(record) {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            anchor_message_id = record.anchor_message_id,
            claim_attempts,
            error = %error,
            "tui_direct_pending_start: failed to persist claim attempt count; retaining in-memory retry budget"
        );
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

fn records_for_channel(provider: &str, channel_id: u64) -> Vec<TuiDirectPendingStart> {
    load_all()
        .into_iter()
        .filter(|record| record.provider == provider && record.channel_id == channel_id)
        .collect()
}

fn channel_records_are_abandoned_locked(provider: &str, channel_id: u64) -> bool {
    if active_worker_present(provider, channel_id) {
        return false;
    }
    let records = records_for_channel(provider, channel_id);
    !records.is_empty()
        && records
            .iter()
            .all(|record| record.attempt_count >= PENDING_START_MAX_CLAIM_ATTEMPTS)
}

pub(super) fn pending_synthetic_start_abandoned(provider: &str, channel_id: u64) -> bool {
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    channel_records_are_abandoned_locked(provider, channel_id)
}

pub(super) fn clear_abandoned_synthetic_start_presence(provider: &str, channel_id: u64) -> bool {
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !channel_records_are_abandoned_locked(provider, channel_id) {
        return false;
    }
    clear_present(provider, channel_id);
    true
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

/// One worker poll's observation: the pure decision [`PriorTurnView`] plus the
/// live FOREIGN prior inflight's identity at the read instant (`None` when no
/// row exists or the row is our own anchor). The worker threads the LATEST
/// observed identity into the ABORT cleanup as the marker's last-view identity
/// — the PRIMARY pin since #3296 codex r3 ([`pin_abort_foreign_identity`]):
/// it survives the row vanishing before the cleanup's own read AND it cannot
/// be repointed by a successor row that took the slot in that gap, so the
/// commit-tombstone 대조 decides `✅` vs `⚠` for the RIGHT turn.
pub(super) struct PriorTurnObservation {
    pub view: PriorTurnView,
    pub foreign_inflight_identity: Option<(u64, String)>,
}

/// Build the per-poll [`PriorTurnObservation`]. Provided by
/// [`super::tui_prompt_relay`] (it owns inflight/mailbox/runtime-binding
/// access). Returns `None` when the view cannot be computed yet (e.g. mailbox
/// unavailable) — treated as "not finalized" so the worker keeps waiting.
pub(super) type ViewFn = Box<
    dyn for<'a> Fn(
            &'a Arc<SharedData>,
            &'a TuiDirectPendingStart,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<PriorTurnObservation>> + Send + 'a>,
        > + Send
        + Sync,
>;

/// #3282/#3296: Discord-side reconcile hook the worker runs on the terminal
/// backstop ABORT (`backstop_abort_foreign_inflight_live`). The input was
/// already provider-submitted by this point, so the anchor KEEPS its `⏳`; the
/// hook records a durable aborted-anchor marker
/// ([`super::tui_direct_abort_marker`]) so a later prior-owner terminal commit
/// flips it `⏳ → ✅`, or the TTL'd sweep flips it `⏳ → ⚠` when nothing ever
/// covered it. The third argument is the worker's LAST-VIEW foreign inflight
/// identity (codex r2 — see [`PriorTurnObservation`]). Provided by
/// [`super::tui_prompt_relay`].
pub(super) type AbortCleanupFn = Box<
    dyn for<'a> Fn(
            &'a Arc<SharedData>,
            &'a TuiDirectPendingStart,
            Option<(u64, String)>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>
        + Send
        + Sync,
>;

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
) {
    let active_guard = active_worker_guard_for_spawn(&record.provider, record.channel_id);
    super::task_supervisor::spawn_observed("tui_direct_pending_start_worker", async move {
        let _active_guard = active_guard;
        run_worker_inner(shared, record, view_fn, claim_fn, abort_cleanup_fn).await;
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

#[cfg(test)]
async fn run_worker(
    shared: Arc<SharedData>,
    record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
    abort_cleanup_fn: AbortCleanupFn,
) {
    let _active_guard = ActiveWorkerGuard::new(&record.provider, record.channel_id);
    run_worker_inner(shared, record, view_fn, claim_fn, abort_cleanup_fn).await;
}

async fn run_worker_inner(
    shared: Arc<SharedData>,
    mut record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
    abort_cleanup_fn: AbortCleanupFn,
) {
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
                    // #3296: WARN, not ERROR — this branch fires by definition
                    // only when a FOREIGN inflight is live on the SAME channel,
                    // i.e. the input was already submitted and usually merges
                    // into the prior owner's turn (a normal outcome, not a
                    // failure). The event key is load-bearing — never change it.
                    tracing::warn!(
                        provider = %record.provider,
                        channel_id = record.channel_id,
                        tmux_session_name = %record.tmux_session_name,
                        anchor_message_id = record.anchor_message_id,
                        backstop_cycles,
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
            return;
        }

        // Transient claim failure: do NOT delete (P1-2). Retry, bounded.
        claim_attempts = claim_attempts.saturating_add(1);
        update_claim_attempt_count(&mut record, claim_attempts);
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

/// #3303 — after a SUCCESSFUL deferred claim, record a
/// [`super::tui_direct_abort_marker`] marker of kind `DeferredClaim` pinned to
/// the worker's OWN synthetic turn identity (`user_msg_id == anchor`, the
/// freshly-claimed row's `started_at`).
///
/// Why: the claim hands the turn to the watcher, but the observed #3303
/// failure modes (the claim seeded the relay cursor at EOF after a prior
/// drain already consumed the response bytes, or the relay fails and a
/// watchdog clears the row) mean NO terminal-commit pass ever flips the
/// anchor's `⏳ → ✅` — an eternal hourglass with no reconcile owner. With the
/// marker, the watcher chokepoint's drain covers it on the own turn's commit
/// (`✅`, idempotent next to the normal completion), and the sweep bounds the
/// never-committed case with the TTL `⚠`.
///
/// Guards (in order):
/// * **SC3 scope gate** — record ONLY when the post-claim lease says the
///   `TmuxWatcher` owns the relay: a BridgeAdapter-owned turn finalizes via
///   the bridge WITHOUT the watcher chokepoint tombstone, so a marker would
///   contradict its normal completion with a TTL `⚠`.
/// * **Own-row guard** — the inflight row re-read at the record instant must
///   BE this claim's synthetic turn (anchor + tmux session match); its
///   `started_at` is the identity the marker pins (#3303 SC1: never the
///   foreign prior turn — that tombstone is already durable at claim time and
///   would false-`✅` instantly).
/// * **Fail-open** — every miss above (and a failed marker write) only warns:
///   the claim, the durable-record delete, and the turn proceed exactly as
///   before #3303.
fn record_deferred_claim_marker_if_watcher_owned(record: &TuiDirectPendingStart) {
    if record.anchor_message_id == 0 {
        return; // I5: a zero anchor id could never be reconciled (record() rejects it too)
    }
    let lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        &record.provider,
        &record.tmux_session_name,
        record.channel_id,
    );
    let relay_owner = lease.map(|lease| lease.relay_owner);
    if relay_owner != Some(crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::TmuxWatcher)
    {
        tracing::debug!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            relay_owner = ?relay_owner,
            "tui_direct_pending_start: deferred-claim marker skipped — turn is not watcher-owned, the watcher chokepoint will never tombstone it (#3303 SC3)"
        );
        return;
    }
    let Some(provider) = crate::services::provider::ProviderKind::from_str(&record.provider) else {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            anchor_message_id = record.anchor_message_id,
            "tui_direct_pending_start: unparseable provider; deferred-claim marker skipped (fail-open, #3303)"
        );
        return;
    };
    let Some(row) = super::inflight::load_inflight_state(&provider, record.channel_id) else {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            anchor_message_id = record.anchor_message_id,
            "tui_direct_pending_start: no inflight row at the record instant after a successful claim; deferred-claim marker skipped (fail-open, #3303)"
        );
        return;
    };
    let row_is_own_turn = row.user_msg_id == record.anchor_message_id
        && row.tmux_session_name.as_deref() == Some(record.tmux_session_name.as_str());
    if !row_is_own_turn {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            row_user_msg_id = row.user_msg_id,
            row_tmux_session_name = ?row.tmux_session_name,
            "tui_direct_pending_start: inflight row is not this claim's own synthetic turn; deferred-claim marker skipped (fail-open, #3303)"
        );
        return;
    }
    match super::tui_direct_abort_marker::record_for_deferred_claim(
        record.provider.clone(),
        record.channel_id,
        record.anchor_message_id,
        record.tmux_session_name.clone(),
        (record.anchor_message_id, row.started_at),
    ) {
        Ok(marker) => tracing::info!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            own_started_at = ?marker.foreign_started_at,
            tombstone_covered = marker.covered_at_ms.is_some(),
            "tui_direct_pending_start: deferred-claim marker recorded pinning the OWN synthetic turn — its commit drains ⏳ → ✅, a never-committed turn converges to the bounded sweep ⚠ (#3303)"
        ),
        Err(error) => tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            anchor_message_id = record.anchor_message_id,
            error = %error,
            "tui_direct_pending_start: failed to persist the deferred-claim marker; claim proceeds without it (fail-open — pre-#3303 behavior, the anchor ⏳ may linger) (#3303)"
        ),
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

    struct EnvReset(Option<std::ffi::OsString>);

    impl Drop for EnvReset {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    /// Wrap a pure view into the worker's per-poll observation (no foreign
    /// identity — the common already-finalized case).
    fn obs(view: PriorTurnView) -> PriorTurnObservation {
        PriorTurnObservation {
            view,
            foreign_inflight_identity: None,
        }
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

    /// #3296 codex r3 (RED ③ — pure): the ABORT cleanup pins the worker's
    /// LAST-VIEW identity, never the cleanup-instant row, when both exist.
    /// RED pre-r3: the relay hook preferred the live row — when the final
    /// poll's foreign row terminal-committed (tombstone + clear) and a
    /// SUCCESSOR row appeared before the cleanup's read, the marker pinned
    /// the successor: the genuine prior commit's tombstone never matched (no
    /// `✅` from the real answer, bounded `⚠` instead) and the successor's own
    /// commit could false-`✅` the possibly-unanswered anchor.
    #[test]
    fn abort_pin_prefers_last_view_identity_over_successor_row() {
        let last_view = Some((777_u64, "2026-06-10 12:00:00".to_string()));
        let successor = Some((888_u64, "2026-06-10 12:01:00".to_string()));
        assert_eq!(
            pin_abort_foreign_identity(last_view.clone(), || successor.clone()),
            last_view,
            "last-view is PRIMARY: a successor row must never repoint the pin (RED ③)"
        );
        // The primary path must not even READ the current row — the
        // cleanup-instant read is exactly what races the successor.
        let row_read = std::cell::Cell::new(false);
        assert_eq!(
            pin_abort_foreign_identity(last_view.clone(), || {
                row_read.set(true);
                successor.clone()
            }),
            last_view
        );
        assert!(
            !row_read.get(),
            "the row read must be skipped when a last-view identity exists"
        );
        // No-view fallback: the cleanup-instant row is the only evidence left
        // (conservative best-effort — see `pin_abort_foreign_identity`).
        assert_eq!(
            pin_abort_foreign_identity(None, || successor.clone()),
            successor
        );
        assert_eq!(pin_abort_foreign_identity(None, || None), None);
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
    fn abandoned_presence_clear_keeps_durable_record() {
        let _guard = worker_test_lock();
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        reset_present_for_tests();
        let mut rec = record("claude", 70, 700);
        rec.attempt_count = PENDING_START_MAX_CLAIM_ATTEMPTS;
        persist(&rec).unwrap();

        assert!(pending_synthetic_start_present("claude", 70));
        assert!(
            pending_synthetic_start_abandoned("claude", 70),
            "a capped durable attempt_count with no active worker is an abandoned claim"
        );
        assert!(clear_abandoned_synthetic_start_presence("claude", 70));
        assert!(
            !pending_synthetic_start_present("claude", 70),
            "the #3333 clear removes only the in-memory gate"
        );
        assert_eq!(
            load_all()
                .into_iter()
                .filter(|record| record.provider == "claude" && record.channel_id == 70)
                .count(),
            1,
            "the durable record must remain for restart retry"
        );

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

    /// #3282 test double for the ABORT-path anchor `⏳` cleanup, following the
    /// `ViewFn`/`ClaimFn` boxed-closure convention. Records each invocation —
    /// and the last-view foreign identity it received (codex r2) — so a test
    /// can pin WHEN the cleanup fires (terminal backstop ABORT only) and what
    /// identity the worker threaded, and when it must NOT fire (successful
    /// claim — the normal `⏳ → ✅` completion owns the anchor; retry
    /// exhaustion — the record is retained for restart).
    type RecordedForeignIdentity = Arc<Mutex<Option<Option<(u64, String)>>>>;
    fn recording_abort_cleanup() -> (
        AbortCleanupFn,
        Arc<std::sync::atomic::AtomicU32>,
        RecordedForeignIdentity,
    ) {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let identity: RecordedForeignIdentity = Arc::new(Mutex::new(None));
        let calls_for_fn = calls.clone();
        let identity_for_fn = identity.clone();
        let cleanup: AbortCleanupFn = Box::new(move |_shared, _record, foreign| {
            let calls = calls_for_fn.clone();
            let identity = identity_for_fn.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                *identity.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(foreign);
            })
        });
        (cleanup, calls, identity)
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
    // SAFETY (await_holding_lock): `worker_test_lock()` serializes tests that
    // mutate the process-wide PRESENT index / durable store root; the guard is
    // held across `tokio::time::advance` awaits that drive `run_worker`.
    // Releasing before the awaits would let concurrent tests stomp the statics.
    // Test-only.
    #[allow(clippy::await_holding_lock)]
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
                Some(obs(PriorTurnView {
                    // turn1 inflight present until drained.
                    inflight_present: undrained.load(Ordering::SeqCst),
                    inflight_is_own_anchor: false,
                    mailbox_blocking_turn_present: false,
                    mailbox_turn_is_own_anchor: false,
                    runtime_binding_present: true,
                }))
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
        let (a_cleanup, a_cleanup_calls, _) = recording_abort_cleanup();
        let a_handle = tokio::spawn(run_worker(
            shared.clone(),
            rec_a,
            a_view,
            a_claim,
            a_cleanup,
        ));

        // ---- Channel B: prior turn already finalized → relays immediately. ----
        let b_claimed = Arc::new(AtomicBool::new(false));
        let b_view: ViewFn = Box::new(move |_shared, _record| {
            Box::pin(async move {
                Some(obs(PriorTurnView {
                    inflight_present: false,
                    inflight_is_own_anchor: false,
                    mailbox_blocking_turn_present: false,
                    mailbox_turn_is_own_anchor: false,
                    runtime_binding_present: true,
                }))
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
        let (b_cleanup, b_cleanup_calls, _) = recording_abort_cleanup();
        let b_handle = tokio::spawn(run_worker(
            shared.clone(),
            rec_b,
            b_view,
            b_claim,
            b_cleanup,
        ));

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
        assert_eq!(
            a_cleanup_calls.load(Ordering::SeqCst) + b_cleanup_calls.load(Ordering::SeqCst),
            0,
            "#3282: a SUCCESSFUL claim must never run the abort reaction cleanup — \
             the normal watcher/recovery ⏳ → ✅ completion owns these anchors"
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
    // SAFETY (await_holding_lock): `worker_test_lock()` serializes tests that
    // mutate the process-wide PRESENT index / durable store root; the guard is
    // held across `tokio::time::advance` awaits that drive `run_worker`.
    // Releasing before the awaits would let concurrent tests stomp the statics.
    // Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(start_paused = true)]
    async fn backstop_foreign_inflight_live_aborts_without_claim() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let _guard = worker_test_lock();
        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();

        // A foreign prior inflight is live FOREVER (never drains, never ours).
        // Every poll observes its identity — the worker must thread the
        // LAST-VIEW identity into the abort cleanup (codex r2).
        let view: ViewFn = Box::new(move |_shared, _record| {
            Box::pin(async move {
                Some(PriorTurnObservation {
                    view: PriorTurnView {
                        inflight_present: true,
                        inflight_is_own_anchor: false,
                        mailbox_blocking_turn_present: true,
                        mailbox_turn_is_own_anchor: false,
                        runtime_binding_present: true,
                    },
                    foreign_inflight_identity: Some((777, "2026-06-10 12:00:00".to_string())),
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

        let (abort_cleanup, abort_cleanup_calls, abort_cleanup_identity) =
            recording_abort_cleanup();
        let handle = tokio::spawn(run_worker(shared.clone(), rec, view, claim, abort_cleanup));

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
        assert_eq!(
            abort_cleanup_calls.load(Ordering::SeqCst),
            1,
            "#3282/#3296: the terminal backstop ABORT must run the abort \
             reconcile hook EXACTLY ONCE (records the aborted-anchor marker — \
             no claim will ever drive the normal ⏳ → ✅ completion for this \
             anchor). RED if the ABORT branch skips abort_cleanup_fn (the \
             hourglass would linger with no reconcile owner)."
        );
        assert_eq!(
            abort_cleanup_identity
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone(),
            Some(Some((777, "2026-06-10 12:00:00".to_string()))),
            "codex r2: the worker must thread the LAST-VIEW foreign inflight \
             identity into the cleanup, so a row that vanishes before the \
             cleanup's own read still yields an identity-pinned marker — RED \
             if the hook receives None (the marker would be sweep-only and \
             tombstone 대조 could never ✅ it)"
        );
        reset_present_for_tests();
    }

    /// P2-2 (b): the claim returns `false` (transient — another turn briefly
    /// owns the mailbox). The worker MUST retain the durable record (never lose a
    /// Discord-submitted prompt) and retry; once the claim later succeeds it
    /// deletes. Proves the record is RETAINED across the false returns.
    // SAFETY (await_holding_lock): `worker_test_lock()` serializes tests that
    // mutate the process-wide PRESENT index / durable store root; the guard is
    // held across `tokio::time::advance` awaits that drive `run_worker`.
    // Releasing before the awaits would let concurrent tests stomp the statics.
    // Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(start_paused = true)]
    async fn claim_false_retains_record_and_retries() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let _guard = worker_test_lock();
        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();

        // Prior turn is finalized immediately — the wait window is not the point.
        let view: ViewFn =
            Box::new(move |_shared, _record| Box::pin(async move { Some(obs(base_view())) }));

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
        let (abort_cleanup, abort_cleanup_calls, _) = recording_abort_cleanup();
        let handle = tokio::spawn(run_worker(shared.clone(), rec, view, claim, abort_cleanup));

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
        assert_eq!(
            abort_cleanup_calls.load(Ordering::SeqCst),
            0,
            "#3282: transient claim retries that eventually SUCCEED must not run \
             the abort reaction cleanup — the anchor's ⏳ → ✅ completes normally"
        );
        reset_present_for_tests();
    }

    /// P2-2 (b'): the claim returns `false` ACROSS THE WHOLE retry budget. The
    /// worker exhausts attempts but STILL must NOT delete the record (it is left
    /// for a restart re-attempt — never silently lose the prompt).
    // SAFETY (await_holding_lock): `worker_test_lock()` serializes tests that
    // mutate the process-wide PRESENT index / durable store root; the guard is
    // held across `tokio::time::advance` awaits that drive `run_worker`.
    // Releasing before the awaits would let concurrent tests stomp the statics.
    // Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(start_paused = true)]
    async fn claim_false_exhausted_still_retains_record() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let _guard = worker_test_lock();
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();
        let view: ViewFn =
            Box::new(move |_shared, _record| Box::pin(async move { Some(obs(base_view())) }));

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
        let (abort_cleanup, abort_cleanup_calls, _) = recording_abort_cleanup();
        let handle = tokio::spawn(run_worker(shared.clone(), rec, view, claim, abort_cleanup));

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
        assert!(
            pending_synthetic_start_abandoned("claude", 12),
            "after the worker exits with retry exhaustion, the durable capped \
             attempt_count plus no active worker identifies the record as abandoned"
        );
        let retained = records_for_channel("claude", 12);
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].attempt_count, PENDING_START_MAX_CLAIM_ATTEMPTS,
            "retry exhaustion must be persisted so queue_io can distinguish \
             abandoned claims from live workers"
        );
        assert_eq!(
            abort_cleanup_calls.load(Ordering::SeqCst),
            0,
            "#3282: claim-retry exhaustion RETAINS the record for a restart \
             re-attempt — the anchor's ⏳ must stay (the restored worker may still \
             claim and complete it normally), so the abort cleanup must NOT fire"
        );
        reset_present_for_tests();
    }

    // Sync test + explicit block_on: the std-mutex test-env guards live only in
    // this sync scope and never span an await, so no await_holding_lock allow is
    // needed (#3034 ratchet stays frozen at its baseline).
    #[test]
    fn live_worker_with_capped_attempt_count_is_not_abandoned() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = worker_test_lock();
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        reset_present_for_tests();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("test runtime");
        rt.block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let finalized = Arc::new(AtomicBool::new(false));
            let finalized_for_view = finalized.clone();
            let view: ViewFn = Box::new(move |_shared, _record| {
                let finalized = finalized_for_view.clone();
                Box::pin(async move {
                    let view = if finalized.load(Ordering::SeqCst) {
                        base_view()
                    } else {
                        PriorTurnView {
                            inflight_present: true,
                            inflight_is_own_anchor: false,
                            mailbox_blocking_turn_present: true,
                            mailbox_turn_is_own_anchor: false,
                            runtime_binding_present: true,
                        }
                    };
                    Some(obs(view))
                })
            });
            let claim: ClaimFn = Box::new(move |_shared, _record| Box::pin(async move { true }));

            let mut rec = record("claude", 13, 133);
            rec.attempt_count = PENDING_START_MAX_CLAIM_ATTEMPTS;
            persist(&rec).unwrap();
            let (abort_cleanup, _, _) = recording_abort_cleanup();
            let handle = tokio::spawn(run_worker(shared.clone(), rec, view, claim, abort_cleanup));

            tokio::task::yield_now().await;
            assert!(
                !pending_synthetic_start_abandoned("claude", 13),
                "a live worker must protect a capped durable record from being \
                 treated as abandoned during restart re-claim"
            );
            assert!(
                !clear_abandoned_synthetic_start_presence("claude", 13),
                "presence clear must refuse while a worker is active"
            );

            finalized.store(true, Ordering::SeqCst);
            tokio::time::advance(PENDING_START_POLL * 2).await;
            tokio::task::yield_now().await;
            handle.await.unwrap();
            assert!(
                !pending_synthetic_start_present("claude", 13),
                "the live worker's successful claim clears the presence through the normal delete path"
            );
        });

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

    // ====================================================================
    // #3303 — DeferredClaim marker hook on the SUCCESSFUL claim path.
    // Each test drives the REAL `run_worker` on a current-thread runtime via
    // `block_on` on THIS thread (so the marker store's thread-local test root
    // resolves inside the worker, and no lock guard is held across an await
    // point — the await_holding_lock ratchet stays frozen), against a REAL
    // on-disk inflight row under a temp AGENTDESK_ROOT_DIR and a REAL
    // in-memory relay lease.
    // ====================================================================

    /// RAII rig: AGENTDESK_ROOT_DIR → tempdir (real inflight store) + the
    /// marker store's thread-local root override. Construct ONLY while
    /// holding `worker_test_lock()` AND the crate env lock (in that order —
    /// the `durable_restore_roundtrip_loads_fifo_order` convention).
    struct DeferredClaimMarkerRig {
        _temp: tempfile::TempDir,
        prev_env: Option<std::ffi::OsString>,
    }

    impl DeferredClaimMarkerRig {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let prev_env = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
            super::super::tui_direct_abort_marker::set_test_root_override(Some(
                temp.path().to_path_buf(),
            ));
            Self {
                _temp: temp,
                prev_env,
            }
        }
    }

    impl Drop for DeferredClaimMarkerRig {
        fn drop(&mut self) {
            super::super::tui_direct_abort_marker::set_test_root_override(None);
            match self.prev_env.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn finalized_view() -> ViewFn {
        Box::new(|_shared, _record| Box::pin(async move { Some(obs(base_view())) }))
    }

    fn claim_succeeds() -> ClaimFn {
        Box::new(|_shared, _record| Box::pin(async move { true }))
    }

    fn record_lease(
        provider: &str,
        tmux: &str,
        channel_id: u64,
        owner: crate::services::tui_prompt_dedupe::ExternalInputRelayOwner,
    ) {
        let mut lease = crate::services::tui_prompt_dedupe::ExternalInputRelayLease::unassigned(
            Some(channel_id),
        );
        lease.relay_owner = owner;
        let _ = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            provider, tmux, lease,
        );
    }

    /// Save the freshly-claimed OWN synthetic inflight row (the state the
    /// claim leaves behind: `user_msg_id == anchor`). Returns its
    /// `started_at` — the identity component the marker must pin.
    fn save_own_inflight_row(channel_id: u64, anchor: u64, tmux: &str) -> String {
        let state = super::super::inflight::InflightTurnState::new(
            crate::services::provider::ProviderKind::Claude,
            channel_id,
            None,
            0,
            anchor,
            0,
            "/loop tick".to_string(),
            None,
            Some(tmux.to_string()),
            None,
            None,
            0,
        );
        super::super::inflight::save_inflight_state(&state).unwrap();
        state.started_at
    }

    fn current_thread_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// #3303 R1 (the bug): a SUCCESSFUL watcher-owned deferred claim must
    /// record a `DeferredClaim` marker pinned to the OWN synthetic turn
    /// identity (anchor id + the claimed row's `started_at`) before the
    /// durable record is deleted. RED pre-#3303: the success path recorded
    /// NOTHING — a claimed turn whose commit pass never ran (EOF-seeded
    /// cursor after a prior drain consumed the bytes, or relay failure +
    /// watchdog clear) kept its `⏳` forever with no reconcile owner. The
    /// success path must STILL never run the abort cleanup (#3282 contract).
    #[test]
    fn successful_watcher_owned_claim_records_own_identity_marker() {
        use std::sync::atomic::Ordering;

        let _guard = worker_test_lock();
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _rig = DeferredClaimMarkerRig::new();
        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();

        let rec = record("claude", 21, 2100);
        record_lease(
            "claude",
            &rec.tmux_session_name,
            21,
            crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::TmuxWatcher,
        );
        let own_started_at = save_own_inflight_row(21, 2100, &rec.tmux_session_name);
        persist(&rec).unwrap();

        let (cleanup, cleanup_calls, _) = recording_abort_cleanup();
        current_thread_rt().block_on(run_worker(
            shared,
            rec,
            finalized_view(),
            claim_succeeds(),
            cleanup,
        ));

        let markers = super::super::tui_direct_abort_marker::load_for_channel("claude", 21);
        assert_eq!(
            markers.len(),
            1,
            "RED pre-#3303: the success path recorded no marker — the anchor's \
             ⏳ had no reconcile owner when the commit pass never ran"
        );
        let marker = &markers[0];
        assert_eq!(
            marker.origin,
            super::super::tui_direct_abort_marker::MarkerOrigin::DeferredClaim
        );
        assert_eq!(marker.anchor_message_id, 2100);
        assert_eq!(
            marker.foreign_user_msg_id,
            Some(2100),
            "the pin is the OWN synthetic turn — never the foreign prior (SC1)"
        );
        assert_eq!(
            marker.foreign_started_at.as_deref(),
            Some(own_started_at.as_str()),
            "started_at must be re-read from the freshly-claimed row"
        );
        assert_eq!(marker.tmux_session_name, "tmux-21");
        assert_eq!(marker.covered_at_ms, None);
        assert_eq!(
            cleanup_calls.load(Ordering::SeqCst),
            0,
            "#3282: a successful claim must never run the abort cleanup"
        );
        assert!(
            !pending_synthetic_start_present("claude", 21),
            "the durable record is still deleted after the marker hook (fail-open ordering)"
        );
        reset_present_for_tests();
    }

    /// #3303 R7 (SC3 scope gate): a successful claim whose post-claim lease
    /// resolved to the BridgeAdapter records NO marker — bridge-owned turns
    /// finalize via `turn_bridge` WITHOUT the watcher chokepoint tombstone,
    /// so a marker would contradict a normally-completed turn with a TTL `⚠`.
    /// RED if the hook records unconditionally.
    #[test]
    fn bridge_owned_claim_records_no_marker() {
        let _guard = worker_test_lock();
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _rig = DeferredClaimMarkerRig::new();
        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();

        let rec = record("claude", 22, 2200);
        record_lease(
            "claude",
            &rec.tmux_session_name,
            22,
            crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::BridgeAdapter,
        );
        // Even with a perfectly matching own row, the owner gate must win.
        let _ = save_own_inflight_row(22, 2200, &rec.tmux_session_name);
        persist(&rec).unwrap();

        let (cleanup, _calls, _) = recording_abort_cleanup();
        current_thread_rt().block_on(run_worker(
            shared,
            rec,
            finalized_view(),
            claim_succeeds(),
            cleanup,
        ));

        assert!(
            super::super::tui_direct_abort_marker::load_for_channel("claude", 22).is_empty(),
            "bridge-owned turns must record no DeferredClaim marker (SC3)"
        );
        assert!(!pending_synthetic_start_present("claude", 22));
        reset_present_for_tests();
    }

    /// #3303 R8 (restart idempotence, SC2): an abnormal restart can leave a
    /// stale ABORT marker on this anchor's stem; the successful re-claim must
    /// OVERWRITE it with the refreshed own-identity DeferredClaim marker (the
    /// turn is adopted and live — one stem can never hold two markers).
    #[test]
    fn reclaim_overwrites_stale_abort_marker_with_own_identity() {
        let _guard = worker_test_lock();
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _rig = DeferredClaimMarkerRig::new();
        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();

        let rec = record("claude", 23, 2300);
        super::super::tui_direct_abort_marker::record_for_abort(
            "claude".into(),
            23,
            2300,
            rec.tmux_session_name.clone(),
            Some((999, "2026-06-10 12:00:00".into())),
        )
        .unwrap();
        record_lease(
            "claude",
            &rec.tmux_session_name,
            23,
            crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::TmuxWatcher,
        );
        let own_started_at = save_own_inflight_row(23, 2300, &rec.tmux_session_name);
        persist(&rec).unwrap();

        let (cleanup, _calls, _) = recording_abort_cleanup();
        current_thread_rt().block_on(run_worker(
            shared,
            rec,
            finalized_view(),
            claim_succeeds(),
            cleanup,
        ));

        let markers = super::super::tui_direct_abort_marker::load_for_channel("claude", 23);
        assert_eq!(markers.len(), 1, "one stem, one marker (SC2)");
        assert_eq!(
            markers[0].origin,
            super::super::tui_direct_abort_marker::MarkerOrigin::DeferredClaim,
            "the re-claim must replace the stale abort marker"
        );
        assert_eq!(markers[0].foreign_user_msg_id, Some(2300));
        assert_eq!(
            markers[0].foreign_started_at.as_deref(),
            Some(own_started_at.as_str())
        );
        reset_present_for_tests();
    }
}
