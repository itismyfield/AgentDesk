//! The injection order reservation at the actor API: what it withholds, how it settles, and how
//! it ends when its owner or its result goes missing.
use super::*;
use crate::services::discord::input_runtime::fence::{Failure, Gate, Mode};
use crate::services::turn_orchestrator::test_support::{
    fail_queue_saves, injection_abandons, queue_save_faults,
};

fn item(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("input {id}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn context() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "inject-order-test", None)
}

async fn queued(handle: &ChannelMailboxHandle) -> Vec<u64> {
    let queue = handle.snapshot().await.intervention_queue;
    queue.iter().map(|item| item.message_id.get()).collect()
}

async fn reserved(
    handle: &ChannelMailboxHandle,
    message_id: Option<MessageId>,
    claim: ExpectedClaim,
) -> InjectionTicket {
    match handle.reserve_injection(message_id, claim, None).await {
        ReserveOutcome::Reserved(ticket) => ticket,
        other => panic!("reservation refused: {other:?}"),
    }
}

/// A `BehindQueue` claim of `message` as Discord intake makes it.
async fn behind_queue_claim(handle: &ChannelMailboxHandle, message: u64) -> bool {
    let token = Arc::new(CancelToken::new());
    let (owner, message) = (UserId::new(7), MessageId::new(message));
    let (kind, order) = (ActiveTurnKind::UserOrAgent, TurnAdmissionOrder::BehindQueue);
    let claim = handle.try_start_turn_kinded_with_persistence(
        token,
        owner,
        message,
        kind,
        order,
        context(),
    );
    claim.await.started
}

async fn hand_back(
    handle: &ChannelMailboxHandle,
    ticket: InjectionTicket,
    id: u64,
) -> SettleOutcome {
    let settlement = InjectionSettlement::HandBack(Box::new(item(id)));
    handle
        .settle_injected_input(ticket, settlement, context(), None)
        .await
}

/// With the turn over, a Discord-id or message-less reservation alone refuses a `BehindQueue` claim
/// and keeps later input queued; once its ticket drops, that input is dequeued and claimed.
#[tokio::test]
async fn take_next_and_behind_queue_claims_wait_on_a_reservation_after_its_turn_ended() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut observed = Vec::new();
    for (case, message_id) in [(1, Some(MessageId::new(6_845_119))), (2, None)] {
        let channel = ChannelId::new(6_845_110 + case);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let (token, held) = (
            Arc::new(CancelToken::new()),
            MessageId::new(6_845_100 + case),
        );
        assert!(
            handle
                .try_start_turn(token.clone(), UserId::new(7), held)
                .await
        );
        let claim = Some((token, ActiveTurnKind::UserOrAgent, Some(held)));
        let ticket = reserved(&handle, message_id, claim).await;
        handle.hard_stop().await;
        let claimed = behind_queue_claim(&handle, 6_845_130 + case).await;
        let later = 6_845_120 + case;
        assert!(handle.enqueue(item(later), context()).await.enqueued);
        let withheld = handle.take_next_soft(context()).await;
        observed.push(format!(
            "reserved: claim={claimed} head={:?} left={}",
            withheld.intervention.map(|item| item.message_id.get()),
            withheld.queue_len_after,
        ));
        drop(ticket);
        let taken = handle.take_next_soft(context()).await;
        let head = taken.intervention.map(|item| item.message_id.get());
        let claimed = behind_queue_claim(&handle, later).await;
        observed.push(format!("dropped: head={head:?} claim={claimed}"));
    }
    assert_eq!(
        observed,
        [
            "reserved: claim=false head=None left=1",
            "dropped: head=Some(6845121) claim=true",
            "reserved: claim=false head=None left=1",
            "dropped: head=Some(6845122) claim=true",
        ]
    );
}

/// The inbound-order fail-open and the dequeued head's own claim each admit a `BehindQueue`
/// claim without a reservation; neither admits one past a live reservation.
#[test]
fn a_reservation_holds_past_the_fail_open_and_the_dequeued_head_exceptions() {
    let stalled = Instant::now() - INBOUND_ORDER_FAIL_OPEN_AFTER - Duration::from_secs(1);
    let behind = TurnAdmissionOrder::BehindQueue;
    let kind = ActiveTurnKind::UserOrAgent;
    let lease = Arc::new(InjectionLease);
    let dispatch = Arc::new(DispatchLease);
    let mut observed = Vec::new();
    for reserve in [false, true] {
        let mut fail_open = ChannelMailboxState::default();
        fail_open.intervention_queue.push(item(1));
        fail_open.inbound_stall_since = Some(stalled);
        let mut dequeued = ChannelMailboxState::default();
        dequeued.pending_user_dispatch = Some(MessageId::new(3));
        dequeued.pending_user_dispatch_since = Some(Instant::now());
        dequeued.pending_user_dispatch_lease = Some(dispatch.clone());
        if reserve {
            fail_open.injection_reserved = Some((None, lease.clone()));
            dequeued.injection_reserved = Some((Some(MessageId::new(9)), lease.clone()));
        }
        let open = claim_yields(&mut fail_open, kind, MessageId::new(2), behind);
        let head = claim_yields(&mut dequeued, kind, MessageId::new(3), behind);
        observed.push((reserve, open, head));
    }
    assert_eq!(observed, [(false, false, false), (true, true, true)]);
}

