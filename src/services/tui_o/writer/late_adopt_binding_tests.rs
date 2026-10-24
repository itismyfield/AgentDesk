//! The real managed host follows committed binding notices and a frozen first source.

use super::late_host::{Legacy, closed, seed};
use super::*;
use crate::services::tui_o::cutover::{BodyClaim, BodySend, claim_then_send};
use crate::services::tui_o::writer::activation::test_hook::{self, Step};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::host::{FencedFacts, ManagedWriterHandle, start_managed};
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::watch;

struct Log {
    channel: u64,
    events: Mutex<Vec<BindingEvent>>,
    notice: Option<watch::Sender<u64>>,
    reads: AtomicUsize,
    order: Mutex<Vec<&'static str>>,
}

impl Log {
    fn new(closed: bool) -> Arc<Self> {
        Self::for_channel(closed, CHANNEL)
    }
    fn for_channel(closed: bool, channel: u64) -> Arc<Self> {
        Arc::new(Self {
            channel,
            events: Mutex::default(),
            notice: (!closed).then(|| watch::channel(0).0),
            reads: AtomicUsize::new(0),
            order: Mutex::default(),
        })
    }
    fn replace(&self, events: Vec<BindingEvent>) {
        let seq = events.last().map_or(0, |event| event.seq);
        *self.events.lock().unwrap() = events;
        if let Some(notice) = &self.notice {
            notice.send_replace(seq);
        }
    }
}

impl BindingEvents for Log {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        assert_eq!(channel, self.channel);
        self.order.lock().unwrap().push("read");
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.seq > after)
            .cloned()
            .collect())
    }
    fn subscribe(&self, channel: u64) -> watch::Receiver<u64> {
        assert_eq!(channel, self.channel);
        self.order.lock().unwrap().push("subscribe");
        self.notice
            .as_ref()
            .map_or_else(|| watch::channel(0).1, watch::Sender::subscribe)
    }
}

struct Io {
    base: Arc<TestIo>,
    log: Arc<Log>,
    other: Option<Arc<Log>>,
    /// Runs once as the next custody is read, before it is returned.
    on_custody: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl HostIo for Io {
    type Port = FakePort;
    type Lease = Arc<FakeLease>;
    type Alarms = Raised;
    type Bindings = Log;
    fn port(&self) -> impl Future<Output = Arc<FakePort>> + Send {
        self.base.port()
    }
    fn lease(&self) -> Self::Lease {
        self.base.lease()
    }
    fn alarms(&self) -> Raised {
        self.base.alarms()
    }
    fn bindings(&self, channel: u64, _: ShadowProvider) -> Arc<Log> {
        if channel == CHANNEL {
            self.log.clone()
        } else {
            self.other.as_ref().unwrap().clone()
        }
    }
    fn activation_facts(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.base.activation_facts(channel, provider)
    }
    fn intake_fence(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> impl Future<Output = Result<FencedFacts, String>> + Send {
        self.base.intake_fence(channel, provider)
    }
    fn local_custody(&self, channel: u64, provider: ShadowProvider) -> Result<Custody, String> {
        if let Some(hook) = self.on_custody.lock().unwrap().take() {
            hook();
        }
        self.base.local_custody(channel, provider)
    }
    fn legacy(&self) -> Arc<dyn LegacyView> {
        self.base.legacy()
    }
    fn legacy_busy(&self, channel: u64) -> impl Future<Output = bool> + Send {
        self.base.legacy_busy(channel)
    }
    fn relaying(&self, channel: u64) -> bool {
        self.base.relaying(channel)
    }
    fn adopted(&self, channel: u64, provider: ShadowProvider) {
        self.base.adopted(channel, provider);
    }
    fn late_adopt_enabled(&self) -> bool {
        self.base.late_adopt_enabled()
    }
}

fn event(seq: u64, record: BindingRecord) -> BindingEvent {
    let at = Utc::now();
    BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        tmux_session: "binding-host".into(),
        execution_nonce: "binding-host".into(),
        record,
        committed_at: at,
    }
}

fn bound(seq: u64, source: SourceId) -> BindingEvent {
    event(
        seq,
        BindingRecord::Bound {
            old: None,
            new: BindingTarget::Source(source),
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "SessionStart".into(),
                received_at: Utc::now(),
                reclaims: false,
            },
        },
    )
}

