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

use std::{collections::HashMap, panic::AssertUnwindSafe, sync::Arc, time::Duration};

use futures::FutureExt;
use serenity::model::id::ChannelId;
// `tokio::time::Instant` (not `std::time::Instant`) so deadlines respect the
// paused/virtual test clock and the production `interval` clock alike.
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::provider::{CancelToken, ProviderKind};

use super::SharedData;
// #3041 P1-0: dormant lease types for the *Delivery messages below (mod.rs §2-§3).
use super::{DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome};

pub(in crate::services::discord) mod cleanup;
pub(in crate::services::discord) mod completion_signal;
mod delivery_lease;
mod finalize;
mod finalize_context;
mod reconcile;
mod watcher_backstop;

pub(in crate::services::discord) use cleanup::SyntheticClaimSnapshot;
// #3479 r9: completion-signal enum + pure derivation extracted; re-exported so
// the `completion_signal_state` method and the watcher-backstop re-check below
// reference them unqualified, byte-identical.
pub(in crate::services::discord) use self::completion_signal::{
    CompletionSignal, completion_signal_from_transcript,
};
// #3479 r9: dormant delivery-lease handlers extracted to the child module; the
// actor-loop call sites below reference them unqualified, byte-identical.
#[allow(unused_imports)] // handlers are `#[cfg(unix)]`-conditional + dormant.
use self::delivery_lease::{
    handle_acquire_delivery, handle_commit_delivery, handle_release_delivery,
};
// #3479 r9: watcher far-backstop tunables + terminal-or-defer verdict pair
// extracted; the `reconcile` loop references them unqualified, byte-identical.
use self::watcher_backstop::{
    WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL, WATCHER_BACKSTOP_TERMINAL_STREAK,
    watcher_backstop_turn_is_terminal,
};
// #3894: per-submission finalize context extracted; re-exported so external
// submit sites + the routed in-file call sites reference `FinalizeContext`
// unqualified, byte-identical.
pub(in crate::services::discord) use self::finalize_context::FinalizeContext;
// #3894: the finalize side-effect chokepoint extracted; re-imported so
// `handle_terminal` + the reconcile/backstop child call it byte-identically.
use self::finalize::do_finalize;
// #3894: the timer-driven reconcile/backstop cluster extracted; re-imported so
// the actor loop's reconcile `select!` arm stays byte-identical.
use self::reconcile::reconcile;

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

    /// The literal full-identity key for this turn. The finalize ledger keys on
    /// this `LedgerKey` so channel-only (`user_msg_id == 0`) recovery/orphan
    /// terminals collapse onto the same live entry.
    pub(in crate::services::discord) fn exact_key(&self) -> LedgerKey {
        LedgerKey {
            channel_id: self.channel_id,
            generation: self.generation,
            user_msg_id: self.user_msg_id,
        }
    }
}

/// The exact ledger match: channel + restart generation + user message id.
/// Full identity so sequential same-channel turns never collide.
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

/// Channel-only collapse over an explicit candidate set: callers pass their own
/// `(LedgerKey, is_terminal)` pairs and get the identical finalized-guard /
/// single-live-entry semantics as the finalizer's in-line `HashMap` walk above,
/// without duplicating the subtle ambiguity rule.
///
/// - A real `user_msg_id` uses its exact key.
/// - A channel-only id collapses onto the single non-terminal entry ONLY when
///   exactly one live entry exists and no terminal entry exists for the same
///   channel/generation (ambiguous otherwise → route to the literal orphan key,
///   a no-op for the caller).
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
    let mut live_matches = candidates.into_iter().filter(|(lk, is_terminal)| {
        lk.channel_id == key.channel_id && lk.generation == key.generation && !*is_terminal
    });
    let Some((only_live, _)) = live_matches.next() else {
        return key.exact_key();
    };
    if live_matches.next().is_some() {
        return key.exact_key();
    }
    *only_live
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