/// With no other refusal standing, a live reservation, with or without a message id, keeps the
/// actor from a purge; a reservation whose ticket dropped does not.
#[tokio::test]
async fn close_if_idle_refuses_a_live_reservation_and_admits_an_orphaned_one() {
    let mut observed = Vec::new();
    for (case, reservation) in [
        ("none", None),
        ("discord", Some(Some(MessageId::new(6_845_149)))),
        ("imessage", Some(None)),
        ("orphan", Some(None)),
    ] {
        let channel = ChannelId::new(6_845_140 + observed.len() as u64);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let ticket = match reservation {
            Some(message_id) => Some(reserved(&handle, message_id, None).await),
            None => None,
        };
        let ticket = ticket.filter(|_| case != "orphan");
        let outcome = registry.remove_idle_entry(channel).await;
        observed.push(format!("{case}: {outcome:?}"));
        drop(ticket);
    }
    assert_eq!(
        observed,
        [
            "none: Removed",
            "discord: RefusedLiveWork(\"injection_reserved\")",
            "imessage: RefusedLiveWork(\"injection_reserved\")",
            "orphan: Removed",
        ]
    );
}

/// A handback write that did not land keeps the reservation and returns the same ticket: input
/// sent after it stays queued and unclaimed, and the retry puts the handback ahead of it.
#[tokio::test]
async fn an_unwritten_handback_keeps_the_reservation_until_the_same_ticket_lands() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(6_845_151);
    let handle = ChannelMailboxRegistry::default().handle(channel);
    let ticket = reserved(&handle, None, None).await;
    assert!(handle.enqueue(item(6_845_152), context()).await.enqueued);
    fail_queue_saves(channel, 1);
    let SettleOutcome::NotCommitted { ticket, .. } = hand_back(&handle, ticket, 6_845_153).await
    else {
        panic!("the injected save failure must leave the handback unwritten");
    };
    let withheld = handle.take_next_soft(context()).await.intervention;
    let claimed = behind_queue_claim(&handle, 6_845_154).await;
    let unwritten = (withheld.is_none(), claimed, queued(&handle).await);
    assert_eq!(unwritten, (true, false, vec![6_845_152]));
    let SettleOutcome::Committed { queue_exit_events } =
        hand_back(&handle, ticket, 6_845_153).await
    else {
        panic!("the retried handback must land");
    };
    let durable = load_channel_pending_queue(&ProviderKind::Claude, "inject-order-test", channel);
    let durable: Vec<u64> = durable.0.iter().map(|item| item.message_id.get()).collect();
    assert!(queue_exit_events.is_empty());
    assert_eq!(queued(&handle).await, [6_845_153, 6_845_152]);
    assert_eq!(durable, [6_845_153, 6_845_152]);
    let head = handle.take_next_soft(context()).await.intervention;
    assert_eq!(head.map(|item| item.message_id.get()), Some(6_845_153));
}

/// Four attempts at 0, 250ms, 1s and 4s: three unwritten then one landed hands back once with
/// no abandon; four unwritten abandon once and never write a fifth time.
#[tokio::test(start_paused = true)]
async fn the_handback_makes_four_attempts_then_abandons_the_reservation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut observed = Vec::new();
    for (channel, faults) in [(6_845_161, 3), (6_845_162, 5)] {
        let channel = ChannelId::new(channel);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let ticket = reserved(&handle, None, None).await;
        assert!(handle.enqueue(item(1), context()).await.enqueued);
        fail_queue_saves(channel, faults);
        let started = tokio::time::Instant::now();
        let handed = handle.hand_back_injected_input(ticket, item(2), context(), None);
        let result = handed.await.map(|events| events.len());
        let elapsed = started.elapsed().as_millis();
        let left = queue_save_faults(channel, true);
        let abandons = injection_abandons(channel);
        let queue = queued(&handle).await;
        let head = handle.take_next_soft(context()).await.intervention;
        let head = head.map(|item| item.message_id.get());
        observed.push(format!(
            "{result:?} after={elapsed}ms attempts={} abandons={abandons} {queue:?} head={head:?}",
            faults - left + usize::from(result.is_ok()),
        ));
    }
    assert_eq!(
        observed,
        [
            "Ok(0) after=5250ms attempts=4 abandons=0 [2, 1] head=Some(2)",
            "Err(\"handback_persistence\") after=5250ms attempts=4 abandons=1 [1] head=Some(1)",
        ]
    );
}

