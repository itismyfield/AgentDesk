//! EPIC #3016 — Single-authority `TurnFinalizer`.
//!
//! Finalization used to be a distributed handshake (`mailbox_finish_turn`
//! called from ~17 sites, a per-handle `mailbox_finalize_owed` CAS, ≥10
//! `global_active` decrement sites) whose races caused the recurring "turn
//! never finalizes / inflight stuck" bug. This module is ONE actor that OWNS
//! the side-effects of finalize as an atomic, exactly-once unit: (1) inflight
//! clear (honouring `PlannedRestartSkipped`), (2) mailbox cancel_token release
//! via `mailbox_finish_turn`, (3) `global_active` decrement gated on
//! `removed_token.is_some()`, (4) the trailing terminal side-effects (watchdog
//! override clear, `dispatch_thread_parents` retain, voice barge-in drain,
//! `dispatch_role_overrides` cleanup).
//!
//! Exactly-once is decided in one place — the actor task's `Terminal` handling
//! — via a per-turn ledger phase gate (`Pending → Finalizing → Finalized`)
//! inside a single task, so no CAS arbitrates who finalizes. Because
//! `mailbox_finish_turn` is idempotent and the counter decrement is gated on
//! `removed_token.is_some()`, a double-finalize is a harmless no-op — never an
//! underflow, never a double Discord notice.

use std::{collections::HashMap, sync::Arc, time::Duration};

use serenity::model::id::ChannelId;
// `tokio::time::Instant` (not `std::time::Instant`) so deadlines respect the
// paused/virtual test clock and the production `interval` clock alike.
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::provider::{CancelToken, ProviderKind};

use super::SharedData;
// #3041 P1-0: dormant lease types for the *Delivery messages below (mod.rs §2-§3).
use super::{DeliveryLeaseCell, LeaseHolder, LeaseOutcome};

mod cleanup;

pub(in crate::services::discord) use cleanup::SyntheticClaimSnapshot;

/// How often the reconciler `Tick` fires to re-check deadline-armed
/// gate-timeout entries and garbage-collect the ledger.
const RECONCILE_INTERVAL: Duration = Duration::from_millis(1000);

/// Bounded backstop for a gate-timeout whose pane is still busy and whose
/// relay owner is still alive. The reconciler finalizes once the pane
/// quiesces, the owner dies, OR this deadline elapses — seconds, NOT the
/// 1800s placeholder-sweeper horizon the old silent SKIP deferred to (the
/// hosted-TUI pre-submit busy guard remains the correctness floor).
const GATE_BACKSTOP: Duration = Duration::from_secs(8);

/// #3016 phase-5a — the FAR backstop for a watcher-owned `register_start`
/// Pending that never received a terminal (the under-finalize gap the legacy
/// `placeholder_sweeper` SKIPS once content was delivered, ~393). The 1800s
/// horizon is GENEROUS on purpose — never prematurely finalize a legitimately
/// long paused-live turn; at the deadline the reconciler re-checks liveness
/// (`watcher_backstop_turn_is_terminal`) so a still-live turn is deferred.
const WATCHER_REGISTER_BACKSTOP: Duration =
    Duration::from_secs(super::placeholder_sweeper::ABANDON_THRESHOLD_SECS);

/// #3277 (Defect C) — proven-terminal FAST path for the watcher far-backstop.
/// In the #3277 incident the handed-off turn was already PROVABLY complete
/// (JSONL terminator on disk) while its watcher owner sat parked at transcript
/// EOF, so no data-driven finalize ever fired and the channel stayed stranded
/// for the full 1800s. The reconciler therefore PROBES watcher-owned Pending
/// entries with the STRICT (`at_deadline = false`) form of
/// `watcher_backstop_turn_is_terminal`: after
/// `WATCHER_BACKSTOP_TERMINAL_STREAK` terminal probes this interval apart, the
/// far deadline is pulled in to `GATE_BACKSTOP` for a third (still strict)
/// confirmation before finalizing. A single non-terminal probe resets the
/// streak (paused / paused-live / flapping turns keep the generous horizon).
const WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// Consecutive terminal probes required before the fast path pulls the
/// watcher far-backstop deadline in (see above).
const WATCHER_BACKSTOP_TERMINAL_STREAK: u8 = 2;

/// TTL after which a `Finalized` ledger entry is garbage-collected so the
/// ledger stays bounded while still suppressing a late double-submit.
const FINALIZED_TTL: Duration = Duration::from_secs(60);

/// Identity carried by a submission. The ledger key is the FULL identity
/// (`channel_id`, `generation`, `user_msg_id`) so two SEQUENTIAL turns in the
/// same channel are distinct entries — a finalized turn-1 must NOT swallow
/// turn-2's terminal as `AlreadyFinalized` (stuck-channel bug). A channel-only
/// terminal (`user_msg_id == 0`, recovery/orphan paths) collapses onto the
/// channel's single live entry (see `resolve_ledger_key`), never a literal 0.
#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) struct TurnKey {
    pub(in crate::services::discord) channel_id: ChannelId,
    /// 0 == "unknown identity" (recovery/orphan paths): resolved to the
    /// channel's single live entry instead of a literal-0 key.
    pub(in crate::services::discord) user_msg_id: u64,
    pub(in crate::services::discord) generation: u64,
}

impl TurnKey {
    pub(in crate::services::discord) fn new(
        channel_id: ChannelId,
        user_msg_id: u64,
        generation: u64,
    ) -> Self {
        Self {
            channel_id,
            user_msg_id,
            generation,
        }
    }

    /// The literal full-identity key for this turn. Shared with the
    /// `StatusPanelController` (#3078), which keys its panel ledger on the same
    /// `LedgerKey` so the panel and finalize ledgers collapse channel-only
    /// (`user_msg_id == 0`) recovery/orphan terminals onto the same live entry.
    pub(in crate::services::discord) fn exact_key(&self) -> LedgerKey {
        LedgerKey {
            channel_id: self.channel_id,
            generation: self.generation,
            user_msg_id: self.user_msg_id,
        }
    }
}

/// The exact ledger match: channel + restart generation + user message id.
/// Full identity so sequential same-channel turns never collide. Shared with
/// the `StatusPanelController` (#3078) so both ledgers key turns identically.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(in crate::services::discord) struct LedgerKey {
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) generation: u64,
    pub(in crate::services::discord) user_msg_id: u64,
}

/// Resolve a submission to the ledger entry it acts on. A real `user_msg_id`
/// uses its exact key (a stale terminal of a finalized turn matches the
/// retained `Finalized` entry → `AlreadyFinalized`); a channel-only id-0
/// terminal collapses per `resolve_channel_only` (never prematurely finalizing
/// a queued follow-up).
fn resolve_ledger_key(ledger: &HashMap<LedgerKey, LedgerEntry>, key: TurnKey) -> LedgerKey {
    resolve_channel_only(
        key,
        ledger
            .iter()
            .map(|(lk, entry)| (lk, entry.phase == Phase::Finalized)),
    )
}

/// #3016 S1 (read-only): does the channel's `generation` have a live
/// (non-`Finalized`) ledger entry whose relay owner is the watcher? Pure read;
/// the actor's `QueryWatcherPending` arm calls this without mutating the ledger.
/// #3016 phase-5b1: reachable in production via `has_live_watcher_pending`
/// (wired into the `bridge_handoff_finds_watcher_handle` invariant).
fn ledger_has_live_watcher_pending(
    ledger: &HashMap<LedgerKey, LedgerEntry>,
    channel_id: ChannelId,
    generation: u64,
) -> bool {
    ledger.iter().any(|(lk, entry)| {
        lk.channel_id == channel_id
            && lk.generation == generation
            && entry.phase != Phase::Finalized
            && entry.relay_owner == RelayOwnerKind::Watcher
    })
}

/// Channel-only collapse over an explicit candidate set. The finalizer keeps
/// its in-line `HashMap` walk above; the `StatusPanelController` (#3078) passes
/// its own `(LedgerKey, is_terminal)` pairs here so the identical
/// finalized-guard / single-live-entry semantics apply to the panel ledger
/// without duplicating the subtle ambiguity rule.
///
/// - A real `user_msg_id` uses its exact key.
/// - A channel-only id collapses onto the single non-terminal entry ONLY when
///   no terminal entry exists for the same channel/generation (ambiguous
///   otherwise → route to the literal orphan key, a no-op for the caller).
pub(in crate::services::discord) fn resolve_channel_only<'a>(
    key: TurnKey,
    candidates: impl Iterator<Item = (&'a LedgerKey, bool)> + Clone,
) -> LedgerKey {
    if key.user_msg_id != 0 {
        return key.exact_key();
    }
    let channel_has_terminal = candidates.clone().any(|(lk, is_terminal)| {
        lk.channel_id == key.channel_id && lk.generation == key.generation && is_terminal
    });
    if channel_has_terminal {
        return key.exact_key();
    }
    candidates
        .into_iter()
        .find(|(lk, is_terminal)| {
            lk.channel_id == key.channel_id && lk.generation == key.generation && !*is_terminal
        })
        .map(|(lk, _)| *lk)
        .unwrap_or_else(|| key.exact_key())
}

/// Every actor submits ONE of these terminal events. The finalizer's ledger
/// gate decides exactly-once. Phase 1 wires the variants the bridge/watcher
/// terminals need (Phases 2-3); the remaining sweeper/recovery variants land
/// in Phase 4 and are listed here so the matrix is explicit.
#[derive(Clone, Debug)]
pub(in crate::services::discord) enum TerminalEvent {
    /// Normal completion — bridge or watcher relayed (or intentionally
    /// suppressed) terminal output and the turn is done.
    Complete,
    /// `/!stop`, reaction, watchdog. `mark_completion_cleanup()` is skipped
    /// (the cancel is a real mid-stream stop) but `cancelled.store(true)`
    /// still fires.
    Cancel,
    /// #2293/#2780 hosted-TUI quiescence gate. `pane_quiescent == Some(false)`
    /// with a still-alive relay owner is the ONLY event that defers: it arms a
    /// bounded deadline and the reconciler finalizes when the precondition
    /// clears or the deadline elapses. Replaces the silent watcher SKIP.
    GateTimeout { pane_quiescent: Option<bool> },
    /// No output owner and an empty response — finalize once and emit the
    /// observability event from inside the finalizer.
    #[allow(dead_code)]
    // #3034: handled by the finalizer + tested, but not yet emitted in prod.
    RelayMiss,
}

/// Per-submission knobs that keep each routed call-site behaviourally
/// identical to its pre-#3016 inline sequence during the incremental window.
/// Routed sites preserve their old side-effects; only ownership moves.
#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) struct FinalizeContext {
    /// Whether `do_finalize` clears inflight as part of the finalize. Bridge
    /// branches and the watcher clear inflight inline in their own flow before
    /// submitting, so they pass `false`; only the deadline-armed reconcile
    /// backstop (no caller to clear it) passes `true`.
    pub(in crate::services::discord) clear_inflight: bool,
    /// Whether to mark the removed token's completion-cleanup. The bridge did
    /// this on a non-cancel terminal (still gated on `!event.is_cancel()`); the
    /// watcher's `finish_restored_watcher_active_turn` did NOT — it only set
    /// `cancelled`. Keeping this per-site avoids changing provider-watchdog
    /// semantics on the watcher path.
    pub(in crate::services::discord) allow_completion_cleanup: bool,
    /// Whether to drain voice barge-in deferred prompts as part of finalize.
    /// The bridge branches drain voice; the watcher path did NOT.
    pub(in crate::services::discord) drain_voice: bool,
    /// Whether to schedule a deferred idle-queue kickoff when the finalize
    /// leaves a pending soft-queue (gated on `mailbox_online && has_pending`).
    /// The watcher's `finish_restored_watcher_active_turn` did this; the bridge
    /// branches deferred kickoff to a later site, so they pass `false`.
    pub(in crate::services::discord) kickoff_queue: bool,
}

impl FinalizeContext {
    /// Bridge non-delegation / missing-handoff branches: bridge owns the
    /// inflight clear elsewhere, marks completion-cleanup on non-cancel, drains
    /// voice, defers queue kickoff.
    pub(in crate::services::discord) fn bridge() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: true,
            drain_voice: true,
            kickoff_queue: false,
        }
    }

    /// Watcher terminal via `finish_restored_watcher_active_turn`: the watcher
    /// clears inflight inline before submitting, does NOT mark completion
    /// cleanup, does NOT drain voice. The queue kickoff stays at the caller
    /// because it is gated on the caller's `dispatch_ok`, which the finalizer
    /// cannot see — so the context leaves kickoff to the submitter.
    pub(in crate::services::discord) fn watcher() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: false,
        }
    }

    /// Monitor-auto-turn / recovery terminal (#3016 phase 4): the caller owns
    /// the inflight clear (or there is none — synthetic monitor turn / recovery
    /// already cleared it), does NOT mark completion-cleanup, does NOT drain
    /// voice, but DOES kick off any queued backlog (the pre-#3016
    /// `finish_monitor_auto_turn` / `finish_recovered_turn_mailbox` both
    /// scheduled the deferred idle-queue kickoff on `has_pending`). This is
    /// `watcher()` plus the queue kickoff.
    pub(in crate::services::discord) fn monitor() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: true,
        }
    }

    /// Deadline-armed gate-timeout backstop, fired from the reconciler with no
    /// caller to have cleared inflight: finalize fully (clear inflight here),
    /// no completion-cleanup or voice drain (watcher semantics), kick off the
    /// queue if backlog remains.
    fn gate_backstop() -> Self {
        Self {
            clear_inflight: true,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: true,
        }
    }

    /// #3041 §3 P1-0 (DORMANT): context for a lease-release-driven finalize once
    /// the watcher terminal migrates onto the delivery lease (P1-1..). Mirrors
    /// `watcher()` today (no live caller), but kept as a distinct constructor so
    /// wired phases can tune the lease-release knobs independently.
    #[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
    pub(in crate::services::discord) fn delivery_lease() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: false,
        }
    }
}

/// Outcome of a terminal submission. The submitter uses this to decide whether
/// it owns the post-finalize bookkeeping (it performed the one finalize) or
/// must do nothing (someone else already finalized this turn).
pub(in crate::services::discord) enum FinalizeOutcome {
    /// This submission performed the one finalize.
    Finalized {
        removed_token: Option<Arc<CancelToken>>,
        has_pending: bool,
        mailbox_online: bool,
    },
    /// Another actor already finalized this turn; the submitter must do
    /// nothing (no counter touch, no Discord notice, no re-relay).
    AlreadyFinalized,
    /// Gate-timeout with a still-busy pane and a live relay owner — recorded
    /// with a bounded deadline; the reconciler will finalize when the
    /// precondition clears.
    Deferred,
}

/// #3016 S1: the structural completion signal from the provider's JSONL
/// transcript, independent of the ledger.
/// - `Done` — strict reverse-scan found a definitive terminator (Claude
///   `result`/`system{...}`, Codex `turn.completed`): structurally over.
/// - `PausedLive` — in-flight or inconclusive evidence; conservatively a live,
///   paused turn.
/// - `Unknown` — no structured on-disk JSONL turn state (LegacyTmuxWrapper /
///   ProcessBackend / ClaudeEAdapter, or a non-JSONL provider).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
// #3016 S3: wired into the watcher fresh-idle finalize decision.
pub(in crate::services::discord) enum CompletionSignal {
    Done,
    PausedLive,
    Unknown,
}

/// Ledger phase for a single turn. Owned solely by the actor task; the
/// check-and-set on this enum is the one place exactly-once is decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Pending,
    Finalizing,
    Finalized,
}

struct LedgerEntry {
    phase: Phase,
    relay_owner: RelayOwnerKind,
    provider: ProviderKind,
    /// The originating identity, retained so the deadline-armed reconciler can
    /// reconstruct a `TurnKey` for `do_finalize` (channel-scoped, so any of the
    /// colliding ids is equivalent).
    turn_key: TurnKey,
    /// Backstop deadline for a deferred gate-timeout. `None` unless a
    /// `GateTimeout{Some(false)}` armed it.
    terminal_deadline: Option<Instant>,
    /// #3016 phase-5a — FAR backstop deadline armed at `register_start` for a
    /// watcher-owned turn (distinct from the short `terminal_deadline`): the
    /// generous `WATCHER_REGISTER_BACKSTOP` horizon that makes a
    /// never-terminated watcher handoff VISIBLE to the reconciler, which
    /// re-checks liveness before finalizing. `None` for non-watcher owners.
    watcher_backstop_deadline: Option<Instant>,
    /// #3277 (Defect C) — when the proven-terminal fast path last probed this
    /// entry (`None` = never probed; the first reconcile tick probes it).
    watcher_backstop_probe_at: Option<Instant>,
    /// #3277 (Defect C) — consecutive terminal probes observed so far. Reset
    /// to 0 by any non-terminal probe and by an at-deadline deferral.
    watcher_backstop_terminal_streak: u8,
    /// #3277 codex r1 — true while `watcher_backstop_deadline` is the fast-path
    /// PULLED one (its re-check stays STRICT); false on the natural horizon.
    watcher_backstop_deadline_pulled: bool,
    /// When the entry reached `Finalized`, for TTL-based GC.
    finalized_at: Option<Instant>,
}

/// Messages the actor task drains. Each carries `Arc<SharedData>` so the
/// finalize side-effects can run inside the task without the finalizer holding
/// a `Weak<SharedData>` back-reference (which would re-introduce an Arc cycle
/// and ordering ambiguity).
enum FinalizeMsg {
    /// #3018 register: submitted synchronously at intake/handoff BEFORE the
    /// watcher can submit a terminal (arrival order replaces the deleted
    /// Release/AcqRel `mailbox_finalize_owed.store` ordering). #3016 phase-5a:
    /// carries a `Weak<SharedData>` (NOT `Arc` — no cycle) so the actor primes
    /// `cached_shared` from the very FIRST `Start` and the far-backstop tick
    /// runs even when no terminal ever arrives.
    Start {
        key: TurnKey,
        provider: ProviderKind,
        relay_owner: RelayOwnerKind,
        shared: std::sync::Weak<SharedData>,
    },
    Terminal {
        key: TurnKey,
        provider: ProviderKind,
        event: TerminalEvent,
        ctx: FinalizeContext,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<FinalizeOutcome>,
    },
    /// #3041 §2-§3 (DORMANT until P1-2..): CAS-acquire `(key, [start,end))` for
    /// `holder` via the actor. The watcher acquires the cell directly (B4
    /// fast-path), so this variant has no sender yet — it is reserved for the
    /// sink/bridge wiring.
    #[allow(dead_code)] // #3041: no sender until sink/bridge wiring (P1-2..).
    AcquireDelivery {
        key: TurnKey,
        lease: Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        deadline_ms: u64,
        ack: oneshot::Sender<bool>,
    },
    /// #3041 three-way commit; full-identity mismatch = no-op. A `Delivered`
    /// commit also advances the channel's `confirmed_end_offset` watermark to
    /// `end` (§5.2) via the SAME monotonic CAS the watcher's inline advance
    /// uses. DORMANT (reverted in P1-1): the watcher commits + advances INLINE
    /// (`watcher_lease_commit_advance`) because the actor-commit deferral
    /// reopened the #3143 duplicate window; kept for the §5.3 phase.
    #[allow(dead_code)] // #3041: wired in a later phase (ledger-coupled commit, §5.3).
    CommitDelivery {
        key: TurnKey,
        lease: Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
        provider: ProviderKind,
        tmux_session_name: String,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<bool>,
    },
    /// #3041 compare-and-release; full-identity match only. DORMANT (reverted in
    /// P1-1): the watcher releases its lease INLINE after the inline commit, NOT
    /// via this awaited actor round-trip. Kept defined for a later phase.
    #[allow(dead_code)] // #3041: wired in a later phase (alongside CommitDelivery).
    ReleaseDelivery {
        key: TurnKey,
        lease: Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        ack: oneshot::Sender<bool>,
    },
    /// #3016 S1 (A2-banked, read-only): ask the ledger whether the channel's
    /// `generation` has a live (non-`Finalized`) entry that the watcher owns.
    /// Pure read of the actor-owned ledger; mutates nothing. #3016 phase-5b1:
    /// wired into production via `has_live_watcher_pending`.
    QueryWatcherPending {
        channel_id: ChannelId,
        generation: u64,
        ack: oneshot::Sender<bool>,
    },
}

/// A per-runtime actor, held as `Arc<TurnFinalizer>` on `SharedData`. One
/// owning task drains the `mpsc`; all public methods are cheap
/// submit-or-await wrappers.
pub(in crate::services::discord) struct TurnFinalizer {
    tx: mpsc::UnboundedSender<FinalizeMsg>,
}

