//! Codex-ready fixtures: the production binding-log actor behind a ready transition port, with
//! a Codex observation ready, blocked or carrying a retained backlog.

#![cfg(any(test, feature = "test-support"))]

use std::time::SystemTime;

use super::transition::*;
use super::*;
use crate::services::tui_o::alarm::{AlarmHealth, AlarmRouter};
use crate::services::tui_o::store::rotation::{Rotation, Successor};
use crate::services::tui_o::store::spool::{Cursor, source_key};
use crate::services::tui_o::writer::actor::{Owing, spawn_projecting, transition_hooks as hooks};
use crate::services::tui_o::writer::transition_port::{Refused, Reply, TransitionPort};
use crate::test_support::wait::{WAIT_CAP, await_within};

pub const NONCE: &str = "11111111111111111111111111111111";

/// Runs the production binding-log actor behind a ready port while the callback sends its sweep.
pub async fn with_ready_codex<T, F, Fut>(source: SourceId, channel: u64, run: F) -> T
where
    F: FnOnce(Arc<crate::services::tui_o::writer::host::Readiness>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    with_ready_provider(source, channel, ShadowProvider::Codex, Arc::default(), run).await
}

/// Both actual actors share one readiness registry while their provider sweeps run.
pub async fn with_ready_codex_and_claude<T, F, Fut>(source: SourceId, run: F) -> T
where
    F: FnOnce(Arc<crate::services::tui_o::writer::host::Readiness>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    with_ready_provider(source.clone(), 7, ShadowProvider::Codex, Arc::default(), |ready| async move {
        with_ready_provider(source, 8, ShadowProvider::Claude, ready, run).await
    }).await
}

async fn with_ready_provider<T, F, Fut>(
    source: SourceId,
    channel: u64,
    provider: ShadowProvider,
    ready: Arc<crate::services::tui_o::writer::host::Readiness>,
    run: F,
) -> T
where
    F: FnOnce(Arc<crate::services::tui_o::writer::host::Readiness>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    use crate::services::tui_o::writer::binding::ChannelBindingLog;
    use sha2::{Digest, Sha256};

    let harness = Harness::build_channel(channel, |_| match provider {
        ShadowProvider::Codex => vec![InitSource {
            source_id: source.clone(),
            delivery_start: 0,
            prefix_hash: hex::encode(Sha256::digest([])),
        }],
        ShadowProvider::Claude => Vec::new(),
        ShadowProvider::Antigravity => unreachable!("pane fixture"),
    });
    harness.gate.acquired();
    let (host, stopping) = watch::channel(false);
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready_state) = watch::channel(false);
    let (port, receiver) = TransitionPort::new(
        1,
        Arc::clone(&harness.gate),
        Arc::new(SystemTime::now),
        stopping.clone(),
    );
    let port = port.with_provider(provider);
    let projections = (stopped, resumed, watch::channel(None).0, Owing::default());
    let config = WriterConfig {
        enabled: true,
        ..Default::default()
    };
    let task = transition_port::spawning(receiver, || {
        spawn_projecting(
            &config,
            None,
            harness.writer(),
            provider,
            Arc::new(ChannelBindingLog::new(channel, provider)),
            projections,
        )
    })
    .expect("the actual Codex actor started");
    for _ in 0..20 {
        if *ready_state.borrow() {
            break;
        }
        polls(1).await;
    }
    assert!(
        *ready_state.borrow(),
        "the actor restored from the actual P5 log"
    );
    polls(3).await;
    let _hosted = ready.host_port_for_test(channel, &harness.gate, port, (ready_state, stopping));
    let directory = harness
        ._runtime
        .path()
        .join("o_store")
        .join(channel.to_string());
    let before = files(&directory);
    let result = run(ready).await;
    assert!(
        !task.is_finished(),
        "the real provider actor stayed available"
    );
    assert_eq!(
        files(&directory),
        before,
        "observation did not write the O store"
    );
    assert!(
        harness.port.posts().is_empty(),
        "the native fixture had no output"
    );
    halt(stop, task).await;
    drop(host);
    result
}

