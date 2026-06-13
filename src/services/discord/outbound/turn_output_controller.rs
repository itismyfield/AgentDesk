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
use super::super::turn_finalizer::TurnKey;
use super::super::{DeliveryLeaseCell, LeaseHolder, LeaseOutcome, LeaseSnapshot, lease_now_ms};
use super::decision::LengthPolicyDecision;

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
        turn: TurnKey,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        deadline_ms: u64,
    ) -> bool;
    fn commit(
        &self,
        holder: LeaseHolder,
        turn: TurnKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    ) -> bool;
    fn release(&self, holder: LeaseHolder, turn: TurnKey, start: u64, end: u64) -> bool;
    #[allow(dead_code)] // #3089 A1: read by the controller's own tests only.
    fn read(&self) -> LeaseSnapshot;
}

impl DeliveryLease for DeliveryLeaseCell {
    fn try_acquire(
        &self,
        turn: TurnKey,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        deadline_ms: u64,
    ) -> bool {
        DeliveryLeaseCell::try_acquire(self, turn, holder, start, end, deadline_ms)
    }
    fn commit(
        &self,
        holder: LeaseHolder,
        turn: TurnKey,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
    ) -> bool {
        DeliveryLeaseCell::commit(self, holder, turn, start, end, outcome)
    }
    fn release(&self, holder: LeaseHolder, turn: TurnKey, start: u64, end: u64) -> bool {
        DeliveryLeaseCell::release(self, holder, turn, start, end)
    }
    fn read(&self) -> LeaseSnapshot {
        DeliveryLeaseCell::read(self)
    }
}

/// Maximum wall time (process-monotonic ms) the controller holds the delivery
/// lease for a single `deliver_turn_output` attempt before a reconciler could
/// reclaim it. A1 never reclaims (no owner is wired), but the acquire still
/// records a deadline so the lease identity matches the #3041 cell contract.
const TURN_OUTPUT_LEASE_TTL_MS: u64 = 60_000;

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
    /// Replace/edit the live placeholder in place (Inline body that fits a
    /// single message). The `lifecycle` distinguishes the three replace
    /// variants (cancel / prompt-too-long / normal) so a cutover owner can
    /// drive the correct terminal placeholder state (recon risk #5).
    Replace { lifecycle: PlaceholderLifecycle },
    /// Send `chunk_count` new chunked messages (Split body over the inline
    /// limit).
    SendNewChunks { chunk_count: usize },
    /// Nothing to deliver (empty / suppressed body).
    NoOp,
}

impl OutputPlan {
    /// Map an `outbound::decide_policy` length decision into an `OutputPlan`.
    ///
    /// - `Inline` → `Replace` (fits a single message; edit the placeholder in
    ///   place). The replace `lifecycle` is supplied by the caller because the
    ///   length decision alone cannot tell cancel / prompt-too-long / normal
    ///   apart.
    /// - `Split` → `SendNewChunks { chunk_count }`.
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
            },
            LengthPolicyDecision::FileAttachment { .. }
            | LengthPolicyDecision::RejectOverLimit { .. } => OutputPlan::NoOp,
        }
    }
}

