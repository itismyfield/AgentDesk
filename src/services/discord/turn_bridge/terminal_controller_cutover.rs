//! #3089 A5 turn_bridge terminal cutover to the unified turn-output controller.
//!
//! Owns the A5 cutover decision, lease gate, heartbeat adapter, and controller
//! write-backs so `terminal_delivery.rs` remains below its production LoC cap.

use super::*;
// #5071 T1 S4: the ONE door from this file to the `#[cfg(unix)]` journal.
mod unix_journal;

use std::sync::Arc;

use super::super::gateway::TurnGateway;
use super::super::inflight::RelayOwnerKind;
use super::super::outbound::delivery_record as dr;
use super::super::outbound::turn_output_controller as toc;
use super::super::placeholder_controller::{PlaceholderKey, PlaceholderLifecycle};
use super::super::turn_finalizer::TurnKey;
use crate::services::discord::{
    DeliveryLeaseCell, DeliveryLeaseHeartbeat, DeliveryLeaseKey, LeaseHolder, lease_now_ms,
};
use unix_journal::{
    Disposition, begin_controller_terminal as journal_begin,
    settle_controller_terminal as journal_settle,
};
#[cfg(unix)]
#[rustfmt::skip]
pub(super) fn begin_pinned_terminal(
    shared: &SharedData, provider: &ProviderKind, long: bool,
    channels: (ChannelId, ChannelId), range: (u64, u64),
) -> impl FnOnce(Option<MessageId>, bool) {
    let kind = if long { Disposition::LongChunks } else { Disposition::ShortReplace };
    let mut journal = journal_begin(shared, provider, kind, channels, Some(range));
    move |message_id, committed| drop(journal_settle(&mut journal, message_id, committed))
}

/// #3089 A5: the bridge short-replace cut-over decision, computed at the site-5
/// lease-acquire site (mod.rs ~6134).
///
/// Terms (mirroring the legacy short-replace branch arm at mod.rs:6126-6245):
/// - `will_short_replace` — we are in the `can_chain_locally` short-replace arm
///   (NOT the long-chunk send-new-chunks arm; mod.rs:6023/6024). I.e.
///   `can_chain_locally && !should_send_new_chunks`. The long-chunk arm is routed
///   by [`bridge_long_chunks_cutover_decision`] when the A5 flag is ON.
/// - `ordered_range` — `tmux_last_offset > turn_start_offset` (a real `[start,end)`).
///   The legacy `NoRange` arm (deliver-without-advance) is NOT expressible (the
///   controller commits IFF the lease advances) → EXCLUDED (stays legacy via the
///   `bridge_terminal_lease_range` gate returning None when the range is empty).
/// - `has_placeholder` — there is a live placeholder card to edit. Site 5 always
///   edits `current_msg_id`, so this is `true` whenever the arm runs; kept as an
///   explicit predicate input for symmetry with A4 and so a future
///   placeholder-less arm cannot silently slip into the cutover.
/// - `body_non_empty` — the formatted `delivery_response` is non-empty. An empty
///   body short-circuits the controller to `Skipped` (no advance), whereas the
///   legacy short-replace edits even an (already non-empty, since we are in the
///   non-empty `else` at mod.rs:6011) body; the non-empty branch guarantees this,
///   but we pin it so empty bodies (should one ever reach here) stay legacy.
/// - `can_chain_locally` — the bridge will direct-edit (NOT headless enqueue;
///   mod.rs:6023). The headless arm (mod.rs:6247) is EXCLUDED.
#[allow(clippy::too_many_arguments)]
pub(super) fn bridge_short_replace_cutover(
    can_chain_locally: bool,
    will_short_replace: bool,
    ordered_range: bool,
    has_placeholder: bool,
    body_non_empty: bool,
) -> bool {
    can_chain_locally && will_short_replace && ordered_range && has_placeholder && body_non_empty
}

/// #3089 A5: the full short-replace cut-over decision at the site-5 lease-acquire
/// site. It derives `will_short_replace` EXACTLY as the send arm (mod.rs:6024:
/// `!super::terminal_delivery::terminal_delivery_should_send_new_chunks`) so the cutover and the legacy arm
/// agree on which body is "short". Kept here (not inlined) so the frozen
/// `mod.rs` call site stays a single line.
#[allow(clippy::too_many_arguments)]
pub(super) fn bridge_short_replace_cutover_decision(
    can_chain_locally: bool,
    formatted_response: &str,
    ordered_range: bool,
    has_placeholder: bool,
) -> bool {
    // The send arm is the short-replace arm IFF it does NOT send new chunks.
    let will_short_replace = !super::terminal_delivery::terminal_delivery_should_send_new_chunks(
        can_chain_locally,
        formatted_response,
    );
    bridge_short_replace_cutover(
        can_chain_locally,
        will_short_replace,
        ordered_range,
        has_placeholder,
        !formatted_response.is_empty(),
    )
}

/// #3998 S1-d: bridge long-chunk cut-over decision. This is the same owner flag
/// as A5 short-replace, but applies to the legacy send-new-chunks + placeholder
/// delete arm now that `SendNewChunks { delete_anchor: true }` exists.
///
/// Retained exclusions: `NoRange` (no advance authority; #4048), headless (no
/// direct Discord POST), and empty body (consistent with A2b/A3 skip parity).
pub(super) fn bridge_long_chunks_cutover_decision(
    can_chain_locally: bool,
    formatted_response: &str,
    ordered_range: bool,
    has_placeholder: bool,
) -> bool {
    can_chain_locally
        && super::terminal_delivery::terminal_delivery_should_send_new_chunks(
            can_chain_locally,
            formatted_response,
        )
        && ordered_range
        && has_placeholder
        && !formatted_response.is_empty()
}

/// #3089 A5: pure no-double-acquire gate. The legacy site-5 arm acquires its OWN
/// `BridgeDeliveryLease` over `cutover_range` (mod.rs ~6134). When the
/// short-replace branch is cut over, the CONTROLLER owns that single lease, so
/// the legacy acquire MUST be skipped — this returns `None` for any cut-over
/// turn. Extracted so the invariant is testable: dropping `!cutover_short_replace`
/// fails `cutover_skips_bridge_lease_acquire`. Mirrors A4's
/// `watcher_terminal_lease_range`. Scoped to site 5 ONLY — the other four bridge
/// lease-acquire sites (silent-turn, cancel/stop/prompt-too-long) are
/// byte-identical (they never call this); long-chunk has its own S1-d route.
pub(super) fn bridge_terminal_lease_range(
    cutover_range: Option<(u64, u64)>,
    cutover_short_replace: bool,
) -> Option<(u64, u64)> {
    cutover_range.filter(|_| !cutover_short_replace)
}

/// #3089 A5: adapts the bridge's `DeliveryLeaseHeartbeat` to [`toc::PostHeartbeat`].
/// Holds the `Arc` (the controller drives the lease behind a borrowed `&cell`) and
/// spawns the SAME `DeliveryLeaseHeartbeat::spawn` the legacy `BridgeDeliveryLease::acquire`
/// used (terminal_delivery.rs:529, #3041 P1-2 / #3151 — identical renew cadence);
/// the guard Drop aborts the renew task BEFORE the inline commit (#3151 ordering).
/// Mirrors A4's `WatcherPostHeartbeat`.
pub(super) struct BridgePostHeartbeat {
    pub(super) cell: Arc<DeliveryLeaseCell>,
}

impl toc::PostHeartbeat for BridgePostHeartbeat {
    fn start(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
    ) -> Box<dyn toc::PostHeartbeatGuard> {
        Box::new(BridgePostHeartbeatGuard {
            _heartbeat: DeliveryLeaseHeartbeat::spawn(self.cell.clone(), holder, key),
        })
    }
}

struct BridgePostHeartbeatGuard {
    _heartbeat: DeliveryLeaseHeartbeat,
}

impl toc::PostHeartbeatGuard for BridgePostHeartbeatGuard {}

