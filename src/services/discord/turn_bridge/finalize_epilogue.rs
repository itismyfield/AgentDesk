//! #3038 (giant-file decompose, registry deadline 2026-08-31): the turn-bridge
//! finalization epilogue — the finalizing-turns counter decrement plus the
//! `has_queued_turns` queue-drain block — moved verbatim out of the tail of
//! `spawn_turn_bridge`'s async body. Behavior-preserving: this is the LAST block
//! of that async body (no borrow-back), so every captured local is threaded in
//! by value with the exact ownership the inline block used (`shared_owned`,
//! `gateway`, `provider`, `request_owner_name` moved; the `Copy` ids/flags
//! passed by value). The only textual change from the original block is the three
//! discord-level `super::` refs deepened to `super::super::` from the child
//! (same seam-fix as `response_delivery.rs`); all other deps reach via
//! `use super::*;`. #3016 single-authority finalizer ledger is untouched — this
//! epilogue runs strictly AFTER the commit and never writes the ledger.

use super::*;

/// Finalization epilogue: decrement the finalizing-turns counters (symmetric
/// with the `fetch_add` at turn start) and, if this turn had queued follow-ups,
/// drain exactly one next turn under the same guards/order as before
/// (`preserve_inflight_for_cleanup_retry` → restart_pending → live-routing
/// validation → dispatch, with the deferred-idle-kickoff fallback when the live
/// Discord context is missing). The queued-turn mailbox side-effects preserve
/// their original order.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_and_drain_queued_turns(
    shared_owned: Arc<SharedData>,
    has_queued_turns: bool,
    preserve_inflight_for_cleanup_retry: bool,
    gateway: Arc<dyn TurnGateway>,
    channel_id: ChannelId,
    provider: ProviderKind,
    request_owner_name: String,
    tmux_last_offset: Option<u64>,
    watcher_owner_channel_id: ChannelId,
) {
    // Finalization complete — decrement counters
    shared_owned
        .restart
        .finalizing_turns
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    shared_owned
        .restart
        .global_finalizing
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    // Note: deferred restart exit is handled by the 5-second poll loop in mod.rs,
    // which saves pending queues before calling check_deferred_restart.
    // Calling it here would risk exiting before other providers save their queues.

    if has_queued_turns {
        // Drain mode: if restart is pending, don't start new turns from queue.
        // The queued messages will be saved to disk and processed after restart.
        if preserve_inflight_for_cleanup_retry {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ QUEUE-GUARD: preserving queued command(s) for channel {} until placeholder cleanup retry commits",
                channel_id
            );
        } else if shared_owned
            .restart
            .restart_pending
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏸ DRAIN: skipping queued turn dequeue for channel {} (restart pending)",
                channel_id
            );
        } else if let Some(bot_owner_provider) = gateway.bot_owner_provider() {
            if let Err(reason) = gateway.validate_live_routing(channel_id).await {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⚠ QUEUE-GUARD: preserving queued command(s) for channel {} (reason={})",
                    channel_id,
                    reason
                );
            } else {
                let next_intervention = super::super::mailbox_take_next_soft_intervention(
                    &shared_owned,
                    &bot_owner_provider,
                    channel_id,
                )
                .await;

                if let Some(error) = next_intervention.persistence_error.as_ref() {
                    tracing::error!(
                        provider = bot_owner_provider.as_str(),
                        channel_id = channel_id.get(),
                        error = %error,
                        "QUEUE-GUARD: preserving queued command after pending-queue persistence failure"
                    );
                } else if let Some((intervention, has_more_queued_turns)) =
                    next_intervention.into_intervention()
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}] 📋 Processing next queued command");
                    if let Err(e) = gateway
                        .dispatch_queued_turn(
                            channel_id,
                            &intervention,
                            &request_owner_name,
                            has_more_queued_turns,
                        )
                        .await
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!("  [{ts}]   ⚠ queued command failed: {e}");
                        super::super::mailbox_requeue_intervention_front(
                            &shared_owned,
                            &bot_owner_provider,
                            channel_id,
                            intervention,
                        )
                        .await;
                    }
                }
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📦 preserving queued command(s): missing live Discord context — scheduling deferred drain"
            );
            if let Some(offset) = tmux_last_offset
                && let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
            {
                if let Ok(mut guard) = watcher.resume_offset.lock() {
                    *guard = Some(offset);
                }
                watcher.paused.store(false, Ordering::Relaxed);
            }
            super::super::schedule_deferred_idle_queue_kickoff(
                shared_owned.clone(),
                provider.clone(),
                channel_id,
                "turn bridge queued backlog",
            );
        }
    }
}