/// A mailbox whose task takes each message and answers nothing, or answers through `forward`.
fn fixture(
    forward: Option<ChannelMailboxHandle>,
) -> (ChannelMailboxHandle, tokio::task::JoinHandle<usize>) {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let handle = ChannelMailboxHandle {
        sender,
        recovery_done: Arc::new(RecoveryDoneSignal::new()),
    };
    let task = tokio::spawn(async move {
        let mut received = 0;
        while let Some((msg, _)) = receiver.recv().await {
            received += 1;
            let (
                Some(actor),
                ChannelMailboxMsg::Injection(InjectionMsg::Settle {
                    ticket,
                    settlement,
                    persistence,
                    reply,
                    ..
                }),
            ) = (forward.as_ref(), msg)
            else {
                continue;
            };
            let landed = actor
                .settle_injected_input(ticket, settlement, persistence, None)
                .await;
            assert!(matches!(landed, SettleOutcome::Committed { .. }));
            drop(reply);
        }
        received
    });
    (handle, task)
}

/// Sent but unanswered, dropped unread or written with the answer lost, is no success and no
/// resend; a ticket dropped unanswered orphans its reservation, so later input moves again.
#[tokio::test]
async fn a_sent_handback_without_an_answer_is_unknown_and_never_resent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut observed = Vec::new();
    for (case, channel) in [("dropped", 6_845_171), ("answer_lost", 6_845_172)] {
        let channel = ChannelId::new(channel);
        let actor = ChannelMailboxRegistry::default().handle(channel);
        let ticket = reserved(&actor, None, None).await;
        assert!(actor.enqueue(item(1), context()).await.enqueued);
        let forward = (case == "answer_lost").then(|| actor.clone());
        let (relay, task) = fixture(forward);
        let handed = relay.hand_back_injected_input(ticket, item(2), context(), None);
        let result = handed.await.map(|events| events.len());
        drop(relay);
        let sends = task.await.expect("fixture");
        let queue = queued(&actor).await;
        let head = actor.take_next_soft(context()).await.intervention;
        let head = head.map(|item| item.message_id.get());
        observed.push(format!(
            "{case}: {result:?} sends={sends} {queue:?} head={head:?}"
        ));
    }
    assert_eq!(
        observed,
        [
            "dropped: Err(\"handback_unknown\") sends=1 [1] head=Some(1)",
            "answer_lost: Err(\"handback_unknown\") sends=1 [2, 1] head=Some(2)",
        ]
    );
}

/// The handback of an injection admitted before the gate closed lands under its owner's permit
/// while Closing, when no new input is admitted; the drain completes once that permit returns.
#[tokio::test(flavor = "current_thread")]
async fn a_handback_lands_under_its_owner_permit_while_the_gate_closes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(6_845_181);
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).expect("gate");
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let handle = ChannelMailboxRegistry::default().handle(channel);
    let permit = gate.admit().expect("legacy open admits the owner");
    let reserve = handle.reserve_injection(None, None, Some(permit.clone()));
    let ReserveOutcome::Reserved(ticket) = reserve.await else {
        panic!("reservation under the owner permit");
    };
    fail_queue_saves(channel, 1);
    let settle = |ticket| {
        let settlement = InjectionSettlement::HandBack(Box::new(item(2)));
        handle.settle_injected_input(ticket, settlement, context(), Some(permit.clone()))
    };
    let SettleOutcome::NotCommitted { ticket, .. } = settle(ticket).await else {
        panic!("the first write must not land");
    };
    let closing = gate.close().expect("closing");
    let admitted = gate.admit().map(|_| ());
    let refusal = handle.enqueue(item(3), context()).await.refusal_reason;
    assert_eq!(admitted, Err(Failure::Mode(Mode::Closing)));
    assert_eq!(
        refusal,
        Some(EnqueueRefusalReason::InputModeFenced(Mode::Closing))
    );
    let SettleOutcome::Committed { .. } = settle(ticket).await else {
        panic!("the same ticket lands under the owner permit");
    };
    assert_eq!(queued(&handle).await, [2]);
    drop(permit);
    let drained = tokio::time::timeout(Duration::from_secs(5), closing.drain());
    drained.await.expect("the owner permit was the last effect");
}

/// A purge-closed actor refuses a reservation, returns a settle's ticket uncommitted, and lets
/// an abandon through.
#[tokio::test]
async fn a_closed_actor_refuses_reserve_and_settle_and_passes_abandon() {
    let channel = ChannelId::new(6_845_191);
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    let removed = registry.remove_idle_entry(channel).await;
    let reserve = handle.reserve_injection(None, None, None).await;
    let ticket = InjectionTicket {
        lease: Arc::new(InjectionLease),
    };
    let settle = InjectionSettlement::Delivered;
    let settled = match handle
        .settle_injected_input(ticket, settle, context(), None)
        .await
    {
        SettleOutcome::NotCommitted { error, .. } => error,
        _ => "landed".to_string(),
    };
    let ticket = InjectionTicket {
        lease: Arc::new(InjectionLease),
    };
    let abandoned = handle
        .request(|reply| {
            ChannelMailboxMsg::Injection(InjectionMsg::Abandon {
                input_permit: None,
                ticket,
                reply,
            })
        })
        .await;
    let observed = format!("{removed:?} {reserve:?} {settled} {abandoned:?}");
    assert_eq!(observed, "Removed Unavailable actor_closed Ok(())");
}
