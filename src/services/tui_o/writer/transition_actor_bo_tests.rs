//! The Codex observation branch reaches the real actor mailbox without acquiring Apply rights.

use super::*;
use crate::services::tmux_common::TmuxSourceAuthority;
use crate::services::tui_o::alarm::{AlarmHealth, AlarmRouter};
use crate::services::tui_o::writer::codex_snapshot::{
    Coverage, HostPath, Inputs, Observation, Observer, Pane, PaneState, Target,
};
use crate::services::tui_o::writer::transition_cleanup::scope::codex::identity;
use crate::services::tui_o::writer::transition_port::Idle;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events as p5;
use crate::test_support::wait::{WAIT_CAP, await_within};

const NONCE: &str = "11111111111111111111111111111111";

/// Runs the production binding-log actor behind a ready port while the callback sends its sweep.
pub(crate) async fn with_ready_codex<T, F, Fut>(source: SourceId, channel: u64, run: F) -> T
where
    F: FnOnce(Arc<crate::services::tui_o::writer::host::Readiness>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    with_ready_provider(source, channel, ShadowProvider::Codex, Arc::default(), run).await
}

/// Both actual actors share one readiness registry while their provider sweeps run.
pub(crate) async fn with_ready_codex_and_claude<T, F, Fut>(source: SourceId, run: F) -> T
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
pub(crate) async fn with_blocked_codex<T, F, Fut>(source: SourceId, run: F) -> T
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

#[derive(Default)]
struct Delta {
    tail: bool,
    context: bool,
    census: bool,
}

struct Observed {
    pane: String,
    sources: Vec<SourceId>,
    at: DateTime<Utc>,
    delta: std::sync::Mutex<Delta>,
}

impl Observer for Observed {
    fn observe(&self, _authority: &TmuxSourceAuthority<'_>) -> Result<Inputs, String> {
        let delta = self.delta.lock().unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: self.at,
            execution_nonce: NONCE.into(),
            tmux_session: self.pane.clone(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: "/owner".into(),
            host: Some("mac-mini".into()),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: self.sources[0].path.parent().map(Path::to_path_buf),
            first_prompt_digest: Some(if delta.context { "changed" } else { "launch" }.into()),
            source_policy: Some("shadow".into()),
        };
        let event = |seq, pane: &str, old, new| p5::BindingEvent {
            seq,
            channel_id: CHANNEL,
            provider: "codex".into(),
            tmux_session: pane.into(),
            execution_nonce: Some(NONCE.into()),
            old,
            new: p5::BindingTarget::Source(new),
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: Some("session_start".into()),
                received_at: self.at,
            },
            committed_at: self.at + TimeDelta::seconds(seq as i64),
        };
        let mut events: Vec<_> = self
            .sources
            .windows(2)
            .enumerate()
            .map(|(index, pair)| {
                event(
                    index as u64 + 1,
                    &self.pane,
                    Some(pair[0].clone()),
                    pair[1].clone(),
                )
            })
            .collect();
        let panes = vec![
            Pane {
                name: self.pane.clone(),
                state: PaneState::Live,
            },
            Pane {
                name: "historical-other-pane".into(),
                state: PaneState::Dead,
            },
        ];
        if delta.tail {
            events.push(event(
                events.len() as u64 + 1,
                "historical-other-pane",
                None,
                self.sources[0].clone(),
            ));
        }
        Ok(Inputs {
            target: Target {
                channel: CHANNEL,
                tmux: self.pane.clone(),
                nonce: NONCE.into(),
                host: "mac-mini".into(),
                owner_runtime_root: "/owner".into(),
                host_path: HostPath::Legacy,
            },
            context,
            current: self.sources.last().unwrap().clone(),
            tail: events.len() as u64,
            events,
            settled_companions: Default::default(),
            log_digest: if delta.tail {
                "full-tail-3"
            } else {
                "full-tail-2"
            }
            .into(),
            census_digest: if delta.census { "census-2" } else { "census-1" }.into(),
            panes,
            coverage: Coverage::Ok(()),
        })
    }
}

fn codex_chain() -> Chain {
    let chain = chain_named(2, false, |number| {
        let row = serde_json::json!({"type":"response_item", "payload": {
            "type":"message", "role":"assistant", "id":format!("msg_{number}"),
            "content":[{"type":"output_text", "text":format!("hop {number}")}]
        }});
        (format!("h{number}"), codex_line(row))
    });
    for event in chain.bindings.events.lock().unwrap().iter_mut() {
        event.provider = ShadowProvider::Codex;
    }
    chain
}

fn observed(chain: &Chain) -> Arc<Observed> {
    Arc::new(Observed {
        pane: chain.pane.clone(),
        sources: chain.sources.clone(),
        at: Utc::now() - TimeDelta::days(1),
        delta: std::sync::Mutex::default(),
    })
}

fn context(chain: &Chain, observer: &Arc<Observed>) -> Context {
    Context {
        tmux: chain.pane.clone(),
        nonce: NONCE.into(),
        lock: Arc::default(),
        codex: Some(observer.clone()),
    }
}

