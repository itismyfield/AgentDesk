use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use super::super::MailboxPurgeOutcome;
use super::super::recovery_custody::capability::ApplyCapability;
use super::super::recovery_custody::capability::Load;
use super::super::recovery_custody::custody_pg_tests::{Fx, NODE, channel, scoped, seal};
use super::super::recovery_custody::denial_index::{Denial, IndexState};
use super::super::recovery_custody::operation::{Stage, StageStatus};
use super::super::recovery_custody::seam::{self, Point};
use super::super::recovery_custody::{ArmStep, CustodyError};
use super::super::recovery_registry::RecoveryRegistry;
use super::super::recovery_registry::entry::{Begun, WorkerPermit};
use super::*;
use crate::db::o_channel_homes::HomeState;
use crate::services::cluster::channel_home_port::scoped_restore::{ExactWitness, RestoreScope};
use crate::services::discord::input_runtime::fence::{Failure, Gate, Mode, test_health};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::mailbox_unreachable_tests::closed_handle;
use crate::services::turn_orchestrator::scoped_claim::ScopedClaimRefusal;
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, InterventionMode, SourceMessageQueuedGeneration,
};

const EPOCH: i64 = 41;
const TOKEN: &str = "token-a";

fn id(channel_id: &str) -> u64 {
    channel_id.parse().expect("a numeric channel id")
}

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

