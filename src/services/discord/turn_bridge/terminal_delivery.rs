use super::super::formatting::ReplaceLongMessageOutcome;
use super::*;

// #3089 A5: `pub(super)` so the `terminal_controller_cutover` sibling reproduces
// the legacy per-arm cleanup record (the controller's `post_send_finalize` no-ops
// on `Replace { Active }`, so the cutover write-back records it itself).
pub(super) fn record_turn_bridge_terminal_replace_cleanup(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<&str>,
    outcome: super::super::placeholder_cleanup::PlaceholderCleanupOutcome,
    source: &'static str,
) {
    if let super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Failed { class, detail } =
        &outcome
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ placeholder cleanup {} failed ({}) for channel {} msg {}: {}",
            super::super::placeholder_cleanup::PlaceholderCleanupOperation::EditTerminal.as_str(),
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
            operation: super::super::placeholder_cleanup::PlaceholderCleanupOperation::EditTerminal,
            outcome,
            source,
        },
    );
}

// #3034: pure terminal-delivery commit predicate pinned by the unit tests; the
// live path matches the outcome inline. Test contract.
#[allow(dead_code)]
fn replace_outcome_commits_terminal_delivery(outcome: &ReplaceLongMessageOutcome) -> bool {
    matches!(outcome, ReplaceLongMessageOutcome::EditedOriginal)
}

pub(super) fn terminal_delivery_should_send_new_chunks(
    can_chain_locally: bool,
    formatted_response: &str,
) -> bool {
    can_chain_locally && formatted_response.len() > super::super::DISCORD_MSG_LIMIT
}

pub(super) fn record_stopped_turn_terminal_replace_delivery(
    shared: &SharedData,
    provider: &ProviderKind,
    watcher_owner_channel_id: ChannelId,
    range: (u64, u64),
    terminal_anchor_msg_id: MessageId,
    terminal_anchor_channel_id: ChannelId,
    raw_response_body: &str,
) {
    super::super::outbound::delivery_record::record_delivered_frontier_with_body(
        shared,
        provider,
        watcher_owner_channel_id,
        range,
        terminal_anchor_msg_id.get(),
        terminal_anchor_channel_id.get(),
        raw_response_body,
    );
}

/// Returns `(first_chunk_msg_id, last_chunk_msg_id)` on a FULL commit. The send
/// is all-or-nothing: `send_ordered_long_terminal_chunks` propagates the
/// rollback-aware `send_long_message_with_rollback` `Err` (which deletes any
/// already-sent chunks) via `?`, so an `Ok` here means EVERY chunk committed.
///
/// #3610 PR-1c: `last_chunk_msg_id` is the durable TERMINAL ANCHOR for the
/// long-chunk arm — the tail chunk carrying the END of the terminal text. The
/// placeholder is DELETED by this path (no anchor there), so the last NEW chunk
/// is the only stable anchor. It is `Some` whenever Discord returned ≥1 chunk id
/// (always true on `Ok`, since the empty-Vec case errors in
/// `send_ordered_long_terminal_chunks`); callers treat `None` as "no anchor,
/// record range only" (safe, identical to the absent-status-panel case).
pub(super) async fn send_ordered_long_terminal_response(
    shared: &SharedData,
    gateway: &dyn TurnGateway,
    provider: &ProviderKind,
    channel_id: ChannelId,
    placeholder_msg_id: MessageId,
    tmux_session_name: Option<&str>,
    response: &str,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
) -> Result<(MessageId, Option<MessageId>), String> {
    let (first_msg_id, last_msg_id, delete_result) =
        send_ordered_long_terminal_chunks(gateway, channel_id, placeholder_msg_id, response)
            .await?;
    let cleanup_outcome = match delete_result {
        Ok(()) => super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
        Err(error) => super::super::placeholder_cleanup::classify_delete_error(&error),
    };
    shared.ui.placeholder_cleanup.record(
        super::super::placeholder_cleanup::PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id: placeholder_msg_id,
            tmux_session_name: tmux_session_name.map(str::to_string),
            operation:
                super::super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteTerminal,
            outcome: cleanup_outcome,
            source: "turn_bridge_terminal_long_send_cleanup",
        },
    );
    crate::services::observability::emit_relay_delivery(
        provider.as_str(),
        channel_id.get(),
        dispatch_id,
        session_key,
        turn_id,
        Some(first_msg_id.get()),
        "turn_bridge",
        "post",
        None,
        None,
        true,
        Some("terminal long response sent as ordered chunks"),
    );
    Ok((first_msg_id, last_msg_id))
}

/// Returns `(first_chunk_msg_id, last_chunk_msg_id, delete_result)`.
/// `send_long_message_with_rollback` is all-or-nothing — on ANY chunk failure it
/// rolls back (deletes) the already-sent chunks and returns `Err`, propagated
/// here by `?`. So a returned `Vec` is the COMPLETE, committed chunk set; its
/// `.last()` is the terminal text tail (#3610 PR-1c anchor). The empty-Vec case
/// errors (it cannot be a committed delivery), so on the `Ok` path `last` is
/// always `Some` — `Option` is kept for type-honesty at the call boundary.
async fn send_ordered_long_terminal_chunks(
    gateway: &dyn TurnGateway,
    channel_id: ChannelId,
    placeholder_msg_id: MessageId,
    response: &str,
) -> Result<(MessageId, Option<MessageId>, Result<(), String>), String> {
    let message_ids = gateway
        .send_long_message_with_rollback(channel_id, placeholder_msg_id, response)
        .await?;
    let first_msg_id = message_ids
        .first()
        .copied()
        .ok_or_else(|| "long terminal response produced no Discord chunks".to_string())?;
    let last_msg_id = message_ids.last().copied();
    let delete_result = gateway.delete_message(channel_id, placeholder_msg_id).await;
    Ok((first_msg_id, last_msg_id, delete_result))
}

pub(super) fn turn_bridge_replace_outcome_committed(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<&str>,
    replace_result: Result<ReplaceLongMessageOutcome, String>,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    source: &'static str,
) -> bool {
    let committed = match replace_result {
        Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
                source,
            );
            true
        }
        Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { edit_error, .. }) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(edit_error),
                source,
            );
            false
        }
        Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
            sent_chunks,
            total_chunks,
            failed_chunk_index,
            sent_continuation_message_ids,
            cleanup_errors,
            error,
        }) => {
            let display_error =
                super::super::replace_outcome_policy::strip_watcher_send_failure_class_marker(
                    &error,
                );
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(format!(
                    "partial continuation failure: sent_chunks={sent_chunks}, total_chunks={total_chunks}, failed_chunk_index={failed_chunk_index}, cleaned_continuations={}, cleanup_errors={}, error={display_error}",
                    sent_continuation_message_ids.len(),
                    cleanup_errors.len()
                )),
                source,
            );
            false
        }
        Err(error) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(error),
                source,
            );
            false
        }
    };
    // #2838 (relay-stability P0-1): emit a structured event for the bridge-side
    // terminal delivery decision. The watcher path already has the
    // `relay_flight_recorder` tracing, but bridge-owned replace deliveries were
    // unobserved; this makes them PG-queryable and attributable so the
    // duplicate/uncommitted vectors can be measured before the delivery-lease
    // consolidation lands.
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
        Some(source),
    );
    committed
}

pub(super) fn should_complete_work_dispatch_after_terminal_delivery(
    completion_candidate: bool,
    terminal_delivery_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
    resume_failure_detected: bool,
    recovery_retry: bool,
    full_response: &str,
) -> bool {
    completion_candidate
        && terminal_delivery_committed
        && !preserve_inflight_for_cleanup_retry
        && !resume_failure_detected
        && !recovery_retry
        && !full_response.trim().is_empty()
}

pub(super) fn should_fail_dispatch_after_terminal_delivery(
    fail_candidate: bool,
    terminal_delivery_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
) -> bool {
    fail_candidate && terminal_delivery_committed && !preserve_inflight_for_cleanup_retry
}

pub(super) fn empty_sink_preserves_retry(
    full_response: &str,
    resume_retry_queued: bool,
    response_sent_offset: usize,
    channel_id: ChannelId,
) -> bool {
    if full_response.is_empty() && resume_retry_queued {
        return false;
    }
    tracing::warn!(
        channel_id = channel_id.get(),
        full_response_len = full_response.len(),
        response_sent_offset,
        "turn_bridge reached empty terminal delivery without queued resume retry; preserving inflight for retry"
    );
    true
}

pub(super) fn empty_sink_commits_fully_consumed_response(
    full_response: &str,
    response_sent_offset: usize,
) -> bool {
    !full_response.trim().is_empty() && response_sent_offset >= full_response.len()
}