/// The three-way committed result of a delivery attempt. The returned outcome
/// is ALREADY committed (I1): `Delivered` means the lease was committed
/// `Delivered` and the offset advanced before any post-send await ran.
///
/// `Transient` (and its `retry_from_offset`) is part of the contract owners
/// consume from A2; A1 (no owner wired) has no transient transport
/// classification yet, so that arm is dormant until cutover.
#[allow(dead_code)] // #3089 A1: Transient arm dormant; owners wire it at A2.
pub(in crate::services::discord) enum DeliveryOutcome {
    /// Confirmed delivered to Discord; the committed offset advanced to
    /// `committed_to`.
    Delivered { committed_to: u64 },
    /// A transient/retriable failure; the offset did NOT advance. The owner
    /// may retry from `retry_from_offset`.
    Transient { retry_from_offset: u64 },
    /// Ambiguous (drop / panic / partial). I2: the offset did NOT advance.
    Unknown,
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

/// Borrowed delivery context for one `deliver_turn_output` call. The controller
/// drives the borrowed [`DeliveryLeaseCell`] through acquire → send → commit →
/// release internally (I1).
pub(in crate::services::discord) struct TurnOutputCtx<
    'a,
    L: DeliveryLease + ?Sized = DeliveryLeaseCell,
> {
    pub(in crate::services::discord) turn: TurnKey,
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
    let (start, end) = ctx.send_range;

    // NoOp short-circuits before touching the lease — nothing to deliver.
    let chunk_count = match &ctx.plan {
        OutputPlan::NoOp => return DeliveryOutcome::Skipped,
        OutputPlan::Replace { .. } => 1usize,
        OutputPlan::SendNewChunks { chunk_count } => *chunk_count,
    };
    if ctx.body.is_empty() {
        return DeliveryOutcome::Skipped;
    }

    // ---- acquire ---------------------------------------------------------
    let deadline_ms = lease_now_ms().saturating_add(TURN_OUTPUT_LEASE_TTL_MS);
    if !ctx
        .lease
        .try_acquire(ctx.turn, ctx.holder, start, end, deadline_ms)
    {
        // Another holder owns this (channel, turn, range); do not advance.
        return DeliveryOutcome::Transient {
            retry_from_offset: start,
        };
    }

    // ---- send (transport) ------------------------------------------------
    // Any post-send work (placeholder terminal transition, fallback cleanup,
    // release) happens AFTER the inline commit below (I1).
    let transport = drive_transport(gateway, &ctx, chunk_count).await;

    match transport {
        TransportResult::Delivered => {
            // ---- I1: commit + advance INLINE, before any post-send await --
            // The single commit+advance authority lives in `commit_and_finalize`
            // so the ONLY place the lease ever transitions to `Committed` is
            // immediately followed by the post-send finalize await + release.
            // That structural pairing is what makes a "commit on the
            // non-advance arm" mutation observable to the tests: every commit
            // is always trailed by a gateway await a recorder can witness (I1 /
            // review-fix M4).
            commit_and_finalize(gateway, &ctx, start, end).await;
            DeliveryOutcome::Delivered { committed_to: end }
        }
        TransportResult::Transient => {
            // I2: ambiguous-but-retriable. Do NOT commit/advance — release the
            // lease so a retry can re-acquire from `start`. No commit happens on
            // this arm: it never calls `commit_and_finalize`.
            ctx.lease.release(ctx.holder, ctx.turn, start, end);
            DeliveryOutcome::Transient {
                retry_from_offset: start,
            }
        }
        TransportResult::Unknown => {
            // I2: ambiguous (drop / panic / partial). Release WITHOUT commit so
            // the offset never advances. No commit happens on this arm.
            ctx.lease.release(ctx.holder, ctx.turn, start, end);
            DeliveryOutcome::Unknown
        }
    }
}

/// The SINGLE commit+advance authority (I1). Commits the lease `Delivered`
/// (the offset advance to `end`), then runs the post-send finalize await and
/// releases — in that fixed order. Keeping the commit and the trailing
/// finalize/release in one fn means every successful `commit` is structurally
/// paired with a post-send gateway await, so a mutation that commits on the
/// ambiguous arm (Transient/Unknown) is always visible to a gateway-side commit
/// recorder (review-fix M4: no silent commit-then-release).
async fn commit_and_finalize<G, L>(gateway: &G, ctx: &TurnOutputCtx<'_, L>, start: u64, end: u64)
where
    G: TurnGateway + ?Sized,
    L: DeliveryLease + ?Sized,
{
    // commit() verifies the full (holder, turn, range) identity and records the
    // Delivered outcome. This is the offset advance: the committed frontier
    // moves to `end`. It runs synchronously here, BEFORE the post-send
    // placeholder-transition / cleanup awaits below, so a post-send await can
    // never land before the advance (the #3143 fence).
    let committed = ctx
        .lease
        .commit(ctx.holder, ctx.turn, start, end, LeaseOutcome::Delivered);
    debug_assert!(committed, "delivered commit must match the acquired lease");

    // ---- post-send work (AFTER the inline commit) -----------------------
    post_send_finalize(gateway, ctx).await;
    ctx.lease.release(ctx.holder, ctx.turn, start, end);
}

/// Internal three-way transport result, before any lease commit.
///
/// A1's conservative classifier (`transient_or_unknown`) only ever produces
/// `Delivered`/`Unknown`; the `Transient` arm is wired once owners bring a real
/// transport-error taxonomy at A2.
#[allow(dead_code)] // #3089 A1: Transient arm dormant until A2 transport taxonomy.
enum TransportResult {
    Delivered,
    Transient,
    Unknown,
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
                Ok(outcome) => classify_replace_outcome(&outcome),
                Err(_) => transient_or_unknown(ctx),
            }
        }
        // Replace requested but no live placeholder to edit → fall back to a
        // fresh send of the single inline body.
        (OutputPlan::Replace { .. }, PlaceholderSlot::None) => {
            match gateway.send_message(ctx.channel_id, ctx.body).await {
                Ok(_) => TransportResult::Delivered,
                Err(_) => transient_or_unknown(ctx),
            }
        }
        (OutputPlan::SendNewChunks { .. }, slot) => {
            let anchor = match slot {
                PlaceholderSlot::Active { message_id, .. } => *message_id,
                PlaceholderSlot::None => MessageId::new(1),
            };
            match gateway
                .send_long_message_with_rollback(ctx.channel_id, anchor, ctx.body)
                .await
            {
                // A Split body MUST land all `chunk_count` messages to be
                // Delivered. A short write (fewer message IDs than chunks) is a
                // PARTIAL send — ambiguous — and must NEVER advance (I2,
                // review-fix H1). `chunk_count` from `LengthPolicyDecision::Split`
                // is always >= 1, so this is the exact-or-more contract.
                Ok(ids) if ids.len() >= chunk_count => TransportResult::Delivered,
                Ok(_) => TransportResult::Unknown,
                Err(_) => transient_or_unknown(ctx),
            }
        }
        (OutputPlan::NoOp, _) => TransportResult::Delivered,
    }
}

