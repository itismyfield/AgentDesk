//! A policy close's tail is owned by its exact ledger entry: a sealed commit resumes once without
//! asking the mailbox again, and its channel cleanup runs only while no successor claimed.
use super::policy::{assert_released, panic_once_at};
use super::*;
use crate::services::discord::turn_finalizer::{
    FinalizeContext, FinalizeOutcome, PolicyCloseOutcome, TerminalEvent,
};
use crate::services::turn_orchestrator::policy_close::tail::{PolicyTailDecision, PolicyTailSkip};
use crate::services::turn_orchestrator::policy_close::test_hooks::{Loss, Probe, Stage};
use crate::services::turn_orchestrator::policy_close::{
    Point, PolicyClear, PolicyRetain, PolicyStage,
};
use futures::FutureExt;
use std::sync::Mutex;

const GRANTED: Option<PolicyTailDecision> = Some(PolicyTailDecision::Granted);
const SUCCESSOR: Option<PolicyTailDecision> = Some(PolicyTailDecision::Skipped(
    PolicyTailSkip::SuccessorObserved,
));

/// The channel's parent and its counter-model role channel.
fn route_of(close: &Close) -> (ChannelId, ChannelId) {
    let channel = close.row.channel_id;
    (
        ChannelId::new(channel + 3_000_000_000),
        ChannelId::new(channel + 4_000_000_000),
    )
}

/// A claimed turn's route as intake publishes it after its claim.
fn publish(close: &Close, parent: ChannelId, role: ChannelId) {
    let dispatch = &close.shared.dispatch;
    dispatch.thread_parents.insert(parent, close.channel());
    dispatch.role_overrides.insert(close.channel(), role);
}

fn routed(close: &Close, parent: ChannelId) -> (Option<ChannelId>, Option<ChannelId>) {
    let dispatch = &close.shared.dispatch;
    (
        dispatch.thread_parents.get(&parent).map(|thread| *thread),
        dispatch
            .role_overrides
            .get(&close.channel())
            .map(|role| *role),
    )
}

fn recovery_done(close: &Close) -> bool {
    let mailbox = close.shared.mailbox_peek(close.channel()).unwrap();
    mailbox.recovery_done().wait().now_or_never().is_some()
}

fn ticket_released(probe: &Probe) -> bool {
    probe
        .ticket()
        .expect("the ledger ticket")
        .upgrade()
        .is_none()
}

fn read(probe: &Probe, points: &[Point]) -> Vec<u64> {
    probe.snapshot().judge(probe.request, points).unwrap()
}

const TAIL: [Point; 4] = [
    Point::MailboxCommitted,
    Point::QueueExit,
    Point::FinalizeEntered,
    Point::FinalizeReturned,
];

