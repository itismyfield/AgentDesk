//! The Herdr input executor from its real entry points: a real socket transport against an
//! in-process server that, like Herdr, answers the one request a connection carries and closes.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Output;
use std::sync::atomic::AtomicBool;
use std::thread::{self, JoinHandle};

use serde_json::{Value, json};

use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::db::dispatched_sessions::hosted_execution::{HostedOwner, ProcessStamp, SourceRef};
use crate::services::claude_tui::host_input::{
    self, HerdrInput, HostCapture, HostInputOutcome, InputRefusal, InputRun, InputTarget,
    InputTransport, LegacyTmuxGate, SpyGuard, SpyState, StopCause, classify, run_plan,
};
use crate::services::claude_tui::hosting::FollowupHost;
use crate::services::claude_tui::input::TuiInputAction;
use crate::services::herdr_admission::{self, Admission};
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::session_host::herdr::pane_probe::tests::{FakeOs, NONCE, context, env_naming};
use crate::services::session_host::herdr::provenance::{ProcessStart, StartIdentity};
use crate::services::session_host::herdr::transport::{
    HerdrSocketConfig, HerdrSocketTransport, PeerReader,
};
use crate::services::session_host::herdr::wire::{LineJsonFraming, MAX_FRAME_BYTES};
use crate::services::session_host::herdr_registry::{self, HerdrRegistry};
use crate::services::session_host::{
    HostKind, HostSessionRef, ResolvedSessionTarget, SessionTargetInput, TargetHost, TargetSource,
    host_for,
};

const PANE: &str = "w1-1";
/// The root pane a launch through this server creates.
const LAUNCHED: &str = "w2-1";
const LOGICAL: &str = "AgentDesk-claude-herdr";
const SHELL: u32 = 10;
const PROVIDER: u32 = 20;
const CHILD: u32 = 30;
/// The pid every dial reaches unless a test scripts another server.
const SERVER: u32 = 7;

/// One accepted connection: the request it carried and any bytes written after the reply.
#[derive(Debug, Clone, Default)]
struct Seen {
    request: Option<Value>,
    after_reply: usize,
}

