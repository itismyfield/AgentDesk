use std::io::Write;

use std::path::Path;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::sync::mpsc;

use serde_json::json;

use super::*;

use crate::services::tui_o::shadow::SourceId;

use crate::services::tui_o::shadow::binding_reader::source_id_for;

use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvidence, BindingRecord, BindingTarget,
};

#[path = "activity_tests/large_tests.rs"]
mod large;

#[path = "activity_tests/local_control_tests.rs"]
mod local_control;

pub(in crate::services::discord) const CHANNEL: u64 = 6_100_001;

pub(in crate::services::discord) const SESSION: &str = "presence-parent";

pub(in crate::services::discord) struct Fake {
    pub(in crate::services::discord) seq: Mutex<Result<u64, String>>,
    pub(in crate::services::discord) events: Mutex<Result<Vec<BindingEvent>, String>>,
    pub(in crate::services::discord) present: AtomicBool,
    pub(in crate::services::discord) ready: AtomicBool,
    pub(in crate::services::discord) busy: AtomicBool,
    pub(in crate::services::discord) liveness: Mutex<SessionLiveness>,
    folds: AtomicUsize,
    pub(in crate::services::discord) jobs: Mutex<Vec<Job>>,
    pub(in crate::services::discord) spawns: AtomicUsize,
    /// The next pane read reports it started, then waits until released.
    pub(in crate::services::discord) pause: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    /// A host read pauses independently from the pane readiness read.
    pub(in crate::services::discord) host_pause:
        Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Ports for Fake {
    fn liveness(&self, _: ShadowProvider, _: u64, _: &str) -> SessionLiveness {
        let answer = *lock(&self.liveness);
        let pause = lock(&self.host_pause).take();
        if let Some((started, release)) = pause {
            started.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        answer
    }
    fn binding_seq(&self, _: u64) -> Result<u64, String> {
        lock(&self.seq).clone()
    }
    fn binding_events(&self, _: u64, _: ShadowProvider) -> Result<Vec<BindingEvent>, String> {
        self.folds.fetch_add(1, Ordering::SeqCst);
        lock(&self.events).clone()
    }
    fn session_present(&self, _: &str) -> bool {
        self.present.load(Ordering::SeqCst)
    }
    fn final_ready(&self, _: &str) -> bool {
        // Taken before waiting, so another read meanwhile does not block on this lock.
        let pause = lock(&self.pause).take();
        if let Some((started, release)) = pause {
            started.send(()).unwrap();
            let _ = release.recv_timeout(adk_test_support::test_support::wait::WAIT_CAP);
        }
        self.ready.load(Ordering::SeqCst)
    }
    fn claude_busy(&self, _: &str) -> claude_activity::Busy {
        self.busy.load(Ordering::SeqCst).into()
    }
    fn spawn(&self, job: Job) -> bool {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        lock(&self.jobs).push(job);
        true
    }
}

/// One channel's watch with scripted ports; workers run only when the test says so.
pub(in crate::services::discord) struct Probe {
    pub(in crate::services::discord) watch: Arc<Mutex<Watch>>,
    pub(in crate::services::discord) fake: Arc<Fake>,
    pub(in crate::services::discord) provider: ShadowProvider,
    pub(in crate::services::discord) target: Target,
}

impl Probe {
    pub(in crate::services::discord) fn new(
        provider: ShadowProvider,
        events: Vec<BindingEvent>,
    ) -> Self {
        let fake = Arc::new(Fake {
            seq: Mutex::new(Ok(events.len() as u64)),
            events: Mutex::new(Ok(events)),
            present: AtomicBool::new(true),
            ready: AtomicBool::new(true),
            busy: AtomicBool::new(false),
            liveness: Mutex::new(SessionLiveness::Alive),
            folds: AtomicUsize::new(0),
            jobs: Mutex::new(Vec::new()),
            spawns: AtomicUsize::new(0),
            pause: Mutex::new(None),
            host_pause: Mutex::new(None),
        });
        let target = Target::Bound(SESSION.into());
        let watch = Arc::default();
        Self {
            watch,
            fake,
            provider,
            target,
        }
    }

    pub(in crate::services::discord) fn ask(&self) -> (Activity, &'static str) {
        let ports: Arc<dyn Ports> = self.fake.clone();
        let (observed, _) = judge(
            &self.watch,
            &ports,
            self.provider,
            CHANNEL,
            self.target.clone(),
        );
        (observed.activity, observed.reason)
    }

    pub(in crate::services::discord) fn ask_presence(&self) -> (Activity, &'static str) {
        let ports: Arc<dyn Ports> = self.fake.clone();
        let (observed, _) = judge_presence(
            &self.watch,
            &ports,
            self.provider,
            CHANNEL,
            self.target.clone(),
        );
        (observed.activity, observed.reason)
    }

    pub(in crate::services::discord) fn run_jobs(&self) -> usize {
        let jobs = std::mem::take(&mut *lock(&self.fake.jobs));
        let ran = jobs.len();
        jobs.into_iter().for_each(|job| job());
        ran
    }

    /// Asks until no worker is left to finish; a probe that keeps rebuilding stays `catching_up`.
    pub(in crate::services::discord) fn settle(&self) -> (Activity, &'static str) {
        let mut answer = self.ask();
        for _ in 0..8 {
            if answer.1 != "catching_up" || self.run_jobs() == 0 {
                break;
            }
            answer = self.ask();
        }
        answer
    }

    pub(in crate::services::discord) fn counts(&self) -> (usize, usize) {
        let fake = &self.fake;
        (
            fake.folds.load(Ordering::SeqCst),
            fake.spawns.load(Ordering::SeqCst),
        )
    }

    pub(in crate::services::discord) fn age(&self, by: Duration) {
        let mut watch = lock(&self.watch);
        watch.grew_at = Instant::now() - by;
    }
}

pub(in crate::services::discord) fn bound(
    seq: u64,
    session: &str,
    source: &SourceId,
    provider: ShadowProvider,
) -> BindingEvent {
    let target = BindingTarget::Source(source.clone());
    event(seq, session, provider, record(target))
}

pub(in crate::services::discord) fn record(new: BindingTarget) -> BindingRecord {
    let received_at = chrono::Utc::now();
    let hook_event = "SessionStart".into();
    BindingRecord::Bound {
        old: None,
        new,
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event,
            received_at,
            reclaims: false,
        },
    }
}

pub(in crate::services::discord) fn event(
    seq: u64,
    session: &str,
    provider: ShadowProvider,
    record: BindingRecord,
) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider,
        tmux_session: session.into(),
        execution_nonce: "presence".into(),
        record,
        committed_at: chrono::Utc::now(),
    }
}