/// A live mailbox whose actor deliberately does not poll, with a shared suspension witness.
pub async fn with_blocked_codex<T, F, Fut>(source: SourceId, run: F) -> T
where
    F: FnOnce(Arc<crate::services::tui_o::writer::host::Readiness>, Box<dyn Fn() -> bool>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let harness = Harness::build_channel(7, |_| Vec::new());
    harness.gate.acquired();
    let (_stopping, stopped) = watch::channel(false);
    let (_resumed, resumed) = watch::channel(true);
    let suspension = crate::services::tui_o::writer::suspension::Suspension::default();
    let (port, receiver) = TransitionPort::new(
        1,
        harness.gate.clone(),
        Arc::new(SystemTime::now),
        stopped.clone(),
    );
    let port = port
        .with_provider(ShadowProvider::Codex)
        .with_suspension(suspension.clone());
    let ready = Arc::new(crate::services::tui_o::writer::host::Readiness::default());
    let _hosted = ready.host_port_for_test(7, &harness.gate, port, (resumed, stopped));
    let receiver = Arc::new(std::sync::Mutex::new(receiver));
    let mailbox = receiver.clone();
    let result = run(
        ready,
        Box::new(move || {
            let Ok(Some(envelope)) = mailbox.lock().unwrap().try_next() else {
                return false;
            };
            suspension.extend("fresh".into(), [source.clone()]);
            let id = &envelope.operation.request_ref().job_id;
            envelope
                .reply
                .send(Reply::refused(id, Refused::CodexDeferred))
                .unwrap();
            true
        }),
    )
    .await;
    drop(receiver);
    result
}

/// The real permission-aware log must never be entered with current source authority held.
/// The assertion turns accidental lock reentry into a finite test failure before delegating.
struct GuardedProductionBindingLog {
    inner: crate::services::tui_o::writer::binding::ChannelBindingLog,
}

impl crate::services::tui_o::writer::binding::BindingEvents for GuardedProductionBindingLog {
    fn binding_events_since(
        &self,
        channel: u64,
        after: u64,
    ) -> Result<Vec<crate::services::tui_o::writer::binding::BindingEvent>, String> {
        assert!(
            !hooks::snapshot_authority_held(),
            "[ba:legacy_reentry] permission-aware production binding read reentered source authority"
        );
        crate::services::tui_o::writer::binding::BindingEvents::binding_events_since(
            &self.inner,
            channel,
            after,
        )
    }

    fn subscribe(&self, channel: u64) -> watch::Receiver<u64> {
        crate::services::tui_o::writer::binding::BindingEvents::subscribe(&self.inner, channel)
    }
}

struct RetentionAlarms {
    observed: Alarms,
    router: Arc<AlarmRouter>,
    counted: watch::Sender<()>,
    retained: Arc<Mutex<Vec<(usize, usize)>>>,
}

impl AlarmSink for RetentionAlarms {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.observed.raise(channel, alarm.clone());
        self.router.raise(channel, alarm);
    }
    fn reconcile_retained_readers(&self, channel: u64, reading: usize, retained: &[String]) {
        self.retained
            .lock()
            .unwrap()
            .push((reading, retained.len()));
        self.router
            .reconcile_retained_readers(channel, reading, retained);
    }
    fn reconcile_reader_count(&self, channel: u64, count: usize) {
        self.observed.reconcile_reader_count(channel, count);
        self.router.reconcile_reader_count(channel, count);
        self.counted.send_replace(());
    }
}