struct Server {
    path: PathBuf,
    conns: Arc<Mutex<BTreeMap<usize, Seen>>>,
    stop: Arc<AtomicBool>,
    version: Arc<Mutex<String>>,
    close_reply: Arc<Mutex<Option<Value>>>,
    /// Once set, a close removes the pane from every later snapshot, as a last-pane close does.
    vanish_on_close: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn socket_path() -> PathBuf {
    static SOCKETS: AtomicU64 = AtomicU64::new(0);
    let n = SOCKETS.fetch_add(1, Ordering::SeqCst);
    PathBuf::from("/tmp").join(format!("adk-hg-{}-{n}.sock", std::process::id()))
}

/// Answers by method, one request per connection, like Herdr.
fn serve() -> Server {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let conns = Arc::new(Mutex::new(BTreeMap::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let version = Arc::new(Mutex::new("0.9.3".into()));
    let close_reply = Arc::new(Mutex::new(None));
    let vanish_on_close = Arc::new(AtomicBool::new(false));
    let gone = Arc::new(AtomicBool::new(false));
    let (state, stopped) = (conns.clone(), stop.clone());
    let (hello_version, closed) = (version.clone(), close_reply.clone());
    let vanish = vanish_on_close.clone();
    let thread = thread::spawn(move || {
        let mut handlers = Vec::new();
        let mut index = 0;
        while !stopped.load(Ordering::SeqCst) {
            let Ok((stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(2));
                continue;
            };
            state.lock().unwrap().insert(index, Seen::default());
            let state = state.clone();
            let (version, close_reply) = (hello_version.clone(), closed.clone());
            let pane = (vanish.clone(), gone.clone());
            handlers.push(thread::spawn(move || {
                answer(stream, index, &state, &version, &close_reply, &pane)
            }));
            index += 1;
        }
        for handler in handlers {
            let _ = handler.join();
        }
    });
    Server {
        path,
        conns,
        stop,
        version,
        close_reply,
        vanish_on_close,
        thread: Some(thread),
    }
}

fn reply(request: &Value, version: &str, close_reply: Option<Value>, gone: bool) -> Value {
    if request["method"] == "pane.close"
        && let Some(mut reply) = close_reply
    {
        reply["id"] = request["id"].clone();
        return reply;
    }
    let panes = match gone {
        true => json!([]),
        false => json!([{"pane_id": PANE, "terminal_id": "t1", "workspace_id": "w1",
            "tab_id": "w1:1", "focused": false, "agent_status": "idle", "revision": 7}]),
    };
    let result = match request["method"].as_str().unwrap_or("") {
        "ping" => json!({"type": "pong", "version": version, "protocol": 22}),
        "pane.process_info" => json!({"type": "pane_process_info", "process_info": {
            "pane_id": PANE, "shell_pid": SHELL, "foreground_process_group_id": PROVIDER,
            "foreground_processes": [{"pid": PROVIDER, "name": "claude"}, {"pid": CHILD, "name": "node"}]
        }}),
        "session.snapshot" => json!({"type": "session_snapshot", "snapshot": {
            "version": "0.9.3", "protocol": 22, "workspaces": [], "tabs": [], "layouts": [],
            "agents": [], "panes": panes
        }}),
        // A launch's root pane opens where it was asked to.
        "workspace.create" => json!({"type": "workspace_created", "root_pane": {
            "pane_id": LAUNCHED, "workspace_id": "w2", "tab_id": "w2:1", "revision": 0,
            "cwd": request["params"]["cwd"]
        }}),
        _ => json!({"type": "ok"}),
    };
    json!({"id": request["id"], "result": result})
}

fn answer(
    stream: UnixStream,
    index: usize,
    conns: &Mutex<BTreeMap<usize, Seen>>,
    version: &Mutex<String>,
    close_reply: &Mutex<Option<Value>>,
    (vanish_on_close, gone): &(Arc<AtomicBool>, Arc<AtomicBool>),
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let request: Option<Value> = serde_json::from_str(&line).ok();
    conns.lock().unwrap().get_mut(&index).unwrap().request = request.clone();
    if let Some(request) = request {
        let reply = reply(
            &request,
            &version.lock().unwrap(),
            close_reply.lock().unwrap().clone(),
            gone.load(Ordering::SeqCst),
        );
        if request["method"] == "pane.close" && vanish_on_close.load(Ordering::SeqCst) {
            gone.store(true, Ordering::SeqCst);
        }
        let _ = writer.write_all(format!("{reply}\n").as_bytes());
    }
    // One reply, then Herdr's side is closed; later bytes are only counted.
    let _ = writer.shutdown(Shutdown::Write);
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
    conns.lock().unwrap().get_mut(&index).unwrap().after_reply = rest.len();
}

impl Server {
    fn mutations(&self) -> Vec<Value> {
        self.conns()
            .into_iter()
            .filter_map(|seen| seen.request)
            .filter(|r| {
                !matches!(
                    r["method"].as_str(),
                    Some("ping" | "session.snapshot" | "pane.process_info")
                )
            })
            .collect()
    }

    /// Every connection, once no new one has been accepted for a few polls: a refused write
    /// dials and hangs up, which the accept loop sees a moment later.
    fn conns(&self) -> Vec<Seen> {
        let count = || self.conns.lock().unwrap().len();
        let mut seen = count();
        loop {
            thread::sleep(Duration::from_millis(30));
            let now = count();
            if now == seen {
                break;
            }
            seen = now;
        }
        self.conns.lock().unwrap().values().cloned().collect()
    }

    /// The writes that reached the pane, in order.
    fn sends(&self) -> Vec<Value> {
        let requests = self.conns().into_iter().filter_map(|seen| seen.request);
        requests
            .filter(|r| {
                r["method"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("pane.send"))
            })
            .map(|r| r["params"].clone())
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn start(seconds: u64) -> ProcessStart {
    ProcessStart {
        identity: StartIdentity::Darwin { seconds, micros: 0 },
        wall_clock: UNIX_EPOCH + Duration::from_secs(seconds),
    }
}

/// The server pid each dial reaches, in dial order; the last one repeats.
fn peers(pids: Vec<u32>) -> PeerReader {
    let pids = Mutex::new(pids);
    Box::new(move |_stream| {
        let mut pids = pids.lock().unwrap();
        let pid = if pids.len() > 1 {
            pids.remove(0)
        } else {
            pids[0]
        };
        Ok((pid, start(1_000)))
    })
}

fn endpoint(server: &Server) -> HerdrEndpoint {
    HerdrEndpoint::new("mac-mini", "mini", &server.path, "agentdesk")
        .and_then(|endpoint| endpoint.with_herdr_home(std::path::Path::new("/adk/herdr")))
        .unwrap()
}

fn transport(server: &Server, pids: Vec<u32>) -> Arc<dyn HerdrTransport> {
    let config = HerdrSocketConfig {
        io_timeout: Duration::from_millis(500),
        read_deadline: Duration::from_millis(500),
        retry_backoff: Duration::from_millis(50),
        max_frame_bytes: MAX_FRAME_BYTES,
    };
    let socket = HerdrSocketTransport::new(&endpoint(server), config, LineJsonFraming);
    Arc::new(socket.with_peer_reader(peers(pids)))
}

type Restore = fn(&dyn HerdrTransport, &HerdrEndpoint) -> RestoreResume;

/// E7 that reads `Off` for whatever server a fresh connection reaches.
fn off_where_dialled(transport: &dyn HerdrTransport, _: &HerdrEndpoint) -> RestoreResume {
    match transport.server_witness() {
        Ok(witness) => RestoreResume::Off { witness },
        Err(why) => RestoreResume::Unverified(why),
    }
}

/// E7 that names another server than the one the pane readings come from.
fn off_elsewhere(_: &dyn HerdrTransport, endpoint: &HerdrEndpoint) -> RestoreResume {
    let witness = ServerWitness {
        socket: endpoint.socket_path().to_path_buf(),
        pid: 99,
        start: start(1_000).identity,
    };
    RestoreResume::Off { witness }
}

fn audited_version(transport: &dyn HerdrTransport, _: &HerdrEndpoint) -> RestoreResume {
    match transport.hello() {
        Ok(hello)
            if crate::services::session_host::herdr::model::VERIFIED_HERDR_VERSIONS
                .contains(&hello.version.as_str()) =>
        {
            RestoreResume::Off {
                witness: hello.witness,
            }
        }
        Ok(_) => RestoreResume::Unverified(RestoreUnverified::VersionNotVerified),
        Err(why) => RestoreResume::Unverified(why),
    }
}

fn not_canonical(_: &dyn HerdrTransport, _: &HerdrEndpoint) -> RestoreResume {
    RestoreResume::Unverified(RestoreUnverified::ConfigNotCanonical)
}

fn stamp(pid: u32, seconds: u64) -> ProcessStamp {
    ProcessStamp {
        pid,
        start: format!("darwin:{seconds}.000000"),
    }
}

fn stored(server: &Server, state: HostedState) -> HostedExecution {
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "hash".into(),
        channel_id: "1".into(),
        logical_key: LOGICAL.into(),
        owner_node: "mac-mini".into(),
        runtime_root: "test".into(),
    };
    HostedExecution {
        schema: 1,
        state,
        execution_nonce: NONCE.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: "mac-mini".into(),
            endpoint_config_key: "mini".into(),
            socket_addr: server.path.display().to_string(),
            named_session: "agentdesk".into(),
            pane_id: PANE.into(),
        }),
        expected: Some(ExpectedExecution {
            binding_provider: "claude".into(),
            binding_nonce: NONCE.into(),
            root: stamp(SHELL, 1_001),
            provider_process: stamp(PROVIDER, 1_002),
            provenance: "herdr_launch:ppid+env;exec=?".into(),
        }),
        source_ref: SourceRef {
            runtime_root: "test".into(),
            channel: "1".into(),
            provider: "claude".into(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: NONCE.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    }
}

fn resolved(pane: &str) -> ResolvedSessionTarget {
    ResolvedSessionTarget {
        input: SessionTargetInput::RawName("AgentDesk-claude-herdr".into()),
        session_key: None,
        host: TargetHost::Known {
            kind: HostKind::Herdr,
            source: TargetSource::SessionRecord,
            name: pane.into(),
        },
    }
}

/// A test runtime root holding the launch context and a Herdr `.host_kind` marker, admission
/// open, and the server.
struct Rig {
    server: Server,
    context: PathBuf,
    _admission: herdr_admission::ForcedAdmission,
    _root: TestRuntimeRootGuard,
}

fn rig() -> Rig {
    let root = TestRuntimeRootGuard::new();
    std::fs::write(marker_path(), "herdr").unwrap();
    let stop_file = std::env::temp_dir().join(format!("adk-hg-none-{}", uuid::Uuid::new_v4()));
    Rig {
        server: serve(),
        context: context(NONCE),
        _admission: herdr_admission::force_for_test(Admission::new(None, Some(stop_file))),
        _root: root,
    }
}

type Os = fn(&Rig) -> Arc<dyn ProcessOs>;

fn marker_path() -> String {
    crate::services::tmux_common::session_temp_path(LOGICAL, "host_kind")
}

/// The launch's own provider: its environment names this execution's context.
fn launched_os(rig: &Rig) -> Arc<dyn ProcessOs> {
    Arc::new(FakeOs::launched(env_naming(&rig.context)))
}

fn replaced_provider(rig: &Rig) -> Arc<dyn ProcessOs> {
    let os = FakeOs::launched(env_naming(&rig.context));
    Arc::new(os.starting(PROVIDER, &[start(1_005)]))
}

fn replaced_root(rig: &Rig) -> Arc<dyn ProcessOs> {
    let os = FakeOs::launched(env_naming(&rig.context));
    Arc::new(os.starting(SHELL, &[start(9)]))
}

struct MissingProviderStamp(FakeOs);

impl ProcessOs for MissingProviderStamp {
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.0.parent(pid)
    }
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        if pid == PROVIDER {
            Err(RestoreUnverified::ProcessUnreadable)
        } else {
            self.0.start(pid)
        }
    }
    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        self.0.environ(pid)
    }
    fn exec_path(&self, pid: u32) -> Option<String> {
        self.0.exec_path(pid)
    }
}

fn missing_provider_stamp(rig: &Rig) -> Arc<dyn ProcessOs> {
    Arc::new(MissingProviderStamp(FakeOs::launched(env_naming(
        &rig.context,
    ))))
}

fn other_nonce(_: &Rig) -> Arc<dyn ProcessOs> {
    let other = context("fedcba9876543210fedcba9876543210");
    Arc::new(FakeOs::launched(env_naming(&other)))
}

fn registry(rig: &Rig, pids: Vec<u32>) -> HerdrRegistry {
    HerdrRegistry::with_transport(endpoint(&rig.server), transport(&rig.server, pids))
}

/// The production chain: this node's registry, the row's execution, then the input target.
fn hosted_target(
    rig: &Rig,
    pids: Vec<u32>,
    restore: Restore,
    os: Arc<dyn ProcessOs>,
) -> InputTarget {
    let _registry = herdr_registry::force_for_test(registry(rig, pids).with_reads((restore, os)));
    let stored = stored(&rig.server, HostedState::Bound);
    InputTarget::from_hosted_target(&resolved(PANE), &stored)
}

fn herdr(target: &InputTarget) -> &HerdrTarget {
    match target {
        InputTarget::Herdr(herdr) => herdr,
        other => panic!("not a Herdr target: {other:?}"),
    }
}

fn run(target: &InputTarget, actions: &[TuiInputAction]) -> InputRun {
    host_input::run_herdr(herdr(target), actions, None)
}

// A prompt reaches a Herdr pane as one bracketed paste and an Enter, each on a connection of
// its own to the server E7 named, with no byte after any reply.
#[test]
fn herdr_plan_pastes_once_bracketed_then_enter_each_on_its_own_connection() {
    let rig = rig();
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    let plan = [
        TuiInputAction::PasteBuffer("첫 줄\n둘째 줄".into()),
        TuiInputAction::Enter,
    ];
    assert_eq!(run(&target, &plan), InputRun::Applied);
    assert_eq!(
        rig.server.sends(),
        [
            json!({"pane_id": PANE, "text": "\u{1b}[200~첫 줄\n둘째 줄\u{1b}[201~"}),
            json!({"pane_id": PANE, "keys": ["enter"]}),
        ]
    );
    let conns = rig.server.conns();
    assert!(
        conns.iter().all(|seen| seen.after_reply == 0),
        "one request per connection: {conns:?}"
    );
}

// The server behind the socket changed between the gate and the write: that connection
// carries 0 bytes and nothing follows.
#[test]
fn a_server_changed_after_the_gate_gets_no_byte() {
    let rig = rig();
    // E7 and the three pane readings reach server 7; the write's connection reaches 8.
    let pids = vec![SERVER, SERVER, SERVER, SERVER, 8];
    let target = hosted_target(&rig, pids, off_where_dialled, launched_os(&rig));
    let stopped = run(
        &target,
        &[TuiInputAction::Literal("x".into()), TuiInputAction::Enter],
    );
    assert!(
        matches!(&stopped, InputRun::Indeterminate { confirmed: 0, cause: StopCause::Send(e) } if e.contains("server changed")),
        "{stopped:?}"
    );
    assert!(rig.server.sends().is_empty());
    let write = rig.server.conns().last().cloned().unwrap();
    assert!(
        write.request.is_none() && write.after_reply == 0,
        "{write:?}"
    );
}

// Every gate check refuses before any write, with its own typed reason.
#[test]
fn a_failed_gate_check_writes_nothing_and_names_why() {
    use HerdrGateRefusal as Why;
    let cases: [(Restore, Os, Why); 4] = [
        (off_where_dialled, replaced_provider, Why::ProviderReplaced),
        (off_where_dialled, other_nonce, Why::OtherNonce),
        (off_elsewhere, launched_os, Why::ServerChanged),
        (
            not_canonical,
            launched_os,
            Why::RestoreUnverified(RestoreUnverified::ConfigNotCanonical),
        ),
    ];
    for (restore, os, why) in cases {
        let rig = rig();
        let target = hosted_target(&rig, vec![SERVER], restore, os(&rig));
        let plan = [TuiInputAction::Literal("x".into()), TuiInputAction::Enter];
        assert_eq!(
            run(&target, &plan),
            InputRun::Refused(InputRefusal::Herdr(why))
        );
        assert!(rig.server.sends().is_empty(), "{why:?}");
    }
}

// A `.host_kind` marker naming another host, or no readable host, refuses input and cancel
// keys alike with nothing written; the same target writes once the marker names Herdr again.
#[test]
fn a_marker_not_naming_herdr_refuses_every_write() {
    use HerdrGateRefusal as Why;
    let rig = rig();
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    let set = |marker: Option<&str>| {
        let path = marker_path();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&path);
        match marker {
            Some("<dir>") => std::fs::create_dir(&path).unwrap(),
            Some(text) => std::fs::write(&path, text).unwrap(),
            None => {}
        }
    };
    let cases = [
        (Some("tmux"), Why::MarkerOtherHost),
        (Some("zellij"), Why::MarkerUnverified),
        (None, Why::MarkerUnverified),
        (Some("<dir>"), Why::MarkerUnverified),
    ];
    for (marker, why) in cases {
        set(marker);
        for plan in [
            &[TuiInputAction::Literal("x".into()), TuiInputAction::Enter][..],
            &[TuiInputAction::Escape][..],
        ] {
            let refused = InputRun::Refused(InputRefusal::Herdr(why));
            assert_eq!(run(&target, plan), refused, "{marker:?}");
        }
        assert!(rig.server.sends().is_empty(), "{marker:?}");
    }
    set(Some("herdr"));
    assert_eq!(run(&target, &[TuiInputAction::Escape]), InputRun::Applied);
    assert_eq!(rig.server.sends().len(), 1);
}

