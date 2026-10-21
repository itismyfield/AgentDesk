//! Missing-watcher delivery reclaim and its tick disposition.

use super::runtime::{TickContext, TickRuntime};
use super::*;
use crate::services::discord::turn_bridge::retry_state::ReclaimDeliveryOutcome;
use std::ops::ControlFlow;

struct ReclaimDeliveryState<'a> {
    full_response: &'a str,
    bridge_confirmed_response_sent_offset: usize,
    response_sent_offset: &'a mut usize,
    inflight_state: &'a mut InflightTurnState,
    watcher_owns_assistant_relay: &'a mut bool,
    watcher_relay_available_for_turn: &'a mut bool,
    standby_relay_owns_output: bool,
}

pub(super) fn reclaim_missing_watcher_delivery(
    ctx: &TickContext<'_>,
    tick: &mut TickRuntime<'_>,
) -> ControlFlow<(), bool> {
    // The reclaim rewind captures its nonce from this local, so it must still be the actor's.
    if !tick.inflight_state.owned_by_turn_nonce(ctx.captured_nonce) {
        return ControlFlow::Break(());
    }
    let live_watcher = live_watcher_registered_for_relay(
        ctx.shared_owned.as_ref(),
        *tick.watcher_owner_channel_id,
    );
    reclaim_for_missing_watcher(
        ctx.channel_id,
        live_watcher,
        ReclaimDeliveryState {
            full_response: tick.full_response,
            bridge_confirmed_response_sent_offset: *tick.bridge_confirmed_response_sent_offset,
            response_sent_offset: tick.response_sent_offset,
            inflight_state: tick.inflight_state,
            watcher_owns_assistant_relay: tick.watcher_owns_assistant_relay,
            watcher_relay_available_for_turn: tick.watcher_relay_available_for_turn,
            standby_relay_owns_output: *tick.standby_relay_owns_output,
        },
    )
}

fn reclaim_for_missing_watcher(
    channel_id: ChannelId,
    live_watcher_registered: bool,
    state: ReclaimDeliveryState<'_>,
) -> ControlFlow<(), bool> {
    if !bridge_should_reclaim_relay_from_missing_watcher(
        *state.watcher_owns_assistant_relay,
        state.standby_relay_owns_output,
        live_watcher_registered,
    ) {
        return ControlFlow::Continue(false);
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ turn_bridge reclaiming assistant relay for channel {} after watcher disappeared",
        channel_id.get()
    );
    let outcome = rewind_and_persist_delivery_on_reclaim(
        state.full_response,
        state.bridge_confirmed_response_sent_offset,
        state.response_sent_offset,
        state.inflight_state,
        channel_id,
    );
    if outcome == ReclaimDeliveryOutcome::Rejected {
        // A rejected reclaim cannot gain bridge authority through this tick's later flush.
        return ControlFlow::Break(());
    }
    *state.watcher_owns_assistant_relay = false;
    *state.watcher_relay_available_for_turn = false;
    ControlFlow::Continue(true)
}

#[cfg(test)]
#[path = "reclaim/tests.rs"]
mod tests;