/// Starts another turn with its +1 in the close's channel.
async fn start_successor(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    message: u64,
) -> Arc<CancelToken> {
    let successor = Arc::new(CancelToken::new());
    assert!(
        discord::mailbox_try_start_turn_kinded(
            shared,
            channel,
            successor.clone(),
            UserId::new(1),
            MessageId::new(message),
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    discord::increment_global_active(shared, "successor");
    successor
}

/// On another thread while the close's finalizer waits at its seam: a successor claims the channel,
/// starts its recovery cycle, then publishes its route, with the same role channel.
fn successor_at_seam(close: &Close, parent: ChannelId, role: ChannelId) -> (Raced, SeamHook) {
    let slot = Raced::default();
    let (shared, channel, message) = (close.shared.clone(), close.channel(), close.row.user_msg_id);
    let (handle, raced) = (tokio::runtime::Handle::current(), slot.clone());
    let hook = at_seam(move || {
        let (shared, handle) = (shared.clone(), handle.clone());
        let successor = std::thread::spawn(move || {
            handle.block_on(async move {
                let successor = start_successor(&shared, channel, message + 9).await;
                shared
                    .mailbox_peek(channel)
                    .unwrap()
                    .recovery_done()
                    .reset();
                shared.dispatch.thread_parents.insert(parent, channel);
                shared.dispatch.role_overrides.insert(channel, role);
                successor
            })
        });
        *raced.lock().unwrap() = Some(successor.join().unwrap());
    });
    (slot, hook)
}

type Raced = Arc<Mutex<Option<Arc<CancelToken>>>>;
type SeamHook = Box<dyn FnMut(Stage) + Send>;

/// A stage hook that runs `work` once, at the finalizer's seam before the tail.
fn at_seam(work: impl FnOnce() + Send + 'static) -> SeamHook {
    let mut work = Some(work);
    Box::new(move |stage| {
        if stage == Stage::BeforeFinalize {
            if let Some(work) = work.take() {
                work();
            }
        }
    })
}

/// What a skipped tail must leave: the successor's slot, route and recovery cycle.
async fn assert_successor_kept(
    close: &Close,
    successor: &Arc<CancelToken>,
    parent: ChannelId,
    role: ChannelId,
    case: &str,
) {
    let slot = close.slot().await.expect("the successor holds the slot");
    assert!(
        Arc::ptr_eq(&slot, successor),
        "{case}: the successor's token"
    );
    assert!(!successor.cancelled.load(Ordering::Relaxed), "{case}");
    assert_eq!(
        routed(close, parent),
        (Some(close.channel()), Some(role)),
        "{case}: route"
    );
    assert!(
        !recovery_done(close),
        "{case}: the successor's recovery cycle stays open"
    );
}

/// A commit whose reply is lost settles on the first attempt from its sealed ticket: the granted
/// tail takes the episode's parent route once, and publishes one release.
#[tokio::test(flavor = "current_thread")]
async fn s2f_reply_loss_settles_from_the_sealed_ticket() {
    for queued in [true, false] {
        let close = Close::open(5_342_001, |_| {}).await;
        let (parent, role) = route_of(&close);
        publish(&close, parent, role);
        let queue = if queued {
            close.other_turn_and_queue().await
        } else {
            Vec::new()
        };
        let acked = close.acked().await;
        let intent = close.intent(acked);
        let probe = intent.request.trace.probe();
        probe.lose(Loss::MailboxReply);
        let mut events = close.events();
        let holders = Arc::strong_count(&close.actor);
        let outcome = close.submit(intent).await;
        let case = format!("queued={queued}");
        assert!(
            matches!(outcome, PolicyCloseOutcome::Settled),
            "{case}: {outcome:?}"
        );
        assert_eq!(probe.tail(), GRANTED, "{case}");
        assert_eq!(read(&probe, &TAIL), [1, 1, 1, 1], "{case}");
        assert_eq!(close.released(&mut events), 1, "{case}: one release edge");
        assert!(recovery_done(&close), "{case}");
        assert_eq!(
            close.durable().map(|_| ()),
            None,
            "{case}: the receipt is gone"
        );
        assert!(close.slot().await.is_none(), "{case}");
        assert_eq!(close.active(), usize::from(queued), "{case}: one debit");
        assert_eq!(
            Arc::strong_count(&close.actor),
            holders - 1,
            "{case}: the token is let go"
        );
        assert!(
            ticket_released(&probe),
            "{case}: the ledger dropped its commit"
        );
        let (parent_route, kept_role) = routed(&close, parent);
        assert_eq!(parent_route, None, "{case}: the parent route is released");
        if queued {
            // The queued input runs under the same role.
            assert_eq!(kept_role, Some(role), "{case}");
            assert_eq!(close.queued().await, queue, "{case}");
        } else {
            assert_eq!(kept_role, None, "{case}: nothing runs after it");
            assert!(close.queued().await.is_empty(), "{case}");
        }
    }
}

/// A successor that claims while the tail waits after a lost reply keeps everything it holds: the
/// tail is skipped, and only the episode's queue exit and its own finalization run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s2f_successor_before_the_tail_keeps_its_claim_and_route() {
    let close = Close::open(5_342_002, |_| {}).await;
    let (parent, role) = route_of(&close);
    publish(&close, parent, role);
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let intent = close.intent(acked);
    let probe = intent.request.trace.probe();
    probe.lose(Loss::MailboxReply);
    let (raced, hook) = successor_at_seam(&close, parent, role);
    probe.on_stage(hook);
    let mut events = close.events();
    let outcome = close.submit(intent).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    let successor = raced.lock().unwrap().take().expect("the successor claimed");
    assert_eq!(probe.tail(), SUCCESSOR);
    assert_successor_kept(&close, &successor, parent, role, "reply loss").await;
    assert_eq!(read(&probe, &TAIL), [1, 1, 1, 1]);
    assert_eq!(
        close.released(&mut events),
        0,
        "no release edge for a skipped tail"
    );
    assert_eq!(
        close.queued().await,
        queued,
        "nothing dequeued behind the successor"
    );
    assert_eq!(close.active(), 2, "the other turn and the successor");
    assert!(ticket_released(&probe));
}

/// A caught panic before the tail keeps the seal and the restored entry: the successor that claims
/// before the second attempt keeps its state, and the queue exit is handed out once in total.
#[tokio::test(flavor = "current_thread")]
async fn s2f_panic_before_the_tail_resumes_skipped_behind_a_successor() {
    let close = Close::open(5_342_003, |_| {}).await;
    let (parent, role) = route_of(&close);
    publish(&close, parent, role);
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let intent = close.intent(acked.clone());
    let first = intent.request.trace.probe();
    panic_once_at(&first, Stage::BeforeFinalize);
    assert!(matches!(
        close.submit(intent).await,
        PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerPanic)
    ));
    assert_eq!(read(&first, &TAIL[..3]), [1, 0, 0]);
    let successor =
        start_successor(&close.shared, close.channel(), close.row.user_msg_id + 9).await;
    let mailbox = close.shared.mailbox_peek(close.channel()).unwrap();
    mailbox.recovery_done().reset();
    publish(&close, parent, role);
    let mut events = close.events();
    let intent = close.intent(acked);
    let second = intent.request.trace.probe();
    let outcome = close.submit(intent).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    assert_eq!(second.tail(), SUCCESSOR);
    assert_eq!(read(&second, &TAIL), [0, 1, 1, 1], "no second mailbox step");
    assert_eq!(second.stages(), [Stage::BeforeFinalize]);
    assert_successor_kept(&close, &successor, parent, role, "panic").await;
    assert_eq!(close.released(&mut events), 0);
    assert_eq!(close.queued().await, queued);
    assert!(ticket_released(&second));
}