pub(super) fn mirror_frozen_prefix_ids(
    frozen_msg_ids: &[MessageId],
    inflight_state: &mut InflightTurnState,
) {
    for msg_id in frozen_msg_ids.iter().map(|msg_id| msg_id.get()) {
        if !inflight_state
            .streaming_rollover_frozen_msg_ids
            .contains(&msg_id)
        {
            inflight_state
                .streaming_rollover_frozen_msg_ids
                .push(msg_id);
        }
    }
}

pub(super) fn warn_preserved_uncommitted(
    terminal_delivery_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
    channel_id: ChannelId,
) {
    if terminal_delivery_committed || !preserve_inflight_for_cleanup_retry {
        return;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ turn bridge preserved inflight after terminal delivery failed for channel {} — response was not marked sent",
        channel_id
    );
}

// #3089 A5: `pub(super)` so the `terminal_controller_cutover` sibling's advance
// callback runs the SAME monotonic-CAS confirmed-end advance as the legacy site-5.
pub(super) fn advance_tmux_relay_confirmed_end(
    shared: &SharedData,
    channel_id: ChannelId,
    confirmed_end_offset: Option<u64>,
    tmux_session_name: Option<&str>,
) {
    let Some(target_end) = confirmed_end_offset.filter(|offset| *offset > 0) else {
        return;
    };

    let relay_coord = shared.tmux_relay_coord(channel_id);

    // #1270 codex P2 (round 4): capture the `.generation` mtime BEFORE
    // attempting the CAS so the stored mtime is the one that was on disk
    // when we decided to label `target_end` as delivered. Reading after
    // the CAS opens a TOCTOU window where a fresh respawn writes a new
    // `.generation` between our advance and our marker store, then the
    // new mtime ends up paired with the OLD offset and the next
    // regression check mis-classifies the next fresh respawn as
    // same-wrapper rotation. There is still a residual race between this
    // read and any advance that happens earlier in the watcher pipeline
    // (the bytes labelled `target_end` were produced by some prior
    // wrapper, which may already have been replaced before we got here);
    // the fully race-free fix would carry the mtime from byte-read time
    // through the delivery pipeline, but that's a bigger refactor and
    // the typical timeline (cancel → multi-second wait → respawn) keeps
    // this read aligned with the wrapper that produced the bytes.
    let mtime_at_attempt = tmux_session_name
        .map(tmux_generation_file_mtime_ns)
        .filter(|m| *m != 0);

    let mut current = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);
    let mut won_advance = false;

    while current < target_end {
        match relay_coord.confirmed_end_offset.compare_exchange(
            current,
            target_end,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => {
                won_advance = true;
                break;
            }
            Err(observed) => current = observed,
        }
    }

    // #964: observability timestamp — updated through the same relay-progress
    // heartbeat used by confirmed chunk sends. Regression recovery may benignly
    // zero `last_relay_ts_ms` when the output watermark is reset, but positive
    // relay progress still flows through this heartbeat path.
    relay_coord.note_relay_progress_heartbeat(chrono::Utc::now().timestamp_millis());

    // Pair the pre-CAS mtime with the offset only when we actually won
    // the advance. Losers and no-ops leave the mtime baseline alone so
    // the legitimate winner's snapshot remains the one that labels the
    // watermark (PR #1271 round 3).
    if won_advance && let Some(mtime) = mtime_at_attempt {
        relay_coord
            .confirmed_end_generation_mtime_ns
            .store(mtime, std::sync::atomic::Ordering::Release);
    }

    let confirmed_end = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);
    let confirmed_reached_target = confirmed_end >= target_end;
    crate::services::observability::record_invariant_check(
        confirmed_reached_target,
        crate::services::observability::InvariantViolation {
            provider: None,
            channel_id: Some(channel_id.get()),
            dispatch_id: None,
            session_key: None,
            turn_id: None,
            invariant: "tmux_confirmed_end_monotonic",
            code_location: "src/services/discord/turn_bridge/terminal_delivery.rs:advance_tmux_relay_confirmed_end",
            message: "tmux relay confirmed_end_offset must reach the delivered output end",
            details: serde_json::json!({
                "target_end": target_end,
                "confirmed_end": confirmed_end,
            }),
        },
    );
    debug_assert!(
        confirmed_reached_target,
        "tmux relay confirmed_end_offset must reach target end"
    );
}

/// #3041 P1-2: per-channel global counter that mints a unique `instance_id` for
/// each bridge delivery-lease attempt. `LeaseHolder::Bridge` has no `instance_id`
/// field today (only the watcher's holder kind carries one), so the bridge holder
/// identity is `(Bridge, turn, range)`. The counter is retained as future-proofing
/// / observability anchor but does not enter the lease key; the turn+range identity
/// already distinguishes sequential bridge attempts (each turn re-keys on its own
/// pinned `TurnKey`).
static BRIDGE_DELIVERY_LEASE_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// #3041 P1-2: RAII-ish guard that routes the BRIDGE's terminal delivery through
/// the SAME per-channel [`crate::services::discord::DeliveryLeaseCell`] the
/// watcher (P1-1) uses, so the watcher and the bridge are SERIALIZED — whoever
/// holds the live lease blocks the other's acquire (cross-actor duplicate
/// prevention, BLOCKER B6 / design §6 P1-2).
///
/// Lifecycle (mirrors the watcher's inline P1-1 wiring):
///   1. [`Self::acquire`] — `reclaim_if_expired` (self-heal a dead holder) then
///      `try_acquire(key, Bridge, [start,end), now+deadline)`. On success spawns
///      a [`crate::services::discord::DeliveryLeaseHeartbeat`] so a long chunked
///      send (which can exceed the 15s deadline) is never reclaimed mid-flight.
///      On FAILURE the cell is held by the watcher (or another bridge path) for
///      this range/turn → the caller MUST take a B2-style skip (NOT deliver+
///      advance); the live holder owns delivery.
///   2. caller performs `replace_message_with_outcome` / chunked send.
///   3. [`Self::commit_and_advance`] — stop the heartbeat, `commit(Bridge, key,
///      start, end, outcome)`; on `Delivered` AND a successful commit, advance
///      `confirmed_end_offset` (the B6 gate: the advance now ONLY happens via a
///      successful lease commit), then `release` so the cell is free for the next
///      turn. `NotDelivered`/`Unknown` → no advance.
///
/// No-deadlock: every cell op (`reclaim`/`try_acquire`/`renew`/`commit`/
/// `release`) is a synchronous, non-blocking lock on the cell's payload mutex —
/// none of them awaits or calls back into the other actor. The heartbeat lives on
/// its own task and only `renew`s our OWN lease; it is `stop()`ped before commit.
/// So the bridge never blocks on the watcher and vice-versa.
pub(super) struct BridgeDeliveryLease {
    cell: std::sync::Arc<crate::services::discord::DeliveryLeaseCell>,
    holder: crate::services::discord::LeaseHolder,
    key: crate::services::discord::DeliveryLeaseKey,
    start: u64,
    end: u64,
    heartbeat: Option<crate::services::discord::DeliveryLeaseHeartbeat>,
}

/// The result of attempting to acquire the bridge delivery lease for a terminal
/// delivery point.
pub(super) enum BridgeLeaseAcquire {
    /// We hold the lease; proceed to deliver, then `commit_and_advance`.
    Held(BridgeDeliveryLease),
    /// A different live holder (the watcher, or another bridge path) owns the
    /// lease for this range/turn. The caller MUST B2-skip: do NOT deliver or
    /// advance — the holder will commit-advance the offset itself.
    Skip,
    /// The range is empty / inverted (`end <= start`) or there is no `tmux_session`
    /// to advance against, so there is nothing to lease. The caller delivers
    /// exactly as before WITHOUT a lease and WITHOUT an offset advance (a zero
    /// range never advances `confirmed_end_offset`). This is the only path where a
    /// bridge terminal delivery is exempt from the lease — and it is exempt
    /// precisely because it never advances the offset, so B6 ("no advance outside
    /// a lease commit") is not violated.
    NoRange,
}

/// #3041 P1-2 (codex P1-c): whether the silent-turn suppression site (mod.rs
/// site 3) may mark `terminal_delivery_committed` for a given acquire outcome.
///
/// A B2 `Skip` means the live holder (the watcher) owns delivery of this range —
/// the bridge MUST be a no-op on completion side-effects (do NOT mark committed,
/// advance, or clear inflight as delivered) so the existing retry machinery can
/// re-attempt if the holder ultimately fails to deliver. `Held` (the bridge
/// committed the range itself) and `NoRange` (no bytes to deliver; the
/// suppression resolves the empty range) DO mark committed.
pub(super) fn silent_turn_skip_marks_committed(acquire: &BridgeLeaseAcquire) -> bool {
    !matches!(acquire, BridgeLeaseAcquire::Skip)
}

