use super::super::formatting::ReplaceLongMessageOutcome;
use super::*;

fn record_turn_bridge_terminal_replace_cleanup(
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
    shared.placeholder_cleanup.record(
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

fn replace_outcome_commits_terminal_delivery(outcome: &ReplaceLongMessageOutcome) -> bool {
    matches!(outcome, ReplaceLongMessageOutcome::EditedOriginal)
}

pub(super) fn terminal_delivery_should_send_new_chunks(
    can_chain_locally: bool,
    formatted_response: &str,
) -> bool {
    can_chain_locally && formatted_response.len() > super::super::DISCORD_MSG_LIMIT
}

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
) -> Result<MessageId, String> {
    let (first_msg_id, delete_result) =
        send_ordered_long_terminal_chunks(gateway, channel_id, placeholder_msg_id, response)
            .await?;
    let cleanup_outcome = match delete_result {
        Ok(()) => super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
        Err(error) => super::super::placeholder_cleanup::classify_delete_error(&error),
    };
    shared.placeholder_cleanup.record(
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
    Ok(first_msg_id)
}

async fn send_ordered_long_terminal_chunks(
    gateway: &dyn TurnGateway,
    channel_id: ChannelId,
    placeholder_msg_id: MessageId,
    response: &str,
) -> Result<(MessageId, Result<(), String>), String> {
    let message_ids = gateway
        .send_long_message_with_rollback(channel_id, placeholder_msg_id, response)
        .await?;
    let first_msg_id = message_ids
        .first()
        .copied()
        .ok_or_else(|| "long terminal response produced no Discord chunks".to_string())?;
    let delete_result = gateway.delete_message(channel_id, placeholder_msg_id).await;
    Ok((first_msg_id, delete_result))
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
        Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { edit_error }) => {
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
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(format!(
                    "partial continuation failure: sent_chunks={sent_chunks}, total_chunks={total_chunks}, failed_chunk_index={failed_chunk_index}, cleaned_continuations={}, cleanup_errors={}, error={error}",
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

pub(super) fn tui_quiescence_timeout_requires_inflight_retry(
    terminal_delivery_committed: bool,
) -> bool {
    !terminal_delivery_committed
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
///      `try_acquire(turn, Bridge, [start,end), now+deadline)`. On success spawns
///      a [`crate::services::discord::DeliveryLeaseHeartbeat`] so a long chunked
///      send (which can exceed the 15s deadline) is never reclaimed mid-flight.
///      On FAILURE the cell is held by the watcher (or another bridge path) for
///      this range/turn → the caller MUST take a B2-style skip (NOT deliver+
///      advance); the live holder owns delivery.
///   2. caller performs `replace_message_with_outcome` / chunked send.
///   3. [`Self::commit_and_advance`] — stop the heartbeat, `commit(Bridge, turn,
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
    turn: super::super::turn_finalizer::TurnKey,
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

impl BridgeDeliveryLease {
    /// Acquire the per-channel delivery lease for the bridge's terminal delivery
    /// covering `[start, end)` for `turn`. `target_end` is the same end offset the
    /// pre-P1-2 `advance_tmux_relay_confirmed_end` advanced to (the bridge's
    /// `tmux_last_offset`); `start` is the turn's start offset (`turn_start_offset`,
    /// falling back to the same end so an unknown start yields an empty range that
    /// routes to [`BridgeLeaseAcquire::NoRange`]).
    pub(super) fn acquire(
        shared: &SharedData,
        channel_id: ChannelId,
        turn: super::super::turn_finalizer::TurnKey,
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
            turn,
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
            turn,
        ));
        BridgeLeaseAcquire::Held(BridgeDeliveryLease {
            cell,
            holder,
            turn,
            start,
            end,
            heartbeat,
        })
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
        let committed = self
            .cell
            .commit(self.holder, self.turn, self.start, self.end, outcome);
        debug_assert!(
            committed,
            "bridge must be able to commit its own freshly-acquired delivery lease"
        );
        if committed && outcome == crate::services::discord::LeaseOutcome::Delivered {
            // B6: the ONLY confirmed_end advance on the bridge terminal path now
            // flows through this successful lease commit.
            super::advance_tmux_relay_confirmed_end(
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
            .release(self.holder, self.turn, self.start, self.end);
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
            .release(self.holder, self.turn, self.start, self.end);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        replace_outcome_commits_terminal_delivery, send_ordered_long_terminal_chunks,
        should_complete_work_dispatch_after_terminal_delivery,
        should_fail_dispatch_after_terminal_delivery, terminal_delivery_should_send_new_chunks,
        tui_quiescence_timeout_requires_inflight_retry,
    };
    use crate::services::discord::formatting;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
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

        fn add_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn remove_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
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

    #[test]
    fn tui_quiescence_timeout_preserves_inflight_only_before_terminal_delivery() {
        assert!(tui_quiescence_timeout_requires_inflight_retry(false));
        assert!(
            !tui_quiescence_timeout_requires_inflight_retry(true),
            "after Discord terminal delivery commits, timeout may suppress visible completion but must not preserve stale inflight ownership"
        );
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

        let (first_msg_id, delete_result) = send_ordered_long_terminal_chunks(
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
            DELIVERY_LEASE_DEADLINE_MS, LeaseHolder, LeaseOutcome, LeaseSnapshot, lease_now_ms,
            make_shared_data_for_tests,
        };
        use poise::serenity_prelude::ChannelId;

        use super::super::{BridgeDeliveryLease, BridgeLeaseAcquire};

        const CH: u64 = 909_001;

        fn channel() -> ChannelId {
            ChannelId::new(CH)
        }

        fn turn(user_msg_id: u64) -> TurnKey {
            TurnKey::new(channel(), user_msg_id, 1)
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
}
