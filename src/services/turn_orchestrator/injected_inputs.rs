//! A busy-turn injection's place in the mailbox order: reserved before the paste, settled by the
//! injection's owner.
use super::*;
use crate::services::discord::input_runtime::fence::{self, Permit};

/// iMessage refusal reasons for a handback that was never written, or whose result never came back.
pub(crate) const HANDBACK_NOT_WRITTEN: &str = "handback_persistence";
pub(crate) const HANDBACK_UNKNOWN: &str = "handback_unknown";
/// Front-handback attempts, each after its delay; the reservation holds between them.
const HANDBACK_DELAYS: [Duration; 4] = [
    Duration::ZERO,
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(4),
];

/// The owner's share; a reservation whose lease only the actor holds counts as gone.
#[derive(Debug)]
pub(crate) struct InjectionLease;

/// A live reservation. Not `Clone`: settling or abandoning consumes it.
#[derive(Debug)]
pub(crate) struct InjectionTicket {
    lease: Arc<InjectionLease>,
}

/// The mailbox claim the owner judged: token, kind and message.
pub(crate) type ExpectedClaim = Option<(Arc<CancelToken>, ActiveTurnKind, Option<MessageId>)>;

#[derive(Debug)]
pub(crate) enum ReserveOutcome {
    Reserved(InjectionTicket),
    HolderChanged,
    Backlog,
    /// Closed, fenced or unreachable: nothing was reserved.
    Unavailable,
}

pub(crate) enum InjectionSettlement {
    /// The pane took the input or may have; nothing is queued.
    Delivered,
    /// Nothing reached the pane: the input takes the queue front, the place it reserved.
    HandBack(Intervention),
}

pub(crate) enum SettleOutcome {
    Committed {
        queue_exit_events: Vec<QueueExitEvent>,
    },
    /// Nothing was written; the reservation stands and its ticket comes back.
    NotCommitted {
        ticket: InjectionTicket,
        error: String,
    },
    /// Sent, but no result came back.
    Unknown,
}

/// Whether a live reservation holds the mailbox order; an orphaned one is dropped here.
pub(super) fn holds_order(state: &mut ChannelMailboxState) -> bool {
    let orphaned =
        |(_, lease): &(Option<MessageId>, Arc<InjectionLease>)| Arc::strong_count(lease) == 1;
    if state.injection_reserved.as_ref().is_some_and(orphaned) {
        state.injection_reserved = None;
    }
    state.injection_reserved.is_some()
}

/// `TakeNextSoft`'s answer while a reservation holds the order: no head leaves the queue.
pub(super) fn head_withheld(state: &ChannelMailboxState) -> TakeNextSoftResult {
    let queue = &state.intervention_queue;
    TakeNextSoftResult {
        intervention: None,
        dispatch_lease: None,
        has_more: queue.iter().any(|item| item.mode == InterventionMode::Soft),
        queue_len_after: queue.len(),
        queue_exit_events: Vec::new(),
        persistence_error: None,
    }
}

pub(super) fn reserve(
    state: &mut ChannelMailboxState,
    message_id: Option<MessageId>,
    expected: &ExpectedClaim,
) -> ReserveOutcome {
    let claim = state.cancel_token.as_ref();
    let same_claim = match (claim, expected) {
        (None, None) => true,
        (Some(token), Some((expected, kind, message))) => {
            Arc::ptr_eq(token, expected)
                && state.active_turn_kind == *kind
                && state.active_user_message_id == *message
        }
        _ => false,
    };
    if !same_claim {
        return ReserveOutcome::HolderChanged;
    }
    if holds_order(state)
        || !state.intervention_queue.is_empty()
        || state.pending_user_dispatch.is_some()
    {
        return ReserveOutcome::Backlog;
    }
    let lease = Arc::new(InjectionLease);
    state.injection_reserved = Some((message_id, lease.clone()));
    ReserveOutcome::Reserved(InjectionTicket { lease })
}

fn release(state: &mut ChannelMailboxState, ticket: &InjectionTicket) {
    let own =
        |(_, lease): &(Option<MessageId>, Arc<InjectionLease>)| Arc::ptr_eq(lease, &ticket.lease);
    if state.injection_reserved.as_ref().is_some_and(own) {
        state.injection_reserved = None;
    }
}

