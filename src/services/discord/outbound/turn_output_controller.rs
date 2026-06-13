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
use super::super::placeholder_controller::{PlaceholderKey, PlaceholderLifecycle};
use super::super::turn_finalizer::TurnKey;
use super::super::{DeliveryLeaseCell, LeaseHolder, LeaseOutcome, lease_now_ms};
use super::decision::LengthPolicyDecision;

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
pub(in crate::services::discord) struct TurnOutputCtx<'a> {
    pub(in crate::services::discord) turn: TurnKey,
    /// Durable relay-owner identity carried for the durable-lease join (Phase
    /// B) and owner-scoped routing at cutover (A2); not read by the A1
    /// skeleton itself.
    #[allow(dead_code)] // #3089 A1: read by owner routing / durable lease from A2/B.
    pub(in crate::services::discord) owner: RelayOwnerKind,
    pub(in crate::services::discord) holder: LeaseHolder,
    pub(in crate::services::discord) lease: &'a DeliveryLeaseCell,
    pub(in crate::services::discord) channel_id: ChannelId,
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
pub(in crate::services::discord) async fn deliver_turn_output<G: TurnGateway + ?Sized>(
    gateway: &G,
    ctx: TurnOutputCtx<'_>,
) -> DeliveryOutcome {
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
            // commit() verifies the full (holder, turn, range) identity and
            // records the Delivered outcome. This is the offset advance: the
            // committed frontier moves to `end`. It runs synchronously here,
            // BEFORE the post-send placeholder-transition / cleanup awaits
            // below, so a post-send await can never land before the advance
            // (the #3143 fence).
            let committed =
                ctx.lease
                    .commit(ctx.holder, ctx.turn, start, end, LeaseOutcome::Delivered);
            debug_assert!(committed, "delivered commit must match the acquired lease");

            // ---- post-send work (AFTER the inline commit) ----------------
            post_send_finalize(gateway, &ctx).await;
            ctx.lease.release(ctx.holder, ctx.turn, start, end);
            DeliveryOutcome::Delivered { committed_to: end }
        }
        TransportResult::Transient => {
            // I2: ambiguous-but-retriable. Do NOT commit/advance — release the
            // lease so a retry can re-acquire from `start`.
            ctx.lease.release(ctx.holder, ctx.turn, start, end);
            DeliveryOutcome::Transient {
                retry_from_offset: start,
            }
        }
        TransportResult::Unknown => {
            // I2: ambiguous (drop / panic / partial). Release WITHOUT commit so
            // the offset never advances.
            ctx.lease.release(ctx.holder, ctx.turn, start, end);
            DeliveryOutcome::Unknown
        }
    }
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
async fn drive_transport<G: TurnGateway + ?Sized>(
    gateway: &G,
    ctx: &TurnOutputCtx<'_>,
    chunk_count: usize,
) -> TransportResult {
    match (&ctx.plan, &ctx.placeholder) {
        (OutputPlan::Replace { .. }, PlaceholderSlot::Active { message_id, .. }) => {
            match gateway
                .replace_message_with_outcome(ctx.channel_id, *message_id, ctx.body)
                .await
            {
                Ok(_) => TransportResult::Delivered,
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
                Ok(ids) if ids.len() >= chunk_count.min(1) => TransportResult::Delivered,
                // A short write (fewer messages than chunks) is a partial,
                // ambiguous result — never advance on it (I2).
                Ok(_) => TransportResult::Unknown,
                Err(_) => transient_or_unknown(ctx),
            }
        }
        (OutputPlan::NoOp, _) => TransportResult::Delivered,
    }
}

/// Classify a transport error into the ambiguous halves. A1 keeps the rule
/// conservative (design I3): anything we cannot prove transient is treated as
/// `Unknown` so the offset never advances. The owner-specific edit-fail policy
/// only influences post-send placeholder cleanup, never the advance decision.
fn transient_or_unknown(_ctx: &TurnOutputCtx<'_>) -> TransportResult {
    // A1 has no transport-error taxonomy wired (owners land from A2). Be
    // conservative: a bare Err is ambiguous → Unknown (never advance, I2).
    TransportResult::Unknown
}

