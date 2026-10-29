//! A busy-turn injection's place in the mailbox order: reserved before the paste, settled by the
//! injection's owner.
use super::*;
use crate::services::discord::inject_disposition::{self as disposition, InjectionOutcome};

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
#[derive(Debug, Default)]
pub(crate) struct InjectionLease(std::sync::atomic::AtomicBool);

/// A live reservation. Not `Clone`: settling or abandoning consumes it.
#[derive(Debug)]
pub(crate) struct InjectionTicket {
    lease: Arc<InjectionLease>,
    message: Option<MessageId>,
}

/// The mailbox claim the owner judged: token, kind and message.
pub(crate) type ExpectedClaim = Option<(Arc<CancelToken>, ActiveTurnKind, Option<MessageId>)>;

#[derive(Debug)]
pub(crate) enum ReserveOutcome {
    Reserved(InjectionTicket),
    /// The message is already queued, reserved for dispatch, or held by the active turn.
    Owned,
    /// The message was already injected or explicitly discarded.
    Consumed,
    HolderChanged,
    Backlog,
    /// Closed or unreachable: nothing was reserved.
    Unavailable,
}

pub(crate) enum InjectionSettlement {
    /// The pane took the input or may have; nothing is queued.
    Delivered(InjectionOutcome),
    /// Nothing reached the pane: the input takes the queue front, the place it reserved.
    HandBack(Box<Intervention>),
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

/// One injection's actor messages.
pub(crate) enum InjectionMsg {
    Reserve {
        message_id: Option<MessageId>,
        expected_claim: ExpectedClaim,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<ReserveOutcome>,
    },
    Settle {
        ticket: InjectionTicket,
        settlement: InjectionSettlement,
        persistence: QueuePersistenceContext,
        reply: oneshot::Sender<SettleOutcome>,
    },
    Abandon {
        ticket: InjectionTicket,
        reply: oneshot::Sender<()>,
    },
    /// A queued head's claim or settlement; see `queued_head.rs`.
    Head(super::queued_head::HeadMsg),
    /// A held boot restore's settle probe; see `dispatch_reservation/held_settle.rs`.
    Held(super::dispatch_reservation::held_settle::HeldSettleMsg),
}

impl InjectionMsg {
    /// Answers without the actor: nothing is reserved, a settle gets its ticket back uncommitted,
    /// and an abandon's ticket drops here, orphaning its reservation. Returns the arm name.
    pub(super) fn refuse(self, error: String) -> &'static str {
        match self {
            Self::Reserve { reply, .. } => {
                let _ = reply.send(ReserveOutcome::Unavailable);
                "ReserveInjection"
            }
            Self::Settle { ticket, reply, .. } => {
                let _ = reply.send(SettleOutcome::NotCommitted { ticket, error });
                "SettleInjectedInput"
            }
            Self::Abandon { .. } => "AbandonInjection",
            Self::Head(msg) => {
                msg.refuse(error);
                "QueuedHead"
            }
            Self::Held(msg) => {
                msg.refuse(error);
                "HeldSettle"
            }
        }
    }
}