// With admission stopped no input goes out, yet a cancel key still does, after E7 and the pane.
#[test]
fn stopped_admission_refuses_input_but_lets_a_cancel_key_through() {
    let rig = rig();
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    let _off = herdr_admission::force_for_test(Admission::new(Some("off".as_ref()), None));
    let stopped =
        HerdrGateRefusal::AdmissionStopped(crate::services::herdr_admission::StopCause::Env);
    assert_eq!(
        run(&target, &[TuiInputAction::Literal("x".into())]),
        InputRun::Refused(InputRefusal::Herdr(stopped))
    );
    assert!(rig.server.sends().is_empty());
    assert_eq!(run(&target, &[TuiInputAction::Escape]), InputRun::Applied);
    assert_eq!(
        rig.server.sends(),
        [json!({"pane_id": PANE, "keys": ["esc"]})]
    );

    // The cancel key is still refused once the pane no longer runs the stored execution.
    let root_gone = hosted_target(&rig, vec![SERVER], off_where_dialled, replaced_root(&rig));
    assert_eq!(
        run(&root_gone, &[TuiInputAction::Escape]),
        InputRun::Refused(InputRefusal::Herdr(HerdrGateRefusal::RootReplaced))
    );
    assert_eq!(rig.server.sends().len(), 1, "no second cancel");
}

/// A transport no refused target may touch.
struct Untouched;

impl InputTransport for Untouched {
    fn send_literal(&mut self, _: &str, _: &str) -> Result<Output, String> {
        panic!("refused target sent a literal")
    }
    fn load_buffer(&mut self, _: &str, _: &str) -> Result<Output, String> {
        panic!("refused target loaded a buffer")
    }
    fn paste_buffer(&mut self, _: &str, _: &str, _: bool) -> Result<Output, String> {
        panic!("refused target pasted")
    }
    fn send_keys(&mut self, _: &str, _: &[HostKey]) -> Result<Output, String> {
        panic!("refused target sent keys")
    }
    fn capture(&mut self, _: &str, _: i32) -> Option<String> {
        panic!("refused target captured")
    }
    fn pane_alive(&mut self, _: &str) -> bool {
        panic!("refused target probed")
    }
    fn present(&mut self, _: &str) -> bool {
        panic!("refused target probed")
    }
    fn retire(&mut self, _: &str, _: &str, _: &str) {
        panic!("refused target retired")
    }
}

// Only a registered endpoint of this node holding the row's execution makes a target; any
// other pane is refused and no connection is made.
#[test]
fn only_a_registered_local_endpoint_makes_a_herdr_target() {
    let rig = rig();
    let bound = stored(&rig.server, HostedState::Bound);
    let unsupported = InputRefusal::Unsupported(HostKind::Herdr);
    let mut foreign = bound.clone();
    foreign.location.as_mut().unwrap().execution_node = "mac-book".into();
    let mut other_key = bound.clone();
    other_key.location.as_mut().unwrap().endpoint_config_key = "book".into();
    let mut unproven = bound.clone();
    unproven.expected = None;
    let cases = [
        (resolved(PANE), foreign, unsupported),
        (resolved(PANE), other_key, unsupported),
        (
            resolved(PANE),
            stored(&rig.server, HostedState::Retired),
            unsupported,
        ),
        (resolved(PANE), unproven, unsupported),
        (resolved("w1-9"), bound.clone(), InputRefusal::Conflict),
    ];
    let forced = herdr_registry::force_for_test(registry(&rig, vec![SERVER]));
    for (resolved, stored, refusal) in cases {
        let target = InputTarget::from_hosted_target(&resolved, &stored);
        assert_eq!(target, InputTarget::Refused(refusal), "{stored:?}");
        let plan = [TuiInputAction::Escape, TuiInputAction::Enter];
        let ran = run_plan(&target, &LegacyTmuxGate, &mut Untouched, &plan, None);
        assert_eq!(ran, InputRun::Refused(refusal));
    }
    // A Herdr pane that reads missing is never handed to recreation.
    let target = InputTarget::from_hosted_target(&resolved(PANE), &bound);
    assert!(matches!(target, InputTarget::Herdr(_)));
    let outcome = classify(&target, SessionLiveness::Missing, &HostCapture::Unavailable);
    assert!(!outcome.allows_recreate(), "{outcome:?}");
    drop(forced);
    assert_eq!(
        InputTarget::from_hosted_target(&resolved(PANE), &bound),
        InputTarget::Refused(unsupported),
        "no registry, no target"
    );
    assert!(rig.server.conns().is_empty(), "{:?}", rig.server.conns());
}

// A warm follow-up on a Herdr pane retires nothing: no kill, no host call.
#[test]
fn a_herdr_followup_retire_is_refused_and_kills_nothing() {
    let rig = rig();
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    let gate = herdr(&target);
    let spy = SpyGuard::install(SpyState::default());
    let retired = FollowupHost::herdr(gate).retire("stranded_prompt_draft_recreate", "test");
    assert_eq!(
        retired,
        HostInputOutcome::Refused(InputRefusal::Unsupported(HostKind::Herdr))
    );
    HerdrInput::new(gate).retire(PANE, "stranded_prompt_draft_recreate", "test");
    assert!(spy.calls().is_empty(), "{:?}", spy.calls());
    assert!(rig.server.conns().is_empty(), "{:?}", rig.server.conns());
}

// `host_for(Herdr)` reads the one local endpoint but writes nothing: input has only the gate.
#[test]
fn host_for_herdr_reads_the_registry_endpoint_and_never_writes() {
    let rig = rig();
    let _registry = herdr_registry::force_for_test(registry(&rig, vec![SERVER]));
    let host = host_for(HostKind::Herdr);
    let pane = HostSessionRef::herdr_pane(PANE);
    assert_eq!(host.presence(pane), HostPresence::Present);
    let gated = Ok(HostMutation::Refused(HostRefusal::Precondition(
        "herdr_input_goes_through_its_mutation_gate".into(),
    )));
    assert_eq!(host.send_text(pane, "x"), gated);
    assert_eq!(host.send_keys(pane, &["Enter"]), gated);
    assert_eq!(host.interrupt(pane), gated);
    let methods: Vec<Value> = rig
        .server
        .conns()
        .into_iter()
        .map(|seen| seen.request.unwrap()["method"].clone())
        .collect();
    assert_eq!(methods, [json!("session.snapshot")], "no E7, no write");
}

