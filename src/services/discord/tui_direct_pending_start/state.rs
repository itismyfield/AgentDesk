use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::super::{SharedData, inflight, runtime_store, tui_direct_abort_marker, turn_finalizer};

/// Conservative poll interval for the wait predicate.
pub(in crate::services::discord) const PENDING_START_POLL: Duration = Duration::from_millis(100);

/// Backstop matching `turn_finalizer::GATE_BACKSTOP` (8s). After this single
/// wait window expires WITHOUT the prior turn finalizing, the worker does NOT
/// blindly claim (that would overwrite a still-LIVE prior inflight and resurrect
/// the original #3154 wrong-turn-finalize / `response_sent_offset` regression).
/// Instead it re-checks at the claim instant whether the prior inflight is truly
/// gone; if a foreign prior inflight is still live it keeps deferring under
/// bounded escalation (see [`PENDING_START_MAX_BACKSTOP_CYCLES`]).
pub(in crate::services::discord) const PENDING_START_BACKSTOP: Duration = Duration::from_secs(8);

/// Bounded escalation cap. Each cycle is one `PENDING_START_BACKSTOP` wait
/// window during which the prior turn never finalized AND, at the claim instant,
/// a FOREIGN prior inflight was still live (so claiming would overwrite it).
/// After this many such cycles the worker ABORTS the synthetic start safely
/// (surfaces an observability event + deletes the durable record) rather than
/// either overwriting a live prior turn or leaking the record forever. The
/// provider prompt itself is never resubmitted; only the synthetic OWNERSHIP
/// claim is abandoned — the watcher/bridge still relays the provider's output.
pub(in crate::services::discord) const PENDING_START_MAX_BACKSTOP_CYCLES: u32 = 4;

/// On a transient claim failure (`claimed == false`: another turn briefly owns
/// the mailbox, or an inflight save failed) the worker MUST NOT delete the
/// durable record (that would lose a Discord-submitted prompt — the original
/// turn-loss bug). It re-defers and retries, bounded by this cap, so a wedged
/// claim path cannot spin forever.
pub(in crate::services::discord) const PENDING_START_MAX_CLAIM_ATTEMPTS: u32 = 5;

/// Backoff between claim retries after a transient `claimed == false`.
pub(in crate::services::discord) const PENDING_START_CLAIM_RETRY_BACKOFF: Duration =
    Duration::from_millis(250);

/// #4030 mirrors the #4020 positive stale-owner age gate for reclaiming a row
/// that still looks FOREIGN-live but has stopped advancing.
pub(in crate::services::discord) const STALE_FOREIGN_INFLIGHT_MIN_AGE_SECS: i64 = 120;

/// A committed row must remain byte-for-byte frozen for this long after crossing
/// a process generation before pane readiness can replace missing terminal JSONL.
pub(in crate::services::discord) const RESTART_ORPHAN_COMMITTED_GRACE_SECS: i64 = 10 * 60;