/// #3041 P1-2 (codex P1-c): the cleanup-epilogue decision seam. On EVERY bridge
/// skip arm (cancel/stop, prompt-too-long, silent_turn, long-send,
/// normal-replace) the bridge sets `preserve_inflight_for_cleanup_retry = true`
/// when the delivery-lease acquire returns [`BridgeLeaseAcquire::Skip`] — the
/// live holder (the watcher) owns delivery of this range, so the bridge must be a
/// TRUE no-op on completion side-effects.
///
/// These two pure predicates mirror the downstream epilogue gates so the
/// "a Skip preserves retry state" contract is unit-testable without driving the
/// whole 5000-line turn loop:
///   * [`bridge_epilogue_clears_inflight`] — the `~9017` clear-vs-preserve fork:
///     a preserved turn is SAVED (re-deliverable), never cleared.
///   * [`bridge_epilogue_marks_watcher_delivered`] — the `~8422` gate that signs
///     the watcher off as already-delivered: a preserved turn must NOT mark the
///     watcher delivered (it never delivered the range).
///
/// `~9017`: the bridge clears inflight ONLY when neither preserving for retry nor
/// delegating output to another owner. A preserved turn is saved, not cleared.
pub(super) fn bridge_epilogue_clears_inflight(
    preserve_inflight_for_cleanup_retry: bool,
    bridge_output_delegated: bool,
    cancelled_with_restart_mode: bool,
) -> bool {
    !cancelled_with_restart_mode && !preserve_inflight_for_cleanup_retry && !bridge_output_delegated
}

/// #3041 P1-2 (codex P1-2 R3): the cleanup-epilogue save-mode seam. On a
/// delivery-lease `Skip` the live HOLDER (the watcher) — a different actor
/// sharing the same per-channel `DeliveryLeaseCell` — owns this turn's delivery
/// AND its inflight lifecycle (the holder CLEARS inflight on its own success).
/// If the bridge's epilogue blindly re-`save_inflight_state`s after the holder
/// cleared the row, it resurrects a STALE inflight for an already-delivered turn
/// (recovery sees it delivered → returns WITHOUT clearing → permanent leak).
///
/// This predicate gates the epilogue's `~9168` preserve-save: when the preserve
/// is due to a Skip (`bridge_skip_holder_owns_inflight`), the save MUST be
/// identity-guarded (`save_inflight_state_if_identity_unchanged`) so a
/// holder-cleared / newer-turn row no-ops instead of resurrecting. Bridge-owned
/// cleanup-retry preserve sites bypass this predicate; delegated-owner preserve
/// is guarded separately in the epilogue because it can race the watcher clear.
/// Pure so the
/// "a Skip never resurrects a holder-cleared inflight" contract is unit-testable
/// without driving the whole turn loop.
pub(super) fn bridge_epilogue_skip_save_is_identity_guarded(
    bridge_skip_holder_owns_inflight: bool,
) -> bool {
    bridge_skip_holder_owns_inflight
}

/// `~8422`: the bridge signs the watcher off as already-delivered ONLY when not
/// preserving for retry and not delegating relay to the watcher. A preserved turn
/// must NOT mark the watcher delivered.
pub(super) fn bridge_epilogue_marks_watcher_delivered(
    preserve_inflight_for_cleanup_retry: bool,
    bridge_relay_delegated_to_watcher: bool,
) -> bool {
    !preserve_inflight_for_cleanup_retry && !bridge_relay_delegated_to_watcher
}

pub(super) fn bridge_delivery_lease_key_for_inflight(
    watcher_owner_channel_id: ChannelId,
    generation: u64,
    inflight: &crate::services::discord::inflight::InflightTurnState,
) -> crate::services::discord::DeliveryLeaseKey {
    crate::services::discord::DeliveryLeaseKey::from_inflight_state_for_site(
        watcher_owner_channel_id,
        generation,
        inflight,
        "bridge",
    )
}

pub(super) fn bridge_delivery_lease_for_inflight(
    shared: &SharedData,
    watcher_owner_channel_id: ChannelId,
    generation: u64,
    inflight: &crate::services::discord::inflight::InflightTurnState,
    target_end: Option<u64>,
) -> BridgeLeaseAcquire {
    BridgeDeliveryLease::acquire(
        shared,
        watcher_owner_channel_id,
        bridge_delivery_lease_key_for_inflight(watcher_owner_channel_id, generation, inflight),
        inflight.turn_start_offset.unwrap_or(0),
        target_end,
    )
}

impl BridgeDeliveryLease {
    /// Acquire the per-channel delivery lease for the bridge's terminal delivery
    /// covering `[start, end)` for `key`. `target_end` is the same end offset the
    /// pre-P1-2 `advance_tmux_relay_confirmed_end` advanced to (the bridge's
    /// `tmux_last_offset`); `start` is the turn's start offset (`turn_start_offset`,
    /// falling back to the same end so an unknown start yields an empty range that
    /// routes to [`BridgeLeaseAcquire::NoRange`]).
    ///
    /// `channel_id` MUST be the channel whose cell the WATCHER also leases on for
    /// this turn — the watcher's owner channel (`watcher_owner_channel_id` on the
    /// bridge side), NOT the bridge's dispatch `channel_id`. A reused watcher can
    /// own a DIFFERENT channel; if the bridge leased on its dispatch channel while
    /// the watcher leased on its owner channel, the two would hit DIFFERENT cells
    /// and both could acquire+deliver = duplicate (codex P1-a). Keying both on the
    /// watcher's owner channel makes their acquires contend on the SAME cell
    /// (single-holder B2). The same channel is used for the `TurnKey` and the
    /// `confirmed_end_offset` advance in `commit_and_advance`, so acquire, commit,
    /// and advance all operate on one consistent channel.
    pub(super) fn acquire(
        shared: &SharedData,
        channel_id: ChannelId,
        key: crate::services::discord::DeliveryLeaseKey,
        start: u64,
        target_end: Option<u64>,
    ) -> BridgeLeaseAcquire {
        let Some(end) = target_end.filter(|e| *e > 0) else {
            return BridgeLeaseAcquire::NoRange;
        };
        if end <= start {
            return BridgeLeaseAcquire::NoRange;
        }
        let _seq = BRIDGE_DELIVERY_LEASE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let holder = crate::services::discord::LeaseHolder::Bridge;
        let cell = shared.delivery_lease(channel_id);
        // SELF-HEALING acquire (mirrors the watcher B3): reclaim the lease IFF it
        // is EXPIRED (a dead holder that acquired but died before commit/release).
        // A LIVE holder mid-send keeps its deadline pushed forward by its
        // heartbeat, so it is NOT reclaimed and we correctly B2-skip it below.
        cell.reclaim_if_expired(crate::services::discord::lease_now_ms());
        let acquired = cell.try_acquire(
            key.clone(),
            holder,
            start,
            end,
            crate::services::discord::lease_now_ms()
                .saturating_add(crate::services::discord::DELIVERY_LEASE_DEADLINE_MS),
        );
        if !acquired {
            return BridgeLeaseAcquire::Skip;
        }
        // Keep the lease alive WHILE the (possibly chunked, >15s) send runs.
        let heartbeat = Some(crate::services::discord::DeliveryLeaseHeartbeat::spawn(
            cell.clone(),
            holder,
            key.clone(),
        ));
        BridgeLeaseAcquire::Held(BridgeDeliveryLease {
            cell,
            holder,
            key,
            start,
            end,
            heartbeat,
        })
    }

    /// #3610 PR-1c: the lease's committed `(start, end)` offset range — the SAME
    /// range `acquire` took and `commit_and_advance` advances `confirmed_end_offset`
    /// to. The long-chunk anchor record (mod.rs site 4) reads this BEFORE
    /// `commit_and_advance` consumes `self` so the durable frontier range is
    /// offset-consistent with the in-memory advance (no offset-space mixing).
    pub(super) fn range(&self) -> (u64, u64) {
        (self.start, self.end)
    }

    /// Stop the heartbeat, commit the 3-way `outcome`, and — ONLY on a successful
    /// `Delivered` commit — advance `confirmed_end_offset` to the leased `end` via
    /// `advance_tmux_relay_confirmed_end`. Then release. This is the B6 gate: the
    /// confirmed-end advance happens IFF the Delivered lease commit succeeds. Returns
    /// `true` iff the commit succeeded (debug invariant: the bridge must be able to
    /// commit its own freshly-acquired lease).
    pub(super) fn commit_and_advance(
        mut self,
        shared: &SharedData,
        watcher_owner_channel_id: ChannelId,
        tmux_session_name: Option<&str>,
        outcome: crate::services::discord::LeaseOutcome,
    ) -> bool {
        // STOP the heartbeat BEFORE the commit so the renew loop cannot race it.
        if let Some(hb) = self.heartbeat.take() {
            hb.stop();
        }
        let committed =
            self.cell
                .commit(self.holder, self.key.clone(), self.start, self.end, outcome);
        debug_assert!(
            committed,
            "bridge must be able to commit its own freshly-acquired delivery lease"
        );
        if committed && outcome == crate::services::discord::LeaseOutcome::Delivered {
            // B6: the ONLY confirmed_end advance on the bridge terminal path now
            // flows through this successful lease commit.
            advance_tmux_relay_confirmed_end(
                shared,
                watcher_owner_channel_id,
                Some(self.end),
                tmux_session_name,
            );
        }
        // Release (compare-and-release, identity-matched) so the cell returns to
        // Unleased for the NEXT turn — this is what lets the OTHER actor (watcher)
        // proceed. Idempotent no-op if the lease was reclaimed (holder presumed
        // dead) in the meantime.
        let _ = self
            .cell
            .release(self.holder, self.key.clone(), self.start, self.end);
        committed
    }
}