/// The reservation ends only with a written handback; a write that did not land keeps it.
pub(super) fn settle(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    ticket: InjectionTicket,
    settlement: InjectionSettlement,
    persistence: &QueuePersistenceContext,
) -> SettleOutcome {
    state.last_persistence = Some(persistence.clone());
    let InjectionSettlement::HandBack(intervention) = settlement else {
        release(state, &ticket);
        let queue_exit_events = Vec::new();
        return SettleOutcome::Committed { queue_exit_events };
    };
    if let Some(error) = absorb_disk_queue_error(state, channel_id, persistence) {
        return SettleOutcome::NotCommitted { ticket, error };
    }
    let previous_queue = state.intervention_queue.clone();
    let pending = state.pending_user_dispatch;
    let active = state.active_user_message_id;
    let queue = &mut state.intervention_queue;
    let front = requeue_intervention_front(queue, intervention, pending, active, None);
    if !front.enqueued {
        state.intervention_queue = previous_queue;
        let error = format!("front handback refused: {:?}", front.refusal_reason);
        return SettleOutcome::NotCommitted { ticket, error };
    }
    let operation = "injection_handback";
    if let Err(error) =
        persist_queue_or_restore(state, channel_id, persistence, previous_queue, operation)
    {
        return SettleOutcome::NotCommitted { ticket, error };
    }
    release(state, &ticket);
    state.inbound_stall_since = None;
    let queue_exit_events = front.queue_exit_events;
    SettleOutcome::Committed { queue_exit_events }
}

pub(super) fn abandon(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    ticket: InjectionTicket,
) {
    release(state, &ticket);
    let channel = channel_id.get();
    tracing::warn!(
        channel,
        "injection handback abandoned; its order reservation ends"
    );
    #[cfg(test)]
    super::test_support::note_injection_abandon(channel_id);
}

impl ChannelMailboxHandle {
    /// Reserves the mailbox order for one injection when the claim is still `expected_claim` and
    /// nothing waits ahead; later input queues behind it until the ticket settles or drops.
    pub(crate) async fn reserve_injection(
        &self,
        message_id: Option<MessageId>,
        expected_claim: ExpectedClaim,
        input_permit: Option<Permit>,
    ) -> ReserveOutcome {
        self.request(|reply| ChannelMailboxMsg::ReserveInjection {
            input_permit,
            message_id,
            expected_claim,
            reply,
        })
        .await
        .unwrap_or(ReserveOutcome::Unavailable)
    }

    /// One settle attempt. A message the actor never received returns its ticket uncommitted.
    pub(crate) async fn settle_injected_input(
        &self,
        ticket: InjectionTicket,
        settlement: InjectionSettlement,
        persistence: QueuePersistenceContext,
        input_permit: Option<Permit>,
    ) -> SettleOutcome {
        let (reply, answer) = oneshot::channel();
        let msg = ChannelMailboxMsg::SettleInjectedInput {
            input_permit,
            ticket,
            settlement,
            persistence,
            reply,
        };
        match self.sender.send((msg, fence::effect::current())) {
            Ok(()) => answer.await.unwrap_or(SettleOutcome::Unknown),
            Err(unsent) => match unsent.0.0 {
                ChannelMailboxMsg::SettleInjectedInput { ticket, .. } => {
                    let error = "mailbox_unreachable".to_string();
                    SettleOutcome::NotCommitted { ticket, error }
                }
                _ => unreachable!("the unsent message is the settle just built"),
            },
        }
    }

    /// Writes `intervention` to the queue front with the same reservation, retrying only writes
    /// known not to have landed; when every attempt fails the reservation is abandoned.
    pub(crate) async fn hand_back_injected_input(
        &self,
        mut ticket: InjectionTicket,
        intervention: Intervention,
        persistence: QueuePersistenceContext,
        input_permit: Option<Permit>,
    ) -> Result<Vec<QueueExitEvent>, &'static str> {
        for delay in HANDBACK_DELAYS {
            tokio::time::sleep(delay).await;
            let settlement = InjectionSettlement::HandBack(intervention.clone());
            let permit = input_permit.clone();
            match self
                .settle_injected_input(ticket, settlement, persistence.clone(), permit)
                .await
            {
                SettleOutcome::Committed { queue_exit_events } => return Ok(queue_exit_events),
                SettleOutcome::NotCommitted {
                    ticket: kept,
                    error,
                } => {
                    tracing::warn!(%error, "injection handback not written; reservation kept");
                    ticket = kept;
                }
                SettleOutcome::Unknown => return Err(HANDBACK_UNKNOWN),
            }
        }
        let _ = self
            .request(|reply| ChannelMailboxMsg::AbandonInjection {
                input_permit,
                ticket,
                reply,
            })
            .await;
        Err(HANDBACK_NOT_WRITTEN)
    }
}

#[cfg(test)]
#[path = "injected_inputs_tests.rs"]
mod tests;
