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

/// Inspect crosses the real mailbox; only Apply uses a synthetic job to test its closed gate.
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
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let (port, receiver) = TransitionPort::new(
        81,
        gate.clone(),
        Arc::new(std::time::SystemTime::now),
        stopped.clone(),
    );
    let port = port.with_suspension(suspension.clone());
    let mut actor = Actor {
        writer,
        deriver,
        owed: VecDeque::new(),
        verified: None,
        sources: Sources::new(channel, ShadowProvider::Codex, bindings.clone())
            .with_suspension(suspension),
        bindings,
        transitions: Transitions::new(Some(receiver), stopped.clone()),
        notice: None,
        waiting: Waiting::default(),
        clock: (Instant::now(), chrono::Utc::now()),
        scope_hold: None,
        boot_previous_nonce: None,
    };
    assert_eq!(actor.rescope(), !inspect);
    assert_eq!(
        actor.writer.suspension.nonce().as_deref(),
        Some(initial_nonce)
    );
    assert_eq!(actor.writer.suspension.is_empty(), inspect);
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
    let request = Request {
        channel,
        cutoff: actor.writer.store().binding_checkpoint().unwrap().unwrap(),
        job_id: "fresh-root-waiting".into(),
        operator: "fixture".into(),
        at: chrono::Utc::now(),
        reason: "finish an existing transition before selecting a new root".into(),
        termination: None,
    };
    let nonce;
    if inspect {
        // No source has been denied yet, so this is reachable through the production port.
        let pending = port.request(Operation::Inspect(request), context);
        tokio::pin!(pending);
        assert!(futures::poll!(&mut pending).is_pending());
        for _ in 0..3 {
            actor.poll_transition(true, &watch::channel(None).0).await;
            if matches!(
                actor.transitions.job.as_ref().map(|job| &job.stage),
                Some(Stage::Quiet(_))
            ) {
                break;
            }
        }
        let job = actor
            .transitions
            .job
            .as_mut()
            .expect("mailbox accepted Inspect");
        assert!(matches!(job.stage, Stage::Quiet(until) if until > now()));
        assert!(!job.apply);
        // Tap the actor reply, then forward it unchanged through the port's suspension check.
        let (reply, completed) = oneshot::channel();
        let port_reply = std::mem::replace(&mut job.reply, reply);
        nonce = next_root();
        assert!(
            actor.rescope(),
            "a read-only Inspect in Quiet must not delay a proven fresh root"
        );
        assert_eq!(actor.scope_hold, None);
        assert_eq!(
            actor.writer.suspension.nonce().as_deref(),
            Some(nonce.as_str())
        );
        assert!(!actor.writer.suspension.is_empty());
        actor.observe_waiting(false);
        held();
        for _ in 0..3 {
            actor.poll_transition(true, &watch::channel(None).0).await;
        }
        let reply = tokio::time::timeout(Duration::from_millis(100), completed)
            .await
            .expect("fresh suspension must answer the active Inspect in finite time")
            .expect("the actor must send an explicit refusal");
        assert_eq!(reply.refused, Some(Refused::CodexDeferred));
        assert!(actor.transitions.job.is_none());
        port_reply.send(reply).unwrap();
        assert_eq!(pending.await.unwrap_err(), "suspended_sources");
    } else {
        let (_, common) = actor
            .build_snapshot(&context, &gate, &BTreeMap::new())
            .unwrap();
        let (reply, completed) = oneshot::channel();
        // Codex mailbox Apply stays closed; a synthetic existing Apply isolates its hold guard.
        actor.transitions.job = Some(Job {
            request,
            apply: true,
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
        nonce = next_root();
        for _ in 0..3 {
            assert!(
                !actor.rescope(),
                "an Apply job must still hold the new root"
            );
            assert_eq!(actor.scope_hold, Some("new_root_waiting"));
            actor.observe_waiting(false);
            held();
        }
        let mut job = actor.transitions.job.take().unwrap();
        for _ in 0..4 {
            if actor.step(&mut job, &watch::channel(None).0).await {
                break;
            }
            test_hooks::settle_workers();
        }
        actor.finish(job);
        assert!(completed.await.unwrap().refused.is_none());
        assert!(!actor.transitions.has_job());
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