/// #3089 A5: bridge short-replace via the turn-output controller, behaviourally
/// equal to the legacy site-5 `replace_message_with_outcome` arm (mod.rs
/// 6160-6245) — SAME transport, SAME per-channel cell as `LeaseHolder::Bridge`
/// acquired/committed/advanced/released ONCE (no double-acquire: the legacy
/// acquire is skipped via `bridge_terminal_lease_range`), SAME #3041 P1-2 / #3151
/// heartbeat.
///
/// #2757 byte-identical: `EditFailPlaceholderPolicy::PreserveAlways`. The bridge
/// short-replace NEVER deletes the original on edit-fail fallback (only the
/// separate long-chunk site-4 deletes the anchor), so the cutover passes
/// `PreserveAlways`; `DeleteIfProvenStale` stays dormant.
///
/// `FallbackCommitPolicy::NoCommitOnFallback` is the bridge's DISTINGUISHING
/// policy (proven by `sent_fallback_after_edit_failure_does_not_commit_terminal_delivery`
/// + `turn_bridge_replace_outcome_committed` returning `committed = false` on
/// `SentFallbackAfterEditFailure`, terminal_delivery.rs:143): the in-place edit
/// failed, so the terminal-delivery contract treats the card as not yet
/// committed → the offset must NOT advance. The controller maps
/// `SentFallbackAfterEditFailure` → `Unknown { fell_back: true }` (#3089 A5
/// controller extension), which `commit_and_finalize` releases WITHOUT committing
/// (no advance, I2) while surfacing that the body nonetheless landed.
///
/// `AcquireFailureMode::Transient` mirrors the legacy B2-skip arm
/// (mod.rs:6145-6159): a lost acquire means another holder (the watcher) owns the
/// range → do NOT re-send; the holder commits the offset.
///
/// `Replace { Active }` keeps `post_send_finalize` a no-op (the replace IS the
/// edit, like legacy; no terminal placeholder transition).
///
/// Advance: site 5's legacy advance flows through the lease commit IFF
/// `replace_committed` (EditedOriginal). On a CONFIRMED transport the controller
/// invokes this callback (never on Transient/Unknown, I2), so it runs the REAL
/// `super::terminal_delivery::advance_tmux_relay_confirmed_end(.., watcher_owner_channel_id, Some(end), ..)`
/// — the SAME monotonic-CAS, SAME `end` (`tmux_last_offset`), SAME channel as
/// legacy — and returns `true` → Delivered.
///
/// Channel split (codex r1 [High], matching legacy mod.rs site 5 EXACTLY):
/// - `channel_id` (the bridge's delivery/dispatch channel) is the EDIT TARGET —
///   `TurnOutputCtx.channel_id` (→ `replace_message_with_outcome(ctx.channel_id, ..)`,
///   controller:830, == legacy `replace_message_with_outcome(channel_id, ..)`
///   mod.rs:6180) and `PlaceholderKey.channel_id` (the placeholder card lives in
///   the delivery channel).
/// - `watcher_owner_channel_id` (the resolved tmux-session owner channel) is the
///   LEASE/ADVANCE AUTHORITY — the `cell` (keyed by `delivery_lease(watcher_owner_channel_id)`,
///   mod.rs:6105), the `TurnKey` (`TurnKey::new(watcher_owner_channel_id, ..)`,
///   mod.rs:6090), and the advance callback
///   (`advance_tmux_relay_confirmed_end(.., watcher_owner_channel_id, ..)` →
///   `tmux_relay_coord(watcher_owner_channel_id)`, == legacy
///   `commit_and_advance(.., watcher_owner_channel_id, ..)` mod.rs:6216).
///
/// These two CAN differ in production: a recovered/restored bridge that reuses an
/// existing watcher resolves the owner channel X for the lease while still
/// editing its own dispatch channel Y (mod.rs:2207-2213). Routing the edit
/// through `watcher_owner_channel_id` would edit the WRONG channel (or fail and
/// misclassify) — so the edit MUST use the delivery `channel_id`.
///
/// `gateway` is the bridge's already-constructed `Arc<dyn TurnGateway>` (passed as
/// `&dyn`); the test injects a fake driving the REAL controller + real cell.
#[allow(clippy::too_many_arguments)]
pub(super) async fn deliver_short_replace_via_controller(
    gateway: &dyn TurnGateway,
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    watcher_owner_channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    cell: &Arc<DeliveryLeaseCell>,
    placeholder_controller: &super::super::placeholder_controller::PlaceholderController,
    msg_id: MessageId,
    relay_text: &str,
    delivered_body: &str,
    turn: TurnKey,
    lease_key: Option<DeliveryLeaseKey>,
    start: u64,
    end: u64,
) -> toc::DeliveryOutcome {
    let holder = LeaseHolder::Bridge;
    // Self-heal like the legacy acquire (terminal_delivery.rs:516): reclaim an
    // EXPIRED prior holder before the controller's acquire (a stale dead lease
    // must not make this acquire lose and B2-skip a deliverable range).
    cell.reclaim_if_expired(lease_now_ms());
    let heartbeat = BridgePostHeartbeat { cell: cell.clone() };
    // Identity-gated advance: INLINE before any post-send await (I1). Site 5's
    // legacy advance is unconditional on a committed (EditedOriginal) replace, so
    // the callback runs the REAL `super::terminal_delivery::advance_tmux_relay_confirmed_end` to `end` (the
    // legacy `tmux_last_offset`) and returns `true` → Delivered. The controller
    // invokes this ONLY on confirmed transport (never Transient/Unknown, I2), and
    // the monotonic CAS makes it idempotent.
    let advance = |range: (u64, u64)| -> bool {
        debug_assert_eq!(range, (start, end));
        super::terminal_delivery::advance_tmux_relay_confirmed_end(
            shared,
            watcher_owner_channel_id,
            Some(end),
            tmux_session_name,
        );
        true
    };
    // #5071 T1 S4: open the controller family's shadow obligation BEFORE transport.
    // `unix_journal` — the journal is `#[cfg(unix)]` and the durable frontier write
    // below is not, so off unix these three sites are UNINSTRUMENTED.
    #[rustfmt::skip]
    let mut journal = unix_journal::begin_controller_terminal(shared, provider,
        Disposition::ShortReplace, (watcher_owner_channel_id, channel_id), Some((start, end)));
    let outcome = toc::deliver_turn_output(
        gateway,
        toc::TurnOutputCtx {
            turn,
            lease_key,
            // No `Bridge` variant exists on `RelayOwnerKind`; `None` preserves the
            // historical bridge-owned/default shape (observability only).
            owner: RelayOwnerKind::None,
            holder,
            // Lease cell is keyed by `watcher_owner_channel_id` (acquired by the
            // caller via `delivery_lease(watcher_owner_channel_id)`, mod.rs:6105).
            lease: &**cell,
            // EDIT TARGET = the bridge's delivery channel (codex r1 [High]): the
            // controller POSTs `replace_message_with_outcome(ctx.channel_id, ..)`
            // (controller:830), which legacy site 5 routes through `channel_id`
            // (mod.rs:6180), NOT `watcher_owner_channel_id`. These can differ for
            // a recovered/reused-watcher bridge (mod.rs:2207-2213).
            channel_id,
            placeholder_controller,
            placeholder: toc::PlaceholderSlot::Active {
                message_id: msg_id,
                key: PlaceholderKey {
                    provider: provider.clone(),
                    // The placeholder card lives in the delivery `channel_id`.
                    channel_id,
                    message_id: msg_id,
                },
            },
            body: relay_text,
            send_range: (start, end),
            // `Replace { Active }` → non-terminal → `post_send_finalize` no-ops,
            // matching the legacy edit-in-place (no terminal transition).
            plan: toc::OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Active,
            },
            // #2757: the bridge short-replace NEVER deletes the original on
            // edit-fail fallback (only the separate long-chunk site deletes).
            edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
            // The bridge's distinguishing policy: a fallback edit failure does NOT
            // commit → leave the offset un-advanced (terminal_delivery.rs:143).
            fallback_commit_policy: toc::FallbackCommitPolicy::NoCommitOnFallback,
            // B2 (single-holder): a lost acquire is another holder's range → do
            // NOT re-send. Mirrors the legacy site-5 B2-skip arm.
            acquire_failure_mode: toc::AcquireFailureMode::Transient,
            advance: Some(&advance),
            heartbeat: Some(&heartbeat),
        },
    )
    .await;
    // #3089 B2a: shadow-mirror durable delivered frontier — flag-gated, observe-only,
    // Delivered-only (I2), OFF=no-op. Keyed by `watcher_owner_channel_id` (the
    // OFFSET-AUTHORITY channel where `advance_tmux_relay_confirmed_end` advanced
    // `confirmed_end_offset`), NOT the edit-target `channel_id` — so the durable
    // frontier and the in-memory authority B2b fuses share one channel key.
    //
    // #3610 PR-1b: record the (anchor channel, anchor msg) PAIR — the real prod /
    // incident terminal path is THIS bridge cutover, and PR-1 left it null. The
    // anchor message terminal-replace edits in place is `msg_id` (the `Active`
    // placeholder slot's `message_id`, passed straight into
    // `TurnOutputCtx.placeholder = Active { message_id: msg_id, .. }` →
    // `replace_message_with_outcome(channel_id, msg_id, ..)`), and it LIVES IN the
    // edit-target `channel_id` (the bridge's delivery channel — the EDIT TARGET per
    // the `Channel split` doc above, NOT `watcher_owner_channel_id`). These two
    // channels are cleanly separated function parameters here, so the anchor pair is
    // unambiguous: `panel_channel_id = Some(channel_id)`, `panel_msg_id =
    // Some(msg_id)`. The frontier KEY stays `watcher_owner_channel_id` (offset
    // authority unchanged — B2b's fusion still shares one channel key); only the
    // recorded anchor pair points at the edit channel/message.
    let delivered = dr::outcome_is_shadow_delivered(&outcome);
    dr::shadow_mirror_delivered_frontier(
        shared,
        provider,
        watcher_owner_channel_id,
        tmux_session_name,
        (start, end),
        delivered,
        Some(msg_id.get()),
        Some(channel_id.get()),
        Some(delivered_body),
        // #4564: explicit inbound turn id from the turn snapshot — the ledger is
        // keyed by the delivery channel (`channel_id`), NOT `watcher_owner_channel_id`.
        Some(turn.user_msg_id),
    );
    unix_journal::settle_controller_terminal(&mut journal, Some(msg_id), delivered);
    outcome
}

/// #3998 S1-d: bridge long-chunk delivery via the turn-output controller. Mirrors
/// legacy site 4: acquire bridge lease, send all chunks with rollback, delete the
/// placeholder anchor best-effort after full chunk success, commit Delivered and
/// advance only on success; send failure commits NotDelivered and preserves the
/// anchor for retry.
#[allow(clippy::too_many_arguments)]
pub(super) async fn deliver_long_chunks_via_controller(
    gateway: &dyn TurnGateway,
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    watcher_owner_channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    cell: &Arc<DeliveryLeaseCell>,
    placeholder_controller: &super::super::placeholder_controller::PlaceholderController,
    msg_id: MessageId,
    relay_text: &str,
    delivered_body: &str,
    turn: TurnKey,
    lease_key: Option<DeliveryLeaseKey>,
    start: u64,
    end: u64,
) -> toc::DeliveryOutcome {
    let holder = LeaseHolder::Bridge;
    cell.reclaim_if_expired(lease_now_ms());
    let heartbeat = BridgePostHeartbeat { cell: cell.clone() };
    let advance = |range: (u64, u64)| -> bool {
        debug_assert_eq!(range, (start, end));
        super::terminal_delivery::advance_tmux_relay_confirmed_end(
            shared,
            watcher_owner_channel_id,
            Some(end),
            tmux_session_name,
        );
        true
    };
    let chunk_count = super::super::formatting::split_message(relay_text).len();
    #[rustfmt::skip]
    let mut journal = unix_journal::begin_controller_terminal(shared, provider,
        Disposition::LongChunks, (watcher_owner_channel_id, channel_id), Some((start, end)));
    let outcome = toc::deliver_turn_output(
        gateway,
        toc::TurnOutputCtx {
            turn,
            lease_key,
            owner: RelayOwnerKind::None,
            holder,
            lease: &**cell,
            channel_id,
            placeholder_controller,
            placeholder: toc::PlaceholderSlot::Active {
                message_id: msg_id,
                key: PlaceholderKey {
                    provider: provider.clone(),
                    channel_id,
                    message_id: msg_id,
                },
            },
            body: relay_text,
            send_range: (start, end),
            plan: toc::OutputPlan::SendNewChunks {
                chunk_count,
                delete_anchor: true,
            },
            edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: toc::FallbackCommitPolicy::NoCommitOnFallback,
            acquire_failure_mode: toc::AcquireFailureMode::Transient,
            advance: Some(&advance),
            heartbeat: Some(&heartbeat),
        },
    )
    .await;
    if let toc::DeliveryOutcome::Delivered {
        new_chunks: Some(chunks),
        ..
    } = &outcome
    {
        dr::record_long_chunk_terminal_delivery(
            shared,
            provider,
            watcher_owner_channel_id,
            channel_id,
            tmux_session_name,
            (start, end),
            chunks.tail_message_id.map(|m| m.get()),
            delivered_body,
            // #4564: explicit inbound turn id; ledger keyed by delivery `channel_id`.
            Some(turn.user_msg_id),
        );
        unix_journal::settle_controller_terminal(&mut journal, chunks.tail_message_id, true);
    }
    unix_journal::settle_controller_terminal(&mut journal, None, false); // no-op if settled
    outcome
}