fn audit(seq: u64) -> BindingEvent {
    event(
        seq,
        BindingRecord::Rejected {
            detail: "audit only".into(),
            closes: None,
        },
    )
}

struct Fixture {
    harness: Harness,
    path: PathBuf,
    io: Arc<Io>,
    legacy: Arc<Legacy>,
    ready: Arc<Readiness>,
    old: Arc<Mutex<Vec<String>>>,
    _boot: test_override::ChannelsGuard,
}

impl Fixture {
    fn new(closed_notice: bool) -> Self {
        let (harness, path) = fresh(|_| Vec::new());
        harness.gate.acquired();
        let boot = seed(&harness, false);
        let base = TestIo::over(&harness);
        base.late_enabled.store(true, Ordering::SeqCst);
        let legacy = Arc::new(Legacy {
            path: path.clone(),
            frontier: AtomicU64::new(0),
        });
        *base.legacy.lock().unwrap() = Some(legacy.clone());
        Self {
            harness,
            path,
            io: Arc::new(Io {
                base,
                log: Log::new(closed_notice),
                other: None,
                on_custody: Mutex::default(),
            }),
            legacy,
            ready: Arc::default(),
            old: Arc::default(),
            _boot: boot,
        }
    }
    fn start(&self) -> ManagedWriterHandle {
        let mut handles = start_managed(
            ShadowProvider::Claude,
            true,
            cutover::boot_ownership(),
            || HostParts {
                io: self.io.clone(),
                runtime_root: Some(root(&self.harness)),
                gate: self.harness.gate.clone(),
                readiness: self.ready.clone(),
            },
        );
        assert_eq!(handles.len(), 1, "a real host must be created");
        handles.pop().unwrap()
    }
    async fn deliver_old(&self) -> (SourceId, u64) {
        let result = claim_then_send(Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))), || async {
            self.old
                .lock()
                .unwrap()
                .push("old delivered payload".into());
            append(&self.path, &row("binding-old", "old delivered payload"));
            append(&self.path, &closed());
            self.legacy.frontier.store(
                std::fs::metadata(&self.path).unwrap().len(),
                Ordering::SeqCst,
            );
        })
        .await
        .unwrap();
        assert_eq!(result, BodySend::Sent(()));
        test_override::with_channels(|boot| {
            assert_eq!(boot.unwrap().candidate(CHANNEL).unwrap().sends(), (1, 1))
        });
        (
            source_id_for("s1", &self.path).unwrap(),
            std::fs::metadata(&self.path).unwrap().len(),
        )
    }
    async fn assert_new_once(&self, end: u64) {
        for _ in 0..20 {
            if self.ready.accepts(CHANNEL) {
                break;
            }
            tokio::time::advance(Duration::from_secs(5)).await;
            spin().await;
        }
        assert!(
            self.ready.accepts(CHANNEL),
            "host did not adopt: {:?}",
            self.io.base.alarms.0.lock().unwrap()
        );
        assert_eq!(adoption(CHANNEL), Adoption::Committed);
        let init = self.harness.store.read_init(CHANNEL).unwrap().unwrap();
        assert_eq!(init.sources[0].delivery_start, end);
        assert!(
            self.harness.port.posts().is_empty(),
            "O must not repost the delivered old body"
        );
        append(
            &self.path,
            &row("binding-new", "unique binding new payload"),
        );
        polls(3).await;
        assert_eq!(self.harness.port.posts(), ["unique binding new payload"]);
        assert_eq!(*self.old.lock().unwrap(), ["old delivered payload"]);
    }
}