// Only this node's endpoints register; no node id, no section or another node's registers none.
#[test]
fn the_boot_registry_holds_only_this_nodes_endpoints() {
    let built = |local: Option<&str>, channels: &[(u64, &str)]| {
        let _boot = crate::config::session_hosts::force_for_test(local, channels);
        crate::config::session_hosts::with_boot(|boot| HerdrRegistry::build(boot.unwrap()))
    };
    assert!(
        built(Some("mac-mini"), &[(1, "mac-mini")])
            .sole_host()
            .is_some()
    );
    assert!(built(Some("mac-mini"), &[(1, "mac-book")]).is_empty());
    assert!(built(None, &[(1, "mac-mini")]).is_empty());
    assert!(built(Some("mac-mini"), &[]).is_empty());
}

use crate::services::termination_audit::host_terminate::herdr_terminate::{
    HerdrTerminateResult, OperatorTerminateWarrant, TerminateRefusal, terminate_herdr_once,
};

const TERMINATE_TEST_CHILD: &str = "ADK_HERDR_TERMINATE_TEST_CHILD";

// Isolate the live config snapshot from parallel tests that install their own config.
fn run_terminate_child(name: &str) -> bool {
    let name = format!("services::session_host::herdr_gate::tests::{name}");
    if let Some(child) = std::env::var_os(TERMINATE_TEST_CHILD) {
        assert_eq!(child, name.as_str());
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(TERMINATE_TEST_CHILD, &name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success()
            && stdout
                .lines()
                .filter(|line| line.starts_with("test result:"))
                .count()
                == 1
            && stdout.lines().any(|line| line
                .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")),
        "{name}: {}\n{stdout}\n{stderr}",
        output.status
    );
    eprintln!("isolated child verified: {name}; selected=1");
    true
}

struct LiveTerminateSwitch(crate::config::Config);

impl LiveTerminateSwitch {
    fn new() -> Self {
        assert!(std::env::var_os(TERMINATE_TEST_CHILD).is_some());
        let previous = crate::config_live_reload::current()
            .map(|c| (*c).clone())
            .unwrap_or_default();
        let guard = Self(previous);
        guard.set(Some(true));
        guard
    }

    fn set(&self, enabled: Option<bool>) {
        let mut config = self.0.clone();
        config.runtime.herdr_terminate_enabled = enabled;
        crate::config_live_reload::install(config);
    }
}

impl Drop for LiveTerminateSwitch {
    fn drop(&mut self) {
        crate::config_live_reload::install(self.0.clone());
    }
}

fn terminate(target: &InputTarget) -> HerdrTerminateResult {
    terminate_herdr_once(OperatorTerminateWarrant::issue(herdr(target).clone()))
}

#[test]
fn m0_terminate_off_has_zero_effect() {
    if run_terminate_child("m0_terminate_off_has_zero_effect") {
        return;
    }
    let rig = rig();
    let switch = LiveTerminateSwitch::new();
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    for off in [None, Some(false)] {
        switch.set(off);
        let before = rig.server.conns().len();
        let result = terminate(&target);
        assert!(rig.server.mutations().is_empty());
        assert_eq!(
            result,
            HerdrTerminateResult::Refused(TerminateRefusal::Disabled)
        );
        assert_eq!(
            rig.server.conns().len(),
            before,
            "disabled owner performs no I/O"
        );
        herdr(&target).pin_terminate().unwrap();
        assert_eq!(
            herdr(&target).send_close_pinned(),
            CloseEffect::NotSent("herdr_terminate_disabled".into())
        );
        assert!(rig.server.mutations().is_empty());
    }
    switch.set(Some(true));
    herdr(&target).pin_terminate().unwrap();
    switch.set(Some(false));
    assert_eq!(
        herdr(&target).send_close_pinned(),
        CloseEffect::NotSent("herdr_terminate_disabled".into())
    );
    assert!(
        rig.server.mutations().is_empty(),
        "a pinned on snapshot cannot survive switch off"
    );
    switch.set(Some(true));
    assert_eq!(terminate(&target), HerdrTerminateResult::Acknowledged);
    switch.set(Some(false));
    assert_eq!(
        terminate(&target),
        HerdrTerminateResult::Refused(TerminateRefusal::Disabled)
    );
    assert_eq!(rig.server.mutations().len(), 1);
}

#[test]
fn m0_close_effect_positive() {
    if run_terminate_child("m0_close_effect_positive") {
        return;
    }
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    let result = terminate(&target);
    let mutations = rig.server.mutations();
    assert_eq!(mutations.len(), 1);
    assert_eq!(result, HerdrTerminateResult::Acknowledged);
    assert_eq!(mutations[0]["method"], "pane.close");
    assert_eq!(mutations[0]["params"], json!({"pane_id": PANE}));
    assert!(rig.server.conns().iter().all(|seen| seen.after_reply == 0));
    assert!(matches!(
        herdr(&target).send_close_pinned(),
        CloseEffect::NotSent(_)
    ));
    assert_eq!(rig.server.mutations().len(), 1, "one consumed judgment");
}

#[test]
fn m0_terminate_admission_off_still_judges() {
    if run_terminate_child("m0_terminate_admission_off_still_judges") {
        return;
    }
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    let _off = herdr_admission::force_for_test(Admission::new(Some("off".as_ref()), None));
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    let result = terminate(&target);
    assert_eq!(rig.server.mutations().len(), 1);
    assert_eq!(result, HerdrTerminateResult::Acknowledged);
    for (restore, os, why) in [
        (
            off_where_dialled as Restore,
            replaced_provider as Os,
            HerdrGateRefusal::ProviderReplaced,
        ),
        (
            not_canonical as Restore,
            launched_os as Os,
            HerdrGateRefusal::RestoreUnverified(RestoreUnverified::ConfigNotCanonical),
        ),
        (
            off_where_dialled as Restore,
            other_nonce as Os,
            HerdrGateRefusal::OtherNonce,
        ),
    ] {
        let refused = hosted_target(&rig, vec![SERVER], restore, os(&rig));
        assert_eq!(
            terminate(&refused),
            HerdrTerminateResult::Refused(TerminateRefusal::Gate(why))
        );
    }
    std::fs::write(marker_path(), "tmux").unwrap();
    assert_eq!(
        terminate(&target),
        HerdrTerminateResult::Refused(TerminateRefusal::Gate(HerdrGateRefusal::MarkerOtherHost))
    );
    assert_eq!(rig.server.mutations().len(), 1);
}

#[test]
fn m0_close_witness_mismatch_zero_bytes() {
    if run_terminate_child("m0_close_witness_mismatch_zero_bytes") {
        return;
    }
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    let target = hosted_target(
        &rig,
        vec![SERVER, SERVER, SERVER, SERVER, 8],
        off_where_dialled,
        launched_os(&rig),
    );
    assert!(
        matches!(terminate(&target), HerdrTerminateResult::NotSent(why) if why.contains("server changed"))
    );
    assert!(rig.server.mutations().is_empty());
    let write = rig.server.conns().last().cloned().unwrap();
    assert!(
        write.request.is_none() && write.after_reply == 0,
        "{write:?}"
    );
}

#[test]
fn m0_close_requires_both_stamps() {
    if run_terminate_child("m0_close_requires_both_stamps") {
        return;
    }
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    for (os, why) in [
        (replaced_root as Os, HerdrGateRefusal::RootReplaced),
        (replaced_provider as Os, HerdrGateRefusal::ProviderReplaced),
        (
            missing_provider_stamp as Os,
            HerdrGateRefusal::PaneUnverified,
        ),
    ] {
        let target = hosted_target(&rig, vec![SERVER], off_where_dialled, os(&rig));
        let result = terminate(&target);
        assert!(rig.server.mutations().is_empty());
        assert_eq!(
            result,
            HerdrTerminateResult::Refused(TerminateRefusal::Gate(why))
        );
    }
    let mut unproven = stored(&rig.server, HostedState::Bound);
    unproven.expected = None;
    assert!(
        HerdrTarget::new(
            endpoint(&rig.server),
            transport(&rig.server, vec![SERVER]),
            &unproven
        )
        .is_none()
    );
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn m0_terminate_request_pair_is_exact() {
    if run_terminate_child("m0_terminate_request_pair_is_exact") {
        return;
    }
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    let gate = herdr(&target);
    let mut close_results = Vec::new();
    let mut input_results = Vec::new();
    let close = HerdrRequest::PaneClose {
        pane_id: PANE.into(),
    };
    for input in [Mutation::Input, Mutation::Cancel] {
        gate.pin(input).unwrap();
        close_results.push(gate.send_close_pinned());
        gate.pin(input).unwrap();
        input_results.push(gate.send_pinned(close.clone()));
    }
    gate.pin_terminate().unwrap();
    input_results.push(gate.send_pinned(close));
    for request in [
        HerdrRequest::PaneSendText {
            pane_id: PANE.into(),
            text: "x".into(),
        },
        HerdrRequest::PaneSendKeys {
            pane_id: PANE.into(),
            keys: vec!["esc".into()],
        },
        HerdrRequest::PaneSendInput {
            pane_id: PANE.into(),
            text: "x".into(),
            keys: vec!["enter".into()],
        },
    ] {
        gate.pin_terminate().unwrap();
        input_results.push(gate.send_pinned(request));
    }
    assert!(rig.server.mutations().is_empty());
    assert!(
        close_results
            .iter()
            .all(|r| matches!(r, CloseEffect::NotSent(_)))
    );
    assert!(
        input_results
            .iter()
            .all(|r| matches!(r, Ok(HostMutation::Refused(_))))
    );
}

#[test]
fn m0_close_unknown_version_refused() {
    if run_terminate_child("m0_close_unknown_version_refused") {
        return;
    }
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    *rig.server.version.lock().unwrap() = "0.9.4".into();
    let target = hosted_target(&rig, vec![SERVER], audited_version, launched_os(&rig));
    let result = terminate(&target);
    assert!(rig.server.mutations().is_empty());
    assert_eq!(
        result,
        HerdrTerminateResult::Refused(TerminateRefusal::Gate(
            HerdrGateRefusal::TerminateUnsupportedServer
        ))
    );
}

#[test]
fn m0_confirmation_required_no_escalation() {
    if run_terminate_child("m0_confirmation_required_no_escalation") {
        return;
    }
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    *rig.server.close_reply.lock().unwrap() = Some(json!({"error": {
        "code": "confirmation_required", "message": "closing this pane would close a worktree group"
    }}));
    let result = terminate(&target);
    let mutations = rig.server.mutations();
    assert_eq!(mutations.len(), 1);
    assert_eq!(result, HerdrTerminateResult::ConfirmationRequired);
    assert_eq!(mutations[0]["method"], "pane.close");
}

#[test]
fn m0_close_uncertain_reply_never_retries() {
    if run_terminate_child("m0_close_uncertain_reply_never_retries") {
        return;
    }
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    let target = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    for body in [
        json!({"result": {"type": "future_close"}}),
        json!({"error": {"code": "other", "message": "unknown"}}),
    ] {
        *rig.server.close_reply.lock().unwrap() = Some(body);
        assert!(matches!(
            terminate(&target),
            HerdrTerminateResult::Indeterminate(_)
        ));
    }
    assert_eq!(
        rig.server.mutations().len(),
        2,
        "one mutation per explicit request"
    );
    let call = HerdrCall {
        id: "closed".into(),
        request: HerdrRequest::PaneClose {
            pane_id: PANE.into(),
        },
    };
    assert!(matches!(
        contract::close_result(
            &call,
            Err(contract::HerdrTransportError::AfterWrite("lost ACK".into()))
        ),
        CloseEffect::Indeterminate(_)
    ));
}

#[test]
fn m1_production_e7_rejects_unaudited_version_before_provenance() {
    use crate::services::session_host::herdr::observe::{self, ConfigRead, ServerProvenance};
    use std::path::Path;
    struct NoReads;
    impl ServerProvenance for NoReads {
        fn process_start(&self, _: u32) -> Result<ProcessStart, RestoreUnverified> {
            panic!("unknown version must not reach process provenance")
        }
        fn process_env(&self, _: u32, _: &str) -> Result<Vec<String>, RestoreUnverified> {
            panic!("unknown version must not reach environment provenance")
        }
        fn read_config(&self, _: &Path) -> Result<ConfigRead, RestoreUnverified> {
            panic!("unknown version must not reach config provenance")
        }
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let endpoint = endpoint(&rig.server);
    let transport = transport(&rig.server, vec![SERVER]);
    *rig.server.version.lock().unwrap() = "0.9.4".into();
    assert_eq!(
        observe::read_restore_resume_with(transport.as_ref(), &endpoint, &NoReads),
        RestoreResume::Unverified(RestoreUnverified::VersionNotVerified)
    );
}

#[test]
fn m1_production_e7_accepts_audited_version() {
    use crate::services::session_host::herdr::observe::{self, ConfigRead, ServerProvenance};
    use std::path::Path;
    struct Proven;
    impl ServerProvenance for Proven {
        fn process_start(&self, _: u32) -> Result<ProcessStart, RestoreUnverified> {
            Ok(start(1_000))
        }
        fn process_env(&self, _: u32, key: &str) -> Result<Vec<String>, RestoreUnverified> {
            Ok(vec![
                match key {
                    "HERDR_CONFIG_PATH" => "/adk/herdr/config.toml",
                    "XDG_CONFIG_HOME" => "/adk/herdr/xdg",
                    _ => panic!("unexpected key"),
                }
                .into(),
            ])
        }
        fn read_config(&self, _: &Path) -> Result<ConfigRead, RestoreUnverified> {
            Ok(ConfigRead {
                bytes: observe::CANONICAL_CONFIG.to_vec(),
                modified: UNIX_EPOCH + Duration::from_secs(900),
            })
        }
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let transport = transport(&rig.server, vec![SERVER]);
    assert!(matches!(
        observe::read_restore_resume_with(transport.as_ref(), &endpoint(&rig.server), &Proven),
        RestoreResume::Off { .. }
    ));
}

#[test]
fn m1_input_pin_races_terminate_across_fresh_targets() {
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let a = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    let b = hosted_target(&rig, vec![SERVER], off_where_dialled, launched_os(&rig));
    herdr(&a).pin(Mutation::Input).unwrap();
    assert!(matches!(
        herdr(&b).termination_fence(),
        Err(HerdrGateRefusal::MutationBusy)
    ));
    herdr(&a).discard_pin();
    let fence = herdr(&b).termination_fence().unwrap();
    assert!(matches!(
        herdr(&a).pin(Mutation::Input),
        Err(HerdrGateRefusal::MutationBusy)
    ));
    assert_eq!(rig.server.mutations(), Vec::<Value>::new());
    fence.reopen();
    drop(fence);
    herdr(&a).pin(Mutation::Input).unwrap();
    herdr(&a).discard_pin();
}

/// The termination service driven end to end against the fake server, a throwaway PG row and a
/// scripted OS.
mod service {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    pub(super) use crate::services::discord::herdr_terminate::{
        OperatorTerminate, TerminationResult, probe_settlement_window, terminate_explicit_herdr,
        test_queue, test_turn,
    };

    pub(super) const KEY: &str = "claude/hash/mac-mini:AgentDesk-claude-service";
    pub(super) const SUCCESSOR: &str = "fedcba9876543210fedcba9876543210";

    type CloseHook = Box<dyn FnOnce() + Send>;

    /// The recorded pane as the server lists it: gone from the start, after a close, or never.
    pub(super) struct PaneLife {
        inner: Arc<dyn HerdrTransport>,
        gone: AtomicBool,
        vanish_on_close: bool,
        on_close: Mutex<Option<CloseHook>>,
    }

    impl PaneLife {
        pub(super) fn new(rig: &Rig, gone: bool, vanish_on_close: bool) -> Arc<Self> {
            Arc::new(Self {
                inner: transport(&rig.server, vec![SERVER]),
                gone: AtomicBool::new(gone),
                vanish_on_close,
                on_close: Mutex::new(None),
            })
        }

        /// Runs once, after the close reached the server and before the service reads the end.
        pub(super) fn on_close(&self, hook: CloseHook) {
            *self.on_close.lock().unwrap() = Some(hook);
        }
    }

    impl HerdrTransport for PaneLife {
        fn call(
            &self,
            call: &HerdrCall,
        ) -> (
            crate::services::session_host::herdr::contract::HerdrOutcome,
            crate::services::session_host::herdr::contract::Witnessed,
        ) {
            let (mut result, witness) = self.inner.call(call);
            if matches!(call.request, HerdrRequest::SessionSnapshot {})
                && self.gone.load(Ordering::SeqCst)
                && let Ok(reply) = &mut result
                && let Ok(
                    crate::services::session_host::herdr::model::HerdrResult::SessionSnapshot {
                        snapshot,
                    },
                ) = &mut reply.body
            {
                snapshot.panes.clear();
            }
            (result, witness)
        }
        fn call_with_witness(
            &self,
            call: &HerdrCall,
            witness: &ServerWitness,
        ) -> crate::services::session_host::herdr::contract::HerdrOutcome {
            let outcome = self.inner.call_with_witness(call, witness);
            // The service's only witnessed write is its close; `closes` asserts the shape.
            if !call.request.is_read_only() {
                if self.vanish_on_close {
                    self.gone.store(true, Ordering::SeqCst);
                }
                if let Some(hook) = self.on_close.lock().unwrap().take() {
                    hook();
                }
            }
            outcome
        }
        fn server_witness(&self) -> crate::services::session_host::herdr::contract::Witnessed {
            self.inner.server_witness()
        }
        fn hello(
            &self,
        ) -> Result<crate::services::session_host::herdr::contract::ServerHello, RestoreUnverified>
        {
            self.inner.hello()
        }
    }

    /// An OS whose launch evidence can be swapped between requests and whose answer for the
    /// recorded provider's existence is scripted.
    pub(super) struct ScriptedOs {
        inner: Mutex<Arc<dyn ProcessOs>>,
        exists: Result<bool, RestoreUnverified>,
    }

    impl ScriptedOs {
        pub(super) fn new(
            inner: Arc<dyn ProcessOs>,
            exists: Result<bool, RestoreUnverified>,
        ) -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(inner),
                exists,
            })
        }
        pub(super) fn swap(&self, inner: Arc<dyn ProcessOs>) {
            *self.inner.lock().unwrap() = inner;
        }
        fn os(&self) -> Arc<dyn ProcessOs> {
            self.inner.lock().unwrap().clone()
        }
    }

    impl ProcessOs for ScriptedOs {
        fn exists(&self, _: u32) -> Result<bool, RestoreUnverified> {
            self.exists
        }
        fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
            self.os().parent(pid)
        }
        fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
            self.os().start(pid)
        }
        fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
            self.os().environ(pid)
        }
        fn exec_path(&self, pid: u32) -> Option<String> {
            self.os().exec_path(pid)
        }
    }

    /// This node's registry, live switch and one PG row holding `row`, with its hold taken.
    pub(super) struct Service {
        pub(super) pool: sqlx::PgPool,
        db: TestPostgresDb,
        pub(super) shared: Arc<crate::services::discord::SharedData>,
        _registry: herdr_registry::ForcedRegistry,
        _node: crate::config::session_hosts::ForcedSessionHosts,
        _switch: LiveTerminateSwitch,
    }

    impl Service {
        pub(super) async fn start(
            rig: &Rig,
            pane: Arc<PaneLife>,
            os: Arc<ScriptedOs>,
            nonce: &str,
        ) -> Self {
            let _switch = LiveTerminateSwitch::new();
            let _registry = herdr_registry::force_for_test(
                HerdrRegistry::with_transport(endpoint(&rig.server), pane)
                    .with_reads((off_where_dialled, os)),
            );
            let _node = crate::config::session_hosts::force_for_test(Some("mac-mini"), &[]);
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let shared = crate::services::discord::make_shared_data_for_tests();
            let mut record = stored(&rig.server, HostedState::Bound);
            record.owner.discord_token_hash = shared.token_hash.clone();
            record.execution_nonce = nonce.into();
            record.source_ref.execution_nonce = nonce.into();
            if let Some(expected) = record.expected.as_mut() {
                expected.binding_nonce = nonce.into();
            }
            sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, '1', $3)")
                .bind(KEY).bind(&shared.token_hash).bind(serde_json::to_value(&record).unwrap())
                .execute(&pool).await.unwrap();
            crate::services::claude::herdr_turn::hold(nonce).unwrap();
            Self {
                pool,
                db,
                shared,
                _registry,
                _node,
                _switch,
            }
        }

        pub(super) async fn terminate(&self, nonce: &str) -> TerminationResult {
            let request = OperatorTerminate {
                session_key: KEY.into(),
                execution_nonce: nonce.into(),
            };
            terminate_explicit_herdr(request, self.shared.clone(), &self.pool).await
        }

        /// The row's state and nonce.
        pub(super) async fn row(&self) -> (String, String) {
            sqlx::query_as("SELECT hosted_execution->>'state', hosted_execution->>'execution_nonce' FROM sessions WHERE session_key = $1")
                .bind(KEY).fetch_one(&self.pool).await.unwrap()
        }

        pub(super) async fn finish(self) {
            self.pool.close().await;
            self.db.drop().await;
        }
    }

    pub(super) fn held(nonce: &str) -> bool {
        crate::services::claude::herdr_turn::input_holds()
            .unwrap()
            .iter()
            .any(|(held, _)| held == nonce)
    }

    pub(super) fn closes(rig: &Rig) -> Vec<Value> {
        let mutations = rig.server.mutations();
        assert!(
            mutations
                .iter()
                .all(|m| m["params"] == json!({"pane_id": PANE})
                    && m["method"].as_str().is_some_and(|m| m.ends_with(".close"))),
            "{mutations:?}"
        );
        mutations
    }

    pub(super) fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// A launch of `nonce` under the rig's runtime root, its provider alive.
    pub(super) fn launch_of(nonce: &str) -> Arc<dyn ProcessOs> {
        Arc::new(FakeOs::launched(env_naming(&context(nonce))))
    }
}