pub(in crate::services::discord) fn write(path: &Path, rows: &[serde_json::Value]) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
}

pub(in crate::services::discord) fn prompt(id: &str) -> serde_json::Value {
    json!({"type":"user","uuid":id,"message":{"content":"direct turn"}})
}

pub(in crate::services::discord) fn end() -> serde_json::Value {
    json!({"type":"system","subtype":"turn_duration"})
}

pub(in crate::services::discord) fn summary() -> serde_json::Value {
    json!({"type":"summary","summary":"earlier session"})
}

fn codex_header(root: &Path) -> serde_json::Value {
    json!({"type":"session_meta","payload":{"id":"parent","cwd":root,"source":"cli","originator":"codex-tui"}})
}

pub(in crate::services::discord) fn codex(kind: &str, id: &str) -> serde_json::Value {
    json!({"type":"event_msg","payload":{"type":kind,"turn_id":id}})
}

/// A Claude probe bound to one transcript holding `rows`.
pub(in crate::services::discord) fn claude(
    root: &Path,
    rows: &[serde_json::Value],
) -> (Probe, std::path::PathBuf) {
    std::fs::create_dir_all(root).unwrap();
    let path = root.join("parent.jsonl");
    write(&path, rows);
    let source = source_id_for("parent", &path).unwrap();
    let probe = Probe::new(
        ShadowProvider::Claude,
        vec![bound(1, SESSION, &source, ShadowProvider::Claude)],
    );
    (probe, path)
}

pub(in crate::services::discord) fn compact_fixture() -> Vec<serde_json::Value> {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/claude_idle_compact_summary.jsonl"
    ))
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect()
}