async fn spin() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn late_adopt_binding_notice_subscribes_before_read_and_wakes_without_fallback_delay() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_binding_notice_subscribes_before_read_and_wakes_without_fallback_delay"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let handle = f.start();
    spin().await;
    assert_eq!(f.io.log.order.lock().unwrap().first(), Some(&"subscribe"));
    assert!(!init_path(&f.harness, CHANNEL).exists());
    let (source, end) = f.deliver_old().await;
    let before = f.io.log.reads.load(Ordering::SeqCst);
    let at = tokio::time::Instant::now();
    f.io.log.replace(vec![bound(1, source)]);
    spin().await;
    assert_eq!(
        tokio::time::Instant::now(),
        at,
        "notice processing must not require the five-second fallback"
    );
    assert!(
        f.io.log.reads.load(Ordering::SeqCst) > before,
        "the actual host must reread the binding notice immediately"
    );
    f.assert_new_once(end).await;
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_binding_closed_watch_is_bounded_and_retries_at_five_seconds() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_binding_closed_watch_is_bounded_and_retries_at_five_seconds"
    )) {
        return;
    }
    let f = Fixture::new(true);
    let handle = f.start();
    spin().await;
    let initial = f.io.log.reads.load(Ordering::SeqCst);
    assert!(
        (1..=3).contains(&initial),
        "closed watch must be disabled after its one closure wake"
    );
    spin().await;
    assert_eq!(f.io.log.reads.load(Ordering::SeqCst), initial);
    let (source, end) = f.deliver_old().await;
    f.io.log.replace(vec![bound(1, source)]);
    tokio::time::advance(Duration::from_secs(4)).await;
    spin().await;
    assert_eq!(f.io.log.reads.load(Ordering::SeqCst), initial);
    tokio::time::advance(Duration::from_secs(1)).await;
    spin().await;
    assert!(
        f.io.log.reads.load(Ordering::SeqCst) > initial,
        "fallback must reread the actual host log"
    );
    f.assert_new_once(end).await;
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_binding_pending_then_resolved_uses_resolved_seq_and_posts_new_once() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_binding_pending_then_resolved_uses_resolved_seq_and_posts_new_once"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let handle = f.start();
    spin().await;
    let (source, end) = f.deliver_old().await;
    let pending = event(
        1,
        BindingRecord::Bound {
            old: None,
            new: BindingTarget::Pending {
                payload_session_id: source.session_id.clone(),
                payload_transcript_path: source.path.clone(),
            },
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "SessionStart".into(),
                received_at: Utc::now(),
                reclaims: false,
            },
        },
    );
    f.io.log.replace(vec![pending.clone()]);
    spin().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    spin().await;
    assert!(
        !init_path(&f.harness, CHANNEL).exists(),
        "Pending must remain a reservation"
    );
    assert_eq!(adoption(CHANNEL), Adoption::Deferred);
    let events = vec![
        pending,
        event(
            2,
            BindingRecord::Resolved {
                resolves_seq: 1,
                source: source.clone(),
            },
        ),
    ];
    assert_eq!(
        crate::services::tui_o::writer::late_adopt::first_bound(&events),
        Some((source, 2)),
        "Resolved's own seq, not its Pending seq, starts the baseline"
    );
    f.io.log.replace(events);
    f.assert_new_once(end).await;
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_binding_first_bound_or_resolved_rejects_coalesced_rotation_before_pin() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_binding_first_bound_or_resolved_rejects_coalesced_rotation_before_pin"
    )) {
        return;
    }
    for resolved in [false, true] {
        let f = Fixture::new(false);
        let handle = f.start();
        spin().await;
        let (a, _) = f.deliver_old().await;
        let second = f.harness._runtime.path().join("second.jsonl");
        std::fs::write(
            &second,
            [row("binding-second", "second delivered payload"), closed()].concat(),
        )
        .unwrap();
        let b = source_id_for("s2", &second).unwrap();
        *f.io.base.legacy.lock().unwrap() = Some(Arc::new(Legacy {
            path: second.clone(),
            frontier: AtomicU64::new(std::fs::metadata(&second).unwrap().len()),
        }));
        let first = if resolved {
            event(
                3,
                BindingRecord::Resolved {
                    resolves_seq: 1,
                    source: a.clone(),
                },
            )
        } else {
            bound(3, a.clone())
        };
        let prefix = if resolved {
            event(
                1,
                BindingRecord::Bound {
                    old: None,
                    new: BindingTarget::Pending {
                        payload_session_id: a.session_id.clone(),
                        payload_transcript_path: a.path.clone(),
                    },
                    cause: BindingCause::Startup,
                    parent_hint: None,
                    evidence: BindingEvidence {
                        hook_event: "SessionStart".into(),
                        received_at: Utc::now(),
                        reclaims: false,
                    },
                },
            )
        } else {
            audit(1)
        };
        let pins = Arc::new(AtomicUsize::new(0));
        let observed = pins.clone();
        test_hook::set(CHANNEL, Step::Snapshot, move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let events = vec![prefix, audit(2), first, audit(4), bound(5, b), audit(6)];
        assert_eq!(
            crate::services::tui_o::writer::late_adopt::first_bound(&events),
            Some((a, 3)),
            "the actual first binding event owns the baseline seq"
        );
        f.io.log.replace(events);
        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(5)).await;
            spin().await;
        }
        assert_eq!(
            adoption(CHANNEL),
            Adoption::Released,
            "FIRST source must remain A@3 rather than current B@6"
        );
        assert_eq!(
            pins.load(Ordering::SeqCst),
            0,
            "rotation must be checked before even the first pin"
        );
        assert!(
            f.io.base
                .alarms
                .released()
                .iter()
                .any(|(_, detail)| detail.contains("bound while"))
        );
        assert!(!init_path(&f.harness, CHANNEL).exists());
        assert!(f.harness.port.posts().is_empty());
        handle.stop_and_join().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn late_adopt_binding_rotation_after_intake_fence_cannot_publish_init() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_binding_rotation_after_intake_fence_cannot_publish_init"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let handle = f.start();
    spin().await;
    let (a, _) = f.deliver_old().await;
    f.io.log.replace(vec![bound(1, a.clone())]);
    let second = f.harness._runtime.path().join("after-fence.jsonl");
    std::fs::write(&second, b"").unwrap();
    let b = source_id_for("s2", &second).unwrap();
    let log = f.io.log.clone();
    let fenced = Arc::new(AtomicBool::new(false));
    let observed = fenced.clone();
    test_hook::set(CHANNEL, Step::AfterFence, move || {
        observed.store(true, Ordering::SeqCst);
        log.replace(vec![bound(1, a), bound(2, b)]);
        Ok(())
    });
    for _ in 0..20 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
        if adoption(CHANNEL) == Adoption::Released {
            break;
        }
    }
    assert!(
        fenced.load(Ordering::SeqCst),
        "the real commit must reach its intake fence"
    );
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    assert!(!init_path(&f.harness, CHANNEL).exists());
    assert!(f.harness.port.posts().is_empty());
    assert_eq!(*f.old.lock().unwrap(), ["old delivered payload"]);
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_binding_unsafe_channel_does_not_block_other_channels_new_body() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_binding_unsafe_channel_does_not_block_other_channels_new_body"
    )) {
        return;
    }
    let mut f = Fixture::new(false);
    let mut value = serde_json::to_value(super::late_host::config(false)).unwrap();
    value["tui_o"]["writer"]["channels"] = serde_json::json!([CHANNEL, OTHER]);
    value["agents"].as_array_mut().unwrap().push(serde_json::json!({"id":"late-other", "name":"Late other", "channels":{"claude":{"id":OTHER.to_string(),"runtime":"tui"}}}));
    let config: crate::config::Config = serde_json::from_value(value).unwrap();
    let snapshot = BootChannels::validate(&config)
        .unwrap()
        .seeded(true, &config, |channels, _| {
            Ok(crate::services::tui_o::channel_policy::stored(
                Some(f.harness._runtime.path()),
                channels,
            ))
        })
        .unwrap();
    crate::config_live_reload::install(config);
    let _multi_boot = test_override::force_boot(snapshot);
    let unsafe_log = Log::for_channel(false, OTHER);
    f.io = Arc::new(Io {
        base: f.io.base.clone(),
        log: f.io.log.clone(),
        other: Some(unsafe_log.clone()),
        on_custody: Mutex::default(),
    });
    let handles = start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: f.io.clone(),
            runtime_root: Some(root(&f.harness)),
            gate: f.harness.gate.clone(),
            readiness: f.ready.clone(),
        },
    );
    assert_eq!(handles.len(), 2, "both channels must run real host tasks");
    spin().await;
    let (safe_source, end) = f.deliver_old().await;
    let unsafe_old = Arc::new(AtomicUsize::new(0));
    let sent = unsafe_old.clone();
    assert_eq!(
        claim_then_send(
            Some(BodyClaim::new(OTHER, Some(ClaudeTui))),
            || async move {
                sent.fetch_add(1, Ordering::SeqCst);
            }
        )
        .await
        .unwrap(),
        BodySend::Sent(())
    );
    let paths = [
        f.harness._runtime.path().join("unsafe-a.jsonl"),
        f.harness._runtime.path().join("unsafe-b.jsonl"),
    ];
    for path in &paths {
        std::fs::write(path, b"").unwrap();
    }
    let a = source_id_for("unsafe-a", &paths[0]).unwrap();
    let b = source_id_for("unsafe-b", &paths[1]).unwrap();
    let mut events = vec![bound(1, a), bound(2, b), audit(3)];
    for event in &mut events {
        event.channel_id = OTHER;
    }
    unsafe_log.replace(events);
    f.io.log.replace(vec![bound(1, safe_source)]);
    f.assert_new_once(end).await;
    let other_state =
        test_override::with_channels(|boot| boot.unwrap().candidate(OTHER).unwrap().peek());
    assert_eq!(other_state, Adoption::Released);
    assert!(!init_path(&f.harness, OTHER).exists());
    assert_eq!(
        unsafe_old.load(Ordering::SeqCst),
        1,
        "the unsafe channel's real Legacy body must remain delivered"
    );
    assert!(!f.ready.accepts(OTHER));
    for handle in handles {
        handle.stop_and_join().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn late_adopt_binding_two_simultaneous_sources_each_commit_and_post_new_once() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_binding_two_simultaneous_sources_each_commit_and_post_new_once"
    )) {
        return;
    }
    struct ByTmux(std::collections::HashMap<String, Arc<Legacy>>);
    impl LegacyView for ByTmux {
        fn started(&self) -> bool {
            true
        }
        fn cursor(&self, tmux: &str) -> LegacyCursor {
            self.0.get(tmux).unwrap().cursor(tmux)
        }
        fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
            self.0.get(tmux).unwrap().late_cursor(tmux)
        }
        fn frontier(&self, channel: u64, tmux: &str, eof: u64) -> Option<u64> {
            self.0.get(tmux).unwrap().frontier(channel, tmux, eof)
        }
        fn tail_running(&self, tmux: &str) -> bool {
            self.0.get(tmux).unwrap().tail_running(tmux)
        }
    }
    let mut f = Fixture::new(false);
    let mut value = serde_json::to_value(super::late_host::config(false)).unwrap();
    value["tui_o"]["writer"]["channels"] = serde_json::json!([CHANNEL, OTHER]);
    value["agents"].as_array_mut().unwrap().push(serde_json::json!({"id":"late-other", "name":"Late other", "channels":{"claude":{"id":OTHER.to_string(),"runtime":"tui"}}}));
    let config: crate::config::Config = serde_json::from_value(value).unwrap();
    let snapshot = BootChannels::validate(&config)
        .unwrap()
        .seeded(true, &config, |channels, _| {
            Ok(crate::services::tui_o::channel_policy::stored(
                Some(f.harness._runtime.path()),
                channels,
            ))
        })
        .unwrap();
    crate::config_live_reload::install(config);
    let _multi_boot = test_override::force_boot(snapshot);
    let other_log = Log::for_channel(false, OTHER);
    let other_path = f.harness._runtime.path().join("other-positive.jsonl");
    std::fs::write(&other_path, b"").unwrap();
    let other_legacy = Arc::new(Legacy {
        path: other_path.clone(),
        frontier: AtomicU64::new(0),
    });
    *f.io.base.legacy.lock().unwrap() = Some(Arc::new(ByTmux(std::collections::HashMap::from([
        ("binding-host".into(), f.legacy.clone()),
        ("binding-other".into(), other_legacy.clone()),
    ]))));
    f.io = Arc::new(Io {
        base: f.io.base.clone(),
        log: f.io.log.clone(),
        other: Some(other_log.clone()),
        on_custody: Mutex::default(),
    });
    let handles = start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: f.io.clone(),
            runtime_root: Some(root(&f.harness)),
            gate: f.harness.gate.clone(),
            readiness: f.ready.clone(),
        },
    );
    assert_eq!(
        handles.len(),
        2,
        "both positive channels must enter the real host loop"
    );
    let generations: Vec<_> = handles
        .iter()
        .map(|handle| (handle.channel(), handle.generation()))
        .collect();
    spin().await;
    assert!(f.ready.is_hosted(CHANNEL) && f.ready.is_hosted(OTHER));
    assert!(!init_path(&f.harness, CHANNEL).exists() && !init_path(&f.harness, OTHER).exists());
    let (first_source, first_end) = f.deliver_old().await;
    let other_old = Arc::new(AtomicUsize::new(0));
    let sent = other_old.clone();
    assert_eq!(
        claim_then_send(Some(BodyClaim::new(OTHER, Some(ClaudeTui))), || async {
            sent.fetch_add(1, Ordering::SeqCst);
            append(
                &other_path,
                &row("other-positive-old", "other old delivered payload"),
            );
            append(&other_path, &closed());
            other_legacy.frontier.store(
                std::fs::metadata(&other_path).unwrap().len(),
                Ordering::SeqCst,
            );
        })
        .await
        .unwrap(),
        BodySend::Sent(())
    );
    test_override::with_channels(|boot| {
        for channel in [CHANNEL, OTHER] {
            assert_eq!(boot.unwrap().candidate(channel).unwrap().sends(), (1, 1));
        }
    });
    let other_end = std::fs::metadata(&other_path).unwrap().len();
    let other_source = source_id_for("other-positive", &other_path).unwrap();
    let mut other_binding = bound(1, other_source);
    other_binding.channel_id = OTHER;
    other_binding.tmux_session = "binding-other".into();
    let at = tokio::time::Instant::now();
    f.io.log.replace(vec![bound(1, first_source)]);
    other_log.replace(vec![other_binding]);
    assert_eq!(
        tokio::time::Instant::now(),
        at,
        "both bindings must arrive in the same clock instant"
    );
    for _ in 0..20 {
        if f.ready.accepts(CHANNEL) && f.ready.accepts(OTHER) {
            break;
        }
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
    }
    for (channel, end, path) in [
        (CHANNEL, first_end, &f.path),
        (OTHER, other_end, &other_path),
    ] {
        assert!(
            f.ready.accepts(channel),
            "channel {channel} did not become ready"
        );
        assert_eq!(adoption(channel), Adoption::Committed);
        let init = f.harness.store.read_init(channel).unwrap().unwrap();
        assert_eq!(init.sources.len(), 1);
        assert_eq!(init.sources[0].delivery_start, end);
        assert_eq!(&init.sources[0].source_id.path, path);
    }
    assert!(
        f.harness.port.posts().is_empty(),
        "neither channel may repost its delivered old body"
    );
    append(
        &f.path,
        &row("first-positive-new", "unique first channel new payload"),
    );
    append(
        &other_path,
        &row("other-positive-new", "unique second channel new payload"),
    );
    polls(4).await;
    let posts = f.harness.port.posts();
    assert_eq!(posts.len(), 2);
    for expected in [
        "unique first channel new payload",
        "unique second channel new payload",
    ] {
        assert_eq!(
            posts
                .iter()
                .filter(|post| post.as_str() == expected)
                .count(),
            1,
            "each channel's unique new body must post exactly once"
        );
    }
    assert_eq!(*f.old.lock().unwrap(), ["old delivered payload"]);
    assert_eq!(other_old.load(Ordering::SeqCst), 1);
    assert_eq!(
        handles
            .iter()
            .map(|handle| (handle.channel(), handle.generation()))
            .collect::<Vec<_>>(),
        generations
    );
    for handle in handles {
        handle.stop_and_join().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn late_adopt_binding_legacy_send_while_the_boundary_reads_custody_keeps_the_whole_history() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_binding_legacy_send_while_the_boundary_reads_custody_keeps_the_whole_history"
    )) {
        return;
    }
    let f = Fixture::new(false);
    let mut sources = Vec::new();
    for name in ["first", "second"] {
        let path = f.harness._runtime.path().join(format!("{name}.jsonl"));
        std::fs::write(
            &path,
            [row(name, "delivered before a restart"), closed()].concat(),
        )
        .unwrap();
        sources.push(source_id_for(name, &path).unwrap());
    }
    append(&f.path, &row("binding-old", "old delivered payload"));
    append(&f.path, &closed());
    let end = std::fs::metadata(&f.path).unwrap().len();
    f.legacy.frontier.store(end, Ordering::SeqCst);
    sources.push(source_id_for("s1", &f.path).unwrap());
    let events = (sources.iter().enumerate())
        .map(|(index, source)| bound(index as u64 + 1, source.clone()))
        .collect();
    f.io.log.replace(events);
    // A Legacy body completes between the boundary's first look at its sends and its custody read.
    *f.io.on_custody.lock().unwrap() = Some(Box::new(|| {
        let claim = Some(BodyClaim::new(CHANNEL, Some(ClaudeTui)));
        let sent = futures::executor::block_on(claim_then_send(claim, || async {}));
        assert_eq!(sent.unwrap(), BodySend::Sent(()));
    }));
    let handle = f.start();
    for _ in 0..10 {
        tokio::time::advance(Duration::from_secs(5)).await;
        spin().await;
    }
    test_override::with_channels(|boot| {
        assert_eq!(boot.unwrap().candidate(CHANNEL).unwrap().sends(), (1, 1))
    });
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    let (rotated, first) = (sources[1].path.display(), sources[0].path.display());
    let expected = format!(
        "source {rotated} was bound while the adoption waited (seq 2; baseline {first}@1; boundary 0;"
    );
    let released = f.io.base.alarms.released();
    assert!(
        released
            .iter()
            .any(|(_, detail)| detail.contains(&expected)),
        "{released:?}"
    );
    assert!(!init_path(&f.harness, CHANNEL).exists());
    assert!(f.harness.port.posts().is_empty());
    handle.stop_and_join().await.unwrap();
}

