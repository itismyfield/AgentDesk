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
    match handle
        .reserve_injection(message_id, claim, context(), None)
        .await
    {
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

/// A fresh actor that has not loaded the disk reserves nothing past input queued there or a
/// dequeued head's marker, and nothing at all when either file cannot be read.
#[tokio::test]
async fn a_reservation_checks_the_queue_and_dispatch_marker_a_fresh_actor_has_not_loaded() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let token_hash = "inject-order-test";
    let root = crate::services::discord::runtime_store::discord_pending_queue_root();
    let dir = root
        .expect("pending queue root")
        .join(provider.as_str())
        .join(token_hash);
    std::fs::create_dir_all(&dir).expect("queue dir");
    let mut observed = Vec::new();
    for (n, case) in [
        "empty",
        "queued",
        "dispatched",
        "queue_unreadable",
        "marker_unreadable",
    ]
    .into_iter()
    .enumerate()
    {
        let channel = ChannelId::new(6_845_341 + n as u64);
        let file = |ext: &str| dir.join(format!("{}.{ext}", channel.get()));
        match case {
            "queued" => {
                let earlier = ChannelMailboxRegistry::default().handle(channel);
                assert!(earlier.enqueue(item(1), context()).await.enqueued);
            }
            "dispatched" => {
                let save = save_channel_pending_dispatch_marker;
                save(&provider, token_hash, channel, &item(1), None).expect("marker");
            }
            "queue_unreadable" => std::fs::write(file("json"), "not json").expect("queue"),
            "marker_unreadable" => std::fs::write(file("dispatch"), "not json").expect("marker"),
            _ => {}
        }
        let fresh = ChannelMailboxRegistry::default().handle(channel);
        let outcome = match fresh.reserve_injection(None, None, context(), None).await {
            ReserveOutcome::Reserved(_) => "Reserved".to_string(),
            refused => format!("{refused:?}"),
        };
        observed.push(format!("{case}: {outcome}"));
    }
    assert_eq!(
        observed,
        [
            "empty: Reserved",
            "queued: Backlog",
            "dispatched: Backlog",
            "queue_unreadable: Unavailable",
            "marker_unreadable: Unavailable",
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
    let reserve = handle.reserve_injection(None, None, context(), Some(permit.clone()));
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
    let reserve = handle.reserve_injection(None, None, context(), None).await;
    let ticket = InjectionTicket {
        lease: Arc::new(InjectionLease),
        message: None,
    };
    let settle = InjectionSettlement::Delivered(InjectionOutcome::Observed);
    let settled = match handle
        .settle_injected_input(ticket, settle, context(), None)
        .await
    {
        SettleOutcome::NotCommitted { error, .. } => error,
        _ => "landed".to_string(),
    };
    let ticket = InjectionTicket {
        lease: Arc::new(InjectionLease),
        message: None,
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

/// A Discord message's injection, reserved and settled as delivered with `outcome`.
async fn injected(handle: &ChannelMailboxHandle, message: u64, outcome: InjectionOutcome) {
    let ticket = reserved(handle, Some(MessageId::new(message)), None).await;
    let settle = InjectionSettlement::Delivered(outcome);
    let settled = handle.settle_injected_input(ticket, settle, context(), None);
    assert!(matches!(settled.await, SettleOutcome::Committed { .. }));
}

/// The actor never takes a delivered message back, by enqueue or by start, yet still takes
/// input that merges it with a message not yet delivered.
#[tokio::test]
async fn an_injected_message_is_neither_enqueued_nor_started_again() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let handle = ChannelMailboxRegistry::default().handle(ChannelId::new(6_845_201));
    injected(&handle, 6_845_202, InjectionOutcome::Observed).await;
    let refused = handle.enqueue(item(6_845_202), context()).await;
    let token = Arc::new(CancelToken::new());
    let start = handle.try_start_turn_kinded_with_persistence(
        token,
        UserId::new(7),
        MessageId::new(6_845_202),
        ActiveTurnKind::UserOrAgent,
        TurnAdmissionOrder::Immediate,
        context(),
    );
    let started = start.await.started;
    let snapshot = handle.snapshot().await;
    let mut merged = item(6_845_203);
    merged
        .source_message_ids
        .insert(0, MessageId::new(6_845_202));
    let merged = handle.enqueue(merged, context()).await;
    let observed = (
        refused.enqueued,
        refused.refusal_reason,
        started,
        snapshot.cancel_token.is_some(),
        snapshot.intervention_queue.len(),
        merged.enqueued,
    );
    let refusal = Some(EnqueueRefusalReason::AlreadyActiveTurn);
    assert_eq!(observed, (false, refusal, false, false, 0, true));
}

/// A catch-up enqueue classified before the reservation is refused by its claim CAS even once
/// the in-process record has aged out and no longer answers for the message.
#[tokio::test]
async fn a_scan_from_before_the_reservation_is_refused_after_the_terminal_ages_out() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (channel, message) = (ChannelId::new(6_845_211), MessageId::new(6_845_212));
    let handle = ChannelMailboxRegistry::default().handle(channel);
    let before = handle.snapshot().await.claim_observation;
    injected(&handle, message.get(), InjectionOutcome::Observed).await;
    let age = crate::services::discord::inject_disposition::test_support::age;
    age(
        &ProviderKind::Claude,
        message.get(),
        disposition::MEMORY_TTL,
    );
    let aged = disposition::terminal(None, message, std::time::Instant::now());
    let enqueue = handle.enqueue_observed(item(message.get()), context(), Some(before));
    let refusal = enqueue.await.refusal_reason;
    let claimed = Some(EnqueueRefusalReason::ClaimedSinceObservation);
    assert_eq!((aged, refusal), (None, claimed));
}

/// A message mid-injection is refused even where no claim CAS sees the reservation, until its
/// owner drops the ticket and the same enqueue is accepted.
#[tokio::test]
async fn a_live_injection_refuses_its_message_until_its_lease_dies() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (channel, message) = (ChannelId::new(6_845_221), MessageId::new(6_845_222));
    let handle = ChannelMailboxRegistry::default().handle(channel);
    let ticket = reserved(&handle, Some(message), None).await;
    let after = handle.snapshot().await.claim_observation;
    let mut observed = Vec::new();
    for observation in [Some(after), None] {
        let enqueue = handle.enqueue_observed(item(message.get()), context(), observation);
        let outcome = enqueue.await;
        observed.push((outcome.enqueued, outcome.refusal_reason));
    }
    drop(ticket);
    let outcome = handle.enqueue(item(message.get()), context()).await;
    observed.push((outcome.enqueued, outcome.refusal_reason));
    let in_progress = (false, Some(EnqueueRefusalReason::ClaimedSinceObservation));
    assert_eq!(observed, [in_progress, in_progress, (true, None)]);
}

/// A reservation for a message the mailbox holds (queued, live dispatch, active) is `Owned` and
/// for an injected one `Consumed`, ahead of the backlog check that a dead dispatch falls to.
#[tokio::test]
async fn a_reservation_for_a_held_or_injected_message_is_owned_or_consumed() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut observed = Vec::new();
    for (n, case) in ["queued", "dispatched", "orphaned", "active", "injected"]
        .iter()
        .enumerate()
    {
        let channel = ChannelId::new(6_845_231 + n as u64);
        let message = MessageId::new(6_845_241 + n as u64);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let (mut claim, mut _dispatch) = (None, None);
        match *case {
            "queued" => assert!(
                handle
                    .enqueue(item(message.get()), context())
                    .await
                    .enqueued
            ),
            "dispatched" | "orphaned" => {
                assert!(
                    handle
                        .enqueue(item(message.get()), context())
                        .await
                        .enqueued
                );
                let taken = handle.take_next_soft(context()).await;
                let head = taken.intervention.map(|item| item.message_id);
                assert_eq!(head, Some(message));
                _dispatch = taken.dispatch_lease.filter(|_| *case == "dispatched");
                let orphan_after = super::super::PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER;
                handle
                    .age_inbound_waits_for_test(orphan_after + Duration::from_secs(1))
                    .await;
            }
            "active" => {
                let token = Arc::new(CancelToken::new());
                assert!(
                    handle
                        .try_start_turn(token.clone(), UserId::new(7), message)
                        .await
                );
                claim = Some((token, ActiveTurnKind::UserOrAgent, Some(message)));
            }
            _ => injected(&handle, message.get(), InjectionOutcome::Observed).await,
        }
        let reserve = handle.reserve_injection(Some(message), claim, context(), None);
        observed.push(format!("{case}: {:?}", reserve.await));
    }
    assert_eq!(
        observed,
        [
            "queued: Owned",
            "dispatched: Owned",
            "orphaned: Backlog",
            "active: Owned",
            "injected: Consumed"
        ]
    );
}