/// Wherever the step unwinds after the receipt clear, the commit is sealed before the actor reads
/// on: the same attempt settles, releases and debits once, and a re-ask is a plain duplicate.
#[tokio::test(flavor = "current_thread")]
async fn s2f_unwound_commit_is_sealed_at_every_seam() {
    for at in [
        Stage::AfterClear,
        Stage::AfterDetach,
        Stage::AfterDebit,
        Stage::AfterFence,
        Stage::AfterCommit,
        Stage::BeforeSeal,
    ] {
        let close = Close::open(5_342_004, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let acked = close.acked().await;
        let intent = close.intent(acked.clone());
        let probe = intent.request.trace.probe();
        panic_once_at(&probe, at);
        let mut events = close.events();
        let outcome = close.submit(intent).await;
        let case = format!("{at:?}");
        assert!(
            matches!(outcome, PolicyCloseOutcome::Settled),
            "{case}: {outcome:?}"
        );
        assert_eq!(probe.tail(), GRANTED, "{case}");
        assert!(
            probe.stages().contains(&Stage::Unwound),
            "{case}: the step unwound"
        );
        assert_released(&close, &queued, &case).await;
        assert_eq!(close.released(&mut events), 1, "{case}");
        let again = close.submit(close.intent(acked)).await;
        assert!(
            matches!(again, PolicyCloseOutcome::DuplicateConfirmed),
            "{case}: {again:?}"
        );
        assert_released(&close, &queued, &case).await;
        assert_eq!(close.released(&mut events), 0, "{case}: no second release");
    }
}

/// A repeated start of the same exact turn keeps the pending commit: the second attempt resumes
/// the sealed tail instead of asking the mailbox for a commit it already made.
#[tokio::test(flavor = "current_thread")]
async fn s2f_repeat_start_keeps_the_pending_commit() {
    let close = Close::open(5_342_005, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let intent = close.intent(acked.clone());
    panic_once_at(&intent.request.trace.probe(), Stage::BeforeFinalize);
    assert!(matches!(
        close.submit(intent).await,
        PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerPanic)
    ));
    let generation = close.shared.restart.current_generation;
    let key = TurnKey::new(close.channel(), close.row.user_msg_id, generation)
        .with_episode_nonce(close.row.turn_nonce.as_deref());
    let provider = close.shared.provider.clone();
    close
        .fin
        .register_start(key, provider, RelayOwnerKind::None, &close.shared);
    let mut events = close.events();
    let intent = close.intent(acked);
    let probe = intent.request.trace.probe();
    let outcome = close.submit(intent).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    assert_eq!(probe.tail(), GRANTED);
    assert_eq!(read(&probe, &TAIL), [0, 1, 1, 1]);
    assert_eq!(close.released(&mut events), 1);
    assert_released(&close, &queued, "repeat start").await;
}

/// B, the next herdr episode on the close's channel, settles its own close through the bridge, so
/// the mailbox's one-slot witness now names B.
async fn settle_next_episode(close: &Close, offset: u64) -> InflightTurnState {
    let channel = close.channel();
    let (b_actor, owner) = prepared(ProviderKind::Claude, channel.get());
    let mut b_row = close.row.clone();
    b_row.user_msg_id += offset;
    b_row.current_msg_id += offset;
    b_row.turn_nonce = b_actor.turn_nonce().map(str::to_owned);
    assert!(
        discord::mailbox_try_start_turn_kinded(
            &close.shared,
            channel,
            b_actor.clone(),
            UserId::new(1),
            MessageId::new(b_row.user_msg_id),
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    discord::increment_global_active(&close.shared, "episode_b");
    discord::inflight::save_inflight_state(&b_row).unwrap();
    actual_fresh_exit(b_actor.clone(), owner, ProviderKind::Claude, true).await;
    let ExitDecision::PolicyClose(b_proof) = seal_exit(&b_actor, false, false) else {
        panic!("positive proof required");
    };
    let generation = close.shared.restart.current_generation;
    let b_key = TurnKey::new(channel, b_row.user_msg_id, generation)
        .with_episode_nonce(b_row.turn_nonce.as_deref());
    let provider = ProviderKind::Claude;
    close
        .fin
        .register_start(b_key, provider, RelayOwnerKind::None, &close.shared);
    let b_close = (&b_proof, MessageId::new(b_row.current_msg_id));
    let consumed = consume(
        &close.shared,
        &close.fin,
        &b_actor,
        &b_row,
        b_close,
        shown(),
    );
    assert!(consumed.await, "B settles its own close");
    b_row
}

/// A's tail stopped before it ran and B's close displaced the witness: A's second attempt resumes
/// from its exact ledger ticket alone, skips the channel tail behind B, and leaves B as it is.
#[tokio::test(flavor = "current_thread")]
async fn s2f_displaced_witness_resumes_from_the_ledger_ticket() {
    let close = Close::open(5_342_006, |_| {}).await;
    let acked = close.acked().await;
    let intent = close.intent(acked.clone());
    panic_once_at(&intent.request.trace.probe(), Stage::BeforeFinalize);
    assert!(matches!(
        close.submit(intent).await,
        PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerPanic)
    ));
    let b_row = settle_next_episode(&close, 4).await;
    let queued = close.other_turn_and_queue().await;
    let (slot, active) = (close.slot().await, close.active());
    let mut events = close.events();
    let intent = close.intent(acked);
    let probe = intent.request.trace.probe();
    let outcome = close.submit(intent).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    assert_eq!(probe.stages(), [Stage::BeforeFinalize], "no mailbox step");
    assert_eq!(read(&probe, &TAIL), [0, 1, 1, 1]);
    assert_eq!(probe.tail(), SUCCESSOR, "B claimed after A's commit");
    assert_eq!(
        close.slot().await.map(|s| Arc::as_ptr(&s)),
        slot.map(|s| Arc::as_ptr(&s))
    );
    assert_eq!(close.active(), active);
    assert_eq!(close.queued().await, queued);
    assert_eq!(close.released(&mut events), 0);
    assert!(close.durable().is_none(), "B's receipt went with B's close");
    assert_ne!(b_row.turn_nonce, close.row.turn_nonce);
    assert!(ticket_released(&probe));
}

/// The actor that sealed A's commit was purged before the tail ran: the tail claim meets the
/// closed actor, A finalizes from the seal, and the successor actor is untouched.
#[tokio::test(flavor = "current_thread")]
async fn s2f_purged_actor_finalizes_from_the_seal() {
    let close = Close::open(5_342_007, |_| {}).await;
    let acked = close.acked().await;
    let intent = close.intent(acked.clone());
    let first = intent.request.trace.probe();
    first.lose(Loss::MailboxReply);
    panic_once_at(&first, Stage::BeforeFinalize);
    assert!(matches!(
        close.submit(intent).await,
        PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerPanic)
    ));
    let channel = close.channel();
    assert!(matches!(
        close.shared.mailboxes.remove_idle_entry(channel).await,
        crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome::Removed
    ));
    let successor = start_successor(&close.shared, channel, close.row.user_msg_id + 5).await;
    let queued = close.other_turn_and_queue().await;
    let mut events = close.events();
    let intent = close.intent(acked);
    let probe = intent.request.trace.probe();
    let outcome = close.submit(intent).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    assert_eq!(
        probe.tail(),
        Some(PolicyTailDecision::Skipped(PolicyTailSkip::ActorClosed))
    );
    assert_eq!(
        read(&probe, &TAIL),
        [0, 1, 1, 1],
        "the seal, not a mailbox reading"
    );
    let slot = close.slot().await.unwrap();
    assert!(Arc::ptr_eq(&slot, &successor) && !successor.cancelled.load(Ordering::Relaxed));
    assert_eq!(close.queued().await, queued);
    assert_eq!(close.active(), 2);
    assert_eq!(close.released(&mut events), 0);
    assert!(close.durable().is_none());
    assert!(ticket_released(&probe));
}

/// A finalized close whose ACK alone was lost, its witness displaced by B: the re-ask confirms the
/// release receipt-only and runs no tail, queue exit or release again.
#[tokio::test(flavor = "current_thread")]
async fn s2f_ack_loss_after_the_tail_is_a_duplicate_without_a_tail() {
    let close = Close::open(5_342_008, |_| {}).await;
    let acked = close.acked().await;
    let intent = close.intent(acked.clone());
    intent.request.trace.probe().lose(Loss::Ack);
    let mut events = close.events();
    assert!(matches!(
        close.submit(intent).await,
        PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerAck)
    ));
    assert_eq!(close.released(&mut events), 1);
    settle_next_episode(&close, 4).await;
    let queued = close.other_turn_and_queue().await;
    let (slot, active) = (close.slot().await, close.active());
    let _ = close.released(&mut events);
    let intent = close.intent(acked);
    let probe = intent.request.trace.probe();
    let outcome = close.submit(intent).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::DuplicateConfirmed),
        "{outcome:?}"
    );
    assert_eq!(probe.stages(), [Stage::BeforeClaim], "the receipt-only arm");
    assert_eq!(read(&probe, &TAIL[..3]), [0, 0, 0]);
    assert_eq!(probe.tail(), None, "no tail ran");
    assert_eq!(
        close.slot().await.map(|s| Arc::as_ptr(&s)),
        slot.map(|s| Arc::as_ptr(&s))
    );
    assert_eq!(close.active(), active);
    assert_eq!(close.queued().await, queued);
    assert_eq!(close.released(&mut events), 0);
}

