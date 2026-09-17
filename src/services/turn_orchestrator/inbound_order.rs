//! #5937 — who may take the mailbox turn slot while work is already waiting.
use std::time::Duration;

use poise::serenity_prelude::MessageId;

use super::{
    ActiveTurnKind, ChannelMailboxState, PENDING_USER_DISPATCH_MAX_YIELDS,
    clear_pending_user_dispatch, pending_dispatch_lease_is_orphaned,
    record_valve_cleared_pending_dispatch,
};

/// How long a queued backlog may sit before an inbound claim stops waiting
/// behind it. The idle-queue backstop drains every 60s, so a channel whose
/// drain still works never reaches this; past it the drain is wedged and
/// holding arrivals back would only pile them against the overflow cap.
pub(super) const INBOUND_ORDER_FAIL_OPEN_AFTER: Duration = Duration::from_secs(180);

/// Whether a turn claim may take an idle slot ahead of queued work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TurnAdmissionOrder {
    /// Recovery, reaper and healing claims: not inbound traffic, never held.
    #[default]
    Immediate,
    /// Discord text intake: refused while older inbound work is still queued.
    BehindQueue,
}

/// True when work that arrived earlier still owns the slot this claim wants.
pub(super) fn claim_yields(
    state: &mut ChannelMailboxState,
    turn_kind: ActiveTurnKind,
    user_message_id: MessageId,
    admission_order: TurnAdmissionOrder,
) -> bool {
    let background = background_defers_claim(state, turn_kind);
    let order = inbound_order_defers_claim(state, user_message_id, admission_order);
    background || order
}

/// #3167 BLOCKER-2 — a background cycle that wins the freed slot ahead of the
/// deferred kickoff starves the queued user turn, so `Background` yields to a
/// queued backlog and to the reservation `TakeNextSoft` holds across the
/// dequeue→claim window, where the queue is empty. A `false` return is the
/// background callers' normal lost-race path. Only that reservation-only
/// window can deadlock, if the dequeued user turn is lost, so after
/// `PENDING_USER_DISPATCH_MAX_YIELDS` such refusals the stale reservation is
/// force-cleared; queue-backed refusals are real backlog and never counted.
fn background_defers_claim(state: &mut ChannelMailboxState, turn_kind: ActiveTurnKind) -> bool {
    let queue_non_empty = !state.intervention_queue.is_empty();
    let reservation_held = state.pending_user_dispatch.is_some();
    let yields = turn_kind.is_background() && (queue_non_empty || reservation_held);
    if yields && !queue_non_empty && reservation_held {
        state.pending_user_dispatch_yield_count += 1;
        if state.pending_user_dispatch_yield_count >= PENDING_USER_DISPATCH_MAX_YIELDS {
            if pending_dispatch_lease_is_orphaned(state)
                && let Some(cleared_id) = clear_pending_user_dispatch(state)
            {
                record_valve_cleared_pending_dispatch(state, cleared_id);
            }
        }
    }
    yields
}

/// #5937 — true when this claim would jump ahead of inbound work sent earlier:
/// a queued backlog, or a head `TakeNextSoft` handed out that has not claimed
/// the slot yet. Three claims are not overtakes — the dequeued head itself (it
/// is the drain), a queued copy of the claiming message (the start path purges
/// it), and a backlog wedged far past the 60s idle-queue backstop.
fn inbound_order_defers_claim(
    state: &ChannelMailboxState,
    user_message_id: MessageId,
    admission_order: TurnAdmissionOrder,
) -> bool {
    if admission_order != TurnAdmissionOrder::BehindQueue {
        return false;
    }
    if state.pending_user_dispatch == Some(user_message_id)
        || state
            .pending_user_dispatch_source_ids
            .contains(&user_message_id)
    {
        return false;
    }
    let mut foreign_backlog = false;
    for item in &state.intervention_queue {
        if item.message_id == user_message_id
            && item
                .source_message_ids
                .iter()
                .all(|id| *id == user_message_id)
        {
            continue;
        }
        if item.created_at.elapsed() >= INBOUND_ORDER_FAIL_OPEN_AFTER {
            return false;
        }
        foreign_backlog = true;
    }
    foreign_backlog
        || (state.pending_user_dispatch.is_some() && !pending_dispatch_lease_is_orphaned(state))
}