pub(super) fn step(state: &mut ChannelMailboxState, channel_id: ChannelId, msg: InjectionMsg) {
    match msg {
        InjectionMsg::Reserve {
            message_id,
            expected_claim,
            persistence,
            reply,
            ..
        } => {
            let reserved = reserve(state, channel_id, message_id, &expected_claim, &persistence);
            let _ = reply.send(reserved);
        }
        InjectionMsg::Settle {
            ticket,
            settlement,
            persistence,
            reply,
            ..
        } => {
            let _ = reply.send(settle(state, channel_id, ticket, settlement, &persistence));
        }
        InjectionMsg::Abandon { ticket, reply, .. } => {
            abandon(state, channel_id, ticket);
            let _ = reply.send(());
        }
        InjectionMsg::Head(msg) => super::queued_head::step(state, channel_id, msg),
        InjectionMsg::Held(msg) => msg.step(state, channel_id),
    }
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
        selection_stale: false,
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
    channel_id: ChannelId,
    message_id: Option<MessageId>,
    expected: &ExpectedClaim,
    persistence: &QueuePersistenceContext,
) -> ReserveOutcome {
    if let Some(message) = message_id {
        if state.remint_fence.refuses_discarded(&[message]) {
            return ReserveOutcome::Consumed;
        }
        let now = std::time::Instant::now();
        if disposition::terminal(Some(&persistence.provider), message, now).is_some() {
            return ReserveOutcome::Consumed;
        }
        if mailbox_holds(state, message) {
            return ReserveOutcome::Owned;
        }
    }
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
    // A reservation, a dequeued head or a queued head mid-injection is unfinished ownership.
    if holds_order(state)
        || state.pending_user_dispatch.is_some()
        || super::queued_head::holds_front(state)
    {
        return ReserveOutcome::Backlog;
    }
    use crate::services::discord::busy_followup_retry_store::{
        BacklogLayers, LayerSnapshot, RetryClass, input_class,
    };
    let classify = |queue: &[Intervention]| -> Vec<RetryClass> {
        let class = |item| input_class(&persistence.provider, channel_id, item);
        queue.iter().map(class).collect()
    };
    let disk = match disk_backlog(channel_id, persistence) {
        Ok(Some(queued)) => Some(classify(&queued)),
        Ok(None) => return ReserveOutcome::Backlog,
        Err(error) => {
            tracing::warn!(channel = channel_id.get(), %error, "injection reservation refused");
            None
        }
    };
    let memory = classify(&state.intervention_queue);
    let layers = BacklogLayers {
        memory: Some(LayerSnapshot::Classes(&memory)),
        disk: Some(
            disk.as_deref()
                .map_or(LayerSnapshot::Unreadable, LayerSnapshot::Classes),
        ),
    };
    if !layers.admits_later_input() {
        return match disk {
            Some(_) => ReserveOutcome::Backlog,
            None => ReserveOutcome::Unavailable,
        };
    }
    let lease = Arc::new(InjectionLease::default());
    state.injection_reserved = Some((message_id, lease.clone()));
    if let Some(message) = message_id {
        // A catch-up enqueue classified before this reservation is refused by its claim CAS.
        state.record_injection_claim(message);
    }
    let message = message_id;
    ReserveOutcome::Reserved(InjectionTicket { lease, message })
}

/// Whether the mailbox already holds `message`: queued, handed out to a live dispatch, or in the
/// turn. A dispatch whose lease was dropped no longer holds it.
fn mailbox_holds(state: &ChannelMailboxState, message: MessageId) -> bool {
    let queued = (state.intervention_queue.iter())
        .any(|item| item.message_id == message || item.source_message_ids.contains(&message));
    let dispatching =
        state.pending_user_dispatch == Some(message) && !pending_dispatch_lease_is_orphaned(state);
    queued
        || dispatching
        || state.active_user_message_id == Some(message)
        || state.active_absorbed_source_ids.contains(&message)
}

/// The provider this actor last persisted for; `None` on an actor that has not persisted yet.
pub(super) fn actor_provider(state: &ChannelMailboxState) -> Option<&ProviderKind> {
    state
        .last_persistence
        .as_ref()
        .map(|persistence| &persistence.provider)
}

/// `message` under a reservation whose owner still holds its lease.
fn reserved_live(state: &ChannelMailboxState, message: MessageId) -> bool {
    let live = |(reserved, lease): &(Option<MessageId>, Arc<InjectionLease>)| {
        *reserved == Some(message) && Arc::strong_count(lease) > 1
    };
    state.injection_reserved.as_ref().is_some_and(live)
}

/// Whether an injection owns `message` under `provider` (`None`: any provider), live or ended; a
/// claim of it yields. Another channel's injection does not own a message this mailbox holds.
pub(super) fn owns(
    state: &ChannelMailboxState,
    provider: Option<&ProviderKind>,
    message: MessageId,
) -> bool {
    let now = std::time::Instant::now();
    let elsewhere =
        || disposition::in_progress(provider, message, now) && !mailbox_holds(state, message);
    reserved_live(state, message)
        || disposition::terminal(provider, message, now).is_some()
        || elsewhere()
}

/// Refuses an enqueue of injected input: every source ended in a pane, or one is mid-injection.
pub(super) fn enqueue_refusal(
    state: &ChannelMailboxState,
    intervention: &Intervention,
) -> Option<EnqueueRefusalReason> {
    let sources = &intervention.source_message_ids;
    let now = std::time::Instant::now();
    let provider = actor_provider(state);
    let ended = |source: &MessageId| disposition::terminal(provider, *source, now).is_some();
    let (reason, label) = if !sources.is_empty() && sources.iter().all(ended) {
        (EnqueueRefusalReason::AlreadyActiveTurn, "injected_terminal")
    } else if sources.iter().any(|source| {
        reserved_live(state, *source) || disposition::in_progress(provider, *source, now)
    }) {
        (
            EnqueueRefusalReason::ClaimedSinceObservation,
            "injection_in_progress",
        )
    } else {
        return None;
    };
    tracing::info!(
        message_id = intervention.message_id.get(),
        label,
        "injected input refused"
    );
    Some(reason)
}