/// An exact release of A's own token through the finalizer.
async fn finish_exactly(close: &Close, user_msg_id: u64, nonce: Option<String>) {
    let generation = close.shared.restart.current_generation;
    let key = TurnKey::new(close.channel(), user_msg_id, generation);
    let outcome = close
        .fin
        .submit_terminal_with_episode_nonce(
            key,
            close.shared.provider.clone(),
            TerminalEvent::Complete,
            FinalizeContext::bridge(),
            nonce,
            close.shared.clone(),
        )
        .await;
    assert!(
        matches!(
            outcome,
            FinalizeOutcome::Finalized {
                removed_token: Some(_),
                ..
            }
        ),
        "the exact terminal releases its own token"
    );
}

/// A finalized after a finish that named no episode, then B released exactly: the fence refuses A,
/// yet A's late close on the purged actor keeps its receipt, as A's own release was never exact.
#[tokio::test(flavor = "current_thread")]
async fn s2f_finalized_without_an_exact_release_never_reconciles() {
    for wide in ["finish_turn", "hard_stop", "finish_cancelled_turn"] {
        let close = Close::open(5_342_009, |_| {}).await;
        let acked = close.acked().await;
        let stale = close.intent(acked);
        let (channel, provider) = (close.channel(), close.shared.provider.clone());
        let mailbox = close.shared.mailbox_peek(channel).unwrap();
        let removed = match wide {
            "finish_turn" => discord::mailbox_finish_turn(&close.shared, &provider, channel).await,
            "hard_stop" => mailbox.hard_stop().await,
            _ => {
                close.actor.cancelled.store(true, Ordering::Relaxed);
                discord::mailbox_finish::mailbox_finish_cancelled_turn(&close.shared, channel).await
            }
        };
        assert!(
            removed
                .removed_token
                .is_some_and(|t| Arc::ptr_eq(&t, &close.actor)),
            "{wide}"
        );
        let generation = close.shared.restart.current_generation;
        let key = TurnKey::new(channel, close.row.user_msg_id, generation)
            .with_episode_nonce(close.row.turn_nonce.as_deref());
        let finalized = close
            .fin
            .submit_terminal(
                key,
                provider.clone(),
                TerminalEvent::Complete,
                FinalizeContext::bridge(),
                close.shared.clone(),
            )
            .await;
        assert!(
            matches!(
                finalized,
                FinalizeOutcome::Finalized {
                    removed_token: None,
                    ..
                }
            ),
            "{wide}: the terminal finds no token"
        );
        // B starts and releases exactly on the same actor, which the registry then purges.
        let b_token = start_successor(&close.shared, channel, close.row.user_msg_id + 6).await;
        let b_key = TurnKey::new(channel, close.row.user_msg_id + 6, generation)
            .with_episode_nonce(b_token.turn_nonce());
        close
            .fin
            .register_start(b_key, provider.clone(), RelayOwnerKind::None, &close.shared);
        let b_nonce = b_token.turn_nonce().map(str::to_owned);
        finish_exactly(&close, close.row.user_msg_id + 6, b_nonce).await;
        assert!(matches!(
            close.shared.mailboxes.remove_idle_entry(channel).await,
            crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome::Removed
        ));
        let successor = start_successor(&close.shared, channel, close.row.user_msg_id + 7).await;
        let queued = close.other_turn_and_queue().await;
        let (bytes, active) = (close.bytes(), close.active());
        let mut events = close.events();
        let outcome = close
            .fin
            .submit_policy_close(stale, close.shared.clone())
            .await;
        assert!(
            matches!(
                outcome,
                PolicyCloseOutcome::Retained(PolicyRetain::ActorClosed)
            ),
            "{wide}: {outcome:?}"
        );
        assert_eq!(
            close.bytes(),
            bytes,
            "{wide}: the receipt row keeps its bytes"
        );
        let slot = close.slot().await.unwrap();
        assert!(Arc::ptr_eq(&slot, &successor), "{wide}");
        assert!(!successor.cancelled.load(Ordering::Relaxed), "{wide}");
        assert_eq!(close.queued().await, queued, "{wide}");
        assert_eq!(close.active(), active, "{wide}");
        assert_eq!(close.released(&mut events), 0, "{wide}");
    }
}