fn human(id: u64) -> Intervention {
    let source = SourceMessageQueuedGeneration::user_instruction(MessageId::new(id), 5);
    Intervention {
        author_id: UserId::new(id),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 5,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![source],
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

fn proof(channel: ChannelId, message: u64) -> Installation {
    let nonce = format!("nonce-{channel}-{message}");
    Installation {
        channel,
        token: Arc::new(CancelToken::from_persisted_turn_nonce(Some(nonce))),
        episode: "episode-b3b4a2b1".into(),
        birth: "birth".into(),
        owner: UserId::new(7),
        message: MessageId::new(message),
        generation: 1,
        current: Arc::new(AtomicU64::new(1)),
    }
}

/// An install of `proof` and the receiver of its answer; the payload holds the only reply sender.
fn install(proof: &Installation) -> (Install, oneshot::Receiver<InstallAnswer>) {
    let (reply, answer) = oneshot::channel();
    let persistence = ctx();
    let proof = proof.clone();
    (
        Install {
            proof,
            persistence,
            reply,
        },
        answer,
    )
}

fn witness(attempt: &AttemptHandle) -> Option<RestoreStatus> {
    attempt
        .witness()
        .reader()
        .read()
        .map(|witness| witness.status)
}

/// A registry attempt on a channel, its scoped row's operation and one worker's `ActorInstall` arm
/// without its capability, which [`arm_install`] returns apart.
struct Arming {
    attempt: AttemptHandle,
    _worker: WorkerPermit,
    revoke: RevokeHandle,
    report: OutcomeReport,
    collector: tokio::task::JoinHandle<Result<i64, CustodyError>>,
    op: String,
}

async fn arm_install(fx: &Fx, registry: &RecoveryRegistry, ch: &str) -> (Arming, ApplyCapability) {
    fx.home(ch, NODE, EPOCH).await;
    let slot = registry.resolve(id(ch)).expect("a slot");
    let scope = RestoreScope {
        channel: id(ch),
        provider: "claude".into(),
        epoch: EPOCH,
        state: HomeState::Worker,
        holder: Some(NODE.into()),
        target: None,
        local: NODE.into(),
    };
    let Ok(Begun::Started(attempt)) = slot.begin(scope, 1) else {
        panic!("the first observation starts the attempt");
    };
    let (revision, op) = fx.reserve(scoped(ch, seal("q1", None, None), EPOCH)).await;
    let worker = attempt.worker().expect("a worker place");
    let step = ArmStep {
        channel_id: ch,
        expected_revision: revision,
        operation_id: &op,
        stage: Stage::ActorInstall,
        expected_after: seal("q2", None, None),
        grant: None,
    };
    let armed = worker.arm(&fx.custody, step).await.expect("armed");
    let arming = Arming {
        attempt,
        _worker: worker,
        revoke: armed.revoke,
        report: armed.report,
        collector: armed.collector,
        op,
    };
    (arming, armed.capability)
}

/// The carrier came back as `refusal` with its own arm and the same live payload, still queued.
fn back(
    sent: Result<(), NotSent>,
    refusal: Unsent,
    arming: &Arming,
    proof: &Installation,
    answer: &mut oneshot::Receiver<InstallAnswer>,
) -> Carrier<Install> {
    let Err(NotSent::Carrier(Unapplied {
        refusal: seen,
        carrier,
    })) = sent
    else {
        panic!("{refusal:?}: the carrier did not come back");
    };
    assert_eq!(seen, refusal);
    assert_eq!(carrier.arm_id(), arming.revoke.arm_id(), "{refusal:?}");
    let payload = carrier.payload().expect("its payload");
    let same = Arc::ptr_eq(&payload.proof.token, &proof.token);
    assert!(same, "{refusal:?}: another payload came back");
    let queued = Load {
        held: false,
        queued: 1,
        active: 0,
    };
    assert_eq!(arming.revoke.load(), queued, "{refusal:?}");
    let unanswered = matches!(answer.try_recv(), Err(TryRecvError::Empty));
    assert!(
        unanswered,
        "{refusal:?}: the install's reply was answered or dropped"
    );
    carrier
}

/// Unread and undecided bindings never take a plain install; a carried one comes back unchanged
/// from the binding, a gone mailbox, a closed actor, a fence and a revoked arm of the same exact
/// operation, each time as the same carrier with the same payload and one queued count.
#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b1_the_adapter_hands_every_refused_carrier_back_unchanged_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fx::create().await;
    let registry = RecoveryRegistry::isolated(8, 2);
    let (ch, fenced_ch) = (channel(951), channel(952));
    let chan = ChannelId::new(id(&ch));

    // A plain install reaches a mailbox only where no custody answers; a read goes on any binding.
    let unknown = Binding::Attached(IndexState::Unknown);
    for binding in [Binding::Unread, unknown, Binding::Unmanaged] {
        let (reply, mut answer) = oneshot::channel();
        let claim = ScopedMsg::Claim {
            proof: proof(chan, 1),
            persistence: ctx(),
            reply,
        };
        let sent = request(&closed_handle(), &binding, &fx.custody, Ask::Plain(claim)).await;
        match (&binding, sent) {
            (Binding::Unmanaged, Err(NotSent::Unreachable)) => {}
            (Binding::Unmanaged, _) => panic!("an unmanaged plain install was not sent"),
            (_, Err(NotSent::Refused(ScopedMsg::Claim { .. }))) => {
                let kept = matches!(answer.try_recv(), Err(TryRecvError::Empty));
                assert!(kept, "{binding:?}: the refused claim lost its reply");
            }
            (_, _) => panic!("{binding:?} let a plain install through as legacy"),
        }
        let (reply, _read) = oneshot::channel();
        let read = Ask::Plain(ScopedMsg::Pending { reply });
        let sent = request(&closed_handle(), &binding, &fx.custody, read).await;
        assert!(matches!(sent, Err(NotSent::Unreachable)), "{binding:?}");
    }

    let (arming, capability) = arm_install(&fx, &registry, &ch).await;
    let attached = registry.binding(&fx.custody, id(&ch)).await;
    let exact = IndexState::ExactOperation {
        operation_id: arming.op.clone(),
        stage: Stage::ActorInstall,
        arm_id: arming.revoke.arm_id().to_string(),
    };
    assert_eq!(attached, Binding::Attached(exact.clone()));
    let proof_a = proof(chan, 201);
    let (payload, mut answer) = install(&proof_a);
    let mut carrier = capability.carry(payload);
    let mailboxes = ChannelMailboxRegistry::default();
    let live = mailboxes.handle(chan);
    for (binding, refusal) in [
        (Binding::Unread, Unsent::Unread),
        (Binding::Unmanaged, Unsent::Unmanaged),
    ] {
        let sent = request(&live, &binding, &fx.custody, Ask::Carried(carrier)).await;
        carrier = back(sent, refusal, &arming, &proof_a, &mut answer);
    }
    let sent = request(
        &closed_handle(),
        &attached,
        &fx.custody,
        Ask::Carried(carrier),
    )
    .await;
    carrier = back(sent, Unsent::SendFailed, &arming, &proof_a, &mut answer);
    // A purge tombstones the live actor; its successor gets the same carrier back from the index.
    assert_eq!(
        mailboxes.remove_idle_entry(chan).await,
        MailboxPurgeOutcome::Removed
    );
    let sent = request(&live, &attached, &fx.custody, Ask::Carried(carrier)).await;
    carrier = back(sent, Unsent::Closed, &arming, &proof_a, &mut answer);
    let successor = mailboxes.handle(chan);
    assert!(
        !successor.same_actor(&live),
        "the purge left the same actor"
    );
    arming.revoke.revoke();
    let sent = request(&successor, &attached, &fx.custody, Ask::Carried(carrier)).await;
    let denied = Unsent::Denied(Denial(exact));
    carrier = back(sent, denied, &arming, &proof_a, &mut answer);
    assert_eq!(seam::effects(&ch), 0, "a refused carrier applied");
    assert_eq!(successor.installed_pending().await.map(|_| ()), None);

    // A fence refusal on its own channel's actor hands its carrier back the same way.
    let (fenced, capability) = arm_install(&fx, &registry, &fenced_ch).await;
    let fenced_chan = ChannelId::new(id(&fenced_ch));
    let gate = Gate::protect(ProviderKind::Claude, fenced_chan.get()).unwrap();
    let _health = test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();
    let proof_b = proof(fenced_chan, 211);
    let (payload, mut answer_b) = install(&proof_b);
    let carried = Ask::Carried(capability.carry(payload));
    let handle = mailboxes.handle(fenced_chan);
    let binding = registry.binding(&fx.custody, id(&fenced_ch)).await;
    let sent = request(&handle, &binding, &fx.custody, carried).await;
    let closing = Unsent::Fenced(Failure::Mode(Mode::Closing));
    let fenced_back = back(sent, closing, &fenced, &proof_b, &mut answer_b);

    // Given up, the payload returns and the arms count nothing more.
    assert!(carrier.into_payload().is_some() && fenced_back.into_payload().is_some());
    let idle = Load {
        held: false,
        queued: 0,
        active: 0,
    };
    assert_eq!(arming.revoke.load(), idle);
    fx.close().await;
}

/// What each seam on the apply path saw: its point, the arm's count and the thread it ran on.
type Seen = Arc<Mutex<Vec<(Point, Load, std::thread::ThreadId)>>>;

fn watch(ch: &str, arming: &Arming, seen: &Seen) {
    for point in [Point::Queued, Point::Active, Point::BeforeEffect] {
        let (revoke, seen) = (arming.revoke.clone(), seen.clone());
        seam::at(ch, point, move || {
            let thread = std::thread::current().id();
            seen.lock().unwrap().push((point, revoke.load(), thread));
        });
    }
}

/// On the actor task and on its `spawn_blocking` branch alike, the carrier moves whole into the
/// step: it counts as queued until the index entry turns it active, so the arm never reads idle,
/// and the one install result names the actor, the arm and the debts its pending record owes.
#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b1_a_carried_install_runs_on_either_branch_without_a_zero_count_gap_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fx::create().await;
    let registry = RecoveryRegistry::isolated(8, 2);
    let mailboxes = ChannelMailboxRegistry::default();
    let test_thread = std::thread::current().id();
    for (n, blocking) in [false, true].into_iter().enumerate() {
        let ch = channel(953 + n as u32);
        let chan = ChannelId::new(id(&ch));
        let (arming, capability) = arm_install(&fx, &registry, &ch).await;
        let handle = mailboxes.handle(chan);
        for message in [201, 202] {
            assert!(handle.enqueue(human(message), ctx()).await.enqueued);
        }
        let gate = blocking.then(|| Gate::protect(ProviderKind::Claude, chan.get()).unwrap());
        let _health = gate.as_ref().map(test_health::Clear::new);
        let seen = Seen::default();
        watch(&ch, &arming, &seen);
        let proof = proof(chan, 201);
        let (payload, answer) = install(&proof);
        let carried = Ask::Carried(capability.carry(payload));
        let binding = registry.binding(&fx.custody, id(&ch)).await;
        let sent = request(&handle, &binding, &fx.custody, carried).await;
        assert!(sent.is_ok(), "blocking={blocking}: the carrier came back");
        let Ok(InstallAnswer::Installed(installed)) = answer.await else {
            panic!("blocking={blocking}: the install did not answer as installed");
        };

        let seen = std::mem::take(&mut *seen.lock().unwrap());
        let count = |queued, active| Load {
            held: false,
            queued,
            active,
        };
        let loads: Vec<_> = seen.iter().map(|(point, load, _)| (*point, load)).collect();
        let expected = [
            (Point::Queued, &count(1, 0)),
            (Point::Active, &count(0, 1)),
            (Point::BeforeEffect, &count(0, 1)),
        ];
        assert_eq!(loads, expected, "blocking={blocking}");
        let off_task = seen[1].2 != test_thread;
        assert_eq!(off_task, blocking, "the step ran on the other branch");
        assert_eq!(arming.revoke.load(), count(0, 0));

        let pending = handle.installed_pending().await.expect("a pending record");
        let owed: Vec<_> = (pending.exits.iter())
            .map(|exit| exit.debt.event_id.clone())
            .collect();
        assert_eq!(owed.len(), 1, "blocking={blocking}");
        assert!(installed.same_actor(&handle));
        let named = (installed.operation_id(), installed.arm_id());
        assert_eq!(named, (arming.op.as_str(), arming.revoke.arm_id()));
        assert_eq!(installed.event_ids(), owed.as_slice());

        let Arming {
            attempt,
            report,
            collector,
            ..
        } = arming;
        let recorded = installed.record(&attempt, report, seal("q2", None, None));
        assert!(recorded.is_ok(), "blocking={blocking}: not recorded");
        collector.await.unwrap().expect("custody recorded");
        let op = fx
            .record(&ch)
            .await
            .active_operation
            .expect("its operation");
        let applied = (op.applied.contains(&Stage::ActorInstall), op.status);
        assert_eq!(
            applied,
            (true, StageStatus::KnownOutcome),
            "blocking={blocking}"
        );
        assert_eq!(witness(&attempt), Some(RestoreStatus::Installed));
    }
    fx.close().await;
}