/// Map a `replace_message_with_outcome` success into the controller's transport
/// classification, mirroring the EXACT semantics the existing owners already
/// give each `ReplaceLongMessageOutcome` variant (review-fix H2). The catch-all
/// `Ok(_) => Delivered` was wrong: `PartialContinuationFailure` is a
/// not-delivered / retry-preserving result for every owner, never an advance.
///
/// Owner-mapping evidence (the controller's first cutover target is the sink):
/// - `EditedOriginal` → delivered:
///   `session_relay_sink.rs:863` (`Delivered` + `advance_after_confirmed_post`),
///   `turn_bridge/terminal_delivery.rs:131` (committed = true),
///   `formatting.rs:1785` (`Ok(())`).
/// - `SentFallbackAfterEditFailure` → delivered (the fallback POST carried the
///   response): `session_relay_sink.rs:905` (`Delivered` +
///   `advance_after_confirmed_post`), `formatting.rs:1786` (`Ok(())`).
///   (turn_bridge treats it as a non-commit cleanup case; A1 follows the sink,
///   the cutover-order-first owner — see commit message decision note.)
/// - `PartialContinuationFailure` → ambiguous, NEVER advance (I2):
///   `session_relay_sink.rs:956` (`RelaySinkError::Transient`),
///   `turn_bridge/terminal_delivery.rs:155` + the
///   `partial_continuation_failure_does_not_commit_terminal_delivery` test at
///   `:891` (committed = false), `formatting.rs:1787` (`Err`).
fn classify_replace_outcome(
    outcome: &crate::services::discord::formatting::ReplaceLongMessageOutcome,
) -> TransportResult {
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    match outcome {
        ReplaceLongMessageOutcome::EditedOriginal
        | ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. } => {
            TransportResult::Delivered
        }
        // Partial continuation failure: chunks were sent then a continuation
        // failed mid-stream. Every owner treats this as not-delivered and
        // preserves the retry offset. Map to Unknown so the offset never
        // advances (I2). Explicit — no catch-all.
        ReplaceLongMessageOutcome::PartialContinuationFailure { .. } => TransportResult::Unknown,
    }
}

