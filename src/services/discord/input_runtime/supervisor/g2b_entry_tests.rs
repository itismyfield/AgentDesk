//! Adoption and wake commands use the same reserved supervisor and final runtime probe.

use super::*;
use crate::services::discord::input_runtime::supervisor::command::WakeCause;
use crate::services::discord::input_runtime::supervisor::external::{self, ExternalReceipt};
use crate::services::discord::input_runtime::supervisor::receipt::Deferred;
use crate::services::tui_input::rows::receipt_identity::ReceiptIdentity;
use tokio::sync::{mpsc as mailbox, oneshot};

struct HandoffMove {
    inner: MoveFake,
    channel: u64,
}

impl Host for HandoffMove {
    fn collect(&mut self, ledger: &Ledger) -> io::Result<Vec<Input>> {
        let mut inputs = self.inner.collect(ledger)?;
        for input in &mut inputs {
            input.payload = json!({
                "text": format!("input {}", input.key),
                "receipt_identity": ReceiptIdentity::new(
                    input.key, vec![input.key], 7, self.channel, self.channel
                ).unwrap(),
            });
        }
        Ok(inputs)
    }
    fn evidence(&mut self, input: &Input) -> io::Result<MoveEvidence> {
        self.inner.evidence(input)
    }
    fn delete(&mut self, phase: DeletePhase) -> io::Result<()> {
        self.inner.delete(phase)
    }
    fn start_actor(&mut self) -> io::Result<()> {
        self.inner.start_actor()
    }
    fn reconcile(&mut self, key: u64, row: &Row) -> io::Result<(bool, Composer)> {
        self.inner.reconcile(key, row)
    }
    fn enqueue(&mut self, key: u64, row: &Row) -> io::Result<EnqueueOutcome> {
        self.inner.enqueue(key, row)
    }
    fn notice(&mut self, key: Option<u64>, reason: &'static str) -> io::Result<()> {
        self.inner.notice(key, reason)
    }
}

struct HandoffFake {
    inner: Fake,
    probe: mapping::Probe,
    buffered: Option<(Option<u64>, &'static str)>,
}

impl Ports for HandoffFake {
    type Clear = ClearFake;
    type Move = HandoffMove;
    fn source_policy<'a>(
        &'a mut self,
        source: &'a super::super::super::source::Source,
    ) -> Step<'a, bool> {
        self.inner.source_policy(source)
    }
    fn mapping(&self) -> mapping::Probe {
        self.probe.clone()
    }
    fn freeze(&mut self, closing: Arc<Closing>) -> Step<'_, Result<(), Failure>> {
        self.inner.freeze(closing)
    }
    fn clear(&mut self) -> Step<'_, Option<(ClearFake, OwnedMutexGuard<()>)>> {
        self.inner.clear()
    }
    fn transition(&mut self, closing: Arc<Closing>) -> io::Result<HandoffMove> {
        let mut host = self.inner.transition(closing)?;
        if let Some((key, reason)) = self.buffered.take() {
            Host::notice(&mut host, key, reason)?;
        }
        Ok(HandoffMove {
            inner: host,
            channel: self.inner.channel,
        })
    }
    fn take_notices(host: &mut HandoffMove) -> Vec<(Option<u64>, &'static str)> {
        Fake::take_notices(&mut host.inner)
    }
    fn notice(&mut self, text: String) -> Step<'_, bool> {
        self.inner.notice(text)
    }
}

fn handoff(rig: &Rig, reserved: Reserved, inputs: Vec<u64>) -> Supervisor<HandoffFake> {
    let config = Config {
        provider: ProviderKind::Claude,
        channel: rig.channel,
        root: rig.root.clone(),
        request: Request::Ledger,
        refusal: None,
        binding: rig.binding.clone(),
    };
    let ports = HandoffFake {
        inner: Fake {
            channel: rig.channel,
            world: rig.world.clone(),
            registry: rig.registry,
            inputs,
            mailbox: ChannelMailboxRegistry::default(),
        },
        probe: mapping::Probe::default(),
        buffered: None,
    };
    Supervisor::start(reserved, config, ports).unwrap()
}