impl Drop for BridgeDeliveryLease {
    fn drop(&mut self) {
        // Safety net for an early return / panic between `acquire` and
        // `commit_and_advance`: abort the heartbeat (its own Drop also does this)
        // and abandon-release the still-`Leased` lease so a dropped bridge frame
        // never strands the cell (the deadline reclaim would also free it, but
        // releasing immediately lets the next turn / the watcher proceed without
        // waiting out the deadline). Identity-matched, so it is a harmless no-op
        // if `commit_and_advance` already released.
        self.heartbeat.take();
        let _ = self
            .cell
            .release(self.holder, self.key.clone(), self.start, self.end);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bridge_epilogue_clears_inflight, bridge_epilogue_marks_watcher_delivered,
        bridge_epilogue_skip_save_is_identity_guarded, empty_sink_commits_fully_consumed_response,
        empty_sink_preserves_retry, mirror_frozen_prefix_ids,
        record_stopped_turn_terminal_replace_delivery, replace_outcome_commits_terminal_delivery,
        send_ordered_long_terminal_chunks, should_complete_work_dispatch_after_terminal_delivery,
        should_fail_dispatch_after_terminal_delivery, terminal_delivery_should_send_new_chunks,
    };
    use crate::services::discord::formatting;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
    use crate::services::discord::{
        DeliveryLeaseKey, InflightTurnState, make_shared_data_for_tests,
    };
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeOrderedChunkGateway {
        sent_chunks: Arc<Mutex<Vec<String>>>,
        deleted_messages: Arc<Mutex<Vec<MessageId>>>,
        fail_after_sent_chunks: Option<usize>,
    }