pub(in crate::services::discord) fn codex_probe(
    root: &Path,
    rows: &[serde_json::Value],
) -> (Probe, std::path::PathBuf) {
    let path = root.join("rollout.jsonl");
    write(&path, &[codex_header(root)]);
    write(&path, rows);
    let source = source_id_for("parent", &path).unwrap();
    let probe = Probe::new(
        ShadowProvider::Codex,
        vec![bound(1, SESSION, &source, ShadowProvider::Codex)],
    );
    (probe, path)
}

pub(in crate::services::discord) fn strict_reading(probe: &Probe) -> Reading {
    let ports: Arc<dyn Ports> = probe.fake.clone();
    let (observed, stamp) = judge_presence(
        &probe.watch,
        &ports,
        probe.provider,
        CHANNEL,
        probe.target.clone(),
    );
    Reading::bound(observed, stamp, Some(probe.watch.clone()), Some(SESSION))
}

/// A paused probe, its transcript, what overtakes its pane read and what the overtaking ask reads.
pub(in crate::services::discord) type Overtaken<'a> = (
    Probe,
    std::path::PathBuf,
    &'a dyn Fn(&Probe, &Path),
    (Activity, &'static str),
);

/// Keeps this thread's binding-log root for the life of a fixture.
pub(crate) struct BindingRoot(Option<std::path::PathBuf>);

impl BindingRoot {
    pub(crate) fn enter(root: &Path) -> Self {
        use crate::services::tui_prompt_dedupe::binding_events as p5;
        let saved = p5::test_root();
        p5::set_test_root(Some(root));
        Self(saved)
    }
}

impl Drop for BindingRoot {
    fn drop(&mut self) {
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(self.0.as_deref());
    }
}

/// A confirmed turn-mode channel whose live watcher and one-record binding log name `path`, as
/// the binding writer leaves them; turn mode lasts while the returned guard lives.
pub(crate) fn bind_turn_mode_transcript(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
    session: &str,
    path: &Path,
) -> crate::services::tui_o::turn_mode::TestConfirmation {
    use crate::services::discord::TmuxWatcherHandle;
    shared.tmux_watchers.insert(
        ChannelId::new(channel),
        TmuxWatcherHandle {
            tmux_session_name: session.into(),
            output_path: path.display().to_string(),
            paused: Arc::new(false.into()),
            resume_offset: Arc::new(Mutex::new(None)),
            cancel: Arc::new(false.into()),
            pause_epoch: Arc::new(0.into()),
            turn_delivered: Arc::new(false.into()),
            last_heartbeat_ts_ms: Arc::new(chrono::Utc::now().timestamp_millis().into()),
        },
    );
    log_turn_source(provider, channel, session, "turn-presence-test", path);
    crate::services::tui_o::turn_mode::TestConfirmation::new(channel)
}

/// The channel's binding log as one record of execution `nonce` naming `path` as `session`'s source.
pub(crate) fn log_turn_source(
    provider: &ProviderKind,
    channel: u64,
    session: &str,
    nonce: &str,
    path: &Path,
) {
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    let source = source_id_for("parent", path).unwrap();
    let now = chrono::Utc::now();
    let event = p5::BindingEvent {
        seq: 1,
        channel_id: channel,
        provider: provider.as_str().into(),
        tmux_session: session.into(),
        execution_nonce: Some(nonce.into()),
        old: None,
        new: p5::BindingTarget::Source(source),
        cause: p5::BindingCause::Startup,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: Some("SessionStart".into()),
            received_at: now,
        },
        committed_at: now,
    };
    let root = p5::test_root().unwrap().join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&root).unwrap();
    let line = format!("{}\n", serde_json::to_string(&event).unwrap());
    std::fs::write(root.join(format!("{channel}.log")), line).unwrap();
}