fn reserved(rig: &Rig) -> Reserved {
    rig.registry
        .reserve_for_test(&ProviderKind::Claude, rig.channel, &rig.root)
        .unwrap()
}

fn transition_effects(world: &World) -> Vec<&'static str> {
    world
        .effects
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|effect| *effect != "subscribe")
        .collect()
}

fn runtime(provider: ProviderKind) -> Arc<crate::services::discord::SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = provider;
    shared
}

async fn eventually(done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the supervisor reached the asserted boundary");
}

async fn adopt(commands: &mailbox::Sender<SupervisorCmd>, channel: u64) -> Result<(), Refused> {
    let (ack, response) = oneshot::channel();
    let sent = commands.send(SupervisorCmd::Adopt { channel, ack }).await;
    assert!(sent.is_ok(), "adopted supervisor keeps receiver alive");
    let response = tokio::time::timeout(Duration::from_secs(10), response)
        .await
        .expect("adoption reply");
    assert!(response.is_ok(), "adopted supervisor keeps receiver alive");
    response.unwrap()
}

fn task(
    mut supervisor: Supervisor<HandoffFake>,
    screen: Arc<Screen>,
    expected: Vec<ProviderKind>,
    views: oneshot::Receiver<Vec<Arc<crate::services::discord::SharedData>>>,
    commands: mailbox::Receiver<SupervisorCmd>,
) -> tokio::task::JoinHandle<(Supervisor<HandoffFake>, bool)> {
    tokio::spawn(async move {
        let returned = supervisor
            .serve(DriveFake(screen), &expected, views, commands)
            .await;
        (supervisor, returned)
    })
}

#[tokio::test]
async fn b_adopt_preserves_reservation_identity_and_is_not_receipt_admission() {
    let rig = Rig::new(6_326_301);
    let permit = rig.gate.admit().unwrap();
    let mut reservation = reserved(&rig);
    let closing = reservation.registration.closing(&rig.gate).unwrap();
    let slot = reservation.registration.slot.as_mut().unwrap();
    slot.get()
        .unwrap()
        .append_entry(
            &Entry::Received {
                key: 41,
                input: json!({"text":"kept", "receipt_identity":
                    crate::services::tui_input::rows::receipt_identity::ReceiptIdentity::new(41, vec![41], 7, rig.channel, rig.channel).unwrap()}),
            },
            &[],
        )
        .unwrap();
    let before = wal_bytes(&rig.root, rig.channel);
    let before_opens = opens();
    let mut supervisor = handoff(&rig, reservation, vec![]);
    supervisor.prepare_handoff();
    for (channel, expected) in [
        (rig.channel + 1, Err(Refused::Mismatch)),
        (rig.channel, Ok(())),
        (rig.channel, Err(Refused::Duplicate)),
    ] {
        let (ack, response) = oneshot::channel();
        supervisor
            .input_command(SupervisorCmd::Adopt { channel, ack }, false)
            .await;
        assert_eq!(response.await.unwrap(), expected);
    }
    assert!(Arc::ptr_eq(supervisor.gate.as_ref().unwrap(), &rig.gate));
    assert!(Arc::ptr_eq(
        &supervisor.registration.closing(&rig.gate).unwrap(),
        &closing
    ));
    permit.validate(&ProviderKind::Claude, rig.channel).unwrap();
    assert_eq!(
        supervisor
            .slot()
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .row(41)
            .unwrap()
            .input["text"],
        json!("kept")
    );
    assert_eq!(
        opens(),
        before_opens,
        "adoption reuses the already-open reserved slot"
    );
    assert_eq!(rig.gate.mode(), Mode::Closing);
    assert!(
        !supervisor.admission_open(),
        "the ACK proves task adoption only"
    );
    let (reply, response) = oneshot::channel();
    let source = external::source(rig.channel, 7, "new input", None, "adopt-ack").unwrap();
    supervisor
        .input_command(
            SupervisorCmd::SubmitExternal {
                source: Box::new(source),
                reply,
            },
            true,
        )
        .await;
    assert_eq!(
        response.await.unwrap(),
        ExternalReceipt::Deferred(Deferred::Closed)
    );
    assert_eq!(
        wal_bytes(&rig.root, rig.channel),
        before,
        "ACK creates no receipt"
    );
    assert!(rig.world.log().is_empty(), "ACK runs no boot stage");
    assert!(supervisor.release());
    drop(permit);
    drop(rig);

    let rig = Rig::new(6_326_302);
    let mut supervisor = handoff(&rig, reserved(&rig), vec![]);
    rig.registry
        .entries
        .lock()
        .unwrap()
        .remove(&("claude".into(), rig.channel));
    let (ack, response) = oneshot::channel();
    supervisor
        .input_command(
            SupervisorCmd::Adopt {
                channel: rig.channel,
                ack,
            },
            false,
        )
        .await;
    assert_eq!(
        response.await.unwrap(),
        Err(Refused::Poisoned),
        "a missing reservation cannot ACK adoption"
    );
    assert!(supervisor.release());
}