use service::{PaneLife, ScriptedOs, Service, TerminationResult, block_on, closes, held};

#[test]
fn m1_service_missing_pane_absent_provider_retires_pg() {
    if run_terminate_child("m1_service_missing_pane_absent_provider_retires_pg") {
        return;
    }
    use service::{probe_settlement_window, test_queue};
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, true, false);
    let os = ScriptedOs::new(launched_os(&rig), Ok(false));
    block_on(async {
        let service = Service::start(&rig, pane, os, NONCE).await;
        let channel = serenity::model::id::ChannelId::new(1);
        test_queue::queue_original(&service.shared, channel).await;
        let mut record = stored(&rig.server, HostedState::Bound);
        record.owner.discord_token_hash = service.shared.token_hash.clone();
        let window = Arc::new(Mutex::new(None));
        let (probe_shared, probe_window) = (service.shared.clone(), window.clone());
        probe_settlement_window(Box::new(move || {
            Box::pin(async move {
                // Input that arrives after the hold is gone: a fresh target of the execution and the queue.
                let pane = herdr_registry::registry().target(&record).unwrap();
                let pinned = pane.pin(Mutation::Input);
                pane.discard_pin();
                let taken = test_queue::take_queued(&probe_shared, channel).await;
                *probe_window.lock().unwrap() = Some((
                    pinned,
                    taken,
                    test_queue::queued_texts(&probe_shared, channel).await,
                ));
            })
        }));
        assert_eq!(service.terminate(NONCE).await, TerminationResult::Retired);
        let (pinned, taken, queued) = window
            .lock()
            .unwrap()
            .take()
            .expect("settlement window observed");
        assert!(
            matches!(pinned, Err(HerdrGateRefusal::MutationBusy)),
            "{pinned:?}"
        );
        assert_eq!(taken, None, "no queued start before settlement");
        assert_eq!(queued, vec![test_queue::ORIGINAL.to_string()]);
        assert_eq!(
            test_queue::take_queued(&service.shared, channel)
                .await
                .as_deref(),
            Some(test_queue::ORIGINAL)
        );
        assert_eq!(
            test_queue::take_queued(&service.shared, channel).await,
            None
        );
        assert_eq!(service.row().await.0, "retired");
        assert!(!held(NONCE));
        assert!(
            rig.server.mutations().is_empty(),
            "a missing pane must never be closed again"
        );
        service.finish().await;
    });
}