fn inspected(
    reply: &Reply,
) -> (
    &crate::services::tui_o::writer::codex_snapshot::Snapshot,
    String,
) {
    assert_eq!(reply.refused, None, "Codex Inspect reached its actor");
    assert_eq!(reply.operation, Some(Kind::Inspect));
    let diagnostics = reply
        .diagnostics
        .as_ref()
        .expect("the actor returned its snapshot");
    let observation = diagnostics.codex.as_ref().expect("typed Codex diagnostic");
    let Observation::Complete(snapshot) = observation else {
        panic!("the known complete fixture was not Complete: {observation:?}");
    };
    let shown = diagnostics.observation_id.clone().unwrap().unwrap();
    assert_eq!(shown, identity(observation).unwrap());
    (snapshot, shown)
}

#[tokio::test(start_paused = true)]
async fn bo_codex_inspect_returns_complete_but_automatic_apply_keeps_the_backlog() {
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
    let chain = codex_chain();
    let observer = observed(&chain);
    let actor = run_writer(
        &chain,
        None,
        Arc::default(),
        chain.harness.writer(),
        ShadowProvider::Codex,
    );
    settle(&chain).await;
    let before = files(&chain.dir());
    let reply = ask(
        &actor,
        Operation::Inspect(chain.request("bo-inspect")),
        context(&chain, &observer),
    )
    .await;
    let (snapshot, _) = inspected(&reply);
    assert_eq!(snapshot.current, *chain.sources.last().unwrap());
    assert_eq!(snapshot.held, chain.sources[1..]);
    assert_eq!(reply.execution, Some(Ok(chain.sources[1..].to_vec())));
    assert_eq!(snapshot.tail, 2);
    assert_eq!(reply.hops.len(), 2);
    assert!(reply.hops.iter().all(|hop| {
        hop.judged.verdict == Verdict::Refused(Refusal::CodexDeferred)
            && hop.outcome == Outcome::Unchanged
    }));
    let automatic = Operation::AutomaticApply(
        chain.request("bo-automatic"),
        ScopeDigest::parse(UNSCOPED).unwrap(),
        Idle(Arc::new(|| Box::pin(std::future::ready(true)))),
    );
    let reply = ask(&actor, automatic, context(&chain, &observer)).await;
    assert_eq!(reply.refused, Some(Refused::CodexDeferred));
    assert_eq!(files(&chain.dir()), before, "BO did not write the store");
    assert!((0..2).all(|number| !chain.cursor(number).retired && chain.audit(number).is_none()));
    halt(actor.stop, actor.task).await;
}

#[tokio::test(start_paused = true)]
async fn bo_codex_identity_observes_full_tail_context_and_census_without_writing() {
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
    let chain = codex_chain();
    let observer = observed(&chain);
    let actor = run_writer(
        &chain,
        None,
        Arc::default(),
        chain.harness.writer(),
        ShadowProvider::Codex,
    );
    settle(&chain).await;
    let before = files(&chain.dir());
    let mut ids = std::collections::BTreeSet::new();
    for case in 0..4 {
        *observer.delta.lock().unwrap() = Delta {
            tail: case == 1,
            context: case == 2,
            census: case == 3,
        };
        let request = chain.request(&format!("bo-identity-{case}"));
        let reply = ask(
            &actor,
            Operation::Inspect(request),
            context(&chain, &observer),
        )
        .await;
        let (snapshot, shown) = inspected(&reply);
        assert_eq!(snapshot.current, *chain.sources.last().unwrap());
        assert_eq!(snapshot.held, chain.sources[1..]);
        assert!(
            ids.insert(shown),
            "the changed component must move the observation identity: {case}"
        );
        assert_eq!(files(&chain.dir()), before);
    }
    halt(actor.stop, actor.task).await;
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
pub(crate) async fn with_ready_codex_backlog<T, F, Fut>(
    sources: Vec<SourceId>,
    pane: String,
    configured: bool,
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
    let (port, receiver) = TransitionPort::new(
        1,
        Arc::clone(&harness.gate),
        Arc::new(SystemTime::now),
        stopping.clone(),
    );
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
    let result = run(ready, probe).await;
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
    halt(stop, task).await;
    drop(host);
    (result, cursors, reading, health)
}

/// Reads durable cursors and routed health while the actual backlog actor keeps running.
pub(crate) struct BacklogProbe {
    root: PathBuf,
    sources: Vec<SourceId>,
    counts: watch::Receiver<()>,
    pub(crate) health: Arc<AlarmHealth>,
    alarms: Alarms,
    port: Arc<FakePort>,
    pub(crate) retained: Arc<Mutex<Vec<(usize, usize)>>>,
}

impl BacklogProbe {
    pub(crate) fn cursors(&self) -> Vec<Cursor> {
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

    pub(crate) async fn settled(&mut self, retired: usize) {
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

    pub(crate) async fn regrown(&mut self, source: &SourceId) {
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