/// #3998 S1-d: borrowed long-chunk locals the controller path writes back into.
pub(super) struct BridgeLongChunksLocals<'a> {
    pub(super) terminal_delivery_committed: &'a mut bool,
    pub(super) terminal_body_visible: &'a mut bool,
    pub(super) completion_footer_terminal_text: &'a mut Option<String>,
    pub(super) preserve_inflight_for_cleanup_retry: &'a mut bool,
    pub(super) bridge_skip_holder_owns_inflight: &'a mut bool,
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) inflight_response_sent_offset: &'a mut usize,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_bridge_long_chunks_controller(
    gateway: &dyn TurnGateway,
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    watcher_owner_channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    cell: &Arc<DeliveryLeaseCell>,
    placeholder_controller: &super::super::placeholder_controller::PlaceholderController,
    msg_id: MessageId,
    relay_text: &str,
    delivered_body: &str,
    full_response_len: usize,
    turn: TurnKey,
    start: u64,
    end: u64,
    single_message_panel_footer_mode: bool,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    lease_key: Option<DeliveryLeaseKey>,
    locals: BridgeLongChunksLocals<'_>,
) {
    let outcome = deliver_long_chunks_via_controller(
        gateway,
        shared,
        provider,
        channel_id,
        watcher_owner_channel_id,
        tmux_session_name,
        cell,
        placeholder_controller,
        msg_id,
        relay_text,
        delivered_body,
        turn,
        lease_key,
        start,
        end,
    )
    .await;
    apply_bridge_long_chunks_outcome(
        outcome,
        shared,
        provider,
        channel_id,
        msg_id,
        tmux_session_name,
        relay_text,
        full_response_len,
        single_message_panel_footer_mode,
        dispatch_id,
        session_key,
        turn_id,
        locals,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_bridge_long_chunks_legacy(
    lease_acquire: BridgeLeaseAcquire,
    gateway: &dyn TurnGateway,
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    watcher_owner_channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    msg_id: MessageId,
    relay_text: &str,
    delivered_body: &str,
    full_response_len: usize,
    single_message_panel_footer_mode: bool,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    // #4564: inbound turn id of the delivered turn (delivery channel = `channel_id`),
    // threaded from the caller's inflight snapshot so the completed-turn ledger is
    // keyed by the inbound channel, not `watcher_owner_channel_id`.
    ledger_user_msg_id: u64,
    locals: BridgeLongChunksLocals<'_>,
) {
    if matches!(lease_acquire, BridgeLeaseAcquire::Skip) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            channel_id = channel_id.get(),
            "  [{ts}] 🌉 #3041 B2: delivery lease held by another holder — bridge skipped duplicate long terminal send (channel {})",
            channel_id
        );
        *locals.preserve_inflight_for_cleanup_retry = true;
        *locals.bridge_skip_holder_owns_inflight = true;
        return;
    }
    let lease = match lease_acquire {
        BridgeLeaseAcquire::Held(lease) => Some(lease),
        _ => None,
    };
    #[rustfmt::skip]
    let mut journal = unix_journal::begin_controller_terminal(shared, provider,
        Disposition::LongChunksLegacy, (watcher_owner_channel_id, channel_id),
        lease.as_ref().map(|lease| lease.range()));
    match send_ordered_long_terminal_response(
        shared,
        gateway,
        provider,
        channel_id,
        msg_id,
        tmux_session_name,
        relay_text,
        dispatch_id,
        session_key,
        turn_id,
    )
    .await
    {
        Ok((_first, last_chunk_msg_id)) => {
            *locals.terminal_delivery_committed = true;
            *locals.terminal_body_visible = true;
            if single_message_panel_footer_mode {
                *locals.completion_footer_terminal_text = Some(relay_text.to_string());
            }
            *locals.response_sent_offset = full_response_len;
            *locals.inflight_response_sent_offset = full_response_len;
            if let Some(lease) = lease {
                let lease_range = lease.range();
                let committed = lease.commit_and_advance(
                    shared,
                    watcher_owner_channel_id,
                    tmux_session_name,
                    crate::services::discord::LeaseOutcome::Delivered,
                );
                if committed {
                    dr::record_long_chunk_terminal_delivery(
                        shared,
                        provider,
                        watcher_owner_channel_id,
                        channel_id,
                        tmux_session_name,
                        lease_range,
                        last_chunk_msg_id.map(|m| m.get()),
                        delivered_body,
                        Some(ledger_user_msg_id),
                    );
                    unix_journal::settle_controller_terminal(&mut journal, last_chunk_msg_id, true);
                }
            }
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ terminal long response send failed for channel {}: {} — preserving inflight for retry",
                channel_id,
                error
            );
            if let Some(lease) = lease {
                lease.commit_and_advance(
                    shared,
                    watcher_owner_channel_id,
                    tmux_session_name,
                    crate::services::discord::LeaseOutcome::NotDelivered,
                );
            }
            *locals.preserve_inflight_for_cleanup_retry = true;
        }
    }
    unix_journal::settle_controller_terminal(&mut journal, None, false); // no-op if settled
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_bridge_long_chunks_outcome(
    outcome: toc::DeliveryOutcome,
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    msg_id: MessageId,
    tmux_session_name: Option<&str>,
    relay_text: &str,
    full_response_len: usize,
    single_message_panel_footer_mode: bool,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    locals: BridgeLongChunksLocals<'_>,
) {
    match outcome {
        toc::DeliveryOutcome::Delivered {
            new_chunks: Some(chunks),
            ..
        } => {
            *locals.terminal_delivery_committed = true;
            *locals.terminal_body_visible = true;
            if single_message_panel_footer_mode {
                *locals.completion_footer_terminal_text = Some(relay_text.to_string());
            }
            *locals.response_sent_offset = full_response_len;
            *locals.inflight_response_sent_offset = full_response_len;
            record_bridge_long_chunk_delete_cleanup(
                shared,
                provider,
                channel_id,
                msg_id,
                tmux_session_name,
                chunks.anchor_delete_error,
            );
            crate::services::observability::emit_relay_delivery(
                provider.as_str(),
                channel_id.get(),
                dispatch_id,
                session_key,
                turn_id,
                chunks.first_message_id.map(|m| m.get()),
                "turn_bridge",
                "post",
                None,
                None,
                true,
                Some("terminal long response sent as ordered chunks"),
            );
        }
        toc::DeliveryOutcome::Transient { .. } => {
            *locals.preserve_inflight_for_cleanup_retry = true;
            *locals.bridge_skip_holder_owns_inflight = true;
        }
        toc::DeliveryOutcome::FreshDelivered { .. }
        | toc::DeliveryOutcome::NotDelivered { .. }
        | toc::DeliveryOutcome::Unknown { .. }
        | toc::DeliveryOutcome::Skipped
        | toc::DeliveryOutcome::Delivered { .. } => {
            *locals.preserve_inflight_for_cleanup_retry = true;
        }
    }
}

fn record_bridge_long_chunk_delete_cleanup(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<&str>,
    anchor_delete_error: Option<String>,
) {
    let outcome = match anchor_delete_error {
        Some(error) => super::super::placeholder_cleanup::classify_delete_error(&error),
        None => super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
    };
    if let super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Failed { class, detail } =
        &outcome
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ placeholder cleanup {} failed ({}) for channel {} msg {}: {}",
            super::super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteTerminal.as_str(),
            class.as_str(),
            channel_id.get(),
            message_id.get(),
            detail
        );
    }
    shared.ui.placeholder_cleanup.record(
        super::super::placeholder_cleanup::PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id,
            tmux_session_name: tmux_session_name.map(str::to_string),
            operation:
                super::super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteTerminal,
            outcome,
            source: "turn_bridge_terminal_long_send_controller_cleanup",
        },
    );
}

/// #3089 A5: borrowed `&mut` handles to the site-5 send-arm locals the controller
/// path writes back into. Bundled into one struct so the frozen `mod.rs` call site
/// stays small (LoC) while keeping the write-back explicit and testable. Mirrors
/// A4's `WatcherShortReplaceLocals`.
pub(super) struct BridgeShortReplaceLocals<'a> {
    pub(super) terminal_delivery_committed: &'a mut bool,
    pub(super) terminal_body_visible: &'a mut bool,
    pub(super) completion_footer_terminal_text: &'a mut Option<String>,
    pub(super) preserve_inflight_for_cleanup_retry: &'a mut bool,
    pub(super) bridge_skip_holder_owns_inflight: &'a mut bool,
    /// `inflight_state.response_sent_offset` — the dual-offset target.
    pub(super) inflight_response_sent_offset: &'a mut usize,
}

/// #3089 A5: run the controller short-replace then write the outcome back into the
/// site-5 send-arm locals — the production cut-over wiring. Maps `DeliveryOutcome`
/// → bridge locals reproducing the legacy site-5 behaviour EXACTLY (mod.rs
/// 6160-6245). `gateway` is the bridge's already-built `Arc<dyn TurnGateway>`.
///
/// Note on the cleanup record: the controller's `post_send_finalize` no-ops on
/// `Replace { Active }`, so this write-back records a `PlaceholderCleanupRecord` +
/// `emit_relay_delivery` event mirroring the legacy
/// `turn_bridge_replace_outcome_committed` per arm (Succeeded on EditedOriginal,
/// failed(..) on the fell_back / partial / transport-error arms). The cleanup
/// `detail` is descriptive (the upstream edit error is not threaded through the
/// `Unknown` variant — observability only; the dual-offset behaviour is exact).
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_bridge_short_replace_controller(
    gateway: &dyn TurnGateway,
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    watcher_owner_channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    cell: &Arc<DeliveryLeaseCell>,
    placeholder_controller: &super::super::placeholder_controller::PlaceholderController,
    msg_id: MessageId,
    relay_text: &str,
    delivered_body: &str,
    full_response_len: usize,
    turn: TurnKey,
    start: u64,
    end: u64,
    single_message_panel_footer_mode: bool,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    lease_key: Option<DeliveryLeaseKey>,
    locals: BridgeShortReplaceLocals<'_>,
) {
    let outcome = deliver_short_replace_via_controller(
        gateway,
        shared,
        provider,
        channel_id,
        watcher_owner_channel_id,
        tmux_session_name,
        cell,
        placeholder_controller,
        msg_id,
        relay_text,
        delivered_body,
        turn,
        lease_key,
        start,
        end,
    )
    .await;
    apply_bridge_short_replace_outcome(
        outcome,
        shared,
        provider,
        channel_id,
        msg_id,
        tmux_session_name,
        relay_text,
        full_response_len,
        single_message_panel_footer_mode,
        dispatch_id,
        session_key,
        turn_id,
        locals,
    );
}

