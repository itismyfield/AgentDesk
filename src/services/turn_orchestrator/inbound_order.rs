//! #5937 — who may take the mailbox turn slot while work is already waiting.
use std::time::{Duration, Instant};

use poise::serenity_prelude::MessageId;

use super::{
    ActiveTurnKind, ChannelMailboxState, PENDING_USER_DISPATCH_MAX_YIELDS,
    clear_pending_user_dispatch, pending_dispatch_lease_is_orphaned,
    record_valve_cleared_pending_dispatch,
};

/// How long the drain may fail to advance before an inbound claim stops waiting
/// behind queued work, measured from the last turn start or turn end. The
/// idle-queue backstop runs every 60s, so three missed rounds mean the drain is
/// wedged and holding arrivals back only piles them against the overflow cap.
pub(super) const INBOUND_ORDER_FAIL_OPEN_AFTER: Duration = Duration::from_secs(180);

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

/// The drain advanced: a `UserOrAgent` turn claimed the slot, or one ended.
pub(super) fn note_inbound_drain_progress(state: &mut ChannelMailboxState) {
    state.inbound_drain_progress_at = Some(Instant::now());
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
/// it), and any claim on a channel whose drain has not advanced for
/// `INBOUND_ORDER_FAIL_OPEN_AFTER`. That clock only runs while something is
/// ahead of the claim, so an idle channel never accrues stall.
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
        state.inbound_drain_progress_at = None;
        return false;
    }
    let stalled_since = *state
        .inbound_drain_progress_at
        .get_or_insert_with(Instant::now);
    stalled_since.elapsed() < INBOUND_ORDER_FAIL_OPEN_AFTER
}

#[cfg(test)]
mod tests {
    use super::super::{Intervention, InterventionMode};
    use super::*;
    use poise::serenity_prelude::UserId;

    fn queued(message_id: u64, created_at: Instant) -> Intervention {
        Intervention {
            author_id: UserId::new(5_937),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: 0,
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "queued".to_string(),
            mode: InterventionMode::Soft,
            created_at,
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    fn past_the_window() -> Instant {
        Instant::now()
            .checked_sub(INBOUND_ORDER_FAIL_OPEN_AFTER + Duration::from_secs(1))
            .expect("test clock must reach past the fail-open window")
    }

    fn defers(state: &mut ChannelMailboxState, claim: u64) -> bool {
        inbound_order_defers_claim(
            state,
            MessageId::new(claim),
            TurnAdmissionOrder::BehindQueue,
        )
    }

    /// A message that merely waited a long time — behind a long turn, or behind
    /// a parked capped retry — is not evidence of a wedged drain.
    #[test]
    fn an_aged_queue_item_does_not_fail_open_while_the_drain_advances() {
        let mut state = ChannelMailboxState::default();
        state
            .intervention_queue
            .push(queued(5_937_401, past_the_window()));
        note_inbound_drain_progress(&mut state);

        assert!(
            defers(&mut state, 5_937_402),
            "an arrival must still wait behind an old message when the drain is moving"
        );
    }

    /// Merging rewrites the queued head's `created_at`, so message age can be
    /// held at zero indefinitely by a user who keeps typing into a wedged
    /// channel. The stall clock is not resettable that way.
    #[test]
    fn a_stalled_drain_fails_open_even_when_every_queued_item_is_fresh() {
        let mut state = ChannelMailboxState::default();
        state
            .intervention_queue
            .push(queued(5_937_411, Instant::now()));
        state.inbound_drain_progress_at = Some(past_the_window());

        assert!(
            !defers(&mut state, 5_937_412),
            "a drain that has not advanced past the window must let arrivals through"
        );
    }

    /// The #3167 BLOCKER-2 reservation guards the dequeue→claim window, where
    /// the queue is empty; an aged queue entry must not answer for it.
    #[test]
    fn an_aged_queue_item_does_not_release_a_live_reservation() {
        let mut state = ChannelMailboxState::default();
        state
            .intervention_queue
            .push(queued(5_937_421, past_the_window()));
        state.pending_user_dispatch = Some(MessageId::new(5_937_422));
        note_inbound_drain_progress(&mut state);

        assert!(
            defers(&mut state, 5_937_423),
            "a live reservation holds the slot regardless of how old the backlog is"
        );
    }

    #[test]
    fn nothing_ahead_of_the_claim_clears_the_stall_clock() {
        let mut state = ChannelMailboxState::default();
        state.inbound_drain_progress_at = Some(past_the_window());

        assert!(!defers(&mut state, 5_937_431));
        assert!(
            state.inbound_drain_progress_at.is_none(),
            "stall must not accrue on a channel with nothing queued or reserved"
        );
    }
}