#[test]
fn m1_last_pane_is_allowed_pg() {
    if run_terminate_child("m1_last_pane_is_allowed_pg") {
        return;
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, false, true);
    let os = ScriptedOs::new(launched_os(&rig), Ok(false));
    block_on(async {
        let service = Service::start(&rig, pane, os, NONCE).await;
        assert_eq!(service.terminate(NONCE).await, TerminationResult::Retired);
        let closes = closes(&rig);
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0]["params"], json!({"pane_id": PANE}));
        assert_eq!(service.row().await, ("retired".into(), NONCE.into()));
        assert!(!held(NONCE));
        service.finish().await;
    });
}

#[test]
fn m1_provider_survived_close_keeps_bound_pg() {
    if run_terminate_child("m1_provider_survived_close_keeps_bound_pg") {
        return;
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, false, true);
    let os = ScriptedOs::new(launched_os(&rig), Ok(true));
    block_on(async {
        let service = Service::start(&rig, pane, os, NONCE).await;
        assert_eq!(
            service.terminate(NONCE).await,
            TerminationResult::ProviderSurvivedClose
        );
        assert_eq!(closes(&rig).len(), 1);
        assert_eq!(service.row().await, ("bound".into(), NONCE.into()));
        assert!(held(NONCE), "a live provider keeps its mailbox hold");
        service.finish().await;
    });
}

#[test]
fn m1_unreadable_process_never_means_absent_pg() {
    if run_terminate_child("m1_unreadable_process_never_means_absent_pg") {
        return;
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, false, true);
    let os = ScriptedOs::new(launched_os(&rig), Err(RestoreUnverified::ProcessUnreadable));
    block_on(async {
        let service = Service::start(&rig, pane, os, NONCE).await;
        assert!(matches!(
            service.terminate(NONCE).await,
            TerminationResult::Indeterminate(_)
        ));
        assert_eq!(closes(&rig).len(), 1);
        assert_eq!(service.row().await, ("bound".into(), NONCE.into()));
        assert!(held(NONCE));
        service.finish().await;
    });
}

#[test]
fn m1_replaced_root_refuses_before_send_pg() {
    if run_terminate_child("m1_replaced_root_refuses_before_send_pg") {
        return;
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, false, true);
    let os = ScriptedOs::new(replaced_root(&rig), Ok(false));
    block_on(async {
        let service = Service::start(&rig, pane, os, NONCE).await;
        assert_eq!(
            service.terminate(NONCE).await,
            TerminationResult::Close(HerdrTerminateResult::Refused(TerminateRefusal::Gate(
                HerdrGateRefusal::RootReplaced
            )))
        );
        assert!(rig.server.mutations().is_empty());
        assert_eq!(service.row().await, ("bound".into(), NONCE.into()));
        assert!(held(NONCE));
        service.finish().await;
    });
}