/// #3646 OBSERVATION-ONLY: stable wire string for the `finalizer_ledger_owner`
/// event's `terminal_event` field (avoids leaking `GateTimeout`'s inner struct
/// into the payload via `Debug`).
fn terminal_event_kind_str(event: &TerminalEvent) -> &'static str {
    match event {
        TerminalEvent::Complete => "complete",
        TerminalEvent::Cancel => "cancel",
        TerminalEvent::GateTimeout { .. } => "gate_timeout",
        TerminalEvent::RelayMiss => "relay_miss",
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
        claim_snapshot: Option<SyntheticClaimSnapshot>,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<FinalizeOutcome>,
    },
    /// #3041 §2-§3 (DORMANT until P1-2..): CAS-acquire `(key, [start,end))` for
    /// `holder` via the actor. The watcher acquires the cell directly (B4
    /// fast-path), so this variant has no sender yet — it is reserved for the
    /// sink/bridge wiring.
    #[allow(dead_code)] // #3041: no sender until sink/bridge wiring (P1-2..).
    AcquireDelivery {
        key: DeliveryLeaseKey,
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
        key: DeliveryLeaseKey,
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
        key: DeliveryLeaseKey,
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
                claim_snapshot,
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
        key: DeliveryLeaseKey,
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
        key: DeliveryLeaseKey,
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

/// #3866: render a caught panic payload for the actor's error log without
/// re-panicking. `catch_unwind` yields `Box<dyn Any + Send>`; the common
/// payloads are `&'static str` and `String`.
fn panic_payload_summary(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
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
                        claim_snapshot,
                        shared,
                        ack,
                    } => {
                        cached_shared = Some(Arc::downgrade(&shared));
                        // #3866: the actor is a single, never-respawned task that
                        // owns finalize for the whole process. The crate unwinds
                        // (not `panic = "abort"`), so without this guard ONE panic
                        // in a finalize side-effect would silently kill the loop
                        // and every subsequent turn would fall through to
                        // `AlreadyFinalized` (placeholder stuck / final answer
                        // never delivered) for the rest of the process lifetime.
                        // Contain the per-message handler: a caught panic logs and
                        // resolves the ack as `AlreadyFinalized` so the submitter
                        // does not hang, and the loop survives to finalize the next
                        // turn.
                        let outcome = match AssertUnwindSafe(handle_terminal(
                            &mut ledger,
                            key,
                            provider,
                            event,
                            ctx,
                            claim_snapshot,
                            &shared,
                        ))
                        .catch_unwind()
                        .await
                        {
                            Ok(outcome) => outcome,
                            Err(payload) => {
                                tracing::error!(
                                    panic = %panic_payload_summary(payload.as_ref()),
                                    "TurnFinalizer handle_terminal panicked; actor loop \
                                     contained the panic and stays alive (#3866)"
                                );
                                FinalizeOutcome::AlreadyFinalized
                            }
                        };
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
                    // #3866: the reconcile arm runs the SAME finalize side-effect
                    // surface as the Terminal arm — the gate-timeout deadline
                    // backstop and the watcher far-backstop both drive
                    // `run_backstop_finalize -> do_finalize`. Guarding ONLY the
                    // Terminal arm left this path naked: a panic in a backstop
                    // finalize (or in the proven-terminal probe / lease reclaim /
                    // GC) would unwind the single never-respawned actor and
                    // silently kill finalize for the rest of the process. Contain
                    // the whole reconcile pass exactly like `handle_terminal`.
                    // `do_finalize` is ALSO guarded inside `run_backstop_finalize`
                    // (so a mid-finalize panic still flips the entry
                    // Finalizing->Finalized and is never left stuck); this outer
                    // guard additionally contains the non-finalize reconcile
                    // surface and keeps the loop alive.
                    if let Err(payload) = AssertUnwindSafe(reconcile(&mut ledger, &shared))
                        .catch_unwind()
                        .await
                    {
                        tracing::error!(
                            panic = %panic_payload_summary(payload.as_ref()),
                            "TurnFinalizer reconcile panicked; actor loop contained the panic \
                             and stays alive (#3866)"
                        );
                    }
                }
            }
        }
    }
}