#[tokio::test]
async fn b_adopt_requires_all_provider_mapping_views_and_both_edge_directions() {
    for (index, case) in ["missing", "duplicate", "foreign", "key", "value"]
        .into_iter()
        .enumerate()
    {
        let rig = Rig::new(6_326_310 + index as u64);
        let claude = rig.world.runtime.shared.clone();
        let codex = runtime(ProviderKind::Codex);
        let mut views = vec![claude.clone(), codex.clone()];
        let edge = match case {
            "missing" => {
                views.pop();
                None
            }
            "duplicate" => {
                views[1] = claude;
                None
            }
            "foreign" => {
                views[1] = runtime(ProviderKind::Gemini);
                None
            }
            "key" => Some((rig.channel, rig.channel + 50)),
            _ => Some((rig.channel + 50, rig.channel)),
        };
        if let Some((parent, thread)) = edge {
            codex
                .dispatch
                .thread_parents
                .insert(ChannelId::new(parent), ChannelId::new(thread));
        }
        let supervisor = handoff(&rig, reserved(&rig), vec![41]);
        let probe = supervisor.ports.probe.clone();
        let (supply, supplied) = oneshot::channel();
        let (commands, received) = mailbox::channel(4);
        let screen = Arc::new(Screen::default());
        let run = task(
            supervisor,
            screen.clone(),
            vec![ProviderKind::Claude, ProviderKind::Codex],
            supplied,
            received,
        );
        eventually(|| {
            rig.health()
                .iter()
                .any(|line| line.contains("reason=runtime_view_pending"))
        })
        .await;
        assert_eq!(probe.check(rig.channel), mapping::Check::Pending);
        assert_eq!(adopt(&commands, rig.channel).await, Ok(()));
        assert!(
            rig.world.log().is_empty(),
            "{case}: ACK cannot start boot before final views"
        );
        assert_eq!(rig.gate.mode(), Mode::Closing);
        assert!(supply.send(views).is_ok());
        eventually(|| {
            rig.health().iter().any(|line| {
                line.contains(if edge.is_some() {
                    "reason=mapped_thread"
                } else {
                    "reason=mapping_unavailable"
                })
            })
        })
        .await;
        assert!(screen.sent.lock().unwrap().is_empty());
        assert!(
            !rig.root.join("input_ledger").exists(),
            "{case}: no ledger effect after invalid mapping view"
        );
        if let Some((parent, thread)) = edge {
            let cause = rig
                .registry
                .latched(&("claude".into(), rig.channel))
                .unwrap();
            assert_eq!(
                cause,
                HoldCause::MappingPresent(
                    mapping::Edge {
                        writer: "codex".into(),
                        parent,
                        thread
                    },
                    "after_freeze"
                )
            );
            assert_eq!(rig.gate.mode(), Mode::Held);
        } else {
            assert_eq!(probe.check(rig.channel), mapping::Check::Unavailable);
            assert_ne!(rig.gate.mode(), Mode::LegacyOpen);
        }
        assert_eq!(adopt(&commands, rig.channel).await, Err(Refused::Duplicate));
        assert!(!run.is_finished(), "Held retains its adopted task");
        drop(commands);
        let (supervisor, returned) = run.await.unwrap();
        assert!(!returned);
        assert!(supervisor.release());
    }
}

