//! #3089 Phase A1 — turn-output controller skeleton (pure add, no owner wired).
//!
//! This module introduces the single delivery entry point that Phase A will
//! eventually route all seven turn-output surfaces through. A1 is a **pure
//! add**: the controller is fully implemented and tested, but NO live owner
//! (sink / standby / watcher / turn_bridge / recovery / tui_prompt_relay) calls
//! it yet — the live-path cutover starts at A2 (`session_relay_sink` first).
//!
//! ## Invariant I1 — commit+advance is owned by the controller, inline, before
//! any post-send await
//!
//! Design §4.1 (review fix H2): an `async deliver_turn_output(...).await`
//! that hands the outcome back for the *caller* to commit is insufficient,
//! because owners (notably the watcher) have post-send awaits before they
//! advance — a caller-side commit can land after an await and re-open the
//! #3143 duplicate. Therefore `deliver_turn_output` performs
//! `lease.commit() + offset advance` **internally, synchronously, immediately
//! after confirmed transport success and before it does any cleanup / status /
//! placeholder-transition / await work**, and returns an already-committed
//! [`DeliveryOutcome`].
//!
//! ## Invariant I2 — ambiguous never advances
//!
//! Design §4.1: an ambiguous transport result (`Unknown` / a transport error
//! classified as transient) must NOT advance the committed offset. The
//! controller `release`s the lease *without* committing in that case, so the
//! durable frontier (Phase B) and the in-memory lease both stay at the
//! pre-send value.

use poise::serenity_prelude::{ChannelId, MessageId};

use super::super::gateway::TurnGateway;
use super::super::inflight::RelayOwnerKind;
use super::super::placeholder_controller::{
    PlaceholderController, PlaceholderControllerOutcome, PlaceholderKey, PlaceholderLifecycle,
};
use super::super::replace_outcome_policy::{
    WatcherSendFailureClass, classify_watcher_send_failure_message,
    strip_watcher_send_failure_class_marker,
};
use super::super::turn_finalizer::TurnKey;
use super::super::{
    DELIVERY_LEASE_DEADLINE_MS, DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome,
    LeaseSnapshot, lease_now_ms,
};
use super::decision::LengthPolicyDecision;

mod fresh_send;
#[cfg(test)]
mod fresh_send_tests;

/// The narrow delivery-lease surface the turn-output controller drives
/// (acquire → commit → release, plus `read` for the tests). Abstracting the
/// concrete [`DeliveryLeaseCell`] behind this trait lets the controller's tests
/// hang a **commit recorder** directly on the lease (codex review-fix M4): a
/// recording wrapper counts every `commit` call, so a mutation that commits on
/// the ambiguous (Transient/Unknown) arm — even a silent "commit then
/// immediately release" with no intervening await — is caught the moment it
/// invokes `commit`, not only if a later gateway await happens to observe the
/// `Committed` state.
///
/// `DeliveryLeaseCell` (the frozen #3041 cell) implements this by simple
/// delegation, so the live path is unchanged and A1 stays a pure add.
pub(in crate::services::discord) trait DeliveryLease {
    fn try_acquire(
        &self,
        key: DeliveryLeaseKey,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        deadline_ms: u64,
    ) -> bool;
    fn commit(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    ) -> bool;
    fn release(&self, holder: LeaseHolder, key: DeliveryLeaseKey, start: u64, end: u64) -> bool;
    /// Push this `(holder, key)` lease deadline to `new_deadline_ms` (range is
    /// not matched — see [`DeliveryLeaseCell::renew`]). A no-op `false` once the
    /// lease is no longer ours (committed / released / reclaimed). The A2a POST
    /// heartbeat ([`PostHeartbeat`]) calls this on a fixed interval so the
    /// `Leased{holder, fresh}` deadline stays ahead of the reconciler while a slow
    /// POST is in flight (#3151) — replacing A1's fixed-TTL acquire.
    #[allow(dead_code)] // #3089 A2a: driven by the owner's PostHeartbeat at A2b cutover.
    fn renew(&self, holder: LeaseHolder, key: DeliveryLeaseKey, new_deadline_ms: u64) -> bool;
    /// Reclaim an expired holder before a fresh-send attempt. Normal owner paths
    /// are reconciled externally; S1r-1's NoRange pseudo-range must recover the
    /// same dead-holder window as the D1 recovery path.
    #[allow(dead_code)] // #4046 S1r-1: used by the fresh-send verb before S1r-2~5 wiring.
    fn reclaim_if_expired(&self, now_ms: u64) -> bool;
    #[allow(dead_code)] // #3089 A1: read by the controller's own tests only.
    fn read(&self) -> LeaseSnapshot;
}

impl DeliveryLease for DeliveryLeaseCell {
    fn try_acquire(
        &self,
        key: DeliveryLeaseKey,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        deadline_ms: u64,
    ) -> bool {
        DeliveryLeaseCell::try_acquire(self, key, holder, start, end, deadline_ms)
    }
    fn commit(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    ) -> bool {
        DeliveryLeaseCell::commit(self, holder, key, start, end, outcome)
    }
    fn release(&self, holder: LeaseHolder, key: DeliveryLeaseKey, start: u64, end: u64) -> bool {
        DeliveryLeaseCell::release(self, holder, key, start, end)
    }
    fn renew(&self, holder: LeaseHolder, key: DeliveryLeaseKey, new_deadline_ms: u64) -> bool {
        DeliveryLeaseCell::renew(self, holder, key, new_deadline_ms)
    }
    fn reclaim_if_expired(&self, now_ms: u64) -> bool {
        DeliveryLeaseCell::reclaim_if_expired(self, now_ms)
    }
    fn read(&self) -> LeaseSnapshot {
        DeliveryLeaseCell::read(self)
    }
}

/// #3089 A3: a zero-sized no-op [`DeliveryLease`] for transport-only owners that
/// legacy never held a lease (standby_relay — no `DeliveryLeaseCell`, no offset
/// authority, no heartbeat). `try_acquire` ALWAYS returns `false`, so the
/// controller takes the `ProceedMarkerless` acquire-failure path: it holds no
/// lease (`lease_guard = None`, `heartbeat_guard = None`) and never commits or
/// releases — a pure transport. `commit`/`release`/`renew` are no-ops returning
/// `false` (unreachable on the markerless path, but a defensible answer keeps the
/// trait honest), and `read` reports `Unleased` (nothing was ever held).
/// Combined with `advance: None`, `commit_and_finalize` treats every confirmed
/// transport as `advanced = true` → `Delivered`, reproducing legacy standby
/// short-replace (transport-only, no state mutation).
pub(in crate::services::discord) struct NoLease;

impl DeliveryLease for NoLease {
    fn try_acquire(
        &self,
        _key: DeliveryLeaseKey,
        _holder: LeaseHolder,
        _start: u64,
        _end: u64,
        _deadline_ms: u64,
    ) -> bool {
        // ALWAYS fail the acquire → controller takes `ProceedMarkerless`
        // (transport-only, no lease held). This is the whole point of `NoLease`.
        false
    }
    fn commit(
        &self,
        _holder: LeaseHolder,
        _key: DeliveryLeaseKey,
        _start: u64,
        _end: u64,
        _outcome: LeaseOutcome,
    ) -> bool {
        // Unreachable on the markerless path (`lease_guard == None` ⇒ no commit);
        // a no-op keeps the trait honest.
        false
    }
    fn release(
        &self,
        _holder: LeaseHolder,
        _key: DeliveryLeaseKey,
        _start: u64,
        _end: u64,
    ) -> bool {
        false
    }
    fn renew(&self, _holder: LeaseHolder, _key: DeliveryLeaseKey, _new_deadline_ms: u64) -> bool {
        false
    }
    fn reclaim_if_expired(&self, _now_ms: u64) -> bool {
        false
    }
    fn read(&self) -> LeaseSnapshot {
        // Never held → `Unleased`.
        LeaseSnapshot::Unleased
    }
}

/// Initial acquire deadline (process-monotonic ms) the controller stamps on the
/// delivery lease for a single `deliver_turn_output` attempt. A1 recorded a
/// fixed 60s TTL because nothing renewed it; A2a instead matches the sink/watcher
/// HOLDER-LIVENESS contract — acquire with the shared
/// [`DELIVERY_LEASE_DEADLINE_MS`] (15s) and keep the deadline fresh with a POST
/// heartbeat ([`spawn_post_heartbeat`]). The deadline is therefore a liveness
/// signal, NOT a hard cap on delivery duration: a slow multi-chunk POST stays
/// leased because the heartbeat re-extends within one interval, while a dead
/// controller stops renewing and the lease lapses within ~one deadline (the
/// #3151 contract the sink uses at `session_relay_sink.rs:338`).
const TURN_OUTPUT_LEASE_TTL_MS: u64 = DELIVERY_LEASE_DEADLINE_MS;

/// The placeholder slot carried in the delivery context.
///
/// Design §4.2 names this `PlaceholderState`, but that symbol is already taken
/// by `shared_state::PlaceholderState` (the `SharedData` UI container). To
/// avoid a confusing shadow we name the controller-local slot `PlaceholderSlot`
/// while keeping the exact shape from the design (`None | Active{message_id,
/// key}`).
///
/// Constructed by owners at cutover (A2+); A1 prod has no owner, so the
/// variants are dormant outside the controller's own tests.
#[allow(dead_code)] // #3089 A1: constructed by owners at A2 cutover.
pub(in crate::services::discord) enum PlaceholderSlot {
    /// No live placeholder card to transition — a fresh send.
    None,
    /// An existing live placeholder card the controller may replace and then
    /// drive to a terminal lifecycle state via `PlaceholderController`.
    Active {
        message_id: MessageId,
        key: PlaceholderKey,
    },
}

/// What the controller should do with the turn body, derived from the
/// `outbound` length decision (`Inline → Replace`, `Split → SendNewChunks`).
///
/// Built by owners (via `from_length_decision`) at cutover (A2+); A1 prod has
/// no owner, so the variants and the mapping fn are dormant outside tests.
#[allow(dead_code)] // #3089 A1: built by owners at A2 cutover.
pub(in crate::services::discord) enum OutputPlan {
    /// Publish a new anchor-less message. The body is carried by
    /// [`TurnOutputCtx::body`]; `range` is the authoritative transcript byte range
    /// when one exists. `None` is deliver-without-advance: its pseudo-range only
    /// serializes the process-local lease, and its retry fingerprint lives outside
    /// the watcher-shared delivery-record namespace. `reference` is reserved for
    /// the later S1r owner cutovers; S1r-1 accepts only `None`.
    #[allow(dead_code)] // #4046 S1r-1: constructed by S1r-2~5 owner cutovers.
    SendFresh {
        range: Option<(u64, u64)>,
        reference: Option<(ChannelId, MessageId)>,
        record: FreshSendRecord,
    },
    /// Replace/edit the live placeholder in place (Inline body that fits a
    /// single message). The `lifecycle` distinguishes the three replace
    /// variants (cancel / prompt-too-long / normal) so a cutover owner can
    /// drive the correct terminal placeholder state (recon risk #5).
    Replace { lifecycle: PlaceholderLifecycle },
    /// Send `chunk_count` new chunked messages (Split body over the inline
    /// limit). `delete_anchor` is an owner capability bit for terminal arms
    /// whose legacy long-chunk path deletes the placeholder anchor only after
    /// every chunk has landed.
    SendNewChunks {
        chunk_count: usize,
        delete_anchor: bool,
    },
    /// Nothing to deliver (empty / suppressed body).
    NoOp,
}

pub(in crate::services::discord) use fresh_send::RecordContext as FreshSendRecord;

impl OutputPlan {
    /// Map an `outbound::decide_policy` length decision into an `OutputPlan`.
    ///
    /// - `Inline` → `Replace` (fits a single message; edit the placeholder in
    ///   place). The replace `lifecycle` is supplied by the caller because the
    ///   length decision alone cannot tell cancel / prompt-too-long / normal
    ///   apart.
    /// - `Split` → `SendNewChunks { chunk_count, delete_anchor: false }`.
    /// - `Compact` collapses to its single rendered message → `Replace`.
    /// - `FileAttachment` / `RejectOverLimit` are not turn-body relays through
    ///   this controller → `NoOp` (the owner handles those out of band).
    #[allow(dead_code)] // #3089 A1: called by owners at A2 cutover.
    pub(in crate::services::discord) fn from_length_decision(
        decision: &LengthPolicyDecision,
        replace_lifecycle: PlaceholderLifecycle,
    ) -> Self {
        match decision {
            LengthPolicyDecision::Inline { .. } | LengthPolicyDecision::Compact { .. } => {
                OutputPlan::Replace {
                    lifecycle: replace_lifecycle,
                }
            }
            LengthPolicyDecision::Split { chunk_count, .. } => OutputPlan::SendNewChunks {
                chunk_count: *chunk_count,
                delete_anchor: false,
            },
            LengthPolicyDecision::FileAttachment { .. }
            | LengthPolicyDecision::RejectOverLimit { .. } => OutputPlan::NoOp,
        }
    }
}

/// How a `Replace` plan's body reached Discord on a confirmed delivery, surfaced
/// so an owner can mirror the legacy per-variant post-send cleanup — both advance
/// the offset but the watcher cleans up DIFFERENTLY (#3089 A4 r2, codex r1 [High]).
/// The legacy fallback arm (`tmux_watcher.rs:6289-6372`) does NOT register the
/// original as the footer target and records `Failed(edit_error)` (not
/// `Succeeded`); collapsing both into a bare `Delivered` (r1) lost that → footer
/// mode could later edit the preserved original. `None` on `Delivered` (non-replace
/// / `NewSend` / markerless) means "no replace identity" — owners are unaffected.
#[allow(dead_code)] // #3089 A4: FreshFallbackAfterEditFailure read by the watcher write-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ReplaceDeliveryKind {
    /// The original placeholder was edited in place (the original IS the final
    /// message). Owners take their existing delivered side-effects.
    EditedOriginal,
    /// The in-place edit failed; the body was delivered via a FRESH send and the
    /// original placeholder is preserved (#2757). `edit_error` (the failing edit's
    /// error) lets the watcher record the legacy `failed(edit_error)` cleanup.
    FreshFallbackAfterEditFailure {
        edit_error: String,
        replacement_anchor: Option<MessageId>,
    },
}

/// Metadata for a confirmed `SendNewChunks` delivery. Owners use this to mirror
/// legacy long-terminal side effects after the controller has performed the
/// transport: durable tail-anchor recording and placeholder-delete cleanup
/// bookkeeping.
#[allow(dead_code)] // #3998 S1-d: read by A4/A5 long-chunk cutovers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct NewChunksDelivery {
    pub(in crate::services::discord) first_message_id: Option<MessageId>,
    pub(in crate::services::discord) tail_message_id: Option<MessageId>,
    pub(in crate::services::discord) anchor_delete_error: Option<String>,
}