/// Maps site-5 outcomes onto footer, retry, sent-offset, and holder cleanup state.
/// Reproduces the legacy site-5 mapping (mod.rs 6160-6245) EXACTLY:
/// - `Delivered` (EditedOriginal): committed → `terminal_delivery_committed = true;
///   terminal_body_visible = true;` footer if footer-mode; `Succeeded` cleanup;
///   the outer epilogue (mod.rs:6293) bumps `response_sent_offset` — the
///   controller already advanced `confirmed_end`. We also bump
///   `inflight_response_sent_offset` here so the in-struct mirror matches legacy
///   (mod.rs:6295 sets `inflight_state.response_sent_offset` on the committed path).
/// - `Unknown { fell_back: true }` (SentFallbackAfterEditFailure):
///   `preserve_inflight_for_cleanup_retry = true;` AND the dual-offset bump
///   `inflight_response_sent_offset = full_response_len` (mod.rs:6241); record
///   `failed(..)` cleanup; NO `confirmed_end` advance (released Unknown without
///   commit); NO completion_footer.
/// - `Unknown { fell_back: false }` (Partial / Err):
///   `preserve_inflight_for_cleanup_retry = true;` record `failed(detail)` cleanup;
///   NO `response_sent_offset` bump (distinguishes from the fell_back arm).
/// - `Transient` (lost acquire / B2-skip): `preserve_inflight_for_cleanup_retry =
///   true; bridge_skip_holder_owns_inflight = true;` no transport, no cleanup
///   record (the legacy B2-skip arm at mod.rs:6145 records none).
/// - `NotDelivered` (advance refused — not normally reachable: site-5's advance is
///   unconditional on a committed replace): conservative
///   `preserve_inflight_for_cleanup_retry = true` (no commit), record failed
///   cleanup. Documented as the defensive default.
/// - `Skipped` (empty body — excluded by the cutover gate; unreachable in prod):
///   `preserve_inflight_for_cleanup_retry = true`.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_bridge_short_replace_outcome(
    outcome: toc::DeliveryOutcome,
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    msg_id: MessageId,
    tmux_session_name: Option<&str>,
    relay_text: &str,
    full_response_len: usize,
    single_message_panel_footer_mode: bool,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    locals: BridgeShortReplaceLocals<'_>,
) {
    use super::super::placeholder_cleanup::PlaceholderCleanupOutcome;
    match outcome {
        // Legacy EditedOriginal committed arm (mod.rs:6226-6232): the original was
        // edited in place → mark committed/visible, register the footer target in
        // footer-mode, record the `Succeeded` cleanup + emit committed=true. The
        // controller already advanced `confirmed_end`.
        toc::DeliveryOutcome::Delivered { .. } => {
            *locals.terminal_delivery_committed = true;
            *locals.terminal_body_visible = true;
            if single_message_panel_footer_mode {
                *locals.completion_footer_terminal_text = Some(relay_text.to_string());
            }
            // mod.rs:6293-6295: the committed epilogue sets response_sent_offset to
            // the full response length and mirrors it onto the inflight row.
            *locals.inflight_response_sent_offset = full_response_len;
            emit_bridge_replace_cleanup(
                shared,
                provider,
                channel_id,
                msg_id,
                tmux_session_name,
                PlaceholderCleanupOutcome::Succeeded,
                true,
                dispatch_id,
                session_key,
                turn_id,
            );
        }
        // SentFallbackAfterEditFailure (mod.rs:6233-6242, the `fallback_delivered`
        // block): the in-place edit FAILED but the fallback POST carried the body.
        // Preserve for cleanup retry + the DUAL-OFFSET bump (response_sent_offset =
        // full_response.len()) so the preserved turn never re-presents as a
        // never-delivered leak; record `failed(..)`; NO confirmed_end advance (the
        // controller released Unknown without commit); NO footer.
        toc::DeliveryOutcome::Unknown { fell_back: true } => {
            *locals.preserve_inflight_for_cleanup_retry = true;
            *locals.inflight_response_sent_offset = full_response_len;
            emit_bridge_replace_cleanup(
                shared,
                provider,
                channel_id,
                msg_id,
                tmux_session_name,
                PlaceholderCleanupOutcome::failed("fallback after edit failure".to_string()),
                false,
                dispatch_id,
                session_key,
                turn_id,
            );
        }
        // Partial continuation / transport error (mod.rs:6234, NO fallback bump):
        // preserve for cleanup retry; record failed; NO response_sent_offset bump.
        toc::DeliveryOutcome::Unknown { fell_back: false } => {
            *locals.preserve_inflight_for_cleanup_retry = true;
            emit_bridge_replace_cleanup(
                shared,
                provider,
                channel_id,
                msg_id,
                tmux_session_name,
                PlaceholderCleanupOutcome::failed("terminal replace not committed".to_string()),
                false,
                dispatch_id,
                session_key,
                turn_id,
            );
        }
        // Lost acquire → the legacy B2-skip arm (mod.rs:6145-6159): another holder
        // (the watcher) owns this range. No transport, no offset advance — the
        // holder commits it. The legacy B2-skip records NO cleanup, so neither do
        // we. Preserve + identity-guard the epilogue save (codex P1-2 R3).
        toc::DeliveryOutcome::Transient { .. } => {
            *locals.preserve_inflight_for_cleanup_retry = true;
            *locals.bridge_skip_holder_owns_inflight = true;
        }
        // Advance refused (not normally reachable: site-5's advance is
        // unconditional on a committed replace). Conservative: treat as not
        // committed → preserve for retry, record failed. Documented defensive arm.
        toc::DeliveryOutcome::NotDelivered { .. } => {
            *locals.preserve_inflight_for_cleanup_retry = true;
            emit_bridge_replace_cleanup(
                shared,
                provider,
                channel_id,
                msg_id,
                tmux_session_name,
                PlaceholderCleanupOutcome::failed("terminal replace advance refused".to_string()),
                false,
                dispatch_id,
                session_key,
                turn_id,
            );
        }
        // SendFresh and empty body are excluded by this replace cutover gate.
        toc::DeliveryOutcome::FreshDelivered { .. } | toc::DeliveryOutcome::Skipped => {
            *locals.preserve_inflight_for_cleanup_retry = true;
        }
    }
}

/// #3089 A5: record the bridge-side terminal-replace cleanup + emit the
/// `relay_delivery` observability event, reproducing what the legacy
/// `turn_bridge_replace_outcome_committed` recorded per arm (terminal_delivery.rs:118-211)
/// so the controller path's bridge observability stays byte-identical.
#[allow(clippy::too_many_arguments)]
fn emit_bridge_replace_cleanup(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<&str>,
    outcome: super::super::placeholder_cleanup::PlaceholderCleanupOutcome,
    committed: bool,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
) {
    super::terminal_delivery::record_turn_bridge_terminal_replace_cleanup(
        shared,
        provider,
        channel_id,
        message_id,
        tmux_session_name,
        outcome,
        "turn_bridge_terminal_replace_controller",
    );
    crate::services::observability::emit_relay_delivery(
        provider.as_str(),
        channel_id.get(),
        dispatch_id,
        session_key,
        turn_id,
        Some(message_id.get()),
        "turn_bridge",
        "edit",
        None,
        None,
        committed,
        Some("turn_bridge_terminal_replace_controller"),
    );
}

#[cfg(test)]
mod tests {
    // #3089 A5: the bridge short-replace cutover. These drive the REAL controller
    // (`deliver_short_replace_via_controller`) + the pure write-back
    // (`apply_bridge_short_replace_outcome`) + the pure gate/predicate helpers
    // against a real per-channel `DeliveryLeaseCell`, proving: NoCommitOnFallback →
    // Unknown{fell_back} → no advance + the dual-offset bump; EditedOriginal →
    // advance + committed + footer; Transient (lost acquire) → no transport;
    // PartialContinuation → no advance + NO bump; heartbeat-before-commit; the pure
    // lease-range/predicate gates; and OFF byte-identical. Mirrors A4's set.
    mod bridge_short_replace_controller {
        use super::super::{
            BridgeLongChunksLocals, BridgeShortReplaceLocals, apply_bridge_long_chunks_controller,
            apply_bridge_short_replace_outcome, bridge_long_chunks_cutover_decision,
            bridge_short_replace_cutover, bridge_short_replace_cutover_decision,
            bridge_terminal_lease_range, deliver_short_replace_via_controller,
        };
        use crate::services::discord::formatting::ReplaceLongMessageOutcome;
        use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
        use crate::services::discord::outbound::turn_output_controller as toc;
        use crate::services::discord::turn_finalizer::TurnKey;
        use crate::services::discord::{
            DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseSnapshot, SharedData,
            lease_now_ms, make_shared_data_for_tests,
        };
        use crate::services::provider::ProviderKind;
        use serenity::all::{ChannelId, MessageId};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

        const CH: u64 = 8_151;
        const MSG: u64 = 77;
        const START: u64 = 12;
        const END: u64 = 48;
        // codex r1 [High] regression: a recovered/reused-watcher bridge resolves a
        // DIFFERENT owner channel for the lease/advance than its delivery channel
        // (mod.rs:2207-2213). The edit MUST land in the delivery channel.
        const OWNER_CH: u64 = 9_999;

        #[cfg(unix)]
        #[rustfmt::skip]
        use super::super::super::{terminal_delivery::{BridgeDeliveryLease, BridgeLeaseAcquire, PinnedBridgeDeliveryLease}, terminal_outcome_delivery::PinnedTerminalTransport};
        #[cfg(unix)]
        #[rustfmt::skip]
        use crate::services::discord::{inflight::{CodexRange, InflightTurnIdentity}, outbound::delivery_record::{self as dr, ExactJsonlSourceIdentity}};
        fn ch() -> ChannelId {
            ChannelId::new(CH)
        }
        fn owner_ch() -> ChannelId {
            ChannelId::new(OWNER_CH)
        }
        fn turn() -> TurnKey {
            TurnKey::new(ch(), 21, 0)
        }
        fn lease_key() -> DeliveryLeaseKey {
            DeliveryLeaseKey::from_turn_key(turn())
        }