/// An injected outcome survives the settle as given: an unconfirmed paste stays unconfirmed.
#[tokio::test]
async fn a_settle_keeps_the_injection_outcome() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let handle = ChannelMailboxRegistry::default().handle(ChannelId::new(6_845_251));
    let mut observed = Vec::new();
    for (message, outcome) in [
        (6_845_252, InjectionOutcome::Unconfirmed),
        (6_845_253, InjectionOutcome::Observed),
    ] {
        injected(&handle, message, outcome).await;
        let now = std::time::Instant::now();
        let kept = disposition::terminal(Some(&ProviderKind::Claude), MessageId::new(message), now);
        observed.push(kept);
    }
    let (unconfirmed, seen) = (InjectionOutcome::Unconfirmed, InjectionOutcome::Observed);
    assert_eq!(observed, [Some(unconfirmed), Some(seen)]);
}

/// An idle slot refuses an injected message's claim before any other yield rule runs, leaving
/// the background-yield count, pending-dispatch retirement and stall clock untouched.
#[test]
fn an_injected_claim_yields_before_the_yield_helpers_count_or_clear_anything() {
    let message = MessageId::new(6_845_261);
    let now = std::time::Instant::now();
    let outcome = InjectionOutcome::Observed;
    let channel = ChannelId::new(6_845_260);
    disposition::note_terminal(&ProviderKind::Claude, channel, Some(message), outcome, now);
    let behind = TurnAdmissionOrder::BehindQueue;
    let mut observed = Vec::new();
    for case in ["plain", "background_pending_only", "stall_clock"] {
        let mut state = ChannelMailboxState {
            last_persistence: Some(context()),
            ..ChannelMailboxState::default()
        };
        let mut kind = ActiveTurnKind::UserOrAgent;
        let lease = Arc::new(DispatchLease);
        match case {
            "background_pending_only" => {
                kind = ActiveTurnKind::Background;
                state.pending_user_dispatch = Some(MessageId::new(3));
                state.pending_user_dispatch_since = Some(Instant::now());
                state.pending_user_dispatch_lease = Some(lease.clone());
            }
            "stall_clock" => state.intervention_queue.push(item(4)),
            _ => {}
        }
        let yields = claim_yields(&mut state, kind, message, behind);
        observed.push(format!(
            "{case}: yields={yields} count={} pending={} stall={}",
            state.pending_user_dispatch_yield_count,
            state.pending_user_dispatch.is_some(),
            state.inbound_stall_since.is_some(),
        ));
    }
    assert_eq!(
        observed,
        [
            "plain: yields=true count=0 pending=false stall=false",
            "background_pending_only: yields=true count=0 pending=true stall=false",
            "stall_clock: yields=true count=0 pending=false stall=false",
        ]
    );
}