/// Classify a transport error into the ambiguous halves. A1 keeps the rule
/// conservative (design I3): anything we cannot prove transient is treated as
/// `Unknown` so the offset never advances. The owner-specific edit-fail policy
/// only influences post-send placeholder cleanup, never the advance decision.
fn transient_or_unknown<L: DeliveryLease + ?Sized>(_ctx: &TurnOutputCtx<'_, L>) -> TransportResult {
    // A1 has no transport-error taxonomy wired (owners land from A2). Be
    // conservative: a bare Err is ambiguous → Unknown (never advance, I2).
    TransportResult::Unknown
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
        release_calls: AtomicUsize,
    }

    impl RecordingLease {
        fn new(channel: ChannelId) -> Self {
            Self {
                inner: DeliveryLeaseCell::new(channel),
                commit_calls: AtomicUsize::new(0),
                delivered_commit_calls: AtomicUsize::new(0),
                release_calls: AtomicUsize::new(0),
            }
        }
    }

    impl DeliveryLease for RecordingLease {
        fn try_acquire(
            &self,
            turn: TurnKey,
            holder: LeaseHolder,
            start: u64,
            end: u64,
            deadline_ms: u64,
        ) -> bool {
            self.inner
                .try_acquire(turn, holder, start, end, deadline_ms)
        }
        fn commit(
            &self,
            holder: LeaseHolder,
            turn: TurnKey,
            start: u64,
            end: u64,
            outcome: LeaseOutcome,
        ) -> bool {
            self.commit_calls.fetch_add(1, Ordering::SeqCst);
            if outcome == LeaseOutcome::Delivered {
                self.delivered_commit_calls.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.commit(holder, turn, start, end, outcome)
        }
        fn release(&self, holder: LeaseHolder, turn: TurnKey, start: u64, end: u64) -> bool {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.release(holder, turn, start, end)
        }
        fn read(&self) -> LeaseSnapshot {
            self.inner.read()
        }
    }

    fn turn_key(channel_id: ChannelId) -> TurnKey {
        TurnKey::new(channel_id, 7, 1)
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
        /// step counter — proves the temporal order of the observations.
        clock: AtomicUsize,
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
    }

    impl ObservingGateway {
        fn new(lease: Arc<dyn DeliveryLease + Send + Sync>, transport_ok: bool) -> Self {
            Self {
                lease,
                clock: AtomicUsize::new(1),
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
            }
        }

        /// After this is called, `edit_message` fails permanently so the
        /// placeholder `transition` reports `EditFailed` (M3 policy arms).
        fn fail_edits_from_now(&self) {
            self.edit_fails.store(true, Ordering::SeqCst);
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
                Ok(())
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

        fn add_reaction<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _e: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async move {})
        }
        fn remove_reaction<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _e: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async move {})
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
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;

        // The returned outcome is already committed/advanced to `end`.
        match outcome {
            DeliveryOutcome::Delivered { committed_to } => {
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
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Unknown),
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
    /// default (one `send_message`, so exactly ONE message id). A
    /// `SendNewChunks { chunk_count: 3 }` plan therefore receives 1 id for a
    /// 3-chunk send — the exact partial the old `chunk_count.min(1)` bug
    /// committed as Delivered. With the fix (`ids.len() >= chunk_count`) it must
    /// classify Unknown, leave the lease uncommitted, and release to Unleased.
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
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::None,
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::SendNewChunks { chunk_count: 3 },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Unknown),
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
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Sink,
            lease: lease.as_ref(),
            channel_id: channel,
            placeholder_controller: &controller,
            placeholder: PlaceholderSlot::None,
            body,
            send_range: (0, body.len() as u64),
            plan: OutputPlan::SendNewChunks { chunk_count: 1 },
            edit_fail_policy: EditFailPlaceholderPolicy::PreserveAlways,
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        match outcome {
            DeliveryOutcome::Delivered { committed_to } => {
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
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        assert!(
            matches!(outcome, DeliveryOutcome::Unknown),
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

    /// H2 companion — `SentFallbackAfterEditFailure` is Delivered for the sink
    /// (session_relay_sink.rs:905 advances), so the controller commits it. This
    /// pins the explicit-match contract: only `PartialContinuationFailure` is
    /// non-advance; the two success variants advance.
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
                });
        let controller = PlaceholderController::default();
        prime_active(&controller, &gateway, key.clone()).await;
        let body = "replace body delivered via fallback post";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
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
        };

        let outcome = deliver_turn_output(&gateway, ctx).await;
        match outcome {
            DeliveryOutcome::Delivered { committed_to } => {
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
            "H2: SentFallbackAfterEditFailure must commit Delivered exactly once"
        );
        assert!(
            matches!(lease.read(), LeaseSnapshot::Unleased),
            "delivered fallback must release the lease"
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
            OutputPlan::SendNewChunks { chunk_count: 3 }
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
            DeliveryOutcome::Transient { .. } => "Transient",
            DeliveryOutcome::Unknown => "Unknown",
            DeliveryOutcome::Skipped => "Skipped",
        }
    }
}