/// Exactly-once gate + immediate-finalize / deferral decision. Runs inside the
/// actor task, so the check-and-set on `Phase` needs no synchronization.
/// #3866: test-only one-shot panic injection. A test arms the next
/// `handle_terminal` to panic so the live actor loop (single-threaded test
/// runtime → same thread as the spawned actor task) exercises the real
/// `catch_unwind` containment path.
#[cfg(test)]
mod test_panic_hook {
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        // #3866: a SEPARATE one-shot fired from INSIDE `do_finalize` (after the
        // caller flipped the entry Pending->Finalizing), so a test can prove the
        // caught-panic path resets the entry instead of leaving it stuck
        // `Finalizing`. The top-of-`handle_terminal` arm panics BEFORE any ledger
        // mutation and so under-proves the reset (the original #3866 test).
        static ARMED_IN_FINALIZE: Cell<bool> = const { Cell::new(false) };
    }

    /// Arm the next (and only the next) `handle_terminal` to panic at the TOP,
    /// before `resolve_ledger_key` / the phase flip — exercises only the actor
    /// loop's outer guard (no ledger mutation runs).
    pub(super) fn arm_once() {
        ARMED.with(|a| a.set(true));
    }

    /// Arm the next (and only the next) `do_finalize` to panic — fired AFTER the
    /// caller flipped the entry to `Finalizing`, so the test exercises the
    /// reset-to-`Finalized` repair on BOTH the terminal and reconcile/backstop
    /// paths (both run `do_finalize`).
    pub(super) fn arm_in_finalize_once() {
        ARMED_IN_FINALIZE.with(|a| a.set(true));
    }

    /// Consume the one-shot arm and panic if set.
    pub(super) fn maybe_panic() {
        if ARMED.with(|a| a.replace(false)) {
            panic!("injected finalize side-effect panic (#3866 test)");
        }
    }

    /// Consume the one-shot `do_finalize` arm and panic if set.
    pub(super) fn maybe_panic_in_finalize() {
        if ARMED_IN_FINALIZE.with(|a| a.replace(false)) {
            panic!("injected do_finalize side-effect panic after phase flip (#3866 test)");
        }
    }
}