#[tokio::test]
async fn b_mapping_present_latches_after_durable_move_across_wake_clear_and_receipts() {
    let rig = Rig::new(6_326_320);
    push(&rig, "n1", pending("next", &rig.root.join("next.jsonl")));
    let codex = runtime(ProviderKind::Codex);
    let supervisor = handoff(&rig, reserved(&rig), vec![41]);
    let (supply, supplied) = oneshot::channel();
    let (commands, received) = mailbox::channel(8);
    let screen = Arc::new(Screen::default());
    let run = task(
        supervisor,
        screen.clone(),
        vec![ProviderKind::Claude, ProviderKind::Codex],
        supplied,
        received,
    );
    assert_eq!(adopt(&commands, rig.channel).await, Ok(()));
    assert!(
        supply
            .send(vec![rig.world.runtime.shared.clone(), codex.clone()])
            .is_ok()
    );
    eventually(|| rig.gate.mode() == Mode::LedgerOpen).await;
    assert_eq!(state(&rig, 41), RowState::Received);
    let moved = rows(&rig.root, rig.channel);
    assert!(
        moved.boundary_since(1),
        "Move durably committed its staged row"
    );
    assert!(moved.receipt_identity(41).is_some());
    assert_eq!(rig.world.effects("delete"), 4);
    assert_eq!(rig.world.effects("actor"), 1);
    let before = wal_bytes(&rig.root, rig.channel);
    let effects = transition_effects(&rig.world);
    let subscriptions = rig.world.effects("subscribe");
    codex
        .dispatch
        .thread_parents
        .insert(ChannelId::new(rig.channel), ChannelId::new(rig.channel + 1));
    commands.send(SupervisorCmd::Clear).await.unwrap();
    assert_eq!(adopt(&commands, rig.channel).await, Err(Refused::Duplicate));
    let first = rig
        .registry
        .latched(&("claude".into(), rig.channel))
        .expect("the Clear effect boundary detects mapping before any clear work");
    assert_eq!(
        first,
        HoldCause::MappingPresent(
            mapping::Edge {
                writer: "codex".into(),
                parent: rig.channel,
                thread: rig.channel + 1,
            },
            "before_clear",
        )
    );
    assert_eq!(transition_effects(&rig.world), effects);
    codex.dispatch.thread_parents.clear();
    for cause in [WakeCause::Binding, WakeCause::Retry] {
        commands.send(SupervisorCmd::Wake { cause }).await.unwrap();
    }
    commands.send(SupervisorCmd::Clear).await.unwrap();
    let (reply, response) = oneshot::channel();
    let source = external::source(rig.channel, 7, "held receipt", None, "mapped").unwrap();
    commands
        .send(SupervisorCmd::SubmitExternal {
            source: Box::new(source),
            reply,
        })
        .await
        .unwrap();
    assert_eq!(
        response.await.unwrap(),
        ExternalReceipt::Deferred(Deferred::Closed)
    );
    assert_eq!(
        rig.registry.latched(&("claude".into(), rig.channel)),
        Some(first)
    );
    assert_eq!(rig.gate.mode(), Mode::Held);
    assert_eq!(rig.world.effects("subscribe"), subscriptions + 2);
    assert_eq!(
        wal_bytes(&rig.root, rig.channel),
        before,
        "completed durable Move is retained"
    );
    assert_eq!(
        transition_effects(&rig.world),
        effects,
        "no clear, Move or deletion after the latch"
    );
    assert!(screen.sent.lock().unwrap().is_empty());
    drop(commands);
    let (supervisor, returned) = run.await.unwrap();
    assert!(!returned);
    assert!(supervisor.release());
}