/// The reason effect points read once the channel's rebuild finished.
pub(crate) async fn settled_reason(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: u64,
) -> &'static str {
    for _ in 0..500 {
        let observed = activity_now(shared, provider, ChannelId::new(channel)).await;
        if observed.reason != "catching_up" {
            return observed.reason;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("channel {channel} never finished catching up");
}

/// Waits for the live binding-log worker, then returns a fresh effect-point reading.
pub(crate) async fn companion_settled_activity(shared: &Arc<SharedData>, channel: u64) -> Observed {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let observed =
                activity_now(shared, &ProviderKind::Codex, ChannelId::new(channel)).await;
            if !matches!(observed.reason, "catching_up" | "facts_resumed") {
                return observed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual Codex binding-log worker did not finish")
}

/// Advances the production watch's monotonic age without a ten-minute wall-clock sleep.
pub(crate) fn companion_age_open_watch(channel: u64, age: Duration) {
    let watch = lock(&WATCHES)
        .get(&channel)
        .cloned()
        .expect("the actual activity reader must have installed its watch first");
    let mut watch = lock(&watch);
    assert!(matches!(
        watch.outcome,
        Outcome::Source { .. } | Outcome::Tail(_)
    ));
    watch.grew_at = Instant::now() - age;
}

/// A live watcher over the caller's existing log; this never creates or rewrites binding records.
pub(crate) fn companion_attach_log_watcher(
    shared: &SharedData,
    channel: u64,
    tmux: &str,
    path: &Path,
) {
    shared.tmux_watchers.insert(
        ChannelId::new(channel),
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: tmux.into(),
            output_path: path.display().to_string(),
            paused: Arc::new(false.into()),
            resume_offset: Arc::new(Mutex::new(None)),
            cancel: Arc::new(false.into()),
            pause_epoch: Arc::new(0.into()),
            turn_delivered: Arc::new(false.into()),
            last_heartbeat_ts_ms: Arc::new(chrono::Utc::now().timestamp_millis().into()),
        },
    );
}

/// Lets non-Discord host tests inspect their exact settled log through the live activity reader.
pub(crate) async fn companion_activity_for_log_for_tests(
    channel: u64,
    tmux: &str,
    path: &Path,
) -> (&'static str, bool) {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let _confirmation = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
    companion_attach_log_watcher(&shared, channel, tmux, path);
    let observed = companion_settled_activity(&shared, channel).await;
    (observed.reason, observed.activity == Activity::Idle)
}

/// The actual Unbound collector's verdict before its public reason is aggregated.
pub(in crate::services::discord) async fn unbound_verdict_for_tests(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<String, String> {
    super::unbound::live::tests::verdict_for_tests(shared, provider, channel).await
}

pub(in crate::services::discord) fn publish<R>(
    reading: &Reading,
    run: impl FnOnce() -> R,
) -> Option<R> {
    futures::executor::block_on(reading.publish_if_current(run))
}

pub(crate) fn small_tail_limits(channel: u64) -> impl Drop {
    tail::overrides::Guard::new(channel, 1, 4096, 4096, 37)
}

#[path = "activity_tests/effect_tests.rs"]
mod effects;

pub(in crate::services::discord) async fn installed(watch: &Mutex<Watch>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !matches!(lock(watch).outcome, Outcome::Rebuilding) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

pub(crate) fn exercise_tail_switch_entry_order(expected: bool, supervisor_first: bool) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let _runtime = crate::config::TestRuntimeRootGuard::new();
            let root = tempfile::tempdir().unwrap();
            let _binding = BindingRoot::enter(root.path());
            let shared = crate::services::discord::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            if supervisor_first {
                super::super::supervisor::spawn(shared.clone(), provider.clone());
            }
            let unbound = ChannelId::new(69_183_700);
            let watch = lock(&WATCHES).entry(unbound.get()).or_default().clone();
            let generation = lock(&watch).generation;
            let reading = reading_now(&shared, &provider, unbound).await;
            assert_eq!(
                lock(&watch).generation,
                generation + 1,
                "Tail off bypassed Unbound invalidation"
            );
            assert!(reading.session().is_none() && reading.source().is_none());
            if !supervisor_first {
                super::super::supervisor::spawn(shared.clone(), provider.clone());
            }
            assert_eq!(tail::enabled(), expected);
            let channel = 69_183_701;
            let _limits = tail::overrides::Guard::new(channel, 1, 4096, 4096, 59);
            let path = root.path().join("cold.jsonl");
            write(&path, &[prompt("A"), end()]);
            let _turn =
                bind_turn_mode_transcript(&shared, &provider, channel, "s1-switch-cold", &path);
            assert_eq!(
                settled_reason(&shared, &provider, channel).await,
                if expected {
                    "closed"
                } else {
                    "transcript_too_large"
                }
            );
            let channel = 69_183_702;
            let path = root.path().join("full.jsonl");
            write(&path, &[prompt("B"), end()]);
            let size = std::fs::metadata(&path).unwrap().len();
            let _limits = tail::overrides::Guard::new(channel, size, 4096, 4096, 59);
            let _turn =
                bind_turn_mode_transcript(&shared, &provider, channel, "s1-switch-full", &path);
            assert_eq!(settled_reason(&shared, &provider, channel).await, "closed");
            write(&path, &[end()]);
            assert!(std::fs::metadata(&path).unwrap().len() > size);
            assert_eq!(
                settled_reason(&shared, &provider, channel).await,
                "closed",
                "Tail off stopped installed Full growth"
            );
        });
}

