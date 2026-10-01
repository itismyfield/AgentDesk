use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::{self, test_override};
use crate::services::tui_o::writer::activation::ActivationFacts;
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::{HostIo, HostParts, Readiness, start};

use super::*;
use crate::services::tui_prompt_dedupe::binding_events as p5;
#[cfg(unix)]
use chrono::{DateTime, TimeDelta};

const OTHER: u64 = 8;

#[derive(Clone, Default)]
struct Raised(Arc<Mutex<Vec<(u64, WriterAlarm)>>>);

impl AlarmSink for Raised {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.0.lock().unwrap().push((channel, alarm));
    }
}

impl Raised {
    fn halted(&self) -> Vec<(u64, String)> {
        let raised = self.0.lock().unwrap();
        let halted = raised.iter().filter_map(|(channel, alarm)| match alarm {
            WriterAlarm::Halted { detail } => Some((*channel, detail.clone())),
            _ => None,
        });
        halted.collect()
    }

    fn has(&self, channel: u64, wanted: &WriterAlarm) -> bool {
        let raised = self.0.lock().unwrap();
        raised
            .iter()
            .any(|(c, alarm)| *c == channel && alarm == wanted)
    }
}

/// The gateway side as the host sees it; records which channel asked for what.
struct TestIo {
    port: Arc<FakePort>,
    lease: Arc<FakeLease>,
    alarms: Raised,
    calls: Mutex<Vec<(&'static str, u64)>>,
    facts: Mutex<Result<ActivationFacts, String>>,
    custody: Mutex<Result<bool, String>>,
    /// Runs once while the next facts are read.
    on_facts: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The gateway never comes up.
    port_down: std::sync::atomic::AtomicBool,
}

impl TestIo {
    fn over(harness: &Harness) -> Arc<Self> {
        Arc::new(Self {
            port: Arc::clone(&harness.port),
            lease: Arc::clone(&harness.lease),
            alarms: Raised::default(),
            calls: Mutex::default(),
            facts: Mutex::new(Ok(ActivationFacts::default())),
            custody: Mutex::new(Ok(false)),
            on_facts: Mutex::default(),
            port_down: Default::default(),
        })
    }

    fn calls(&self) -> Vec<(&'static str, u64)> {
        self.calls.lock().unwrap().clone()
    }
}

impl HostIo for TestIo {
    type Port = FakePort;
    type Lease = Arc<FakeLease>;
    type Alarms = Raised;
    type Bindings = ChannelBindingLog;

    fn port(&self) -> impl Future<Output = Arc<FakePort>> + Send {
        self.calls.lock().unwrap().push(("port", 0));
        let port = Arc::clone(&self.port);
        let down = self.port_down.load(Ordering::SeqCst);
        async move {
            if down {
                std::future::pending::<()>().await;
            }
            port
        }
    }

    fn lease(&self) -> Arc<FakeLease> {
        self.calls.lock().unwrap().push(("lease", 0));
        Arc::clone(&self.lease)
    }

    fn alarms(&self) -> Raised {
        self.alarms.clone()
    }

    fn bindings(&self, channel: u64, provider: ShadowProvider) -> Arc<ChannelBindingLog> {
        self.calls.lock().unwrap().push(("bindings", channel));
        Arc::new(ChannelBindingLog::new(channel, provider))
    }

    fn activation_facts(
        &self,
        channel: u64,
        _provider: ShadowProvider,
    ) -> impl Future<Output = Result<ActivationFacts, String>> + Send {
        self.calls.lock().unwrap().push(("facts", channel));
        if let Some(hook) = self.on_facts.lock().unwrap().take() {
            hook();
        }
        let facts = self.facts.lock().unwrap().clone();
        async move { facts }
    }

    fn local_custody(&self, _: u64, _: ShadowProvider) -> Result<bool, String> {
        self.custody.lock().unwrap().clone()
    }