#[tokio::test]
async fn b_failed_buffered_notice_remains_pending_until_success() {
    let rig = Rig::new(6_326_330);
    let mut supervisor = handoff(&rig, reserved(&rig), vec![41]);
    supervisor.ports.buffered = Some((Some(41), "move_unbound"));
    assert!(supervisor.ports.probe.adopt(
        &[ProviderKind::Claude],
        vec![rig.world.runtime.shared.clone()]
    ));
    rig.world.failing_delivery.store(true, Ordering::SeqCst);
    assert_eq!(supervisor.boot().await, Landing::Admitted);
    assert_eq!(
        rig.world.undelivered.load(Ordering::SeqCst),
        1,
        "the buffered Notice reached the delivery port"
    );
    let before = wal_bytes(&rig.root, rig.channel);
    let screen = Arc::new(Screen::default());
    let (mut drive, replay) = supervisor.start_drive(DriveFake(screen)).unwrap();
    supervisor
        .tick(&mut drive, replay, std::time::Instant::now())
        .await;
    assert_eq!(
        rig.world.undelivered.load(Ordering::SeqCst),
        2,
        "a consumed host buffer must still retry"
    );
    rig.world.failing_delivery.store(false, Ordering::SeqCst);
    for _ in 0..3 {
        supervisor
            .tick(&mut drive, Ok(vec![]), std::time::Instant::now())
            .await;
    }
    let notices = rig.world.notices();
    assert_eq!(
        notices,
        [reconcile::notice(Some(41), "move_unbound")],
        "only success deduplicates the original buffered Notice"
    );
    assert_eq!(wal_bytes(&rig.root, rig.channel), before);
    assert_eq!(state(&rig, 41), RowState::Received);
    supervisor.stop_drive(drive);
    assert!(supervisor.release());
}

#[tokio::test]
async fn b_mailbox_send_before_task_takeover_never_acks_completion() {
    let rig = Rig::new(6_326_340);
    let mut supervisor = handoff(&rig, reserved(&rig), vec![]);
    let (_supply, supplied) = oneshot::channel();
    let (commands, received) = mailbox::channel(1);
    let (ack, response) = oneshot::channel();
    commands
        .send(SupervisorCmd::Adopt {
            channel: rig.channel,
            ack,
        })
        .await
        .unwrap();
    let expected = [ProviderKind::Claude];
    let interrupted = supervisor.serve(
        DriveFake(Arc::new(Screen::default())),
        &expected,
        supplied,
        received,
    );
    drop(interrupted);
    assert!(
        response.await.is_err(),
        "mailbox send is not supervisor task takeover"
    );
    assert!(!supervisor.adopted);
    assert!(!supervisor.admission_open());
    assert_eq!(rig.gate.mode(), Mode::Closing);
    assert!(rig.world.log().is_empty());
    assert!(!rig.root.join("input_ledger").exists());
    assert!(supervisor.release());
}

#[tokio::test]
async fn b_dropped_adopt_reply_keeps_the_adopted_held_task_alive() {
    let rig = Rig::new(6_326_341);
    let supervisor = handoff(&rig, reserved(&rig), vec![]);
    let (supply, supplied) = oneshot::channel();
    let (commands, received) = mailbox::channel(4);
    let run = task(
        supervisor,
        Arc::new(Screen::default()),
        vec![ProviderKind::Claude, ProviderKind::Codex],
        supplied,
        received,
    );
    let (ack, response) = oneshot::channel();
    commands
        .send(SupervisorCmd::Adopt {
            channel: rig.channel,
            ack,
        })
        .await
        .unwrap();
    drop(response);
    assert_eq!(
        adopt(&commands, rig.channel).await,
        Err(Refused::Duplicate),
        "lost reply does not revoke adoption"
    );
    drop(supply);
    eventually(|| {
        rig.health()
            .iter()
            .any(|line| line.contains("reason=mapping_unavailable"))
    })
    .await;
    assert!(!run.is_finished());
    assert!(!rig.root.join("input_ledger").exists());
    assert_eq!(adopt(&commands, rig.channel).await, Err(Refused::Duplicate));
    drop(commands);
    let (supervisor, returned) = run.await.unwrap();
    assert!(!returned);
    assert!(!supervisor.admission_open());
    assert!(supervisor.release());
}