/// The three-way committed result of a delivery attempt. The returned outcome
/// is ALREADY committed (I1): `Delivered` means the lease was committed
/// `Delivered` and the offset advanced before owner post-send finalization ran.
/// For the opt-in long-chunk anchor-delete arm, the anchor delete is classified
/// as part of the transport result so the controller mirrors legacy ordering
/// (chunks, then best-effort anchor delete, then Delivered/NotDelivered commit).
///
/// `Transient` (and its `retry_from_offset`) is part of the contract owners
/// consume from A2; A1 (no owner wired) has no transient transport
/// classification yet, so that arm is dormant until cutover.
#[allow(dead_code)] // #3089 A1: Transient arm dormant; owners wire it at A2.
pub(in crate::services::discord) enum DeliveryOutcome {
    /// Confirmed delivered to Discord; the committed offset advanced to
    /// `committed_to`. `replace_kind` carries HOW a `Replace` plan's body reached
    /// Discord (edit-in-place vs fresh fallback after an edit failure) so an owner
    /// can mirror the legacy per-variant post-send cleanup (#3089 A4 r2).
    /// `new_chunks` carries long-chunk tail-anchor/delete metadata for owners
    /// that opt into anchor deletion. `None` values are ignored by owners that
    /// do not need the corresponding side effect.
    Delivered {
        committed_to: u64,
        replace_kind: Option<ReplaceDeliveryKind>,
        new_chunks: Option<NewChunksDelivery>,
    },
    /// A fresh-message POST was confirmed. `committed_to` is `Some(end)` only
    /// when a real transcript range passed the owner's advance gate; `None` is a
    /// NoRange deliver-without-advance and never evaluates that callback.
    /// `persistence_recorded` reports whether the range frontier/new anchor or
    /// the isolated NoRange retry fingerprint was persisted for the current
    /// wrapper generation. `false` is still a confirmed POST, so callers must not
    /// blindly retry it and risk a duplicate message.
    FreshDelivered {
        committed_to: Option<u64>,
        persistence_recorded: bool,
    },
    /// Transport was confirmed, but the owner's identity-gated advance callback
    /// REFUSED to advance the offset (e.g. the inflight turn was cleared /
    /// replaced during a slow POST). The lease is committed `NotDelivered`, the
    /// offset stays at `committed_from`, and the owner's committed-offset
    /// reconciliation re-sends (no black-hole). Mirrors the sink's
    /// `advanced == false` arm (`session_relay_sink.rs:571-577`): commit
    /// `NotDelivered`, never advance.
    ///
    /// A2a: produced only when an owner passes an advance callback that returns
    /// `false`; A1 owners (no callback) keep the unconditional `Delivered`.
    NotDelivered { committed_from: u64 },
    /// A transient/retriable failure; the offset did NOT advance. The owner
    /// may retry from `retry_from_offset`.
    Transient { retry_from_offset: u64 },
    /// Ambiguous (drop / panic / partial). I2: the offset did NOT advance.
    ///
    /// `fell_back` (#3089 A5) is `true`, on the `NoCommitOnFallback` arm ONLY,
    /// when the body still reached Discord via a FRESH fallback send
    /// (`SentFallbackAfterEditFailure`): still NO advance, but the turn_bridge
    /// owner does the legacy dual-offset recovery (`response_sent_offset` bump,
    /// mod.rs ~6241) + a `failed(..)` cleanup. `false` for partial / Err.
    Unknown { fell_back: bool },
    /// Nothing was delivered by design (NoOp plan / suppressed); offset
    /// unchanged.
    Skipped,
}

/// How an edit-fail fallback should treat the original placeholder. Explicit,
/// with NO `Default` (the #2757 fence): the watcher's conditional-delete must
/// never silently reach sink/standby, which preserve the original on fallback
/// to avoid streamed-body loss.
///
/// `DeleteIfProvenStale` is the watcher arm, exercised from A2; A1 only
/// constructs `PreserveAlways` in its own tests.
#[allow(dead_code)] // #3089 A1: DeleteIfProvenStale arm wired by the watcher at A2.
pub(in crate::services::discord) enum EditFailPlaceholderPolicy {
    /// Never delete the original placeholder on edit-fail fallback
    /// (sink / standby — #2757).
    PreserveAlways,
    /// Delete the original placeholder ONLY if it is proven stale
    /// (watcher's conditional-delete arm).
    DeleteIfProvenStale,
}

/// How a `SentFallbackAfterEditFailure` replace result advances the lease.
///
/// `ReplaceLongMessageOutcome::SentFallbackAfterEditFailure` is NOT classified
/// uniformly across the existing owners, so the controller must NOT hard-code
/// one mapping:
///
/// - The sink commits/advances it: it bumps `delivered_total` and calls
///   `advance_after_confirmed_post`, returning `Delivered`
///   (`session_relay_sink.rs:905`). The fallback POST carried the response, so
///   the offset advances.
/// - Standby likewise returns success (`standby_relay.rs:662`): the fallback
///   delivered the body, advance.
/// - `turn_bridge`/`terminal_delivery` does NOT commit it: it records the
///   cleanup failure and returns `committed = false`
///   (`terminal_delivery.rs:143`); its commit predicate matches `EditedOriginal`
///   only (`terminal_delivery.rs:42`). The placeholder edit failed and the
///   terminal-delivery contract treats a non-edited terminal card as not yet
///   committed, so the offset must NOT advance.
///
/// Each owner therefore MUST pass its policy explicitly — NO `Default` (the
/// #2757 fence philosophy shared with [`EditFailPlaceholderPolicy`]): a missing
/// policy must be a compile error, never a silent advance/non-advance. The
/// sink/standby cutovers (A2/A3) pass `CommitOnFallback`; the turn_bridge
/// cutover (A5) passes `NoCommitOnFallback`.
#[allow(dead_code)] // #3089 A1: NoCommitOnFallback arm wired by turn_bridge at A5.
pub(in crate::services::discord) enum FallbackCommitPolicy {
    /// The fallback POST counts as delivery → commit/advance the offset
    /// (sink `session_relay_sink.rs:905`, standby `standby_relay.rs:662`).
    CommitOnFallback,
    /// The fallback edit failure does NOT commit → leave the offset un-advanced
    /// (turn_bridge `terminal_delivery.rs:143`, predicate `:42`). Maps to
    /// `Unknown` so a retry can re-deliver from the same offset (I2).
    NoCommitOnFallback,
}

/// What the controller does when the delivery-lease acquire FAILS (another
/// holder owns the `(channel, turn, range)` coordinate). Explicit, with NO
/// `Default` — each owner must pin its acquire-failure semantics, because the
/// two existing behaviours diverge and a silent default would either drop a send
/// or reintroduce a duplicate:
///
/// - `Transient` — the A1 behaviour: do NOT send; return
///   [`DeliveryOutcome::Transient`] so the owner retries. The watcher/bridge
///   terminal-delivery paths use this — a lost acquire means the range is being
///   handled elsewhere and a blind send would duplicate.
/// - `ProceedMarkerless` — the SINK behaviour (`session_relay_sink.rs:777-795`):
///   a failed acquire yields no marker, so the sink POSTs WITHOUT the lease and
///   WITHOUT a heartbeat. It never blocks delivery on a lost acquire (no
///   self-black-hole), and no duplicate arises because the OTHER holder owns the
///   range (single-winner CAS). Because the lease is not held on this path the
///   controller cannot commit it, so the advance is delegated to the owner
///   callback exactly as on the held path (the marker only gated the watcher; it
///   was never the advance authority).
///
/// Constructed by owners at cutover (A2+); A1/A2a prod has no owner, so both
/// arms are dormant outside the controller's own tests.
#[allow(dead_code)] // #3089 A2a: pure add; owners pin a mode at cutover (A2b+).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum AcquireFailureMode {
    /// A lost acquire is a not-now: do not send, return `Transient` (A1 /
    /// watcher / bridge).
    Transient,
    /// A lost acquire still POSTs, markerless and heartbeat-less (sink,
    /// `session_relay_sink.rs:777-795`).
    ProceedMarkerless,
}

/// Owner-provided, identity-gated advance callback (A2a capability 2).
///
/// On a CONFIRMED transport success the controller does not own the advance
/// logic — it DELEGATES to this callback, exactly as the sink delegates to
/// `advance_after_confirmed_post` → `advance_offset_for_confirmed_delegated_terminal`
/// (`session_relay_sink.rs:542-644`), which re-loads a FRESH inflight AFTER the
/// POST and runs the strict `(user_msg_id, started_at, turn_start_offset)`
/// identity gate before calling `tmux::advance_watcher_confirmed_end`.
///
/// Contract (matches that `-> bool` return):
/// - `true`  → the identity gate matched and the confirmed end was advanced; the
///   controller commits the lease `Delivered` and returns
///   [`DeliveryOutcome::Delivered`].
/// - `false` → the identity gate REFUSED (inflight cleared/replaced during the
///   slow POST); the controller commits `NotDelivered` and returns
///   [`DeliveryOutcome::NotDelivered`] so the owner's committed-offset
///   reconciliation re-sends (the `advanced == false` arm at
///   `session_relay_sink.rs:571-577`).
///
/// The argument is the controller's confirmed `(start, end)` byte range so the
/// owner can pin the same coordinate the lease was acquired for. The callback is
/// invoked INLINE, BEFORE any post-send await (I1) and ONLY on a confirmed
/// transport (never on Transient/Unknown — I2).
///
/// `&dyn` (not `FnOnce`) because the controller borrows it from the ctx and the
/// I1 inline-before-await ordering needs no ownership transfer. Owners that have
/// no identity gate (A1 semantics — unconditional advance) simply pass `None`,
/// preserving the existing always-`Delivered` behaviour.
/// `Send + Sync` (#3089 A2b): the first live owner (the session-bound sink) calls
/// `deliver_turn_output` from an `#[async_trait]` `RelaySink::deliver`, whose
/// future is `Send`. The advance callback is borrowed by `commit_and_finalize`
/// across no await (it runs inline before the post-send awaits, I1), but the ctx
/// that holds it lives across `drive_transport().await`, so the trait object must
/// be `Send + Sync` for the future to be `Send`.
pub(in crate::services::discord) type ConfirmedAdvance<'a> =
    &'a (dyn Fn((u64, u64)) -> bool + Send + Sync + 'a);

/// Internal RAII guard owning the controller's WON acquire, mirroring legacy
/// `SinkDeliveryLeaseGuard::Drop` (`session_relay_sink.rs:354`): the lease is
/// `release`d on EVERY exit — the normal commit/release arms AND an unwind /
/// future-cancellation between the acquire (`:480`) and the inline commit
/// (`:514` transport await). Without it a dropped/panicking `deliver_turn_output`
/// future stopped the heartbeat (its own Drop) but left the lease `Leased` until
/// the deadline — a leak the legacy guard never had (review-fix H1 r2).
///
/// `release` is full-identity-gated and valid from BOTH `Leased` (failure) and
/// `Committed` (success), so dropping the guard after a commit clears ONLY our
/// own `(holder, key, [start,end))` marker — a newer turn that re-leased the
/// cell survives, exactly as the legacy guard. `disarm` (via `release_and_disarm`)
/// is called once an explicit release has run so the Drop cannot double-release
/// (a second release is a no-op under the identity gate, but disarming keeps the
/// contract crisp). Only the held path arms it; a markerless send holds no lease.
struct ControllerLeaseGuard<'a, L: DeliveryLease + ?Sized> {
    lease: &'a L,
    holder: LeaseHolder,
    key: DeliveryLeaseKey,
    start: u64,
    end: u64,
    armed: bool,
}

impl<'a, L: DeliveryLease + ?Sized> ControllerLeaseGuard<'a, L> {
    fn arm(lease: &'a L, holder: LeaseHolder, key: DeliveryLeaseKey, start: u64, end: u64) -> Self {
        Self {
            lease,
            holder,
            key,
            start,
            end,
            armed: true,
        }
    }

    fn lease_key(&self) -> DeliveryLeaseKey {
        self.key.clone()
    }

    /// Release now and disarm so the Drop is a no-op. Used by the normal arms so
    /// the release ORDERING (AFTER `post_send_finalize`, I1) stays explicit while
    /// the Drop is the cancel/panic safety net only.
    fn release_and_disarm(&mut self) {
        if self.armed {
            self.armed = false;
            self.lease
                .release(self.holder, self.key.clone(), self.start, self.end);
        }
    }
}

impl<L: DeliveryLease + ?Sized> Drop for ControllerLeaseGuard<'_, L> {
    fn drop(&mut self) {
        if self.armed {
            self.lease
                .release(self.holder, self.key.clone(), self.start, self.end);
        }
    }
}

/// The POST-duration heartbeat the controller drives (A2a capability 3).
///
/// A1 stamped a single fixed 60s TTL at acquire and never renewed it; the sink
/// (`session_relay_sink.rs:343`) and watcher instead acquire a short
/// [`DELIVERY_LEASE_DEADLINE_MS`] (15s) and spawn a
/// [`super::super::DeliveryLeaseHeartbeat`] that `renew`s the deadline every
/// [`super::super::DELIVERY_LEASE_HEARTBEAT_MS`] while the POST is in flight,
/// `stop()`ing it BEFORE the inline commit so the renew loop can never race the
/// commit (#3151).
///
/// The controller cannot own the concrete `DeliveryLeaseHeartbeat` here because
/// it drives the lease behind the [`DeliveryLease`] trait (a borrowed `&L`, not
/// the `Arc<DeliveryLeaseCell>` the heartbeat task needs). So an owner supplies a
/// heartbeat through this trait: `start` is called right after a winning acquire
/// (returning an opaque RAII guard whose `Drop` aborts the task on any early
/// return / panic), and the guard is dropped — equivalently `stop`ped — BEFORE
/// the inline commit. The production impl (wired by owners at A2b+) is a thin
/// adapter over `DeliveryLeaseHeartbeat::spawn`; A2a's tests use a recorder that
/// counts `renew` ticks.
// `Send + Sync` (#3089 A2b): see `ConfirmedAdvance` — the sink owner drives this
// from a `Send` `async_trait` future, so the borrowed `&dyn PostHeartbeat` and
// the boxed guard held across the POST await must both be `Send`.
pub(in crate::services::discord) trait PostHeartbeat:
    Send + Sync
{
    /// Begin renewing `(holder, key)`'s lease deadline for the duration of the
    /// POST. Returns an opaque guard; dropping it stops the heartbeat (mirrors
    /// `DeliveryLeaseHeartbeat`'s `Drop`/`stop`). Called ONLY on the held-lease
    /// path (a markerless `ProceedMarkerless` send holds no lease to renew).
    fn start(&self, holder: LeaseHolder, key: DeliveryLeaseKey) -> Box<dyn PostHeartbeatGuard>;
}

/// RAII guard returned by [`PostHeartbeat::start`]. Dropping it stops the
/// heartbeat task; the controller drops it explicitly BEFORE the inline commit
/// so a last renew tick can never race the commit (#3151 ordering). `Send` so it
/// can be held across the POST await inside a `Send` future (#3089 A2b).
pub(in crate::services::discord) trait PostHeartbeatGuard:
    Send
{
}

/// Borrowed delivery context for one `deliver_turn_output` call. The controller
/// drives the borrowed [`DeliveryLeaseCell`] through acquire → send → commit →
/// release internally (I1).
pub(in crate::services::discord) struct TurnOutputCtx<
    'a,
    L: DeliveryLease + ?Sized = DeliveryLeaseCell,
