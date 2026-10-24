//! A request already queued when selection freezes is refused by the actual actor poll.

use super::*;
use crate::services::tui_o::store::STORE_DIR_NAME;
use crate::services::tui_o::writer::actor::codex_submission::scoped_tests::{fixture, tree};
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::transition_port::TransitionPort;
use std::task::Poll;

#[tokio::test]
async fn queued_transition_is_refused_when_selection_freezes_before_actor_poll() {
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for freeze in [false, true] {
        let (root, writer, floor) = fixture();
        let before = tree(&root.path().join(STORE_DIR_NAME));
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let (_stop, stopped) = watch::channel(false);
        let (port, receiver) = TransitionPort::new(
            81,
            gate,
            Arc::new(std::time::SystemTime::now),
            stopped.clone(),
        );
        let operation = Operation::Inspect(Request {
            channel: 7,
            cutoff: 1,
            job_id: "queued-before-selection".into(),
            operator: "test".into(),
            at: chrono::Utc::now(),
            reason: "inspect queued boundary".into(),
            termination: None,
        });
        let context = Context {
            codex: None,
            tmux: "test-pane".into(),
            nonce: "old-execution".into(),
            lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        // Leave the port's selection empty: only Actor.poll_transition may reject this envelope.
        let request = port.request(operation, context);
        tokio::pin!(request);
        assert!(futures::poll!(&mut request).is_pending());
        let bindings = Arc::new(ChannelBindingLog::new(7, ShadowProvider::Claude));
        let mut actor = Actor {
            writer,
            deriver: UnitDeriver::new(7, ShadowProvider::Claude),
            owed: VecDeque::new(),
            verified: None,
            sources: Sources::new(7, ShadowProvider::Claude, bindings.clone()),
            bindings,
            transitions: Transitions::new(Some(receiver), stopped),
            notice: None,
            waiting: Waiting::default(),
            clock: (Instant::now(), chrono::Utc::now()),
            scope_hold: None,
            boot_previous_nonce: None,
        };
        if freeze {
            actor
                .writer
                .suspension
                .extend("successor".into(), [floor.checkpoint().0.clone()]);
        }
        // An unresolved ordinary actor leaves the mailbox queued; a scoped one must refuse now.
        actor.poll_transition(false, &watch::channel(None).0).await;
        match (freeze, futures::poll!(&mut request)) {
            (true, Poll::Ready(Ok(reply))) => {
                assert_eq!(reply.refused, Some(Refused::CodexDeferred));
                assert!(reply.hops.is_empty());
            }
            (false, Poll::Pending) => {}
            (freeze, other) => {
                panic!("scope frozen={freeze}: expected immediate scope refusal, got {other:?}")
            }
        }
        assert!(!actor.transitions.has_job());
        assert_eq!(tree(&root.path().join(STORE_DIR_NAME)), before);
    }
}

/// A preexisting operator job isolates the quiescence gate; Codex mailbox admission stays closed.
pub(crate) async fn exercise_scope_waiting_job_for_tests<P, L, A, B>(
    mut writer: ChannelWriter<P, L, A>,
    bindings: Arc<B>,
    tmux: &str,
    initial_nonce: &str,
    inspect: bool,
    next_root: impl FnOnce() -> String,
    held: impl Fn(),
) -> String
where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
    B: BindingEvents,
{
    let channel = writer.channel();
    let suspension = writer.suspension.clone();
    let deriver = super::super::fresh_resume::configured_deriver(
        channel,
        ShadowProvider::Codex,
        writer.store().ledger(),
    );
    let (_stop, stopped) = watch::channel(false);
    let mut actor = Actor {
        writer,
        deriver,
        owed: VecDeque::new(),
        verified: None,
        sources: Sources::new(channel, ShadowProvider::Codex, bindings.clone())
            .with_suspension(suspension),
        bindings,
        transitions: Transitions::new(None, stopped.clone()),
        notice: None,
        waiting: Waiting::default(),
        clock: (Instant::now(), chrono::Utc::now()),
        scope_hold: None,
        boot_previous_nonce: None,
    };
    assert!(
        actor.rescope(),
        "the first exact root must install its selection"
    );
    actor
        .sources
        .resume(&mut actor.writer, &mut actor.deriver, &mut actor.owed)
        .unwrap();
    for _ in 0..6 {
        actor.read_sources();
        actor.deliver_owed().await;
        actor.collect_settled();
        tokio::task::yield_now().await;
    }
    assert!(actor.owed.is_empty());
    assert!(!actor.deriver.has_unsealed());
    assert!(actor.sources.quiescent_for_rescope());
    let context = Context {
        codex: None,
        tmux: tmux.into(),
        nonce: initial_nonce.into(),
        lock: Arc::default(),
    };
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let (_, common) = actor
        .build_snapshot(&context, &gate, &BTreeMap::new())
        .unwrap();
    let (reply, completed) = oneshot::channel();
    actor.transitions.job = Some(Job {
        request: Request {
            channel,
            cutoff: actor.writer.store().binding_checkpoint().unwrap().unwrap(),
            job_id: "fresh-root-waiting".into(),
            operator: "fixture".into(),
            at: chrono::Utc::now(),
            reason: "finish an existing transition before selecting a new root".into(),
            termination: None,
        },
        // Apply is a synthetic pre-existing job; Codex mailbox Apply remains refused.
        apply: !inspect,
        automatic: None,
        scope: None,
        stopping: stopped,
        context,
        reply,
        gate,
        wall: Arc::new(std::time::SystemTime::now),
        common,
        first: Vec::new(),
        probes: BTreeMap::new(),
        queue: VecDeque::new(),
        stage: Stage::First,
        worker: None,
        deadline: now() + DEADLINE,
        hashed: 0,
        outcomes: BTreeMap::new(),
    });
    if inspect {
        let cursors: Vec<_> = actor.writer.store().cursors().cloned().collect();
        let cursor = cursors
            .into_iter()
            .find(|cursor| !actor.writer.suspension.contains(&cursor.source))
            .expect("the proven current source is readable");
        let key = source_key(&cursor.source);
        let token = actor.token(&cursor);
        let job = actor.transitions.job.as_mut().unwrap();
        job.probes.insert(
            key.clone(),
            Probe {
                token,
                earlier: None,
                later: None,
                last_dialogue: None,
            },
        );
        job.queue.push_back(key);
        // Reach Quiet through the production worker/probe state machine, without advancing time.
        for _ in 0..4 {
            let mut job = actor.transitions.job.take().unwrap();
            assert!(!actor.step(&mut job, &watch::channel(None).0).await);
            let quiet = matches!(job.stage, Stage::Quiet(until) if until > now());
            actor.transitions.job = Some(job);
            test_hooks::settle_workers();
            if quiet {
                break;
            }
        }
        let job = actor.transitions.job.as_ref().unwrap();
        assert!(matches!(job.stage, Stage::Quiet(until) if until > now()));
        assert!(!job.apply);
        assert!(job.probes.values().all(|probe| probe.earlier.is_some()));
    }
    let nonce = next_root();
    assert!(actor.transitions.has_job());
    if inspect {
        assert!(
            actor.rescope(),
            "a read-only Inspect in Quiet must not delay a proven fresh root"
        );
        assert_eq!(actor.scope_hold, None);
        assert_eq!(
            actor.writer.suspension.nonce().as_deref(),
            Some(nonce.as_str())
        );
        assert!(matches!(
            actor.transitions.job.as_ref().unwrap().stage,
            Stage::Quiet(_)
        ));
        held();
    } else {
        for _ in 0..3 {
            assert!(
                !actor.rescope(),
                "an Apply job must still hold the new root"
            );
            assert_eq!(actor.scope_hold, Some("new_root_waiting"));
            actor.observe_waiting(false);
            held();
        }
    }
    let mut job = actor.transitions.job.take().unwrap();
    if inspect {
        tokio::time::advance(RETIRE_QUIET).await;
    }
    for _ in 0..4 {
        if actor.step(&mut job, &watch::channel(None).0).await {
            break;
        }
        test_hooks::settle_workers();
    }
    actor.finish(job);
    assert!(completed.await.unwrap().refused.is_none());
    assert!(!actor.transitions.has_job());
    if !inspect {
        assert!(
            actor.rescope(),
            "finishing Apply must permit the exact new root"
        );
    }
    assert_eq!(actor.scope_hold, None);
    actor.observe_waiting(false);
    actor
        .sources
        .resume(&mut actor.writer, &mut actor.deriver, &mut actor.owed)
        .unwrap();
    for _ in 0..6 {
        actor.read_sources();
        actor.deliver_owed().await;
        actor.collect_settled();
        actor.observe_waiting(true);
        tokio::task::yield_now().await;
    }
    assert!(actor.owed.is_empty());
    assert!(!actor.writer.is_stopped());
    nonce
}