#[tokio::test]
async fn b_binding_wake_rereads_log_and_progresses_before_the_natural_tick() {
    let rig = Rig::new(6_326_350);
    push(&rig, "n1", pending("next", &rig.root.join("next.jsonl")));
    let supervisor = handoff(&rig, reserved(&rig), vec![41]);
    let (supply, supplied) = oneshot::channel();
    let (commands, received) = mailbox::channel(8);
    let screen = Arc::new(Screen::default());
    let run = task(
        supervisor,
        screen.clone(),
        vec![ProviderKind::Claude],
        supplied,
        received,
    );
    assert_eq!(adopt(&commands, rig.channel).await, Ok(()));
    assert!(supply.send(vec![rig.world.runtime.shared.clone()]).is_ok());
    eventually(|| rig.gate.mode() == Mode::LedgerOpen).await;
    assert_eq!(adopt(&commands, rig.channel).await, Err(Refused::Duplicate));
    assert_eq!(state(&rig, 41), RowState::Received);
    tokio::time::pause();
    let frozen_at = tokio::time::Instant::now();
    let next = idle_transcript(&rig, "next", false);
    *screen.record.lock().unwrap() = Some(next.path.clone());
    let seq = rig.binding.seq() + 1;
    rig.binding.events.lock().unwrap().push(BindingEvent {
        seq,
        channel_id: rig.channel,
        provider: ShadowProvider::Claude,
        tmux_session: tmux(rig.channel),
        execution_nonce: "n1".into(),
        record: source(None, &next),
        committed_at: chrono::Utc::now(),
    });
    for _ in 0..3 {
        commands
            .send(SupervisorCmd::Wake {
                cause: WakeCause::Binding,
            })
            .await
            .unwrap();
    }
    drop(commands);
    let (mut supervisor, returned) = run.await.unwrap();
    assert_eq!(
        tokio::time::Instant::now(),
        frozen_at,
        "no natural tick elapsed"
    );
    assert_eq!(
        supervisor.cursor().unwrap().seq(),
        seq,
        "explicit Wake rereads the unpublished binding log entry"
    );
    assert_eq!(screen.sent.lock().unwrap().len(), 1);
    assert_eq!(state(&rig, 41), RowState::Running);
    assert!(!returned);
    assert!(supervisor.release());
}

#[tokio::test]
async fn b_ready_views_cannot_boot_before_adopt_then_open_the_ledger() {
    let rig = Rig::new(6_326_390);
    let mut supervisor = handoff(&rig, reserved(&rig), vec![]);
    let screen = Arc::new(Screen::default());
    let (supply, supplied) = oneshot::channel();
    let (commands, received) = mailbox::channel(4);
    let expected = [ProviderKind::Claude, ProviderKind::Codex];
    assert!(
        supply
            .send(vec![
                rig.world.runtime.shared.clone(),
                runtime(ProviderKind::Codex),
            ])
            .is_ok()
    );
    tokio::time::pause();
    let returned = {
        let run = supervisor.serve(DriveFake(screen.clone()), &expected, supplied, received);
        tokio::pin!(run);
        // Poll ready views first so premature boot reaches its synchronous port observations.
        tokio::select! {
            biased;
            _ = &mut run => panic!("the task retains the unadopted reservation"),
            () = tokio::task::yield_now() => {},
        }
        tokio::time::advance(3 * TICK).await;
        tokio::select! {
            biased;
            _ = &mut run => panic!("the task retains the unadopted reservation"),
            () = tokio::task::yield_now() => {},
        }
        assert!(
            rig.world.log().is_empty(),
            "ready views cannot run any boot stage before Adopt"
        );
        assert!(!rig.root.join("input_ledger").exists());
        assert_eq!(rig.gate.mode(), Mode::Closing);
        assert!(
            rig.health()
                .contains(&HoldCause::RuntimeViewPending.health("claude", rig.channel))
        );
        assert!(screen.sent.lock().unwrap().is_empty());
        tokio::time::resume();
        tokio::select! {
            _ = &mut run => panic!("the adopted task reaches LedgerOpen"),
            result = adopt(&commands, rig.channel) => assert_eq!(result, Ok(())),
        }
        tokio::select! {
            _ = &mut run => panic!("the adopted task reaches LedgerOpen"),
            () = eventually(|| rig.gate.mode() == Mode::LedgerOpen) => {},
        }
        assert!(rig.root.join("input_ledger").exists());
        assert_eq!(rig.world.effects("freeze"), 1);
        drop(commands);
        run.await
    };
    assert!(!returned);
    assert!(supervisor.release());
}