/// The persisted queue this actor may not have loaded, or `None` when a dequeued head's marker
/// still runs first; an error when the queue file or the marker cannot be read.
fn disk_backlog(
    channel_id: ChannelId,
    persistence: &QueuePersistenceContext,
) -> Result<Option<Vec<Intervention>>, String> {
    use super::pending_queue_persistence as disk;
    let (provider, token_hash) = (&persistence.provider, persistence.token_hash.as_str());
    let (queued, _) = disk::load_channel_pending_queue_checked(provider, token_hash, channel_id)?;
    let marker =
        disk::load_channel_pending_dispatch_marker_checked(provider, token_hash, channel_id)?;
    Ok(marker.is_none().then_some(queued))
}

fn release(state: &mut ChannelMailboxState, ticket: &InjectionTicket) {
    let own =
        |(_, lease): &(Option<MessageId>, Arc<InjectionLease>)| Arc::ptr_eq(lease, &ticket.lease);
    if state.injection_reserved.as_ref().is_some_and(own) {
        state.injection_reserved = None;
    }
}

pub(super) fn discard_waiting_reservation(
    state: &mut ChannelMailboxState,
    protected: &[MessageId],
) -> (Option<MessageId>, bool) {
    if state
        .injection_reserved
        .as_ref()
        .is_some_and(|(id, _)| id.is_some_and(|id| protected.contains(&id)))
    {
        return (None, false);
    }
    match state.injection_reserved.take() {
        Some((id, lease)) => {
            lease.0.store(true, Ordering::Release);
            (id, id.is_none())
        }
        None => (None, false),
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
    let intervention = match settlement {
        InjectionSettlement::HandBack(intervention) => intervention,
        InjectionSettlement::Delivered(outcome) => {
            let (provider, now) = (&persistence.provider, std::time::Instant::now());
            disposition::note_terminal(provider, channel_id, ticket.message, outcome, now);
            disposition::note_channel_delivery(provider, channel_id, now);
            release(state, &ticket);
            let queue_exit_events = Vec::new();
            return SettleOutcome::Committed { queue_exit_events };
        }
    };
    if ticket.lease.0.load(Ordering::Acquire) || super::clear_fence::refuses(state, &intervention) {
        release(state, &ticket);
        return SettleOutcome::Committed {
            queue_exit_events: vec![QueueExitEvent::new(
                *intervention,
                QueueExitKind::Superseded,
            )],
        };
    }
    if let Some(error) = absorb_disk_queue_error(state, channel_id, persistence) {
        return SettleOutcome::NotCommitted { ticket, error };
    }
    let previous_queue = state.intervention_queue.clone();
    let pending = state.pending_user_dispatch;
    let active = state.active_user_message_id;
    let queue = &mut state.intervention_queue;
    let front = requeue_intervention_front(queue, *intervention, pending, active, None);
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
    /// nothing waits ahead in memory or on disk; later input queues behind it until it settles.
    pub(crate) async fn reserve_injection(
        &self,
        message_id: Option<MessageId>,
        expected_claim: ExpectedClaim,
        persistence: QueuePersistenceContext,
    ) -> ReserveOutcome {
        self.request(|reply| {
            ChannelMailboxMsg::Injection(InjectionMsg::Reserve {
                message_id,
                expected_claim,
                persistence,
                reply,
            })
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
    ) -> SettleOutcome {
        let (reply, answer) = oneshot::channel();
        let msg = ChannelMailboxMsg::Injection(InjectionMsg::Settle {
            ticket,
            settlement,
            persistence,
            reply,
        });
        match self.sender.send(msg) {
            Ok(()) => answer.await.unwrap_or(SettleOutcome::Unknown),
            Err(unsent) => match unsent.0 {
                ChannelMailboxMsg::Injection(InjectionMsg::Settle { ticket, .. }) => {
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
    ) -> Result<Vec<QueueExitEvent>, &'static str> {
        for delay in HANDBACK_DELAYS {
            tokio::time::sleep(delay).await;
            let settlement = InjectionSettlement::HandBack(Box::new(intervention.clone()));
            match self
                .settle_injected_input(ticket, settlement, persistence.clone())
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
            .request(|reply| ChannelMailboxMsg::Injection(InjectionMsg::Abandon { ticket, reply }))
            .await;
        Err(HANDBACK_NOT_WRITTEN)
    }
}

#[cfg(test)]
#[path = "injected_inputs_tests.rs"]
mod tests;