    fn legacy(&self) -> Arc<dyn crate::services::tui_o::writer::adoption::LegacyView> {
        Arc::new(crate::services::tui_o::writer::adoption::NoLegacy)
    }
}

/// This thread's adoption of a selected channel.
fn adoption(channel: u64) -> Adoption {
    let candidate = |boot: Option<&BootChannels>| boot.unwrap().candidate(channel).unwrap().peek();
    test_override::with_channels(candidate)
}

fn root(harness: &Harness) -> PathBuf {
    harness._runtime.path().to_path_buf()
}

fn host(harness: &Harness, io: &Arc<TestIo>, pg: bool, ready: &Arc<Readiness>) -> usize {
    hosted(harness, io, pg, ready).len()
}

fn hosted(
    harness: &Harness,
    io: &Arc<TestIo>,
    pg: bool,
    ready: &Arc<Readiness>,
) -> Vec<tokio::task::JoinHandle<()>> {
    p5::set_test_root(Some(harness._runtime.path()));
    let parts = || HostParts {
        io: Arc::clone(io),
        runtime_root: Some(root(harness)),
        gate: Arc::clone(&harness.gate),
        readiness: Arc::clone(ready),
    };
    start(ShadowProvider::Claude, pg, parts)
}

fn empty_init(channel: u64) -> Initialized {
    let (initial_anchor, build_digest, at) = (100, "b".to_string(), Utc::now());
    Initialized {
        channel,
        sources: Vec::new(),
        initial_anchor,
        build_digest,
        at,
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn only_a_selected_channel_gets_an_actor_and_it_is_ready_only_while_owned() {
    let (harness, path, source) = switched_over(&row("m0", "before the switch"));
    harness.store.init_channel(&empty_init(OTHER)).unwrap();
    let startup = p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source));
    p5_log(harness._runtime.path(), CHANNEL, &startup);
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    assert_eq!(host(&harness, &io, true, &ready), 1);
    append(&path, &row("m1", "first"));
    polls(3).await;
    let expected = [("port", 0), ("lease", 0), ("bindings", CHANNEL)];
    assert_eq!(io.calls(), expected, "the unselected channel gets nothing");
    assert!(
        !ready.is_ready(CHANNEL),
        "a resumed actor alone is not ready"
    );
    assert!(io.alarms.has(CHANNEL, &WriterAlarm::PausedNoGateway));
    assert!(harness.port.posts().is_empty());
    harness.gate.acquired();
    polls(3).await;
    assert!(ready.is_ready(CHANNEL) && !ready.is_ready(OTHER));
    assert_eq!(harness.port.posts(), ["first"]);
    assert_eq!(
        host(&harness, &io, true, &ready),
        0,
        "one actor per channel"
    );
    harness.gate.lost();
    polls(1).await;
    assert!(!ready.is_ready(CHANNEL));
    assert_eq!(io.alarms.halted(), []);
}

#[tokio::test(start_paused = true)]
async fn a_published_flag_alone_does_not_accept_work() {
    let (harness, _, source) = switched_over(&row("m0", "before the switch"));
    let startup = p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source));
    p5_log(harness._runtime.path(), CHANNEL, &startup);
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    harness.gate.acquired();
    let hosts = hosted(&harness, &io, true, &ready);
    polls(3).await;
    assert!(ready.is_ready(CHANNEL) && ready.accepts(CHANNEL));
    harness.gate.lost();
    assert!(
        ready.is_ready(CHANNEL),
        "the published flag still trails the gate"
    );
    assert!(!ready.accepts(CHANNEL), "a lost gate takes no work");
    harness.gate.acquired();
    polls(1).await;
    assert!(ready.accepts(CHANNEL));
    hosts.iter().for_each(|host| host.abort());
    polls(3).await;
    assert!(
        ready.is_ready(CHANNEL),
        "nothing cleared the flag once its host was gone"
    );
    assert!(!ready.accepts(CHANNEL), "an ended actor takes no work");
}

#[test]
fn nothing_is_hosted_while_the_writer_is_off_or_selects_no_channel() {
    let unprepared =
        || -> HostParts<TestIo> { panic!("nothing is owned, yet host parts were built") };
    let selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let off = test_override::force_off();
    assert!(start(ShadowProvider::Claude, true, unprepared).is_empty());
    drop(off);
    drop(selected);
    let empty = test_override::force_channels(&[]);
    assert!(start(ShadowProvider::Claude, true, unprepared).is_empty());
    drop(empty);
    let _foreign = test_override::force_foreign(&[(CHANNEL, ClaudeTui)], "home");
    assert!(
        start(ShadowProvider::Claude, true, unprepared).is_empty(),
        "off the O home"
    );
}