/// An install whose answer was lost is read back from the same actor's pending record, with the
/// same debts each time and no new one, and from no other actor, whose own refused claim answers
/// with its reason; only its own attempt records it, and another attempt's refusal hands the
/// result and the report back with nothing written.
#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b1_a_lost_install_answer_is_recovered_from_the_same_actor_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fx::create().await;
    let registry = RecoveryRegistry::isolated(8, 2);
    let mailboxes = ChannelMailboxRegistry::default();
    let (ch, other_ch) = (channel(955), channel(956));
    let chan = ChannelId::new(id(&ch));
    let (arming, capability) = arm_install(&fx, &registry, &ch).await;
    let (other, other_capability) = arm_install(&fx, &registry, &other_ch).await;
    let handle = mailboxes.handle(chan);
    for message in [201, 202] {
        assert!(handle.enqueue(human(message), ctx()).await.enqueued);
    }
    let proof = proof(chan, 201);
    let (payload, answer) = install(&proof);
    // The caller leaves after the claim ran and before its answer was sent.
    seam::at(&ch, Point::AfterEffect, move || drop(answer));
    let carried = Ask::Carried(capability.carry(payload));
    let binding = registry.binding(&fx.custody, id(&ch)).await;
    let sent = request(&handle, &binding, &fx.custody, carried).await;
    assert!(sent.is_ok(), "the carrier came back");

    let owed = |pending: Option<InstalledPending>| {
        let pending = pending.expect("a pending record");
        let exits = pending.exits.iter();
        exits
            .map(|exit| exit.debt.event_id.clone())
            .collect::<Vec<_>>()
    };
    let before = owed(handle.installed_pending().await);
    assert_eq!(before.len(), 1);
    let revoke = &arming.revoke;
    let first = recover(&handle, &proof, revoke).await.expect("recovered");
    let again = recover(&handle, &proof, revoke)
        .await
        .expect("recovered again");
    assert_eq!(first.event_ids(), before.as_slice());
    assert_eq!(again.event_ids(), before.as_slice());
    assert_eq!(
        owed(handle.installed_pending().await),
        before,
        "a debt was issued again"
    );
    assert!(first.same_actor(&handle));

    // Another channel's actor refuses the claim under its own arm and answers with that refusal.
    let other_handle = mailboxes.handle(ChannelId::new(id(&other_ch)));
    let (payload, refused) = install(&proof);
    let carried = Ask::Carried(other_capability.carry(payload));
    let binding = registry.binding(&fx.custody, id(&other_ch)).await;
    let sent = request(&other_handle, &binding, &fx.custody, carried).await;
    assert!(sent.is_ok(), "the other carrier came back");
    let Ok(InstallAnswer::Claim(ScopedClaimOutcome::Refused(why))) = refused.await else {
        panic!("the foreign claim did not answer with its refusal");
    };
    assert!(matches!(why, ScopedClaimRefusal::WrongChannel), "{why:?}");
    let elsewhere = recover(&other_handle, &proof, revoke).await;
    assert!(
        elsewhere.is_none(),
        "another actor answered for the install"
    );

    let Arming {
        attempt,
        report,
        collector,
        ..
    } = arming;
    let after = seal("q2", None, None);
    let raw = fx.raw(&ch).await;
    let refused = first.record(&other.attempt, report, after.clone());
    let Err((NotRecorded::OtherAttempt, first, report)) = refused else {
        panic!("another attempt recorded the install");
    };
    assert_eq!(witness(&other.attempt), Some(RestoreStatus::Restoring));
    assert_eq!(witness(&attempt), Some(RestoreStatus::Restoring));
    assert_eq!(fx.raw(&ch).await, raw, "a refused record wrote the row");
    assert!(first.record(&attempt, report, after).is_ok());
    collector.await.unwrap().expect("custody recorded");
    assert_eq!(witness(&attempt), Some(RestoreStatus::Installed));
    let op = fx
        .record(&ch)
        .await
        .active_operation
        .expect("its operation");
    assert_eq!(op.status, StageStatus::KnownOutcome);
    fx.close().await;
}