> {
    #[allow(dead_code)] // Transport/finalizer metadata; lease identity is `lease_key`.
    pub(in crate::services::discord) turn: TurnKey,
    /// Canonical delivery-lease identity for real lease owners. `None` is reserved
    /// for transport-only / markerless paths such as [`NoLease`].
    pub(in crate::services::discord) lease_key: Option<DeliveryLeaseKey>,
    /// Durable relay-owner identity carried for the durable-lease join (Phase
    /// B) and owner-scoped routing at cutover (A2); not read by the A1
    /// skeleton itself.
    #[allow(dead_code)] // #3089 A1: read by owner routing / durable lease from A2/B.
    pub(in crate::services::discord) owner: RelayOwnerKind,
    pub(in crate::services::discord) holder: LeaseHolder,
    pub(in crate::services::discord) lease: &'a L,
    pub(in crate::services::discord) channel_id: ChannelId,
    /// The shared placeholder lifecycle controller (#1255). The turn-output
    /// controller drives a live placeholder card to its terminal state through
    /// `PlaceholderController.transition` (design §5 A1: "Wires
    /// `PlaceholderController.transition`"), so A2+ owners reuse the same FSM /
    /// edit-coalescer instead of raw-editing the card.
    pub(in crate::services::discord) placeholder_controller: &'a PlaceholderController,
    pub(in crate::services::discord) placeholder: PlaceholderSlot,
    pub(in crate::services::discord) body: &'a str,
    pub(in crate::services::discord) send_range: (u64, u64),
    pub(in crate::services::discord) plan: OutputPlan,
    /// Explicit per-owner edit-fail fallback policy; NO default (#2757 fence).
    pub(in crate::services::discord) edit_fail_policy: EditFailPlaceholderPolicy,
    /// Explicit per-owner advance policy for `SentFallbackAfterEditFailure`; NO
    /// default. The sink/standby advance on fallback POST; turn_bridge does not
    /// (see [`FallbackCommitPolicy`]). The controller must not hard-code one.
    pub(in crate::services::discord) fallback_commit_policy: FallbackCommitPolicy,
    /// A2a capability 1: what to do when the lease acquire FAILS. NO default —
    /// the sink (`ProceedMarkerless`) and watcher/bridge (`Transient`) diverge.
    pub(in crate::services::discord) acquire_failure_mode: AcquireFailureMode,
    /// A2a capability 2: owner-provided, identity-gated advance callback. `Some`
    /// → the controller delegates the advance decision (and commits
    /// `Delivered`/`NotDelivered` accordingly); `None` → A1 semantics
    /// (unconditional `Delivered` advance, for owners with no identity gate).
    /// Invoked INLINE before any post-send await (I1), only on confirmed
    /// transport (never on Transient/Unknown — I2).
    pub(in crate::services::discord) advance: Option<ConfirmedAdvance<'a>>,
    /// A2a capability 3: owner-provided POST-duration heartbeat. `Some` → the
    /// controller renews the lease deadline while the POST is in flight and stops
    /// the heartbeat before the inline commit (#3151); `None` → A1 behaviour (the
    /// single acquire deadline is the only liveness signal). Only the held-lease
    /// path renews; a `ProceedMarkerless` send holds no lease.
    pub(in crate::services::discord) heartbeat: Option<&'a dyn PostHeartbeat>,
}

/// Deliver one turn's output through the single controller path.
///
/// Commit+advance happen INSIDE this fn (I1), synchronously, immediately after
/// confirmed transport success and before any post-send await; the returned
/// outcome is already committed. An ambiguous transport result releases the
/// lease without committing (I2).
///
/// A1 is a pure add — no live owner calls this yet (cutover starts at A2).
#[allow(dead_code)] // #3089 A1: pure add; owners wired from A2.
pub(in crate::services::discord) async fn deliver_turn_output<G, L>(
    gateway: &G,
    ctx: TurnOutputCtx<'_, L>,
) -> DeliveryOutcome
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    if matches!(&ctx.plan, OutputPlan::SendFresh { .. }) {
        return fresh_send::deliver(gateway, ctx).await;
    }

    let (start, end) = ctx.send_range;

    // NoOp short-circuits before touching the lease — nothing to deliver.
    let chunk_count = match &ctx.plan {
        OutputPlan::NoOp => return DeliveryOutcome::Skipped,
        OutputPlan::SendFresh { .. } => unreachable!("fresh-send dispatches to its child module"),
        OutputPlan::Replace { .. } => 1usize,
        OutputPlan::SendNewChunks { chunk_count, .. } => *chunk_count,
    };
    if ctx.body.is_empty() {
        return DeliveryOutcome::Skipped;
    }

    // ---- acquire ---------------------------------------------------------
    // A2a: acquire with the shared HOLDER-LIVENESS deadline (15s); the POST
    // heartbeat (below) keeps it fresh — no fixed 60s TTL.
    let deadline_ms = lease_now_ms().saturating_add(TURN_OUTPUT_LEASE_TTL_MS);
    let held_lease_key = match ctx.lease_key.as_ref() {
        Some(key) => ctx
            .lease
            .try_acquire(key.clone(), ctx.holder, start, end, deadline_ms)
            .then_some(key),
        _ => None,
    };
    if held_lease_key.is_none() {
        // A2a capability 1: another holder owns this (channel, turn, range).
        match ctx.acquire_failure_mode {
            AcquireFailureMode::Transient => {
                // Watcher/bridge: do not send; the owner retries.
                return DeliveryOutcome::Transient {
                    retry_from_offset: start,
                };
            }
            AcquireFailureMode::ProceedMarkerless => {
                // Sink (`session_relay_sink.rs:777-795`): POST WITHOUT a marker
                // and WITHOUT a heartbeat — fall through to the send below. We
                // hold no lease, so the markerless path never commits/releases.
            }
        }
    }

    // RAII lease guard (review-fix H1 r2): arm it the instant the acquire wins,
    // so a future drop / panic during the transport await below releases the
    // lease just like the legacy `SinkDeliveryLeaseGuard` — no leak-until-deadline.
    // `None` on the markerless path (it holds no lease). The normal arms call
    // `release_and_disarm()` so the release ORDERING stays explicit (after the
    // inline commit + post-send finalize, I1) while the Drop is the cancel/panic
    // safety net only.
    let mut lease_guard = held_lease_key
        .map(|key| ControllerLeaseGuard::arm(ctx.lease, ctx.holder, key.clone(), start, end));

    // A2a capability 3: while the POST is in flight, keep the (held) lease
    // deadline fresh. Only the held-lease path has a lease to renew; a
    // markerless send holds none. The guard's Drop stops the heartbeat, so an
    // early return / panic in `drive_transport` can never leak the renew task;
    // it is also dropped explicitly BEFORE the inline commit (#3151 ordering).
    let heartbeat_guard =
        held_lease_key.and_then(|key| ctx.heartbeat.map(|hb| hb.start(ctx.holder, key.clone())));

    // ---- send (transport) ------------------------------------------------
    // Any post-send work (placeholder terminal transition, fallback cleanup,
    // release) happens AFTER the inline commit below (I1).
    let transport = drive_transport(gateway, &ctx, chunk_count).await;

    match transport {
        TransportResult::Delivered {
            replace_kind,
            new_chunks,
        } => {
            // ---- I1: commit + advance INLINE, before any post-send await --
            // Stop the heartbeat FIRST (#3151) so its renew loop cannot race the
            // commit, THEN run the single commit+advance authority. The advance
            // is DELEGATED to the owner callback (A2a capability 2); its bool
            // decides Delivered vs NotDelivered. The whole commit+finalize lives
            // in one fn so every commit is structurally trailed by a post-send
            // gateway await a recorder can witness (I1 / review-fix M4).
            // `replace_kind` is threaded onto the returned `Delivered` so the owner
            // can mirror the legacy per-variant cleanup (#3089 A4 r2).
            drop(heartbeat_guard);
            commit_and_finalize(
                gateway,
                &ctx,
                start,
                end,
                lease_guard.as_mut(),
                replace_kind,
                new_chunks,
            )
            .await
        }
        TransportResult::NotDelivered => {
            // Long terminal chunk send failed after the rollback-aware sender
            // reported a clean failure. Legacy A4/A5 commit `NotDelivered` for
            // this arm (no advance, anchor preserved) rather than leaving the
            // range ambiguous.
            drop(heartbeat_guard);
            commit_not_delivered_and_release(&ctx, start, end, lease_guard.as_mut())
        }
        TransportResult::Transient => {
            // I2: ambiguous-but-retriable. Do NOT commit/advance — release the
            // (held) lease so a retry can re-acquire from `start`. No commit
            // happens on this arm: it never calls `commit_and_finalize`.
            drop(heartbeat_guard);
            if let Some(guard) = lease_guard.as_mut() {
                guard.release_and_disarm();
            }
            DeliveryOutcome::Transient {
                retry_from_offset: start,
            }
        }
        TransportResult::PermanentFailure => {
            // Permanent watcher transport failure: release without commit/advance
            // and return the owner's no-op/no-retry outcome.
            drop(heartbeat_guard);
            if let Some(guard) = lease_guard.as_mut() {
                guard.release_and_disarm();
            }
            DeliveryOutcome::Skipped
        }
        TransportResult::Unknown { fell_back } => {
            // I2: ambiguous — release WITHOUT commit; carry `fell_back` (#3089 A5).
            drop(heartbeat_guard);
            if let Some(guard) = lease_guard.as_mut() {
                guard.release_and_disarm();
            }
            DeliveryOutcome::Unknown { fell_back }
        }
    }
}

/// The SINGLE commit+advance authority (I1). Runs the owner's identity-gated
/// advance (A2a capability 2), commits the lease with the matching outcome
/// (`Delivered` when the advance succeeded, `NotDelivered` when the owner's gate
/// refused), then runs the post-send finalize await and releases — in that fixed
/// order. Returns the corresponding [`DeliveryOutcome`].
///
/// Keeping the advance + commit + trailing finalize/release in one fn means
/// every successful `commit` is structurally paired with a post-send gateway
/// await, so a mutation that commits on the ambiguous arm (Transient/Unknown) is
/// always visible to a gateway-side commit recorder (review-fix M4: no silent
/// commit-then-release).
///
/// `lease_guard` is `Some` on the held path (sink-acquire won, or
/// watcher/bridge) and `None` on the markerless `ProceedMarkerless` path where
/// the acquire LOST: with no lease there is nothing to commit/release, but the
/// owner's identity-gated advance still runs (the marker only gated the watcher;
/// the advance authority was always the identity gate, not the lease). The held
/// path commits through the same `(holder, key, range)` the guard owns, then
/// releases via the guard AFTER `post_send_finalize` (so its Drop stays the
/// cancel/panic safety net without double-releasing).
async fn commit_and_finalize<G, L>(
    gateway: &G,
    ctx: &TurnOutputCtx<'_, L>,
    start: u64,
    end: u64,
    lease_guard: Option<&mut ControllerLeaseGuard<'_, L>>,
    replace_kind: Option<ReplaceDeliveryKind>,
    new_chunks: Option<NewChunksDelivery>,
) -> DeliveryOutcome
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    // A2a capability 2: delegate the advance decision. `None` → A1 semantics
    // (unconditional advance: owners with no identity gate). `Some(cb)` → the
    // owner's identity gate runs HERE, synchronously, BEFORE the post-send awaits
    // below, so a post-send await can never land before the advance (#3143).
    // This mirrors the sink running `advance_after_confirmed_post` inline before
    // the marker clears (`session_relay_sink.rs:560-577`).
    let advanced = match ctx.advance {
        Some(advance) => advance((start, end)),
        None => true,
    };

    // commit() verifies the full (holder, key, range) identity and records the
    // outcome. On the advanced arm the committed frontier moves to `end`; on the
    // refused arm we commit `NotDelivered` so the owner's committed-offset
    // reconciliation re-sends (the sink's `advanced == false` arm). On the
    // markerless path there is no lease to commit — the advance bool alone
    // decides the outcome. Runs synchronously here, BEFORE the post-send awaits.
    if let Some(guard) = lease_guard.as_ref() {
        let outcome = if advanced {
            LeaseOutcome::Delivered
        } else {
            LeaseOutcome::NotDelivered
        };
        let committed = ctx
            .lease
            .commit(ctx.holder, guard.lease_key(), start, end, outcome);
        debug_assert!(committed, "confirmed commit must match the acquired lease");
    }

    // ---- post-send work (AFTER the inline commit) -----------------------
    post_send_finalize(gateway, ctx).await;
    if let Some(guard) = lease_guard {
        // I1 release ordering: AFTER the post-send finalize. Disarms so the
        // guard's Drop (cancel/panic safety net) cannot double-release.
        guard.release_and_disarm();
    }

    if advanced {
        DeliveryOutcome::Delivered {
            committed_to: end,
            // Surface the replace identity so the owner mirrors the legacy
            // per-variant cleanup (#3089 A4 r2). `None` on a refused advance
            // (`NotDelivered` has no replace identity to carry) and for non-replace
            // plans. The advance refusal drops `replace_kind` on the floor because
            // the body was NOT committed — there is no delivered original to footer.
            replace_kind,
            new_chunks,
        }
    } else {
        DeliveryOutcome::NotDelivered {
            committed_from: start,
        }
    }
}

/// Commit a clean non-delivery result for rollback-aware long-chunk sends. This
/// mirrors the legacy terminal long-chunk arms: failed send ⇒ commit
/// `NotDelivered`, do not advance, do not delete the placeholder anchor.
fn commit_not_delivered_and_release<L>(
    ctx: &TurnOutputCtx<'_, L>,
    start: u64,
    end: u64,
    lease_guard: Option<&mut ControllerLeaseGuard<'_, L>>,
) -> DeliveryOutcome
where
    L: DeliveryLease + ?Sized,
{
    if let Some(guard) = lease_guard.as_ref() {
        let committed = ctx.lease.commit(
            ctx.holder,
            guard.lease_key(),
            start,
            end,
            LeaseOutcome::NotDelivered,
        );
        debug_assert!(
            committed,
            "confirmed NotDelivered commit must match the acquired lease"
        );
    }
    if let Some(guard) = lease_guard {
        guard.release_and_disarm();
    }
    DeliveryOutcome::NotDelivered {
        committed_from: start,
    }
}

/// Internal three-way transport result, before any lease commit.
///
/// A1's conservative classifier only ever produced `Delivered`/`Unknown`; the
/// extra arms are used by owners that bring a real transport-error taxonomy.
#[allow(dead_code)] // #3089 A1: Transient arm dormant until A2 transport taxonomy.
enum TransportResult {
    /// Confirmed delivered. `Option<ReplaceDeliveryKind>` carries the replace
    /// identity (edit-in-place vs fresh fallback) for `Replace` plans so the
    /// owner write-back can mirror the legacy per-variant cleanup (#3089 A4 r2);
    /// `None` for `NewSend` / chunked / NoOp. `new_chunks` carries the tail
    /// chunk + anchor-delete metadata for confirmed long-chunk sends.
    Delivered {
        replace_kind: Option<ReplaceDeliveryKind>,
        new_chunks: Option<NewChunksDelivery>,
    },
    /// Clean non-delivery for rollback-aware long-chunk sends. The owner wants a
    /// committed `NotDelivered` lease result (retryable, no advance), not an
    /// ambiguous `Unknown`.
    NotDelivered,
    Transient,
    PermanentFailure,
    /// Ambiguous, never advance (I2). `fell_back` (#3089 A5): see
    /// [`DeliveryOutcome::Unknown`] — true only on NoCommitOnFallback fresh-fallback.
    Unknown {
        fell_back: bool,
    },
}

