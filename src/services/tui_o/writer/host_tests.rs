use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
use crate::services::tui_o::cutover::{self, test_override};
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::host::{HostIo, HostParts, Readiness, start};

use super::*;
use crate::services::tui_prompt_dedupe::binding_events as p5;

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
}

impl TestIo {
    fn over(harness: &Harness) -> Arc<Self> {
        Arc::new(Self {
            port: Arc::clone(&harness.port),
            lease: Arc::clone(&harness.lease),
            alarms: Raised::default(),
            calls: Mutex::default(),
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
        async move { port }
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
    let _empty = test_override::force_channels(&[]);
    assert!(start(ShadowProvider::Claude, true, unprepared).is_empty());
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
    let (foreign, missing) = (10, 11);
    copy_dir(
        &store_dir.join(CHANNEL.to_string()),
        &store_dir.join(foreign.to_string()),
    );
    let _selected = test_override::force_channels(&[(foreign, ClaudeTui), (missing, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    assert_eq!(host(&harness, &io, true, &ready), 2);
    polls(3).await;
    assert_eq!(io.calls(), [], "no gateway, lease or binding is taken");
    assert!(harness.port.posts().is_empty());
    let halted = io.alarms.halted();
    assert_eq!(halted.len(), 2, "{halted:?}");
    let held = |channel: u64, why: &str| {
        halted
            .iter()
            .any(|(c, detail)| *c == channel && detail.contains(why))
    };
    assert!(held(foreign, "init names another channel"), "{halted:?}");
    assert!(held(missing, "channel has no init"), "{halted:?}");
    assert!(!ready.is_ready(foreign) && !ready.is_ready(missing));

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