    impl TurnGateway for FakeOrderedChunkGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async { Err("single-message send must not be used".to_string()) })
        }

        fn send_long_message_with_rollback<'a>(
            &'a self,
            _channel_id: ChannelId,
            _rollback_anchor_msg_id: MessageId,
            content: &'a str,
        ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
            let sent_chunks = self.sent_chunks.clone();
            let fail_after_sent_chunks = self.fail_after_sent_chunks;
            Box::pin(async move {
                let chunks = formatting::split_message(content);
                let mut message_ids = Vec::new();
                for (index, chunk) in chunks.iter().enumerate() {
                    sent_chunks
                        .lock()
                        .expect("sent chunks lock")
                        .push(chunk.clone());
                    message_ids.push(MessageId::new(9000 + index as u64));
                    if fail_after_sent_chunks == Some(index + 1) {
                        sent_chunks.lock().expect("sent chunks lock").clear();
                        return Err("simulated chunk failure after rollback".to_string());
                    }
                }
                Ok(message_ids)
            })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            message_id: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            let deleted_messages = self.deleted_messages.clone();
            Box::pin(async move {
                deleted_messages
                    .lock()
                    .expect("deleted messages lock")
                    .push(message_id);
                Ok(())
            })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a crate::services::discord::Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            true
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Codex)
        }
    }

    #[test]
    fn work_dispatch_completion_requires_terminal_delivery_commit() {
        assert!(should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            false,
            false,
            "visible final response",
        ));

        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            false,
            false,
            false,
            false,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            true,
            false,
            false,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            true,
            false,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            false,
            true,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true, true, false, false, false, "   ",
        ));
    }

    #[test]
    fn final_completion_delivery_stays_blocked_until_terminal_message_commits() {
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            false,
            false,
            false,
            false,
            "final response waiting for Discord delivery",
        ));
        assert!(should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            false,
            false,
            "final response delivered",
        ));
    }

    // #3041 P1-2 (codex P1-c): every bridge skip arm sets
    // `preserve_inflight_for_cleanup_retry = true`. These predicates encode the two
    // downstream epilogue gates the production loop now routes through; the
    // assertions prove a Skip (preserve = true) is a TRUE no-op on completion
    // side-effects — inflight is NOT cleared and the watcher is NOT marked
    // delivered — so the turn stays fully retry-able if the holder later fails.
    #[test]
    fn bridge_skip_preserves_inflight_and_does_not_mark_watcher_delivered() {
        // A B2 Skip sets preserve = true → the epilogue must NOT clear inflight…
        assert!(
            !bridge_epilogue_clears_inflight(true, false, false),
            "a preserved (skipped) turn must NOT clear inflight — it stays retry-able"
        );
        // …and must NOT mark the watcher delivered (the bridge never delivered it).
        assert!(
            !bridge_epilogue_marks_watcher_delivered(true, false),
            "a preserved (skipped) turn must NOT mark the watcher delivered"
        );
    }

    // #3041 P1-2 (codex P1-2 R3): the epilogue preserve-save MUST be
    // identity-guarded ONLY on a Skip (the holder owns the inflight lifecycle and
    // may have cleared the row on success). Bridge-owned preserve sites and the
    // delegated-owner path keep the blind save (no competing holder).
    #[test]
    fn empty_sink_preserves_retry_unless_resume_retry_was_queued() {
        let channel = ChannelId::new(42);
        assert!(empty_sink_preserves_retry(
            "Error: transport failed",
            false,
            4096,
            channel,
        ));
        assert!(!empty_sink_preserves_retry("", true, 0, channel));
    }

    #[test]
    fn empty_sink_commits_nonempty_response_that_was_already_fully_consumed() {
        assert!(empty_sink_commits_fully_consumed_response(
            "already delivered",
            "already delivered".len()
        ));
        assert!(empty_sink_commits_fully_consumed_response(
            "already delivered",
            "already delivered".len() + 10
        ));
        assert!(!empty_sink_commits_fully_consumed_response("", 0));
        assert!(!empty_sink_commits_fully_consumed_response(
            "tail remains",
            "tail ".len()
        ));
    }

    #[test]
    fn bridge_rollover_frozen_prefix_ids_are_mirrored_for_watcher_cleanup() {
        let mut inflight = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("agentdesk-test".to_string()),
            7,
            1001,
            1002,
            "prompt".to_string(),
            None,
            Some("AgentDesk-codex-prefix-cleanup".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        inflight.streaming_rollover_frozen_msg_ids = vec![10];

        mirror_frozen_prefix_ids(&[MessageId::new(10), MessageId::new(11)], &mut inflight);

        assert_eq!(inflight.streaming_rollover_frozen_msg_ids, vec![10, 11]);
    }

    #[test]
    fn bridge_skip_save_is_identity_guarded_only_when_holder_owns_inflight() {
        // Skip → holder (watcher) owns inflight → the save MUST be identity-guarded
        // so a holder-cleared row is never resurrected.
        assert!(
            bridge_epilogue_skip_save_is_identity_guarded(true),
            "on a Skip the holder owns the inflight lifecycle; the epilogue save must be identity-guarded so it never resurrects a holder-cleared row"
        );
        // Bridge-owned preserve (EditFailed / PG-cancel-fail / send-fail / TUI
        // timeout) or delegated-owner → no competing holder → the blind save stays
        // authoritative.
        assert!(
            !bridge_epilogue_skip_save_is_identity_guarded(false),
            "bridge-owned preserve and delegated paths have no competing holder; the blind epilogue save stays authoritative"
        );
    }

    #[test]
    fn bridge_non_skip_normal_completion_clears_and_marks_delivered() {
        // A normal committed turn (preserve = false, no delegation, not a
        // restart-cancel) DOES clear inflight and DOES mark the watcher delivered.
        assert!(bridge_epilogue_clears_inflight(false, false, false));
        assert!(bridge_epilogue_marks_watcher_delivered(false, false));
    }

    #[test]
    fn bridge_delegation_and_restart_cancel_never_clear_inflight() {
        // Output delegated to another owner → preserve (save), never clear.
        assert!(!bridge_epilogue_clears_inflight(false, true, false));
        // A restart-mode cancel saves inflight on its own branch → never clear here.
        assert!(!bridge_epilogue_clears_inflight(false, false, true));
        // Relay delegated to the watcher → the watcher relays itself; the bridge
        // must not also sign it off as delivered.
        assert!(!bridge_epilogue_marks_watcher_delivered(false, true));
    }

    /// #3089 A1 r3 pin — terminal_delivery does NOT commit a fallback-after-
    /// edit-failure. The commit predicate matches `EditedOriginal` only
    /// (`replace_outcome_commits_terminal_delivery`, this file `:42`), and the
    /// live path records the cleanup failure and returns `committed = false`
    /// (this file `:143`). This characterization pins that non-commit branch
    /// BEFORE A5 cuts turn_bridge over to the unified controller: the controller
    /// must pass `FallbackCommitPolicy::NoCommitOnFallback` for this owner to
    /// preserve the behavior pinned here (sink/standby pass `CommitOnFallback`).
    /// If the predicate ever started committing the fallback variant, this test
    /// fails and the A5 cutover would be caught.
    #[test]
    fn sent_fallback_after_edit_failure_does_not_commit_terminal_delivery() {
        let outcome = ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
            edit_error: "edit 500; fallback POST succeeded".to_string(),
            replacement_anchor: None,
        };

        // The commit predicate: a fallback-after-edit-failure is NOT a commit.
        assert!(!replace_outcome_commits_terminal_delivery(&outcome));
        // And so the downstream dispatch gates do not complete the work
        // dispatch on a non-committed terminal delivery.
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            replace_outcome_commits_terminal_delivery(&outcome),
            false,
            false,
            false,
            "final response delivered via fallback after edit failure",
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true,
            replace_outcome_commits_terminal_delivery(&outcome),
            false,
        ));

        // Contrast: an actual edit IS a commit (the only committing variant).
        assert!(replace_outcome_commits_terminal_delivery(
            &ReplaceLongMessageOutcome::EditedOriginal
        ));
    }

    #[test]
    fn partial_continuation_failure_does_not_commit_terminal_delivery() {
        let outcome = ReplaceLongMessageOutcome::PartialContinuationFailure {
            sent_chunks: 1,
            total_chunks: 3,
            failed_chunk_index: 1,
            sent_continuation_message_ids: Vec::new(),
            cleanup_errors: Vec::new(),
            error: "HTTP 500".to_string(),
        };

        assert!(!replace_outcome_commits_terminal_delivery(&outcome));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            replace_outcome_commits_terminal_delivery(&outcome),
            false,
            false,
            false,
            "final response with missing continuation",
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true,
            replace_outcome_commits_terminal_delivery(&outcome),
            false,
        ));
    }

    #[test]
    fn long_terminal_response_uses_new_chunk_messages() {
        let body = format!(
            "[E2E:E15:BEGIN]\n{}\n[E2E:E15:MID]\n{}\n[E2E:E15:END]",
            "E15-LINE-010\n".repeat(90),
            "E15-LINE-150\n".repeat(90)
        );

        assert!(body.len() > crate::services::discord::DISCORD_MSG_LIMIT);
        assert!(terminal_delivery_should_send_new_chunks(true, &body));
        assert!(!terminal_delivery_should_send_new_chunks(
            true,
            "[E2E:E15:BEGIN]\nE15-LINE-150\n[E2E:E15:END]"
        ));
        assert!(!terminal_delivery_should_send_new_chunks(false, &body));
    }

    #[tokio::test]
    async fn ordered_long_terminal_delivery_sends_all_chunks_and_deletes_placeholder() {
        let body = format!(
            "[E2E:E15:BEGIN]{}[E2E:E15:MID]{}[E2E:E15:END]",
            "A".repeat(2500),
            "B".repeat(2500)
        );
        let gateway = FakeOrderedChunkGateway::default();
        let placeholder_msg_id = MessageId::new(42);

        let (first_msg_id, last_msg_id, delete_result) = send_ordered_long_terminal_chunks(
            &gateway,
            ChannelId::new(7),
            placeholder_msg_id,
            &body,
        )
        .await
        .expect("ordered long terminal send");

        assert_eq!(first_msg_id, MessageId::new(9000));
        assert!(delete_result.is_ok());
        let chunks = gateway
            .sent_chunks
            .lock()
            .expect("sent chunks lock")
            .clone();
        assert!(chunks.len() > 1);
        // #3610 PR-1c: the anchor is the LAST chunk's id (the fake mints
        // `9000 + index`), so it tracks the tail chunk, not the first.
        assert_eq!(
            last_msg_id,
            Some(MessageId::new(9000 + chunks.len() as u64 - 1))
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= crate::services::discord::DISCORD_MSG_LIMIT)
        );
        assert_eq!(chunks.concat(), body);
        assert_eq!(
            gateway
                .deleted_messages
                .lock()
                .expect("deleted messages lock")
                .as_slice(),
            &[placeholder_msg_id]
        );
    }

    #[tokio::test]
    async fn ordered_long_terminal_delivery_rolls_back_partial_chunks_before_retry() {
        let body = format!(
            "[E2E:E15:BEGIN]{}[E2E:E15:MID]{}[E2E:E15:END]",
            "A".repeat(2500),
            "B".repeat(2500)
        );
        let gateway = FakeOrderedChunkGateway {
            fail_after_sent_chunks: Some(1),
            ..FakeOrderedChunkGateway::default()
        };

        let result = send_ordered_long_terminal_chunks(
            &gateway,
            ChannelId::new(7),
            MessageId::new(42),
            &body,
        )
        .await;

        assert!(result.is_err());
        assert!(
            gateway
                .sent_chunks
                .lock()
                .expect("sent chunks lock")
                .is_empty(),
            "rollback-aware sender must not leave partial chunks that a retry would duplicate"
        );
        assert!(
            gateway
                .deleted_messages
                .lock()
                .expect("deleted messages lock")
                .is_empty(),
            "placeholder cleanup must wait until all chunks commit"
        );
    }

    #[test]
    fn transport_error_dispatch_failure_requires_terminal_delivery_commit() {
        assert!(should_fail_dispatch_after_terminal_delivery(
            true, true, false,
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true, false, false,
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true, true, true,
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            false, true, false,
        ));
    }

    // #4353: drives `discord::tmux` (cfg(unix)) directly.
    #[cfg(unix)]
    #[test]
    fn stopped_turn_terminal_replace_raw_fingerprint_refuses_phantom_rerelay_4081() {
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
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().expect("temp runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let session = "AgentDesk-codex-adk-cdx-stop-raw-4081";
        let channel_id = ChannelId::new(7_4084);
        let raw_body = "# stopped heading\nraw extractor body";
        let display_body = format!(
            "{}\n\n[Stopped]",
            formatting::format_for_discord_with_provider(raw_body, &provider)
        );
        assert_ne!(raw_body, display_body);

        let gen_path = crate::services::tmux_common::session_temp_path(session, "generation");
        std::fs::create_dir_all(std::path::Path::new(&gen_path).parent().unwrap()).unwrap();
        std::fs::write(&gen_path, b"1").unwrap();
        crate::services::discord::tmux::advance_watcher_confirmed_end(
            &shared,
            &provider,
            channel_id,
            session,
            raw_body.len() as u64,
            "terminal_delivery_tests:stopped_turn_terminal_replace_raw_fingerprint_refuses_phantom_rerelay_4081",
        );
        record_stopped_turn_terminal_replace_delivery(
            &shared,
            &provider,
            channel_id,
            (0, raw_body.len() as u64),
            MessageId::new(94_084),
            channel_id,
            raw_body,
        );

        let degenerate_key =
            DeliveryLeaseKey::new_for_site(channel_id, 33, 0, None, None, "watcher");
        let recent_raw =
            super::super::super::outbound::delivery_record::recent_delivered_content_matches(
                &provider, channel_id, session, raw_body,
            );
        assert!(degenerate_key.is_degenerate_legacy());
        assert!(
            recent_raw,
            "stopped-turn terminal replace must fingerprint the raw extractor body"
        );
        assert!(
            !super::super::super::outbound::delivery_record::recent_delivered_content_matches(
                &provider,
                channel_id,
                session,
                &display_body,
            ),
            "display formatting plus [Stopped] must not enter the duplicate fingerprint"
        );
        let fresh_assistant_text_in_observed_range = false;
        assert!(
            degenerate_key.is_degenerate_legacy()
                && recent_raw
                && !fresh_assistant_text_in_observed_range,
            "phantom degenerate-key re-relay of the same raw bytes must be refused"
        );
    }

    // #3041 P1-2: matrix tests for the BRIDGE delivery-lease wiring. These drive
    // `BridgeDeliveryLease::acquire` / `commit_and_advance` against a REAL
    // per-channel `DeliveryLeaseCell` (the SAME cell the watcher uses), proving:
    //   - Bridge/Delivered advances `confirmed_end_offset` exactly once via the
    //     lease commit (B6: advance only on a successful Delivered commit);
    //   - Bridge acquire-contention with a watcher holding the lease → Skip (and
    //     the converse: the watcher's `try_acquire` skips when the bridge holds);
    //   - Bridge/Unknown and Bridge/NotDelivered → no advance;
    //   - Bridge then watcher next turn → the second turn acquires fine (release
    //     works);
    //   - no double-advance on a same-range re-commit.
    // `start_paused` keeps the heartbeat's `tokio::time::interval` from doing real
    // sleeps; the lease deadline reclaim is driven via explicit `now_ms` args.
    mod bridge_delivery_lease {
        use crate::services::discord::turn_finalizer::TurnKey;
        use crate::services::discord::{
            DELIVERY_LEASE_DEADLINE_MS, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseSnapshot,
            lease_now_ms, make_shared_data_for_tests,
        };
        use poise::serenity_prelude::ChannelId;

        use super::super::{
            BridgeDeliveryLease, BridgeLeaseAcquire, bridge_delivery_lease_key_for_inflight,
        };
        use crate::services::discord::inflight::InflightTurnState;
        use crate::services::provider::ProviderKind;

        const CH: u64 = 909_001;

        fn channel() -> ChannelId {
            ChannelId::new(CH)
        }

        fn turn(user_msg_id: u64) -> DeliveryLeaseKey {
            DeliveryLeaseKey::from_turn_key(TurnKey::new(channel(), user_msg_id, 1))
        }

        #[tokio::test(start_paused = true)]
        async fn bridge_delivered_advances_offset_once_via_lease_commit() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            assert_eq!(shared.committed_relay_offset(ch), 0);

            let acquire = BridgeDeliveryLease::acquire(&shared, ch, turn(11), 0, Some(64));
            let lease = match acquire {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("expected Held on a fresh cell with a real range"),
            };
            // While the bridge holds the lease, the cell is Leased by Bridge.
            assert!(matches!(
                shared.delivery_lease(ch).read(),
                LeaseSnapshot::Leased {
                    holder: LeaseHolder::Bridge,
                    ..
                }
            ));

            let committed = lease.commit_and_advance(&shared, ch, None, LeaseOutcome::Delivered);
            assert!(committed, "bridge must commit its own fresh lease");
            assert_eq!(
                shared.committed_relay_offset(ch),
                64,
                "Delivered commit advances confirmed_end to the leased end"
            );
            // Released back to Unleased after commit.
            assert!(matches!(
                shared.delivery_lease(ch).read(),
                LeaseSnapshot::Unleased
            ));
        }

        #[tokio::test(start_paused = true)]
        async fn bridge_unknown_outcome_does_not_advance() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            let lease = match BridgeDeliveryLease::acquire(&shared, ch, turn(12), 0, Some(64)) {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("expected Held"),
            };
            lease.commit_and_advance(&shared, ch, None, LeaseOutcome::Unknown);
            assert_eq!(
                shared.committed_relay_offset(ch),
                0,
                "Unknown must NOT advance the offset"
            );
            assert!(matches!(
                shared.delivery_lease(ch).read(),
                LeaseSnapshot::Unleased
            ));
        }

        #[tokio::test(start_paused = true)]
        async fn bridge_not_delivered_does_not_advance() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            let lease = match BridgeDeliveryLease::acquire(&shared, ch, turn(13), 0, Some(64)) {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("expected Held"),
            };
            lease.commit_and_advance(&shared, ch, None, LeaseOutcome::NotDelivered);
            assert_eq!(shared.committed_relay_offset(ch), 0);
        }

        #[tokio::test(start_paused = true)]
        async fn bridge_skips_when_watcher_holds_lease() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            // A watcher acquires the SAME per-channel cell first (live, not yet
            // committed/released/reclaimed).
            let cell = shared.delivery_lease(ch);
            let watcher = LeaseHolder::Watcher { instance_id: 7 };
            assert!(cell.try_acquire(
                turn(20),
                watcher,
                0,
                64,
                lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
            ));
            // The bridge's acquire for the same range must B2-skip.
            assert!(matches!(
                BridgeDeliveryLease::acquire(&shared, ch, turn(20), 0, Some(64)),
                BridgeLeaseAcquire::Skip
            ));
            assert_eq!(
                shared.committed_relay_offset(ch),
                0,
                "skipped bridge must not advance"
            );
            // Watcher still holds it (the bridge's failed acquire did not touch it).
            assert!(matches!(
                cell.read(),
                LeaseSnapshot::Leased {
                    holder: LeaseHolder::Watcher { instance_id: 7 },
                    ..
                }
            ));
        }

        #[tokio::test(start_paused = true)]
        async fn bridge_id0_without_offset_acquires_degenerate_key_instead_of_skip() {
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
            let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
            let temp = tempfile::TempDir::new().expect("temp runtime root");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

            let shared = make_shared_data_for_tests();
            let ch = channel();
            let mut inflight = InflightTurnState::new(
                ProviderKind::Codex,
                ch.get(),
                Some("agentdesk-test".to_string()),
                7,
                0,
                123,
                "prompt".to_string(),
                None,
                Some("AgentDesk-codex-degenerate-bridge".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                Some("/tmp/in.fifo".to_string()),
                0,
            );
            inflight.started_at = "2026-07-03T06:00:00Z".to_string();
            inflight.turn_start_offset = None;

            let key = bridge_delivery_lease_key_for_inflight(ch, 1, &inflight);
            let acquire = BridgeDeliveryLease::acquire(&shared, ch, key, 0, Some(64));
            let lease = match acquire {
                BridgeLeaseAcquire::Held(lease) => lease,
                BridgeLeaseAcquire::Skip => panic!("degenerate id-0 bridge key must acquire"),
                BridgeLeaseAcquire::NoRange => panic!("test range is non-empty"),
            };

            assert!(lease.commit_and_advance(&shared, ch, None, LeaseOutcome::Delivered));
            assert_eq!(shared.committed_relay_offset(ch), 64);
        }

        #[tokio::test(start_paused = true)]
        async fn watcher_skips_when_bridge_holds_lease() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            // The bridge acquires first.
            let _lease = match BridgeDeliveryLease::acquire(&shared, ch, turn(21), 0, Some(64)) {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("expected Held"),
            };
            // A watcher's `try_acquire` on the SAME cell must lose (single holder).
            let cell = shared.delivery_lease(ch);
            let watcher = LeaseHolder::Watcher { instance_id: 8 };
            assert!(
                !cell.try_acquire(
                    turn(21),
                    watcher,
                    0,
                    64,
                    lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
                ),
                "watcher must B2-skip while the bridge holds the live lease"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn bridge_release_lets_next_turn_acquire() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            // Turn 1: bridge delivers and commits.
            let lease = match BridgeDeliveryLease::acquire(&shared, ch, turn(30), 0, Some(32)) {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("expected Held"),
            };
            lease.commit_and_advance(&shared, ch, None, LeaseOutcome::Delivered);
            assert_eq!(shared.committed_relay_offset(ch), 32);

            // Turn 2 (a later, non-overlapping range): the watcher acquires fine
            // because the bridge released the cell.
            let cell = shared.delivery_lease(ch);
            let watcher = LeaseHolder::Watcher { instance_id: 9 };
            assert!(
                cell.try_acquire(
                    turn(31),
                    watcher,
                    32,
                    96,
                    lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
                ),
                "release must free the cell for the next turn's acquirer"
            );
            assert!(cell.commit(watcher, turn(31), 32, 96, LeaseOutcome::Delivered));
        }

        #[tokio::test(start_paused = true)]
        async fn no_double_advance_on_same_range_recommit() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            // First Delivered commit advances to 64.
            let lease = match BridgeDeliveryLease::acquire(&shared, ch, turn(40), 0, Some(64)) {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("expected Held"),
            };
            lease.commit_and_advance(&shared, ch, None, LeaseOutcome::Delivered);
            assert_eq!(shared.committed_relay_offset(ch), 64);

            // A same-holder re-acquire+commit of the SAME range advances to the SAME
            // 64 — the monotonic CAS in `advance_tmux_relay_confirmed_end` cannot
            // double-advance.
            let lease2 = match BridgeDeliveryLease::acquire(&shared, ch, turn(40), 0, Some(64)) {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("expected Held on re-acquire after release"),
            };
            lease2.commit_and_advance(&shared, ch, None, LeaseOutcome::Delivered);
            assert_eq!(
                shared.committed_relay_offset(ch),
                64,
                "same-range re-commit must not double-advance"
            );
        }

        // #3041 P1-2 (codex P1-a): a REUSED watcher can own a channel DIFFERENT
        // from the bridge's dispatch `channel_id`. The watcher leases (and advances
        // `confirmed_end_offset`) on ITS owner channel's cell. The bridge MUST lease
        // on that SAME owner-channel cell (it passes `watcher_owner_channel_id`), so
        // the two contend on ONE cell (single-holder B2) and never both deliver.
        // This test proves the cell is shared by-channel: the watcher holds the
        // OWNER channel's cell; the bridge acquiring on the OWNER channel B2-skips
        // (same cell), while a bridge acquiring on the unrelated dispatch channel
        // would have hit a DIFFERENT cell and wrongly acquired — which is exactly
        // the duplicate the P1-a fix routes the bridge onto the owner channel to
        // avoid.
        #[tokio::test(start_paused = true)]
        async fn bridge_leases_on_watcher_owner_channel_shares_cell_under_reuse() {
            let shared = make_shared_data_for_tests();
            // The reused watcher's OWNER channel (where it leases + advances).
            let owner_ch = ChannelId::new(CH);
            // The bridge's dispatch channel — DIFFERENT from the owner (watcher
            // reuse). Its cell is a SEPARATE `DeliveryLeaseCell`.
            let dispatch_ch = ChannelId::new(CH + 1);
            assert_ne!(owner_ch, dispatch_ch);

            // The watcher acquires the OWNER channel's cell, keyed on the owner
            // channel's TurnKey (mirrors `tmux_output_watcher_with_restore`, which
            // leases on its own `channel_id` == owner).
            let owner_cell = shared.delivery_lease(owner_ch);
            let watcher = LeaseHolder::Watcher { instance_id: 70 };
            let watcher_turn = DeliveryLeaseKey::from_turn_key(TurnKey::new(owner_ch, 99, 1));
            assert!(owner_cell.try_acquire(
                watcher_turn,
                watcher,
                0,
                64,
                lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
            ));

            // P1-a fix: the bridge acquires on `watcher_owner_channel_id` (the OWNER
            // channel) → SAME cell → B2-skip (contention detected, NOT both-deliver).
            let bridge_turn = DeliveryLeaseKey::from_turn_key(TurnKey::new(owner_ch, 99, 1));
            assert!(
                matches!(
                    BridgeDeliveryLease::acquire(&shared, owner_ch, bridge_turn, 0, Some(64)),
                    BridgeLeaseAcquire::Skip
                ),
                "bridge keyed on the watcher's owner channel must hit the SAME cell and B2-skip"
            );

            // Regression contrast: keying on the unrelated DISPATCH channel hits a
            // DIFFERENT cell → the bridge would WRONGLY acquire (the pre-fix
            // duplicate). This documents WHY the bridge must use the owner channel.
            let dispatch_turn = DeliveryLeaseKey::from_turn_key(TurnKey::new(dispatch_ch, 99, 1));
            assert!(
                matches!(
                    BridgeDeliveryLease::acquire(&shared, dispatch_ch, dispatch_turn, 0, Some(64)),
                    BridgeLeaseAcquire::Held(_)
                ),
                "the unrelated dispatch-channel cell is a DIFFERENT cell — leasing there would duplicate"
            );

            // The watcher still holds the owner cell; neither bridge acquire touched it.
            assert!(matches!(
                owner_cell.read(),
                LeaseSnapshot::Leased {
                    holder: LeaseHolder::Watcher { instance_id: 70 },
                    ..
                }
            ));
        }

        // #3041 P1-2 (codex P1-b): an equal NONZERO range (`end == start`, e.g.
        // start==end==64) routes to `NoRange` — there are NO new bytes to commit, so
        // the bridge delivers WITHOUT a lease and NEVER advances `confirmed_end`. The
        // pre-fix bug advanced the offset to 64 outside any lease commit (B6
        // violation); this asserts no advance happens.
        #[tokio::test(start_paused = true)]
        async fn equal_nonzero_range_is_no_range_and_never_advances() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            assert_eq!(shared.committed_relay_offset(ch), 0);
            // start == end == 64 (nonzero, degenerate) → NoRange.
            assert!(matches!(
                BridgeDeliveryLease::acquire(&shared, ch, turn(60), 64, Some(64)),
                BridgeLeaseAcquire::NoRange
            ));
            // No lease was held, so there is nothing to commit/advance. The offset
            // MUST remain at its prior value (B6: no advance outside a lease commit).
            assert_eq!(
                shared.committed_relay_offset(ch),
                0,
                "an equal nonzero range must NOT advance confirmed_end (codex P1-b / B6)"
            );
            assert!(matches!(
                shared.delivery_lease(ch).read(),
                LeaseSnapshot::Unleased
            ));
        }

        // #3041 P1-2 (codex P1-c): when the bridge B2-skips (the watcher holds the
        // live lease), the skip must be a NO-OP on completion side-effects — the
        // bridge must NOT advance and the watcher (the live holder) retains exclusive
        // ownership of the range. If the holder later commits NotDelivered (it did
        // NOT actually deliver), a SUBSEQUENT acquirer can still take the range and
        // deliver — i.e. the skip never black-holes the delivery.
        // #3041 P1-2 (codex P1-c): the silent-turn commit decision. A Skip must NOT
        // mark `terminal_delivery_committed` (the holder owns delivery; stay
        // retry-able); Held/NoRange DO mark it.
        #[test]
        fn silent_turn_skip_does_not_mark_committed() {
            use super::super::silent_turn_skip_marks_committed;
            assert!(
                !silent_turn_skip_marks_committed(&BridgeLeaseAcquire::Skip),
                "a B2-skip must leave the turn uncommitted so retry remains possible (codex P1-c)"
            );
            assert!(silent_turn_skip_marks_committed(
                &BridgeLeaseAcquire::NoRange
            ));
            // `Held` carries a lease we cannot cheaply construct here; the match in
            // `silent_turn_skip_marks_committed` returns `true` for every non-Skip
            // variant (verified for `NoRange` above and exercised for `Held` by the
            // lease-level tests that drive the real site).
        }

        #[tokio::test(start_paused = true)]
        async fn bridge_skip_leaves_range_deliverable_after_holder_not_delivered() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            // Watcher holds the live lease.
            let cell = shared.delivery_lease(ch);
            let watcher = LeaseHolder::Watcher { instance_id: 77 };
            assert!(cell.try_acquire(
                turn(80),
                watcher,
                0,
                64,
                lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
            ));
            // Bridge B2-skips and does NOT advance.
            assert!(matches!(
                BridgeDeliveryLease::acquire(&shared, ch, turn(80), 0, Some(64)),
                BridgeLeaseAcquire::Skip
            ));
            assert_eq!(
                shared.committed_relay_offset(ch),
                0,
                "a B2-skip must not advance — the holder owns delivery"
            );

            // The holder later commits NotDelivered (it did NOT deliver) and releases.
            assert!(cell.commit(watcher, turn(80), 0, 64, LeaseOutcome::NotDelivered));
            assert_eq!(
                shared.committed_relay_offset(ch),
                0,
                "NotDelivered must not advance — the range is still undelivered"
            );
            assert!(cell.release(watcher, turn(80), 0, 64));

            // Because the skip was a no-op (offset still 0, cell released), a
            // subsequent acquirer (the bridge or a later watcher pass) can STILL take
            // the range and deliver it — the skip never black-holed the delivery.
            let retry = match BridgeDeliveryLease::acquire(&shared, ch, turn(80), 0, Some(64)) {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("range must be re-acquirable after the holder released NotDelivered"),
            };
            assert!(retry.commit_and_advance(&shared, ch, None, LeaseOutcome::Delivered));
            assert_eq!(
                shared.committed_relay_offset(ch),
                64,
                "the retry delivers the previously-skipped range (no black-hole)"
            );
        }

        // #3610 (codex review): the M4 gate the long-chunk arm (mod.rs site 4)
        // now relies on. `record_long_chunk_terminal_delivery` MUST only fire when
        // `commit_and_advance` returned `true` — i.e. the in-memory
        // `confirmed_end_offset` actually advanced. This pins the underlying
        // coupling the bool gate depends on: in a RECLAIMED / identity-mismatch
        // state the bridge's own `DeliveryLeaseCell::commit` returns `false` AND
        // `confirmed_end_offset` does NOT advance. (The full `commit_and_advance`
        // wrapper carries a `debug_assert!(committed)` — it deliberately panics on
        // a false commit in debug/test builds because the bridge committing its OWN
        // fresh lease is an invariant — so the false-commit path is exercised at the
        // `cell.commit` boundary the wrapper delegates to. Were the durable frontier
        // recorded here, its END (= range.1) would sit AHEAD of the un-advanced
        // `confirmed_end_offset`, the exact M4 violation the bool gate prevents.)
        #[tokio::test(start_paused = true)]
        async fn reclaimed_lease_commit_is_false_and_never_advances_offset() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            assert_eq!(shared.committed_relay_offset(ch), 0);

            // The bridge acquires a real lease for (0, 64) on the channel's cell.
            let _lease = match BridgeDeliveryLease::acquire(&shared, ch, turn(90), 0, Some(64)) {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("expected Held on a fresh cell"),
            };

            // The deadline passes and a reconciler reclaims the (presumed-dead)
            // holder's lease → the cell flips back to Unleased.
            let cell = shared.delivery_lease(ch);
            assert!(
                cell.reclaim_if_expired(
                    lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS + 1),
                ),
                "an expired lease must be reclaimable"
            );

            // A REPLACEMENT holder (a fresh watcher) takes the reclaimed range.
            let watcher = LeaseHolder::Watcher { instance_id: 91 };
            assert!(cell.try_acquire(
                turn(90),
                watcher,
                0,
                64,
                lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS),
            ));

            // The original bridge's commit (same identity `commit_and_advance`
            // delegates to) now hits a DIFFERENT holder → identity mismatch →
            // `false`. This is the commit==false branch the mod.rs bool gate guards.
            let committed = cell.commit(
                LeaseHolder::Bridge,
                turn(90),
                0,
                64,
                LeaseOutcome::Delivered,
            );
            assert!(
                !committed,
                "a reclaimed-then-reacquired cell rejects the original holder's commit"
            );
            // Because the commit failed, `confirmed_end_offset` did NOT advance —
            // so recording `delivered_frontier.range = (0, 64)` would put the
            // durable END (64) ahead of the in-memory authority (0): M4 violation.
            // The mod.rs `if committed { record_long_chunk_terminal_delivery(..) }`
            // gate is what keeps the durable frontier a faithful mirror.
            assert_eq!(
                shared.committed_relay_offset(ch),
                0,
                "a failed commit must never advance confirmed_end (M4 — durable frontier mirrors it)"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn empty_range_routes_to_no_range() {
            let shared = make_shared_data_for_tests();
            let ch = channel();
            // end <= start → NoRange (nothing to lease, never advances).
            assert!(matches!(
                BridgeDeliveryLease::acquire(&shared, ch, turn(50), 64, Some(64)),
                BridgeLeaseAcquire::NoRange
            ));
            // None / zero end → NoRange.
            assert!(matches!(
                BridgeDeliveryLease::acquire(&shared, ch, turn(50), 0, None),
                BridgeLeaseAcquire::NoRange
            ));
            assert!(matches!(
                BridgeDeliveryLease::acquire(&shared, ch, turn(50), 0, Some(0)),
                BridgeLeaseAcquire::NoRange
            ));
            assert_eq!(shared.committed_relay_offset(ch), 0);
            assert!(matches!(
                shared.delivery_lease(ch).read(),
                LeaseSnapshot::Unleased
            ));
        }
    }

    // #3089 A0 — characterization of the terminal-delivery
    // should-send-new-chunks predicate (design §5 A0 item 1, surface:
    // turn_bridge terminal delivery). `terminal_delivery_should_send_new_chunks
    // (can_chain_locally, body)` is one of the FOUR per-surface `len > 2000`
    // predicates the #3089 controller unifies; its gate is
    // `can_chain_locally && body.len() > DISCORD_MSG_LIMIT`. Pinned inline in
    // this `#[cfg(test)] mod tests` block => ZERO production LoC.
    mod a0_characterization_tests {
        use super::super::terminal_delivery_should_send_new_chunks as should_send;
        use crate::services::discord::DISCORD_MSG_LIMIT;

        #[test]
        fn a0_terminal_delivery_predicate_gates_on_can_chain_and_over_limit() {
            let over = "x".repeat(DISCORD_MSG_LIMIT + 1); // 2001 bytes
            let under = "x".repeat(DISCORD_MSG_LIMIT); // exactly 2000 bytes

            // Both conditions required: can_chain_locally AND len > 2000.
            assert!(
                should_send(true, &over),
                "chainable AND over-limit => send new chunks"
            );
            assert!(
                !should_send(false, &over),
                "not chainable suppresses new chunks even when over-limit"
            );
            assert!(
                !should_send(true, &under),
                "exactly at the 2000 limit is NOT over-limit (strict >)"
            );
            assert!(
                !should_send(false, &under),
                "neither condition => no new chunks"
            );
        }

        #[test]
        fn a0_terminal_delivery_predicate_boundary_is_strictly_greater_than_2000() {
            // The cliff is strict `>`: 2000 stays single, 2001 splits.
            assert!(!should_send(true, &"a".repeat(2000)));
            assert!(should_send(true, &"a".repeat(2001)));
        }
    }

    // #3089 A0 — I2 invariant (design §5 A0 item 3): the committed relay offset
    // advances ONLY when the bridge lease commits `Delivered`; `Unknown` /
    // `NotDelivered` must leave it pinned so the next turn re-delivers the
    // ambiguous range. This drives the REAL production advance path
    // (`BridgeDeliveryLease::acquire` + `commit_and_advance` against the same
    // per-channel `DeliveryLeaseCell` the watcher uses) and reads the real
    // `committed_relay_offset` — NOT a local closure restating the rule — so a
    // production mutation that advanced on a non-Delivered outcome (or stopped
    // advancing on Delivered) fails here. Zero production LoC (in `mod tests`).
    mod a0_i2_advance_characterization_tests {
        use super::super::{BridgeDeliveryLease, BridgeLeaseAcquire};
        use crate::services::discord::turn_finalizer::TurnKey;
        use crate::services::discord::{
            DeliveryLeaseKey, LeaseOutcome, make_shared_data_for_tests,
        };
        use poise::serenity_prelude::ChannelId;

        const CH: u64 = 909_777;

        fn held_lease(
            shared: &std::sync::Arc<crate::services::discord::SharedData>,
            ch: ChannelId,
            user_msg_id: u64,
        ) -> BridgeDeliveryLease {
            match BridgeDeliveryLease::acquire(
                shared,
                ch,
                DeliveryLeaseKey::from_turn_key(TurnKey::new(ch, user_msg_id, 1)),
                0,
                Some(64),
            ) {
                BridgeLeaseAcquire::Held(lease) => lease,
                _ => panic!("expected Held lease on a fresh cell"),
            }
        }

        #[tokio::test(start_paused = true)]
        async fn a0_i2_only_delivered_advances_the_committed_offset() {
            // Delivered => advance to the leased end.
            let shared = make_shared_data_for_tests();
            let ch = ChannelId::new(CH);
            assert_eq!(shared.committed_relay_offset(ch), 0);
            assert!(held_lease(&shared, ch, 1).commit_and_advance(
                &shared,
                ch,
                None,
                LeaseOutcome::Delivered,
            ));
            assert_eq!(
                shared.committed_relay_offset(ch),
                64,
                "Delivered commit advances the committed offset to the leased end (I2)"
            );

            // Unknown => no advance (ambiguous: must re-deliver).
            let shared = make_shared_data_for_tests();
            let ch = ChannelId::new(CH);
            held_lease(&shared, ch, 2).commit_and_advance(&shared, ch, None, LeaseOutcome::Unknown);
            assert_eq!(
                shared.committed_relay_offset(ch),
                0,
                "Unknown must NOT advance the offset (I2)"
            );

            // NotDelivered => no advance.
            let shared = make_shared_data_for_tests();
            let ch = ChannelId::new(CH);
            held_lease(&shared, ch, 3).commit_and_advance(
                &shared,
                ch,
                None,
                LeaseOutcome::NotDelivered,
            );
            assert_eq!(
                shared.committed_relay_offset(ch),
                0,
                "NotDelivered must NOT advance the offset (I2)"
            );
        }
    }
}