        #[cfg(unix)]
        #[rustfmt::skip]
        fn stamp(tmux: &str, seconds: i64) -> i64 {
            let path = crate::services::tmux_common::session_temp_path(tmux, "generation");
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap(); std::fs::write(&path, "g").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(seconds, 1)).unwrap(); dr::current_generation_mtime_ns(tmux)
        }
        #[cfg(unix)]
        #[rustfmt::skip]
        fn pinned_parts(shared: &Arc<SharedData>, root: &std::path::Path, owner: ChannelId, user: u64) -> (BridgeLeaseAcquire, CodexRange, crate::services::discord::inflight::InflightTurnState, ExactJsonlSourceIdentity) {
            use crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset as write_marker;
            let (tmux, rollout) = ("AgentDesk-codex-5264-barrier", root.join(format!("rollout-{user}.jsonl")));
            std::fs::write(&rollout, [b'x'; 64]).unwrap(); write_marker(tmux, &rollout, Some("raw-session"), Some(0)).unwrap();
            // #5264 PR-B: production never pins a source without a live CodexTui runtime
            // binding — `CodexRange::revalidated_source` requires `binding_matches` before
            // `prepare_bridge_lease` builds the lease. Install it so the fixture holds that
            // precondition; without it `source_authority_is_live` is false for a reason no
            // production caller can reach, and every sub-case below passes vacuously.
            crate::services::codex_tui::session::install_codex_tui_runtime_binding(tmux, Some(0), crate::services::tui_prompt_dedupe::TuiRuntimeBinding { runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui, output_path: rollout.display().to_string(), relay_output_path: None, input_fifo_path: None, session_id: Some("raw-session".into()), last_offset: 64, relay_last_offset: None });
            let source = ExactJsonlSourceIdentity { provider: "codex".into(), tmux_session_name: tmux.into(), turn_nonce: format!("nonce-{user}"), range: (0,64), generation_mtime_ns: stamp(tmux, 1_700_526_400), offset_authority_channel_id: owner.get(), delivery_channel_id: CH };
            let range = CodexRange { identity: InflightTurnIdentity { user_msg_id: user, started_at: "now".into(), tmux_session_name: Some(tmux.into()), turn_start_offset: Some(0) }, result: "answer".into(), rollout_path: rollout.display().to_string(), session_id: "raw-session".into(), source: source.clone() };
            // Canonicalized: `revalidated_source` compares the row's output path against
            // the canonicalized live source path, and on macOS the tempdir resolves
            // /var -> /private/var. A raw tempdir path makes revalidation return Ok(None)
            // for a reason production never sees.
            let canonical_rollout = std::fs::canonicalize(&rollout).unwrap_or_else(|_| rollout.clone());
            let mut inflight = crate::services::discord::inflight::InflightTurnState::new(ProviderKind::Codex, CH, None, 1, user, MSG, "prompt".into(), None, Some(tmux.into()), Some(canonical_rollout.display().to_string()), None, 0);
            inflight.started_at = "now".into(); inflight.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui); inflight.turn_nonce = Some(format!("nonce-{user}")); inflight.last_offset = 64; inflight.full_response = "answer".into(); inflight.watcher_owner_channel_id = Some(owner.get());
            crate::services::discord::inflight::save_inflight_state(&inflight).unwrap();
            let key = DeliveryLeaseKey::from_turn_key(TurnKey::new(owner, user, 1));
            let acquire = BridgeDeliveryLease::acquire(shared, owner, key, 0, Some(64));
            (acquire, range, inflight, source)
        }
        // The pinned lease the barrier tests use, built from the parts above so the
        // decision-table test can observe the same fixture one step earlier.
        #[cfg(unix)]
        #[rustfmt::skip]
        fn pinned(shared: &Arc<SharedData>, root: &std::path::Path, owner: ChannelId, user: u64) -> (PinnedBridgeDeliveryLease, ExactJsonlSourceIdentity) {
            let (acquire, range, _inflight, source) = pinned_parts(shared, root, owner, user);
            let lease = match acquire { BridgeLeaseAcquire::Held(lease) => lease, _ => panic!("held") };
            (lease.pin_exact_source(shared, &ProviderKind::Codex, ch(), range).unwrap(), source)
        }

        // #5264 PR-B: `prepare_bridge_lease` is the ONLY production route into the pinned
        // cutover, and nothing drove it. A build that short-circuits it to always return
        // `Legacy` makes the entire feature inert — no CodexTui turn ever pins — and every
        // barrier test still passes, because they construct `PinnedBridgeDeliveryLease`
        // themselves. These assertions are the witness that the production decision still
        // reaches the pinned arm at all.
        #[cfg(unix)]
        #[tokio::test(flavor = "current_thread")]
        async fn prepare_bridge_lease_decision_table_5264() {
            use crate::services::discord::turn_bridge::stream_loop::types::{
                PreparedBridgeLease, prepare_bridge_lease,
            };
            let _lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            crate::services::tui_prompt_dedupe::reset_state_for_tests();
            let root = runtime_root_guard();

            let shared = make_shared_data_for_tests();
            let (acquire, range, inflight, _source) =
                pinned_parts(&shared, root._temp.path(), ch(), 11);
            // Stated separately so a fixture drift shows up as "revalidation rejected the
            // source" rather than as an opaque "did not pin".
            let revalidated = range.revalidated_source(&inflight);
            assert!(
                revalidated.is_ok(),
                "the fixture must satisfy the revalidation preconditions"
            );
            assert!(
                revalidated.expect("checked").is_some(),
                "revalidation must accept the fixture's source"
            );
            assert!(
                matches!(
                    prepare_bridge_lease(
                        acquire,
                        Some(&range),
                        &inflight,
                        shared.as_ref(),
                        &ProviderKind::Codex,
                        ch()
                    ),
                    PreparedBridgeLease::Pinned(_)
                ),
                "an admitted frame whose source revalidates, with a held lease, must pin"
            );

            // A Skip is the one acquire outcome that means another actor really owns this
            // turn, so it must stay a Skip rather than being pinned over.
            let skip_shared = make_shared_data_for_tests();
            let (_skip_acquire, skip_range, skip_inflight, _s) =
                pinned_parts(&skip_shared, root._temp.path(), ch(), 12);
            assert!(
                matches!(
                    prepare_bridge_lease(
                        BridgeLeaseAcquire::Skip,
                        Some(&skip_range),
                        &skip_inflight,
                        skip_shared.as_ref(),
                        &ProviderKind::Codex,
                        ch()
                    ),
                    PreparedBridgeLease::Legacy(BridgeLeaseAcquire::Skip)
                ),
                "a real Skip must survive the pinned decision untouched"
            );

            // With no admitted frame there is nothing to pin, but the held lease must not
            // be discarded or replaced by a fabricated outcome.
            let legacy_shared = make_shared_data_for_tests();
            let (legacy_acquire, _range, legacy_inflight, _s) =
                pinned_parts(&legacy_shared, root._temp.path(), ch(), 13);
            assert!(
                matches!(
                    prepare_bridge_lease(
                        legacy_acquire,
                        None,
                        &legacy_inflight,
                        legacy_shared.as_ref(),
                        &ProviderKind::Codex,
                        ch()
                    ),
                    PreparedBridgeLease::Legacy(BridgeLeaseAcquire::Held(_))
                ),
                "no admitted frame must pass the real acquire through, not fabricate one"
            );

            // A revalidation error is not evidence that anyone else holds this turn. The
            // caller turns a Skip into `bridge_skip_holder_owns_inflight`, so fabricating
            // one here claims a holder that does not exist while this bridge still owns the
            // lease it acquired.
            let err_shared = make_shared_data_for_tests();
            let (err_acquire, err_range, err_inflight, _s) =
                pinned_parts(&err_shared, root._temp.path(), ch(), 14);
            let mut committed_inflight = err_inflight;
            committed_inflight.terminal_delivery_committed = true;
            assert!(
                err_range.revalidated_source(&committed_inflight).is_err(),
                "the fixture must actually reach the revalidation error arm"
            );
            assert!(
                matches!(
                    prepare_bridge_lease(
                        err_acquire,
                        Some(&err_range),
                        &committed_inflight,
                        err_shared.as_ref(),
                        &ProviderKind::Codex,
                        ch()
                    ),
                    PreparedBridgeLease::Legacy(BridgeLeaseAcquire::Held(_))
                ),
                "a revalidation error must pass the real acquire through, never fabricate a \
                 holder-owned Skip"
            );

            // Same class, one arm lower: `pin_exact_source` consumes the lease and can
            // still refuse, leaving nobody holding the cell. Reporting Skip there would
            // claim a holder AND mark the turn handled, so the legacy fallback would never
            // run and the turn would go undelivered. Reached here by pinning against a
            // delivery channel the source was not minted for.
            let pin_shared = make_shared_data_for_tests();
            let (pin_acquire, pin_range, pin_inflight, pin_source) =
                pinned_parts(&pin_shared, root._temp.path(), ch(), 15);
            assert_ne!(
                pin_source.delivery_channel_id,
                owner_ch().get(),
                "the fixture must actually mismatch the pin's delivery channel"
            );
            assert!(
                matches!(
                    prepare_bridge_lease(
                        pin_acquire,
                        Some(&pin_range),
                        &pin_inflight,
                        pin_shared.as_ref(),
                        &ProviderKind::Codex,
                        owner_ch()
                    ),
                    PreparedBridgeLease::Legacy(BridgeLeaseAcquire::NoRange)
                ),
                "a refused pin must report NoRange so the fallback still delivers, never a \
                 holder-owned Skip that suppresses delivery entirely"
            );

            // The third arm. A source that no longer matches is not a holder either, and
            // this arm had no case at all: mutating it to Skip passed the whole table.
            let stale_shared = make_shared_data_for_tests();
            let (stale_acquire, stale_range, stale_inflight, _s) =
                pinned_parts(&stale_shared, root._temp.path(), ch(), 16);
            let mut drifted = stale_inflight;
            // Persisted, not just local: `revalidated_source` compares the row it re-reads
            // under the inflight lock, so mutating only the in-memory copy leaves the arm
            // unreached.
            drifted.full_response = "a different answer".into();
            crate::services::discord::inflight::save_inflight_state(&drifted).unwrap();
            assert!(
                matches!(stale_range.revalidated_source(&drifted), Ok(None)),
                "the fixture must actually reach the source-no-longer-exact arm"
            );
            assert!(
                !matches!(
                    prepare_bridge_lease(
                        stale_acquire,
                        Some(&stale_range),
                        &drifted,
                        stale_shared.as_ref(),
                        &ProviderKind::Codex,
                        ch()
                    ),
                    PreparedBridgeLease::Legacy(BridgeLeaseAcquire::Skip)
                ),
                "a source that no longer revalidates must not be reported as a holder-owned \
                 Skip, which would suppress the fallback and drop the turn"
            );
        }
        #[cfg(unix)]
        #[rustfmt::skip]
        fn flip(source: ExactJsonlSourceIdentity) -> Arc<dyn Fn() + Send + Sync> {
            Arc::new(move || { let mut g2 = source.clone(); g2.generation_mtime_ns = stamp(&g2.tmux_session_name, 1_800_526_400); g2.turn_nonce = "g2".into(); g2.range = (0, 7); dr::record_current_pinned_delivery(&g2, 9_999).unwrap(); })
        }
        #[cfg(unix)]
        #[rustfmt::skip]
        fn pinned_transport<'a>(shared: &'a SharedData, gateway: &'a dyn TurnGateway, provider: &'a ProviderKind, owner: ChannelId) -> PinnedTerminalTransport<'a> {
            PinnedTerminalTransport { source: (shared, gateway, provider), target: (owner, ch(), MessageId::new(MSG)), payload: (Some("AgentDesk-codex-5264-barrier"), "answer", (0, 64)), trace: (None, None, None) }
        }
        #[cfg(unix)]
        #[rustfmt::skip]
        fn assert_pinned_barrier(source: &ExactJsonlSourceIdentity, message_id: u64, user_msg_id: u64, body: &str) {
            let record = dr::read_record(&ProviderKind::Codex, source.offset_authority_channel_id).unwrap(); let frontier = record.delivered_frontier.unwrap();
            assert_eq!((frontier.range, frontier.generation_mtime_ns), ((0, 7), dr::current_generation_mtime_ns(&source.tmux_session_name)));
            assert!(dr::historical_pinned_delivery_exists(source, message_id));
            let mut wrong = source.clone(); wrong.generation_mtime_ns = frontier.generation_mtime_ns; wrong.turn_nonce = "g2".into();
            assert!(!dr::historical_pinned_delivery_exists(&wrong, message_id));
            // #5264 PR-B r7: seal `record_pinned_delivery_metadata`, the only new durable
            // write this branch adds to the pinned path. Until these three assertions
            // existed, DELETING the call outright and REVERTING the fingerprint key to
            // `delivery_channel_id` both left `delivery_record`, `terminal_delivery`,
            // `terminal_controller_cutover`, `5264` and `tmux_watcher` fully green — so the
            // migration doc's (now removed) "known gap" could have been "closed" by
            // reintroducing the real inert-fingerprint bug against a green CI.
            // The oracle is the fingerprint's KEY SPACE, not `recent_delivered_content_matches`:
            // both of these tests deliberately flip the generation during delivery, so the
            // reader-side helper (which compares against the LIVE generation) can never match
            // here by construction. That mismatch is the disclosed fail-open of a dedupe
            // heuristic; what this seal pins is which channel the entry lands under.
            let fingerprinted = |channel: u64| dr::read_record(&ProviderKind::Codex, channel).is_some_and(|record| {
                record.recent_delivered_contents.iter().any(|entry| entry.channel_id == channel
                    && entry.generation_mtime_ns == source.generation_mtime_ns
                    && entry.content_len == body.len() as u64)
            });
            assert!(fingerprinted(source.offset_authority_channel_id),
                "the delivered-content fingerprint must land under the OFFSET-AUTHORITY channel, which is where every reader (tmux_watcher/turn_identity.rs, tmux_watcher/terminal_preflight.rs) looks it up");
            // Both barrier tests sweep `owner in [ch(), owner_ch()]`, so the split-channel
            // half of the seal is asserted only on the iteration where they actually differ.
            // That iteration is what kills the key reversion; the assertion above is what
            // kills deleting the write.
            if source.offset_authority_channel_id != source.delivery_channel_id {
                assert!(!fingerprinted(source.delivery_channel_id),
                    "and NOT under the delivery channel: writing it there strands the duplicate defence in exactly the split-channel case it exists for");
            }
            let settled = crate::services::discord::outbound::completed_turn_ledger::settled_user_msg_ids(&ProviderKind::Codex, source.delivery_channel_id);
            assert!(settled.contains(&user_msg_id),
                "the completed-turn ledger entry must be recorded under the DELIVERY channel {}: expected {user_msg_id}, ledger holds {settled:?}", source.delivery_channel_id);
        }
        // A fake `TurnGateway` whose `replace_message_with_outcome` returns a fixed
        // outcome (or `Err`), counts transport calls, AND records the `channel_id`
        // it was called with (0 = never called) so a test can assert the edit was
        // routed to the DELIVERY channel — not the lease/advance owner channel
        // (codex r1 [High]). All other methods panic — the short-replace path must
        // touch ONLY `replace_message_with_outcome` (the `Active` lifecycle keeps
        // `post_send_finalize` a no-op, no edit).
        struct ShortReplaceFakeGateway {
            outcome: ReplaceLongMessageOutcome,
            ok: bool,
            hook: Option<Arc<dyn Fn() + Send + Sync>>,
            replace_calls: AtomicUsize,
            replace_channel: AtomicU64,
        }

        impl TurnGateway for ShortReplaceFakeGateway {
            fn replace_message_with_outcome<'a>(
                &'a self,
                c: ChannelId,
                _m: MessageId,
                _content: &'a str,
            ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
                Box::pin(async move {
                    self.replace_calls.fetch_add(1, Ordering::SeqCst);
                    self.replace_channel.store(c.get(), Ordering::SeqCst);
                    self.hook.iter().for_each(|hook| hook());
                    if self.ok {
                        Ok(self.outcome.clone())
                    } else {
                        Err("fake transport failure".to_string())
                    }
                })
            }
            fn send_message<'a>(
                &'a self,
                _c: ChannelId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<MessageId, String>> {
                panic!("short-replace never sends a new message")
            }
            fn send_long_message_with_rollback<'a>(
                &'a self,
                _c: ChannelId,
                _a: MessageId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
                panic!("short-replace never chunks")
            }
            fn edit_message<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("Active lifecycle → post_send_finalize no-op → no edit")
            }
            fn delete_message<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("short-replace never deletes (PreserveAlways)")
            }

            fn schedule_retry_with_history<'a>(
                &'a self,
                _c: ChannelId,
                _u: MessageId,
                _t: &'a str,
            ) -> GatewayFuture<'a, ()> {
                panic!("unused on the short-replace path")
            }
            fn dispatch_queued_turn<'a>(
                &'a self,
                _c: ChannelId,
                _i: &'a crate::services::discord::Intervention,
                _o: &'a str,
                _h: bool,
                _dispatch_lease: Option<
                    std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
                >,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("unused on the short-replace path")
            }
            fn validate_live_routing<'a>(
                &'a self,
                _c: ChannelId,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("unused on the short-replace path")
            }
            fn requester_mention(&self) -> Option<String> {
                None
            }
            fn can_chain_locally(&self) -> bool {
                true
            }
            fn bot_owner_provider(&self) -> Option<ProviderKind> {
                None
            }
        }

        fn gateway(outcome: ReplaceLongMessageOutcome, ok: bool) -> ShortReplaceFakeGateway {
            ShortReplaceFakeGateway {
                outcome,
                ok,
                hook: None,
                replace_calls: AtomicUsize::new(0),
                replace_channel: AtomicU64::new(0),
            }
        }

        // Drive the REAL controller through the production helper with a fresh cell,
        // returning the `DeliveryOutcome` the bridge write-back consumes. The existing
        // tests use the SAME channel for delivery and owner (no drift); the
        // routing regression below uses `run_split` with differing ids.
        async fn run(
            gw: &ShortReplaceFakeGateway,
            shared: &Arc<SharedData>,
            cell: &Arc<DeliveryLeaseCell>,
        ) -> toc::DeliveryOutcome {
            run_split(gw, shared, cell, ch(), ch()).await
        }

        // Drive the REAL controller with EXPLICIT delivery vs owner channels so a
        // test can assert the edit routes to the delivery channel while the
        // lease/advance use the owner channel (codex r1 [High]).
        async fn run_split(
            gw: &ShortReplaceFakeGateway,
            shared: &Arc<SharedData>,
            cell: &Arc<DeliveryLeaseCell>,
            delivery_channel: ChannelId,
            owner_channel: ChannelId,
        ) -> toc::DeliveryOutcome {
            deliver_short_replace_via_controller(
                gw,
                shared.as_ref(),
                &ProviderKind::Claude,
                delivery_channel,
                owner_channel,
                Some("AgentDesk-claude-8151"),
                cell,
                &shared.ui.placeholder_controller,
                MessageId::new(MSG),
                "answer body",
                "answer body",
                turn(),
                Some(lease_key()),
                START,
                END,
            )
            .await
        }

        // Default locals + a backing struct so the write-back can be asserted.
        #[derive(Default)]
        struct Locals {
            committed: bool,
            visible: bool,
            footer: Option<String>,
            preserve: bool,
            skip_holder: bool,
            response_offset: usize,
            inflight_offset: usize,
        }
        impl Locals {
            fn borrow(&mut self) -> BridgeShortReplaceLocals<'_> {
                BridgeShortReplaceLocals {
                    terminal_delivery_committed: &mut self.committed,
                    terminal_body_visible: &mut self.visible,
                    completion_footer_terminal_text: &mut self.footer,
                    preserve_inflight_for_cleanup_retry: &mut self.preserve,
                    bridge_skip_holder_owns_inflight: &mut self.skip_holder,
                    inflight_response_sent_offset: &mut self.inflight_offset,
                }
            }
        }

        fn apply(outcome: toc::DeliveryOutcome, footer_mode: bool, full_len: usize) -> Locals {
            let shared = make_shared_data_for_tests();
            let mut locals = Locals::default();
            apply_bridge_short_replace_outcome(
                outcome,
                shared.as_ref(),
                &ProviderKind::Claude,
                ch(),
                MessageId::new(MSG),
                Some("AgentDesk-claude-8151"),
                "answer body",
                full_len,
                footer_mode,
                None,
                None,
                None,
                locals.borrow(),
            );
            locals
        }

        struct RuntimeRootGuard {
            previous: Option<std::ffi::OsString>,
            _temp: tempfile::TempDir,
        }

        impl Drop for RuntimeRootGuard {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }

        fn runtime_root_guard() -> RuntimeRootGuard {
            let temp = match tempfile::tempdir() {
                Ok(temp) => temp,
                Err(error) => panic!("runtime root tempdir failed: {error}"),
            };
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
            RuntimeRootGuard {
                previous,
                _temp: temp,
            }
        }

        struct LongChunksFakeGateway {
            send_ok: bool,
            delete_ok: bool,
            hook: Option<Arc<dyn Fn() + Send + Sync>>,
            send_calls: AtomicUsize,
            delete_calls: AtomicUsize,
            clock: AtomicUsize,
            send_step: AtomicUsize,
            delete_step: AtomicUsize,
        }

        impl TurnGateway for LongChunksFakeGateway {
            #[rustfmt::skip]
            fn send_long_message_with_rollback<'a>(
                &'a self,
                _c: ChannelId,
                _a: MessageId,
                _content: &'a str,
            ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
                Box::pin(async move {
                    self.send_calls.fetch_add(1, Ordering::SeqCst);
                    self.hook.iter().for_each(|hook| hook());
                    self.send_step
                        .store(self.clock.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
                    if self.send_ok {
                        Ok((self.send_step.load(Ordering::SeqCst) != usize::MAX).then_some(vec![MessageId::new(9000), MessageId::new(9001)]).unwrap_or_default())
                    } else {
                        Err("chunk send failed after rollback".to_string())
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
                    if self.delete_ok {
                        Ok(())
                    } else {
                        Err("delete failed".to_string())
                    }
                })
            }
            fn send_message<'a>(
                &'a self,
                _c: ChannelId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<MessageId, String>> {
                panic!("long-chunk helper uses send_long_message_with_rollback")
            }
            fn replace_message_with_outcome<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
                _content: &'a str,
            ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
                panic!("long-chunk helper never replaces")
            }
            fn edit_message<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("long-chunk helper never edits")
            }

            fn schedule_retry_with_history<'a>(
                &'a self,
                _c: ChannelId,
                _u: MessageId,
                _t: &'a str,
            ) -> GatewayFuture<'a, ()> {
                panic!("unused on the long-chunk path")
            }
            fn dispatch_queued_turn<'a>(
                &'a self,
                _c: ChannelId,
                _i: &'a crate::services::discord::Intervention,
                _o: &'a str,
                _h: bool,
                _dispatch_lease: Option<
                    std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
                >,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("unused on the long-chunk path")
            }
            fn validate_live_routing<'a>(
                &'a self,
                _c: ChannelId,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("unused on the long-chunk path")
            }
            fn requester_mention(&self) -> Option<String> {
                None
            }
            fn can_chain_locally(&self) -> bool {
                true
            }
            fn bot_owner_provider(&self) -> Option<ProviderKind> {
                None
            }
        }

        fn long_gateway(send_ok: bool, delete_ok: bool) -> LongChunksFakeGateway {
            LongChunksFakeGateway {
                send_ok,
                delete_ok,
                hook: None,
                send_calls: AtomicUsize::new(0),
                delete_calls: AtomicUsize::new(0),
                clock: AtomicUsize::new(1),
                send_step: AtomicUsize::new(0),
                delete_step: AtomicUsize::new(0),
            }
        }

        #[cfg(unix)]
        #[tokio::test(flavor = "current_thread")]
        #[rustfmt::skip]
        async fn pinned_codex_short_replace_generation_flip_5264() {
            let _lock = crate::config::shared_test_env_lock().lock().unwrap_or_else(|e| e.into_inner()); let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK.lock().unwrap_or_else(|poison| poison.into_inner()); crate::services::tui_prompt_dedupe::reset_state_for_tests(); let root = runtime_root_guard(); let provider = ProviderKind::Codex;
            // A per-iteration runtime root: the same-channel sweep (owner == delivery) writes
            // its fingerprint under CH, and the split-channel sweep asserts CH holds NO
            // fingerprint. Both sweeps use one tmux session, one generation stamp and one
            // body, so a shared root would leave the first sweep's entry indistinguishable
            // from a mis-keyed write and the negative assertion would be unfalsifiable.
            for owner in [ch(), owner_ch()] { let root = runtime_root_guard(); let shared = make_shared_data_for_tests(); let (pin, source) = pinned(&shared, root._temp.path(), owner, owner.get()); let mut gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, true); gw.hook = Some(flip(source.clone()));
                let (committed, fallback, _) = pinned_transport(shared.as_ref(), &gw, &provider, owner).deliver(pin, false).await;
                assert!(committed && !fallback); assert_eq!(gw.replace_calls.load(Ordering::SeqCst), 1); assert_pinned_barrier(&source, MSG, owner.get(), "answer"); assert!(matches!(shared.delivery_lease(owner).read(), LeaseSnapshot::Unleased)); }
            let shared = make_shared_data_for_tests(); let (pin, source) = pinned(&shared, root._temp.path(), ch(), 3); let gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, false);
            assert!(!pinned_transport(shared.as_ref(), &gw, &provider, ch()).deliver(pin, false).await.0); assert!(!dr::historical_pinned_delivery_exists(&source, MSG)); assert!(matches!(shared.delivery_lease(ch()).read(), LeaseSnapshot::Unleased)); let broken = runtime_root_guard(); std::fs::create_dir_all(broken._temp.path().join("runtime")).unwrap(); std::fs::write(broken._temp.path().join("runtime/discord_delivery_records"), b"blocked").unwrap(); let shared = make_shared_data_for_tests(); let (pin, source) = pinned(&shared, broken._temp.path(), ch(), 6); let gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, true); assert!(!pinned_transport(shared.as_ref(), &gw, &provider, ch()).deliver(pin, false).await.0); assert_eq!((gw.replace_calls.load(Ordering::SeqCst), shared.committed_relay_offset(ch())), (1, 64)); assert!(dr::read_record(&provider, CH).is_none()); assert!(!dr::historical_pinned_delivery_exists(&source, MSG)); assert!(matches!(shared.delivery_lease(ch()).read(), LeaseSnapshot::Unleased));
        }

        #[cfg(unix)]
        #[tokio::test(flavor = "current_thread")]
        #[rustfmt::skip]
        async fn pinned_codex_long_chunks_generation_flip_5264() {
            let _lock = crate::config::shared_test_env_lock().lock().unwrap_or_else(|e| e.into_inner()); let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK.lock().unwrap_or_else(|poison| poison.into_inner()); crate::services::tui_prompt_dedupe::reset_state_for_tests(); let root = runtime_root_guard(); let provider = ProviderKind::Codex;
            // Per-iteration runtime root — same reason as the short-replace sweep above.
            for owner in [ch(), owner_ch()] { let root = runtime_root_guard(); let shared = make_shared_data_for_tests(); let (pin, source) = pinned(&shared, root._temp.path(), owner, owner.get() + 4); let mut gw = long_gateway(true, true); gw.hook = Some(flip(source.clone())); let outcome = pinned_transport(shared.as_ref(), &gw, &provider, owner).deliver(pin, true).await;
                assert_eq!((outcome.0, outcome.1), (true, false)); let mut inflight = crate::services::discord::inflight::load_inflight_state_read_only(&provider, CH).unwrap(); assert!(super::super::super::stream_loop::types::persist_pinned_terminal_anchor(&mut inflight, outcome.2)); let restored = crate::services::discord::inflight::load_inflight_state_read_only(&provider, CH).unwrap(); assert!(dr::confirmed_delivery_receipt_exists(&provider, ch(), restored.current_msg_id, &source)); assert_eq!(gw.send_calls.load(Ordering::SeqCst), 1); assert_pinned_barrier(&source, 9001, owner.get() + 4, "answer"); assert!(!dr::historical_pinned_delivery_exists(&source, 9000)); assert!(!dr::historical_pinned_delivery_exists(&source, MSG)); assert!(matches!(shared.delivery_lease(owner).read(), LeaseSnapshot::Unleased)); }
            let shared = make_shared_data_for_tests(); let (pin, source) = pinned(&shared, root._temp.path(), ch(), 5); let gw = long_gateway(false, true);
            assert!(!pinned_transport(shared.as_ref(), &gw, &provider, ch()).deliver(pin, true).await.0); assert!(!dr::historical_pinned_delivery_exists(&source, 9001)); assert!(matches!(shared.delivery_lease(ch()).read(), LeaseSnapshot::Unleased)); let shared = make_shared_data_for_tests(); let (pin, source) = pinned(&shared, root._temp.path(), ch(), 7); let gw = long_gateway(true, true); gw.clock.store(usize::MAX, Ordering::SeqCst); assert!(!pinned_transport(shared.as_ref(), &gw, &provider, ch()).deliver(pin, true).await.0); assert_eq!(gw.send_calls.load(Ordering::SeqCst), 1); assert!(!dr::historical_pinned_delivery_exists(&source, 9001)); assert!(matches!(shared.delivery_lease(ch()).read(), LeaseSnapshot::Unleased)); let broken = runtime_root_guard(); std::fs::create_dir_all(broken._temp.path().join("runtime")).unwrap(); std::fs::write(broken._temp.path().join("runtime/discord_delivery_records"), b"blocked").unwrap(); let shared = make_shared_data_for_tests(); let (pin, source) = pinned(&shared, broken._temp.path(), ch(), 8); let gw = long_gateway(true, true); assert!(!pinned_transport(shared.as_ref(), &gw, &provider, ch()).deliver(pin, true).await.0); assert_eq!((gw.send_calls.load(Ordering::SeqCst), shared.committed_relay_offset(ch())), (1, 64)); assert!(dr::read_record(&provider, CH).is_none()); assert!(!dr::historical_pinned_delivery_exists(&source, 9001)); assert!(matches!(shared.delivery_lease(ch()).read(), LeaseSnapshot::Unleased));
        }

        async fn run_long_apply(
            gw: &LongChunksFakeGateway,
            locals: &mut Locals,
        ) -> Arc<SharedData> {
            let shared = make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            apply_bridge_long_chunks_controller(
                gw,
                shared.as_ref(),
                &ProviderKind::Claude,
                ch(),
                ch(),
                Some("AgentDesk-claude-8151"),
                &cell,
                &shared.ui.placeholder_controller,
                MessageId::new(MSG),
                &"x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 10),
                &"x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 10),
                8192,
                turn(),
                START,
                END,
                true,
                None,
                None,
                None,
                Some(lease_key()),
                BridgeLongChunksLocals {
                    terminal_delivery_committed: &mut locals.committed,
                    terminal_body_visible: &mut locals.visible,
                    completion_footer_terminal_text: &mut locals.footer,
                    preserve_inflight_for_cleanup_retry: &mut locals.preserve,
                    bridge_skip_holder_owns_inflight: &mut locals.skip_holder,
                    response_sent_offset: &mut locals.response_offset,
                    inflight_response_sent_offset: &mut locals.inflight_offset,
                },
            )
            .await;
            shared
        }

        // (1) NoCommitOnFallback: SentFallbackAfterEditFailure → Unknown{fell_back}
        // → the write-back PRESERVES + bumps `inflight_response_sent_offset` to the
        // full response len (the dual-offset recovery) WITHOUT advancing
        // confirmed_end, and records NO completion_footer. Mutation: flipping the
        // controller policy to CommitOnFallback makes the controller advance
        // (`bridge_short_replace_edited_original_advances` covers the Delivered side).
        #[tokio::test(flavor = "current_thread")]
        async fn bridge_short_replace_no_commit_on_fallback_no_advance() {
            let shared = make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            assert_eq!(shared.committed_relay_offset(ch()), 0);
            let gw = gateway(
                ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                    edit_error: "edit 500; fallback POST succeeded".to_string(),
                    replacement_anchor: None,
                },
                true,
            );
            let outcome = run(&gw, &shared, &cell).await;
            assert!(
                matches!(outcome, toc::DeliveryOutcome::Unknown { fell_back: true }),
                "NoCommitOnFallback + SentFallback → Unknown{{fell_back:true}}"
            );
            assert_eq!(gw.replace_calls.load(Ordering::SeqCst), 1, "one POST");
            assert_eq!(
                shared.committed_relay_offset(ch()),
                0,
                "NoCommitOnFallback must NOT advance confirmed_end (I2)"
            );
            assert!(
                matches!(cell.read(), LeaseSnapshot::Unleased),
                "controller released the lease WITHOUT committing"
            );

            // Write-back: preserve + the dual-offset bump, no footer, not committed.
            let full_len = 4096usize;
            let locals = apply(outcome, true, full_len);
            assert!(locals.preserve, "fell_back preserves inflight for retry");
            assert_eq!(
                locals.inflight_offset, full_len,
                "dual-offset bump: response_sent_offset = full_response.len()"
            );
            assert!(!locals.committed, "fell_back is NOT a terminal commit");
            assert!(
                locals.footer.is_none(),
                "fell_back must NOT register the original as the footer target"
            );
        }

        // (2) EditedOriginal → controller advances confirmed_end to END + commits;
        // the write-back marks committed/visible, sets the footer in footer-mode, and
        // bumps the inflight offset. The advance to the real watermark is decisive.
        #[tokio::test(flavor = "current_thread")]
        async fn bridge_short_replace_edited_original_advances() {
            let shared = make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            let gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, true);
            let outcome = run(&gw, &shared, &cell).await;
            assert!(
                matches!(outcome, toc::DeliveryOutcome::Delivered { .. }),
                "EditedOriginal → Delivered"
            );
            assert_eq!(
                shared.committed_relay_offset(ch()),
                END,
                "confirmed transport advances the watermark to the leased end"
            );
            assert!(matches!(cell.read(), LeaseSnapshot::Unleased));

            let full_len = 2048usize;
            let locals = apply(outcome, true, full_len);
            assert!(locals.committed && locals.visible);
            assert_eq!(
                locals.footer.as_deref(),
                Some("answer body"),
                "footer-mode registers the edited original as the footer target"
            );
            assert_eq!(locals.inflight_offset, full_len);
        }

        // (3) lost acquire (a foreign holder pre-occupies the cell) → Transient → no
        // transport; the write-back sets preserve + the skip-holder flag. Mutation:
        // Transient→ProceedMarkerless would POST → replace_calls == 1.
        #[tokio::test(flavor = "current_thread")]
        async fn bridge_short_replace_acquire_transient_no_send() {
            let shared = make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            let other = LeaseHolder::Watcher { instance_id: 999 };
            assert!(cell.try_acquire(
                lease_key(),
                other,
                START,
                END,
                lease_now_ms().saturating_add(60_000),
            ));
            let gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, true);
            let outcome = run(&gw, &shared, &cell).await;
            assert!(
                matches!(outcome, toc::DeliveryOutcome::Transient { .. }),
                "a lost acquire is Transient (the legacy B2-skip equivalent)"
            );
            assert_eq!(
                gw.replace_calls.load(Ordering::SeqCst),
                0,
                "Transient acquire-fail MUST NOT POST (mutation to ProceedMarkerless POSTs)"
            );
            let locals = apply(outcome, false, 100);
            assert!(locals.preserve && locals.skip_holder, "B2-skip locals set");
            assert!(!locals.committed);
        }

        // (4) the advance identity gate: confirmed EditedOriginal advances exactly
        // ONCE to END via the real `advance_tmux_relay_confirmed_end`; a refused
        // advance (NotDelivered) does NOT advance. We exercise the refused side via
        // the write-back's conservative preserve mapping.
        #[tokio::test(flavor = "current_thread")]
        async fn bridge_short_replace_advance_identity_gate() {
            let shared = make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            let gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, true);
            run(&gw, &shared, &cell).await;
            assert_eq!(
                shared.committed_relay_offset(ch()),
                END,
                "advanced once to END"
            );

            // NotDelivered (defensive arm, not normally reachable): no advance,
            // conservative preserve.
            let locals = apply(
                toc::DeliveryOutcome::NotDelivered {
                    committed_from: START,
                },
                false,
                100,
            );
            assert!(locals.preserve && !locals.committed);
        }

        // (5) heartbeat-before-commit (#3151): the bridge heartbeat renews the lease
        // mid-POST and is stopped before the commit so the renew loop never races it.
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn bridge_short_replace_heartbeat_before_commit() {
            use crate::services::discord::{DELIVERY_LEASE_HEARTBEAT_MS, DeliveryLeaseHeartbeat};
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            let holder = LeaseHolder::Bridge;
            let short = lease_now_ms().saturating_add(100);
            assert!(cell.try_acquire(lease_key(), holder, START, END, short));
            let hb = DeliveryLeaseHeartbeat::spawn(cell.clone(), holder, lease_key());
            for _ in 0..3 {
                tokio::time::advance(std::time::Duration::from_millis(
                    DELIVERY_LEASE_HEARTBEAT_MS,
                ))
                .await;
                tokio::task::yield_now().await;
            }
            let renewed = match cell.read() {
                LeaseSnapshot::Leased { deadline_ms, .. } => deadline_ms,
                other => panic!("still Leased mid-POST, got {other:?}"),
            };
            assert!(renewed > short, "heartbeat renewed the deadline forward");
            hb.stop();
            tokio::task::yield_now().await;
            assert!(
                cell.commit(
                    holder,
                    lease_key(),
                    START,
                    END,
                    crate::services::discord::LeaseOutcome::Delivered
                ),
                "the holder's own commit succeeds after heartbeat-stop (#3151)"
            );
        }

        // (6) PartialContinuationFailure → Unknown{fell_back:false} → no advance AND
        // NO `response_sent_offset` bump (distinguishes from the fell_back=true arm in
        // test 1). Pins the dual-offset distinction + the controller `fell_back`
        // extension.
        #[tokio::test(flavor = "current_thread")]
        async fn bridge_short_replace_partial_failure_no_advance() {
            let shared = make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            let gw = gateway(
                ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks: 1,
                    total_chunks: 3,
                    failed_chunk_index: 1,
                    sent_continuation_message_ids: vec![1],
                    cleanup_errors: vec![],
                    error: "mid-stream".to_string(),
                },
                true,
            );
            let outcome = run(&gw, &shared, &cell).await;
            assert!(
                matches!(outcome, toc::DeliveryOutcome::Unknown { fell_back: false }),
                "PartialContinuationFailure → Unknown{{fell_back:false}}"
            );
            assert_eq!(
                shared.committed_relay_offset(ch()),
                0,
                "I2: a partial failure NEVER advances the offset"
            );
            let locals = apply(outcome, false, 4096);
            assert!(locals.preserve, "partial failure preserves for retry");
            assert_eq!(
                locals.inflight_offset, 0,
                "NO dual-offset bump on the partial arm (nothing landed)"
            );
            assert!(!locals.committed);
        }

        // (6b) codex r1 [High] channel-routing regression: when the delivery channel
        // differs from the lease/advance OWNER channel (a recovered/reused-watcher
        // bridge, mod.rs:2207-2213), the in-place EDIT must route to the DELIVERY
        // channel (legacy `replace_message_with_outcome(channel_id, ..)` mod.rs:6180),
        // NOT the owner channel — while the lease (cell keyed by the owner channel)
        // and the confirmed_end advance use the OWNER channel (legacy
        // `commit_and_advance(.., watcher_owner_channel_id, ..)` mod.rs:6216).
        //
        // Mutation pin: reverting the ctx to route the edit through
        // `watcher_owner_channel_id` (the r1 bug) records OWNER_CH on the gateway →
        // the DELIVERY-channel assertion fails. (Manually applied + reverted to
        // confirm the test catches it.)
        #[tokio::test(flavor = "current_thread")]
        async fn bridge_short_replace_routes_edit_to_delivery_channel() {
            let shared = make_shared_data_for_tests();
            // The lease cell is keyed by the OWNER channel (delivery_lease(owner)).
            let cell = Arc::new(DeliveryLeaseCell::new(owner_ch()));
            assert_ne!(CH, OWNER_CH, "delivery and owner channels must differ");
            assert_eq!(shared.committed_relay_offset(ch()), 0);
            assert_eq!(shared.committed_relay_offset(owner_ch()), 0);

            let gw = gateway(ReplaceLongMessageOutcome::EditedOriginal, true);
            let outcome = run_split(&gw, &shared, &cell, ch(), owner_ch()).await;
            assert!(
                matches!(outcome, toc::DeliveryOutcome::Delivered { .. }),
                "EditedOriginal → Delivered"
            );

            // The edit went to the DELIVERY channel — NOT the owner channel.
            assert_eq!(gw.replace_calls.load(Ordering::SeqCst), 1, "one edit POST");
            assert_eq!(
                gw.replace_channel.load(Ordering::SeqCst),
                CH,
                "the in-place edit MUST route to the DELIVERY channel (codex r1 [High])"
            );
            assert_ne!(
                gw.replace_channel.load(Ordering::SeqCst),
                OWNER_CH,
                "the edit MUST NOT route to the lease/advance owner channel"
            );

            // The advance committed on the OWNER channel (lease/advance authority),
            // NOT the delivery channel.
            assert_eq!(
                shared.committed_relay_offset(owner_ch()),
                END,
                "confirmed_end advances on the OWNER channel (lease/advance authority)"
            );
            assert_eq!(
                shared.committed_relay_offset(ch()),
                0,
                "the delivery channel is NOT the advance authority — no offset there"
            );
            assert!(
                matches!(cell.read(), LeaseSnapshot::Unleased),
                "the owner-keyed lease cell released after commit"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test(flavor = "current_thread")]
        async fn bridge_long_chunks_controller_delivered_deletes_anchor_and_advances() {
            let _env_lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let _root = runtime_root_guard();
            let gw = long_gateway(true, true);
            let mut locals = Locals::default();
            let shared = run_long_apply(&gw, &mut locals).await;
            assert_eq!(gw.send_calls.load(Ordering::SeqCst), 1);
            assert_eq!(gw.delete_calls.load(Ordering::SeqCst), 1);
            assert!(
                gw.send_step.load(Ordering::SeqCst) < gw.delete_step.load(Ordering::SeqCst),
                "placeholder delete must run after the full chunk send"
            );
            assert_eq!(shared.committed_relay_offset(ch()), END);
            assert!(locals.committed && locals.visible);
            assert_eq!(locals.response_offset, 8192);
            assert_eq!(locals.inflight_offset, 8192);
            assert_eq!(
                locals.footer.as_ref().map(String::len),
                Some(crate::services::discord::DISCORD_MSG_LIMIT + 10)
            );
            assert!(
                shared.ui.placeholder_cleanup.terminal_cleanup_committed(
                    &ProviderKind::Claude,
                    ch(),
                    MessageId::new(MSG)
                ),
                "delete cleanup success is recorded"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test(flavor = "current_thread")]
        async fn bridge_long_chunks_delete_failure_still_commits() {
            let _env_lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let _root = runtime_root_guard();
            let gw = long_gateway(true, false);
            let mut locals = Locals::default();
            let shared = run_long_apply(&gw, &mut locals).await;
            assert_eq!(shared.committed_relay_offset(ch()), END);
            assert!(locals.committed && locals.visible);
            assert_eq!(locals.inflight_offset, 8192);
            assert!(
                shared
                    .ui
                    .placeholder_cleanup
                    .terminal_cleanup_retry_pending(
                        &ProviderKind::Claude,
                        ch(),
                        MessageId::new(MSG)
                    ),
                "delete failure is recorded but does not un-deliver"
            );
        }

        #[tokio::test(flavor = "current_thread")]
        async fn bridge_long_chunks_send_failure_preserves_without_delete_or_advance() {
            let gw = long_gateway(false, true);
            let mut locals = Locals::default();
            let shared = run_long_apply(&gw, &mut locals).await;
            assert_eq!(gw.send_calls.load(Ordering::SeqCst), 1);
            assert_eq!(gw.delete_calls.load(Ordering::SeqCst), 0);
            assert_eq!(shared.committed_relay_offset(ch()), 0);
            assert!(locals.preserve);
            assert!(!locals.committed);
            assert_eq!(locals.inflight_offset, 0);
        }

        #[tokio::test(flavor = "current_thread")]
        async fn bridge_long_chunks_acquire_transient_no_send() {
            let shared = make_shared_data_for_tests();
            let cell = Arc::new(DeliveryLeaseCell::new(ch()));
            assert!(cell.try_acquire(
                lease_key(),
                LeaseHolder::Watcher { instance_id: 999 },
                START,
                END,
                lease_now_ms().saturating_add(60_000),
            ));
            let gw = long_gateway(true, true);
            let mut locals = Locals::default();
            apply_bridge_long_chunks_controller(
                &gw,
                shared.as_ref(),
                &ProviderKind::Claude,
                ch(),
                ch(),
                Some("AgentDesk-claude-8151"),
                &cell,
                &shared.ui.placeholder_controller,
                MessageId::new(MSG),
                &"x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 10),
                &"x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 10),
                8192,
                turn(),
                START,
                END,
                false,
                None,
                None,
                None,
                Some(lease_key()),
                BridgeLongChunksLocals {
                    terminal_delivery_committed: &mut locals.committed,
                    terminal_body_visible: &mut locals.visible,
                    completion_footer_terminal_text: &mut locals.footer,
                    preserve_inflight_for_cleanup_retry: &mut locals.preserve,
                    bridge_skip_holder_owns_inflight: &mut locals.skip_holder,
                    response_sent_offset: &mut locals.response_offset,
                    inflight_response_sent_offset: &mut locals.inflight_offset,
                },
            )
            .await;
            assert_eq!(gw.send_calls.load(Ordering::SeqCst), 0);
            assert!(locals.preserve && locals.skip_holder);
            assert_eq!(shared.committed_relay_offset(ch()), 0);
        }

        // (7) pure no-double-acquire gate + flag-ON skip: `bridge_terminal_lease_range`
        // returns None for any cut-over turn (the controller owns the lease).
        // Mutation: dropping `!cutover_short_replace` makes it return Some(..).
        #[test]
        fn bridge_terminal_lease_range_pins_cutover() {
            assert_eq!(
                bridge_terminal_lease_range(Some((START, END)), true),
                None,
                "a cut-over turn must NOT acquire the legacy bridge lease (no double-acquire)"
            );
            assert_eq!(
                bridge_terminal_lease_range(Some((START, END)), false),
                Some((START, END)),
                "a non-cut-over turn keeps the legacy lease range"
            );
            assert_eq!(
                bridge_terminal_lease_range(None, false),
                None,
                "an empty range never leases (NoRange)"
            );
        }

        // (8) the cut-over predicate truth table.
        #[test]
        fn bridge_short_replace_cutover_predicate() {
            // All-true → cut over.
            assert!(bridge_short_replace_cutover(true, true, true, true, true));
            // Each false term independently disables the cutover.
            assert!(!bridge_short_replace_cutover(false, true, true, true, true));
            assert!(!bridge_short_replace_cutover(true, false, true, true, true));
            assert!(!bridge_short_replace_cutover(true, true, false, true, true));
            assert!(!bridge_short_replace_cutover(true, true, true, false, true));
            assert!(!bridge_short_replace_cutover(true, true, true, true, false));

            // The decision helper: a long body (would chunk) is NOT short-replace.
            let long = "x".repeat(crate::services::discord::DISCORD_MSG_LIMIT + 10);
            assert!(
                !bridge_short_replace_cutover_decision(true, &long, true, true),
                "a body that exceeds the inline limit is not the short-replace arm"
            );
            assert!(
                bridge_long_chunks_cutover_decision(true, &long, true, true),
                "the long-chunk predicate routes the same body when A5 is enabled"
            );
            // A short body with a real range → cut over.
            assert!(bridge_short_replace_cutover_decision(
                true, "short", true, true
            ));
            // An empty body is NOT cut over (controller would Skip).
            assert!(!bridge_short_replace_cutover_decision(true, "", true, true));
            // No ordered range → not cut over.
            assert!(!bridge_short_replace_cutover_decision(
                true, "short", false, true
            ));
        }
    }
}
