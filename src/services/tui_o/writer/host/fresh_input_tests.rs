use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::writer::actor::codex_submission;
use crate::services::tui_o::writer::suspension::Suspension;
use crate::services::tui_o::writer::transition_port::{self, TransitionPort};

struct Hosted {
    readiness: Arc<Readiness>,
    channel: u64,
    suspension: Suspension,
    submission: codex_submission::Port,
    _submission_receiver: codex_submission::Receiver,
    transition: TransitionPort,
    transition_receiver: transition_port::Receiver,
    _resumed: watch::Sender<bool>,
    _stopping: watch::Sender<bool>,
    _unsettled: watch::Sender<Option<usize>>,
    _owing: Owing,
    _claim: ReadinessClaim,
}

impl Hosted {
    fn new() -> Self {
        let readiness = Arc::new(Readiness::default());
        let channel = 8_700_041;
        let claim = readiness.claim(channel).unwrap();
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let (resumed, resumed_rx) = watch::channel(true);
        let (stopping, stopping_rx) = watch::channel(false);
        let (unsettled, unsettled_rx) = watch::channel(Some(0));
        let owing = Owing::default();
        let owed = OwedView {
            published: owing.published.subscribe(),
            demand: owing.demand.clone(),
        };
        readiness.track(
            channel,
            Arc::clone(&gate),
            (resumed_rx, unsettled_rx, owed, stopping_rx.clone()),
        );
        let suspension = Suspension::default();
        let (submission, submission_receiver) = codex_submission::Port::new(claim.generation);
        let submission = submission.with_suspension(suspension.clone());
        let (transition, transition_receiver) = TransitionPort::new(
            claim.generation,
            gate,
            Arc::new(std::time::SystemTime::now),
            stopping_rx,
        );
        let transition = transition.with_suspension(suspension.clone());
        {
            let mut live = locked(&readiness.live);
            let live = live.get_mut(&channel).unwrap();
            live.submission = Some(submission.clone());
            live.transition = Some(transition.clone());
        }
        readiness.set(channel, true);
        assert!(readiness.accepts(channel));
        assert!(transition.is_open(), "receiver must remain alive");
        Self {
            readiness,
            channel,
            suspension,
            submission,
            _submission_receiver: submission_receiver,
            transition,
            transition_receiver,
            _resumed: resumed,
            _stopping: stopping,
            _unsettled: unsettled,
            _owing: owing,
            _claim: claim,
        }
    }

    fn suspend(&self) {
        self.suspension.extend(
            "new-test-incarnation".into(),
            [SourceId {
                session_id: "retained-old".into(),
                path: "/unit/unopened-old.jsonl".into(),
                dev: 7,
                ino: 11,
            }],
        );
        assert!(
            self.readiness.accepts(self.channel),
            "ordinary readiness stays true; only strong consumers close"
        );
        assert!(self.transition.is_open());
        assert!(!self.suspension.is_empty());
    }
}

fn reply() -> transition_port::Reply {
    transition_port::Reply::refused("prebuilt-boundary-reply", transition_port::Refused::Busy)
}

#[test]
fn suspended_live_blocks_submission_and_prebuilt_transition_reply_callbacks() {
    let hosted = Hosted::new();
    let mut submission_calls = 0;
    hosted
        .readiness
        .with_submission(hosted.channel, |port| {
            assert!(port.same_run(&hosted.submission));
            submission_calls += 1;
            Ok(())
        })
        .unwrap();
    let prebuilt = reply();
    let mut transition_calls = 0;
    hosted
        .readiness
        .with_transition_reply(hosted.channel, &hosted.transition, prebuilt.clone(), |_| {
            transition_calls += 1;
        })
        .unwrap();
    assert_eq!((submission_calls, transition_calls), (1, 1));
    hosted.suspend();
    let submission = hosted.readiness.with_submission(hosted.channel, |_| {
        submission_calls += 1;
        Ok(())
    });
    assert_eq!(submission, Err("suspended_sources"));
    let transition = hosted.readiness.with_transition_reply(
        hosted.channel,
        &hosted.transition,
        prebuilt,
        |_| {
            transition_calls += 1;
        },
    );
    assert_eq!(transition, Err("suspended_sources"));
    assert_eq!(
        (submission_calls, transition_calls),
        (1, 1),
        "no strong callback may consume the earlier reply after suspension"
    );
}