/// Post-send finalization: placeholder terminal transition + edit-fail
/// fallback cleanup. Runs ONLY after the inline commit (I1). Best-effort —
/// failures here never un-advance the already-committed offset.
///
/// This is an `async` step with a real post-send await (`gateway.edit_message`)
/// — the very kind of await I1 forbids the commit from landing AFTER. The
/// controller calls it only once the inline commit above has already advanced
/// the offset, so this await can never re-open #3143.
async fn post_send_finalize<G: TurnGateway + ?Sized>(gateway: &G, ctx: &TurnOutputCtx<'_>) {
    if let (OutputPlan::Replace { lifecycle }, PlaceholderSlot::Active { message_id, key }) =
        (&ctx.plan, &ctx.placeholder)
    {
        // Drive the placeholder card to its terminal lifecycle state. Only
        // terminal targets are valid transitions; a non-terminal `lifecycle`
        // (e.g. Active) is left untouched here.
        if matches!(
            lifecycle,
            PlaceholderLifecycle::Completed
                | PlaceholderLifecycle::TimedOut
                | PlaceholderLifecycle::Aborted
        ) {
            // A1 skeleton seam: finalize the card with a post-send `edit`
            // await. The cutover owner (A2+) injects a `PlaceholderController`
            // and routes this through `PlaceholderController.transition`
            // instead; the call shape (a post-send await on the gateway, after
            // the inline commit) is what A1 pins. `edit_fail_policy` governs
            // whether a failed edit deletes the now-stale original (the #2757
            // fence) — owners exercise both arms from A2.
            let finalize_text = ctx.body;
            if gateway
                .edit_message(ctx.channel_id, *message_id, finalize_text)
                .await
                .is_err()
            {
                match ctx.edit_fail_policy {
                    EditFailPlaceholderPolicy::DeleteIfProvenStale => {
                        let _ = gateway.delete_message(ctx.channel_id, *message_id).await;
                    }
                    EditFailPlaceholderPolicy::PreserveAlways => { /* #2757: keep the original */ }
                }
            }
            let _ = key;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::LeaseSnapshot;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::GatewayFuture;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

    /// A fake `TurnGateway` that SHARES the same `DeliveryLeaseCell` the
    /// controller drives (via `Arc`), so each gateway method can READ the lease
    /// state at the exact moment the controller awaits it. This is what lets us
    /// prove I1 without any unsafe pointer: the transport-send method observes
    /// the lease BEFORE the inline commit, and the post-send `edit_message`
    /// await observes it AFTER.
    struct ObservingGateway {
        lease: Arc<DeliveryLeaseCell>,
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
        /// when false, the transport send returns Err (drives the I2 path).
        transport_ok: bool,
    }

    impl ObservingGateway {
        fn new(lease: Arc<DeliveryLeaseCell>, transport_ok: bool) -> Self {
            Self {
                lease,
                clock: AtomicUsize::new(1),
                committed_at_send: AtomicBool::new(false),
                send_step: AtomicUsize::new(0),
                committed_at_post_send_await: AtomicBool::new(false),
                post_send_await_step: AtomicUsize::new(0),
                post_send_await_seen: AtomicBool::new(false),
                transport_ok,
            }
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
                // FIRST post-send await point (driven by post_send_finalize).
                // I1 requires the inline commit ALREADY ran, so the lease must
                // read Committed{Delivered} here.
                tokio::task::yield_now().await;
                if !self.post_send_await_seen.swap(true, Ordering::SeqCst) {
                    self.post_send_await_step
                        .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                    self.committed_at_post_send_await
                        .store(self.lease_is_committed_delivered(), Ordering::SeqCst);
                }
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
                self.send_step
                    .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                self.committed_at_send
                    .store(self.lease_is_committed_delivered(), Ordering::SeqCst);
                if self.transport_ok {
                    Ok(ReplaceLongMessageOutcome::EditedOriginal)
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
        let lease = Arc::new(DeliveryLeaseCell::new(channel));
        let gateway = ObservingGateway::new(lease.clone(), true);
        let body = "hello turn output";
        let placeholder_msg = MessageId::new(7777);

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Sink,
            lease: &lease,
            channel_id: channel,
            // Active placeholder + a terminal lifecycle so post_send_finalize
            // performs its post-send `edit_message` await.
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key: placeholder_key(channel, placeholder_msg),
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

        // The post-send await actually ran (post_send_finalize edited the card).
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
        let lease = Arc::new(DeliveryLeaseCell::new(channel));
        // transport fails → controller classifies conservatively as Unknown.
        let gateway = ObservingGateway::new(lease.clone(), false);
        let body = "ambiguous turn output";
        let placeholder_msg = MessageId::new(8888);

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            owner: RelayOwnerKind::Watcher,
            holder: LeaseHolder::Sink,
            lease: &lease,
            channel_id: channel,
            placeholder: PlaceholderSlot::Active {
                message_id: placeholder_msg,
                key: placeholder_key(channel, placeholder_msg),
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
        let lease = Arc::new(DeliveryLeaseCell::new(channel));
        let gateway = ObservingGateway::new(lease.clone(), true);
        let body = "skipped";

        let ctx = TurnOutputCtx {
            turn: turn_key(channel),
            owner: RelayOwnerKind::None,
            holder: LeaseHolder::Sink,
            lease: &lease,
            channel_id: channel,
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