/// Lifecycle state of a durable pending-start record. Kept tiny and
/// string-serialized so a forward/backward dcserver swap reads it tolerantly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(in crate::services::discord) enum PendingStartState {
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
pub(in crate::services::discord) struct TuiDirectPendingStart {
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

pub(super) fn mark_present(provider: &str, channel_id: u64) {
    let mut map = PRESENT.lock().unwrap_or_else(|e| e.into_inner());
    *map.entry((provider.to_string(), channel_id)).or_insert(0) += 1;
}

pub(super) fn mark_absent(provider: &str, channel_id: u64) {
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

pub(super) struct ActiveWorkerGuard {
    provider: String,
    channel_id: u64,
}

impl ActiveWorkerGuard {
    pub(super) fn new(provider: &str, channel_id: u64) -> Self {
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

pub(super) fn active_worker_guard_for_spawn(provider: &str, channel_id: u64) -> ActiveWorkerGuard {
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
pub(in crate::services::discord) fn pending_synthetic_start_present(
    provider: &str,
    channel_id: u64,
) -> bool {
    let map = PRESENT.lock().unwrap_or_else(|e| e.into_inner());
    map.get(&(provider.to_string(), channel_id))
        .copied()
        .unwrap_or(0)
        > 0
}

pub(in crate::services::discord) fn pending_synthetic_start_blocks_idle_kickoff(
    provider: &str,
    channel_id: u64,
) -> bool {
    if !pending_synthetic_start_present(provider, channel_id) {
        return false;
    }

    if clear_abandoned_synthetic_start_presence(provider, channel_id) {
        tracing::warn!(
            provider,
            channel_id,
            issue = "#3691",
            "idle queue gate cleared abandoned TUI-direct pending-start presence; durable record retained for restart retry"
        );
        return false;
    }

    true
}

/// Re-mark a record present during restart restore. [`load_all`] reads the
/// durable store but does not touch the in-memory index; this restores the gate
/// state before the respawned worker's first poll. The worker's terminal
/// [`delete`] balances it.
pub(in crate::services::discord) fn mark_present_on_restore(provider: &str, channel_id: u64) {
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    mark_present(provider, channel_id);
    preclaim_active_worker(provider, channel_id);
}

#[cfg(test)]
pub(in crate::services::discord) fn reset_present_for_tests() {
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

pub(super) fn root() -> Option<std::path::PathBuf> {
    runtime_store::tui_direct_pending_start_root()
}

fn write_record(record: &TuiDirectPendingStart) -> Result<(), String> {
    let Some(root) = root() else {
        return Ok(());
    };
    let path = root.join(format!("{}.json", record.file_stem()));
    let data = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    runtime_store::critical_atomic_write(
        &path,
        &data,
        runtime_store::AtomicWriteContext::new("tui_direct_pending_start")
            .provider(&record.provider)
            .channel_id(record.channel_id),
    )
}

/// Persist (or update) a pending-start record and mark it present in the
/// in-memory index. Called BEFORE any wait, immediately after the anchor/lease
/// are created.
pub(in crate::services::discord) fn persist(record: &TuiDirectPendingStart) -> Result<(), String> {
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    mark_present(&record.provider, record.channel_id);
    write_record(record)?;
    Ok(())
}

/// Delete a pending-start record AFTER the inflight save succeeds (or when the
/// worker gives up). Idempotent.
pub(in crate::services::discord) fn delete(record: &TuiDirectPendingStart) {
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    mark_absent(&record.provider, record.channel_id);
    if let Some(root) = root() {
        let path = root.join(format!("{}.json", record.file_stem()));
        let _ = std::fs::remove_file(path);
    }
}

pub(super) fn update_claim_attempt_count(record: &mut TuiDirectPendingStart, claim_attempts: u32) {
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
pub(in crate::services::discord) fn load_all() -> Vec<TuiDirectPendingStart> {
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

pub(super) fn records_for_channel(provider: &str, channel_id: u64) -> Vec<TuiDirectPendingStart> {
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

#[cfg(test)]
pub(in crate::services::discord) fn pending_synthetic_start_abandoned(
    provider: &str,
    channel_id: u64,
) -> bool {
    let _guard = PRESENCE_RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    channel_records_are_abandoned_locked(provider, channel_id)
}

pub(in crate::services::discord) fn clear_abandoned_synthetic_start_presence(
    provider: &str,
    channel_id: u64,
) -> bool {
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
pub(in crate::services::discord) struct PriorTurnView {
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
pub(in crate::services::discord) fn prior_turn_finalized(view: PriorTurnView) -> bool {
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
pub(in crate::services::discord) fn backstop_claim_is_safe(view: PriorTurnView) -> bool {
    // The ONLY thing the backstop relaxes is the mailbox-blocking and
    // runtime-binding waits (a wedged-but-present prior turn / a transiently
    // missing binding). It must NEVER relax the live-foreign-inflight guard:
    // overwriting a live prior inflight is the exact regression this fixes.
    !view.inflight_present || view.inflight_is_own_anchor
}

/// Decide whether [`crate::services::discord::tui_prompt_relay::relay_observed_prompt`] must DEFER the synthetic turn-start
/// off the observer loop (persist a record + spawn the worker) instead of
/// claiming inline.
///
/// Defer when the prior turn is NOT finalized — i.e. claiming inline now would
/// reproduce the offset collision. When the prior turn is already finalized the
/// inline claim is safe and the deferral machinery is skipped entirely (keeps
/// the common no-interleave path on its existing fast path).
pub(in crate::services::discord) fn should_defer_synthetic_turn_start(
    prior: PriorTurnView,
) -> bool {
    !prior_turn_finalized(prior)
}

// ---------------------------------------------------------------------------
// Detached worker
// ---------------------------------------------------------------------------

/// The claim action the worker runs once the prior turn is finalized. Provided
/// by [`crate::services::discord::tui_prompt_relay`] (where `claim_tui_direct_synthetic_turn` is
/// private). Returns `true` when an inflight was saved (claimed) AND the claim's
/// `relay_owner` was adopted into the in-memory lease (so the observer-side
/// BridgeAdapter tail stops once the watcher owns the turn — P1-3); `false` on a
/// transient failure (another turn briefly owns the mailbox, or an inflight save
/// failed), in which case the worker re-defers and retries WITHOUT deleting the
/// durable record (P1-2 — never lose a Discord-submitted prompt).
pub(in crate::services::discord) type ClaimFn = Box<
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
/// — the PRIMARY pin since #3296 codex r3 ([`super::pin_abort_foreign_identity`]):
/// it survives the row vanishing before the cleanup's own read AND it cannot
/// be repointed by a successor row that took the slot in that gap, so the
/// commit-tombstone 대조 decides `✅` vs `⚠` for the RIGHT turn.
pub(in crate::services::discord) struct PriorTurnObservation {
    pub view: PriorTurnView,
    pub foreign_inflight_identity: Option<(u64, String)>,
}

/// Build the per-poll [`PriorTurnObservation`]. Provided by
/// [`crate::services::discord::tui_prompt_relay`] (it owns inflight/mailbox/runtime-binding
/// access). Returns `None` when the view cannot be computed yet (e.g. mailbox
/// unavailable) — treated as "not finalized" so the worker keeps waiting.
pub(in crate::services::discord) type ViewFn = Box<
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
/// ([`tui_direct_abort_marker`]) so a later prior-owner terminal commit
/// flips it `⏳ → ✅`, or the TTL'd sweep flips it `⏳ → ⚠` when nothing ever
/// covered it. The third argument is the worker's LAST-VIEW foreign inflight
/// identity (codex r2 — see [`PriorTurnObservation`]). Provided by
/// [`crate::services::discord::tui_prompt_relay`].
pub(in crate::services::discord) type AbortCleanupFn = Box<
    dyn for<'a> Fn(
            &'a Arc<SharedData>,
            &'a TuiDirectPendingStart,
            Option<(u64, String)>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>
        + Send
        + Sync,
>;

/// #3982: the worker's per-escalation-cycle orphan-reclaim attempt, consulted in
/// the `BackstopForeignInflightLive` branch BEFORE the terminal abort. The
/// backstop can only observe an inflight row; it cannot tell a genuinely live
/// FOREIGN turn from a producer-dead `SessionBoundRelay` orphan born after its
/// per-turn StreamRelay producer already exited (a stale `get_producer` `Some`
/// stamps the owner `SessionBoundRelay`; the row never commits and is perpetually
/// misread as live-foreign → every later TUI-direct turn aborts). This closure
/// loads the row and, IFF it is orphan-shaped (300s-quiescent, zero-progress,
/// never-delivered), downgrades its relay owner to `None` via the
/// identity-guarded `downgrade_orphaned_session_bound_relay_owner_locked`.
///
/// Returns `true` ONLY when the owner was downgraded — the worker then
/// re-evaluates immediately (`continue`): the next view's ownerless-stale filter
/// drops the now-`None` row, so the deferred claim proceeds instead of aborting.
/// Returns `false` for a genuinely live turn (not orphan-shaped), an
/// identity/lifecycle mismatch, or an I/O failure → the worker keeps its EXISTING
/// bounded escalation/abort (no new infinite spin). Provided by
/// [`crate::services::discord::tui_prompt_relay`] (it owns inflight access); it NEVER gates on the
/// proven-stale `get_producer` oracle — the authoritative guard is the in-lock
/// orphan-shape re-check + identity inside the downgrade primitive (#3982).
pub(in crate::services::discord) type ReclaimOrphanFn = Box<
    dyn for<'a> Fn(
            &'a Arc<SharedData>,
            &'a TuiDirectPendingStart,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = ReclaimStaleForeignOutcome> + Send + 'a>,
        > + Send
        + Sync,
>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ReclaimStaleForeignOutcome {
    None,
    StaleForeignDemoted,
    SessionBoundOrphanReclaimed,
}

impl ReclaimStaleForeignOutcome {
    pub(super) fn is_reclaimed(self) -> bool {
        !matches!(self, Self::None)
    }

    pub(super) fn event_key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::StaleForeignDemoted => "tui_direct_pending_start.backstop_stale_foreign_demoted",
            Self::SessionBoundOrphanReclaimed => {
                "tui_direct_pending_start.backstop_orphan_reclaimed"
            }
        }
    }
}

fn stale_foreign_inflight_age_permits_reclaim(
    state: &inflight::InflightTurnState,
    now_unix_secs: i64,
) -> bool {
    let Some(updated_at_unix) = inflight::parse_updated_at_unix(&state.updated_at) else {
        return false;
    };
    let age_secs = now_unix_secs.saturating_sub(updated_at_unix);
    age_secs >= STALE_FOREIGN_INFLIGHT_MIN_AGE_SECS
}

pub(super) fn output_capture_offset(state: &inflight::InflightTurnState) -> Option<u64> {
    state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len())
}

pub(super) fn stale_foreign_inflight_is_reclaimable_at(
    state: &inflight::InflightTurnState,
    record: &TuiDirectPendingStart,
    now_unix_secs: i64,
) -> bool {
    let is_own_anchor = state.turn_source == inflight::TurnSource::ExternalInput
        && state.tmux_session_name.as_deref() == Some(record.tmux_session_name.as_str())
        && state.user_msg_id == record.anchor_message_id;
    !is_own_anchor
        && state.tmux_session_name.as_deref() == Some(record.tmux_session_name.as_str())
        && state.effective_relay_owner_kind() != inflight::RelayOwnerKind::SessionBoundRelay
        && !state.terminal_delivery_committed
        && stale_foreign_inflight_age_permits_reclaim(state, now_unix_secs)
}

pub(super) fn stale_foreign_cancel_finalize_context() -> turn_finalizer::FinalizeContext {
    turn_finalizer::FinalizeContext {
        clear_inflight: true,
        allow_completion_cleanup: false,
        drain_voice: false,
        kickoff_queue: true,
        expected_idempotent_guard_miss: false,
    }
}

pub(super) fn committed_foreign_complete_finalize_context() -> turn_finalizer::FinalizeContext {
    turn_finalizer::FinalizeContext {
        clear_inflight: true,
        allow_completion_cleanup: false,
        drain_voice: false,
        kickoff_queue: true,
        expected_idempotent_guard_miss: false,
    }
}

pub(super) fn committed_foreign_inflight_is_finalize_clearable(
    state: &inflight::InflightTurnState,
    record: &TuiDirectPendingStart,
) -> bool {
    let is_own_anchor = state.turn_source == inflight::TurnSource::ExternalInput
        && state.tmux_session_name.as_deref() == Some(record.tmux_session_name.as_str())
        && state.user_msg_id == record.anchor_message_id;
    !is_own_anchor
        && state.tmux_session_name.as_deref() == Some(record.tmux_session_name.as_str())
        && state.terminal_delivery_committed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RestartOrphanEvidence {
    pub(super) generation_crossed: bool,
    pub(super) committed_frozen_past_grace: bool,
    pub(super) pane_ready_for_input: bool,
}

impl RestartOrphanEvidence {
    pub(super) fn permits_finalize_clear(self) -> bool {
        self.generation_crossed && self.committed_frozen_past_grace && self.pane_ready_for_input
    }
}

pub(super) fn inflight_generation_precedes_current(
    state: &inflight::InflightTurnState,
    current_generation: u64,
) -> bool {
    state.born_generation != 0 && state.born_generation != current_generation
        || state
            .restart_generation
            .is_some_and(|generation| generation != current_generation)
}

pub(super) fn restart_orphan_evidence_at(
    state: &inflight::InflightTurnState,
    current_generation: u64,
    now_unix_secs: i64,
    pane_ready_for_input: bool,
) -> RestartOrphanEvidence {
    let committed_frozen_past_grace = inflight::parse_updated_at_unix(&state.updated_at)
        .is_some_and(|updated_at| {
            now_unix_secs.saturating_sub(updated_at) >= RESTART_ORPHAN_COMMITTED_GRACE_SECS
        });
    RestartOrphanEvidence {
        generation_crossed: inflight_generation_precedes_current(state, current_generation),
        committed_frozen_past_grace,
        pane_ready_for_input,
    }
}

pub(super) fn claude_tui_output_path_missing(state: &inflight::InflightTurnState) -> bool {
    crate::services::tui_turn_state::claude_tui_output_path_missing(
        state.runtime_kind,
        state.output_path.as_deref(),
    )
}

pub(super) fn restart_orphan_independent_pane_ready(
    state: &inflight::InflightTurnState,
    readiness: crate::services::tmux_turn_liveness::IndependentTmuxReadiness,
) -> bool {
    !claude_tui_output_path_missing(state)
        && matches!(
            readiness,
            crate::services::tmux_turn_liveness::IndependentTmuxReadiness::ReadyForInput
        )
}

pub(super) fn restart_orphan_pane_ready_for_input(
    provider: &crate::services::provider::ProviderKind,
    state: &inflight::InflightTurnState,
    tmux_session_name: &str,
) -> bool {
    let output_path = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(std::path::Path::new);
    let readiness = crate::services::tmux_turn_liveness::independent_tmux_readiness(
        tmux_session_name,
        provider,
        state.runtime_kind,
        output_path,
        Some(state.last_offset),
    );
    restart_orphan_independent_pane_ready(state, readiness)
}

/// #3303 — after a SUCCESSFUL deferred claim, record a
/// [`tui_direct_abort_marker`] marker of kind `DeferredClaim` pinned to
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
/// #3350: the marker-record chokepoint SHARED by the deferred worker (#3303,
/// via the thin [`super::record_deferred_claim_marker_if_watcher_owned`] wrapper) and
/// the INLINE synthetic claim (`tui_prompt_relay`). Both claim paths must
/// leave the same durable `DeferredClaim` marker, or an inline-claimed turn
/// whose output is never committed (e.g. stale input right after `/clear`)
/// keeps an eternal anchor `⏳`. Body unchanged from the #3303 helper — every
/// guard above applies verbatim.
pub(in crate::services::discord) fn record_claim_marker_if_watcher_owned(
    provider: &str,
    channel_id: u64,
    anchor_message_id: u64,
    tmux_session_name: &str,
) {
    if anchor_message_id == 0 {
        return; // I5: a zero anchor id could never be reconciled (record() rejects it too)
    }
    let lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        provider,
        tmux_session_name,
        channel_id,
    );
    let relay_owner = lease.map(|lease| lease.relay_owner);
    if relay_owner != Some(crate::services::tui_prompt_dedupe::ExternalInputRelayOwner::TmuxWatcher)
    {
        tracing::debug!(
            provider = %provider,
            channel_id,
            tmux_session_name = %tmux_session_name,
            anchor_message_id,
            relay_owner = ?relay_owner,
            "tui_direct_pending_start: deferred-claim marker skipped — turn is not watcher-owned, the watcher chokepoint will never tombstone it (#3303 SC3)"
        );
        return;
    }
    let Some(provider_kind) = crate::services::provider::ProviderKind::from_str(provider) else {
        tracing::warn!(
            provider = %provider,
            channel_id,
            anchor_message_id,
            "tui_direct_pending_start: unparseable provider; deferred-claim marker skipped (fail-open, #3303)"
        );
        return;
    };
    let Some(row) = inflight::load_inflight_state(&provider_kind, channel_id) else {
        tracing::warn!(
            provider = %provider,
            channel_id,
            anchor_message_id,
            "tui_direct_pending_start: no inflight row at the record instant after a successful claim; deferred-claim marker skipped (fail-open, #3303)"
        );
        return;
    };
    let row_is_own_turn = row.user_msg_id == anchor_message_id
        && row.tmux_session_name.as_deref() == Some(tmux_session_name);
    if !row_is_own_turn {
        tracing::warn!(
            provider = %provider,
            channel_id,
            tmux_session_name = %tmux_session_name,
            anchor_message_id,
            row_user_msg_id = row.user_msg_id,
            row_tmux_session_name = ?row.tmux_session_name,
            "tui_direct_pending_start: inflight row is not this claim's own synthetic turn; deferred-claim marker skipped (fail-open, #3303)"
        );
        return;
    }
    match tui_direct_abort_marker::record_for_deferred_claim(
        provider.to_string(),
        channel_id,
        anchor_message_id,
        tmux_session_name.to_string(),
        (anchor_message_id, row.started_at),
        row.turn_start_offset,
    ) {
        Ok(marker) => tracing::info!(
            provider = %provider,
            channel_id,
            tmux_session_name = %tmux_session_name,
            anchor_message_id,
            own_started_at = ?marker.foreign_started_at,
            tombstone_covered = marker.covered_at_ms.is_some(),
            "tui_direct_pending_start: deferred-claim marker recorded pinning the OWN synthetic turn — its commit drains ⏳ → ✅, a never-committed turn converges to the bounded sweep ⚠ (#3303)"
        ),
        Err(error) => tracing::warn!(
            provider = %provider,
            channel_id,
            anchor_message_id,
            error = %error,
            "tui_direct_pending_start: failed to persist the deferred-claim marker; claim proceeds without it (fail-open — pre-#3303 behavior, the anchor ⏳ may linger) (#3303)"
        ),
    }
}