#[tokio::test]
async fn c2_actual_serve_publishes_existing_client_only_after_adopt_and_final_views() {
    use crate::services::discord::input_runtime::intake::{Access, lookup};
    use crate::services::tui_input::rows::receipt_identity::Responsibility;

    let rig = Rig::new(6_326_391);
    let mut supervisor = handoff(&rig, reserved(&rig), vec![41]);
    let overflow = supervisor.pending_overflow();
    let (supply, supplied) = oneshot::channel();
    let (commands, received) = mailbox::channel(8);
    let client = supervisor.bind_intake(commands.clone()).unwrap();
    let screen = Arc::new(Screen::default());
    let mut run = task(
        supervisor,
        screen.clone(),
        vec![ProviderKind::Claude, ProviderKind::Codex],
        supplied,
        received,
    );
    let before = overflow.generation();
    assert!(matches!(
        lookup(rig.registry, &ProviderKind::Claude, rig.channel),
        Access::Deferred(Deferred::Closed)
    ));
    assert_eq!(
        client.pending(vec![41]).await.unwrap_err(),
        Deferred::Closed
    );
    assert_eq!(overflow.generation(), before + 2);
    assert_eq!(adopt(&commands, rig.channel).await, Ok(()));
    let before = overflow.generation();
    assert!(matches!(
        lookup(rig.registry, &ProviderKind::Claude, rig.channel),
        Access::Deferred(Deferred::Closed)
    ));
    assert_eq!(
        client.pending(vec![41]).await.unwrap_err(),
        Deferred::Closed
    );
    assert_eq!(overflow.generation(), before + 2);
    assert!(rig.world.log().is_empty());
    assert!(!rig.root.join("input_ledger").exists());
    assert!(
        supply
            .send(vec![
                rig.world.runtime.shared.clone(),
                runtime(ProviderKind::Codex),
            ])
            .is_ok()
    );
    let ready = tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            lookup(rig.registry, &ProviderKind::Claude, rig.channel),
            Access::Ready(_)
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if ready.is_err() {
        run.abort();
        let _ = (&mut run).await;
    }
    assert!(
        ready.is_ok(),
        "actual serve must publish the existing client after Adopt and final views"
    );
    assert!(matches!(
        lookup(rig.registry, &ProviderKind::Claude, rig.channel),
        Access::Ready(ref published) if Arc::ptr_eq(published, &client)
    ));
    eventually(|| rig.gate.mode() == Mode::LedgerOpen).await;
    let pending = client.pending(vec![41]).await;
    assert!(
        pending.is_ok(),
        "the published client uses the live mailbox"
    );
    assert_eq!(pending.unwrap().sources, vec![41]);
    let identity = ReceiptIdentity::new(41, vec![41], 7, rig.channel, rig.channel).unwrap();
    assert!(matches!(
        client.lookup_responsibility(identity).await,
        Ok(Responsibility::Known { key: 41, .. })
    ));
    assert!(client.close().await.is_ok());
    assert!(screen.sent.lock().unwrap().is_empty());
    drop(client);
    drop(commands);
    let (supervisor, returned) = run.await.unwrap();
    assert!(!returned);
    assert!(!supervisor.admission_open());
    assert_eq!(supervisor.pending_snapshot().sources, vec![41]);
    assert!(supervisor.release());
}