async fn handle_terminal(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    key: TurnKey,
    provider: ProviderKind,
    event: TerminalEvent,
    ctx: FinalizeContext,
    claim_snapshot: Option<SyntheticClaimSnapshot>,
    shared: &Arc<SharedData>,
) -> FinalizeOutcome {
    // #3866: test-only injection point — lets a test drive a real finalize
    // side-effect panic through the live actor loop to prove the catch_unwind
    // guard keeps the loop alive. No effect in production builds.
    #[cfg(test)]
    test_panic_hook::maybe_panic();

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

    // #3646 OBSERVATION-ONLY (finalizer_ledger_owner companion): emit the
    // actor-owned ledger entry's `relay_owner` — the SECOND owner signal the
    // watcher-side `terminal_body_commit` event cannot read (the ledger lives on
    // this actor task; a synchronous cross-task query from the watcher would be
    // new behaviour). Keyed on the same `discord:<channel>:<user_msg_id>` turn id
    // so the two owner signals JOIN in PG and the #3607 "None-ledger vs
    // Watcher-finalize" ambiguity resolves. Read-only: it neither inspects nor
    // changes the clear_inflight / defer / finalize decision that follows.
    //
    // codex review #3678: key on the RESOLVED entry identity (`entry.turn_key`),
    // NOT the submitted `key`. A channel-only id-0 terminal collapses onto the
    // channel's real registered turn via `resolve_ledger_key`; that entry's
    // `turn_key` carries the real `user_msg_id`. Keying on the submitted `key`
    // would emit `user_msg_id=0` for exactly those collapsed terminals, dropping
    // the turn_id and breaking the JOIN against the watcher event — the #3607
    // cases this signal exists to disambiguate. Genuine orphans (id-0 with no
    // live entry) still carry id-0 here, which is correct (no real turn exists).
    super::relay_owner_observability::emit_finalizer_ledger_owner(
        entry.provider.as_str(),
        entry.turn_key.channel_id.get(),
        entry.turn_key.user_msg_id,
        entry.relay_owner.as_str(),
        terminal_event_kind_str(&event),
        ctx.clear_inflight,
    );

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
        // inflight — the caller's `watcher()` submit SKIPS its cleanup block and
        // discards this outcome, so reproduce the deadline-armed
        // `gate_backstop()` context shape: clear inflight here (else the file
        // keeps blocking the channel after the mailbox release) and preserve the
        // queue-admission bit; actual drain is the #4048 `do_finalize` event.
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
    // #3866/#4048: `do_finalize` is the single chokepoint for finalize
    // side-effects (inflight clear, mailbox token release, `global_active`
    // decrement, voice drain, completion-event publish). Contain a panic HERE
    // rather than only at the actor loop so the Finalizing->Finalized flip below
    // STILL runs on a caught panic.
    // That matters: the entry was just flipped to `Finalizing`, and reconcile GC
    // reaps only `Finalized` while every backstop/probe gates on `Pending`, so an
    // entry left stuck in `Finalizing` after a panic would leak FOREVER and
    // poison `ledger_has_live_watcher_pending` / `resolve_channel_only` for this
    // channel+generation. Resetting it to `Finalized` (the normal post-finalize
    // flip) lets GC reap it and frees the channel for the next turn.
    let outcome = match AssertUnwindSafe(do_finalize(
        finalize_key,
        provider,
        &event,
        effective_ctx,
        claim_snapshot.as_ref(),
        shared,
    ))
    .catch_unwind()
    .await
    {
        Ok(outcome) => outcome,
        Err(payload) => {
            tracing::error!(
                panic = %panic_payload_summary(payload.as_ref()),
                channel = ledger_key.channel_id.get(),
                user_msg_id = ledger_key.user_msg_id,
                "TurnFinalizer do_finalize panicked on the terminal path; contained, the \
                 ledger entry is reset Finalizing->Finalized below so it is never stuck (#3866)"
            );
            FinalizeOutcome::AlreadyFinalized
        }
    };
    if let Some(entry) = ledger.get_mut(&ledger_key) {
        entry.phase = Phase::Finalized;
        entry.finalized_at = Some(Instant::now());
        entry.terminal_deadline = None;
    }
    outcome
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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

    /// #3866: a panic inside a single `handle_terminal` must NOT kill the actor
    /// task. The next turn must still finalize. Without the `catch_unwind`
    /// guard in the actor loop the spawned task would unwind and die, so the
    /// second turn below would hang on its ack `oneshot` (the actor never
    /// replies) — the test would time out instead of observing `Finalized`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn panic_in_handle_terminal_does_not_kill_actor_loop() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
            let fin = TurnFinalizer::spawn();

            // First turn: arm the one-shot panic so this finalize unwinds inside
            // the actor task.
            let k1 = key(3866);
            fin.register_start(k1, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            test_panic_hook::arm_once();
            let first = fin
                .submit_terminal(
                    k1,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            // The caught panic resolves the ack as `AlreadyFinalized` (not a
            // hang) — the submitter survives too.
            assert!(
                matches!(first, FinalizeOutcome::AlreadyFinalized),
                "panicking finalize should resolve the ack as AlreadyFinalized (not hang)"
            );

            // Second, independent turn: the actor loop must still be alive and
            // finalize it normally. If the panic had killed the task this await
            // would hang forever (the start-paused runtime would stall).
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let k2 = key(38661);
            fin.register_start(k2, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            let second = fin
                .submit_terminal(
                    k2,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(second, FinalizeOutcome::Finalized { .. }),
                "actor loop must survive the earlier panic and finalize the next turn"
            );
        })
        .await;
    }

    /// #3866 (Finding 2 / 3a): a panic INSIDE `do_finalize` — fired AFTER the
    /// caller flipped the entry `Pending -> Finalizing` — must NOT leave the
    /// ledger entry stuck in `Finalizing`. A stuck `Finalizing` entry is never
    /// GC'd (reconcile reaps only `Finalized`) and never re-finalized (every
    /// backstop/probe gates on `Pending`), so it would leak forever and keep
    /// reporting the channel as a live watcher-pending turn. The earlier
    /// top-of-`handle_terminal` panic test arms BEFORE any ledger mutation, so it
    /// cannot prove this repair. Here the caught panic must reset the entry to
    /// `Finalized` (observed via `has_live_watcher_pending` flipping false), the
    /// actor loop must survive, and a same-channel follow-up turn must finalize.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn panic_inside_do_finalize_resets_stuck_finalizing_entry() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(38662);
            let generation = 0;

            // A live, watcher-owned turn.
            let k1 = TurnKey::new(ch, 11, generation);
            fin.register_start(k1, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            tokio::task::yield_now().await;
            assert!(
                fin.has_live_watcher_pending(ch, generation).await,
                "the registered watcher turn must start out live/pending"
            );

            // Arm a panic INSIDE do_finalize (after the Finalizing flip) and submit
            // the terminal. The actor flips Pending->Finalizing, calls do_finalize,
            // which panics; the guard contains it and the post-call flip resets the
            // entry to Finalized.
            shared.restart.global_active.store(1, Ordering::Relaxed);
            test_panic_hook::arm_in_finalize_once();
            let first = fin
                .submit_terminal(
                    k1,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(first, FinalizeOutcome::AlreadyFinalized),
                "a panic inside do_finalize must resolve the ack (not hang)"
            );

            // KEY assertion (Finding 2): the entry is reset to Finalized, NOT left
            // stuck Finalizing. A stuck entry would still be reported as a live
            // watcher-pending turn for this channel+generation forever.
            assert!(
                !fin.has_live_watcher_pending(ch, generation).await,
                "after a contained do_finalize panic the entry must be Finalized (not stuck \
                 Finalizing, which would stay watcher-pending forever)"
            );

            // The actor loop survived AND the channel is free: a same-channel
            // follow-up turn (new user_msg_id) still finalizes normally.
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let k2 = TurnKey::new(ch, 22, generation);
            fin.register_start(k2, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            let second = fin
                .submit_terminal(
                    k2,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(second, FinalizeOutcome::Finalized { .. }),
                "a same-channel follow-up turn must still finalize after the contained panic"
            );
        })
        .await;
    }

    /// #3866 (Finding 1 / 3b): a panic on the RECONCILE/BACKSTOP finalize path
    /// must be contained too. The original guard wrapped only the Terminal arm, so
    /// a panic in a gate-timeout backstop finalize (driven by the reconcile timer,
    /// NOT a terminal submission) would still unwind and permanently kill the
    /// single never-respawned actor. Here a deferred gate-timeout's deadline
    /// elapses, the reconciler runs the backstop `do_finalize` which panics; the
    /// guard must contain it, reset the entry to Finalized, and keep the loop
    /// alive to finalize a fresh turn.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn panic_on_reconcile_backstop_path_is_contained() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
            let fin = TurnFinalizer::spawn();
            let k = key(38663);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

            // Defer via a busy-pane gate-timeout (arms the backstop deadline).
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

            // Arm the panic so the reconciler's backstop do_finalize unwinds, then
            // let the deadline elapse so the reconcile tick fires it. Under
            // start_paused the runtime auto-advances once all tasks idle on timers.
            test_panic_hook::arm_in_finalize_once();
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            // The contained panic reset the deferred entry to Finalized: a late
            // terminal now sees AlreadyFinalized (the ack completing at all proves
            // the actor loop is still alive — a dead actor would hang it forever
            // under the paused runtime).
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
                "the backstop entry must be reset to Finalized after the contained panic"
            );

            // And the loop still finalizes a brand-new, independent turn.
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let k2 = key(386631);
            fin.register_start(k2, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            let fresh = fin
                .submit_terminal(
                    k2,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(fresh, FinalizeOutcome::Finalized { .. }),
                "actor loop must survive a reconcile/backstop panic and finalize the next turn"
            );
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn already_finalized_loser_removes_thread_parent_and_kicks_parent_queue() {
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
            let fin = TurnFinalizer::spawn();
            let parent = ChannelId::new(4_024_260);
            let thread = ChannelId::new(4_024_261);
            let turn_id = 4_024_262u64;
            let key = TurnKey::new(thread, turn_id, 0);

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

            shared.dispatch.thread_parents.insert(parent, thread);
            shared.restart.global_active.store(1, Ordering::Relaxed);
            shared
                .mailbox(thread)
                .restore_active_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    MessageId::new(turn_id),
                )
                .await;

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
                !shared.dispatch.thread_parents.contains_key(&parent),
                "AlreadyFinalized cleanup must drop the finalized thread's parent mapping"
            );
            assert_eq!(
                shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
                1,
                "dropping the parent mapping must schedule the parent queue kick"
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
    /// the pending-queue state so a queued follow-up is visible to the
    /// completion-event drain listener:
    ///   * outcome is `Finalized` (the channel does NOT stay stuck), and
    ///   * `removed_token` is `Some` (the active turn's mailbox token is
    ///     released — inflight/mailbox not orphaned), and
    ///   * `has_pending` is `true` (the queued follow-up is surfaced while the
    ///     #4048 completion event schedules its dispatch).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn restored_unregistered_watcher_gate_timeout_finalizes_and_kicks_off_queue() {
        use crate::services::turn_orchestrator::{Intervention, InterventionMode};
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            // true; the immediate-no-owner path must surface it for the
            // completion-event queue drain.
            shared
                .mailbox(ch)
                .replace_queue(
                    vec![Intervention {
                        author_id: UserId::new(1),
                        author_is_bot: false,
                        message_id: MessageId::new(71),
                        queued_generation: crate::services::discord::runtime_store::load_generation(
                        ),
                        source_message_ids: vec![MessageId::new(71)],
                        source_message_queued_generations: Vec::new(),
                        source_text_segments: Vec::new(),
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
                        "the queued follow-up must be honored for the completion-event queue drain"
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
                        queued_generation: crate::services::discord::runtime_store::load_generation(
                        ),
                        source_message_ids: vec![MessageId::new(5003)],
                        source_message_queued_generations: Vec::new(),
                        source_text_segments: Vec::new(),
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
            shared.dispatch.thread_parents.insert(thread_ch, ch);
            shared.dispatch.role_overrides.insert(ch, override_ch);

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
                    .dispatch
                    .thread_parents
                    .get(&thread_ch)
                    .is_some_and(|v| *v == ch),
                "stale terminal must NOT drop turn-2's dispatch_thread_parents entry"
            );
            assert!(
                shared
                    .dispatch
                    .role_overrides
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
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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

    #[test]
    fn channel_only_resolve_refuses_multi_live_collapse() {
        let ch = ChannelId::new(4243);
        let generation = 0u64;
        let live_a = LedgerKey {
            channel_id: ch,
            generation,
            user_msg_id: 1001,
        };
        let live_b = LedgerKey {
            channel_id: ch,
            generation,
            user_msg_id: 1002,
        };
        let zero_key = TurnKey::new(ch, 0, generation);
        let candidates = [
            (&live_a, /* is_terminal */ false),
            (&live_b, /* is_terminal */ false),
        ];

        assert_eq!(
            resolve_channel_only(zero_key, candidates.iter().copied()),
            zero_key.exact_key(),
            "an id-0 terminal with multiple live candidates must not pick a HashMap-arbitrary entry"
        );
    }

    // =======================================================================
    // #3016 S1 — read-only watcher-pending probes. (#3479 r9: the
    // completion-signal derivation tests moved to the `completion_signal`
    // child module alongside the enum + pure fn they exercise.)
    // Additive, dead until S3/S4; these prove the read-only contract now.
    // =======================================================================

    // has_live_watcher_pending: true for a watcher-owned non-finalized entry.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_pending_true_for_live_watcher_entry() {
        // #3016 phase-5a: `register_start` now takes `&Arc<SharedData>` (to prime
        // the reconcile cache). This test only registers + probes the ledger (no
        // terminal, no reconcile), so an inert shared suffices.
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
        use crate::services::discord::{DeliveryLeaseCell, DeliveryLeaseKey};
        use crate::services::provider::ProviderKind;
        use serenity::model::id::ChannelId;
        use std::sync::Arc;

        fn watcher(id: u64) -> LeaseHolder {
            LeaseHolder::Watcher { instance_id: id }
        }

        fn lease_key(ch: ChannelId, user_msg_id: u64) -> DeliveryLeaseKey {
            DeliveryLeaseKey::from_turn_key(TurnKey::new(ch, user_msg_id, 0))
        }

        /// Watcher/Delivered: a freshly-acquired lease committed `Delivered`
        /// advances `confirmed_end_offset` to the leased `end` EXACTLY ONCE, and
        /// no duplicate occurs.
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn watcher_delivered_advances_offset_once() {
            with_isolated_runtime_root(|| async move {
                let shared = make_shared_data_for_tests_with_storage(None);
                let fin = TurnFinalizer::spawn();
                let ch = ChannelId::new(7001);
                let lease = shared.delivery_lease(ch);
                let turn = lease_key(ch, 11);
                let h = watcher(1);

                // Acquire on the cell (the watcher fast-path), then commit through
                // the actor (the path the watcher uses).
                assert!(lease.try_acquire(turn.clone(), h, 0, 64, 1_000));
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
                let shared = make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(7002);
                let lease = shared.delivery_lease(ch);
                let turn = lease_key(ch, 22);

                let w1 = watcher(1);
                let w2 = watcher(2);
                // First watcher acquires for [0,32).
                assert!(lease.try_acquire(turn.clone(), w1, 0, 32, 5_000));
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
                let shared = make_shared_data_for_tests_with_storage(None);
                let fin = TurnFinalizer::spawn();
                let ch = ChannelId::new(7003);
                let lease = shared.delivery_lease(ch);
                let turn = lease_key(ch, 33);
                let h = watcher(1);

                assert!(lease.try_acquire(turn.clone(), h, 0, 48, 1_000));
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
                let shared = make_shared_data_for_tests_with_storage(None);
                let fin = TurnFinalizer::spawn();
                let ch = ChannelId::new(7004);
                let lease = shared.delivery_lease(ch);
                let turn = lease_key(ch, 44);
                let h = watcher(1);

                assert!(lease.try_acquire(turn.clone(), h, 0, 80, 1_000));
                assert!(
                    fin.commit_delivery(
                        turn.clone(),
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
                let shared = make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(7005);
                let lease = shared.delivery_lease(ch);
                let turn_a = lease_key(ch, 55);
                let dead = watcher(1);

                // A holder acquires with deadline 100ms (monotonic units) but never
                // commits/releases (dead).
                assert!(lease.try_acquire(turn_a.clone(), dead, 0, 16, 100));
                // Before the deadline, the sweep is a no-op and the cell stays held.
                assert_eq!(shared.reclaim_expired_delivery_leases(50), 0);
                assert!(!lease.try_acquire(turn_a, watcher(2), 0, 16, 100));
                // Past the deadline, the sweep reclaims exactly this cell.
                assert_eq!(shared.reclaim_expired_delivery_leases(100), 1);
                // A later legitimate acquire (new instance, new turn) succeeds.
                let turn_b = lease_key(ch, 66);
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
                let shared = make_shared_data_for_tests_with_storage(None);
                let fin = TurnFinalizer::spawn();
                let ch = ChannelId::new(7006);
                let lease = shared.delivery_lease(ch);
                let turn1 = lease_key(ch, 77);
                let h = watcher(1);

                assert!(lease.try_acquire(turn1.clone(), h, 0, 24, 1_000));
                assert!(
                    fin.commit_delivery(
                        turn1.clone(),
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
                let turn2 = lease_key(ch, 88);
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
                let shared = make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(7007);
                let lease = shared.delivery_lease(ch);
                let dead_turn = lease_key(ch, 101);
                let dead = watcher(1);

                // Dead holder acquires with deadline 100 (monotonic units) and
                // then dies — never commits, never releases. No `SharedData` was
                // ever cached in any finalizer (we never spawn/route a Terminal).
                assert!(lease.try_acquire(dead_turn.clone(), dead, 0, 40, 100));

                // BEFORE the deadline: a NON-expired live lease still B2-skips —
                // the replacement's acquire-time reclaim is a no-op and the
                // acquire loses (single-holder invariant intact, no duplicate).
                let live = watcher(2);
                assert!(
                    !lease.reclaim_if_expired(50),
                    "a non-expired lease must NOT be reclaimed (would reintroduce duplicates)"
                );
                assert!(
                    !lease.try_acquire(dead_turn.clone(), live, 0, 40, 100),
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
                let shared = make_shared_data_for_tests_with_storage(None);
                let ch = ChannelId::new(7008);
                let lease = shared.delivery_lease(ch);
                let turn = lease_key(ch, 202);
                let h = watcher(1);

                assert!(lease.try_acquire(turn.clone(), h, 0, 96, 1_000));
                assert_eq!(
                    shared.committed_relay_offset(ch),
                    0,
                    "no advance before commit"
                );

                // The INLINE production sequence: synchronous cell commit, then
                // (on Delivered) the synchronous offset advance — NO actor await.
                let committed = lease.commit(h, turn.clone(), 0, 96, LeaseOutcome::Delivered);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
        let shared = super::super::make_shared_data_for_tests_with_storage(None);

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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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

    /// #3277 verify-3 (MINOR): a watcher-owned turn on a non-JSONL runtime
    /// (Gemini → `CompletionSignal::Unknown`) keeps the generous 1800s
    /// at-deadline behavior — the fast path never pulls its deadline in, so it
    /// stays Pending through the whole fast-path window.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn non_jsonl_runtime_turn_does_not_take_fast_path() {
        use serenity::model::id::{MessageId, UserId};
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