#[tokio::test(start_paused = true)]
async fn another_providers_channel_is_left_to_its_own_bot() {
    let (harness, _, _) = switched_over(&row("m0", "before the switch"));
    let _selected = test_override::force_channels(&[(CHANNEL, CodexTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    assert_eq!(host(&harness, &io, true, &ready), 0);
    assert_eq!(io.calls(), []);
}

#[tokio::test(start_paused = true)]
async fn a_channel_without_its_own_recovered_store_is_held_without_an_actor() {
    let (harness, _, _) = switched_over(&row("m0", "before the switch"));
    harness.gate.acquired();
    let store_dir = harness._runtime.path().join("o_store");
    let foreign = 10;
    copy_dir(
        &store_dir.join(CHANNEL.to_string()),
        &store_dir.join(foreign.to_string()),
    );
    let _selected = test_override::force_channels(&[(foreign, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    assert_eq!(host(&harness, &io, true, &ready), 1);
    polls(3).await;
    assert_eq!(io.calls(), [], "no gateway, lease or binding is taken");
    assert!(harness.port.posts().is_empty());
    let halted = io.alarms.halted();
    assert!(
        matches!(halted.as_slice(), [(c, detail)] if *c == foreign && detail.contains("init names another channel")),
        "{halted:?}"
    );
    assert!(!ready.is_ready(foreign));

    let bare = Harness::new();
    std::fs::remove_file(bare._runtime.path().join("o_store").join("o_era")).unwrap();
    let io = TestIo::over(&bare);
    let _bare = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    assert_eq!(host(&bare, &io, true, &ready), 1);
    polls(1).await;
    assert!(io.alarms.halted()[0].1.contains("no writer era"));
    assert_eq!(io.calls(), []);
}

#[tokio::test(start_paused = true)]
async fn an_actor_that_cannot_resume_its_sources_is_never_ready() {
    let (harness, path, _) = switched_over(&row("m0", "before the switch"));
    std::fs::remove_file(&path).unwrap();
    harness.gate.acquired();
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    assert_eq!(host(&harness, &io, true, &ready), 1);
    polls(3).await;
    assert!(!ready.is_ready(CHANNEL));
    let halted = io.alarms.halted();
    assert!(matches!(halted.as_slice(), [(CHANNEL, detail)] if detail.contains("source reopen")));
}

#[tokio::test(start_paused = true)]
async fn without_a_pg_gateway_lease_a_selected_channel_is_held_and_stays_with_o() {
    let (harness, _, _) = switched_over(&row("m0", "before the switch"));
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    assert_eq!(host(&harness, &io, false, &ready), 0);
    harness.gate.acquired();
    polls(3).await;
    assert_eq!(io.calls(), []);
    let halted = io.alarms.halted();
    assert!(matches!(halted.as_slice(), [(CHANNEL, detail)] if detail.contains("no PG gateway")));
    assert!(!ready.is_ready(CHANNEL));
    let owned = cutover::o_owns_tui_output_for_channel(CHANNEL, Some(ClaudeTui));
    assert_eq!(owned, Ok(true), "Legacy does not take the body back");
}

#[tokio::test(start_paused = true)]
async fn a_recovered_channel_knows_its_newest_post_while_the_gateway_is_down() {
    let (harness, _, _) = switched_over(&row("m0", "before the switch"));
    harness.gate.acquired();
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "hello")).await, Step::Done);
    let Some(PieceOutcome::Posted(posted)) = outcome(&mut writer, "m1") else {
        panic!("piece not posted");
    };
    drop(writer);
    let _restart = deliver::forget_posted_for_tests(CHANNEL);
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    io.port_down.store(true, Ordering::SeqCst);
    let tasks = hosted(&harness, &io, true, &ready);
    polls(3).await;
    assert_eq!(
        io.calls(),
        [("port", 0)],
        "the actor still waits for its gateway"
    );
    assert!(deliver::last_posted(CHANNEL) >= Some(posted));
    abort(tasks);
}

fn p5_log(root: &Path, channel: u64, line: &[u8]) {
    let dir = root.join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{channel}.log")), line).unwrap();
}

fn p5_event(channel: u64, provider: &str, new: p5::BindingTarget) -> Vec<u8> {
    let event = p5::BindingEvent {
        seq: 1,
        channel_id: channel,
        provider: provider.into(),
        tmux_session: "tmux".into(),
        execution_nonce: None,
        old: None,
        new,
        cause: p5::BindingCause::Startup,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: None,
            received_at: Utc::now(),
        },
        committed_at: Utc::now(),
    };
    let mut line = serde_json::to_vec(&event).unwrap();
    line.push(b'\n');
    line
}

#[test]
fn a_channel_binding_log_carries_each_production_event_of_its_channel_and_provider_only() {
    let root = tempfile::tempdir().unwrap();
    p5::set_test_root(Some(root.path()));
    let logs = [(CHANNEL, "claude"), (OTHER, "claude"), (9, "codex")];
    for (channel, provider) in logs {
        let new = p5::BindingTarget::Pending {
            payload_session_id: format!("s{channel}"),
            payload_transcript_path: None,
        };
        p5_log(root.path(), channel, &p5_event(channel, provider, new));
    }
    let log = ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude);
    let events = log.binding_events_since(CHANNEL, 0).unwrap();
    let seen: Vec<_> = events.iter().map(|e| (e.seq, e.channel_id)).collect();
    assert_eq!(seen, [(1, CHANNEL)]);
    let pending = &events[0].record;
    assert!(
        matches!(pending, BindingRecord::Bound { new: BindingTarget::Pending { payload_session_id, .. }, .. } if payload_session_id == "s7")
    );
    assert!(log.binding_events_since(OTHER, 0).is_err());
    let codex = ChannelBindingLog::new(9, ShadowProvider::Claude);
    assert!(codex.binding_events_since(9, 0).is_err());
    p5::set_test_root(None);
}

/// A selected channel before its first activation: no era or init, and a binding log holding
/// `event` for an empty transcript.
fn fresh(event: impl FnOnce(SourceId) -> Vec<u8>) -> (Harness, PathBuf) {
    let harness = Harness::new();
    let store_dir = harness._runtime.path().join("o_store");
    std::fs::remove_dir_all(store_dir.join(CHANNEL.to_string())).unwrap();
    std::fs::remove_file(store_dir.join("o_era")).unwrap();
    let path = harness._runtime.path().join("t.jsonl");
    std::fs::write(&path, b"").unwrap();
    let source = source_id_for("s1", &path).unwrap();
    p5_log(harness._runtime.path(), CHANNEL, &event(source));
    (harness, path)
}

fn startup(source: SourceId) -> Vec<u8> {
    p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source))
}