#[test]
fn m1_afterwrite_never_auto_retries_pg() {
    if run_terminate_child("m1_afterwrite_never_auto_retries_pg") {
        return;
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, false, false);
    let os = ScriptedOs::new(launched_os(&rig), Ok(false));
    *rig.server.close_reply.lock().unwrap() = Some(json!({"result": {"type": "future_close"}}));
    block_on(async {
        let service = Service::start(&rig, pane, os.clone(), NONCE).await;
        assert!(matches!(
            service.terminate(NONCE).await,
            TerminationResult::Indeterminate(_)
        ));
        assert_eq!(
            closes(&rig).len(),
            1,
            "an uncertain close is never sent again on its own"
        );
        assert_eq!(service.row().await, ("bound".into(), NONCE.into()));
        assert!(held(NONCE));
        // A repeated command judges afresh: a root replaced since then refuses before any write.
        os.swap(replaced_root(&rig));
        assert_eq!(
            service.terminate(NONCE).await,
            TerminationResult::Close(HerdrTerminateResult::Refused(TerminateRefusal::Gate(
                HerdrGateRefusal::RootReplaced
            )))
        );
        assert_eq!(closes(&rig).len(), 1);
        assert!(held(NONCE));
        service.finish().await;
    });
}

#[test]
fn m1_confirmation_required_stops_pg() {
    if run_terminate_child("m1_confirmation_required_stops_pg") {
        return;
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, false, false);
    let os = ScriptedOs::new(launched_os(&rig), Ok(false));
    *rig.server.close_reply.lock().unwrap() = Some(json!({"error": {
        "code": "confirmation_required", "message": "closing this pane would close a worktree group"
    }}));
    block_on(async {
        let service = Service::start(&rig, pane, os, NONCE).await;
        assert_eq!(
            service.terminate(NONCE).await,
            TerminationResult::Close(HerdrTerminateResult::ConfirmationRequired)
        );
        assert_eq!(closes(&rig).len(), 1, "no group, workspace or second close");
        assert_eq!(service.row().await, ("bound".into(), NONCE.into()));
        assert!(held(NONCE));
        service.finish().await;
    });
}

#[test]
fn m1_stale_a_never_releases_b_pg() {
    if run_terminate_child("m1_stale_a_never_releases_b_pg") {
        return;
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, false, true);
    let os = ScriptedOs::new(service::launch_of(service::SUCCESSOR), Ok(false));
    block_on(async {
        let service = Service::start(&rig, pane, os, service::SUCCESSOR).await;
        assert_eq!(
            service.terminate(NONCE).await,
            TerminationResult::Refused("execution identity differs".into())
        );
        assert!(
            rig.server.mutations().is_empty(),
            "B's pane is never closed for A"
        );
        assert_eq!(
            service.row().await,
            ("bound".into(), service::SUCCESSOR.into())
        );
        assert!(held(service::SUCCESSOR));
        service.finish().await;
    });
}

#[test]
fn m1_retire_cas_failure_keeps_hold_pg() {
    if run_terminate_child("m1_retire_cas_failure_keeps_hold_pg") {
        return;
    }
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, false, true);
    let os = ScriptedOs::new(launched_os(&rig), Ok(false));
    block_on(async {
        let service = Service::start(&rig, pane.clone(), os, NONCE).await;
        let (pool, runtime) = (service.pool.clone(), tokio::runtime::Handle::current());
        let mut successor = stored(&rig.server, HostedState::Bound);
        successor.owner.discord_token_hash = service.shared.token_hash.clone();
        successor.execution_nonce = service::SUCCESSOR.into();
        // A successor row lands between the close and the retire CAS.
        pane.on_close(Box::new(move || {
            runtime.block_on(async {
                sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
                    .bind(service::KEY)
                    .bind(serde_json::to_value(&successor).unwrap())
                    .execute(&pool)
                    .await
                    .unwrap();
            });
        }));
        assert!(
            matches!(service.terminate(NONCE).await, TerminationResult::Indeterminate(why) if why.contains("CAS"))
        );
        assert_eq!(closes(&rig).len(), 1);
        assert_eq!(
            service.row().await,
            ("bound".into(), service::SUCCESSOR.into())
        );
        assert!(held(NONCE), "a failed CAS never releases the hold");
        service.finish().await;
    });
}

// P2-1: a finalizer that fails before releasing captured turn A is acknowledged
// `AlreadyFinalized`; the retire and hold release that already happened are reported as
// Indeterminate, with A's token, the queue and the counter left exactly as they were.
#[test]
fn m2_already_finalized_with_active_a_is_indeterminate_pg() {
    if run_terminate_child("m2_already_finalized_with_active_a_is_indeterminate_pg") {
        return;
    }
    use service::{test_queue, test_turn};
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, true, false);
    let os = ScriptedOs::new(launched_os(&rig), Ok(false));
    block_on(async {
        let service = Service::start(&rig, pane, os, NONCE).await;
        let channel = serenity::model::id::ChannelId::new(1);
        let token = test_turn::start(&service.shared, channel, 51, "turn-a").await;
        test_queue::queue_original(&service.shared, channel).await;
        test_turn::arm_settlement_panic();
        let result = service.terminate(NONCE).await;
        assert!(
            matches!(&result, TerminationResult::Indeterminate(why) if why.contains("captured turn still active")),
            "{result:?}"
        );
        assert_eq!(service.row().await.0, "retired");
        assert!(
            !held(NONCE),
            "the partial completion is reported, not undone"
        );
        let active = test_turn::active(&service.shared, channel).await;
        assert!(active.is_some_and(|active| Arc::ptr_eq(&active, &token)));
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(
            test_queue::queued_texts(&service.shared, channel).await,
            vec![test_queue::ORIGINAL.to_string()],
            "nothing finishes the channel or starts its queue"
        );
        assert_eq!(
            service.shared.restart.global_active.load(Ordering::Relaxed),
            1
        );
        assert!(rig.server.mutations().is_empty());
        service.finish().await;
    });
}

// P2-1: the captured actor stops answering between the hold release and the settlement check;
// an unanswered read is never an idle mailbox.
#[test]
fn m2_settlement_mailbox_unreadable_is_indeterminate_pg() {
    if run_terminate_child("m2_settlement_mailbox_unreadable_is_indeterminate_pg") {
        return;
    }
    use service::{probe_settlement_window, test_turn};
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, true, false);
    let os = ScriptedOs::new(launched_os(&rig), Ok(false));
    block_on(async {
        let service = Service::start(&rig, pane, os, NONCE).await;
        let channel = serenity::model::id::ChannelId::new(1);
        // The mailbox actor lives on a runtime of its own, so the test can end it mid-service.
        let (started, stop) = (
            std::sync::mpsc::channel(),
            tokio::sync::oneshot::channel::<()>(),
        );
        let (shared, started_tx, stop_rx) = (service.shared.clone(), started.0, stop.1);
        let actor = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let token = test_turn::start(&shared, channel, 51, "turn-a").await;
                    started_tx.send(token).unwrap();
                    let _ = stop_rx.await;
                });
        });
        let token = started.1.recv().unwrap();
        let stopper = Arc::new(Mutex::new(Some((stop.0, actor))));
        let ended = Arc::new(AtomicBool::new(false));
        let (probe_stopper, probe_ended) = (stopper.clone(), ended.clone());
        probe_settlement_window(Box::new(move || {
            Box::pin(async move {
                let (stop, thread) = probe_stopper.lock().unwrap().take().unwrap();
                stop.send(()).unwrap();
                thread.join().unwrap();
                probe_ended.store(true, Ordering::SeqCst);
            })
        }));
        test_turn::arm_settlement_panic();
        let result = service.terminate(NONCE).await;
        assert!(
            ended.load(Ordering::SeqCst),
            "the actor ended inside the window"
        );
        assert!(
            matches!(&result, TerminationResult::Indeterminate(why) if why.contains("unreadable")),
            "{result:?}"
        );
        assert!(
            !test_turn::answers(&service.shared, channel).await,
            "the captured actor is gone"
        );
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(service.row().await.0, "retired");
        assert!(!held(NONCE));
        service.finish().await;
    });
}

