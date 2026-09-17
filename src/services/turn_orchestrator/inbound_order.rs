//! #5937 — who may take the mailbox turn slot while work is already waiting.
use std::time::{Duration, Instant};

use poise::serenity_prelude::MessageId;

use super::{
    ActiveTurnKind, ChannelMailboxState, PENDING_USER_DISPATCH_MAX_YIELDS,
    clear_pending_user_dispatch, pending_dispatch_lease_is_orphaned,
    record_valve_cleared_pending_dispatch,
};

/// How long the drain may fail to advance, counting only time the slot stood
/// free for it to use, before an inbound claim stops waiting behind queued
/// work. The idle-queue backstop runs every 60s, so three missed rounds mean
/// the drain is wedged and holding arrivals back only feeds the overflow cap.
pub(super) const INBOUND_ORDER_FAIL_OPEN_AFTER: Duration = Duration::from_secs(3 * 60);

/// Whether a turn claim may take an idle slot ahead of queued work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TurnAdmissionOrder {
    /// Recovery, reaper and healing claims, plus the headless entry point that
    /// still carries API/routine/voice intake with them (#5983). Never held.
    #[default]
    Immediate,
    /// Discord text intake: refused while older inbound work is still queued.
    BehindQueue,
}

/// A turn owned the slot from `turn_started` until now. A busy slot blocks the
/// drain for an ordinary reason, so the stall clock skips that window — and
/// skipping is not resetting: a turn that drained nothing buys no fresh window.
pub(super) fn pause_inbound_stall_for_turn(
    state: &mut ChannelMailboxState,
    turn_started: Option<Instant>,
) {
    if let (Some(stalled_since), Some(held_since)) = (state.inbound_stall_since, turn_started) {
        state.inbound_stall_since = Some(stalled_since + held_since.max(stalled_since).elapsed());
    }
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
/// dequeue→claim window. Only that reservation-only window can deadlock, if
/// the dequeued user turn is lost, so after `PENDING_USER_DISPATCH_MAX_YIELDS`
/// such refusals the stale reservation is force-cleared; backlog refusals are
/// a normal lost race and are never counted.
fn background_defers_claim(state: &mut ChannelMailboxState, turn_kind: ActiveTurnKind) -> bool {
    let queue_non_empty = !state.intervention_queue.is_empty();
    let reservation_held = state.pending_user_dispatch.is_some();
    let yields = turn_kind.is_background() && (queue_non_empty || reservation_held);
    if yields && !queue_non_empty && reservation_held {
        state.pending_user_dispatch_yield_count += 1;
        if state.pending_user_dispatch_yield_count >= PENDING_USER_DISPATCH_MAX_YIELDS
            && pending_dispatch_lease_is_orphaned(state)
        {
            retire_stalled_pending_dispatch(state);
        }
    }
    yields
}

/// Give up a reservation whose holder is not coming back. Leaving it standing
/// no-ops every `TakeNextSoft`, so the queue behind it never moves again.
fn retire_stalled_pending_dispatch(state: &mut ChannelMailboxState) {
    if let Some(cleared_id) = clear_pending_user_dispatch(state) {
        record_valve_cleared_pending_dispatch(state, cleared_id);
    }
}

/// #5937 — true when this claim would jump ahead of inbound work sent earlier: a queued
/// backlog, or a head `TakeNextSoft` handed out that has not claimed the slot yet. Not
/// overtakes: the dequeued head itself (it IS the drain, so it clears the stall), a queued copy
/// of the claiming message, and any claim on a channel stalled for
/// `INBOUND_ORDER_FAIL_OPEN_AFTER`. A whole window of free-slot stall also proves a
/// reservation's holder is gone, so the fail-open retires it — else the drain stays wedged and
/// one arrival per window is all the channel ever answers.
fn inbound_order_defers_claim(
    state: &mut ChannelMailboxState,
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
        state.inbound_stall_since = None;
        return false;
    }
    let foreign_backlog = state.intervention_queue.iter().any(|item| {
        item.message_id != user_message_id
            || item
                .source_message_ids
                .iter()
                .any(|id| *id != user_message_id)
    });
    let reserved =
        state.pending_user_dispatch.is_some() && !pending_dispatch_lease_is_orphaned(state);
    if !foreign_backlog && !reserved {
        state.inbound_stall_since = None;
        return false;
    }
    let stalled_since = *state.inbound_stall_since.get_or_insert_with(Instant::now);
    if stalled_since.elapsed() < INBOUND_ORDER_FAIL_OPEN_AFTER {
        return true;
    }
    if reserved {
        retire_stalled_pending_dispatch(state);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::super::actor_hydrate_regression_tests::make_intervention;
    use super::*;

    fn past_the_window() -> Instant {
        Instant::now()
            .checked_sub(INBOUND_ORDER_FAIL_OPEN_AFTER + Duration::from_secs(1))
            .expect("test clock reaches past the fail-open window")
    }

    fn with_queued(message_id: u64, created_at: Instant) -> ChannelMailboxState {
        let mut state = ChannelMailboxState::default();
        let queued = make_intervention(message_id, "queued", created_at);
        state.intervention_queue.push(queued);
        state
    }

    fn defers(state: &mut ChannelMailboxState, claim: u64) -> bool {
        inbound_order_defers_claim(
            state,
            MessageId::new(claim),
            TurnAdmissionOrder::BehindQueue,
        )
    }

    /// Winding a clock back by the constant holds for any value of it, so the
    /// window is pinned here: three missed 60s idle-queue backstop rounds.
    #[test]
    fn the_fail_open_window_spans_three_idle_queue_backstop_rounds() {
        assert_eq!(INBOUND_ORDER_FAIL_OPEN_AFTER, Duration::from_secs(180));
    }

    /// Waiting a long time behind a long turn is not evidence of a wedge.
    #[test]
    fn an_aged_queue_item_does_not_fail_open_while_the_drain_advances() {
        let mut state = with_queued(5_937_401, past_the_window());

        assert!(defers(&mut state, 5_937_402), "old work still leads");
    }

    /// Merging rewrites the queued head's `created_at`, so a user typing into a
    /// wedged channel could hold message age at zero forever. Stall cannot be.
    #[test]
    fn a_stalled_drain_fails_open_even_when_every_queued_item_is_fresh() {
        let mut state = with_queued(5_937_411, Instant::now());
        state.inbound_stall_since = Some(past_the_window());

        assert!(!defers(&mut state, 5_937_412), "a wedged drain yields");
    }

    /// The #3167 BLOCKER-2 reservation guards a window in which the queue is
    /// empty, so an aged queue entry must not answer for it.
    #[test]
    fn an_aged_queue_item_does_not_release_a_live_reservation() {
        let mut state = with_queued(5_937_421, past_the_window());
        state.pending_user_dispatch = Some(MessageId::new(5_937_422));

        assert!(defers(&mut state, 5_937_423), "the reservation holds");
    }

    #[test]
    fn nothing_ahead_of_the_claim_clears_the_stall_clock() {
        let mut state = ChannelMailboxState::default();
        state.inbound_stall_since = Some(past_the_window());

        assert!(!defers(&mut state, 5_937_431));
        assert!(state.inbound_stall_since.is_none(), "no stall when idle");
    }

    /// Healing, reaper and routine turns claim `Immediate` and drain nothing.
    /// Counting their cycle as progress let a channel refresh forever.
    #[test]
    fn an_immediate_turn_cycle_never_refreshes_the_stall_clock() {
        let mut state = with_queued(5_937_441, Instant::now());
        state.inbound_stall_since = Some(past_the_window());
        let claim = MessageId::new(5_937_442);

        inbound_order_defers_claim(&mut state, claim, TurnAdmissionOrder::Immediate);
        pause_inbound_stall_for_turn(&mut state, Some(Instant::now()));

        assert!(!defers(&mut state, 5_937_443), "immediate is not progress");
    }

    /// The claim a fail-open let through drained nothing either, so it must not
    /// re-arm the clock; retiring the reservation is what unwedges the drain.
    #[test]
    fn a_fail_open_claim_retires_the_wedge_without_re_arming_the_clock() {
        let mut state = with_queued(5_937_451, Instant::now());
        state.pending_user_dispatch = Some(MessageId::new(5_937_452));
        state.inbound_stall_since = Some(past_the_window());

        assert!(!defers(&mut state, 5_937_453), "the wedged drain yields");
        assert_eq!(state.pending_user_dispatch, None, "the wedge is retired");
        pause_inbound_stall_for_turn(&mut state, Some(Instant::now()));

        assert!(!defers(&mut state, 5_937_454), "and stays open after");
    }
}