fn init_path(harness: &Harness, channel: u64) -> PathBuf {
    let dir = harness
        ._runtime
        .path()
        .join("o_store")
        .join(channel.to_string());
    dir.join("init")
}

fn abort(tasks: Vec<tokio::task::JoinHandle<()>>) {
    tasks.iter().for_each(tokio::task::JoinHandle::abort);
}

fn start_host(
    harness: &Harness,
    io: &Arc<TestIo>,
    ready: &Arc<Readiness>,
) -> Vec<tokio::task::JoinHandle<()>> {
    p5::set_test_root(Some(harness._runtime.path()));
    let parts = || HostParts {
        io: Arc::clone(io),
        runtime_root: Some(root(harness)),
        gate: Arc::clone(&harness.gate),
        readiness: Arc::clone(ready),
    };
    start(ShadowProvider::Claude, true, parts)
}

#[tokio::test(start_paused = true)]
async fn a_new_empty_channel_gets_one_first_init_and_a_ready_actor_that_a_restart_reuses() {
    let (harness, path) = fresh(startup);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = start_host(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(io.alarms.halted(), []);
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    let era = harness.store.read_era().unwrap().unwrap();
    assert_eq!(era.initial_channels, [CHANNEL]);
    let init = harness.store.read_init(CHANNEL).unwrap().unwrap();
    let source = source_id_for("s1", &path).unwrap();
    let attached: Vec<_> = init
        .sources
        .iter()
        .map(|s| (&s.source_id, s.delivery_start))
        .collect();
    assert_eq!(attached, [(&source, 0)]);
    assert!(
        ready.is_ready(CHANNEL),
        "the actor resumed over the new init"
    );
    append(&path, &row("m1", "first"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first"]);

    abort(tasks);
    polls(2).await;
    let written = std::fs::read(init_path(&harness, CHANNEL)).unwrap();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let _tasks = start_host(&harness, &io, &ready);
    append(&path, &row("m2", "second"));
    polls(3).await;
    assert!(
        !io.calls().contains(&("facts", CHANNEL)),
        "a restart recovers, it does not activate"
    );
    assert_eq!(
        std::fs::read(init_path(&harness, CHANNEL)).unwrap(),
        written
    );
    assert_eq!(harness.store.read_era().unwrap().unwrap(), era);
    assert!(ready.is_ready(CHANNEL));
    assert_eq!(harness.port.posts(), ["first", "second"]);
}

#[tokio::test(start_paused = true)]
async fn a_channel_that_is_not_new_and_empty_is_held_without_any_store() {
    let facts = |edit: fn(&mut ActivationFacts)| {
        let mut facts = ActivationFacts::default();
        edit(&mut facts);
        Ok(facts)
    };
    let pending = |source| {
        let pending = p5::BindingTarget::Pending {
            payload_session_id: "s2".into(),
            payload_transcript_path: None,
        };
        let mut later: serde_json::Value =
            serde_json::from_slice(&p5_event(CHANNEL, "claude", pending)).unwrap();
        later["seq"] = 2.into();
        [
            startup(source),
            serde_json::to_vec(&later).unwrap(),
            b"\n".to_vec(),
        ]
        .concat()
    };
    let codex = |source| p5_event(CHANNEL, "codex", p5::BindingTarget::Source(source));
    let unbound = |_: SourceId| Vec::new();
    type Case = (
        &'static str,
        Result<ActivationFacts, String>,
        fn(SourceId) -> Vec<u8>,
        &'static [u8],
        bool,
    );
    let cases: [Case; 10] = [
        (
            "open intake rows",
            facts(|f| f.open_intake = 1),
            startup,
            b"",
            true,
        ),
        (
            "sessions on another node",
            facts(|f| f.runner_sessions = 1),
            startup,
            b"",
            true,
        ),
        (
            "node override to runner",
            facts(|f| f.node_override = Some("runner".into())),
            startup,
            b"",
            true,
        ),
        (
            "Legacy retains delivery custody",
            facts(|_| ()),
            startup,
            b"",
            true,
        ),
        ("no PG pool", Err("no PG pool".into()), startup, b"", true),
        ("no PG gateway lease", facts(|_| ()), startup, b"", false),
        ("no source is bound", facts(|_| ()), unbound, b"", true),
        (
            "legacy cursor not established",
            facts(|_| ()),
            startup,
            b"legacy answer\n",
            true,
        ),
        ("still pending", facts(|_| ()), pending, b"", true),
        ("Codex", facts(|_| ()), codex, b"", true),
    ];
    for (why, facts, event, body, pg) in cases {
        let (harness, path) = fresh(event);
        append(&path, body);
        harness.gate.acquired();
        let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
        let io = TestIo::over(&harness);
        *io.facts.lock().unwrap() = facts;
        *io.custody.lock().unwrap() = Ok(why.starts_with("Legacy"));
        let ready = Arc::new(Readiness::default());
        host(&harness, &io, pg, &ready);
        polls(3).await;
        let halted = io.alarms.halted();
        assert!(
            matches!(halted.as_slice(), [(CHANNEL, detail)] if detail.contains(why)),
            "{why}: {halted:?}"
        );
        assert_eq!(harness.store.read_era().unwrap(), None, "{why}");
        assert!(!harness.store.has_channel_dir(CHANNEL), "{why}");
        assert!(!io.calls().iter().any(|(call, _)| *call == "port"), "{why}");
        assert!(
            !ready.is_ready(CHANNEL) && harness.port.posts().is_empty(),
            "{why}"
        );
        // Nothing was written, so Legacy keeps the channel; without a lease nothing was decided.
        let left = if pg {
            Adoption::Released
        } else {
            Adoption::Pending
        };
        assert_eq!(adoption(CHANNEL), left, "{why}");
    }
}

#[tokio::test(start_paused = true)]
async fn missing_or_damaged_store_state_holds_instead_of_a_first_init() {
    let (harness, _, _) = switched_over(&row("m0", "before the switch"));
    harness.gate.acquired();
    std::fs::remove_file(init_path(&harness, CHANNEL)).unwrap();
    let orphan = init_path(&harness, OTHER);
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    let empty = |source| p5_event(OTHER, "claude", p5::BindingTarget::Source(source));
    let path = harness._runtime.path().join("other.jsonl");
    std::fs::write(&path, b"").unwrap();
    p5_log(
        harness._runtime.path(),
        OTHER,
        &empty(source_id_for("s2", &path).unwrap()),
    );
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui), (OTHER, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    assert_eq!(host(&harness, &io, true, &ready), 2);
    polls(3).await;
    let halted = io.alarms.halted();
    let held = |channel, why: &str| halted.iter().any(|(c, d)| *c == channel && d.contains(why));
    assert!(held(CHANNEL, "era channel has no init"), "{halted:?}");
    assert!(held(OTHER, "store files but no init"), "{halted:?}");
    assert_eq!(adoption(OTHER), Adoption::Held, "store files keep O's hold");
    assert!(!init_path(&harness, CHANNEL).exists() && !orphan.exists());
    assert!(
        !io.calls().contains(&("facts", CHANNEL)),
        "an era channel is never activated again"
    );

    let (damaged, _, _) = switched_over(&row("m0", "before the switch"));
    std::fs::write(init_path(&damaged, CHANNEL), b"{\"channel\":").unwrap();
    let (io, ready) = (TestIo::over(&damaged), Arc::new(Readiness::default()));
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    host(&damaged, &io, true, &ready);
    polls(3).await;
    assert!(matches!(io.alarms.halted().as_slice(), [(CHANNEL, d)] if d.contains("StoreDamage")));
    assert_eq!(
        std::fs::read(init_path(&damaged, CHANNEL)).unwrap(),
        b"{\"channel\":"
    );
    assert_eq!(io.calls(), []);
}

#[tokio::test(start_paused = true)]
async fn a_writer_that_stops_takes_no_work_before_its_next_poll() {
    let (harness, path, source) = switched_over(&row("m0", "before the switch"));
    let startup = p5_event(CHANNEL, "claude", p5::BindingTarget::Source(source));
    p5_log(harness._runtime.path(), CHANNEL, &startup);
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    harness.gate.acquired();
    let _hosts = hosted(&harness, &io, true, &ready);
    polls(3).await;
    assert!(ready.accepts(CHANNEL));
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Refused(403));
    append(&path, &row("m1", "refused"));
    for _ in 0..500 {
        if !harness.port.posts().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(harness.port.posts(), ["refused"]);
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(
        io.alarms
            .has(CHANNEL, &WriterAlarm::Blocked { status: 403 })
    );
    assert!(
        !ready.accepts(CHANNEL),
        "a stopped writer takes no work while its poll sleeps"
    );
}

#[tokio::test(start_paused = true)]
async fn a_gate_lost_while_activation_facts_are_read_creates_nothing_until_owned_again() {
    let (harness, _) = fresh(startup);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let gate = Arc::clone(&harness.gate);
    *io.on_facts.lock().unwrap() = Some(Box::new(move || gate.lost()));
    let _tasks = start_host(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(harness.store.read_era().unwrap(), None);
    assert!(!harness.store.has_channel_dir(CHANNEL));
    assert!(!ready.is_ready(CHANNEL) && !ready.accepts(CHANNEL));
    assert_eq!(io.alarms.halted(), [], "a lost gate is waited on, not held");
    harness.gate.acquired();
    polls(3).await;
    let facts = io
        .calls()
        .iter()
        .filter(|call| **call == ("facts", CHANNEL))
        .count();
    assert_eq!(facts, 2, "the facts are read again once Owned");
    let era = harness.store.read_era().unwrap().unwrap();
    assert_eq!(era.initial_channels, [CHANNEL]);
    assert!(ready.accepts(CHANNEL));
    assert_eq!(io.alarms.halted(), []);
}

/// A Claude pane logging channel `CHANNEL` through the real hook judgment, launched on `a_path`.
#[cfg(unix)]
struct ProducerPane {
    tmux: &'static str,
    a: String,
    base: DateTime<Utc>,
    _root: tempfile::TempDir,
    _env: (tempfile::TempDir, [crate::config::TestEnvVarGuard; 2]),
    _rotations: std::sync::MutexGuard<'static, ()>,
    _state: std::sync::MutexGuard<'static, ()>,
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

#[cfg(unix)]
impl ProducerPane {
    fn launch(a: &str, a_path: &Path) -> Self {
        use crate::services::tui_prompt_dedupe as dedupe;
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let env = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        let rotations = dedupe::lock_claude_session_rotations_for_tests();
        dedupe::reset_state_for_tests();
        // An earlier pane's hooks may still wait in the adoption queue this pane shares by name.
        crate::services::claude_tui::hook_server::adoption_retry::reset_deferred_adoptions_for_tests();
        let root = tempfile::tempdir().unwrap();
        p5::set_test_root(Some(root.path()));
        let tmux = "o-superseded-pane";
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let marker = crate::services::tmux_common::session_temp_path(tmux, "spawn_nonce");
        std::fs::write(marker, nonce).unwrap();
        dedupe::register_tmux_channel(tmux, CHANNEL);
        dedupe::register_provider_session("claude", a, tmux);
        let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: ClaudeTui,
            output_path: a_path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(a.to_owned()),
            last_offset: 0,
            relay_last_offset: None,
        };
        dedupe::register_launched_tmux_runtime_binding(tmux, binding);
        Self {
            tmux,
            a: a.to_owned(),
            base: Utc::now(),
            _root: root,
            _env: env,
            _rotations: rotations,
            _state: state,
            _env_lock: env_lock,
        }
    }

    /// Sends hook `event` naming `session`'s transcript, published `secs` after the launch.
    fn send(&self, event: &str, source: Option<&str>, session: &str, path: &Path, secs: i64) {
        use crate::services::claude_tui::hook_server::adoption_retry::adopt_from_hook;
        let payload = serde_json::json!({ "source": source, "transcript_path": path });
        let hook = p5::HookSignal {
            published_at: Some(self.base + TimeDelta::seconds(secs)),
            ..p5::HookSignal::from_payload(event, &payload)
        };
        // Delivery drains each rotation before the next hook, as a settled pane does.
        crate::services::tui_prompt_dedupe::clear_claude_session_rotation(self.tmux);
        adopt_from_hook(&self.a, session, &hook);
    }

    fn bound(&self) -> Option<String> {
        let binding =
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(self.tmux);
        binding.and_then(|binding| binding.session_id)
    }
}

#[cfg(unix)]
impl Drop for ProducerPane {
    fn drop(&mut self) {
        p5::set_test_root(None);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }
}

#[cfg(unix)]
fn session_row(session: &str) -> Vec<u8> {
    let row = serde_json::json!({"type": "mode", "sessionId": session});
    let mut line = serde_json::to_vec(&row).unwrap();
    line.push(b'\n');
    line
}

/// The log the real judgment writes for A → Pending B (clear) → C → resume B → Pending D (clear)
/// resolved, read as O reads it: the superseded Pending B is passed by evidence in the log.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn o_recovers_past_a_superseded_pending_when_a_later_source_is_bound() {
    use super::super::binding::BindingLog;
    let [a, b, c, d] = [(); 4].map(|_| uuid::Uuid::new_v4().to_string());
    let mut bound_a = None;
    let harness = Harness::build(|runtime| {
        let path = runtime.join(format!("{a}.jsonl"));
        let body = session_row(&a);
        std::fs::write(&path, &body).unwrap();
        let source_id = source_id_for(&a, &path).unwrap();
        bound_a = Some(path);
        let delivery_start = body.len() as u64;
        let prefix_hash = hex::encode(Sha256::digest(&body));
        vec![InitSource {
            source_id,
            delivery_start,
            prefix_hash,
        }]
    });
    harness.gate.acquired();
    let a_path = bound_a.unwrap();
    let path = |session: &str| a_path.with_file_name(format!("{session}.jsonl"));
    let pane = ProducerPane::launch(&a, &a_path);
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    append(&a_path, &row("m1", "a tail"));
    let prompt = "user_prompt_submit";
    pane.send("session_start", Some("clear"), &b, &path(&b), 10);
    std::fs::write(path(&c), session_row(&c)).unwrap();
    pane.send(prompt, None, &c, &path(&c), 20);
    std::fs::write(path(&b), session_row(&b)).unwrap();
    pane.send("session_start", Some("resume"), &b, &path(&b), 30);
    pane.send("session_start", Some("clear"), &d, &path(&d), 40);
    std::fs::write(path(&d), session_row(&d)).unwrap();
    pane.send(prompt, None, &d, &path(&d), 41);
    append(&path(&d), &row("n1", "d out"));
    assert_eq!(pane.bound().as_deref(), Some(d.as_str()), "[O:binding_d]");
    let events = bindings.binding_events_since(CHANNEL, 0).unwrap();
    let last = events.last().unwrap().seq;
    let pending = |e: &&BindingEvent| {
        matches!(
            &e.record,
            BindingRecord::Bound {
                new: BindingTarget::Pending { .. },
                ..
            }
        )
    };
    // The only Resolved names D, so B's Pending was passed, not resolved.
    let resolved = |e: &&BindingEvent| matches!(&e.record, BindingRecord::Resolved { source, .. } if source.session_id == d);
    let any = |e: &&BindingEvent| matches!(&e.record, BindingRecord::Resolved { .. });
    let counts = (
        events.iter().filter(pending).count(),
        events.iter().filter(any).count(),
        events.iter().filter(resolved).count(),
    );
    assert_eq!(
        counts,
        (2, 1, 1),
        "[O:pendings] only D's is resolved {events:#?}"
    );
    polls(6).await;
    let store = harness.channel();
    assert_eq!(
        store.binding_checkpoint().unwrap(),
        Some(last),
        "[O:checkpoint]"
    );
    let d_source = source_id_for(&d, &path(&d)).unwrap();
    assert!(store.cursor(&d_source).is_some(), "[O:d_reader]");
    assert_eq!(harness.port.posts(), ["a tail", "d out"], "[O:posts]");
    let stalled = |alarm: &WriterAlarm| matches!(alarm, WriterAlarm::BindingPending { .. });
    assert!(!harness.alarms.taken().iter().any(stalled), "[O:no_wait]");
    let (bound, _) = super::super::adoption::logged(&events).expect("[O:fresh] adoptable");
    assert_eq!(bound.last().map(|s| &s.session_id), Some(&d), "[O:fresh]");

    // A restart of O and of the log writer reads the same log to the same place.
    halt(stop, task).await;
    p5::forget_channel_for_tests(CHANNEL);
    append(&path(&d), &row("n2", "d after restart"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(6).await;
    let posts = harness.port.posts();
    assert_eq!(posts, ["a tail", "d out", "d after restart"], "[O:restart]");
    assert_eq!(
        harness.channel().binding_checkpoint().unwrap(),
        Some(last),
        "[O:restart]"
    );
    assert!(!harness.alarms.taken().iter().any(stalled), "[O:restart]");
    halt(stop, task).await;
}

/// The real judgment logs no record for a resume of the session the pane already holds verified,
/// so that hook proves no old source; a /clear it does log proves every hop of the pane.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_resume_the_log_keeps_unchanged_leaves_the_old_source_read_and_a_logged_clear_retires_it()
{
    use super::super::binding::BindingLog;
    let [a, b, d] = [(); 3].map(|_| uuid::Uuid::new_v4().to_string());
    let mut bound_a = None;
    let harness = Harness::build(|runtime| {
        let path = runtime.join(format!("{a}.jsonl"));
        let body = session_row(&a);
        std::fs::write(&path, &body).unwrap();
        let source_id = source_id_for(&a, &path).unwrap();
        bound_a = Some(path);
        let delivery_start = body.len() as u64;
        let prefix_hash = hex::encode(Sha256::digest(&body));
        vec![InitSource {
            source_id,
            delivery_start,
            prefix_hash,
        }]
    });
    harness.gate.acquired();
    let a_path = bound_a.unwrap();
    let a_source = source_id_for(&a, &a_path).unwrap();
    let path = |session: &str| a_path.with_file_name(format!("{session}.jsonl"));
    let pane = ProducerPane::launch(&a, &a_path);
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    std::fs::write(path(&b), session_row(&b)).unwrap();
    pane.send("user_prompt_submit", None, &b, &path(&b), 10);
    assert_eq!(pane.bound().as_deref(), Some(b.as_str()), "[P21:bound_b]");
    let before = bindings.binding_events_since(CHANNEL, 0).unwrap();
    pane.send("session_start", Some("resume"), &b, &path(&b), 20);
    let after = bindings.binding_events_since(CHANNEL, 0).unwrap();
    assert_eq!(after, before, "[P21:unchanged]");
    polls(15).await;
    let retired = |source: &SourceId| harness.channel().cursor(source).unwrap().retired;
    assert!(!retired(&a_source), "[P21:unproven]");

    pane.send("session_start", Some("clear"), &d, &path(&d), 30);
    std::fs::write(path(&d), session_row(&d)).unwrap();
    pane.send("user_prompt_submit", None, &d, &path(&d), 31);
    assert_eq!(pane.bound().as_deref(), Some(d.as_str()), "[P21:bound_d]");
    polls(15).await;
    let b_source = source_id_for(&b, &path(&b)).unwrap();
    assert!(retired(&a_source), "[P21:clear_proves_a]");
    assert!(retired(&b_source), "[P21:clear_proves_b]");
    halt(stop, task).await;
}