/// Restores a real pre-existing backlog, preserving its persisted rotation and delivery permission.
pub async fn with_ready_codex_backlog<T, F, Fut>(
    sources: Vec<SourceId>,
    pane: String,
    configured: bool,
    callback_budget: std::time::Duration,
    run: F,
) -> (T, Vec<Cursor>, Option<usize>, Arc<AlarmHealth>)
where
    F: FnOnce(Arc<crate::services::tui_o::writer::host::Readiness>, BacklogProbe) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    use crate::services::tui_o::writer::binding::ChannelBindingLog;
    use sha2::{Digest, Sha256};
    let originals: Vec<_> = sources
        .iter()
        .map(|source| std::fs::read(&source.path).unwrap())
        .collect();
    let harness = Harness::build_channel(7, |_| {
        sources
            .iter()
            .map(|source| InitSource {
                source_id: source.clone(),
                delivery_start: 0,
                prefix_hash: hex::encode(Sha256::digest([])),
            })
            .collect()
    });
    let mut rotation = Rotation::default();
    for (index, pair) in sources.windows(2).enumerate() {
        rotation.successors.insert(
            source_key(&pair[0]),
            Successor {
                source: pair[1].clone(),
                seq: Some(index as u64 + 2),
                tmux_session: Some(pane.clone()),
                drain_to: Some(std::fs::metadata(&pair[0].path).unwrap().len()),
                proof: None,
            },
        );
    }
    harness.channel().write_rotation(&rotation).unwrap();
    harness.gate.acquired();
    let ready = Arc::new(crate::services::tui_o::writer::host::Readiness::default());
    let (host, stopping) = watch::channel(false);
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready_state) = watch::channel(false);
    let (started, wall) = (tokio::time::Instant::now(), SystemTime::now());
    // Probe timestamps follow the same paused clock as the actor's quiet period.
    let wall = Arc::new(move || wall + started.elapsed());
    let (port, receiver) =
        TransitionPort::new(1, Arc::clone(&harness.gate), wall, stopping.clone());
    let port = port.with_provider(ShadowProvider::Codex);
    let health = Arc::new(AlarmHealth::default());
    let (counted, mut counts) = watch::channel(());
    let retained = Arc::new(Mutex::new(Vec::new()));
    let alarms = RetentionAlarms {
        observed: harness.alarms.clone(),
        router: Arc::new(AlarmRouter::new(None, None, health.clone())),
        counted,
        retained: retained.clone(),
    };
    let writer = ChannelWriter::new(
        harness.channel(),
        harness.gate.clone(),
        harness.port.clone(),
        harness.lease.clone(),
        alarms,
    );
    let task = transition_port::spawning(receiver, || {
        spawn_projecting(
            &WriterConfig {
                enabled: true,
                ..Default::default()
            },
            configured.then_some(ReaderConfig { dormant: false }),
            writer,
            ShadowProvider::Codex,
            Arc::new(GuardedProductionBindingLog {
                inner: ChannelBindingLog::new(7, ShadowProvider::Codex),
            }),
            (stopped, resumed, watch::channel(None).0, Owing::default()),
        )
    })
    .expect("actual backlog actor started");
    await_within(
        async {
            loop {
                let captured = {
                    let store = harness.channel();
                    sources.iter().all(|source| {
                        store.cursor(source).is_some_and(|cursor| {
                            cursor.captured_through
                                == std::fs::metadata(&source.path).unwrap().len()
                        })
                    })
                };
                if *ready_state.borrow() && captured && harness.port.posts().len() == sources.len()
                {
                    break;
                }
                counts.changed().await.unwrap();
            }
        },
        WAIT_CAP,
    )
    .await;
    assert!(*ready_state.borrow());
    assert_eq!(
        harness.port.posts().len(),
        sources.len(),
        "actual native dialogue was posted before retention"
    );
    let _hosted = ready.host_port_for_test(7, &harness.gate, port, (ready_state, stopping));
    let probe = BacklogProbe {
        root: harness._runtime.path().to_owned(),
        sources: sources.clone(),
        counts: counts.clone(),
        health: health.clone(),
        alarms: harness.alarms.clone(),
        port: harness.port.clone(),
        retained,
    };
    let result = await_within(run(ready, probe), callback_budget).await;
    counts.borrow_and_update();
    for _ in 0..3 {
        await_within(counts.changed(), WAIT_CAP).await.unwrap();
    }
    assert!(!task.is_finished(), "retention actor remains available");
    let cursors = sources
        .iter()
        .map(|source| harness.channel().cursor(source).unwrap().clone())
        .collect();
    let reading = harness.alarms.readers();
    for (source, before) in sources.iter().zip(originals) {
        assert_eq!(
            std::fs::read(&source.path).unwrap()[..before.len()],
            before,
            "retention leaves existing native transcript bytes unchanged"
        );
    }
    stop.send(true).unwrap();
    await_within(task, WAIT_CAP).await.unwrap();
    drop(host);
    (result, cursors, reading, health)
}

/// Reads durable cursors and routed health while the actual backlog actor keeps running.
pub struct BacklogProbe {
    root: PathBuf,
    sources: Vec<SourceId>,
    counts: watch::Receiver<()>,
    pub health: Arc<AlarmHealth>,
    alarms: Alarms,
    port: Arc<FakePort>,
    pub retained: Arc<Mutex<Vec<(usize, usize)>>>,
}

impl BacklogProbe {
    pub fn cursors(&self) -> Vec<Cursor> {
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &self.root)
            .unwrap()
            .unwrap();
        let era = store.read_era().unwrap().unwrap();
        let channel = store.open_channel(&era, 7).unwrap().unwrap();
        self.sources
            .iter()
            .map(|s| channel.cursor(s).unwrap().clone())
            .collect()
    }

    pub async fn settled(&mut self, retired: usize) {
        await_within(
            async {
                while self.cursors().iter().filter(|c| c.retired).count() < retired {
                    self.counts.changed().await.unwrap();
                }
            },
            WAIT_CAP,
        )
        .await;
    }

    pub async fn regrown(&mut self, source: &SourceId) {
        await_within(
            async {
                loop {
                    if self.cursors().iter().any(|c| {
                        c.source == *source
                            && !c.retired
                            && c.captured_through == std::fs::metadata(&source.path).unwrap().len()
                    }) && self
                        .port
                        .posts()
                        .last()
                        .is_some_and(|p| p == "late after retirement")
                    {
                        break;
                    }
                    self.counts.changed().await.unwrap();
                }
            },
            WAIT_CAP,
        )
        .await;
        assert!(self.alarms.taken().iter().any(|alarm| matches!(alarm,
            WriterAlarm::RetiredSourceGrew { source: grown, .. } if grown == source)));
        assert!(
            self.health
                .current_at(std::time::Instant::now())
                .contains(&"tui_o:retired_source_grew:7".into())
        );
    }
}