/// Drive the gateway transport for the plan. Returns ONLY the transport
/// outcome — it never touches the lease, so the inline commit in the caller is
/// the single advance authority (I1).
async fn drive_transport<G, L>(
    gateway: &G,
    ctx: &TurnOutputCtx<'_, L>,
    chunk_count: usize,
) -> TransportResult
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    match (&ctx.plan, &ctx.placeholder) {
        (OutputPlan::Replace { .. }, PlaceholderSlot::Active { message_id, .. }) => {
            match gateway
                .replace_message_with_outcome(ctx.channel_id, *message_id, ctx.body)
                .await
            {
                Ok(outcome) => classify_replace_outcome(&outcome, &ctx.fallback_commit_policy),
                Err(error) => classify_transport_failure(ctx, &error),
            }
        }
        // Replace requested but no live placeholder to edit → fall back to a
        // fresh send of the single inline body. No replace identity to surface
        // (there was no original placeholder to edit) → `None`.
        (OutputPlan::Replace { .. }, PlaceholderSlot::None) => {
            match gateway.send_message(ctx.channel_id, ctx.body).await {
                Ok(_) => TransportResult::Delivered {
                    replace_kind: None,
                    new_chunks: None,
                },
                Err(error) => classify_transport_failure(ctx, &error),
            }
        }
        (OutputPlan::SendNewChunks { delete_anchor, .. }, slot) => {
            let anchor = match slot {
                PlaceholderSlot::Active { message_id, .. } => *message_id,
                PlaceholderSlot::None => MessageId::new(1),
            };
            match gateway
                .send_long_message_with_rollback(ctx.channel_id, anchor, ctx.body)
                .await
            {
                // A Split body MUST land all `chunk_count` messages to be
                // Delivered. A short write (fewer IDs than chunks) is a PARTIAL
                // send — ambiguous — and must NEVER advance (I2, review-fix H1).
                // `chunk_count` is always >= 1 (exact-or-more contract). Chunked
                // sends carry no replace identity → `None`.
                Ok(ids) if ids.len() >= chunk_count => {
                    let anchor_delete_error = if *delete_anchor {
                        delete_active_anchor_after_chunks(gateway, ctx, slot).await
                    } else {
                        None
                    };
                    TransportResult::Delivered {
                        replace_kind: None,
                        new_chunks: Some(NewChunksDelivery {
                            first_message_id: ids.first().copied(),
                            tail_message_id: ids.last().copied(),
                            anchor_delete_error,
                        }),
                    }
                }
                // Short chunked write: ambiguous, nothing fell back (#3089 A5).
                Ok(_) => TransportResult::Unknown { fell_back: false },
                Err(_) if *delete_anchor => TransportResult::NotDelivered,
                Err(error) => classify_transport_failure(ctx, &error),
            }
        }
        (OutputPlan::SendFresh { .. }, _) => {
            unreachable!("fresh-send transport is owned by the child module")
        }
        (OutputPlan::NoOp, _) => TransportResult::Delivered {
            replace_kind: None,
            new_chunks: None,
        },
    }
}

async fn delete_active_anchor_after_chunks<G, L>(
    gateway: &G,
    ctx: &TurnOutputCtx<'_, L>,
    slot: &PlaceholderSlot,
) -> Option<String>
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    if let PlaceholderSlot::Active { message_id, .. } = slot {
        if let Err(error) = gateway.delete_message(ctx.channel_id, *message_id).await {
            tracing::warn!(
                channel_id = ctx.channel_id.get(),
                message_id = message_id.get(),
                error = %error,
                "long chunk delivery succeeded but anchor delete failed; proceeding as delivered"
            );
            return Some(error);
        }
    }
    None
}

/// Map a `replace_message_with_outcome` success into the controller's transport
/// classification, mirroring the EXACT semantics each owner gives each
/// `ReplaceLongMessageOutcome` variant (review-fix H2 — the catch-all
/// `Ok(_) => Delivered` was wrong: `PartialContinuationFailure` never advances).
///
/// Owner-mapping evidence:
/// - `EditedOriginal` → delivered for EVERY owner:
///   `session_relay_sink.rs:863`, `standby_relay.rs:653`,
///   `turn_bridge/terminal_delivery.rs:131` (committed = true) + predicate `:42`,
///   `formatting.rs:1785` (`Ok(())`).
/// - `SentFallbackAfterEditFailure` → owner-SPECIFIC (review-fix H1 r3): the sink
///   advances (`session_relay_sink.rs:905`) and standby advances
///   (`standby_relay.rs:662`), but turn_bridge does NOT
///   (`terminal_delivery.rs:143` returns `committed = false`; predicate `:42`
///   commits `EditedOriginal` only). The controller consults the owner-passed
///   `FallbackCommitPolicy`: `CommitOnFallback` → `Delivered`,
///   `NoCommitOnFallback` → `Unknown { fell_back: true }` (#3089 A5).
/// - `PartialContinuationFailure` → ambiguous, NEVER advance (I2):
///   `session_relay_sink.rs:956`, `standby_relay.rs:678`,
///   `turn_bridge/terminal_delivery.rs:155` (committed = false), `formatting.rs:1787`.
fn classify_replace_outcome(
    outcome: &crate::services::discord::formatting::ReplaceLongMessageOutcome,
    fallback_commit_policy: &FallbackCommitPolicy,
) -> TransportResult {
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    match outcome {
        // Edited in place → carry the `EditedOriginal` replace identity so the
        // owner takes its delivered side-effects (footer register, Succeeded).
        ReplaceLongMessageOutcome::EditedOriginal => TransportResult::Delivered {
            replace_kind: Some(ReplaceDeliveryKind::EditedOriginal),
            new_chunks: None,
        },
        // Owner-specific (H1 r3): the edit failed but a fallback POST carried the
        // body. Honour the owner's `FallbackCommitPolicy` (sink/standby advance;
        // turn_bridge does not). On the committing arm carry the
        // `FreshFallbackAfterEditFailure { edit_error, replacement_anchor }`
        // identity (#3089 A4 r2 + D1) so the watcher mirrors the legacy fallback
        // cleanup and recovery can durably bind a stale-anchor fallback POST.
        ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
            edit_error,
            replacement_anchor,
        } => {
            match fallback_commit_policy {
                FallbackCommitPolicy::CommitOnFallback => TransportResult::Delivered {
                    replace_kind: Some(ReplaceDeliveryKind::FreshFallbackAfterEditFailure {
                        edit_error: edit_error.clone(),
                        replacement_anchor: *replacement_anchor,
                    }),
                    new_chunks: None,
                },
                // #3089 A5: edit FAILED but fallback POST landed the body → no
                // advance + `fell_back = true` (see `DeliveryOutcome::Unknown`).
                FallbackCommitPolicy::NoCommitOnFallback => {
                    TransportResult::Unknown { fell_back: true }
                }
            }
        }
        // Partial continuation failure: never advance (I2); `fell_back = false`
        // (nothing landed → no bump, #3089 A5).
        ReplaceLongMessageOutcome::PartialContinuationFailure { .. } => {
            TransportResult::Unknown { fell_back: false }
        }
    }
}

/// Classify a transport error into the ambiguous halves. A1 keeps the rule
/// conservative (design I3): anything we cannot prove transient is `Unknown` so
/// the offset never advances (the edit-fail policy only affects post-send cleanup).
fn classify_transport_failure<L: DeliveryLease + ?Sized>(
    ctx: &TurnOutputCtx<'_, L>,
    error: &str,
) -> TransportResult {
    let class = classify_watcher_send_failure_message(error);
    if ctx.owner == RelayOwnerKind::Watcher
        && matches!(
            class,
            WatcherSendFailureClass::Permanent | WatcherSendFailureClass::RollbackIncomplete
        )
    {
        let display_error = strip_watcher_send_failure_class_marker(error);
        tracing::warn!(
            channel_id = ctx.channel_id.get(),
            owner = ?ctx.owner,
            failure_class = class.as_str(),
            error = %display_error,
            "turn-output controller: permanent watcher transport failure will not retry"
        );
        return TransportResult::PermanentFailure;
    }
    // Unknown keeps the existing retry/no-advance owner behavior for transient
    // watcher transport failures and for non-watcher owners.
    TransportResult::Unknown { fell_back: false }
}