#[cfg(test)]
mod relay_state_contract_refs {
    //! #4268 — relay-state contract symbol anchors for the `turn_bridge`
    //! producer sites (compiler-checked existence). These live here rather than
    //! in a single central module because the referenced fns are `pub(super)` to
    //! `turn_bridge` and are only nameable from within the `turn_bridge` subtree.
    //! See the header on `inflight::store::relay_state_contract_refs` for the
    //! contract, the CI wiring, and why there are no `// sym:` labels.
    #[test]
    fn contract_symbols_exist() {
        use super::super::spawn_turn_bridge as _;
        use super::advance_tmux_relay_confirmed_end as _;
        let _ = super::BridgeDeliveryLease::commit_and_advance;
        use super::super::terminal_controller_cutover::deliver_long_chunks_via_controller as _;
        use super::super::terminal_controller_cutover::deliver_short_replace_via_controller as _;
        // I5 turn_delivered producer: the in-band terminal-outcome delivery path.
        use super::super::terminal_outcome_delivery::run_terminal_outcome_delivery as _;
        // I5 resume_offset producers: completion postlude + runtime-handoff loop.
        use super::super::completion_postlude::run_completion_postlude as _;
        use super::super::runtime_handoff_loop::handle_runtime_handoff_loop_message as _;
    }
}