/// An actor that knows its provider reads only that provider's terminals; one that has not
/// persisted yet reads any provider's, which can only refuse more.
#[test]
fn a_fresh_actor_reads_any_provider_terminal_and_a_known_one_only_its_own() {
    let message = MessageId::new(6_845_271);
    let (channel, now) = (ChannelId::new(6_845_270), std::time::Instant::now());
    let outcome = InjectionOutcome::Observed;
    disposition::note_terminal(&ProviderKind::Codex, channel, Some(message), outcome, now);
    let claude = ChannelMailboxState {
        last_persistence: Some(context()),
        ..ChannelMailboxState::default()
    };
    let fresh = ChannelMailboxState::default();
    assert_eq!(
        (owns(&fresh, message), owns(&claude, message)),
        (true, false)
    );
}

/// A busy slot answers a claim of an injected message as it answers any other: no start, and
/// the running turn keeps its token, so a stale-busy heal sees what it saw before.
#[tokio::test]
async fn a_busy_slot_keeps_its_turn_when_an_injected_message_is_claimed() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let handle = ChannelMailboxRegistry::default().handle(ChannelId::new(6_845_281));
    injected(&handle, 6_845_282, InjectionOutcome::Observed).await;
    let running = Arc::new(CancelToken::new());
    let holder = MessageId::new(6_845_283);
    assert!(
        handle
            .try_start_turn(running.clone(), UserId::new(7), holder)
            .await
    );
    let other = Arc::new(CancelToken::new());
    let message = MessageId::new(6_845_282);
    let started = handle.try_start_turn(other, UserId::new(7), message).await;
    let snapshot = handle.snapshot().await;
    let kept = snapshot
        .cancel_token
        .as_ref()
        .is_some_and(|token| Arc::ptr_eq(token, &running));
    let observed = (started, kept, snapshot.active_user_message_id);
    assert_eq!(observed, (false, true, Some(holder)));
}

/// A cancel that removed the last input's file but failed the parent fsync is not reported;
/// memory keeps the input, so the reservation still sees backlog with disk and marker empty.
#[tokio::test(flavor = "current_thread")]
async fn a_cancel_that_removed_the_file_but_failed_its_fsync_still_holds_the_reservation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(6_845_291);
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).expect("gate");
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let handle = ChannelMailboxRegistry::default().handle(channel);
    assert!(handle.enqueue(item(6_845_292), context()).await.enqueued);
    super::super::pending_queue_persistence::fsync_fault::fail_next(channel);
    let cancel = handle.cancel_queued_primary_message(MessageId::new(6_845_292), context());
    let cancelled = cancel.await;
    let (provider, hash) = (ProviderKind::Claude, "inject-order-test");
    let disk = load_channel_pending_queue(&provider, hash, channel).0.len();
    let marker = super::super::load_channel_pending_dispatch_marker(&provider, hash, channel);
    let reserve = handle.reserve_injection(None, None, context(), None).await;
    let observed = format!(
        "removed={} error={} memory={:?} disk={disk} marker={} reserve={reserve:?}",
        cancelled.removed.is_some(),
        cancelled.persistence_error.is_some(),
        queued(&handle).await,
        marker.is_some(),
    );
    let expected = "removed=false error=true memory=[6845292] disk=0 marker=false reserve=Backlog";
    assert_eq!(observed, expected);
}