pub(crate) struct HandoffMutation(tail::tests::HandoffHook);

impl HandoffMutation {
    pub(crate) fn new(
        channel: u64,
        bootstrap: Option<Box<dyn FnOnce() + Send>>,
        resume: Option<Box<dyn FnOnce() + Send>>,
    ) -> Self {
        Self(tail::tests::HandoffHook::new(channel, bootstrap, resume))
    }
    pub(crate) fn calls(&self) -> usize {
        self.0.calls()
    }
}

/// Runs the production judge/rebuild/install path while the test owns worker completion.
pub(in crate::services::discord) struct NoticeReaderProbe {
    probe: Probe,
    channel: u64,
}

impl NoticeReaderProbe {
    pub(in crate::services::discord) fn new(channel: u64) -> Self {
        let mut probe = Probe::new(ShadowProvider::Claude, Vec::new());
        probe.target = Target::Bound(format!("notice-reader-{channel}"));
        *lock(&probe.fake.events) = Err("binding log unreadable".into());
        Self { probe, channel }
    }

    pub(in crate::services::discord) fn read(&self) -> Reading {
        let ports: Arc<dyn Ports> = self.probe.fake.clone();
        let (observed, stamp) = judge_presence(
            &self.probe.watch,
            &ports,
            self.probe.provider,
            self.channel,
            self.probe.target.clone(),
        );
        let Target::Bound(label) = &self.probe.target else {
            unreachable!()
        };
        Reading::bound(observed, stamp, Some(self.probe.watch.clone()), Some(label))
    }

    pub(in crate::services::discord) fn unreadable_seq(&self) {
        *lock(&self.probe.fake.seq) = Err("unreadable seq".into());
    }

    pub(in crate::services::discord) fn force_retry_due(&self) {
        let mut watch = lock(&self.probe.watch);
        let Outcome::Unreadable { retry_at } = &mut watch.outcome else {
            panic!("the actual reader must have installed Unreadable before retry")
        };
        *retry_at = Instant::now();
    }

    pub(in crate::services::discord) fn run_jobs(&self) -> usize {
        self.probe.run_jobs()
    }
    pub(in crate::services::discord) fn counts(&self) -> (usize, usize) {
        self.probe.counts()
    }

    pub(in crate::services::discord) fn pending(&self) {
        let Target::Bound(label) = &self.probe.target else {
            unreachable!()
        };
        let target = BindingTarget::Pending {
            payload_session_id: "pending".into(),
            payload_transcript_path: std::path::PathBuf::from("/notice-pending.jsonl"),
        };
        let mut binding = event(1, label, self.probe.provider, record(target));
        binding.channel_id = self.channel;
        *lock(&self.probe.fake.events) = Ok(vec![binding]);
        *lock(&self.probe.fake.seq) = Ok(1);
    }
}

pub(super) fn resumed_notice_observations(
    root: &Path,
) -> (NoticeObservation, usize, NoticeObservation) {
    let (probe, path) = codex_probe(root, &[codex("task_started", "A")]);
    assert_eq!(probe.settle(), (Activity::Busy, "open"));
    let blocked = json!({"type":"response_item","payload":{"type":"unknown_item","id":"x"}});
    write(&path, &[blocked, codex("turn_aborted", "B")]);
    let resumed = strict_reading(&probe).notice_observation();
    let jobs = probe.run_jobs();
    let boundary = strict_reading(&probe).notice_observation();
    (resumed, jobs, boundary)
}

#[path = "activity_tests/native_input_tests.rs"]
mod native_input_tests;

use super::observation_hook_tests::ReadTrace;
pub(crate) use super::observation_hook_tests::ReadyPane;