fn operation(channel: u64) -> transition_port::Operation {
    transition_port::Operation::Inspect(transition_port::Request {
        channel,
        cutoff: 1,
        job_id: "prebuilt-request".into(),
        operator: "unit".into(),
        at: chrono::Utc::now(),
        reason: "strong boundary test".into(),
        termination: None,
    })
}

fn context() -> transition_port::Context {
    transition_port::Context {
        tmux: "unit-no-live-target".into(),
        nonce: "unit-no-native-proof".into(),
        home_epoch: None,
        lock: Arc::new(tokio::sync::Mutex::new(())),
        permit: None,
    }
}

#[tokio::test]
async fn queued_transition_reply_is_rejected_after_shared_selection_changes() {
    let mut hosted = Hosted::new();
    let transition = hosted.transition.clone();
    let request = transition.request(operation(hosted.channel), context());
    tokio::pin!(request);
    assert!(
        matches!(futures::poll!(&mut request), std::task::Poll::Pending),
        "live receiver must accept the pre-freeze request"
    );
    let envelope = hosted
        .transition_receiver
        .try_next()
        .unwrap()
        .expect("actual queued request");
    hosted.suspend();
    envelope.reply.send(reply()).unwrap();
    assert_eq!(request.await.unwrap_err(), "suspended_sources");
    let refused = hosted
        .transition
        .request(operation(hosted.channel), context());
    tokio::pin!(refused);
    assert!(
        matches!(futures::poll!(&mut refused), std::task::Poll::Ready(Err(ref reason)) if reason == "suspended_sources"),
        "post-freeze transition request must reject before waiting for an actor reply"
    );
    assert!(
        hosted.transition_receiver.try_next().unwrap().is_none(),
        "post-freeze request must not enter the mailbox"
    );
}

#[tokio::test]
async fn submission_request_rejects_suspension_with_a_live_receiver() {
    let hosted = Hosted::new();
    let port = hosted.submission.clone();
    let execution =
        crate::services::tui_prompt_dedupe::codex_verified::provenance::ExecutionProofRef {
            owner_runtime_root: "unit".into(),
            tmux_session: "unit-no-live-target".into(),
            execution_nonce: "unit-no-native-proof".into(),
            proof_seq: 1,
            source: SourceId {
                session_id: "unit".into(),
                path: "/unit/unopened.jsonl".into(),
                dev: 1,
                ino: 2,
            },
        };
    let pending = port.request(codex_submission::Operation::ReadBack(Box::new(
        execution.clone(),
    )));
    tokio::pin!(pending);
    assert!(
        matches!(futures::poll!(&mut pending), std::task::Poll::Pending),
        "the existing receiver must be alive before suspension"
    );
    hosted.suspend();
    let refused = hosted
        .submission
        .request(codex_submission::Operation::ReadBack(Box::new(execution)));
    tokio::pin!(refused);
    assert!(
        matches!(futures::poll!(&mut refused), std::task::Poll::Ready(Err(ref reason)) if reason == "suspended_sources"),
        "post-freeze submission request must reject before waiting for an actor reply"
    );
}

#[test]
fn initial_fresh_refuses_a_closed_actor_even_when_ready_stays_true() {
    static CHANNEL: AtomicU64 = AtomicU64::new(8_701_000);
    let channel = CHANNEL.fetch_add(1, Ordering::SeqCst);
    let readiness = process_readiness();
    let claim = readiness.claim(channel).unwrap();
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let (resumed, resumed_rx) = watch::channel(true);
    let (_stopping, stopping_rx) = watch::channel(false);
    let (_unsettled, unsettled_rx) = watch::channel(Some(0));
    let owing = Owing::default();
    let owed = OwedView {
        published: owing.published.subscribe(),
        demand: owing.demand.clone(),
    };
    readiness.track(channel, gate, (resumed_rx, unsettled_rx, owed, stopping_rx));
    let (submission, _receiver) = codex_submission::Port::new(claim.generation);
    locked(&readiness.live)
        .get_mut(&channel)
        .unwrap()
        .submission = Some(submission);
    readiness.set(channel, true);
    assert!(readiness.accepts(channel));
    assert_eq!(initial_fresh_refusal(channel), None);
    drop(resumed);
    assert!(
        readiness.is_ready(channel),
        "the sticky ready bit must remain set for this negative"
    );
    assert!(
        *locked(&readiness.live)
            .get(&channel)
            .unwrap()
            .resumed
            .borrow()
    );
    assert_eq!(
        initial_fresh_refusal(channel),
        Some("actor_closed"),
        "fresh admission must read the live actor sender rather than trust sticky ready"
    );
}