impl TurnFinalizer {
    /// Spawn the owning actor task and return the handle. Cheap and idle until
    /// the first submission, so it is safe to construct unconditionally at
    /// every `SharedData` construction site (incl. tests). The actor task is
    /// only spawned when a Tokio runtime handle is available: synchronous unit
    /// tests build `SharedData` outside any runtime (`tokio::spawn` would
    /// panic), so without a reactor the unbounded sender just buffers and the
    /// dormant finalizer stays inert.
    pub(in crate::services::discord) fn spawn() -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(actor_loop(rx));
        }
        Arc::new(Self { tx })
    }

    /// #3018 — register a turn so the ledger knows it exists before any
    /// terminal can arrive. Idempotent: a second `Start` for a key already in
    /// the ledger only refreshes the relay owner. #3016 phase-5a: `shared` is
    /// downgraded to a `Weak` carried on the `Start` so the actor primes its
    /// `cached_shared` from the first register (see `FinalizeMsg::Start`).
    pub(in crate::services::discord) fn register_start(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        relay_owner: RelayOwnerKind,
        shared: &Arc<SharedData>,
    ) {
        // UnboundedSender::send only fails if the actor task is gone (process
        // teardown); dropping the Start there is harmless because no terminal
        // will be awaited either.
        let _ = self.tx.send(FinalizeMsg::Start {
            key,
            provider,
            relay_owner,
            shared: Arc::downgrade(shared),
        });
    }

    /// Submit a terminal event and await the exactly-once decision. Returns
    /// `AlreadyFinalized` if another actor already finalized this turn.
    pub(in crate::services::discord) async fn submit_terminal(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        event: TerminalEvent,
        ctx: FinalizeContext,
        shared: Arc<SharedData>,
    ) -> FinalizeOutcome {
        self.submit_terminal_with_claim_snapshot(key, provider, event, ctx, None, shared)
            .await
    }

    /// #3350 codex r1-1: `submit_terminal` carrying the submit-time row
    /// snapshot for the #3303 DeferredClaim marker ensure. Watcher submitters
    /// clear the row BEFORE submitting, so `do_finalize`'s row re-load proves
    /// nothing for their turns — the pre-clear snapshot is the identity
    /// evidence. The idempotent, reaction-free ensure runs HERE, before the
    /// exactly-once gate: an `AlreadyFinalized` loser's snapshot still ensures
    /// (the winner may have finalized row/snapshot-less; gates: cleanup.rs).
    pub(in crate::services::discord) async fn submit_terminal_with_claim_snapshot(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        event: TerminalEvent,
        ctx: FinalizeContext,
        claim_snapshot: Option<SyntheticClaimSnapshot>,
        shared: Arc<SharedData>,
    ) -> FinalizeOutcome {
        if let Some(snapshot) = claim_snapshot.as_ref() {
            cleanup::ensure_synthetic_claim_marker_before_clear(key, &provider, Some(snapshot));
        }
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(FinalizeMsg::Terminal {
                key,
                provider: provider.clone(),
                event: event.clone(),
                ctx,
                shared: shared.clone(),
                ack,
            })
            .is_err()
        {
            // Actor task gone: stop submitter-side bookkeeping.
            return FinalizeOutcome::AlreadyFinalized;
        }
        let Ok(out) = rx.await else {
            return FinalizeOutcome::AlreadyFinalized;
        };
        if matches!(out, FinalizeOutcome::AlreadyFinalized) {
            cleanup::already_finalized_active_state(key, &provider, &event, ctx, &shared).await;
        }
        out
    }

    /// #3041: route a three-way `CommitDelivery` through the actor so the lease
    /// transition AND the `Delivered`-commit offset advance run as one serialized
    /// unit on the finalize owner. Returns whether the lease actually committed
    /// (identity matched a live `Leased` lease). If the actor task is gone
    /// (teardown) returns `false`.
    ///
    /// DORMANT (reverted in P1-1): the watcher commits + advances INLINE (see
    /// tmux_watcher.rs) to keep the pre-P1-1 prompt advance — awaiting this
    /// behind the actor's `Terminal` mailbox reopened the #3143 duplicate window.
    /// Retained (exercised only by the lease unit tests) for a later
    /// ledger-coupled-commit phase (§5.3).
    #[allow(dead_code)] // #3041: wired in a later phase (ledger-coupled commit, §5.3).
    pub(in crate::services::discord) async fn commit_delivery(
        &self,
        key: TurnKey,
        lease: Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
        provider: ProviderKind,
        tmux_session_name: String,
        shared: Arc<SharedData>,
    ) -> bool {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(FinalizeMsg::CommitDelivery {
                key,
                lease,
                holder,
                start,
                end,
                outcome,
                provider,
                tmux_session_name,
                shared,
                ack,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// #3041: route a compare-and-`ReleaseDelivery` through the actor. Returns
    /// the standard `Committed`/`Leased` → `Unleased` result; identity mismatch
    /// is a no-op `false`. If the actor task is gone returns `false`.
    ///
    /// DORMANT (reverted in P1-1): the watcher releases INLINE after its inline
    /// commit. Retained (exercised only by the lease unit tests) for a later
    /// phase.
    #[allow(dead_code)] // #3041: wired in a later phase (alongside commit_delivery).
    pub(in crate::services::discord) async fn release_delivery(
        &self,
        key: TurnKey,
        lease: Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        start: u64,
        end: u64,
    ) -> bool {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(FinalizeMsg::ReleaseDelivery {
                key,
                lease,
                holder,
                start,
                end,
                ack,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// #3016 S1 (PURE, READ-ONLY): the structural completion signal for a turn,
    /// derived solely from the provider's on-disk JSONL transcript — a
    /// stateless read that never touches the ledger or actor channel.
    ///
    /// Unlike the idle-queue drain's LENIENT `jsonl_strict_terminator_idle`
    /// (whole "Idle-class" family — correct for "ready for input?" but WRONG
    /// for "did THIS turn end?", since a completed `agent_message` right before
    /// a tool call is mid-turn), this applies the STRICTER turn-END terminator:
    /// gate on `provider_runtime_has_structured_jsonl_turn_state`, then
    /// `jsonl_turn_end_terminator_idle`, which accepts ONLY the authoritative
    /// per-provider turn terminator (Codex `turn.completed`; Claude `result` /
    /// `system{turn_duration|stop_hook_summary}`). Non-JSONL runtime →
    /// `Unknown`; terminator found → `Done`; else → `PausedLive`.
    ///
    /// #3016 S3 (Concern 3): intentionally NO `turn_start_offset` param. The
    /// turn-END reverse scan is relay-offset-independent by construction, and
    /// TURN-correctness is guaranteed at the call site (the watcher fresh-idle
    /// decision pins the finalize id from a pre-cleanup inflight snapshot —
    /// `pinned_finalize_user_msg_id` /
    /// `committed_completion_is_stale_for_newer_turn`); an offset param here
    /// could only be silently ignored. Wired into
    /// `watcher_fresh_idle_finalize_decision` — Done → finalize, PausedLive →
    /// defer, Unknown → legacy flag-gated.
    pub(in crate::services::discord) fn completion_signal_state(
        &self,
        provider: &ProviderKind,
        runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
        transcript_path: &std::path::Path,
    ) -> CompletionSignal {
        completion_signal_from_transcript(provider, runtime_kind, transcript_path)
    }

    /// #3016 S1 (A2-banked, READ-ONLY): whether the channel's `generation` has a
    /// live (non-`Finalized`) ledger entry owned by the watcher. Routes a
    /// read-only `QueryWatcherPending` through the actor (the ledger is owned by
    /// the actor task) and awaits the answer; it mutates nothing. If the actor
    /// task is gone (teardown) it returns `false`. #3016 phase-5b1: wired into
    /// the `bridge_handoff_finds_watcher_handle` invariant (`turn_bridge/mod.rs`)
    /// in place of the legacy `mailbox_finalize_owed.load()` consumer.
    pub(in crate::services::discord) async fn has_live_watcher_pending(
        &self,
        channel_id: ChannelId,
        generation: u64,
    ) -> bool {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(FinalizeMsg::QueryWatcherPending {
                channel_id,
                generation,
                ack,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

/// The single owning task. Owns the ledger and a NON-owning cached handle to
/// `SharedData`, and drives the reconcile timer in the same task via `select!`
/// so the deadline-armed gate-timeout finalize fires deterministically (no
/// cross-task nudge ordering to reason about).
async fn actor_loop(mut rx: mpsc::UnboundedReceiver<FinalizeMsg>) {
    let mut ledger: HashMap<LedgerKey, LedgerEntry> = HashMap::new();
    // A `Weak` (NOT `Arc`) so the actor never keeps `SharedData` alive: the
    // cycle would otherwise be SharedData → Arc<TurnFinalizer> → sender →
    // actor → cached Arc<SharedData>, leaking the whole runtime across a
    // provider restart. The reconcile pass upgrades on demand; if the upgrade
    // fails the runtime is shutting down and there is nothing left to finalize.
    let mut cached_shared: Option<std::sync::Weak<SharedData>> = None;
    let mut reconcile_timer = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_msg = rx.recv() => {
                let Some(msg) = maybe_msg else { break };
                match msg {
                    FinalizeMsg::Start {
                        key,
                        provider,
                        relay_owner,
                        shared,
                    } => {
                        // #3016 phase-5a: prime the reconcile cache from the very
                        // FIRST register so a FRESH actor whose first watcher-owned
                        // turn never submits its own terminal still gets the
                        // far-backstop tick. `is_none()` guard: the `Terminal` arm
                        // stays authoritative (always overwrites with the live Arc);
                        // a dead `Weak` here just skips reconcile harmlessly until a
                        // live submission re-primes it.
                        if cached_shared.is_none() {
                            cached_shared = Some(shared);
                        }
                        // #3016 phase-5a: a watcher-owned handoff arms the FAR
                        // backstop so a never-terminated turn is visible to the
                        // reconciler. `get_or_insert`: set ONCE — a repeat `Start`
                        // must never push an armed deadline forward (the EPIC's
                        // never-finalizing bug, mirrored from the gate-timeout arm).
                        let arm_watcher_backstop = relay_owner == RelayOwnerKind::Watcher;
                        // A `Start` always carries the real `user_msg_id`, so it
                        // registers under the exact full-identity key.
                        ledger
                            .entry(key.exact_key())
                            .and_modify(|e| {
                                // Only refresh the owner while still live; never
                                // resurrect a finalized turn.
                                if e.phase != Phase::Finalized {
                                    e.relay_owner = relay_owner;
                                    e.provider = provider.clone();
                                    e.turn_key = key;
                                    if arm_watcher_backstop {
                                        e.watcher_backstop_deadline.get_or_insert_with(|| {
                                            Instant::now() + WATCHER_REGISTER_BACKSTOP
                                        });
                                    }
                                }
                            })
                            .or_insert(LedgerEntry {
                                phase: Phase::Pending,
                                relay_owner,
                                provider,
                                turn_key: key,
                                terminal_deadline: None,
                                watcher_backstop_deadline: arm_watcher_backstop
                                    .then(|| Instant::now() + WATCHER_REGISTER_BACKSTOP),
                                watcher_backstop_probe_at: None,
                                watcher_backstop_terminal_streak: 0,
                                watcher_backstop_deadline_pulled: false,
                                finalized_at: None,
                            });
                    }
                    FinalizeMsg::Terminal {
                        key,
                        provider,
                        event,
                        ctx,
                        shared,
                        ack,
                    } => {
                        cached_shared = Some(Arc::downgrade(&shared));
                        let outcome =
                            handle_terminal(&mut ledger, key, provider, event, ctx, &shared)
                                .await;
                        let _ = ack.send(outcome);
                    }
                    // #3041 §2-§3 P1-0 (DORMANT, UNREACHABLE today). Routing these
                    // through the actor serializes lease transitions on the finalize
                    // owner (P1-1.. relies on this). Nothing sends them yet.
                    FinalizeMsg::AcquireDelivery {
                        key,
                        lease,
                        holder,
                        start,
                        end,
                        deadline_ms,
                        ack,
                    } => {
                        let won = handle_acquire_delivery(
                            &lease,
                            key,
                            holder,
                            start,
                            end,
                            deadline_ms,
                        );
                        let _ = ack.send(won);
                    }
                    FinalizeMsg::CommitDelivery {
                        key,
                        lease,
                        holder,
                        start,
                        end,
                        outcome,
                        provider,
                        tmux_session_name,
                        shared,
                        ack,
                    } => {
                        let committed = handle_commit_delivery(
                            &lease,
                            key,
                            holder,
                            start,
                            end,
                            outcome,
                            &provider,
                            &tmux_session_name,
                            &shared,
                        );
                        let _ = ack.send(committed);
                    }
                    FinalizeMsg::ReleaseDelivery {
                        key,
                        lease,
                        holder,
                        start,
                        end,
                        ack,
                    } => {
                        let released = handle_release_delivery(&lease, key, holder, start, end);
                        let _ = ack.send(released);
                    }
                    // #3016 S1: pure read of the actor-owned ledger — no mutation.
                    FinalizeMsg::QueryWatcherPending {
                        channel_id,
                        generation,
                        ack,
                    } => {
                        let pending =
                            ledger_has_live_watcher_pending(&ledger, channel_id, generation);
                        let _ = ack.send(pending);
                    }
                }
            }
            _ = reconcile_timer.tick() => {
                if let Some(shared) = cached_shared.as_ref().and_then(std::sync::Weak::upgrade) {
                    reconcile(&mut ledger, &shared).await;
                }
            }
        }
    }
}

/// Exactly-once gate + immediate-finalize / deferral decision. Runs inside the
/// actor task, so the check-and-set on `Phase` needs no synchronization.
async fn handle_terminal(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    key: TurnKey,
    provider: ProviderKind,
    event: TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
) -> FinalizeOutcome {
    // Resolve to the entry this terminal acts on: a real id keys exactly; a
    // channel-only id-0 collapses onto the channel's single live entry
    // (recovery/orphan). An unregistered turn (post-restart inflight, no live
    // `Start`) still finalizes below — idempotent, exactly-once.
    let ledger_key = resolve_ledger_key(ledger, key);

    // Codex P1 — ambiguous channel-only terminal: an id-0 submission that fell
    // back to the literal orphan key (a recently-`Finalized` entry exists) is
    // most likely that turn's STALE terminal. With a DIFFERENT live entry on
    // this channel, the channel-scoped finish would release the follow-up's
    // token / decrement `global_active` — treat as no-op. (A genuine orphan
    // with NO live entry still finalizes below; idempotent + counter-gated.)
    if key.user_msg_id == 0 && !ledger.contains_key(&ledger_key) {
        let channel_has_live_turn = ledger.iter().any(|(lk, e)| {
            lk.channel_id == key.channel_id
                && lk.generation == key.generation
                && e.phase != Phase::Finalized
        });
        if channel_has_live_turn {
            return FinalizeOutcome::AlreadyFinalized;
        }
    }

    let entry = ledger.entry(ledger_key).or_insert(LedgerEntry {
        phase: Phase::Pending,
        relay_owner: RelayOwnerKind::None,
        provider,
        turn_key: key,
        terminal_deadline: None,
        // Orphan/terminal-first entries (no prior `register_start`) finalize
        // right here, so they never need the watcher far-backstop.
        watcher_backstop_deadline: None,
        watcher_backstop_probe_at: None,
        watcher_backstop_terminal_streak: 0,
        watcher_backstop_deadline_pulled: false,
        finalized_at: None,
    });

    match entry.phase {
        Phase::Finalizing | Phase::Finalized => {
            return FinalizeOutcome::AlreadyFinalized;
        }
        Phase::Pending => {}
    }

    // Gate-timeout with a still-busy pane AND a live relay owner is the only
    // event that defers instead of finalizing now.
    let mut effective_ctx = ctx;
    if let TerminalEvent::GateTimeout {
        pane_quiescent: Some(false),
    } = event
    {
        if entry.relay_owner != RelayOwnerKind::None {
            // Arm the backstop deadline ONCE. The watcher submits a
            // GateTimeout on every pass while the pane stays busy; if each
            // submission pushed the deadline forward by GATE_BACKSTOP the
            // backstop would never fire on a persistently busy pane (exactly
            // the never-finalizing bug). So only set it if not already armed.
            entry
                .terminal_deadline
                .get_or_insert_with(|| Instant::now() + GATE_BACKSTOP);
            return FinalizeOutcome::Deferred;
        }
        // No live relay owner → nothing will drive the pane to quiescence;
        // finalize now. This recovered/orphan watcher case (post-restart
        // inflight, no `register_start`) has no later watcher block to clear
        // inflight or kick the queue — the caller's `watcher()` submit SKIPS
        // its cleanup block and discards this outcome, so reproduce what the
        // deadline-armed `gate_backstop()` would have done: clear inflight
        // here (else the file keeps blocking the channel after the mailbox
        // release) AND kick off the queued soft-queue backlog (else a queued
        // follow-up stays stuck — the EPIC restart/#3011 regression).
        effective_ctx.clear_inflight = true;
        effective_ctx.kickoff_queue = true;
    }

    // Flip Pending → Finalizing, run the side-effects, flip → Finalized.
    entry.phase = Phase::Finalizing;
    let provider = entry.provider.clone();
    // Codex P1 — finalize on the RESOLVED identity: an id-0 terminal that
    // collapsed onto an entry registered with the real `user_msg_id` finalizes
    // under THAT identity, so `do_finalize` takes the guarded if-matches paths
    // instead of the unguarded channel-scoped finish (which could release a
    // newer turn's token when the entry is stale). Otherwise the same key.
    let finalize_key = if key.user_msg_id == 0 && entry.turn_key.user_msg_id != 0 {
        entry.turn_key
    } else {
        key
    };
    let outcome = do_finalize(finalize_key, provider, &event, effective_ctx, shared).await;
    if let Some(entry) = ledger.get_mut(&ledger_key) {
        entry.phase = Phase::Finalized;
        entry.finalized_at = Some(Instant::now());
        entry.terminal_deadline = None;
    }
    outcome
}

/// The single owner of finalize's side-effects. Reproduces today's exact
/// `clear_inflight (per-site) + mailbox_finish_turn + counter-- + trailing
/// terminal side-effects` sequence so each routed call-site stays
/// behaviourally identical during the incremental landing.
async fn do_finalize(
    key: TurnKey,
    provider: ProviderKind,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
) -> FinalizeOutcome {
    let channel_id = key.channel_id;

    // #3350 ②: ensure the #3303 DeferredClaim marker for a watcher-owned TUI-direct
    // synthetic turn BEFORE (A) erases the row evidence. Codex r1-1: watcher
    // submitters cleared the row pre-submit, so for them this row re-load proves
    // nothing — their guarantee runs at submit time from the pre-clear snapshot
    // (`submit_terminal_with_claim_snapshot`); rationale/gates: cleanup.rs.
    cleanup::ensure_synthetic_claim_marker_before_clear(key, &provider, None);

    // (A) inflight clear. Only the gate-timeout backstop and the immediate
    //     no-owner restored-watcher path set `clear_inflight` (live bridge /
    //     watcher sites clear inline, pass `false`). They consolidate the
    //     pre-#3016 IDENTITY-GUARDED 1800s sweeper: a real identity clears via
    //     `clear_inflight_state_if_matches` — never a newer turn's inflight,
    //     preserving `PlannedRestartSkipped` / `RebindOriginSkipped`; a true
    //     orphan (id-0, nothing to authenticate) keeps the unguarded clear.
    if ctx.clear_inflight {
        if key.user_msg_id != 0 {
            let _ = super::inflight::clear_inflight_state_if_matches(
                &provider,
                channel_id.get(),
                key.user_msg_id,
            );
        } else {
            super::inflight::clear_inflight_state(&provider, channel_id.get());
        }
    }

    // (B) mailbox cancel_token release — the routed sites' single, idempotent
    //     `mailbox_finish_turn` (`removed_token = None` on a second call).
    //     #3016 root-cause: a real identity uses the IDENTITY-GUARDED finish so
    //     finalize only releases the token it owns — a stale channel-scoped
    //     terminal post-finalize/ledger-GC must not release the NEWER turn's
    //     token or decrement `global_active`. Ambiguous id-0 (recovery/orphan)
    //     keeps the channel-scoped finish (ledger gate + id-0 no-op bound it).
    let finish = if key.user_msg_id != 0 {
        super::mailbox_finish_turn_if_matches(
            shared,
            &provider,
            channel_id,
            serenity::model::id::MessageId::new(key.user_msg_id),
        )
        .await
    } else {
        super::mailbox_finish_turn(shared, &provider, channel_id).await
    };

    if let Some(token) = finish.removed_token.as_ref() {
        // A normal completion releases lingering token observers via
        // `mark_completion_cleanup` so provider watchdogs don't treat the
        // post-terminal `cancelled` flip as a live mid-stream cancel. A real
        // cancel must NOT mark completion-cleanup; nor does the watcher path
        // (it historically only set `cancelled`).
        if ctx.allow_completion_cleanup && !matches!(event, TerminalEvent::Cancel) {
            token.mark_completion_cleanup();
        }
        // Stop any lingering watchdog timer from firing on a newer turn's
        // token.
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // (C) #3019 active-counter — decrement ONLY here, ONLY when this submission
    //     actually removed the active turn. Gating on `removed_token.is_some()`
    //     is what guarantees no underflow even under a transitional
    //     double-call.
    if finish.removed_token.is_some() {
        super::saturating_decrement_global_active(shared);
    }

    // The CHANNEL-SCOPED trailing side-effects (D)/(E) below mutate per-channel
    // routing/watchdog state that belongs to whatever turn is CURRENTLY active
    // in the channel. They are safe to run when this finalize actually finished
    // the turn (`removed_token.is_some()`), and harmlessly idempotent on the
    // legacy unguarded id-0 path (which always ran them). But when the
    // IDENTITY-GUARDED finish MISSED — a real `user_msg_id` that did NOT match
    // the live active turn, so `removed_token` is `None` — a DIFFERENT (newer)
    // turn owns the channel. Running these would clear the newer turn's
    // watchdog override, drop its `dispatch_thread_parents` / `dispatch_role_
    // overrides`, and drain its voice deferrals, corrupting a turn this stale
    // terminal does not own (Codex P2). So when the guard was used and missed,
    // skip the channel cleanup entirely — exactly as we already skip the token
    // release and counter decrement. (An id-0 orphan keeps today's behaviour.)
    let guarded_finish_missed = key.user_msg_id != 0 && finish.removed_token.is_none();

    let has_pending_after_voice = if guarded_finish_missed {
        // No-op finalize on a stale terminal: leave the live newer turn's
        // channel state untouched. Report NO backlog (Codex P2): the newer turn
        // is still active and owns its queue. Surfacing `finish.has_pending`
        // here would let the bridge propagate `has_queued_turns` and later drain
        // a queued soft message behind the live turn — concurrently dispatching
        // a follow-up this stale terminal does not own. A guarded miss is a true
        // no-op: no queue kickoff, no backlog reporting.
        false
    } else {
        // (D) trailing terminal side-effects that today follow
        //     `mailbox_finish_turn` inline at the bridge/watcher call-sites.
        //     Moved here so they cannot diverge between the routed paths.
        super::clear_watchdog_deadline_override(channel_id.get()).await;
        shared
            .dispatch_thread_parents
            .retain(|_, thread| *thread != channel_id);

        let voice_deferred_enqueued = if ctx.drain_voice {
            shared
                .voice_barge_in
                .drain_deferred_after_turn(shared, &provider, channel_id)
                .await
        } else {
            false
        };
        let has_pending_after_voice = finish.has_pending || voice_deferred_enqueued;
        if !has_pending_after_voice {
            shared.dispatch_role_overrides.remove(&channel_id);
        }

        // (E) optional deferred queue kickoff (watcher path), gated exactly as
        //     `finish_restored_watcher_active_turn` did.
        if ctx.kickoff_queue && finish.mailbox_online && has_pending_after_voice {
            // #3005: idle has just been confirmed on this finalize, so let the
            // first kickoff attempt run immediately (skipping the 2s pre-sleep)
            // instead of waiting the full deferred-drain INITIAL_DELAY before a
            // queued follow-up can start. Subsequent retries keep the existing
            // 2s cadence (e.g. if the hosted TUI is still transiently Busy).
            super::schedule_deferred_idle_queue_kickoff_immediate(
                shared.clone(),
                provider.clone(),
                channel_id,
                "turn_finalizer terminal completion with queued backlog",
            );
        }
        has_pending_after_voice
    };

    cleanup::finalized_reaction_lifecycle(key, event, ctx, shared, "finalized");

    // (F) relay-miss observability — emitted from inside the finalizer so the
    //     signal fires exactly once per finalize regardless of submitter.
    if matches!(event, TerminalEvent::RelayMiss) {
        crate::services::observability::emit_inflight_lifecycle_event(
            provider.as_str(),
            channel_id.get(),
            None,
            None,
            None,
            "relay_miss_finalized",
            serde_json::json!({
                "removed_token": finish.removed_token.is_some(),
                "has_pending": has_pending_after_voice,
            }),
        );
    }

    FinalizeOutcome::Finalized {
        removed_token: finish.removed_token,
        has_pending: has_pending_after_voice,
        mailbox_online: finish.mailbox_online,
    }
}

// #3041 §2-§3 — delivery-lease handlers: thin wrappers over the
// `DeliveryLeaseCell` state machine (mod.rs), run in the actor task. DORMANT
// after the R2 revert (the watcher works the cell INLINE); kept + unit-tested
// for the sink/bridge wiring (P1-2..).

/// CAS-acquire for `(key, [start,end))` on behalf of `holder`. #3041, dormant
/// in the non-test build (the watcher acquires the cell directly, B4).
#[allow(dead_code)] // #3041: AcquireDelivery actor arm dormant until sink/bridge wiring.
fn handle_acquire_delivery(
    lease: &DeliveryLeaseCell,
    key: TurnKey,
    holder: LeaseHolder,
    start: u64,
    end: u64,
    deadline_ms: u64,
) -> bool {
    lease.try_acquire(key, holder, start, end, deadline_ms)
}

/// Three-way commit; full `(holder, key, [start,end))` mismatch = no-op. #3041
/// P1-1: a successful `Delivered` commit advances the channel's
/// `confirmed_end_offset` watermark to `end` (§5.2), gated on the lease having
/// actually committed (so a rejected stale/duplicate commit never touches the
/// offset) and via `advance_watcher_confirmed_end`'s monotonic CAS (never a
/// double-advance). `NotDelivered`/`Unknown` never advance: an ambiguous
/// terminal must not claim bytes as delivered.
fn handle_commit_delivery(
    lease: &DeliveryLeaseCell,
    key: TurnKey,
    holder: LeaseHolder,
    start: u64,
    end: u64,
    outcome: LeaseOutcome,
    provider: &ProviderKind,
    tmux_session_name: &str,
    shared: &SharedData,
) -> bool {
    let committed = lease.commit(holder, key, start, end, outcome);
    // `mod tmux` is `#[cfg(unix)]`; non-unix commits the lease without an
    // advance and consumes the otherwise-unused unix-only params.
    #[cfg(unix)]
    if committed && outcome == LeaseOutcome::Delivered {
        super::tmux::advance_watcher_confirmed_end(
            shared,
            provider,
            key.channel_id,
            tmux_session_name,
            end,
            "src/services/discord/turn_finalizer.rs:commit_delivery_advance",
        );
    }
    #[cfg(not(unix))]
    let _ = (shared, provider, tmux_session_name);
    committed
}

/// Compare-and-release; full `(holder, key, [start,end))` match only. #3041.
#[allow(dead_code)] // #3041 P1-0: dormant, wired in P1-1..
fn handle_release_delivery(
    lease: &DeliveryLeaseCell,
    key: TurnKey,
    holder: LeaseHolder,
    start: u64,
    end: u64,
) -> bool {
    lease.release(holder, key, start, end)
}

/// #3016 S1 (PURE) — the structural completion signal derived solely from the
/// provider's on-disk JSONL transcript. Shared by the public
/// `completion_signal_state` (see it for the Done/PausedLive/Unknown
/// rationale) and the reconciler's watcher-backstop liveness re-check.
pub(in crate::services::discord) fn completion_signal_from_transcript(
    provider: &ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    transcript_path: &std::path::Path,
) -> CompletionSignal {
    if !crate::services::tui_turn_state::provider_runtime_has_structured_jsonl_turn_state(
        provider,
        runtime_kind,
    ) {
        return CompletionSignal::Unknown;
    }
    if crate::services::tui_turn_state::jsonl_turn_end_terminator_idle(provider, transcript_path) {
        CompletionSignal::Done
    } else {
        CompletionSignal::PausedLive
    }
}

/// #3016 phase-5a — the reconciler's terminal-or-defer verdict for a
/// watcher-owned `register_start` Pending. `at_deadline == true` is the
/// NATURAL 1800s far-backstop expiry; `false` (the #3277 fast-path probe AND
/// the re-check of a fast-path-PULLED deadline, codex r1) stays STRICTLY
/// transcript-proven. Never finalizes a legitimately long paused-live turn:
///   * NO LIVE handle — absent (also under the inflight `tmux_session_name`
///     re-key below: #3277 verify-1, a `claim_or_reuse_watcher` ReuseExisting
///     dispatch registers under the OWNER channel only), `cancel` set, or
///     `heartbeat_stale()` (#3268) → terminal ONLY at the natural deadline
///     (nothing is left to drive the pane). The strict mode DEFERS: a watcher
///     replace/reuse leaves the registry transiently absent/stale while the
///     transcript still says busy — absence proves nothing about the TURN;
///     dead/absent authority stays with the far horizon, never the fast path.
///   * live-but-`paused` (a Discord turn took the session over) → defer.
///   * else `watcher_backstop_signal_is_terminal` on the transcript: `Done`
///     terminal; `PausedLive` defers; `Unknown` (non-JSONL runtime) consults
///     the pane-ready fallback ONLY at the natural deadline.
fn watcher_backstop_turn_is_terminal(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    at_deadline: bool,
) -> bool {
    let inflight_tmux = super::inflight::load_inflight_state(provider, channel_id.get())
        .and_then(|state| state.tmux_session_name);
    let (tmux_session_name, output_path, paused) = {
        let handle = match inflight_tmux.as_deref() {
            Some(tmux) => shared.tmux_watchers.by_tmux_session.get(tmux),
            None => shared.tmux_watchers.get(&channel_id),
        };
        let Some(handle) = handle else {
            return at_deadline;
        };
        if handle.cancel.load(std::sync::atomic::Ordering::Relaxed) || handle.heartbeat_stale() {
            return at_deadline;
        }
        (
            handle.tmux_session_name.clone(),
            handle.output_path.clone(),
            handle.paused.load(std::sync::atomic::Ordering::Acquire),
        )
        // dashmap `Ref` dropped here, BEFORE the (blocking) pane capture below.
    };
    if paused {
        return false;
    }
    let runtime_kind =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux_session_name)
            .map(|binding| binding.runtime_kind)
            .or_else(|| {
                crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&tmux_session_name)
            });
    watcher_backstop_signal_is_terminal(
        completion_signal_from_transcript(
            provider,
            runtime_kind,
            std::path::Path::new(&output_path),
        ),
        at_deadline,
        || {
            crate::services::tui_turn_state::pane_ready_fallback_allowed(provider, runtime_kind)
                && crate::services::provider::tmux_session_ready_for_input(
                    &tmux_session_name,
                    provider,
                )
        },
    )
}

/// #3277 verify-3 — the verdict over the transcript completion signal. The
/// strict mode (`allow_pane_probe == false`: fast-path probe and pulled
/// re-check) treats `Unknown` (non-JSONL Gemini / OpenCode / Qwen / legacy
/// wrapper: no provable terminator) as NON-terminal: the synchronous
/// pane-capture fallback can misread a dialog or a long silent stretch as
/// idle, and probing it every 15s would amplify the old once-per-1800s
/// exposure ~120× (and block the actor task). Only the NATURAL at-deadline
/// re-check (`true`) consults `pane_ready` — lazily, only on `Unknown`.
fn watcher_backstop_signal_is_terminal(
    signal: CompletionSignal,
    allow_pane_probe: bool,
    pane_ready: impl FnOnce() -> bool,
) -> bool {
    match signal {
        CompletionSignal::PausedLive => false,
        CompletionSignal::Done => true,
        CompletionSignal::Unknown => allow_pane_probe && pane_ready(),
    }
}

/// Run the deadline-elapsed backstop finalize for ONE entry: flip
/// `Pending → Finalizing` (skip if a concurrent terminal already advanced it),
/// run `do_finalize` on the backstop context, then flip `Finalized`. Shared by
/// the gate-timeout deadline arm and the phase-5a watcher far-backstop arm so
/// exactly-once is decided in one place. (#3016 phase-5b2: the legacy
/// `mailbox_finalize_owed` revoke that used to run here is gone with the flag.)
async fn run_backstop_finalize(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    ledger_key: LedgerKey,
    turn_key: TurnKey,
    provider: ProviderKind,
    shared: &Arc<SharedData>,
    now: Instant,
) {
    match ledger.get_mut(&ledger_key) {
        Some(entry) if entry.phase == Phase::Pending => entry.phase = Phase::Finalizing,
        _ => return,
    }
    // Backstop finalize: the deferred terminal originated from the watcher but
    // no caller is around to clear inflight, so the backstop context clears it
    // here — and the row, still on disk, feeds the marker-ensure row fallback.
    let _ = do_finalize(
        turn_key,
        provider,
        &TerminalEvent::GateTimeout {
            pane_quiescent: Some(true),
        },
        FinalizeContext::gate_backstop(),
        shared,
    )
    .await;
    // #3016 phase-5b2: the legacy `mailbox_finalize_owed` revoke that ran here
    // is gone — the ledger's exactly-once phase gate is the sole arbiter, so
    // there is no stale flag a surviving watcher could swap.
    if let Some(entry) = ledger.get_mut(&ledger_key) {
        entry.phase = Phase::Finalized;
        entry.finalized_at = Some(now);
        entry.terminal_deadline = None;
        entry.watcher_backstop_deadline = None;
    }
}

/// The one reconciler. Finalizes deadline-armed gate-timeouts whose backstop
/// elapsed, GUARANTEES the phase-5a watcher far-backstop for watcher-owned
/// `register_start` Pending entries that never received a terminal (re-checking
/// liveness so a paused-live turn is deferred, never over-finalized), and
/// garbage-collects `Finalized` entries past their TTL so the ledger stays
/// bounded.
async fn reconcile(ledger: &mut HashMap<LedgerKey, LedgerEntry>, shared: &Arc<SharedData>) {
    let now = Instant::now();

    // #3041 P1-1 (B3): reclaim any delivery lease whose acquire deadline has
    // elapsed (a dead/stuck holder), so a legitimate successor can acquire. This
    // runs on the reconcile tick (1s) and is identity-agnostic; a `Committed`
    // lease is never reclaimed (it awaits an explicit holder release). Uses the
    // process-monotonic `lease_now_ms()` clock — the SAME clock the watcher's
    // acquire deadline is computed against — so a live holder mid-send (whose
    // ~15s deadline is kept ahead by the watcher's heartbeat-renew) is never
    // reclaimed.
    let _ = shared.reclaim_expired_delivery_leases(super::lease_now_ms());

    // Collect deadline-elapsed gate-timeout entries to finalize. We must not
    // hold a `&mut` borrow across the `do_finalize` await, so snapshot first.
    // The stored `turn_key` carries the identity `do_finalize` needs.
    let due: Vec<(LedgerKey, TurnKey, ProviderKind)> = ledger
        .iter()
        .filter_map(|(ledger_key, entry)| {
            if entry.phase == Phase::Pending
                && let Some(deadline) = entry.terminal_deadline
                && now >= deadline
            {
                Some((*ledger_key, entry.turn_key, entry.provider.clone()))
            } else {
                None
            }
        })
        .collect();

    for (ledger_key, turn_key, provider) in due {
        run_backstop_finalize(ledger, ledger_key, turn_key, provider, shared, now).await;
    }

    // #3277 (Defect C) — proven-terminal fast-path probe (no await). For
    // watcher-owned Pending entries whose far deadline is still distant, run
    // the STRICT (`at_deadline = false`) predicate: transcript-proven `Done`
    // under a LIVE unpaused handle ONLY — absent/cancelled/stale handles and
    // non-JSONL runtimes always defer here (codex r1, #3277 verify-3). After
    // WATCHER_BACKSTOP_TERMINAL_STREAK interval-spaced terminal probes, pull
    // the deadline in to GATE_BACKSTOP for the deadline arm's third (still
    // strict — the entry is flagged `pulled`) confirmation within seconds
    // instead of 1800s. Any non-terminal probe resets the streak.
    let probe_due: Vec<(LedgerKey, ChannelId, ProviderKind)> = ledger
        .iter()
        .filter_map(|(ledger_key, entry)| {
            let probe_spacing_elapsed = entry.watcher_backstop_probe_at.is_none_or(|at| {
                now.duration_since(at) >= WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL
            });
            if entry.phase == Phase::Pending
                && entry.relay_owner == RelayOwnerKind::Watcher
                && probe_spacing_elapsed
                && let Some(deadline) = entry.watcher_backstop_deadline
                && deadline > now + GATE_BACKSTOP
            {
                Some((
                    *ledger_key,
                    entry.turn_key.channel_id,
                    entry.provider.clone(),
                ))
            } else {
                None
            }
        })
        .collect();
    for (ledger_key, channel_id, provider) in probe_due {
        let terminal = watcher_backstop_turn_is_terminal(shared, channel_id, &provider, false);
        let Some(entry) = ledger.get_mut(&ledger_key) else {
            continue;
        };
        entry.watcher_backstop_probe_at = Some(now);
        if !terminal {
            entry.watcher_backstop_terminal_streak = 0;
            continue;
        }
        entry.watcher_backstop_terminal_streak =
            entry.watcher_backstop_terminal_streak.saturating_add(1);
        if entry.watcher_backstop_terminal_streak == WATCHER_BACKSTOP_TERMINAL_STREAK {
            entry.watcher_backstop_deadline = Some(now + GATE_BACKSTOP);
            entry.watcher_backstop_deadline_pulled = true;
            tracing::warn!(
                channel = channel_id.get(),
                provider = %provider.as_str(),
                streak = entry.watcher_backstop_terminal_streak,
                "#3277: watcher-owned turn is provably terminal but no terminal was ever \
                 submitted — pulling the far-backstop deadline in (finalize after a final \
                 at-deadline liveness re-check)"
            );
        }
    }

    // #3016 phase-5a — the watcher-owned `register_start` FAR backstop. Collect
    // watcher-owned Pending entries whose generous `watcher_backstop_deadline`
    // elapsed (those the watcher fresh-idle finalize never caught — the
    // under-finalize gap the `placeholder_sweeper` SKIPS once content was
    // delivered). Snapshot first so no `&mut` borrow is held across the awaits.
    let watcher_due: Vec<(LedgerKey, TurnKey, ProviderKind, bool)> = ledger
        .iter()
        .filter_map(|(ledger_key, entry)| {
            if entry.phase == Phase::Pending
                && entry.relay_owner == RelayOwnerKind::Watcher
                && let Some(deadline) = entry.watcher_backstop_deadline
                && now >= deadline
            {
                let pulled = entry.watcher_backstop_deadline_pulled;
                Some((*ledger_key, entry.turn_key, entry.provider.clone(), pulled))
            } else {
                None
            }
        })
        .collect();

    for (ledger_key, turn_key, provider, pulled) in watcher_due {
        // Liveness re-check: NEVER finalize a paused-live / still-busy turn at
        // the deadline; a still-live one EXTENDS its backstop a full horizon.
        // A fast-path-PULLED deadline stays STRICT (codex r1) so a transiently
        // absent/stale handle cannot smuggle a busy turn past the third check.
        if watcher_backstop_turn_is_terminal(shared, turn_key.channel_id, &provider, !pulled) {
            run_backstop_finalize(ledger, ledger_key, turn_key, provider, shared, now).await;
        } else if let Some(entry) = ledger.get_mut(&ledger_key) {
            if entry.phase == Phase::Pending {
                entry.watcher_backstop_deadline = Some(now + WATCHER_REGISTER_BACKSTOP);
                // #3277: re-prove from scratch on the restored generous horizon.
                entry.watcher_backstop_deadline_pulled = false;
                entry.watcher_backstop_terminal_streak = 0;
            }
        }
    }

    // GC finalized entries past their TTL.
    ledger.retain(|_, entry| {
        !(entry.phase == Phase::Finalized
            && entry
                .finalized_at
                .is_some_and(|t| now.duration_since(t) >= FINALIZED_TTL))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn key(ch: u64) -> TurnKey {
        TurnKey::new(ChannelId::new(ch), 1, 0)
    }

    /// Isolate a finalizer test from the process-global `AGENTDESK_ROOT_DIR`
    /// runtime root. The finalize path reads that env var (via
    /// `inflight::clear_inflight_state_if_matches` → `runtime_root()`) and can
    /// delete inflight files under whatever root is currently set, so these
    /// tests must serialize against the standby/watcher tests that also mutate
    /// the env. Acquire the SAME shared mutex the existing helpers use
    /// (`standby_relay::with_isolated_runtime_root`,
    /// `gateway::with_isolated_runtime_root`, `runtime_store::lock_test_env`,
    /// all of which resolve to `config::shared_test_env_lock()`) for the FULL
    /// duration of the test and point `AGENTDESK_ROOT_DIR` at a private temp
    /// dir, then clear it on exit so nothing leaks to a concurrent test.
    ///
    /// The guard is a `std::sync::MutexGuard` (not `Send`), held across the
    /// test's `.await` points. That is sound because every finalizer test runs
    /// on a `flavor = "current_thread"` runtime, so its future is never moved
    /// across threads; concurrent tests on other OS threads simply block on
    /// `lock()` until this test releases the guard, which is exactly the
    /// serialization we want.
    // SAFETY (await_holding_lock): the doc comment above explains why the
    // env-dir Mutex is intentionally held across the test awaits (current-thread
    // runtime, serialization is the whole point). Test-only.
    #[allow(clippy::await_holding_lock)]
    async fn with_isolated_runtime_root<F, Fut>(f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Codex P3 — SAVE the prior value and RESTORE it on exit (instead of
        // unconditionally removing) so running the suite with the variable
        // already set does not leak a cleared env to later tests.
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let tmp = tempfile::tempdir().expect("create temp runtime dir for turn finalizer test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                tmp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        f().await;
        unsafe {
            match prev {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }

    /// A `Complete` on a registered Pending turn finalizes exactly once and the
    /// late loser receives `AlreadyFinalized`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn exactly_once_complete_then_late_complete() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let fin = TurnFinalizer::spawn();
            let k = key(101);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(first, FinalizeOutcome::Finalized { .. }));

            let second = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(second, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// #3274 residual: a terminal loser that receives `AlreadyFinalized` must
    /// still perform identity-guarded active-state cleanup. This models the
    /// bridge/watcher migration gap where the ledger says "done" but the same
    /// turn's mailbox token and durable inflight row survived the winner path.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn already_finalized_loser_cleans_same_turn_mailbox_and_inflight() {
        use crate::services::discord::inflight::{InflightTurnState, save_inflight_state};
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(3274);
            let turn_id = 3274_1001u64;
            let key = TurnKey::new(ch, turn_id, 0);

            fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            let first = fin
                .submit_terminal(
                    key,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(first, FinalizeOutcome::Finalized { .. }));

            let token = Arc::new(CancelToken::new());
            shared.restart.global_active.store(1, Ordering::Relaxed);
            shared
                .mailbox(ch)
                .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(turn_id))
                .await;
            let inflight = InflightTurnState::new(
                ProviderKind::Claude,
                ch.get(),
                None,
                7,
                turn_id,
                turn_id + 1,
                "prompt".to_string(),
                None,
                Some("AgentDesk-claude-adk-cc-3274".to_string()),
                None,
                None,
                0,
            );
            save_inflight_state(&inflight).expect("persist same-turn inflight");

            let late = fin
                .submit_terminal(
                    key,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;

            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
            assert!(
                crate::services::discord::inflight::load_inflight_state(
                    &ProviderKind::Claude,
                    ch.get(),
                )
                .is_none(),
                "AlreadyFinalized loser must clear the matching stale inflight row"
            );
            assert!(
                shared.mailbox(ch).snapshot().await.cancel_token.is_none(),
                "AlreadyFinalized loser must release the matching stale mailbox token"
            );
            assert!(
                token.cancelled.load(Ordering::Relaxed),
                "removed token must be marked cancelled so watchdog observers stop"
            );
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "cleanup decrements the active counter exactly when it removes a token"
            );
        })
        .await;
    }

    /// #3274 residual safety: the `AlreadyFinalized` cleanup is keyed to the
    /// terminal's turn id. A stale turn-1 loser must not clear turn-2's active
    /// mailbox token or durable inflight row.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn already_finalized_loser_preserves_newer_turn_active_state() {
        use crate::services::discord::inflight::{InflightTurnState, save_inflight_state};
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(3275);
            let turn1_id = 3275_1001u64;
            let turn2_id = 3275_1002u64;
            let turn1 = TurnKey::new(ch, turn1_id, 0);

            fin.register_start(
                turn1,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                &shared,
            );
            let _ = fin
                .submit_terminal(
                    turn1,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;

            let turn2_token = Arc::new(CancelToken::new());
            shared.restart.global_active.store(1, Ordering::Relaxed);
            shared
                .mailbox(ch)
                .restore_active_turn(
                    turn2_token.clone(),
                    UserId::new(7),
                    MessageId::new(turn2_id),
                )
                .await;
            let turn2_inflight = InflightTurnState::new(
                ProviderKind::Claude,
                ch.get(),
                None,
                7,
                turn2_id,
                turn2_id + 1,
                "turn-2 prompt".to_string(),
                None,
                Some("AgentDesk-claude-adk-cc-3275".to_string()),
                None,
                None,
                0,
            );
            save_inflight_state(&turn2_inflight).expect("persist turn-2 inflight");

            let late = fin
                .submit_terminal(
                    turn1,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;

            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
            let snapshot = shared.mailbox(ch).snapshot().await;
            assert!(
                snapshot.cancel_token.is_some(),
                "stale AlreadyFinalized cleanup must not release turn-2's token"
            );
            assert_eq!(
                snapshot.active_user_message_id,
                Some(MessageId::new(turn2_id)),
                "turn-2 must remain the active mailbox owner"
            );
            assert!(
                !turn2_token.cancelled.load(Ordering::Relaxed),
                "turn-2 token must not be marked cancelled by turn-1 cleanup"
            );
            let persisted = crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                ch.get(),
            )
            .expect("turn-2 inflight must survive stale cleanup");
            assert_eq!(persisted.user_msg_id, turn2_id);
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                1,
                "stale cleanup must not decrement the newer live turn"
            );
        })
        .await;
    }

    /// A turn registered with the real `user_msg_id` and a terminal submitted
    /// with the channel-only `user_msg_id == 0` (recovery/orphan path) MUST
    /// resolve to the SAME ledger entry, so the channel-only terminal finalizes
    /// the registered turn and a later real-id terminal loses the exactly-once
    /// gate. Regression for the codex P2: keying on `user_msg_id` would split
    /// them and duplicate the finalize side-effects.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn unknown_user_msg_id_collapses_onto_registered_turn() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(606);
            let registered = TurnKey::new(ch, 99_999, 0);
            let channel_only = TurnKey::new(ch, 0, 0);
            fin.register_start(
                registered,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                &shared,
            );

            // Channel-only (id 0) terminal finalizes the registered turn.
            let first = fin
                .submit_terminal(
                    channel_only,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(first, FinalizeOutcome::Finalized { .. }));

            // Real-id terminal now hits the same entry → AlreadyFinalized.
            let second = fin
                .submit_terminal(
                    registered,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(second, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// Two SEQUENTIAL turns in the SAME channel within `FINALIZED_TTL` must be
    /// distinct ledger entries: finalizing turn-1 must NOT make turn-2's
    /// terminal return `AlreadyFinalized` (that would strand turn-2's mailbox
    /// token and re-introduce the stuck-channel bug). Regression for the codex
    /// P1 on the channel-only ledger key.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn sequential_same_channel_turns_each_finalize() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(909);
            let turn1 = TurnKey::new(ch, 1001, 0);
            let turn2 = TurnKey::new(ch, 1002, 0);

            fin.register_start(
                turn1,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                &shared,
            );
            let f1 = fin
                .submit_terminal(
                    turn1,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(f1, FinalizeOutcome::Finalized { .. }));

            // turn-2 starts immediately (well within the 60s Finalized TTL) and
            // must finalize on its own, not be swallowed by turn-1's entry.
            fin.register_start(
                turn2,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                &shared,
            );
            let f2 = fin
                .submit_terminal(
                    turn2,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(f2, FinalizeOutcome::Finalized { .. }),
                "turn-2 must finalize independently of turn-1"
            );
        })
        .await;
    }

    /// Cross-turn safety: a STALE channel-only (id-0) terminal arriving after
    /// turn-1 finalized and turn-2 registered must NOT finalize turn-2. The
    /// resolver routes it to the orphan key (because a Finalized entry exists),
    /// leaving turn-2 live so turn-2's own terminal still finalizes it.
    /// Regression for the codex P1 on channel-only resolution.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_channel_only_terminal_does_not_finalize_next_turn() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(919);
            let turn1 = TurnKey::new(ch, 2001, 0);
            let turn2 = TurnKey::new(ch, 2002, 0);
            let channel_only = TurnKey::new(ch, 0, 0);

            fin.register_start(
                turn1,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                &shared,
            );
            let _ = fin
                .submit_terminal(
                    turn1,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;

            // turn-2 registers (queued follow-up now live).
            fin.register_start(
                turn2,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                &shared,
            );

            // A STALE id-0 terminal (from turn-1's watcher) arrives. Because a
            // Finalized entry exists AND a different live entry (turn-2) exists,
            // the ambiguous-channel-only guard returns it as a no-op
            // (AlreadyFinalized) WITHOUT running channel-scoped cleanup — so it
            // cannot release turn-2's token.
            let stale = fin
                .submit_terminal(
                    channel_only,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(stale, FinalizeOutcome::AlreadyFinalized),
                "ambiguous stale id-0 terminal must be a no-op"
            );

            // turn-2 is still live: its own terminal finalizes it.
            let f2 = fin
                .submit_terminal(
                    turn2,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(f2, FinalizeOutcome::Finalized { .. }),
                "turn-2 must still be live after the stale id-0 terminal"
            );
        })
        .await;
    }

    /// The counter is decremented at most once even under a double terminal
    /// submission — `removed_token.is_some()` gates the decrement and the
    /// saturating helper can never underflow.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn no_underflow_on_double_terminal() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            // No active mailbox turn → mailbox_finish_turn returns removed_token=None.
            shared.restart.global_active.store(0, Ordering::Relaxed);
            let fin = TurnFinalizer::spawn();
            let k = key(202);

            let _ = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            let _ = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;

            // Never underflows below zero.
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// A gate-timeout with a busy pane and a live relay owner defers; once the
    /// backstop deadline elapses the reconciler finalizes exactly once.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_pane_busy_finalizes_after_backstop() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(303);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let deferred = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(false),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(deferred, FinalizeOutcome::Deferred));

            // Sleep past GATE_BACKSTOP. Under `start_paused` the runtime
            // auto-advances the clock once all tasks are idle on timers, which lets
            // the actor's own reconcile interval fire and finalize the deferred
            // entry. A couple of extra reconcile intervals guarantees the pass ran.
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            // A late terminal now sees Finalized → AlreadyFinalized, proving the
            // reconciler finalized the deferred entry.
            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// Repeated gate-timeouts (the watcher submits one per pass while the pane
    /// stays busy) must NOT push the backstop deadline forward — otherwise a
    /// persistently busy pane never finalizes. The deadline is armed once; even
    /// with re-submissions arriving every ~half-backstop, the original deadline
    /// elapses and the reconciler finalizes.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn repeated_gate_timeout_does_not_postpone_backstop() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(313);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            // Re-submit gate-timeouts at ~1/3 of the backstop apart, three times,
            // staying UNDER the single backstop window in total. Each is Deferred;
            // critically, the re-submissions must not reset the original deadline.
            let third = GATE_BACKSTOP / 3;
            for _ in 0..3 {
                let d = fin
                    .submit_terminal(
                        k,
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(d, FinalizeOutcome::Deferred));
                tokio::time::sleep(third).await;
            }
            // ~GATE_BACKSTOP has now elapsed since the FIRST (and only effective)
            // arming. If re-submissions had postponed it, the deadline would still
            // be ~third away. Sleep a hair more to cross the original deadline and
            // let the reconciler run.
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 2).await;
            tokio::task::yield_now().await;

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "the backstop must have finalized despite repeated gate-timeouts"
            );
        })
        .await;
    }

    /// A gate-timeout whose pane is already quiescent finalizes immediately
    /// (no deferral).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_pane_quiescent_finalizes_now() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(404);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(true),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
        })
        .await;
    }

    /// A cancel submission does not double-apply completion cleanup and yields
    /// the exactly-once gate (a late Complete loses).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_then_late_complete_already_finalized() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(505);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let cancelled = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Cancel,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(cancelled, FinalizeOutcome::Finalized { .. }));

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// Phase 3 race: the watcher (`FinalizeContext::watcher`) and the bridge
    /// (`FinalizeContext::bridge`) both submit `Complete` for the same turn.
    /// The single-task ledger gate yields exactly one `Finalized` and one
    /// `AlreadyFinalized` regardless of which arrives first — replacing the old
    /// `mailbox_finalize_owed` CAS handoff. The counter is decremented at most
    /// once (no active mailbox turn here, so it stays at 0 either way).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn bridge_watcher_race_finalizes_exactly_once() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(0, Ordering::Relaxed);
            let fin = TurnFinalizer::spawn();
            let k = key(707);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let watcher = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            let bridge = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;

            // Exactly one Finalized, exactly one AlreadyFinalized.
            let finalized = [&watcher, &bridge]
                .iter()
                .filter(|o| matches!(o, FinalizeOutcome::Finalized { .. }))
                .count();
            let already = [&watcher, &bridge]
                .iter()
                .filter(|o| matches!(o, FinalizeOutcome::AlreadyFinalized))
                .count();
            assert_eq!(finalized, 1, "exactly one submission performs the finalize");
            assert_eq!(already, 1, "the loser receives AlreadyFinalized");
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// Phase 3 gate-timeout then watcher-complete-before-deadline: a
    /// `GateTimeout{Some(false)}` defers, then the watcher submits `Complete`
    /// (pane quiesced) before the backstop elapses → that Complete finalizes
    /// once and the later reconciler tick finds the entry already Finalized, so
    /// no double finalize.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_then_complete_before_deadline_no_double() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(808);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let deferred = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(false),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(deferred, FinalizeOutcome::Deferred));

            // Pane quiesced: the watcher's next pass submits Complete well before
            // the backstop deadline.
            let complete = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(complete, FinalizeOutcome::Finalized { .. }));

            // Let the backstop elapse; the reconciler must NOT finalize again.
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// Restored/unregistered-watcher gate-timeout (#3016 P2 regression): a
    /// watcher restored after a restart submits `GateTimeout{Some(false)}`
    /// WITHOUT a prior `register_start` (the bridge handoff never ran), so the
    /// ledger entry is created on-demand with `relay_owner == None`. That path
    /// must finalize IMMEDIATELY (not Deferred — no owner will ever drive the
    /// pane to quiescence and the caller discards the outcome while the
    /// `!lifecycle_stage_paused` cleanup block is skipped), AND it must honor
    /// the pending-queue state so a queued follow-up actually kicks off:
    ///   * outcome is `Finalized` (the channel does NOT stay stuck), and
    ///   * `removed_token` is `Some` (the active turn's mailbox token is
    ///     released — inflight/mailbox not orphaned), and
    ///   * `has_pending` is `true` (the queued follow-up is surfaced so the
    ///     finalizer's `kickoff_queue` schedules its dispatch).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn restored_unregistered_watcher_gate_timeout_finalizes_and_kicks_off_queue() {
        use crate::services::turn_orchestrator::{Intervention, InterventionMode};
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(1101);

            // An active turn (live cancel token) so `mailbox_finish_turn` returns
            // `removed_token = Some` — the orphan/restored turn whose token must be
            // released by the immediate finalize.
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token.clone(), UserId::new(7), MessageId::new(70))
                .await;
            // A queued soft follow-up behind the active turn so `has_pending` is
            // true; the immediate-no-owner path must surface it (kickoff_queue).
            shared
                .mailbox(ch)
                .replace_queue(
                    vec![Intervention {
                        author_id: UserId::new(1),
                        author_is_bot: false,
                        message_id: MessageId::new(71),
                        source_message_ids: vec![MessageId::new(71)],
                        text: "queued follow-up".to_string(),
                        mode: InterventionMode::Soft,
                        created_at: std::time::Instant::now(),
                        reply_context: None,
                        has_reply_boundary: false,
                        merge_consecutive: false,
                        pending_uploads: Vec::new(),
                        voice_announcement: None,
                    }],
                    super::super::queue_persistence_context(&shared, &ProviderKind::Claude, ch),
                )
                .await;

            let fin = TurnFinalizer::spawn();
            // NOTE: no `register_start` — this is the restored/unregistered watcher.
            // The watcher submits with `FinalizeContext::watcher()` exactly as the
            // tmux_watcher gate-timeout call-site does, and discards the outcome.
            let outcome = fin
                .submit_terminal(
                    TurnKey::new(ch, 0, 0),
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(false),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;

            match outcome {
                FinalizeOutcome::Finalized {
                    removed_token,
                    has_pending,
                    ..
                } => {
                    assert!(
                        removed_token.is_some(),
                        "restored watcher gate-timeout must release the active mailbox token, \
                     not leave it orphaned"
                    );
                    assert!(
                        has_pending,
                        "the queued follow-up must be honored so the finalizer kicks off the queue"
                    );
                }
                other => panic!(
                    "restored/unregistered-watcher gate-timeout must finalize immediately \
                 (not stay stuck), got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Counter decremented exactly once (token was removed).
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// #3016 root-cause regression (the escalated [P1] wrong-turn race): a
    /// terminal carrying turn-1's real `user_msg_id` arriving AFTER turn-1
    /// finalized and turn-2 is the live active mailbox turn must NOT release
    /// turn-2's token or decrement `global_active`. The identity-guarded
    /// `mailbox_finish_turn_if_matches` no-ops because the mailbox's active
    /// `user_message_id` (turn-2) does not match the terminal's (turn-1). This
    /// is the channel-scoped-`mailbox_finish_turn` hazard the guard closes:
    /// before this fix, `do_finalize` would have channel-scoped-finished
    /// turn-2's token.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_real_id_terminal_does_not_release_newer_turn_token() {
        use crate::services::turn_orchestrator::{Intervention, InterventionMode};
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(1313);
            let turn1_id = 5001u64;
            let turn2_id = 5002u64;

            // turn-2 is the CURRENT live active turn in the mailbox (a fresh turn
            // that started after turn-1 finalized — its identity is turn2_id).
            let turn2_token = Arc::new(CancelToken::new());
            shared.restart.global_active.store(1, Ordering::Relaxed);
            shared
                .mailbox(ch)
                .restore_active_turn(
                    turn2_token.clone(),
                    UserId::new(7),
                    MessageId::new(turn2_id),
                )
                .await;
            // A queued soft follow-up sits behind turn-2. If the stale terminal
            // surfaced `has_pending`, the bridge could drain THIS message behind the
            // live turn — so the guard-miss path must report no backlog (Codex P2).
            shared
                .mailbox(ch)
                .replace_queue(
                    vec![Intervention {
                        author_id: UserId::new(1),
                        author_is_bot: false,
                        message_id: MessageId::new(5003),
                        source_message_ids: vec![MessageId::new(5003)],
                        text: "queued behind turn-2".to_string(),
                        mode: InterventionMode::Soft,
                        created_at: std::time::Instant::now(),
                        reply_context: None,
                        has_reply_boundary: false,
                        merge_consecutive: false,
                        pending_uploads: Vec::new(),
                        voice_announcement: None,
                    }],
                    super::super::queue_persistence_context(&shared, &ProviderKind::Claude, ch),
                )
                .await;

            // Channel-scoped routing state that belongs to the LIVE turn-2. A stale
            // turn-1 terminal must not clear any of it (Codex P2 — the trailing
            // side-effects must be skipped when the identity guard misses).
            //   * dispatch_thread_parents is cleaned via `retain(|_, v| *v != ch)`,
            //     so seed an entry whose VALUE is `ch` (a thread routing TO it).
            //   * dispatch_role_overrides is keyed BY `ch`.
            let thread_ch = ChannelId::new(1314);
            let override_ch = ChannelId::new(1315);
            shared.dispatch_thread_parents.insert(thread_ch, ch);
            shared.dispatch_role_overrides.insert(ch, override_ch);

            let fin = TurnFinalizer::spawn();
            // A STALE terminal for turn-1 (its real id) arrives. The finalizer
            // ledger has no entry for it (turn-1's entry was GC'd / never here), so
            // it creates a Pending entry and finalizes — but the identity-guarded
            // mailbox finish must refuse to touch turn-2's token.
            let outcome = fin
                .submit_terminal(
                    TurnKey::new(ch, turn1_id, 0),
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;

            match outcome {
                FinalizeOutcome::Finalized {
                    removed_token,
                    has_pending,
                    ..
                } => {
                    assert!(
                        removed_token.is_none(),
                        "stale turn-1 terminal must NOT release turn-2's mailbox token"
                    );
                    // Codex P2: a guarded miss must report NO backlog so the bridge
                    // does not drain a queued soft message behind the live turn-2.
                    assert!(
                        !has_pending,
                        "stale terminal must report no pending backlog (turn-2 owns its queue)"
                    );
                }
                other => panic!(
                    "stale terminal still flows through the ledger gate as Finalized, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }

            // turn-2's token is untouched and the global counter was NOT
            // decremented for a turn this terminal did not own.
            assert!(
                !turn2_token
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed),
                "turn-2 must not be cancelled by a stale turn-1 terminal"
            );
            assert!(
                shared.mailbox(ch).has_active_turn().await,
                "turn-2 must remain the live active turn"
            );
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                1,
                "global_active must not be decremented for the wrong turn"
            );

            // Codex P2: the live turn-2's channel-scoped routing state survives the
            // stale terminal — the guard-missed finalize must skip the trailing
            // side-effects, not corrupt the newer turn's routing/watchdog metadata.
            assert!(
                shared
                    .dispatch_thread_parents
                    .get(&thread_ch)
                    .is_some_and(|v| *v == ch),
                "stale terminal must NOT drop turn-2's dispatch_thread_parents entry"
            );
            assert!(
                shared
                    .dispatch_role_overrides
                    .get(&ch)
                    .is_some_and(|v| *v == override_ch),
                "stale terminal must NOT drop turn-2's dispatch_role_overrides entry"
            );

            // And turn-2 finalizes correctly on its OWN matching terminal.
            let f2 = fin
                .submit_terminal(
                    TurnKey::new(ch, turn2_id, 0),
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match f2 {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "turn-2's own terminal must release turn-2's token"
                    );
                }
                other => panic!(
                    "turn-2 must finalize on its own id, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "now turn-2 is finalized exactly once"
            );
        })
        .await;
    }

    /// #3016 Task 1 regression: the guarded inflight clear (the deadline-armed
    /// gate-timeout / restored-watcher backstop, which passes
    /// `clear_inflight = true`) must NOT delete a DIFFERENT (next) turn's
    /// inflight file. A backstop firing with turn-1's identity after turn-2 has
    /// written its inflight must preserve turn-2's inflight — exactly as the
    /// pre-#3016 identity-guarded placeholder sweeper did via
    /// `inflight_state_still_same_turn`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn guarded_inflight_clear_preserves_next_turn_inflight() {
        use crate::services::discord::inflight::{InflightTurnState, save_inflight_state};

        with_isolated_runtime_root(|| async move {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(1414);
        let turn1_id = 6001u64;
        let turn2_id = 6002u64;

        // turn-2 has already written its inflight for this channel (it took
        // over after turn-1 finalized).
        let next = InflightTurnState::new(
            ProviderKind::Claude,
            ch.get(),
            None,
            7,
            turn2_id,
            turn2_id,
            "turn-2 text".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        save_inflight_state(&next).expect("persist turn-2 inflight for test");

        let fin = TurnFinalizer::spawn();
        // A backstop-style finalize carrying turn-1's identity with
        // `clear_inflight = true` (gate_backstop semantics). It must NOT delete
        // turn-2's inflight because the on-disk id (turn-2) does not match.
        let _ = fin
            .submit_terminal(
                TurnKey::new(ch, turn1_id, 0),
                ProviderKind::Claude,
                TerminalEvent::GateTimeout {
                    pane_quiescent: Some(true),
                },
                FinalizeContext::gate_backstop(),
                shared.clone(),
            )
            .await;

        // turn-2's inflight survives the wrong-turn backstop clear.
        let surviving = crate::services::discord::inflight::load_inflight_state(
            &ProviderKind::Claude,
            ch.get(),
        );
        assert!(
            surviving.is_some_and(|s| s.user_msg_id == turn2_id),
            "guarded inflight clear must preserve turn-2's inflight when finalize carries turn-1's identity"
        );
        }).await;
    }

    /// #3016 Codex P1 regression: an id-0 watcher/recovery terminal that
    /// collapses onto a ledger entry registered with the REAL `user_msg_id`
    /// must finalize under THAT resolved identity (the guarded
    /// `finish_turn_if_matches` path), not the literal id-0 (unguarded
    /// channel-scoped finish). Here the mailbox's active turn was started with a
    /// DIFFERENT id than the registered turn — simulating a stale ledger entry
    /// while a newer turn is live — so the resolved-identity guarded finish must
    /// refuse to release the newer turn's token.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn id0_terminal_uses_resolved_identity_and_spares_newer_turn() {
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(1515);
        let registered_id = 7001u64; // the ledger entry's real identity
        let live_active_id = 7002u64; // the DIFFERENT turn live in the mailbox

        // The mailbox's active turn is a NEWER turn (live_active_id), distinct
        // from the ledger's registered identity.
        let live_token = Arc::new(CancelToken::new());
        shared.restart.global_active.store(1, Ordering::Relaxed);
        shared
            .mailbox(ch)
            .restore_active_turn(
                live_token.clone(),
                UserId::new(7),
                MessageId::new(live_active_id),
            )
            .await;

        let fin = TurnFinalizer::spawn();
        // Register the ledger entry under registered_id (a real id). A later
        // id-0 terminal collapses onto THIS entry.
        fin.register_start(
            TurnKey::new(ch, registered_id, 0),
            ProviderKind::Claude,
            RelayOwnerKind::Watcher,
            &shared,
        );

        // id-0 terminal → resolves to the registered entry → finalize_key uses
        // the registered real id → guarded finish, which does NOT match the
        // live newer turn (live_active_id) → token spared.
        let outcome = fin
            .submit_terminal(
                TurnKey::new(ch, 0, 0),
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                shared.clone(),
            )
            .await;

        match outcome {
            FinalizeOutcome::Finalized { removed_token, .. } => {
                assert!(
                    removed_token.is_none(),
                    "id-0 terminal resolved to a stale registered id must NOT release the newer live turn's token"
                );
            }
            other => panic!(
                "expected Finalized via the resolved-identity guarded path, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert!(
            !live_token
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed),
            "the newer live turn must not be cancelled"
        );
        assert!(
            shared.mailbox(ch).has_active_turn().await,
            "the newer live turn must remain active"
        );
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            1,
            "counter must not be decremented for the wrong turn"
        );
        }).await;
    }

    // ----------------------------------------------------------------------
    // #3016 step 1 — (actor × terminal-path) exactly-once finalize matrix.
    //
    // The tests below fill the cells the suite above left uncovered so phase 5
    // (legacy `mailbox_finalize_owed` flag removal) lands behind a complete
    // exactly-once guard. Each asserts the three per-cell invariants:
    //   (1) late/double terminal is `AlreadyFinalized` (no double finalize),
    //   (2) `global_active` never underflows,
    //   (3) the mailbox cancel token is released exactly once (no under- or
    //       over-finalize).
    // These SPECIFY current production behaviour (tests only; no prod change).
    // ----------------------------------------------------------------------

    /// Seed a live active mailbox turn so `mailbox_finish_turn` returns
    /// `removed_token = Some` and assert helpers can verify the release.
    /// Returns the token so the caller can check its `cancelled` flag.
    async fn seed_active_turn(
        shared: &Arc<SharedData>,
        ch: ChannelId,
        user_msg_id: u64,
    ) -> Arc<CancelToken> {
        use serenity::model::id::{MessageId, UserId};
        let token = Arc::new(CancelToken::new());
        shared
            .mailbox(ch)
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(user_msg_id))
            .await;
        token
    }

    /// RelayMiss × bridge: a relay-miss terminal on a registered turn finalizes
    /// exactly once, releases the active token, decrements the counter once, and
    /// a late RelayMiss loses the gate. Covers the RelayMiss observability path
    /// (F) of `do_finalize`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn relay_miss_bridge_finalizes_exactly_once() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2001);
            let tid = 8001u64;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "relay-miss finalize must release the active turn's token"
                    );
                }
                other => panic!(
                    "relay-miss must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "the released token must be marked cancelled"
            );
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "counter decremented exactly once"
            );

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "late relay-miss must lose the exactly-once gate"
            );
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "no underflow on the late relay-miss"
            );
        })
        .await;
    }

    /// RelayMiss × watcher then late Complete: the watcher-path relay-miss
    /// finalizes once; a later Complete (different actor) sees `AlreadyFinalized`
    /// and the counter never underflows.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn relay_miss_watcher_then_late_complete_already_finalized() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2002);
            let tid = 8002u64;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "the watcher relay-miss must release the active turn's token"
                    );
                }
                other => panic!(
                    "watcher relay-miss must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Invariant (3): the released token is cancelled and the active turn
            // is cleared off the channel.
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "the released token must be marked cancelled"
            );
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "the relay-miss finalize must clear the active turn"
            );

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// RelayMiss × orphan (id-0, no active mailbox turn): the channel-scoped
    /// finish returns `removed_token = None`, so the counter is left untouched
    /// (already 0) and never underflows even on a double submission.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn relay_miss_orphan_no_active_turn_no_underflow() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(0, Ordering::Relaxed);
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ChannelId::new(2003), 0, 0);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_none(),
                        "orphan relay-miss with no active turn removes no token"
                    );
                }
                other => panic!(
                    "orphan relay-miss still finalizes (idempotent no-op), got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Double submit on the same id-0 orphan. The first submit recorded a
            // `Finalized` ledger entry, so the second MUST lose the exactly-once
            // gate. Binding (not discarding) this outcome guards invariant (1):
            // if id-0/no-token ever regressed to a second `Finalized` no-op the
            // counter would still read 0 and the discarded-outcome version would
            // pass blind — this assert catches that regression.
            let second = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(second, FinalizeOutcome::AlreadyFinalized),
                "the second orphan relay-miss must lose the exactly-once gate, \
                 not re-finalize as a no-op"
            );
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "orphan relay-miss must never underflow the counter"
            );
        })
        .await;
    }

    /// Cancel × watcher: a watcher-path cancel finalizes once, releases the
    /// active token (sets `cancelled`) WITHOUT marking completion-cleanup (the
    /// watcher context passes `allow_completion_cleanup = false` and cancel is
    /// gated out anyway), decrements the counter once, and a late Complete loses.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_watcher_finalizes_and_releases_token() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2004);
            let tid = 8004u64;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let cancelled = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Cancel,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            match cancelled {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "watcher cancel must release the active token"
                    );
                }
                other => panic!(
                    "watcher cancel must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "cancel must set the token's cancelled flag"
            );
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "no underflow after the late complete on a cancelled turn"
            );
        })
        .await;
    }

    /// Reconciler-backstop × deferred gate-timeout (the REAL prod backstop cell):
    /// a `GateTimeout{Some(false)}` with a live relay owner defers (arming the
    /// backstop deadline); once the deadline elapses the reconciler drives the
    /// `gate_backstop()` finalize — the ONLY way `gate_backstop()` is reached in
    /// prod (`reconcile()` always submits `GateTimeout{Some(true)}` through it).
    /// That backstop finalize releases the seeded active turn's token exactly
    /// once, clears the active turn, and decrements the counter once; a late
    /// terminal then loses the gate without underflow.
    ///
    /// (Replaces the former `cancel_gate_backstop_finalizes_exactly_once`, which
    /// injected `TerminalEvent::Cancel` through `gate_backstop()` — an impossible
    /// cell that never occurs in prod and overstated matrix coverage.)
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn reconciler_backstop_finalizes_deferred_gate_timeout_exactly_once() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2005);
            let tid = 8005u64;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            // A live relay owner is what makes `GateTimeout{Some(false)}` defer
            // (arming the backstop) rather than finalize immediately.
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let deferred = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(false),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(deferred, FinalizeOutcome::Deferred),
                "a busy-pane gate-timeout with a live owner must defer to the backstop"
            );
            // The token is still live while the entry is only deferred.
            assert!(
                !token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "the deferred turn's token must not be released before the backstop fires"
            );
            assert!(
                shared.mailbox(ch).has_active_turn().await,
                "the active turn must persist while the gate-timeout is only deferred"
            );

            // Sleep past GATE_BACKSTOP. Under `start_paused` the runtime
            // auto-advances the clock once tasks idle on timers, letting the
            // actor's reconcile interval fire and drive the `gate_backstop()`
            // finalize. A couple extra intervals guarantees the pass ran.
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            // The reconciler-backstop finalize released the seeded active turn's
            // token exactly once.
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "the reconciler backstop must release (cancel) the active turn's token"
            );
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "the reconciler backstop must clear the active turn"
            );
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "the reconciler backstop decrements the counter exactly once"
            );

            // A late terminal now sees Finalized → AlreadyFinalized, proving the
            // backstop finalized the deferred entry and the gate holds.
            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "a late terminal after the backstop must lose the exactly-once gate"
            );
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "no underflow on the late terminal after the backstop finalize"
            );
        })
        .await;
    }

    /// GateTimeout{None} × watcher: `pane_quiescent == None` is NOT the
    /// `Some(false)` deferral trigger, so it finalizes IMMEDIATELY (like
    /// `Some(true)`) even with a live relay owner. Releases the token once and a
    /// late Complete loses.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_pane_quiescent_none_watcher_finalizes_now() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2006);
            let tid = 8006u64;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            // Register with a live owner so we PROVE None does not defer the way
            // Some(false) would with an owner present.
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: None,
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            match outcome {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "GateTimeout{{None}} must finalize now and release the token"
                    );
                }
                other => panic!(
                    "GateTimeout{{None}} must finalize immediately (not Deferred), got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert!(token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// GateTimeout{None} × bridge: same immediate-finalize semantics through the
    /// bridge context (no deferral), exactly once.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_pane_quiescent_none_bridge_finalizes_now() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2007);
            let tid = 8007u64;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: None,
                    },
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match outcome {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "GateTimeout{{None}} via bridge must finalize now and release the token"
                    );
                }
                other => panic!(
                    "GateTimeout{{None}} via bridge must finalize immediately (not Deferred), \
                     got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Invariant (3): the released token is cancelled and the active turn
            // is cleared off the channel.
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "GateTimeout{{None}} via bridge must mark the released token cancelled"
            );
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "GateTimeout{{None}} via bridge must clear the active turn"
            );
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

            // Late probe: a follow-up terminal on the now-finalized turn must
            // lose the exactly-once gate without underflowing the counter.
            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "a late terminal after GateTimeout{{None}} must be AlreadyFinalized"
            );
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "no underflow on the late terminal after GateTimeout{{None}}"
            );
        })
        .await;
    }

    /// Genuine orphan recovery (id-0, NO registered ledger entry) with a live
    /// active mailbox turn: with no `Finalized` entry and no live ledger entry
    /// for the channel, the channel-only resolver collapses onto the literal
    /// id-0 key, the entry is created on-demand and finalizes, and the unguarded
    /// channel-scoped finish releases the active turn's token exactly once. A
    /// second submit is `AlreadyFinalized` and the counter never underflows.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn orphan_id0_recovery_finalizes_and_releases_token() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2008);
            let tid = 8008u64;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            // NO register_start — pure orphan/recovery path keyed only by channel.
            let k = TurnKey::new(ch, 0, 0);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "genuine orphan finalize must release the active turn's token via the \
                         unguarded channel-scoped finish"
                    );
                }
                other => panic!(
                    "genuine orphan terminal must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert!(token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "the orphan finalize must clear the active turn"
            );
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

            // A second id-0 terminal now finds the Finalized entry → no-op.
            let second = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(second, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "no underflow on the second orphan terminal"
            );
        })
        .await;
    }

    /// Watcher→bridge handoff double-terminal: the watcher submits Complete
    /// (finalizing the turn) and the bridge then submits its own Complete for the
    /// SAME turn (the post-handoff straggler). Exactly one Finalized, the loser
    /// is AlreadyFinalized, the token releases once, and the counter decrements
    /// exactly once. This is the sequential handoff complement to the concurrent
    /// `bridge_watcher_race_finalizes_exactly_once`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_then_bridge_handoff_double_terminal_exactly_once() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2009);
            let tid = 8009u64;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let watcher = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            let bridge = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;

            assert!(
                matches!(watcher, FinalizeOutcome::Finalized { .. }),
                "the first (watcher) submission performs the finalize"
            );
            assert!(
                matches!(bridge, FinalizeOutcome::AlreadyFinalized),
                "the post-handoff bridge straggler loses the gate"
            );
            assert!(token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "the counter decremented exactly once across the handoff"
            );
        })
        .await;
    }

    /// Cancel × Cancel double terminal: two cancels for the same turn (e.g. a
    /// reaction and a `/!stop` racing) finalize exactly once and never underflow
    /// the counter. The second cancel is `AlreadyFinalized`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn double_cancel_finalizes_exactly_once_no_underflow() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2010);
            let tid = 8010u64;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Cancel,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "the first cancel must release the active turn's token"
                    );
                }
                other => panic!(
                    "the first cancel must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Invariant (3): cancel sets the token's cancelled flag and clears
            // the active turn off the channel.
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "cancel must set the released token's cancelled flag"
            );
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "the first cancel must clear the active turn"
            );

            let second = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Cancel,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(second, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "double cancel must decrement exactly once, never underflow"
            );
        })
        .await;
    }

    /// #3016 (codex B1): documents WHY an id-0 watcher finalize is UNSAFE in the
    /// stale-newer-turn case — and therefore why the call site
    /// (`tmux_watcher.rs`) must SKIP `finish_restored_watcher_active_turn`
    /// entirely rather than submit `Complete` with `user_msg_id == 0`.
    ///
    /// Scenario reproduced at the resolver level: the channel ledger has NO
    /// terminal(`Finalized`) entry and a SINGLE live (non-finalized) NEWER turn.
    /// A 0-id `TurnKey` (what `pinned_finalize_user_msg_id` returns for a stale
    /// completion) collapses onto that single live entry. If the watcher
    /// submitted `Complete` against this resolved key, the finalizer would
    /// finalize the NEWER still-running turn and release its cancel_token /
    /// ledger entry — a wrong-turn finalize. The guard in the watcher is to not
    /// finalize at all when `completion_is_stale_for_newer_turn`; this test
    /// pins the resolver behavior the guard exists to avoid.
    #[test]
    fn stale_completion_skips_finalize_no_id0_collapse() {
        let ch = ChannelId::new(4242);
        let generation = 0u64;
        // The single LIVE (non-finalized) entry belongs to the NEWER turn
        // (user_msg_id 999). No terminal/finalized entry exists for the channel.
        let newer_live = LedgerKey {
            channel_id: ch,
            generation,
            user_msg_id: 999,
        };
        let candidates = [(&newer_live, /* is_terminal */ false)];

        // A 0-id key (stale watcher completion id) collapses onto the newer
        // live entry — proving an id-0 `Complete` here WOULD finalize the wrong
        // (newer, still-running) turn. This is exactly why the call site skips.
        let zero_key = TurnKey::new(ch, 0, generation);
        let resolved = resolve_channel_only(zero_key, candidates.iter().copied());
        assert_eq!(
            resolved, newer_live,
            "id-0 collapse onto the single live newer entry is the wrong-turn \
             finalize hazard the watcher skip closes (codex B1)"
        );

        // Sanity complement: the same resolver routes a REAL id to its own exact
        // key, never collapsing — so the hazard is unique to the id-0 path the
        // stale-skip guard removes.
        let real_key = TurnKey::new(ch, 777, generation);
        let resolved_real = resolve_channel_only(real_key, candidates.iter().copied());
        assert_eq!(
            resolved_real,
            real_key.exact_key(),
            "a real user_msg_id never collapses onto a different live entry"
        );

        // And the finalized-guard branch: once a terminal entry exists for the
        // channel/generation, even a 0-id key refuses to collapse (routes to the
        // literal orphan key) — the cross-turn safety net. Included so the test
        // documents the full id-0 resolution matrix the guard reasons about.
        let finalized_old = LedgerKey {
            channel_id: ch,
            generation,
            user_msg_id: 100,
        };
        let guarded = [
            (&finalized_old, /* is_terminal */ true),
            (&newer_live, /* is_terminal */ false),
        ];
        let resolved_guarded = resolve_channel_only(zero_key, guarded.iter().copied());
        assert_eq!(
            resolved_guarded,
            zero_key.exact_key(),
            "with a terminal entry present, id-0 routes to the orphan no-op key, \
             not the newer live entry"
        );
    }

    // =======================================================================
    // #3016 S1 — read-only completion-signal + watcher-pending probes.
    // Additive, dead until S3/S4; these prove the read-only contract now.
    // =======================================================================

    use crate::services::agent_protocol::RuntimeHandoffKind;

    fn write_transcript(lines: &[&str]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), lines.join("\n")).unwrap();
        file
    }

    // (a) Claude transcript ending in a real terminator → Done.
    #[test]
    fn completion_signal_claude_terminator_is_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"result","result":"done","session_id":"s"}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::Done,
        );
    }

    // (b) Claude transcript still streaming (no terminator) → PausedLive.
    #[test]
    fn completion_signal_claude_streaming_is_paused_live() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
        );
    }

    // (b) Claude transcript whose latest line is a partial selector fragment
    // after a terminator (a just-restarted turn) → PausedLive (the strict scan
    // refuses to fall through a partial new envelope).
    #[test]
    fn completion_signal_claude_partial_after_terminator_is_paused_live() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"result","result":"done","session_id":"s"}"#,
            r#"{"ty"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
        );
    }

    // (a) Codex transcript ending in `turn.completed` → Done.
    #[test]
    fn completion_signal_codex_terminator_is_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":3}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
            ),
            CompletionSignal::Done,
        );
    }

    // (b) Codex transcript mid-tool-call (no terminator) → PausedLive.
    #[test]
    fn completion_signal_codex_inflight_is_paused_live() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"run_cmd","arguments":"{}","call_id":"c1"}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
        );
    }

    // #3016 S3 (Concern 1): a COMPLETED Codex `agent_message` written right
    // before a tool call is MID-TURN — the turn has not ended. The lenient drain
    // probe would call this Idle, but the finalize `Done` decision uses the
    // turn-END-only probe, so it must resolve to PausedLive (NOT Done) and the
    // watcher therefore CANNOT over-finalize the live turn.
    #[test]
    fn completion_signal_codex_completed_agent_message_is_paused_live_not_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"on it, running a tool next"}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
            "a completed agent_message with no turn.completed is mid-turn → not Done",
        );
    }

    // #3016 S3 (Concern 1): a Codex `event_msg{task_complete}` (a task signal,
    // not the turn record) is likewise NOT the turn terminator → PausedLive.
    #[test]
    fn completion_signal_codex_task_complete_is_paused_live_not_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/repo"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Codex,
                Some(RuntimeHandoffKind::CodexTui),
                file.path(),
            ),
            CompletionSignal::PausedLive,
        );
    }

    // #3016 S3 (Concern 1): a Claude mid-turn assistant message (no terminator)
    // → PausedLive; and a Claude `system{init}` (session-start, not turn-end) is
    // at-rest to the drain probe but must NOT be Done for the finalize decision.
    #[test]
    fn completion_signal_claude_init_and_mid_turn_are_paused_live_not_done() {
        let fin = TurnFinalizer::spawn();
        let mid_turn = write_transcript(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"thinking"}]}}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                mid_turn.path(),
            ),
            CompletionSignal::PausedLive,
        );

        let init_only =
            write_transcript(&[r#"{"type":"system","subtype":"init","session_id":"s"}"#]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                init_only.path(),
            ),
            CompletionSignal::PausedLive,
            "system{{init}} is a session-start marker, not a turn-end terminator → not Done",
        );
    }

    // #3016 S3 (Concern 1): a Claude `system{turn_duration}` IS a real turn-end
    // terminator → Done (the stricter probe still accepts the genuine
    // system-family turn boundary, not only `result`).
    #[test]
    fn completion_signal_claude_turn_duration_is_done() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"system","subtype":"turn_duration","session_id":"s"}"#,
        ]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::Done,
        );
    }

    // (c) Non-JSONL runtime (LegacyTmuxWrapper) → Unknown even with a terminator
    // on disk: the probe must not speak to completion for a runtime that has no
    // structured on-disk turn state.
    #[test]
    fn completion_signal_non_jsonl_runtime_is_unknown() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[r#"{"type":"result","result":"done","session_id":"s"}"#]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::LegacyTmuxWrapper),
                file.path(),
            ),
            CompletionSignal::Unknown,
        );
        // ProcessBackend and ClaudeEAdapter are also non-JSONL.
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ProcessBackend),
                file.path(),
            ),
            CompletionSignal::Unknown,
        );
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeEAdapter),
                file.path(),
            ),
            CompletionSignal::Unknown,
        );
    }

    // (c) Non-JSONL PROVIDER (Qwen) → Unknown regardless of runtime kind.
    #[test]
    fn completion_signal_non_jsonl_provider_is_unknown() {
        let fin = TurnFinalizer::spawn();
        let file = write_transcript(&[r#"{"type":"result","result":"done","session_id":"s"}"#]);
        assert_eq!(
            fin.completion_signal_state(
                &ProviderKind::Qwen,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
            ),
            CompletionSignal::Unknown,
        );
    }

    // has_live_watcher_pending: true for a watcher-owned non-finalized entry.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_pending_true_for_live_watcher_entry() {
        // #3016 phase-5a: `register_start` now takes `&Arc<SharedData>` (to prime
        // the reconcile cache). This test only registers + probes the ledger (no
        // terminal, no reconcile), so an inert shared suffices.
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let fin = TurnFinalizer::spawn();
        let ch = ChannelId::new(7001);
        fin.register_start(
            TurnKey::new(ch, 1, 5),
            ProviderKind::Claude,
            RelayOwnerKind::Watcher,
            &shared,
        );
        // Let the actor drain the Start before querying.
        tokio::task::yield_now().await;
        assert!(fin.has_live_watcher_pending(ch, 5).await);
        // Different generation → no match.
        assert!(!fin.has_live_watcher_pending(ch, 6).await);
        // Different channel → no match.
        assert!(!fin.has_live_watcher_pending(ChannelId::new(7002), 5).await);
    }

    // has_live_watcher_pending: false for a bridge-owned entry (relay owner is
    // not the watcher).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_pending_false_for_bridge_owned_entry() {
        // #3016 phase-5a: inert shared for the `register_start` Arc param (this
        // test only registers + probes; no terminal, no reconcile).
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let fin = TurnFinalizer::spawn();
        let ch = ChannelId::new(7101);
        // `RelayOwnerKind::SessionBoundRelay` is a bridge-owned (non-watcher)
        // owner — the probe must not report it as a live watcher-pending turn.
        fin.register_start(
            TurnKey::new(ch, 1, 0),
            ProviderKind::Claude,
            RelayOwnerKind::SessionBoundRelay,
            &shared,
        );
        tokio::task::yield_now().await;
        assert!(!fin.has_live_watcher_pending(ch, 0).await);
    }

    // has_live_watcher_pending: false once the watcher entry is finalized.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_pending_false_after_finalized() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(7201);
            let k = TurnKey::new(ch, 42, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            tokio::task::yield_now().await;
            assert!(fin.has_live_watcher_pending(ch, 0).await);

            let _ = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;

            // The entry is now `Finalized` → no longer watcher-pending.
            assert!(!fin.has_live_watcher_pending(ch, 0).await);
        })
        .await;
    }

    // =======================================================================
    // #3041 P1-1 — LIVE watcher-terminal-delivery lease wiring tests.
    //
    // NOTE on the commit path: the watcher commits + advances the offset INLINE
    // (see `watcher_inline_*` tests below + tmux_watcher.rs) — the awaited
    // `CommitDelivery`/`ReleaseDelivery` actor round-trip was reverted to dormant
    // because the actor-commit deferral reopened the #3143 duplicate window. The
    // tests immediately below still drive the RETAINED-for-a-later-phase actor
    // `commit_delivery`/`release_delivery` methods to keep that machinery proven
    // correct (lease COMMIT advances `confirmed_end_offset`, B2 single-holder
    // contention, commit idempotency on the monotonic CAS, deadline reclaim,
    // release). The `watcher_inline_*` tests assert the NEW production inline
    // path (synchronous commit+advance, acquire-time self-reclaim). All run on a
    // gated clock (`current_thread`/`start_paused`), mirroring the 26-test
    // finalizer matrix style.
    // =======================================================================
    mod delivery_lease_p1_1 {
        use super::super::{LeaseHolder, LeaseOutcome, TurnFinalizer, TurnKey};
        // `make_shared_data_for_tests_with_storage` lives in the discord module
        // (mod.rs), three module hops up from here (p1_1 → tests → turn_finalizer
        // → discord). `with_isolated_runtime_root` is in the parent `tests` mod.
        use super::super::super::make_shared_data_for_tests_with_storage;
        use super::with_isolated_runtime_root;
        use crate::services::discord::DeliveryLeaseCell;
        use crate::services::provider::ProviderKind;
        use serenity::model::id::ChannelId;
        use std::sync::Arc;

        fn watcher(id: u64) -> LeaseHolder {
            LeaseHolder::Watcher { instance_id: id }
        }

        /// Watcher/Delivered: a freshly-acquired lease committed `Delivered`
        /// advances `confirmed_end_offset` to the leased `end` EXACTLY ONCE, and
        /// no duplicate occurs.
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn watcher_delivered_advances_offset_once() {
            with_isolated_runtime_root(|| async move {
                let shared = make_shared_data_for_tests_with_storage(None, None);
                let fin = TurnFinalizer::spawn();
                let ch = ChannelId::new(7001);
                let lease = shared.delivery_lease(ch);
                let turn = TurnKey::new(ch, 11, 0);
                let h = watcher(1);

                // Acquire on the cell (the watcher fast-path), then commit through
                // the actor (the path the watcher uses).
                assert!(lease.try_acquire(turn, h, 0, 64, 1_000));
                let committed = fin
                    .commit_delivery(
                        turn,
                        lease.clone(),
                        h,
                        0,
                        64,
                        LeaseOutcome::Delivered,
                        ProviderKind::Claude,
                        "p1-1-delivered-session".to_string(),
                        shared.clone(),
                    )
                    .await;
                assert!(committed, "fresh lease must commit");
                assert_eq!(
                    shared.committed_relay_offset(ch),
                    64,
                    "Delivered commit advances confirmed_end_offset to the leased end"
                );
            })
            .await;
        }

        /// Watcher acquire-contention (B2): two watcher instances race to acquire
        /// the SAME turn/range on one channel; exactly one acquires (and would
        /// send), the other is rejected and must skip its duplicate send.
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn watcher_acquire_contention_admits_one_holder() {
            with_isolated_runtime_root(|| async move {
                let shared = make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(7002);
                let lease = shared.delivery_lease(ch);
                let turn = TurnKey::new(ch, 22, 0);

                let w1 = watcher(1);
                let w2 = watcher(2);
                // First watcher acquires for [0,32).
                assert!(lease.try_acquire(turn, w1, 0, 32, 5_000));
                // Replacement watcher's acquire for the SAME turn/range loses
                // while w1 still holds it (B2: it must NOT re-acquire+re-emit).
                assert!(
                    !lease.try_acquire(turn, w2, 0, 32, 5_000),
                    "B2: a second watcher cannot acquire the live lease"
                );
                // No offset advanced yet (nothing committed).
                assert_eq!(shared.committed_relay_offset(ch), 0);
            })
            .await;
        }

        /// Watcher/Unknown: a commit with `Unknown` outcome (ambiguous terminal —
        /// e.g. lifecycle-paused TUI gate) does NOT advance the offset.
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn watcher_unknown_commit_does_not_advance_offset() {
            with_isolated_runtime_root(|| async move {
                let shared = make_shared_data_for_tests_with_storage(None, None);
                let fin = TurnFinalizer::spawn();
                let ch = ChannelId::new(7003);
                let lease = shared.delivery_lease(ch);
                let turn = TurnKey::new(ch, 33, 0);
                let h = watcher(1);

                assert!(lease.try_acquire(turn, h, 0, 48, 1_000));
                let committed = fin
                    .commit_delivery(
                        turn,
                        lease.clone(),
                        h,
                        0,
                        48,
                        LeaseOutcome::Unknown,
                        ProviderKind::Claude,
                        "p1-1-unknown-session".to_string(),
                        shared.clone(),
                    )
                    .await;
                assert!(committed, "Unknown still commits the lease state");
                assert_eq!(
                    shared.committed_relay_offset(ch),
                    0,
                    "Unknown outcome must NOT advance the confirmed offset"
                );
            })
            .await;
        }

        /// Watcher/Delivered then a SECOND commit of the same range is idempotent
        /// on the offset (monotonic CAS): the second commit is a lease no-op (the
        /// cell is Committed, not Leased) and the offset does not double-advance.
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn watcher_second_commit_is_idempotent_on_offset() {
            with_isolated_runtime_root(|| async move {
                let shared = make_shared_data_for_tests_with_storage(None, None);
                let fin = TurnFinalizer::spawn();
                let ch = ChannelId::new(7004);
                let lease = shared.delivery_lease(ch);
                let turn = TurnKey::new(ch, 44, 0);
                let h = watcher(1);

                assert!(lease.try_acquire(turn, h, 0, 80, 1_000));
                assert!(
                    fin.commit_delivery(
                        turn,
                        lease.clone(),
                        h,
                        0,
                        80,
                        LeaseOutcome::Delivered,
                        ProviderKind::Claude,
                        "p1-1-idem-session".to_string(),
                        shared.clone(),
                    )
                    .await
                );
                assert_eq!(shared.committed_relay_offset(ch), 80);

                // A second commit of the same range: the lease is now Committed,
                // so `commit` is a no-op (returns false) and the handler does NOT
                // advance. Even if it did, the monotonic CAS would refuse to move
                // the watermark backward or re-advance it.
                let second = fin
                    .commit_delivery(
                        turn,
                        lease.clone(),
                        h,
                        0,
                        80,
                        LeaseOutcome::Delivered,
                        ProviderKind::Claude,
                        "p1-1-idem-session".to_string(),
                        shared.clone(),
                    )
                    .await;
                assert!(!second, "second commit on a Committed lease is a no-op");
                assert_eq!(
                    shared.committed_relay_offset(ch),
                    80,
                    "offset must not double-advance on a repeated commit"
                );
            })
            .await;
        }

        /// Deadline reclaim of a dead holder: a leased-but-never-committed cell
        /// past its deadline is reclaimed by `reclaim_expired_delivery_leases`,
        /// returns to Unleased, and a later legitimate acquire succeeds.
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn deadline_reclaim_frees_cell_for_later_acquire() {
            with_isolated_runtime_root(|| async move {
                let shared = make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(7005);
                let lease = shared.delivery_lease(ch);
                let turn_a = TurnKey::new(ch, 55, 0);
                let dead = watcher(1);

                // A holder acquires with deadline 100ms (monotonic units) but never
                // commits/releases (dead).
                assert!(lease.try_acquire(turn_a, dead, 0, 16, 100));
                // Before the deadline, the sweep is a no-op and the cell stays held.
                assert_eq!(shared.reclaim_expired_delivery_leases(50), 0);
                assert!(!lease.try_acquire(turn_a, watcher(2), 0, 16, 100));
                // Past the deadline, the sweep reclaims exactly this cell.
                assert_eq!(shared.reclaim_expired_delivery_leases(100), 1);
                // A later legitimate acquire (new instance, new turn) succeeds.
                let turn_b = TurnKey::new(ch, 66, 0);
                assert!(
                    lease.try_acquire(turn_b, watcher(3), 16, 32, 1_000),
                    "a reclaimed cell is acquirable again"
                );
            })
            .await;
        }

        /// Release after commit returns the cell to Unleased so the NEXT turn can
        /// acquire — the lifecycle the watcher drives (acquire→commit→release).
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn release_after_commit_frees_cell_for_next_turn() {
            with_isolated_runtime_root(|| async move {
                let shared = make_shared_data_for_tests_with_storage(None, None);
                let fin = TurnFinalizer::spawn();
                let ch = ChannelId::new(7006);
                let lease = shared.delivery_lease(ch);
                let turn1 = TurnKey::new(ch, 77, 0);
                let h = watcher(1);

                assert!(lease.try_acquire(turn1, h, 0, 24, 1_000));
                assert!(
                    fin.commit_delivery(
                        turn1,
                        lease.clone(),
                        h,
                        0,
                        24,
                        LeaseOutcome::Delivered,
                        ProviderKind::Claude,
                        "p1-1-release-session".to_string(),
                        shared.clone(),
                    )
                    .await
                );
                assert!(
                    fin.release_delivery(turn1, lease.clone(), h, 0, 24).await,
                    "the holder releases its committed lease"
                );
                // Next turn (different range) can now acquire the freed cell.
                let turn2 = TurnKey::new(ch, 88, 0);
                assert!(
                    lease.try_acquire(turn2, watcher(2), 24, 48, 1_000),
                    "released cell is free for the next turn"
                );
            })
            .await;
        }

        /// Issue 1 (HIGH) — acquire-time SELF-RECLAIM of a dead holder, the REAL
        /// black-hole path: a holder `try_acquire`s and then "dies" (never
        /// commits/releases) on a cold path where NO finalizer `Terminal` message
        /// ever cached `SharedData`. Without acquire-time self-reclaim a
        /// replacement watcher would B2-skip the stuck `Leased` lease forever
        /// (permanent black-hole). This asserts the REAL fix: a replacement
        /// reclaims the EXPIRED lease at acquire time and SUCCEEDS — WITHOUT any
        /// finalizer actor, `SharedData`, or reconcile tick involved. It also
        /// asserts a NON-expired live lease still B2-skips (single-holder, §5.2).
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn watcher_inline_acquire_reclaims_dead_holder_without_terminal() {
            with_isolated_runtime_root(|| async move {
                let shared = make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(7007);
                let lease = shared.delivery_lease(ch);
                let dead_turn = TurnKey::new(ch, 101, 0);
                let dead = watcher(1);

                // Dead holder acquires with deadline 100 (monotonic units) and
                // then dies — never commits, never releases. No `SharedData` was
                // ever cached in any finalizer (we never spawn/route a Terminal).
                assert!(lease.try_acquire(dead_turn, dead, 0, 40, 100));

                // BEFORE the deadline: a NON-expired live lease still B2-skips —
                // the replacement's acquire-time reclaim is a no-op and the
                // acquire loses (single-holder invariant intact, no duplicate).
                let live = watcher(2);
                assert!(
                    !lease.reclaim_if_expired(50),
                    "a non-expired lease must NOT be reclaimed (would reintroduce duplicates)"
                );
                assert!(
                    !lease.try_acquire(dead_turn, live, 0, 40, 100),
                    "B2: a replacement cannot acquire while the holder's lease is live (non-expired)"
                );

                // AFTER the deadline: the replacement's acquire-time
                // `reclaim_if_expired` frees the dead holder's EXPIRED lease, then
                // its `try_acquire` SUCCEEDS — the range is delivered, NOT
                // black-holed. This is the exact in-watcher self-heal sequence
                // (reclaim_if_expired immediately before try_acquire), with NO
                // finalizer/SharedData/reconcile dependency.
                let replacement = watcher(3);
                let now_after_deadline = 150_u64;
                let reclaimed = lease.reclaim_if_expired(now_after_deadline);
                assert!(
                    reclaimed,
                    "acquire-time reclaim must free the dead holder's EXPIRED lease"
                );
                assert!(
                    lease.try_acquire(
                        dead_turn,
                        replacement,
                        0,
                        40,
                        now_after_deadline.saturating_add(1_000),
                    ),
                    "the replacement acquires the reclaimed cell and delivers (no black-hole)"
                );
            })
            .await;
        }

        /// Issue 2 (HIGH) — the inline commit advances `confirmed_end_offset`
        /// SYNCHRONOUSLY by the time control returns to the caller (no
        /// actor-deferral window). This replicates the EXACT in-watcher inline
        /// sequence (`cell.commit(Delivered)` then `advance_watcher_confirmed_end`)
        /// and asserts the offset is already advanced with NO `.await` on any
        /// actor in between — closing the #3143 duplicate window the deferred
        /// actor-commit had reopened.
        // `advance_watcher_confirmed_end` lives in the `#[cfg(unix)] mod tmux`;
        // this test drives it directly, so it is unix-only.
        #[cfg(unix)]
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn watcher_inline_commit_advances_offset_synchronously() {
            with_isolated_runtime_root(|| async move {
                let shared = make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(7008);
                let lease = shared.delivery_lease(ch);
                let turn = TurnKey::new(ch, 202, 0);
                let h = watcher(1);

                assert!(lease.try_acquire(turn, h, 0, 96, 1_000));
                assert_eq!(
                    shared.committed_relay_offset(ch),
                    0,
                    "no advance before commit"
                );

                // The INLINE production sequence: synchronous cell commit, then
                // (on Delivered) the synchronous offset advance — NO actor await.
                let committed = lease.commit(h, turn, 0, 96, LeaseOutcome::Delivered);
                assert!(committed, "fresh lease commits");
                super::super::super::tmux::advance_watcher_confirmed_end(
                    &shared,
                    &ProviderKind::Claude,
                    ch,
                    "p1-1-inline-session",
                    96,
                    "test:watcher_inline_commit_advances_offset_synchronously",
                );

                // By the time control returns here — with no actor round-trip in
                // between — the offset is ALREADY advanced. There is no window in
                // which `committed_relay_offset` still reads the old value.
                assert_eq!(
                    shared.committed_relay_offset(ch),
                    96,
                    "inline commit+advance moves confirmed_end_offset synchronously \
                     (no actor-deferral duplicate window)"
                );

                // Inline same-holder release returns the cell to Unleased.
                assert!(
                    lease.release(h, turn, 0, 96),
                    "inline release frees the committed cell for the next turn"
                );
            })
            .await;
        }

        fn _assert_send<T: Send>(_: &T) {}

        /// The shared lease cell is `Send + Sync` (it is shared across watcher
        /// instances via `Arc` and passed into the actor task).
        #[test]
        fn lease_cell_is_send_sync() {
            let c: Arc<DeliveryLeaseCell> = Arc::new(DeliveryLeaseCell::new(ChannelId::new(9)));
            _assert_send(&c);
        }
    }

    // =======================================================================
    // #3041 §2-§3 §6 P1-0 — Dormant `DeliveryLeaseCell` state-machine tests.
    //
    // The cell is wired into no call path yet (P1-1..), but its transitions
    // are proven correct now: single-winner CAS acquire, three-way commit,
    // compare-and-release no-op on holder mismatch, and deadline reclaim. The
    // tests drive the cell directly (and through the dormant handler wrappers)
    // because that is the logic later phases depend on.
    // =======================================================================
    mod delivery_lease {
        use super::super::{
            TurnKey, handle_acquire_delivery, handle_commit_delivery, handle_release_delivery,
        };
        use crate::services::discord::{
            DeliveryLeaseCell, LeaseHolder, LeaseOutcome, LeaseSnapshot,
        };
        use serenity::model::id::ChannelId;
        use std::sync::Arc;

        fn cell() -> DeliveryLeaseCell {
            DeliveryLeaseCell::new(ChannelId::new(42))
        }

        fn turn() -> TurnKey {
            TurnKey::new(ChannelId::new(42), 7, 0)
        }

        #[test]
        fn fresh_cell_is_unleased() {
            let c = cell();
            assert!(matches!(c.read(), LeaseSnapshot::Unleased));
            assert_eq!(c.channel_id(), ChannelId::new(42));
        }

        #[test]
        fn acquire_records_holder_range_and_deadline() {
            let c = cell();
            let h = LeaseHolder::Watcher { instance_id: 1 };
            assert!(c.try_acquire(turn(), h, 10, 20, 1_000));
            match c.read() {
                LeaseSnapshot::Leased {
                    holder,
                    turn,
                    deadline_ms,
                    start,
                    end,
                } => {
                    assert_eq!(holder, h);
                    assert_eq!(turn.exact_key(), self::turn().exact_key());
                    assert_eq!(deadline_ms, 1_000);
                    assert_eq!((start, end), (10, 20));
                }
                other => panic!("expected Leased, got {other:?}"),
            }
        }

        #[test]
        fn acquire_cas_admits_a_single_winner() {
            // Two distinct holders race to acquire the SAME fresh cell; exactly
            // one wins the CAS and the loser is rejected without mutating state.
            let c = cell();
            let w1 = LeaseHolder::Watcher { instance_id: 1 };
            let w2 = LeaseHolder::Watcher { instance_id: 2 };
            assert!(c.try_acquire(turn(), w1, 0, 5, 1_000));
            // Second acquire on an already-Leased cell loses.
            assert!(!c.try_acquire(turn(), w2, 0, 5, 1_000));
            // The winner's payload is intact (loser did not overwrite it).
            match c.read() {
                LeaseSnapshot::Leased { holder, .. } => assert_eq!(holder, w1),
                other => panic!("expected Leased held by winner, got {other:?}"),
            }
        }

        #[test]
        fn concurrent_acquire_has_exactly_one_winner() {
            // Stronger single-winner proof: spawn N threads contending on one
            // shared cell; exactly one try_acquire returns true.
            use std::sync::atomic::{AtomicUsize, Ordering};
            let c = Arc::new(cell());
            let wins = Arc::new(AtomicUsize::new(0));
            let barrier = Arc::new(std::sync::Barrier::new(16));
            let mut handles = Vec::new();
            for i in 0..16u64 {
                let c = Arc::clone(&c);
                let wins = Arc::clone(&wins);
                let barrier = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    if c.try_acquire(turn(), LeaseHolder::Watcher { instance_id: i }, 0, 1, 9_999) {
                        wins.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(wins.load(Ordering::Relaxed), 1, "CAS must admit one winner");
            assert!(matches!(c.read(), LeaseSnapshot::Leased { .. }));
        }

        #[test]
        fn commit_three_way_delivered_not_delivered_unknown() {
            for outcome in [
                LeaseOutcome::Delivered,
                LeaseOutcome::NotDelivered,
                LeaseOutcome::Unknown,
            ] {
                let c = cell();
                let h = LeaseHolder::Sink;
                assert!(c.try_acquire(turn(), h, 3, 9, 1_000));
                assert!(
                    c.commit(h, turn(), 3, 9, outcome),
                    "holder may commit {outcome:?}"
                );
                match c.read() {
                    LeaseSnapshot::Committed {
                        holder,
                        start,
                        end,
                        outcome: got,
                        ..
                    } => {
                        assert_eq!(holder, h);
                        assert_eq!((start, end), (3, 9));
                        assert_eq!(got, outcome);
                    }
                    other => panic!("expected Committed({outcome:?}), got {other:?}"),
                }
            }
        }

        #[test]
        fn commit_by_non_holder_is_noop() {
            let c = cell();
            let owner = LeaseHolder::Watcher { instance_id: 1 };
            let other = LeaseHolder::Watcher { instance_id: 2 };
            assert!(c.try_acquire(turn(), owner, 0, 4, 1_000));
            // Holder mismatch: commit refused, state stays Leased.
            assert!(!c.commit(other, turn(), 0, 4, LeaseOutcome::Delivered));
            assert!(matches!(c.read(), LeaseSnapshot::Leased { .. }));
        }

        #[test]
        fn commit_on_unleased_is_noop() {
            let c = cell();
            assert!(!c.commit(LeaseHolder::Bridge, turn(), 0, 1, LeaseOutcome::Delivered));
            assert!(matches!(c.read(), LeaseSnapshot::Unleased));
        }

        #[test]
        fn release_compare_and_release_noop_on_holder_mismatch() {
            let c = cell();
            let owner = LeaseHolder::Bridge;
            let stale = LeaseHolder::Watcher { instance_id: 99 };
            assert!(c.try_acquire(turn(), owner, 0, 8, 1_000));
            // A stale actor cannot release the live lease.
            assert!(!c.release(stale, turn(), 0, 8));
            assert!(matches!(c.read(), LeaseSnapshot::Leased { .. }));
            // The true holder releases successfully → back to Unleased.
            assert!(c.release(owner, turn(), 0, 8));
            assert!(matches!(c.read(), LeaseSnapshot::Unleased));
        }

        #[test]
        fn release_after_commit_returns_to_unleased() {
            let c = cell();
            let h = LeaseHolder::Sink;
            assert!(c.try_acquire(turn(), h, 0, 2, 1_000));
            assert!(c.commit(h, turn(), 0, 2, LeaseOutcome::Delivered));
            // Release is valid from Committed for the recorded holder.
            assert!(c.release(h, turn(), 0, 2));
            assert!(matches!(c.read(), LeaseSnapshot::Unleased));
            // Idempotent: a second release on the now-Unleased cell is a no-op.
            assert!(!c.release(h, turn(), 0, 2));
        }

        #[test]
        fn stale_turn_commit_and_release_are_noops_after_reacquire() {
            // #3041 §2 hazard, closed: turn A is acquired then reclaimed; turn B
            // reacquires the SAME channel with the SAME holder KIND. A stale
            // commit OR release carrying turn A's key must be a NO-OP and must
            // NOT touch turn B's live lease. (Holder kind alone would match —
            // only the stored turn identity distinguishes the two.)
            let c = cell();
            let holder = LeaseHolder::Sink; // same holder kind across both turns
            let turn_a = TurnKey::new(ChannelId::new(42), 100, 0);
            let turn_b = TurnKey::new(ChannelId::new(42), 200, 0);

            // Turn A acquires, then its deadline elapses and it is reclaimed.
            assert!(c.try_acquire(turn_a, holder, 0, 5, 10));
            assert!(c.reclaim_if_expired(10));
            assert!(matches!(c.read(), LeaseSnapshot::Unleased));

            // Turn B reacquires the freed cell (same channel, same holder kind).
            assert!(c.try_acquire(turn_b, holder, 5, 11, 1_000));

            // Stale commit from turn A: identity mismatch → no-op, B untouched.
            assert!(!c.commit(holder, turn_a, 5, 11, LeaseOutcome::Delivered));
            assert!(!c.commit(holder, turn_a, 0, 5, LeaseOutcome::Delivered));
            // Stale release from turn A: identity mismatch → no-op, B untouched.
            assert!(!c.release(holder, turn_a, 0, 5));
            match c.read() {
                LeaseSnapshot::Leased {
                    turn, start, end, ..
                } => {
                    assert_eq!(turn.exact_key(), turn_b.exact_key(), "B still holds");
                    assert_eq!((start, end), (5, 11));
                }
                other => panic!("turn B lease must survive stale A ops, got {other:?}"),
            }

            // Turn B's own commit/release with its real key still work.
            assert!(c.commit(holder, turn_b, 5, 11, LeaseOutcome::Delivered));
            assert!(!c.release(holder, turn_a, 5, 11)); // stale release post-commit: no-op
            assert!(c.release(holder, turn_b, 5, 11));
            assert!(matches!(c.read(), LeaseSnapshot::Unleased));
        }

        #[test]
        fn same_turn_stale_range_release_is_noop_after_reacquire() {
            // #3041 codex R2: the SAME turn is reclaimed and reacquires a
            // DIFFERENT byte range (e.g. a continuation chunk). A stale release
            // carrying the OLD range — same holder AND same turn — must be a
            // NO-OP and must NOT release the live newer-range lease. Only the
            // correct range releases it (release is now range-scoped, symmetric
            // with commit).
            let c = cell();
            let holder = LeaseHolder::Sink;
            let t = TurnKey::new(ChannelId::new(7), 300, 0);

            // Acquire range [0,5), let the deadline elapse, reclaim.
            assert!(c.try_acquire(t, holder, 0, 5, 10));
            assert!(c.reclaim_if_expired(10));
            // Same turn reacquires a continuation range [5, 12).
            assert!(c.try_acquire(t, holder, 5, 12, 1_000));

            // Stale release with the OLD range [0,5): holder+turn match but the
            // range does not → NO-OP, live [5,12) lease survives.
            assert!(!c.release(holder, t, 0, 5));
            match c.read() {
                LeaseSnapshot::Leased { start, end, .. } => assert_eq!((start, end), (5, 12)),
                other => {
                    panic!("newer-range lease must survive stale-range release, got {other:?}")
                }
            }
            // The correct range releases it.
            assert!(c.release(holder, t, 5, 12));
            assert!(matches!(c.read(), LeaseSnapshot::Unleased));
        }

        #[test]
        fn read_observes_payload_coherent_with_tag_under_race() {
            // #3041 codex coherence fix: a reader that observes a non-`Unleased`
            // state must observe the MATCHING payload — never a `Leased` tag
            // paired with an `Unleased`/empty payload. Because `try_acquire`
            // flips the tag AND writes the payload under one mutex (and `read`
            // also locks), this holds by construction. Hammer it: while one
            // thread repeatedly acquires/reclaims, readers must only ever see
            // `Unleased` or a fully-populated `Leased{turn,range}` — never a
            // torn intermediate.
            use std::sync::atomic::{AtomicBool, Ordering};
            let c = Arc::new(cell());
            let stop = Arc::new(AtomicBool::new(false));
            let t = turn();

            let writer = {
                let c = Arc::clone(&c);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if c.try_acquire(t, LeaseHolder::Sink, 7, 13, 1) {
                            // Immediately reclaim (deadline already in the past)
                            // so the cell churns Unleased↔Leased rapidly.
                            let _ = c.reclaim_if_expired(u64::MAX);
                        }
                    }
                })
            };

            for _ in 0..200_000 {
                match c.read() {
                    LeaseSnapshot::Unleased => {}
                    LeaseSnapshot::Leased {
                        turn, start, end, ..
                    } => {
                        // The payload paired with the Leased state is always the
                        // exact one the writer published — never torn/empty.
                        assert_eq!(turn.exact_key(), t.exact_key());
                        assert_eq!((start, end), (7, 13));
                    }
                    LeaseSnapshot::Committed { .. } => {
                        panic!("writer never commits; tag/payload incoherent")
                    }
                }
            }
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
        }

        #[test]
        fn deadline_reclaim_forces_unleased_when_expired() {
            let c = cell();
            let h = LeaseHolder::Watcher { instance_id: 1 };
            assert!(c.try_acquire(turn(), h, 0, 3, 100));
            // Not yet expired: no reclaim.
            assert!(!c.reclaim_if_expired(50));
            assert!(matches!(c.read(), LeaseSnapshot::Leased { .. }));
            // At/after the deadline: reclaimed regardless of holder identity.
            assert!(c.reclaim_if_expired(100));
            assert!(matches!(c.read(), LeaseSnapshot::Unleased));
            // After a reclaim a fresh acquire can win again.
            assert!(c.try_acquire(turn(), h, 0, 3, 200));
        }

        #[test]
        fn deadline_reclaim_never_touches_committed() {
            let c = cell();
            let h = LeaseHolder::Bridge;
            assert!(c.try_acquire(turn(), h, 0, 3, 10));
            assert!(c.commit(h, turn(), 0, 3, LeaseOutcome::Delivered));
            // A Committed lease awaits an explicit release; deadline reclaim is a
            // no-op even far past the (now meaningless) deadline.
            assert!(!c.reclaim_if_expired(10_000));
            assert!(matches!(c.read(), LeaseSnapshot::Committed { .. }));
        }

        #[test]
        fn dormant_handlers_drive_the_same_transitions() {
            // The actor-task handler wrappers must produce identical results to
            // the direct cell methods (they are wired in P1-1.. and exercised
            // through these wrappers).
            let c = cell();
            let h = LeaseHolder::Watcher { instance_id: 3 };
            assert!(handle_acquire_delivery(&c, turn(), h, 0, 6, 1_000));
            assert!(!handle_acquire_delivery(
                &c,
                turn(),
                LeaseHolder::Sink,
                0,
                6,
                1_000
            ));
            // #3041 P1-1: the commit handler now takes provider/session/shared so
            // a `Delivered` commit can advance the channel watermark. Supply a
            // throwaway `SharedData`; the advance targets the cell's channel (42).
            let shared = super::super::super::make_shared_data_for_tests_with_storage(None, None);
            assert!(handle_commit_delivery(
                &c,
                turn(),
                h,
                0,
                6,
                LeaseOutcome::Delivered,
                &crate::services::provider::ProviderKind::Claude,
                "dormant-handler-test-session",
                &shared,
            ));
            assert!(!handle_release_delivery(
                &c,
                turn(),
                LeaseHolder::Watcher { instance_id: 4 },
                0,
                6
            ));
            assert!(handle_release_delivery(&c, turn(), h, 0, 6));
            assert!(matches!(c.read(), LeaseSnapshot::Unleased));
        }
    }

    // ===================================================================
    // #3016 phase-5a — reconciler watcher-owned `register_start` far-backstop.
    //
    // Actor × terminal-path matrix (#3140 guard discipline):
    //   * watcher-owned, content delivered but UNTERMINATED → finalized by the
    //     backstop after the deadline, no flag needed;
    //   * paused-live past the deadline → NOT finalized (no over-finalize);
    //   * JSONL Done terminal → finalizes PROMPTLY even with the backstop armed;
    //   * exactly-once → no double-finalize when both a terminal and the
    //     backstop window apply.
    // ===================================================================

    /// A live, non-stale watcher handle so the reconciler liveness re-check
    /// resolves a real `tmux_session_name` + transcript `output_path` for the
    /// channel. Mirrors `tmux_watcher::tests::test_watcher_handle`.
    fn backstop_watcher_handle(
        tmux_session_name: &str,
        output_path: &str,
    ) -> crate::services::discord::TmuxWatcherHandle {
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                crate::services::discord::tmux_watcher_now_ms(),
            )),
        }
    }

    /// The reconciler runs off the actor's cached `Weak<SharedData>`, which the
    /// production finalizer populates from its continuous stream of `Terminal`
    /// submissions (a `Start` carries no `SharedData`). A test exercising the
    /// far-backstop on a turn that never submits its OWN terminal must prime that
    /// cache the same way — one unrelated orphan terminal (no mailbox token, so a
    /// harmless no-op beyond setting the cache).
    async fn prime_reconcile_shared(fin: &TurnFinalizer, shared: &Arc<SharedData>) {
        let _ = fin
            .submit_terminal(
                TurnKey::new(ChannelId::new(9_999_999), 0, 0),
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                shared.clone(),
            )
            .await;
    }

    /// A watcher-owned `register_start` Pending whose content was delivered but
    /// whose mailbox was NEVER finalized (no terminal ever submitted — the
    /// under-finalize gap the `placeholder_sweeper` SKIPS once content is
    /// delivered) is finalized by the reconciler far-backstop after the deadline,
    /// WITHOUT any `mailbox_finalize_owed` flag. The active mailbox token is
    /// released exactly once (counter decremented). No live watcher handle exists
    /// for the channel, so the liveness re-check treats the turn as terminal
    /// (nothing left will ever drive it).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_register_start_backstop_finalizes_unterminated_turn() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(5101);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(70))
                .await;

            let fin = TurnFinalizer::spawn();
            prime_reconcile_shared(&fin, &shared).await;
            // Watcher handoff registered the turn; NO terminal is ever submitted
            // for it (the stuck row).
            let k = TurnKey::new(ch, 70, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            tokio::time::sleep(WATCHER_REGISTER_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "the far-backstop must finalize the stuck watcher turn and release its token"
            );
            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            // codex r1: the absent-handle turn now waits for the NATURAL
            // deadline (the fast path defers), so the late terminal usually
            // hits the still-present Finalized row; if FINALIZED_TTL GC won
            // the race it takes the idempotent orphan path. Either way the
            // exactly-once TOKEN contract holds: no token to double-release.
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized)
                    || matches!(
                        late,
                        FinalizeOutcome::Finalized {
                            removed_token: None,
                            ..
                        }
                    ),
                "a late terminal must not double-release after the backstop finalized"
            );
        })
        .await;
    }

    /// #3016 phase-5a regression (codex HIGH): a FRESH actor — `cached_shared`
    /// starts `None`, NO prior terminal of ANY turn was ever processed — whose
    /// VERY FIRST watcher-owned `register_start` gets stuck (its own terminal is
    /// never submitted) must STILL be finalized by the far-backstop at the
    /// deadline. The reconcile cache is now primed by the `Start` itself, so the
    /// tick fires WITHOUT any unrelated terminal priming `cached_shared` first.
    ///
    /// This is the same scenario as
    /// `watcher_register_start_backstop_finalizes_unterminated_turn` but it
    /// deliberately does NOT call `prime_reconcile_shared`: before the fix the
    /// reconcile tick short-circuited on `cached_shared == None` and the token
    /// stayed stuck forever, so this test FAILS on the pre-fix code and PASSES
    /// once `Start` primes the cache.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn fresh_actor_watcher_backstop_finalizes_without_prior_terminal() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(5111);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(71))
                .await;

            // FRESH actor: nothing ever submitted a terminal, so `cached_shared`
            // is `None` until the `register_start` below primes it. NO
            // `prime_reconcile_shared` — that is the whole point of this test.
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, 71, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            tokio::time::sleep(WATCHER_REGISTER_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "a fresh actor's first stuck watcher turn must be finalized by the \
                 far-backstop even though no prior terminal ever primed cached_shared"
            );
            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            // codex r1: see watcher_register_start_backstop_finalizes_unterminated_turn
            // — the absent-handle turn finalizes at the NATURAL deadline; the
            // token contract is what must hold either way.
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized)
                    || matches!(
                        late,
                        FinalizeOutcome::Finalized {
                            removed_token: None,
                            ..
                        }
                    ),
                "a late terminal must not double-release after the backstop finalized"
            );
        })
        .await;
    }

    /// A watcher-owned turn that is genuinely PAUSED-LIVE at the deadline (a
    /// live watcher whose JSONL transcript shows NO turn terminator: selector /
    /// permission prompt / subagent / long tool call) must NOT be finalized by
    /// the far-backstop — over-finalizing it would strand a follow-up and kill a
    /// legitimately long turn. The liveness re-check returns `PausedLive`, so the
    /// backstop defers (re-arms) and the token stays held.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_register_start_backstop_defers_paused_live_turn() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(5202);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(80))
                .await;

            // A live watcher whose transcript has content but NO Claude turn
            // terminator → `completion_signal == PausedLive`.
            let session = format!("3016-phase5a-paused-{}", std::process::id());
            let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
            std::fs::write(
                &transcript,
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"working…\"}]}}\n",
            )
            .unwrap();
            shared.tmux_watchers.insert(
                ch,
                backstop_watcher_handle(&session, transcript.to_str().unwrap()),
            );

            let fin = TurnFinalizer::spawn();
            prime_reconcile_shared(&fin, &shared).await;
            let k = TurnKey::new(ch, 80, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            tokio::time::sleep(WATCHER_REGISTER_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            // NOT finalized: token still held, counter unchanged.
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                1,
                "a paused-live turn must NOT be finalized by the far-backstop"
            );
            // The entry is still Pending → an explicit terminal now finalizes it
            // (returns Finalized, not AlreadyFinalized), proving the backstop did
            // not consume it.
            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(outcome, FinalizeOutcome::Finalized { .. }),
                "the deferred paused-live turn must still be live for its real terminal"
            );
            let _ = std::fs::remove_file(&transcript);
        })
        .await;
    }

    /// #3268 far-backstop dead-watcher gap: a watcher handle that is STILL
    /// registered but DEAD (cancelled, or heartbeat-stale because the task
    /// hung/died — the heartbeat sweeper only flips `cancel` and leaves the
    /// entry indexed) over a still-BUSY (`PausedLive`) transcript must be
    /// treated as terminal by `watcher_backstop_turn_is_terminal`, exactly like
    /// an ABSENT handle — otherwise the busy transcript defers the turn forever
    /// and the mailbox/inflight is never released. A GENUINELY-LIVE handle
    /// (present, not cancelled, fresh heartbeat) over the SAME busy transcript
    /// must STILL defer (return false): the live-watcher semantics are
    /// untouched. codex r1: dead/absent-handle authority is the NATURAL
    /// deadline's ONLY (`at_deadline == true`) — the STRICT mode (fast-path
    /// probe / pulled re-check, `false`) must DEFER all three shapes.
    #[test]
    fn dead_watcher_handle_is_terminal_while_live_handle_defers_busy_transcript() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);

        // A busy transcript: content present but NO Claude turn terminator →
        // `completion_signal_from_transcript` returns `PausedLive`.
        let session = format!("3268-dead-watcher-{}", std::process::id());
        let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
        std::fs::write(
            &transcript,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"working…\"}]}}\n",
        )
        .unwrap();
        let transcript_str = transcript.to_str().unwrap().to_string();

        // 1) GENUINELY-LIVE handle (not cancelled, fresh heartbeat) → the busy
        //    `PausedLive` transcript defers: NOT terminal.
        let ch_live = ChannelId::new(5404);
        shared
            .tmux_watchers
            .insert(ch_live, backstop_watcher_handle(&session, &transcript_str));
        assert!(
            !watcher_backstop_turn_is_terminal(&shared, ch_live, &ProviderKind::Claude, true),
            "a live (present, not cancelled, fresh-heartbeat) watcher over a busy \
             transcript must DEFER — live-watcher PausedLive semantics are unchanged"
        );

        // 2) PRESENT but CANCELLED handle (sweeper flipped `cancel`, left the
        //    entry registered) → terminal, like handle-absence, regardless of the
        //    busy transcript.
        let ch_cancelled = ChannelId::new(5405);
        let cancelled = backstop_watcher_handle(&session, &transcript_str);
        cancelled
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared.tmux_watchers.insert(ch_cancelled, cancelled);
        assert!(
            watcher_backstop_turn_is_terminal(&shared, ch_cancelled, &ProviderKind::Claude, true),
            "a present-but-cancelled watcher has no authority to drive the pane to \
             quiescence — the far-backstop must finalize (terminal), not defer forever"
        );
        assert!(
            !watcher_backstop_turn_is_terminal(&shared, ch_cancelled, &ProviderKind::Claude, false),
            "codex r1: the STRICT mode must never count a cancelled handle as terminal"
        );

        // 3) PRESENT but HEARTBEAT-STALE handle (ancient heartbeat ts) → terminal
        //    for the same reason, even though it is not cancelled.
        let ch_stale = ChannelId::new(5406);
        let stale = backstop_watcher_handle(&session, &transcript_str);
        // 1ms epoch is far older than TMUX_WATCHER_STALE_HEARTBEAT_MS → stale.
        stale
            .last_heartbeat_ts_ms
            .store(1, std::sync::atomic::Ordering::Release);
        assert!(
            stale.heartbeat_stale(),
            "test precondition: handle is stale"
        );
        shared.tmux_watchers.insert(ch_stale, stale);
        assert!(
            watcher_backstop_turn_is_terminal(&shared, ch_stale, &ProviderKind::Claude, true),
            "a present-but-heartbeat-stale watcher must be treated as terminal too"
        );
        assert!(
            !watcher_backstop_turn_is_terminal(&shared, ch_stale, &ProviderKind::Claude, false),
            "codex r1: the STRICT mode must never count a stale handle as terminal"
        );

        // 4) ABSENT handle: terminal at the natural deadline, DEFER in strict.
        let ch_absent = ChannelId::new(5407);
        assert!(watcher_backstop_turn_is_terminal(
            &shared,
            ch_absent,
            &ProviderKind::Claude,
            true
        ));
        assert!(
            !watcher_backstop_turn_is_terminal(&shared, ch_absent, &ProviderKind::Claude, false),
            "codex r1: the STRICT mode must never count an absent handle as terminal"
        );

        let _ = std::fs::remove_file(&transcript);
    }

    /// With the watcher far-backstop ARMED at `register_start`, a JSONL Done
    /// terminal still finalizes PROMPTLY (the backstop only catches turns that
    /// never terminate; it must never delay a real terminal). Exactly once.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn jsonl_done_terminal_finalizes_promptly_with_backstop_armed() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(5303);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(90))
                .await;

            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, 90, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            // No sleep — the real terminal must win immediately despite the armed
            // 1800s backstop.
            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(
                    outcome,
                    FinalizeOutcome::Finalized {
                        removed_token: Some(_),
                        ..
                    }
                ),
                "a Done terminal must finalize promptly and release the token, backstop armed"
            );
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// Exactly-once across the backstop window: a turn finalized PROMPTLY by its
    /// real terminal must NOT be finalized a second time when the
    /// `WATCHER_REGISTER_BACKSTOP` horizon later elapses — the entry is already
    /// `Finalized`, so the reconciler's watcher pass skips it (no double counter
    /// decrement, no double side-effects).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_backstop_no_double_finalize_after_terminal() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(5404);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(100))
                .await;

            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, 100, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(first, FinalizeOutcome::Finalized { .. }));
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

            // Cross the far-backstop horizon. The reconciler watcher pass must
            // find the entry already Finalized (or GC'd) and do nothing.
            tokio::time::sleep(WATCHER_REGISTER_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "the far-backstop must not double-finalize an already-finalized turn"
            );
        })
        .await;
    }

    /// Window after which the #3277 fast path must have finalized a provably
    /// terminal watcher-owned turn: two interval-spaced probes build the
    /// streak, the pulled-in deadline elapses after `GATE_BACKSTOP`, plus
    /// reconcile-tick slack. Far below the 1800s horizon.
    const FAST_PATH_WINDOW: Duration = Duration::from_secs(
        WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL.as_secs() * 2
            + GATE_BACKSTOP.as_secs()
            + RECONCILE_INTERVAL.as_secs() * 3
            + 3,
    );

    /// #3277 (Defect C) incident shape: a watcher-owned `register_start`
    /// Pending whose LIVE unpaused watcher sits parked at transcript EOF over
    /// a JSONL turn terminator already on disk (provably `Done` on every
    /// probe) is finalized by the proven-terminal FAST path well within ~40s —
    /// NOT after the 1800s far-backstop horizon the #3277 incident waited out.
    /// (codex r1: an ABSENT handle no longer takes this path — the proof must
    /// come from the transcript under a live handle, see
    /// `watcher_backstop_fast_path_never_counts_absent_or_dead_handle`.)
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_backstop_fast_path_finalizes_proven_terminal_promptly() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(5501);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(110))
                .await;

            // Live unpaused watcher over a transcript whose Claude turn
            // terminator is already on disk → `CompletionSignal::Done`.
            let session = format!("3277-fastpath-done-{}", std::process::id());
            let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
            std::fs::write(
                &transcript,
                "{\"type\":\"result\",\"result\":\"done\",\"session_id\":\"s\"}\n",
            )
            .unwrap();
            shared.tmux_watchers.insert(
                ch,
                backstop_watcher_handle(&session, transcript.to_str().unwrap()),
            );

            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, 110, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            // FAR less than WATCHER_REGISTER_BACKSTOP (1800s).
            assert!(FAST_PATH_WINDOW < WATCHER_REGISTER_BACKSTOP / 10);
            tokio::time::sleep(FAST_PATH_WINDOW).await;
            tokio::task::yield_now().await;

            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "the proven-terminal fast path must finalize within the short window"
            );
            let _ = std::fs::remove_file(&transcript);
            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "a late terminal must lose the exactly-once gate after the fast path finalized"
            );
        })
        .await;
    }

    /// #3277 codex r1 (HIGH): an ABSENT, CANCELLED, or heartbeat-STALE watcher
    /// handle — e.g. transiently mid watcher replace/reuse — must NEVER count
    /// as a fast-path terminal probe while the JSONL transcript still says
    /// busy (`PausedLive`). Pre-fix the absent/dead early-return short-circuited
    /// BEFORE the transcript read, so two probes pulled the deadline in and
    /// the at-deadline re-check passed for the same reason, finalizing a busy
    /// turn in ~40s. All three turns must stay Pending through the fast-path
    /// window; the NATURAL 1800s expiry then keeps the legacy absent/dead
    /// handle authority (#3268) and releases them — no regression there.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_backstop_fast_path_never_counts_absent_or_dead_handle() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(3, Ordering::Relaxed);

            // Busy transcript: content but NO Claude turn terminator →
            // `PausedLive` for the whole test.
            let session = format!("3277-codexr1-dead-{}", std::process::id());
            let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
            std::fs::write(
                &transcript,
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"working…\"}]}}\n",
            )
            .unwrap();
            let transcript_str = transcript.to_str().unwrap().to_string();

            // ch 6701: NO handle at all. ch 6702: present-but-CANCELLED.
            // ch 6703: present-but-heartbeat-STALE.
            let ch_absent = ChannelId::new(6701);
            let ch_cancelled = ChannelId::new(6702);
            let ch_stale = ChannelId::new(6703);
            let cancelled = backstop_watcher_handle(&session, &transcript_str);
            cancelled
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            shared.tmux_watchers.insert(ch_cancelled, cancelled);
            let stale = backstop_watcher_handle(&session, &transcript_str);
            stale
                .last_heartbeat_ts_ms
                .store(1, std::sync::atomic::Ordering::Release);
            assert!(stale.heartbeat_stale(), "test precondition: stale handle");
            shared.tmux_watchers.insert(ch_stale, stale);

            let fin = TurnFinalizer::spawn();
            for (i, ch) in [ch_absent, ch_cancelled, ch_stale].into_iter().enumerate() {
                let msg_id = 170 + i as u64;
                shared
                    .mailbox(ch)
                    .restore_active_turn(
                        Arc::new(CancelToken::new()),
                        UserId::new(7),
                        MessageId::new(msg_id),
                    )
                    .await;
                fin.register_start(
                    TurnKey::new(ch, msg_id, 0),
                    ProviderKind::Claude,
                    RelayOwnerKind::Watcher,
                    &shared,
                );
            }

            // Through the whole fast-path window: no probe may count the
            // absent/dead handles as terminal (pre-fix: all three finalized).
            tokio::time::sleep(FAST_PATH_WINDOW).await;
            tokio::task::yield_now().await;
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                3,
                "an absent/cancelled/stale handle alone must never feed the \
                 fast-path terminal streak while the transcript says busy"
            );

            // The NATURAL far horizon keeps the pre-#3277 absent/dead-handle
            // authority (#3268): all three finalize at the 1800s deadline.
            tokio::time::sleep(WATCHER_REGISTER_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "the natural 1800s at-deadline must still finalize absent/dead-handle turns"
            );
            let _ = std::fs::remove_file(&transcript);
        })
        .await;
    }

    /// #3277 over-finalize regression guard: a GENUINELY-LIVE (fresh-heartbeat)
    /// PAUSED watcher handle means a Discord turn owns the session — every fast
    /// path probe returns non-terminal, the streak stays 0, the deadline is
    /// never pulled in, and the turn stays Pending through the whole fast-path
    /// window. Its real terminal still finalizes it afterwards.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_backstop_fast_path_defers_live_paused_handle() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(5502);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(120))
                .await;

            // Live paused handle: the probe defers BEFORE any transcript read,
            // so the output path never needs to exist.
            let session = format!("3277-fastpath-paused-{}", std::process::id());
            let handle = backstop_watcher_handle(&session, "/nonexistent/3277-paused.jsonl");
            handle
                .paused
                .store(true, std::sync::atomic::Ordering::Release);
            shared.tmux_watchers.insert(ch, handle);

            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, 120, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            tokio::time::sleep(FAST_PATH_WINDOW).await;
            tokio::task::yield_now().await;

            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                1,
                "a live paused watcher must keep the fast path from pulling the deadline"
            );
            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(outcome, FinalizeOutcome::Finalized { .. }),
                "the deferred turn must still be live for its real terminal"
            );
        })
        .await;
    }

    /// #3277 flapping scenario: the FIRST probe observes no handle (codex r1:
    /// absent now DEFERS, streak stays 0) and a live paused handle appears
    /// before the second probe — the deadline is NOT pulled in, and even when
    /// the full 1800s deadline elapses the at-deadline re-check defers
    /// (paused) and re-arms instead of finalizing.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_backstop_fast_path_flapping_resets_streak() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(5503);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(130))
                .await;

            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, 130, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            // Let the first probe run with NO handle (codex r1: absent →
            // defer), then install a live paused handle BEFORE the second
            // interval-spaced probe (a watcher replace/restart interleaving).
            tokio::time::sleep(RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;
            let session = format!("3277-fastpath-flap-{}", std::process::id());
            let handle = backstop_watcher_handle(&session, "/nonexistent/3277-flap.jsonl");
            handle
                .paused
                .store(true, std::sync::atomic::Ordering::Release);
            shared.tmux_watchers.insert(ch, handle);

            tokio::time::sleep(FAST_PATH_WINDOW).await;
            tokio::task::yield_now().await;
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                1,
                "one terminal probe + a live successor must not pull the deadline in"
            );

            // Cross the full far horizon: the at-deadline re-check must DEFER
            // (paused handle) and re-arm rather than finalize.
            tokio::time::sleep(WATCHER_REGISTER_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                1,
                "the at-deadline re-check must defer a paused-live turn (re-arm, not finalize)"
            );
            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(outcome, FinalizeOutcome::Finalized { .. }),
                "the deferred turn must still be live for its real terminal"
            );
        })
        .await;
    }

    /// #3277 exactly-once across the pulled-in deadline: the fast path pulls
    /// the deadline, then the watcher submits its real terminal BEFORE the
    /// pulled deadline elapses — the backstop must be a no-op (phase guard),
    /// with no double finalize.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_backstop_fast_path_noop_after_real_terminal() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(5504);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(140))
                .await;

            // codex r1: the pull now requires a LIVE unpaused handle over a
            // `Done` transcript (absent-handle probes defer instead).
            let session = format!("3277-fastpath-noop-{}", std::process::id());
            let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
            std::fs::write(
                &transcript,
                "{\"type\":\"result\",\"result\":\"done\",\"session_id\":\"s\"}\n",
            )
            .unwrap();
            shared.tmux_watchers.insert(
                ch,
                backstop_watcher_handle(&session, transcript.to_str().unwrap()),
            );

            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, 140, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            // Past both probes (streak 2 → deadline pulled to +GATE_BACKSTOP)
            // but BEFORE that pulled deadline elapses.
            tokio::time::sleep(WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL + RECONCILE_INTERVAL * 3)
                .await;
            tokio::task::yield_now().await;
            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(outcome, FinalizeOutcome::Finalized { .. }),
                "the real terminal must win while the pulled deadline is still pending"
            );
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

            // Cross the pulled-in deadline: the backstop finds the entry
            // already Finalized and must do nothing.
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                0,
                "the pulled-in backstop must not double-finalize after the real terminal"
            );
            let _ = std::fs::remove_file(&transcript);
        })
        .await;
    }

    /// #3277 verify-1 (MAJOR): a dispatched turn whose live watcher is
    /// registered under a DIFFERENT owner channel (`claim_or_reuse_watcher`
    /// ReuseExisting — the registry's channel index keys OWNER channels only)
    /// must NOT be mis-finalized as "handle absent → terminal". The probe and
    /// the at-deadline re-check resolve the handle via the turn's inflight
    /// `tmux_session_name`, find the live PausedLive watcher on the owner
    /// channel, and DEFER. Pre-fix, the dispatch-channel lookup missed the
    /// handle and the fast path finalized the live turn within ~40s.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_backstop_probe_resolves_owner_keyed_watcher_for_dispatched_turn() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let owner_ch = ChannelId::new(6601);
            let dispatch_ch = ChannelId::new(6602);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(dispatch_ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(150))
                .await;

            // Live watcher on the OWNER channel over a busy (PausedLive)
            // transcript — exactly the #3041 P1-2 reused-watcher shape.
            let session = format!("3277-owner-keyed-{}", std::process::id());
            let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
            std::fs::write(
                &transcript,
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"working…\"}]}}\n",
            )
            .unwrap();
            shared.tmux_watchers.insert(
                owner_ch,
                backstop_watcher_handle(&session, transcript.to_str().unwrap()),
            );

            // The dispatched turn's inflight names the reused tmux session, so
            // the probe can re-key its handle lookup off the dispatch channel.
            let state = super::super::inflight::InflightTurnState::new(
                ProviderKind::Claude,
                dispatch_ch.get(),
                None,
                7,
                150,
                151,
                "dispatched".to_string(),
                None,
                Some(session.clone()),
                Some(transcript.to_str().unwrap().to_string()),
                None,
                0,
            );
            super::super::inflight::save_inflight_state(&state).unwrap();

            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(dispatch_ch, 150, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            // Through the whole fast-path window AND past the far horizon: the
            // owner-keyed live PausedLive watcher must defer both checks.
            tokio::time::sleep(FAST_PATH_WINDOW).await;
            tokio::task::yield_now().await;
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                1,
                "the fast path must find the live owner-keyed watcher and defer \
                 (pre-fix: dispatch-channel lookup missed it → early finalize)"
            );
            tokio::time::sleep(WATCHER_REGISTER_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                1,
                "the at-deadline re-check must also resolve the owner-keyed watcher and defer"
            );
            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(outcome, FinalizeOutcome::Finalized { .. }),
                "the deferred dispatched turn must still be live for its real terminal"
            );
            let _ = std::fs::remove_file(&transcript);
        })
        .await;
    }

    /// #3277 verify-3 (MINOR) truth table: the fast-path probe
    /// (`allow_pane_probe == false`) must NEVER report `Unknown` (non-JSONL
    /// runtime) as terminal — and must not even RUN the pane capture — while
    /// the at-deadline re-check keeps the pane-ready fallback. `Done` /
    /// `PausedLive` verdicts are identical in both modes.
    #[test]
    fn non_jsonl_signal_never_terminal_on_fast_path_probe() {
        use std::cell::Cell;
        // Unknown + fast path: non-terminal AND the pane capture must not run.
        let captured = Cell::new(false);
        assert!(!watcher_backstop_signal_is_terminal(
            CompletionSignal::Unknown,
            false,
            || {
                captured.set(true);
                true
            }
        ));
        assert!(
            !captured.get(),
            "the 15s fast-path probe must never run a blocking pane capture"
        );
        // Unknown + at-deadline: pane fallback decides (both directions).
        assert!(watcher_backstop_signal_is_terminal(
            CompletionSignal::Unknown,
            true,
            || true
        ));
        assert!(!watcher_backstop_signal_is_terminal(
            CompletionSignal::Unknown,
            true,
            || false
        ));
        // Done / PausedLive: identical in both modes, pane never consulted.
        for probe in [false, true] {
            assert!(watcher_backstop_signal_is_terminal(
                CompletionSignal::Done,
                probe,
                || unreachable!("Done must not consult the pane")
            ));
            assert!(!watcher_backstop_signal_is_terminal(
                CompletionSignal::PausedLive,
                probe,
                || unreachable!("PausedLive must not consult the pane")
            ));
        }
    }

    /// #3277 verify-3 (MINOR): a watcher-owned turn on a non-JSONL runtime
    /// (Gemini → `CompletionSignal::Unknown`) keeps the generous 1800s
    /// at-deadline behavior — the fast path never pulls its deadline in, so it
    /// stays Pending through the whole fast-path window.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn non_jsonl_runtime_turn_does_not_take_fast_path() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(6603);
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token, UserId::new(7), MessageId::new(160))
                .await;

            // Live watcher; Gemini has no structured JSONL turn state, so the
            // transcript path is never read (signal = Unknown immediately).
            let session = format!("3277-nonjsonl-{}", std::process::id());
            shared.tmux_watchers.insert(
                ch,
                backstop_watcher_handle(&session, "/nonexistent/3277-nonjsonl.jsonl"),
            );

            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, 160, 0);
            fin.register_start(k, ProviderKind::Gemini, RelayOwnerKind::Watcher, &shared);

            tokio::time::sleep(FAST_PATH_WINDOW).await;
            tokio::task::yield_now().await;
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                1,
                "a non-JSONL runtime turn must never be finalized by the fast path"
            );
            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Gemini,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(outcome, FinalizeOutcome::Finalized { .. }),
                "the non-JSONL turn must still be live for its real terminal"
            );
        })
        .await;
    }
}