/// The receipt-only arm reads a missing row as already cleared and nothing else: a row that is
/// malformed, a directory or of an unknown provider keeps the turn held with its bytes.
#[tokio::test(flavor = "current_thread")]
async fn s2f_only_a_missing_receipt_row_reconciles() {
    type Fault = fn(&Close, &mut InflightTurnState);
    let cases: [(&str, Fault, Option<PolicyClear>); 4] = [
        (
            "missing",
            |close, _| std::fs::remove_file(close.path()).unwrap(),
            None,
        ),
        (
            "malformed",
            |close, _| std::fs::write(close.path(), b"{\"provider\":").unwrap(),
            Some(PolicyClear::Io),
        ),
        (
            "directory",
            |close, _| {
                std::fs::remove_file(close.path()).unwrap();
                std::fs::create_dir(close.path()).unwrap();
            },
            Some(PolicyClear::Io),
        ),
        (
            "provider",
            |_, acked| acked.provider = "retired-provider".into(),
            Some(PolicyClear::Io),
        ),
    ];
    for (case, fault, retained) in cases {
        let close = Close::open(5_342_010, |_| {}).await;
        let mut acked = close.acked().await;
        finish_exactly(&close, close.row.user_msg_id, close.row.turn_nonce.clone()).await;
        fault(&close, &mut acked);
        let before = std::fs::read(close.path()).ok();
        let outcome = close.submit(close.intent(acked)).await;
        match retained {
            None => assert!(
                matches!(outcome, PolicyCloseOutcome::DuplicateConfirmed),
                "{case}: {outcome:?}"
            ),
            Some(clear) => {
                assert!(
                    matches!(&outcome, PolicyCloseOutcome::Retained(PolicyRetain::Clear(c)) if *c == clear),
                    "{case}: {outcome:?}"
                );
                assert_eq!(
                    std::fs::read(close.path()).ok(),
                    before,
                    "{case}: bytes kept"
                );
            }
        }
        if case == "directory" {
            std::fs::remove_dir(close.path()).unwrap();
        }
    }
}