// P2-1: the turn captured before the transition lock is replaced by B before the recheck; the
// service refuses with no close, no settlement of B and B's execution hold kept.
#[test]
fn m2_turn_changed_before_close_has_zero_effect_pg() {
    if run_terminate_child("m2_turn_changed_before_close_has_zero_effect_pg") {
        return;
    }
    use crate::services::discord::herdr_terminate::test_turn;
    let _lock = crate::config::shared_test_env_lock();
    let rig = rig();
    let pane = PaneLife::new(&rig, false, true);
    let os = ScriptedOs::new(launched_os(&rig), Ok(false));
    block_on(async {
        let service = Service::start(&rig, pane, os, NONCE).await;
        let channel = serenity::model::id::ChannelId::new(1);
        let token_a = test_turn::start(&service.shared, channel, 51, "turn-a").await;
        assert_eq!(Arc::weak_count(&token_a), 0);
        let transition = service
            .shared
            .session_transition_lock(channel)
            .lock_owned()
            .await;
        let shared = service.shared.clone();
        let (result, token_b) = tokio::join!(service.terminate(NONCE), async {
            // The capture keeps a weak pointer to A; the service then waits on the transition.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while Arc::weak_count(&token_a) == 0 {
                assert!(std::time::Instant::now() < deadline, "A was never captured");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            test_turn::finish_naturally(&shared, channel, 51, "turn-a", &token_a).await;
            let token_b = test_turn::start(&shared, channel, 61, "turn-b").await;
            drop(transition);
            token_b
        });
        assert_eq!(
            result,
            TerminationResult::Refused("turn changed under transition".into())
        );
        assert!(
            rig.server.mutations().is_empty(),
            "no close for a changed turn"
        );
        assert_eq!(service.row().await, ("bound".into(), NONCE.into()));
        assert!(held(NONCE), "the execution keeps its hold");
        let active = test_turn::active(&service.shared, channel).await;
        assert!(active.is_some_and(|active| Arc::ptr_eq(&active, &token_b)));
        assert!(!token_b.cancelled.load(Ordering::Relaxed));
        assert_eq!(
            test_turn::inflight(channel),
            Some((61, Some("turn-b".into())))
        );
        assert_eq!(
            service.shared.restart.global_active.load(Ordering::Relaxed),
            1
        );
        service.finish().await;
    });
}

/// The termination service's fake Herdr server and scripted OS, wired into a runtime the caller
/// owns: its `SharedData`, pool, runtime root and one Herdr-configured channel. Each thread of
/// that runtime reads the boot section's own registry, whose launch host dials the same server.
pub(crate) mod p11 {
    use super::*;
    use crate::config::session_hosts::BootSessionHosts;
    use crate::services::session_host::herdr::observe::CANONICAL_CONFIG;
    use service::ScriptedOs;

    pub(crate) const NODE: &str = "mac-mini";
    pub(crate) const EXECUTION: &str = NONCE;

    /// Re-runs test `path` (its `module_path!()` name) in a child that owns its whole process:
    /// the boot section, the O writer list, and Herdr provenance naming this child as the
    /// server, whose config home the parent wrote before the child started. Returns whether
    /// this is that child.
    pub(crate) fn in_child(path: &str, channel: u64) -> bool {
        let name = path.split_once("::").unwrap().1;
        if let Some(child) = std::env::var_os(TERMINATE_TEST_CHILD) {
            assert_eq!(child, name);
            return true;
        }
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        std::fs::write(&config, CANONICAL_CONFIG).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(3_600);
        let file = std::fs::File::options().write(true).open(&config).unwrap();
        file.set_modified(old).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(TERMINATE_TEST_CHILD, name)
            .env("HERDR_CONFIG_PATH", &config)
            .env("XDG_CONFIG_HOME", home.path().join("xdg"))
            .env_remove(crate::services::herdr_admission::ADMISSION_ENV)
            .env(
                crate::services::tui_o::cutover::test_override::CHILD_ENV,
                "1",
            )
            .env(
                crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
                format!("[[{channel},\"claude_tui\"]]"),
            )
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success()
                && stdout.lines().any(|line| line
                    .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")),
            "{name}: {}\n{stdout}\n{stderr}",
            output.status
        );
        eprintln!("isolated child verified: {name}; selected=1");
        false
    }

    pub(crate) struct P11TerminationRig {
        server: Server,
        boot: BootSessionHosts,
        os: Arc<ScriptedOs>,
        channel: u64,
    }

    impl P11TerminationRig {
        /// The server and the boot section for `channel`, installed for this whole child
        /// process; nothing under the runtime root is written yet.
        pub(crate) fn boot(channel: u64) -> Self {
            assert!(std::env::var_os(TERMINATE_TEST_CHILD).is_some());
            let server = serve();
            server.vanish_on_close.store(true, Ordering::SeqCst);
            let home = PathBuf::from(std::env::var_os("HERDR_CONFIG_PATH").unwrap());
            let config: crate::config::Config = serde_json::from_value(json!({
                "server": {},
                "cluster": {"instance_id": NODE},
                "agents": [{"id": "p11-m2", "name": "P11", "channels": {
                    "claude": {"id": channel.to_string(), "runtime": "tui"}
                }}],
                "session_hosts": {"herdr": {
                    "endpoints": {"mini": {
                        "execution_node": NODE,
                        "socket_path": server.path,
                        "herdr_home": home.parent().unwrap(),
                        "herdr_session": "agentdesk"
                    }},
                    "channels": {channel.to_string(): "mini"}
                }}
            }))
            .unwrap();
            let boot = BootSessionHosts::from_config(&config).unwrap();
            crate::config::session_hosts::install(&config).unwrap();
            let pending: Arc<dyn ProcessOs> = Arc::new(FakeOs::launched(Vec::new()));
            Self {
                server,
                boot,
                os: ScriptedOs::new(pending, Ok(false)),
                channel,
            }
        }

        /// Installs this node's registry and an accepting O writer on the calling thread for the
        /// rest of its life; a runtime runs it on every thread it starts.
        pub(crate) fn thread_installer(&self) -> impl Fn() + Send + Sync + 'static {
            let (boot, os) = (self.boot.clone(), self.os.clone());
            move || {
                let os: Arc<dyn ProcessOs> = os.clone();
                let registry = HerdrRegistry::build(&boot).with_reads((off_where_dialled, os));
                std::mem::forget(herdr_registry::force_for_test(registry));
                std::mem::forget(crate::services::herdr_launch::force_writer_accepts(Some(
                    true,
                )));
            }
        }

        /// Execution `EXECUTION` Bound on the server's pane under `session_key`, launched by
        /// this runtime root, its provider alive and its input hold taken; the live terminate
        /// and Herdr turn switches on.
        pub(crate) async fn bind(
            &self,
            pool: &sqlx::PgPool,
            token_hash: &str,
            session_key: &str,
        ) -> LiveSwitches {
            let logical = session_key.rsplit_once(':').unwrap().1;
            let marker = crate::services::tmux_common::session_temp_path(logical, "host_kind");
            std::fs::write(marker, "herdr").unwrap();
            self.os
                .swap(Arc::new(FakeOs::launched(env_naming(&context(NONCE)))));
            let mut record = stored(&self.server, HostedState::Bound);
            record.owner.discord_token_hash = token_hash.into();
            record.owner.channel_id = self.channel.to_string();
            record.owner.logical_key = logical.into();
            record.source_ref.channel = self.channel.to_string();
            record.source_ref.logical_key = logical.into();
            sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)")
                .bind(session_key).bind(token_hash).bind(self.channel.to_string())
                .bind(serde_json::to_value(&record).unwrap())
                .execute(pool).await.unwrap();
            crate::services::claude::herdr_turn::hold(NONCE).unwrap();
            let switches = LiveSwitches(LiveTerminateSwitch::new());
            let mut config = switches.0.0.clone();
            config.runtime.herdr_terminate_enabled = Some(true);
            config.runtime.herdr_turn_enabled = Some(true);
            crate::config_live_reload::install(config);
            switches
        }

        /// The record `bind` stored, as a fresh input target would read it.
        pub(crate) fn record(&self, token_hash: &str, session_key: &str) -> HostedExecution {
            let logical = session_key.rsplit_once(':').unwrap().1;
            let mut record = stored(&self.server, HostedState::Bound);
            record.owner.discord_token_hash = token_hash.into();
            record.owner.channel_id = self.channel.to_string();
            record.owner.logical_key = logical.into();
            record
        }

        /// Every write the server received, as `(method, pane_id)`, in order.
        pub(crate) fn writes(&self) -> Vec<(String, String)> {
            self.server
                .mutations()
                .into_iter()
                .map(|m| {
                    let method = m["method"].as_str().unwrap_or("").to_owned();
                    let pane = m["params"]["pane_id"].as_str().unwrap_or("").to_owned();
                    (method, pane)
                })
                .collect()
        }
    }

    /// Restores the live config the child started with.
    pub(crate) struct LiveSwitches(LiveTerminateSwitch);

    /// Whether the execution's input hold is still taken.
    pub(crate) fn held() -> bool {
        service::held(NONCE)
    }

    /// A fresh input target of the execution, refused while its termination fence is shut.
    pub(crate) fn input_pin(record: &HostedExecution) -> Result<(), HerdrGateRefusal> {
        let target = herdr_registry::registry()
            .target(record)
            .ok_or(HerdrGateRefusal::NoStoredExecution)?;
        let pinned = target.pin(Mutation::Input);
        target.discard_pin();
        pinned
    }
}