#[test]
fn late_adopt_binding_baseline_at_boundary_zero_is_the_first_bind_ever_logged() {
    use crate::services::tui_o::writer::late_adopt::{baseline, first_bound};
    let dir = tempfile::tempdir().unwrap();
    let source = |name: &str| {
        let path = dir.path().join(format!("{name}.jsonl"));
        std::fs::write(&path, b"").unwrap();
        source_id_for(name, &path).unwrap()
    };
    let (a, b) = (source("a"), source("b"));
    let resolved = |seq, source: &SourceId| {
        let source = source.clone();
        event(
            seq,
            BindingRecord::Resolved {
                resolves_seq: 1,
                source,
            },
        )
    };
    let rotation = vec![
        audit(1),
        bound(2, a.clone()),
        audit(3),
        resolved(4, &b),
        audit(5),
    ];
    for events in [rotation.clone(), vec![audit(1), audit(2)], Vec::new()] {
        assert_eq!(baseline(&events, 0), first_bound(&events), "{events:?}");
    }
    assert_eq!(baseline(&rotation, 0), Some((a.clone(), 2)));
    assert_eq!(
        baseline(&rotation, 1),
        Some((a.clone(), 2)),
        "the first bind after it"
    );
    assert_eq!(
        baseline(&rotation, 3),
        Some((a.clone(), 2)),
        "the last bind within it"
    );
    assert_eq!(baseline(&rotation, 5), Some((b.clone(), 4)));
    let back = vec![bound(1, a.clone()), bound(2, b), bound(3, a.clone())];
    assert_eq!(baseline(&back, 3), Some((a, 3)));
}