/// Post-send finalization: placeholder terminal transition + edit-fail
/// fallback cleanup. Runs ONLY after the inline commit (I1). Best-effort —
/// failures here never un-advance the already-committed offset.
///
/// This is an `async` step with a real post-send await
/// (`PlaceholderController.transition`, which internally awaits an
/// `edit_message`) — the very kind of await I1 forbids the commit from landing
/// AFTER. The controller calls it only once the inline commit above has already
/// advanced the offset, so this await can never re-open #3143.
///
/// Design §5 A1 ("Wires `PlaceholderController.transition`"): the card is driven
/// to its terminal state through the shared `PlaceholderController` FSM /
/// edit-coalescer, NOT a raw `edit_message`, so A2+ owners do not have to redo
/// this API. `EditFailPlaceholderPolicy` governs the #2757 fence on
/// `EditFailed`.
async fn post_send_finalize<G, L>(gateway: &G, ctx: &TurnOutputCtx<'_, L>)
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    if let (OutputPlan::Replace { lifecycle }, PlaceholderSlot::Active { message_id, key }) =
        (&ctx.plan, &ctx.placeholder)
    {
        // Only terminal targets are valid `transition` inputs; a non-terminal
        // `lifecycle` (e.g. Active) is left untouched here.
        if !matches!(
            lifecycle,
            PlaceholderLifecycle::Completed
                | PlaceholderLifecycle::TimedOut
                | PlaceholderLifecycle::Aborted
        ) {
            return;
        }

        // Drive the card to its terminal state through the shared controller
        // FSM. `transition` performs the post-send PATCH (with the controller's
        // own bounded edit-retry) and reports the lifecycle-aware outcome.
        let outcome = ctx
            .placeholder_controller
            .transition(gateway, key.clone(), *lifecycle)
            .await;

        // Only a hard `EditFailed` (Discord PATCH attempted and failed) engages
        // the #2757 fence. `Edited` / `Coalesced` / `AlreadyTerminal` /
        // `Rejected` are all non-failure terminations (no live PATCH error), so
        // they never delete the original.
        if matches!(outcome, PlaceholderControllerOutcome::EditFailed) {
            match ctx.edit_fail_policy {
                EditFailPlaceholderPolicy::DeleteIfProvenStale => {
                    // Watcher's conditional-delete arm: the edit failed, so the
                    // original placeholder may be stale; delete it.
                    let _ = gateway.delete_message(ctx.channel_id, *message_id).await;
                }
                EditFailPlaceholderPolicy::PreserveAlways => {
                    // #2757: sink/standby preserve the original — a transient
                    // edit failure must never remove already-streamed body.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::LeaseSnapshot;
    use crate::services::discord::formatting::{MonitorHandoffReason, ReplaceLongMessageOutcome};
    use crate::services::discord::gateway::GatewayFuture;
    use crate::services::discord::placeholder_controller::PlaceholderActiveInput;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    /// M4 commit recorder: a `DeliveryLease` that wraps the real
    /// `DeliveryLeaseCell` and counts EVERY `commit` and `release` call (with
    /// the committed outcome). Because the controller takes the lease behind the
    /// `DeliveryLease` trait, this records commits the instant they happen —
    /// independent of any gateway await — so even a silent "commit then
    /// immediately release" mutation on the ambiguous arm is caught.
    struct RecordingLease {
        inner: DeliveryLeaseCell,
        commit_calls: AtomicUsize,
        delivered_commit_calls: AtomicUsize,
        not_delivered_commit_calls: AtomicUsize,
        release_calls: AtomicUsize,
        renew_calls: AtomicUsize,
        /// A2a #3151 ordering: when a test attaches the gateway's shared step
        /// clock (`attach_clock`), the FIRST `commit` call stamps its step here.
        /// Unlike `ObservingGateway::first_commit_step` (only set when a later
        /// gateway await *observes* a `Committed` lease), this records the step
        /// of the actual `commit` *call*, so a test can prove the heartbeat
        /// guard's `Drop` (also stamped on the same clock) precedes the real
        /// commit — independent of any post-send await. `None` clock (the
        /// default) leaves `commit_step` at 0, so non-heartbeat tests are
        /// unaffected.
        clock: std::sync::Mutex<Option<Arc<AtomicUsize>>>,
        commit_step: AtomicUsize,
    }

    impl RecordingLease {
        fn new(channel: ChannelId) -> Self {
            Self {
                inner: DeliveryLeaseCell::new(channel),
                commit_calls: AtomicUsize::new(0),
                delivered_commit_calls: AtomicUsize::new(0),
                not_delivered_commit_calls: AtomicUsize::new(0),
                release_calls: AtomicUsize::new(0),
                renew_calls: AtomicUsize::new(0),
                clock: std::sync::Mutex::new(None),
                commit_step: AtomicUsize::new(0),
            }
        }

        /// Share the gateway's monotonic step clock so the actual `commit` call
        /// is stamped on the SAME clock the heartbeat guard's `Drop` uses,
        /// letting a test assert `drop_step < commit_step` directly.
        fn attach_clock(&self, clock: Arc<AtomicUsize>) {
            *self.clock.lock().unwrap() = Some(clock);
        }
    }

    impl DeliveryLease for RecordingLease {
        fn try_acquire(
            &self,
            key: DeliveryLeaseKey,
            holder: LeaseHolder,
            start: u64,
            end: u64,
            deadline_ms: u64,
        ) -> bool {
            self.inner.try_acquire(key, holder, start, end, deadline_ms)
        }
        fn commit(
            &self,
            holder: LeaseHolder,
            key: DeliveryLeaseKey,
            start: u64,
            end: u64,
            outcome: LeaseOutcome,
        ) -> bool {
            // #3151: stamp the actual commit-call step on the shared clock the
            // FIRST time commit runs (only when a test attached the clock). This
            // measures when the commit truly happens — not when a later gateway
            // await observes the committed lease — so the heartbeat ordering
            // assertion (drop_step < commit_step) cannot be fooled by a mutation
            // that drops the guard after the commit but before the post-send await.
            if self.commit_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                if let Some(clock) = self.clock.lock().unwrap().as_ref() {
                    self.commit_step
                        .store(clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                }
            }
            match outcome {
                LeaseOutcome::Delivered => {
                    self.delivered_commit_calls.fetch_add(1, Ordering::SeqCst);
                }
                LeaseOutcome::NotDelivered => {
                    self.not_delivered_commit_calls
                        .fetch_add(1, Ordering::SeqCst);
                }
                LeaseOutcome::Unknown => {}
            }
            self.inner.commit(holder, key, start, end, outcome)
        }
        fn release(
            &self,
            holder: LeaseHolder,
            key: DeliveryLeaseKey,
            start: u64,
            end: u64,
        ) -> bool {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.release(holder, key, start, end)
        }
        fn renew(&self, holder: LeaseHolder, key: DeliveryLeaseKey, new_deadline_ms: u64) -> bool {
            self.renew_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.renew(holder, key, new_deadline_ms)
        }
        fn reclaim_if_expired(&self, now_ms: u64) -> bool {
            self.inner.reclaim_if_expired(now_ms)
        }
        fn read(&self) -> LeaseSnapshot {
            self.inner.read()
        }
    }

    fn turn_key(channel_id: ChannelId) -> TurnKey {
        TurnKey::new(channel_id, 7, 1)
    }

    fn lease_key(channel_id: ChannelId) -> DeliveryLeaseKey {
        DeliveryLeaseKey::from_turn_key(turn_key(channel_id))
    }

    fn placeholder_key(channel_id: ChannelId, message_id: MessageId) -> PlaceholderKey {
        PlaceholderKey {
            provider: ProviderKind::Claude,
            channel_id,
            message_id,
        }
    }

    fn active_input() -> PlaceholderActiveInput {
        PlaceholderActiveInput {
            reason: MonitorHandoffReason::ExplicitCall,
            started_at_unix: 1_700_000_000,
            tool_summary: None,
            command_summary: None,
            reason_detail: None,
            context_line: None,
            request_line: None,
            progress_line: None,
        }
    }

    /// Prime the controller so the placeholder `key` is already `Active` (has an
    /// `active_snapshot`). Without this, `transition` short-circuits to
    /// `Rejected` and never performs its post-send `edit_message` await, so the
    /// I1 ordering observation could not run. The `ensure_active` itself drives
    /// one `edit_message`; callers reset the gateway observations afterward so
    /// the delivery-under-test observations start clean.
    async fn prime_active(
        controller: &PlaceholderController,
        gateway: &ObservingGateway,
        key: PlaceholderKey,
    ) {
        let outcome = controller.ensure_active(gateway, key, active_input()).await;
        assert_eq!(
            outcome,
            PlaceholderControllerOutcome::Edited,
            "prime_active must put the card into Active via an edit"
        );
        gateway.reset_observations();
    }

    /// A fake `TurnGateway` that SHARES the same `DeliveryLeaseCell` the
    /// controller drives (via `Arc`), so each gateway method can READ the lease
    /// state at the exact moment the controller awaits it. This is what lets us
    /// prove I1 without any unsafe pointer: the transport-send method observes
    /// the lease BEFORE the inline commit, and the post-send `edit_message`
    /// await observes it AFTER.
    struct ObservingGateway {
        /// Shared with the controller (same object) so a gateway await reads the
        /// exact lease state the controller is driving. Held behind the
        /// `DeliveryLease` trait so the recorder-wrapping `RecordingLease` and
        /// the bare `DeliveryLeaseCell` are interchangeable here.
        lease: Arc<dyn DeliveryLease + Send + Sync>,
        /// step counter — proves the temporal order of the observations. Held in
        /// an `Arc` so A2a owner-callback / heartbeat recorders can share the SAME
        /// step clock and order their events against the send / post-send-await
        /// observations (`clock_handle`).
        clock: Arc<AtomicUsize>,
        /// snapshot tag observed inside the transport send call (expected
        /// `Leased`: commit has NOT happened yet).
        committed_at_send: AtomicBool,
        send_step: AtomicUsize,
        /// snapshot tag observed inside the FIRST post-send await
        /// (`edit_message`) (expected `Committed{Delivered}`: the inline commit
        /// already ran).
        committed_at_post_send_await: AtomicBool,
        post_send_await_step: AtomicUsize,
        post_send_await_seen: AtomicBool,
        /// ---- M4 mutation-sensitive commit recorder ----
        /// Every gateway method await observes the shared lease and, the FIRST
        /// time it ever reads a `Committed` state (ANY outcome), records the
        /// step + outcome here. Because the controller's single commit
        /// authority always pairs `commit` with a trailing post-send gateway
        /// await before `release`, a mutation that commits on the ambiguous
        /// (Transient/Unknown) arm makes a `Committed` lease visible to a
        /// gateway await — flipping `commit_count` above 0 and failing the I2
        /// test. A genuine non-advance run never sees a `Committed` lease.
        commit_count: AtomicUsize,
        first_commit_step: AtomicUsize,
        first_commit_was_delivered: AtomicBool,
        /// when false, the transport send returns Err (drives the I2 path).
        transport_ok: bool,
        /// The `ReplaceLongMessageOutcome` returned by `replace_message_with_outcome`
        /// when `transport_ok` (so H2 tests can drive `PartialContinuationFailure`).
        replace_outcome: ReplaceLongMessageOutcome,
        /// When true, `edit_message` returns a PERMANENT error so the
        /// placeholder `transition` reports `EditFailed` (drives the M3
        /// EditFailPlaceholderPolicy arms). Set AFTER `prime_active` so the prime
        /// edit still succeeds.
        edit_fails: AtomicBool,
        /// Count of `delete_message` calls (the DeleteIfProvenStale arm).
        delete_calls: AtomicUsize,
        delete_step: AtomicUsize,
        committed_at_delete: AtomicBool,
        delete_fails: AtomicBool,
    }

    impl ObservingGateway {
        fn new(lease: Arc<dyn DeliveryLease + Send + Sync>, transport_ok: bool) -> Self {
            Self {
                lease,
                clock: Arc::new(AtomicUsize::new(1)),
                committed_at_send: AtomicBool::new(false),
                send_step: AtomicUsize::new(0),
                committed_at_post_send_await: AtomicBool::new(false),
                post_send_await_step: AtomicUsize::new(0),
                post_send_await_seen: AtomicBool::new(false),
                commit_count: AtomicUsize::new(0),
                first_commit_step: AtomicUsize::new(0),
                first_commit_was_delivered: AtomicBool::new(false),
                transport_ok,
                replace_outcome: ReplaceLongMessageOutcome::EditedOriginal,
                edit_fails: AtomicBool::new(false),
                delete_calls: AtomicUsize::new(0),
                delete_step: AtomicUsize::new(0),
                committed_at_delete: AtomicBool::new(false),
                delete_fails: AtomicBool::new(false),
            }
        }

        /// After this is called, `edit_message` fails permanently so the
        /// placeholder `transition` reports `EditFailed` (M3 policy arms).
        fn fail_edits_from_now(&self) {
            self.edit_fails.store(true, Ordering::SeqCst);
        }

        /// A clone of the shared step clock so an A2a owner-callback / heartbeat
        /// recorder can stamp its events on the SAME monotonic step counter the
        /// gateway uses — letting a test order the advance call / heartbeat drop
        /// against the transport send and post-send await observations.
        fn clock_handle(&self) -> Arc<AtomicUsize> {
            self.clock.clone()
        }

        /// Drive a specific replace outcome on the transport-ok path (H2).
        fn with_replace_outcome(mut self, outcome: ReplaceLongMessageOutcome) -> Self {
            self.replace_outcome = outcome;
            self
        }

        /// Clear all observation counters. Used after a `prime_active`
        /// `ensure_active` (which itself drives an `edit_message`) so the
        /// `deliver_turn_output`-under-test observations start from zero.
        fn reset_observations(&self) {
            self.clock.store(1, Ordering::SeqCst);
            self.committed_at_send.store(false, Ordering::SeqCst);
            self.send_step.store(0, Ordering::SeqCst);
            self.committed_at_post_send_await
                .store(false, Ordering::SeqCst);
            self.post_send_await_step.store(0, Ordering::SeqCst);
            self.post_send_await_seen.store(false, Ordering::SeqCst);
            self.commit_count.store(0, Ordering::SeqCst);
            self.first_commit_step.store(0, Ordering::SeqCst);
            self.first_commit_was_delivered
                .store(false, Ordering::SeqCst);
            self.delete_calls.store(0, Ordering::SeqCst);
            self.delete_step.store(0, Ordering::SeqCst);
            self.committed_at_delete.store(false, Ordering::SeqCst);
            self.delete_fails.store(false, Ordering::SeqCst);
        }

        fn lease_is_committed_delivered(&self) -> bool {
            matches!(
                self.lease.read(),
                LeaseSnapshot::Committed {
                    outcome: LeaseOutcome::Delivered,
                    ..
                }
            )
        }

        /// M4 recorder: call at the head of EVERY gateway await. If the shared
        /// lease is in ANY `Committed` state, record that a commit is observable
        /// at this await (step + whether the outcome was `Delivered`). This is
        /// the test-only commit recorder hung on the gateway (the lease cell
        /// itself is frozen #3041 code, so the recorder lives here).
        fn observe_lease_for_commit(&self) {
            if let LeaseSnapshot::Committed { outcome, .. } = self.lease.read() {
                let step = self.clock.fetch_add(1, Ordering::SeqCst);
                if self.commit_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.first_commit_step.store(step, Ordering::SeqCst);
                    self.first_commit_was_delivered
                        .store(outcome == LeaseOutcome::Delivered, Ordering::SeqCst);
                }
            }
        }
    }

    impl TurnGateway for ObservingGateway {
        fn send_message<'a>(
            &'a self,
            _c: ChannelId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async move {
                // transport send: record whether the lease is ALREADY committed
                // here. I1 requires it is NOT (commit comes after this returns).
                self.observe_lease_for_commit();
                self.send_step
                    .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                self.committed_at_send
                    .store(self.lease_is_committed_delivered(), Ordering::SeqCst);
                if self.transport_ok {
                    Ok(MessageId::new(42))
                } else {
                    Err("fake transport failure".to_string())
                }
            })
        }

        fn edit_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move {
                // FIRST post-send await point (driven by post_send_finalize via
                // PlaceholderController.transition). I1 requires the inline
                // commit ALREADY ran, so the lease must read Committed{Delivered}
                // here.
                tokio::task::yield_now().await;
                self.observe_lease_for_commit();
                if !self.post_send_await_seen.swap(true, Ordering::SeqCst) {
                    self.post_send_await_step
                        .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                    self.committed_at_post_send_await
                        .store(self.lease_is_committed_delivered(), Ordering::SeqCst);
                }
                if self.edit_fails.load(Ordering::SeqCst) {
                    // "Unknown Message" is classified Permanent by the controller
                    // retry helper → one attempt, then EditFailed.
                    Err("Unknown Message".to_string())
                } else {
                    Ok(())
                }
            })
        }

        fn delete_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.delete_calls.fetch_add(1, Ordering::SeqCst);
                self.delete_step
                    .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                self.committed_at_delete
                    .store(self.lease_is_committed_delivered(), Ordering::SeqCst);
                if self.delete_fails.load(Ordering::SeqCst) {
                    Err("fake delete failure".to_string())
                } else {
                    Ok(())
                }
            })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async move {
                // transport send (replace path): same observation as send_message.
                self.observe_lease_for_commit();
                self.send_step
                    .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                self.committed_at_send
                    .store(self.lease_is_committed_delivered(), Ordering::SeqCst);
                if self.transport_ok {
                    Ok(self.replace_outcome.clone())
                } else {
                    Err("fake replace failure".to_string())
                }
            })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _t: &'a str,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async move {})
        }
        fn dispatch_queued_turn<'a>(
            &'a self,
            _c: ChannelId,
            _i: &'a crate::services::discord::Intervention,
            _n: &'a str,
            _h: bool,
            _dispatch_lease: Option<
                std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
            >,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move { Ok(()) })
        }
        fn validate_live_routing<'a>(
            &'a self,
            _c: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move { Ok(()) })
        }
        fn requester_mention(&self) -> Option<String> {
            None
        }
        fn can_chain_locally(&self) -> bool {
            false
        }
        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            None
        }
    }

    /// I1 — commit+advance happens INSIDE the controller, after confirmed
    /// transport success and STRICTLY BEFORE any post-send await.
    ///
    /// Proof (no unsafe): the fake gateway shares the controller's lease cell.
    ///   1. Inside the transport send (`replace_message_with_outcome`), the
    ///      lease is read: it must NOT yet be Committed{Delivered} — the commit
    ///      is the synchronous statement the controller runs AFTER the send
    ///      returns.
    ///   2. Inside the FIRST post-send await (`edit_message`, driven by
    ///      `post_send_finalize`), the lease is read again: it MUST already be
    ///      Committed{Delivered}.
    /// Together (send-step < post-send-await-step, uncommitted-at-send,
    /// committed-at-post-send-await) this proves the commit landed in the gap
    /// between the transport send and the first post-send await — exactly I1.
    #[tokio::test]
    async fn i1_commit_advance_is_before_any_post_send_await() {
        let channel = ChannelId::new(100);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let body = "hello turn output";
        let placeholder_msg = MessageId::new(7777);
        let key = placeholder_key(channel, placeholder_msg);

        // Make the card Active so `transition` (driven by post_send_finalize)
        // actually performs its post-send `edit_message` await. Resets the
        // gateway observations afterward.
        prime_active(&controller, &gateway, key.clone()).await;

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            // Active placeholder + a terminal lifecycle so post_send_finalize
            // performs its post-send transition (edit_message) await.
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;

        // The returned outcome is already committed/advanced to `end`.
        match outcome {
            DeliveryOutcome::Delivered { committed_to, .. } => {
                assert_eq!(committed_to, body.len() as u64);
            }
            other => panic!("expected Delivered, got {}", debug_outcome(&other)),
        }

        // The post-send await actually ran (transition edited the card).
        assert!(
            gateway.post_send_await_seen.load(Ordering::SeqCst),
            "post_send_finalize must perform a post-send edit await for this plan"
        );

        // (1) At transport-send time the commit had NOT happened yet.
        assert!(
            !gateway.committed_at_send.load(Ordering::SeqCst),
            "I1: the lease must NOT be committed during the transport send (commit is after)"
        );
        // (2) At the first post-send await the commit HAD already happened.
        assert!(
            gateway.committed_at_post_send_await.load(Ordering::SeqCst),
            "I1: the lease MUST be committed/advanced before any post-send await runs"
        );
        // Temporal order: send strictly precedes the post-send await.
        let send_step = gateway.send_step.load(Ordering::SeqCst);
        let post_step = gateway.post_send_await_step.load(Ordering::SeqCst);
        assert!(
            send_step < post_step,
            "send (step {send_step}) must strictly precede the post-send await (step {post_step})"
        );

        // ---- M4 commit recorder (mutation-sensitive) --------------------
        // The commit was observable AT a gateway await EXACTLY once-or-more,
        // it was the `Delivered` outcome, and it FIRST became visible STRICTLY
        // AFTER the transport send (i.e. between the send and the post-send
        // await). A mutation that moves the commit before the send, or to the
        // ambiguous arm, breaks one of these.
        assert!(
            gateway.commit_count.load(Ordering::SeqCst) >= 1,
            "M4: the Delivered commit must be observable at a post-send gateway await"
        );
        assert!(
            gateway.first_commit_was_delivered.load(Ordering::SeqCst),
            "M4: the first observed commit must carry the Delivered outcome"
        );
        let first_commit_step = gateway.first_commit_step.load(Ordering::SeqCst);
        assert!(
            send_step < first_commit_step,
            "M4: the commit (first observable at step {first_commit_step}) must land \
             strictly after the transport send (step {send_step})"
        );
        // ---- M4 direct commit recorder (await-independent) --------------
        // The RecordingLease counts the commit the instant it is called, so it
        // catches commit mutations even with no intervening gateway await. A
        // Delivered turn commits EXACTLY ONCE with the Delivered outcome and
        // releases exactly once.
        assert_eq!(
            lease.delivered_commit_calls.load(Ordering::SeqCst),
            1,
            "I1: a Delivered turn must commit Delivered exactly once"
        );
        assert_eq!(
            lease.commit_calls.load(Ordering::SeqCst),
            1,
            "I1: a Delivered turn must call commit exactly once"
        );
        assert_eq!(
            lease.release_calls.load(Ordering::SeqCst),
            1,
            "I1: a Delivered turn must release exactly once"
        );

        // The lease was committed AND released (back to Unleased) by the time
        // the controller returned — re-acquire proves it is free, not stranded.
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "lease must be released (Unleased) after a Delivered turn"
        );
    }

    /// I2 — an ambiguous (Unknown) transport result must NOT commit/advance the
    /// lease; the controller releases it straight from `Leased` so it returns to
    /// `Unleased` with no `Committed` transition.
    #[tokio::test]
    async fn i2_ambiguous_releases_without_commit_or_advance() {
        let channel = ChannelId::new(101);
        let lease = Arc::new(RecordingLease::new(channel));
        // transport fails → controller classifies conservatively as Unknown.
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, false);
        let controller = PlaceholderController::default();
        let body = "ambiguous turn output";
        let placeholder_msg = MessageId::new(8888);
        let key = placeholder_key(channel, placeholder_msg);

        // Prime the card Active so that IF a mutation wrongly commits + runs
        // post_send_finalize on this ambiguous arm, `transition` WOULD perform
        // its post-send `edit_message` await — and the M4 commit recorder there
        // would witness a `Committed` lease, flipping `commit_count` and failing
        // the assertion below. Without this prime the recorder could not even
        // see such a mutation.
        prime_active(&controller, &gateway, key.clone()).await;

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Unknown { .. }),
            "ambiguous transport must yield Unknown, got {}",
            debug_outcome(&outcome)
        );
        // No post-send await ran (the send failed before the commit).
        assert!(
            !gateway.post_send_await_seen.load(Ordering::SeqCst),
            "an ambiguous send must not reach the post-send finalize await"
        );

        // ---- M4 mutation-sensitive commit recorder ----------------------
        // The decisive check: the RecordingLease counted ZERO commit calls on
        // this ambiguous run. This catches a commit mutation on the Unknown arm
        // the instant `commit` is invoked — INCLUDING a silent "commit then
        // immediately release" with no intervening gateway await, which the
        // gateway-await observation alone could miss. A genuine non-advance run
        // never commits, so the count stays 0.
        assert_eq!(
            lease.commit_calls.load(Ordering::SeqCst),
            0,
            "M4: an ambiguous (Unknown) delivery must NEVER call commit (even a \
             commit-then-release with no await must be caught)"
        );
        // And no gateway await ever observed a `Committed` lease either.
        assert_eq!(
            gateway.commit_count.load(Ordering::SeqCst),
            0,
            "M4: no gateway await may observe a Committed lease on the ambiguous arm"
        );

        // The lease was released WITHOUT a Committed transition.
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "I2: ambiguous outcome must release the lease without committing/advancing"
        );
    }

    /// A `TurnGateway` whose transport send PARKS forever (never resolves), so a
    /// `deliver_turn_output` future can be POLLED past the acquire and then
    /// DROPPED mid-transport — the exact cancellation the RAII lease guard must
    /// survive (review-fix H1 r2). Records that the send was entered so the test
    /// proves the drop happened AFTER the lease was acquired+leased.
    struct HangingTransportGateway {
        entered_send: Arc<AtomicBool>,
    }

    impl TurnGateway for HangingTransportGateway {
        fn replace_message_with_outcome<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async move {
                self.entered_send.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!("the parked transport never resolves")
            })
        }
        fn send_message<'a>(
            &'a self,
            _c: ChannelId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async move {
                self.entered_send.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!("the parked transport never resolves")
            })
        }
        fn edit_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move { Ok(()) })
        }
        fn delete_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move { Ok(()) })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _t: &'a str,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async move {})
        }
        fn dispatch_queued_turn<'a>(
            &'a self,
            _c: ChannelId,
            _i: &'a crate::services::discord::Intervention,
            _n: &'a str,
            _h: bool,
            _dispatch_lease: Option<
                std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
            >,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move { Ok(()) })
        }
        fn validate_live_routing<'a>(
            &'a self,
            _c: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move { Ok(()) })
        }
        fn requester_mention(&self) -> Option<String> {
            None
        }
        fn can_chain_locally(&self) -> bool {
            false
        }
        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            None
        }
    }

    /// review-fix H1 r2 — the controller-held lease is RAII-released when the
    /// `deliver_turn_output` future is CANCELLED (dropped) mid-transport, exactly
    /// like the legacy `SinkDeliveryLeaseGuard::Drop`. Without the guard the
    /// acquired lease would stay `Leased{Sink}` until the deadline (the leak the
    /// review found): the heartbeat guard drops but the lease was only released
    /// in the normal match arms, which a cancelled future never reaches.
    ///
    /// Proof: poll the future until it parks inside the (never-resolving)
    /// transport — at which point the acquire has ALREADY won (lease is
    /// `Leased`) — then DROP the future and assert the lease is back to
    /// `Unleased` (the guard's Drop released it) and re-acquirable.
    #[tokio::test]
    async fn cancelled_future_raii_releases_held_lease() {
        use std::task::{Context, Poll};
        let channel = ChannelId::new(130);
        let lease = Arc::new(RecordingLease::new(channel));
        let entered_send = Arc::new(AtomicBool::new(false));
        let gateway = HangingTransportGateway {
            entered_send: entered_send.clone(),
        };
        let controller = PlaceholderController::default();
        let body = "this turn's future is cancelled mid-POST";
        let turn = turn_key(channel);

        let mut fut = Box::pin(deliver_turn_output(
            &gateway,
            TurnOutputCtx {
                turn,
                lease_key: Some(DeliveryLeaseKey::from_turn_key(turn)),
                owner: RelayOwnerKind::SessionBoundRelay,
                holder: LeaseHolder::Sink,
                lease: lease.as_ref(),
                channel_id: channel,
                placeholder_controller: &controller,
                placeholder: PlaceholderSlot::None,
                body,
                send_range: (0, body.len() as u64),
                plan: OutputPlan::Replace {
                    lifecycle: PlaceholderLifecycle::Completed,
                },
                edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
                fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
                acquire_failure_mode: AcquireFailureMode::ProceedMarkerless,
                advance: None,
                heartbeat: None,
            },
        ));

        // Poll once: the controller acquires the lease, then parks inside the
        // never-resolving transport send.
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Pending),
            "the future must park inside the hanging transport (acquire already won)"
        );
        assert!(
            entered_send.load(Ordering::SeqCst),
            "the transport send must have been entered (so the acquire ran first)"
        );
        // The lease is HELD at this point — the acquire won and nothing has
        // released it yet.
        assert!(
            matches!(
                lease.read(),
                LeaseSnapshot::Leased {
                    holder: LeaseHolder::Sink,
                    ..
                }
            ),
            "the controller must hold the lease while parked in the transport"
        );

        // CANCEL: drop the future mid-transport. The RAII guard's Drop must
        // release the lease — the legacy `SinkDeliveryLeaseGuard::Drop` semantics.
        drop(fut);

        assert_eq!(
            lease.release_calls.load(Ordering::SeqCst),
            1,
            "H1 r2: cancelling the future mid-transport must RAII-release the held lease exactly once"
        );
        assert_eq!(
            lease.commit_calls.load(Ordering::SeqCst),
            0,
            "H1 r2: a cancelled (never-confirmed) transport must NEVER commit/advance"
        );
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "H1 r2: the lease must be Unleased after the cancelled future's guard Drop"
        );
        // Re-acquirable proof: the cell is genuinely free, not stranded leased.
        assert!(
            lease.try_acquire(
                DeliveryLeaseKey::from_turn_key(turn),
                LeaseHolder::Sink,
                0,
                body.len() as u64,
                lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
            ),
            "H1 r2: the released cell must be immediately re-acquirable (not stranded)"
        );
    }

    /// I2 companion — a NoOp plan skips entirely and never touches the lease.
    #[tokio::test]
    async fn noop_plan_skips_without_touching_lease() {
        let channel = ChannelId::new(102);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let body = "skipped";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::None,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::None,
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::NoOp,
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Skipped),
            "NoOp plan must Skip"
        );
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "NoOp must never touch the lease"
        );
        assert_eq!(
            gateway.send_step.load(Ordering::SeqCst),
            0,
            "NoOp must never call transport"
        );
    }

    /// H1 — a Split send that lands FEWER message IDs than `chunk_count` is a
    /// PARTIAL transport and must NEVER commit/advance.
    ///
    /// The fake's `send_long_message_with_rollback` falls back to the trait
    /// default. This fixture body fits in one split chunk, so a
    /// `SendNewChunks { chunk_count: 3 }` plan receives 1 id for a 3-chunk send —
    /// the exact partial the old `chunk_count.min(1)` bug committed as
    /// Delivered. With the fix (`ids.len() >= chunk_count`) it must classify
    /// Unknown, leave the lease uncommitted, and release to Unleased.
    #[tokio::test]
    async fn split_partial_send_does_not_commit_or_advance() {
        let channel = ChannelId::new(103);
        let lease = Arc::new(RecordingLease::new(channel));
        // transport_ok so the (default) send returns Ok(one id) — a SHORT write
        // relative to chunk_count=3.
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let body = "split body that should have spanned three chunks";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::None,
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::SendNewChunks {
                chunk_count: 3,
                delete_anchor: false,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Unknown { .. }),
            "a partial split send (1 id < 3 chunks) must be Unknown, got {}",
            debug_outcome(&outcome)
        );
        // M4 direct recorder: the partial split never called commit.
        assert_eq!(
            lease.commit_calls.load(Ordering::SeqCst),
            0,
            "H1: a partial split send must NEVER commit/advance the lease"
        );
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "H1: partial split must release the lease without committing"
        );
    }

    /// H1 companion — a Split send that lands AT LEAST `chunk_count` ids IS
    /// Delivered. `chunk_count: 1` is satisfied by the default one-id send, so
    /// this proves the boundary is `>= chunk_count` (exact-or-more), not the old
    /// `min(1)` that always passed.
    #[tokio::test]
    async fn split_full_send_commits_and_advances() {
        let channel = ChannelId::new(104);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let body = "single-chunk split body";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::None,
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::SendNewChunks {
                chunk_count: 1,
                delete_anchor: false,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        match outcome {
            DeliveryOutcome::Delivered { committed_to, .. } => {
                assert_eq!(committed_to, body.len() as u64);
            }
            other => panic!(
                "a full split send (1 id >= 1 chunk) must be Delivered, got {}",
                debug_outcome(&other)
            ),
        }
        assert_eq!(
            lease.delivered_commit_calls.load(Ordering::SeqCst),
            1,
            "H1: a full split send must commit Delivered exactly once"
        );
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "delivered split must release the lease"
        );
    }

    #[tokio::test]
    async fn split_anchor_delete_runs_after_full_chunk_send_before_commit() {
        let channel = ChannelId::new(112);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let placeholder_msg = MessageId::new(4242);

        let outcome = deliver_turn_output(
            &gateway,
            TurnOutputCtx {
                turn: turn_key(channel),
                lease_key: Some(lease_key(channel)),
                owner: RelayOwnerKind::Watcher,
                holder: LeaseHolder::Sink,
                lease: lease.as_ref(),
                channel_id: channel,
                placeholder_controller: &controller,
                placeholder: PlaceholderSlot::Active {
                    message_id: placeholder_msg,
                    key: placeholder_key(channel, placeholder_msg),
                },
                body: "single chunk body",
                send_range: (0, 17),
                plan: OutputPlan::SendNewChunks {
                    chunk_count: 1,
                    delete_anchor: true,
                },
                edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
                fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
                acquire_failure_mode: AcquireFailureMode::Transient,
                advance: None,
                heartbeat: None,
            },
        )
        .await;

        match outcome {
            DeliveryOutcome::Delivered {
                committed_to,
                new_chunks: Some(chunks),
                ..
            } => {
                assert_eq!(committed_to, 17);
                assert_eq!(chunks.first_message_id, Some(MessageId::new(42)));
                assert_eq!(chunks.tail_message_id, Some(MessageId::new(42)));
                assert_eq!(chunks.anchor_delete_error, None);
            }
            other => panic!(
                "expected Delivered with chunk metadata, got {}",
                debug_outcome(&other)
            ),
        }
        assert_eq!(gateway.delete_calls.load(Ordering::SeqCst), 1);
        assert!(
            gateway.send_step.load(Ordering::SeqCst) < gateway.delete_step.load(Ordering::SeqCst),
            "anchor delete must run after the chunk send"
        );
        assert!(
            !gateway.committed_at_delete.load(Ordering::SeqCst),
            "legacy long-chunk ordering deletes the anchor before the Delivered commit"
        );
        assert_eq!(lease.delivered_commit_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn split_anchor_delete_failure_still_delivers() {
        let channel = ChannelId::new(113);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        gateway.delete_fails.store(true, Ordering::SeqCst);
        let controller = PlaceholderController::default();
        let placeholder_msg = MessageId::new(4343);

        let outcome = deliver_turn_output(
            &gateway,
            TurnOutputCtx {
                turn: turn_key(channel),
                lease_key: Some(lease_key(channel)),
                owner: RelayOwnerKind::Watcher,
                holder: LeaseHolder::Sink,
                lease: lease.as_ref(),
                channel_id: channel,
                placeholder_controller: &controller,
                placeholder: PlaceholderSlot::Active {
                    message_id: placeholder_msg,
                    key: placeholder_key(channel, placeholder_msg),
                },
                body: "single chunk body",
                send_range: (0, 17),
                plan: OutputPlan::SendNewChunks {
                    chunk_count: 1,
                    delete_anchor: true,
                },
                edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
                fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
                acquire_failure_mode: AcquireFailureMode::Transient,
                advance: None,
                heartbeat: None,
            },
        )
        .await;

        match outcome {
            DeliveryOutcome::Delivered {
                new_chunks: Some(chunks),
                ..
            } => {
                assert_eq!(chunks.first_message_id, Some(MessageId::new(42)));
                assert_eq!(chunks.tail_message_id, Some(MessageId::new(42)));
                assert_eq!(
                    chunks.anchor_delete_error.as_deref(),
                    Some("fake delete failure")
                );
            }
            other => panic!(
                "delete failure must not un-deliver, got {}",
                debug_outcome(&other)
            ),
        }
        assert_eq!(gateway.delete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(lease.delivered_commit_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn split_anchor_send_failure_commits_not_delivered_without_delete() {
        let channel = ChannelId::new(114);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, false);
        let controller = PlaceholderController::default();
        let placeholder_msg = MessageId::new(4444);

        let outcome = deliver_turn_output(
            &gateway,
            TurnOutputCtx {
                turn: turn_key(channel),
                lease_key: Some(lease_key(channel)),
                owner: RelayOwnerKind::Watcher,
                holder: LeaseHolder::Sink,
                lease: lease.as_ref(),
                channel_id: channel,
                placeholder_controller: &controller,
                placeholder: PlaceholderSlot::Active {
                    message_id: placeholder_msg,
                    key: placeholder_key(channel, placeholder_msg),
                },
                body: "single chunk body",
                send_range: (0, 17),
                plan: OutputPlan::SendNewChunks {
                    chunk_count: 1,
                    delete_anchor: true,
                },
                edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
                fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
                acquire_failure_mode: AcquireFailureMode::Transient,
                advance: None,
                heartbeat: None,
            },
        )
        .await;

        assert!(
            matches!(outcome, DeliveryOutcome::NotDelivered { committed_from: 0 }),
            "rollback-aware long-chunk send failure commits NotDelivered"
        );
        assert_eq!(
            gateway.delete_calls.load(Ordering::SeqCst),
            0,
            "failed chunk sends must preserve the anchor"
        );
        assert_eq!(lease.not_delivered_commit_calls.load(Ordering::SeqCst), 1);
        assert_eq!(lease.delivered_commit_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(lease.read(), LeaseSnapshot::Unleased));
    }

    /// H2 — a `ReplaceLongMessageOutcome::PartialContinuationFailure` is a
    /// not-delivered / retry-preserving result for EVERY existing owner
    /// (session_relay_sink.rs:956 → `RelaySinkError::Transient`,
    /// turn_bridge/terminal_delivery.rs:155 + its
    /// `partial_continuation_failure_does_not_commit_terminal_delivery` test →
    /// committed = false, formatting.rs:1787 → `Err`). The controller must map
    /// it to Unknown / non-advance, NOT commit it (the old `Ok(_) => Delivered`
    /// catch-all bug).
    #[tokio::test]
    async fn replace_partial_continuation_failure_does_not_commit_or_advance() {
        let channel = ChannelId::new(105);
        let lease = Arc::new(RecordingLease::new(channel));
        let placeholder_msg = MessageId::new(9999);
        let key = placeholder_key(channel, placeholder_msg);
        // transport_ok so replace returns Ok(..), but with the real
        // PartialContinuationFailure variant.
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true)
                .with_replace_outcome(ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks: 1,
                    total_chunks: 3,
                    failed_chunk_index: 1,
                    sent_continuation_message_ids: Vec::new(),
                    cleanup_errors: Vec::new(),
                    error: "HTTP 500".to_string(),
                });
        let controller = PlaceholderController::default();
        // Prime Active so a wrongful commit would expose itself via the
        // post-send transition await (M4 recorder).
        prime_active(&controller, &gateway, key.clone()).await;
        let body = "replace body whose continuation failed mid-stream";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Unknown { fell_back: false }),
            "H2: PartialContinuationFailure must be Unknown (non-advance), got {}",
            debug_outcome(&outcome)
        );
        // The send was Ok, but the controller must NOT commit — the recorder
        // counts zero commit calls.
        assert_eq!(
            lease.commit_calls.load(Ordering::SeqCst),
            0,
            "H2: PartialContinuationFailure must NEVER commit/advance the lease"
        );
        assert!(
            !gateway.post_send_await_seen.load(Ordering::SeqCst),
            "H2: a non-advance replace outcome must not reach the post-send finalize await"
        );
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "H2: PartialContinuationFailure must release the lease without committing"
        );
    }

    /// H1 r3 arm A — under `FallbackCommitPolicy::CommitOnFallback`,
    /// `SentFallbackAfterEditFailure` advances. This is the sink/standby owner
    /// policy: the fallback POST carried the body, so the offset commits
    /// (`session_relay_sink.rs:905` bumps `delivered_total` + advances,
    /// `standby_relay.rs:662` returns `true`). Exactly one Delivered commit.
    #[tokio::test]
    async fn replace_sent_fallback_after_edit_failure_commits_and_advances() {
        let channel = ChannelId::new(106);
        let lease = Arc::new(RecordingLease::new(channel));
        let placeholder_msg = MessageId::new(11111);
        let key = placeholder_key(channel, placeholder_msg);
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true)
                .with_replace_outcome(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                    edit_error: "edit 500, fallback POST succeeded".to_string(),
                    replacement_anchor: None,
                });
        let controller = PlaceholderController::default();
        prime_active(&controller, &gateway, key.clone()).await;
        let body = "replace body delivered via fallback post";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::SessionBoundRelay,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        match outcome {
            DeliveryOutcome::Delivered { committed_to, .. } => {
                assert_eq!(committed_to, body.len() as u64);
            }
            other => panic!(
                "H2: SentFallbackAfterEditFailure must be Delivered (sink advances), got {}",
                debug_outcome(&other)
            ),
        }
        assert_eq!(
            lease.delivered_commit_calls.load(Ordering::SeqCst),
            1,
            "H1 r3: CommitOnFallback must commit Delivered exactly once"
        );
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "delivered fallback must release the lease"
        );
    }

    /// H1 r3 arm B — under `FallbackCommitPolicy::NoCommitOnFallback`, the SAME
    /// `SentFallbackAfterEditFailure` transport result must NOT advance. This is
    /// the turn_bridge/terminal_delivery owner policy: `terminal_delivery.rs:143`
    /// records the cleanup failure and returns `committed = false`, and its
    /// commit predicate `terminal_delivery.rs:42` matches `EditedOriginal` only.
    /// Cutting turn_bridge over to the controller must therefore NOT regress
    /// into advancing on a fallback edit failure that is non-committed today.
    ///
    /// Mutation guard (codex r3): flipping `NoCommitOnFallback` back to
    /// `Delivered` in `classify_replace_outcome` makes this test fail — the
    /// outcome would be `Delivered` (not `Unknown`) and `commit_calls` would be
    /// 1 (not 0). The two arms share an identical scenario apart from the
    /// policy, so the policy is the sole load-bearing input.
    #[tokio::test]
    async fn replace_sent_fallback_after_edit_failure_no_commit_does_not_advance() {
        let channel = ChannelId::new(112);
        let lease = Arc::new(RecordingLease::new(channel));
        let placeholder_msg = MessageId::new(44444);
        let key = placeholder_key(channel, placeholder_msg);
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true)
                .with_replace_outcome(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                    edit_error: "edit 500, fallback POST succeeded".to_string(),
                    replacement_anchor: None,
                });
        let controller = PlaceholderController::default();
        // Prime Active so a wrongful commit would expose itself via the
        // post-send transition await (M4 recorder).
        prime_active(&controller, &gateway, key.clone()).await;
        let body = "replace body delivered via fallback post (turn_bridge policy)";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            // turn_bridge is the watcher-owned terminal-delivery path.
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Watcher { instance_id: 1 },
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::NoCommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Unknown { fell_back: true }),
            "H1 r3: NoCommitOnFallback + SentFallback must yield Unknown{{fell_back:true}} (#3089 A5: body landed via fallback, no advance), got {}",
            debug_outcome(&outcome)
        );
        // The send returned Ok, but the owner policy says do not commit — the
        // recorder must count ZERO commit calls (even a silent commit-then-
        // release would be caught here, M4).
        assert_eq!(
            lease.commit_calls.load(Ordering::SeqCst),
            0,
            "H1 r3: NoCommitOnFallback must NEVER commit/advance the lease"
        );
        assert!(
            !gateway.post_send_await_seen.load(Ordering::SeqCst),
            "H1 r3: a non-advance fallback must not reach the post-send finalize await"
        );
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "H1 r3: NoCommitOnFallback must release the lease without committing"
        );
    }

    /// #3089 A5 — the `NoCommitOnFallback` `Unknown` arm SURFACES `fell_back`:
    /// `SentFallbackAfterEditFailure` → `Unknown { fell_back: true }` (the body
    /// reached Discord via a fresh fallback send, so the turn_bridge owner bumps
    /// `response_sent_offset` even though `confirmed_end` is NOT advanced — the
    /// dual-offset recovery), while `PartialContinuationFailure` →
    /// `Unknown { fell_back: false }` (nothing landed — no bump). Neither
    /// advances (I2: `commit_calls == 0` on both). The CommitOnFallback owner is
    /// proven UNAFFECTED by `replace_sent_fallback_after_edit_failure_commits_and_advances`
    /// (same scenario, `CommitOnFallback` → `Delivered { FreshFallback }`).
    /// Mutation guard: flipping the SentFallback arm to
    /// `Unknown { fell_back: false }` (or back to a bare advance) fails the
    /// `fell_back: true` assertion below; collapsing the partial arm to
    /// `fell_back: true` fails the second assertion.
    #[tokio::test]
    async fn no_commit_on_fallback_surfaces_fell_back() {
        async fn run_no_commit(
            channel_n: u64,
            msg_n: u64,
            outcome: ReplaceLongMessageOutcome,
        ) -> (DeliveryOutcome, usize) {
            let channel = ChannelId::new(channel_n);
            let lease = Arc::new(RecordingLease::new(channel));
            let placeholder_msg = MessageId::new(msg_n);
            let key = placeholder_key(channel, placeholder_msg);
            let gateway =
                ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true)
                    .with_replace_outcome(outcome);
            let controller = PlaceholderController::default();
            prime_active(&controller, &gateway, key.clone()).await;
            let body = "turn_bridge short-replace body (NoCommitOnFallback)";
            let ctx = TurnOutputCtx {
                turn: turn_key(channel),
                lease_key: Some(lease_key(channel)),
                owner: RelayOwnerKind::None,
                holder: LeaseHolder::Bridge,
                lease: lease.as_ref(),
                channel_id: channel,
                placeholder_controller: &controller,
                placeholder: PlaceholderSlot::Active {
                    message_id: placeholder_msg,
                    key,
                },
                body,
                send_range: (0, body.len() as u64),
                plan: OutputPlan::Replace {
                    lifecycle: PlaceholderLifecycle::Active,
                },
                edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
                fallback_commit_policy: FallbackCommitPolicy::NoCommitOnFallback,
                acquire_failure_mode: AcquireFailureMode::Transient,
                advance: None,
                heartbeat: None,
            };
            let outcome = deliver_turn_output(&gateway, ctx).await;
            let commits = lease.commit_calls.load(Ordering::SeqCst);
            (outcome, commits)
        }

        // SentFallbackAfterEditFailure → fell_back = true (body landed), no commit.
        let (fell_back_outcome, fell_back_commits) = run_no_commit(
            221,
            55551,
            ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "edit 500; fallback POST succeeded".to_string(),
                replacement_anchor: None,
            },
        )
        .await;
        assert!(
            matches!(
                fell_back_outcome,
                DeliveryOutcome::Unknown { fell_back: true }
            ),
            "SentFallback under NoCommitOnFallback must surface fell_back=true, got {}",
            debug_outcome(&fell_back_outcome)
        );
        assert_eq!(
            fell_back_commits, 0,
            "fell_back=true is still Unknown (I2): NEVER advance/commit"
        );

        // PartialContinuationFailure → fell_back = false (nothing landed), no commit.
        let (partial_outcome, partial_commits) = run_no_commit(
            222,
            55552,
            ReplaceLongMessageOutcome::PartialContinuationFailure {
                sent_chunks: 1,
                total_chunks: 3,
                failed_chunk_index: 1,
                sent_continuation_message_ids: Vec::new(),
                cleanup_errors: Vec::new(),
                error: "HTTP 500".to_string(),
            },
        )
        .await;
        assert!(
            matches!(
                partial_outcome,
                DeliveryOutcome::Unknown { fell_back: false }
            ),
            "PartialContinuationFailure must surface fell_back=false, got {}",
            debug_outcome(&partial_outcome)
        );
        assert_eq!(
            partial_commits, 0,
            "fell_back=false is Unknown (I2): NEVER advance/commit"
        );
    }

    /// M3 — the controller WIRES `PlaceholderController.transition` (design §5
    /// A1), and on `EditFailed` applies the `DeleteIfProvenStale` policy: a
    /// failed terminal placeholder edit deletes the now-stale original. Proves
    /// the watcher conditional-delete arm flows through the controller.
    #[tokio::test]
    async fn edit_fail_delete_if_proven_stale_deletes_original() {
        let channel = ChannelId::new(107);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let placeholder_msg = MessageId::new(22222);
        let key = placeholder_key(channel, placeholder_msg);
        prime_active(&controller, &gateway, key.clone()).await;
        // Now make the terminal transition's edit fail.
        gateway.fail_edits_from_now();
        let body = "delivered, but the terminal placeholder edit fails";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Watcher { instance_id: 1 },
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::DeleteIfProvenStale,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        // Delivered: the transport replace succeeded; the post-send placeholder
        // edit failing never un-advances the committed offset.
        assert!(
            matches!(outcome, DeliveryOutcome::Delivered { .. }),
            "M3: a successful replace stays Delivered even if the terminal edit fails, got {}",
            debug_outcome(&outcome)
        );
        assert_eq!(
            lease.delivered_commit_calls.load(Ordering::SeqCst),
            1,
            "M3: the commit/advance is independent of the post-send placeholder edit"
        );
        // The EditFailed → DeleteIfProvenStale policy deleted the stale original.
        assert_eq!(
            gateway.delete_calls.load(Ordering::SeqCst),
            1,
            "M3: DeleteIfProvenStale must delete the original when transition reports EditFailed"
        );
    }

    /// M3 companion — `PreserveAlways` (#2757) must NEVER delete the original on
    /// `EditFailed`. Same failed-edit scenario, opposite policy.
    #[tokio::test]
    async fn edit_fail_preserve_always_keeps_original() {
        let channel = ChannelId::new(108);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let placeholder_msg = MessageId::new(33333);
        let key = placeholder_key(channel, placeholder_msg);
        prime_active(&controller, &gateway, key.clone()).await;
        gateway.fail_edits_from_now();
        let body = "delivered via sink; preserve original on edit fail";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::SessionBoundRelay,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Delivered { .. }),
            "M3: a successful replace stays Delivered even if the terminal edit fails, got {}",
            debug_outcome(&outcome)
        );
        // #2757: the original is preserved — no delete.
        assert_eq!(
            gateway.delete_calls.load(Ordering::SeqCst),
            0,
            "M3 (#2757): PreserveAlways must NEVER delete the original on EditFailed"
        );
    }

    // ====================================================================
    // #3089 A2a — sink-capable controller: acquire-failure mode, owner
    // advance callback, POST heartbeat. (pure add — no owner wired yet.)
    // ====================================================================

    /// A2a capability 2: an owner advance callback that records WHEN it was
    /// invoked (against the gateway's shared step clock) and returns a fixed
    /// bool. Lets a test prove the callback runs INLINE before any post-send
    /// await (I1) and is skipped on the ambiguous arm (I2).
    struct RecordingAdvance {
        clock: Arc<AtomicUsize>,
        calls: AtomicUsize,
        call_step: AtomicUsize,
        seen_range: std::sync::Mutex<Option<(u64, u64)>>,
        ret: bool,
    }

    impl RecordingAdvance {
        fn new(clock: Arc<AtomicUsize>, ret: bool) -> Self {
            Self {
                clock,
                calls: AtomicUsize::new(0),
                call_step: AtomicUsize::new(0),
                seen_range: std::sync::Mutex::new(None),
                ret,
            }
        }
        fn invoke(&self, range: (u64, u64)) -> bool {
            let step = self.clock.fetch_add(1, Ordering::SeqCst);
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.call_step.store(step, Ordering::SeqCst);
            }
            *self.seen_range.lock().unwrap() = Some(range);
            self.ret
        }
    }

    /// A2a capability 3: a `PostHeartbeat` that, on `start`, fires `ticks`
    /// synchronous `renew`s against the live lease (modelling the in-flight
    /// renew loop) and records the holder/turn it was started with + the post-
    /// renew deadline. The guard records the gateway step at which it is dropped,
    /// so a test can prove the controller STOPS the heartbeat before the inline
    /// commit (#3151).
    struct RecordingHeartbeat {
        lease: Arc<dyn DeliveryLease + Send + Sync>,
        clock: Arc<AtomicUsize>,
        ticks: u64,
        started: AtomicUsize,
        started_holder: std::sync::Mutex<Option<LeaseHolder>>,
        renewed_deadline: AtomicU64,
        drop_step: Arc<AtomicUsize>,
    }

    impl RecordingHeartbeat {
        fn new(lease: Arc<dyn DeliveryLease + Send + Sync>, clock: Arc<AtomicUsize>) -> Self {
            Self {
                lease,
                clock,
                ticks: 2,
                started: AtomicUsize::new(0),
                started_holder: std::sync::Mutex::new(None),
                renewed_deadline: AtomicU64::new(0),
                drop_step: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    struct RecordingHeartbeatGuard {
        clock: Arc<AtomicUsize>,
        drop_step: Arc<AtomicUsize>,
    }
    impl PostHeartbeatGuard for RecordingHeartbeatGuard {}
    impl Drop for RecordingHeartbeatGuard {
        fn drop(&mut self) {
            self.drop_step
                .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
        }
    }

    impl PostHeartbeat for RecordingHeartbeat {
        fn start(&self, holder: LeaseHolder, key: DeliveryLeaseKey) -> Box<dyn PostHeartbeatGuard> {
            self.started.fetch_add(1, Ordering::SeqCst);
            *self.started_holder.lock().unwrap() = Some(holder);
            // Fire the renew loop's ticks now: each pushes the deadline forward,
            // modelling the in-flight heartbeat keeping the lease fresh.
            let mut last = 0u64;
            for i in 1..=self.ticks {
                last = lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS) + i;
                let renewed = self.lease.renew(holder, key.clone(), last);
                assert!(
                    renewed,
                    "A2a heartbeat: renew must succeed against the live lease the \
                     controller acquired (holder/turn must match)"
                );
            }
            self.renewed_deadline.store(last, Ordering::SeqCst);
            Box::new(RecordingHeartbeatGuard {
                clock: self.clock.clone(),
                drop_step: self.drop_step.clone(),
            })
        }
    }

    /// Pre-occupy the lease with a DIFFERENT holder so the controller's
    /// `try_acquire` LOSES — the precondition for both acquire-failure-mode
    /// tests. Returns the foreign holder/turn/range that owns the cell.
    fn occupy_lease_with_foreign_holder(lease: &RecordingLease, channel: ChannelId) {
        let foreign_turn = DeliveryLeaseKey::from_turn_key(TurnKey::new(channel, 99, 1));
        let ok = lease.try_acquire(
            foreign_turn,
            LeaseHolder::Watcher { instance_id: 7 },
            0,
            64,
            lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
        );
        assert!(ok, "foreign holder must win the initial acquire");
    }

    /// A2a capability 1 — `ProceedMarkerless`: when the acquire LOSES, the sink
    /// behaviour still POSTs (markerless), delegating the advance to the owner
    /// callback. The controller holds no lease, so it never commits/releases its
    /// own holder; the send DOES happen and the outcome reflects the callback.
    #[tokio::test]
    async fn proceed_markerless_sends_when_acquire_fails() {
        let channel = ChannelId::new(120);
        let lease = Arc::new(RecordingLease::new(channel));
        occupy_lease_with_foreign_holder(&lease, channel);
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let clock = Arc::new(AtomicUsize::new(1));
        // Markerless send still advances via the owner callback (true here).
        let advance = RecordingAdvance::new(clock.clone(), true);
        let body = "markerless sink delivery";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::SessionBoundRelay,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            // No placeholder → Replace falls back to a fresh send_message.
            placeholder: PlaceholderSlot::None,
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::ProceedMarkerless,
            advance: Some(&|r| advance.invoke(r)),
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        // The send happened (markerless), and the callback advanced → Delivered.
        match outcome {
            DeliveryOutcome::Delivered { committed_to, .. } => {
                assert_eq!(committed_to, body.len() as u64);
            }
            other => panic!(
                "ProceedMarkerless must still deliver, got {}",
                debug_outcome(&other)
            ),
        }
        assert_ne!(
            gateway.send_step.load(Ordering::SeqCst),
            0,
            "ProceedMarkerless: the transport send MUST run even though acquire lost"
        );
        assert_eq!(
            advance.calls.load(Ordering::SeqCst),
            1,
            "markerless path still delegates the advance to the owner callback"
        );
        // The controller never committed/released on its OWN holder — the cell
        // is still held by the FOREIGN holder it lost the acquire to.
        assert_eq!(
            lease.commit_calls.load(Ordering::SeqCst),
            0,
            "ProceedMarkerless holds no lease → never commits"
        );
        assert_eq!(
            lease.release_calls.load(Ordering::SeqCst),
            0,
            "ProceedMarkerless holds no lease → never releases our holder"
        );
        assert!(
            matches!(
                lease.read(),
                LeaseSnapshot::Leased {
                    holder: LeaseHolder::Watcher { instance_id: 7 },
                    ..
                }
            ),
            "the foreign holder's lease is untouched by a markerless send"
        );
    }

    /// A2a capability 1 — `Transient`: when the acquire LOSES, the watcher/bridge
    /// behaviour does NOT send and returns `Transient` (the A1 default). Decisive
    /// vs the markerless test: the SOLE difference is the mode, and it flips
    /// send-happened + outcome.
    #[tokio::test]
    async fn transient_mode_does_not_send_when_acquire_fails() {
        let channel = ChannelId::new(121);
        let lease = Arc::new(RecordingLease::new(channel));
        occupy_lease_with_foreign_holder(&lease, channel);
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let clock = Arc::new(AtomicUsize::new(1));
        let advance = RecordingAdvance::new(clock.clone(), true);
        let body = "watcher loses the acquire";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Watcher { instance_id: 1 },
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::None,
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: Some(&|r| advance.invoke(r)),
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        match outcome {
            DeliveryOutcome::Transient { retry_from_offset } => {
                assert_eq!(retry_from_offset, 0);
            }
            other => panic!(
                "Transient mode must yield Transient, got {}",
                debug_outcome(&other)
            ),
        }
        // The decisive divergence: NO send, NO advance callback, NO commit.
        assert_eq!(
            gateway.send_step.load(Ordering::SeqCst),
            0,
            "Transient mode: a lost acquire must NOT send"
        );
        assert_eq!(
            advance.calls.load(Ordering::SeqCst),
            0,
            "Transient mode: a lost acquire must NOT run the advance callback"
        );
        assert_eq!(
            lease.commit_calls.load(Ordering::SeqCst),
            0,
            "Transient mode: a lost acquire never commits"
        );
    }

    /// A2a capability 2 — the owner advance callback returning `true` commits
    /// `Delivered` and the callback runs INLINE, STRICTLY BEFORE any post-send
    /// await (I1). Proven by the shared step clock: the advance call step is
    /// after the transport send but before the first post-send `edit_message`
    /// await observation.
    #[tokio::test]
    async fn advance_callback_true_commits_delivered_before_post_send_await() {
        let channel = ChannelId::new(122);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let placeholder_msg = MessageId::new(55555);
        let key = placeholder_key(channel, placeholder_msg);
        prime_active(&controller, &gateway, key.clone()).await;
        // Share the gateway's clock so the advance call step orders against the
        // send/post-send-await steps.
        let clock = gateway.clock_handle();
        let advance = RecordingAdvance::new(clock, true);
        let body = "advance gate matched the live inflight";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::SessionBoundRelay,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: Some(&|r| advance.invoke(r)),
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        match outcome {
            DeliveryOutcome::Delivered { committed_to, .. } => {
                assert_eq!(committed_to, body.len() as u64);
            }
            other => panic!("advance=true must Deliver, got {}", debug_outcome(&other)),
        }
        // The callback saw the confirmed (start, end) range.
        assert_eq!(
            *advance.seen_range.lock().unwrap(),
            Some((0, body.len() as u64)),
            "the advance callback must receive the controller's confirmed byte range"
        );
        // Exactly one Delivered commit (the advance=true arm).
        assert_eq!(
            lease.delivered_commit_calls.load(Ordering::SeqCst),
            1,
            "advance=true must commit Delivered exactly once"
        );
        assert_eq!(
            lease.not_delivered_commit_calls.load(Ordering::SeqCst),
            0,
            "advance=true must not commit NotDelivered"
        );
        // I1 ordering: send < advance-call < post-send await.
        let send_step = gateway.send_step.load(Ordering::SeqCst);
        let advance_step = advance.call_step.load(Ordering::SeqCst);
        let post_step = gateway.post_send_await_step.load(Ordering::SeqCst);
        assert!(
            send_step < advance_step,
            "I1: the advance (step {advance_step}) must run AFTER the transport send (step {send_step})"
        );
        assert!(
            advance_step < post_step,
            "I1: the advance (step {advance_step}) must run BEFORE the post-send await (step {post_step})"
        );
    }

    /// A2a capability 2 — the owner advance callback returning `false` (the
    /// identity gate REFUSED, e.g. inflight cleared during a slow POST) commits
    /// `NotDelivered` and returns `NotDelivered`; the offset does NOT advance.
    /// Mirrors the sink's `advanced == false` arm (`session_relay_sink.rs:571-577`).
    /// Mutation guard: the SOLE difference from the test above is the callback
    /// bool, and it flips both the outcome and the commit outcome.
    #[tokio::test]
    async fn advance_callback_false_returns_not_delivered_without_advancing() {
        let channel = ChannelId::new(123);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let placeholder_msg = MessageId::new(66666);
        let key = placeholder_key(channel, placeholder_msg);
        prime_active(&controller, &gateway, key.clone()).await;
        let clock = gateway.clock_handle();
        let advance = RecordingAdvance::new(clock, false);
        let body = "advance gate refused: inflight was replaced";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::SessionBoundRelay,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: Some(&|r| advance.invoke(r)),
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        match outcome {
            DeliveryOutcome::NotDelivered { committed_from } => {
                // The offset stayed at the pre-send start (no advance).
                assert_eq!(committed_from, 0);
            }
            other => panic!(
                "advance=false must yield NotDelivered, got {}",
                debug_outcome(&other)
            ),
        }
        // The lease was committed NotDelivered (so the watcher reconciliation
        // re-sends), NOT Delivered — the offset did not advance.
        assert_eq!(
            lease.not_delivered_commit_calls.load(Ordering::SeqCst),
            1,
            "advance=false must commit NotDelivered exactly once"
        );
        assert_eq!(
            lease.delivered_commit_calls.load(Ordering::SeqCst),
            0,
            "advance=false must NEVER commit Delivered"
        );
        // The lease is released back to Unleased (commit happened, then release).
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "a committed NotDelivered turn still releases the lease"
        );
    }

    /// A2a capability 2 — I2 preserved: the advance callback is NEVER invoked on
    /// an ambiguous (Unknown) transport. A failed send must not even ask the
    /// owner whether to advance.
    #[tokio::test]
    async fn advance_callback_not_invoked_on_ambiguous_transport() {
        let channel = ChannelId::new(124);
        let lease = Arc::new(RecordingLease::new(channel));
        // transport fails → Unknown.
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, false);
        let controller = PlaceholderController::default();
        let clock = Arc::new(AtomicUsize::new(1));
        let advance = RecordingAdvance::new(clock, true);
        let body = "ambiguous send, advance must not be consulted";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::SessionBoundRelay,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::None,
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: Some(&|r| advance.invoke(r)),
            heartbeat: None,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Unknown { .. }),
            "ambiguous transport must yield Unknown, got {}",
            debug_outcome(&outcome)
        );
        assert_eq!(
            advance.calls.load(Ordering::SeqCst),
            0,
            "I2: the advance callback must NEVER run on an ambiguous transport"
        );
        assert_eq!(
            lease.commit_calls.load(Ordering::SeqCst),
            0,
            "I2: an Unknown transport never commits"
        );
    }

    /// A2a capability 3 — the POST heartbeat renews the (held) lease deadline
    /// while the POST is in flight, is started with the controller's
    /// (holder, turn), and is STOPPED before the inline commit (#3151). Proven
    /// by: renew_calls > 0, the cell deadline moved past the acquire deadline,
    /// and the heartbeat guard's drop step precedes the first observed commit.
    #[tokio::test]
    async fn post_heartbeat_renews_held_lease_and_stops_before_commit() {
        let channel = ChannelId::new(125);
        let lease = Arc::new(RecordingLease::new(channel));
        let gateway =
            ObservingGateway::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, true);
        let controller = PlaceholderController::default();
        let placeholder_msg = MessageId::new(77777);
        let key = placeholder_key(channel, placeholder_msg);
        prime_active(&controller, &gateway, key.clone()).await;
        let clock = gateway.clock_handle();
        // Share the gateway's monotonic step clock with the lease so the actual
        // `commit` CALL is stamped on the same clock as the heartbeat guard's
        // `Drop` — the basis for the mutation-sensitive `drop_step < commit_step`
        // assertion below.
        lease.attach_clock(clock.clone());
        let heartbeat =
            RecordingHeartbeat::new(lease.clone() as Arc<dyn DeliveryLease + Send + Sync>, clock);
        let body = "long POST kept alive by the heartbeat";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            lease_key: Some(lease_key(channel)),
            owner: RelayOwnerKind::SessionBoundRelay,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key,
            },
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed,
            },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
            acquire_failure_mode: AcquireFailureMode::Transient,
            advance: None,
            heartbeat: Some(&heartbeat),
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Delivered { .. }),
            "delivered with a heartbeat, got {}",
            debug_outcome(&outcome)
        );
        // The heartbeat was started exactly once, on the held path, with the
        // controller's holder.
        assert_eq!(
            heartbeat.started.load(Ordering::SeqCst),
            1,
            "the held-lease path starts the POST heartbeat exactly once"
        );
        assert_eq!(
            *heartbeat.started_holder.lock().unwrap(),
            Some(LeaseHolder::Sink),
            "the heartbeat must be started with the controller's holder identity"
        );
        // The renew(s) actually pushed the lease deadline forward (not a fixed
        // 60s TTL): renew_calls > 0 and the recorded post-renew deadline is the
        // one the heartbeat installed.
        assert!(
            lease.renew_calls.load(Ordering::SeqCst) >= 1,
            "the POST heartbeat must renew the lease deadline at least once"
        );
        // #3151 ordering (mutation-sensitive): the heartbeat guard's `Drop` and
        // the actual `commit` CALL are both stamped on the same shared step
        // clock, so this directly measures that the guard stopped BEFORE the
        // commit ran — the renew loop cannot race the commit. This catches the
        // mutation "commit first, drop the guard before post_send_finalize().await":
        // the earlier (now-removed) check compared `drop_step` only to
        // `first_commit_step` (set when a *later* gateway await observes the
        // committed lease), which is always after the drop regardless of the real
        // commit time, so it passed under that mutation.
        let drop_step = heartbeat.drop_step.load(Ordering::SeqCst);
        let commit_step = lease.commit_step.load(Ordering::SeqCst);
        assert_ne!(drop_step, 0, "the heartbeat guard must have been dropped");
        assert_ne!(commit_step, 0, "the lease must have been committed");
        assert!(
            drop_step < commit_step,
            "#3151: the heartbeat must STOP (drop_step {drop_step}) before the inline \
             commit CALL runs (commit_step {commit_step})"
        );
        // The commit must also remain observable to a post-send gateway await
        // (the existing I1/M4 ordering): the first await that reads a `Committed`
        // lease lands after the actual commit call.
        let first_commit_step = gateway.first_commit_step.load(Ordering::SeqCst);
        assert!(
            first_commit_step != 0 && commit_step < first_commit_step,
            "#3151: the inline commit (step {commit_step}) must be observable to a \
             post-send gateway await (step {first_commit_step})"
        );
    }

    /// `from_length_decision` mapping: Inline/Compact → Replace, Split →
    /// SendNewChunks, FileAttachment/Reject → NoOp.
    #[test]
    fn output_plan_from_length_decision_maps_each_variant() {
        use crate::services::discord::outbound::result::FallbackUsed;

        let inline = LengthPolicyDecision::Inline { char_count: 10 };
        assert!(matches!(
            OutputPlan::from_length_decision(&inline, PlaceholderLifecycle::Completed),
            OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Completed
            }
        ));

        let compact = LengthPolicyDecision::Compact {
            char_count: 3000,
            compact_char_limit: 2000,
            summary_available: false,
            fallback_used: FallbackUsed::LengthCompacted,
        };
        assert!(matches!(
            OutputPlan::from_length_decision(&compact, PlaceholderLifecycle::Aborted),
            OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Aborted
            }
        ));

        let split = LengthPolicyDecision::Split {
            char_count: 5000,
            chunk_char_limit: 2000,
            chunk_count: 3,
            fallback_used: FallbackUsed::LengthSplit,
        };
        assert!(matches!(
            OutputPlan::from_length_decision(&split, PlaceholderLifecycle::Completed),
            OutputPlan::SendNewChunks {
                chunk_count: 3,
                delete_anchor: false
            }
        ));

        let reject = LengthPolicyDecision::RejectOverLimit {
            char_count: 9999,
            inline_char_limit: 2000,
        };
        assert!(matches!(
            OutputPlan::from_length_decision(&reject, PlaceholderLifecycle::Completed),
            OutputPlan::NoOp
        ));
    }

    fn debug_outcome(o: &DeliveryOutcome) -> &'static str {
        match o {
            DeliveryOutcome::Delivered { .. } => "Delivered",
            DeliveryOutcome::FreshDelivered { .. } => "FreshDelivered",
            DeliveryOutcome::NotDelivered { .. } => "NotDelivered",
            DeliveryOutcome::Transient { .. } => "Transient",
            DeliveryOutcome::Unknown { fell_back: true } => "Unknown{fell_back:true}",
            DeliveryOutcome::Unknown { fell_back: false } => "Unknown{fell_back:false}",
            DeliveryOutcome::Skipped => "Skipped",
        }
    }
}
