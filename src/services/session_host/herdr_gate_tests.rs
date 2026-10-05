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
    let (state, stopped) = (conns.clone(), stop.clone());
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
            handlers.push(thread::spawn(move || answer(stream, index, &state)));
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
        thread: Some(thread),
    }
}

fn reply(request: &Value) -> Value {
    let result = match request["method"].as_str().unwrap_or("") {
        "pane.process_info" => json!({"type": "pane_process_info", "process_info": {
            "pane_id": PANE, "shell_pid": SHELL, "foreground_process_group_id": PROVIDER,
            "foreground_processes": [{"pid": PROVIDER, "name": "claude"}, {"pid": CHILD, "name": "node"}]
        }}),
        "session.snapshot" => json!({"type": "session_snapshot", "snapshot": {
            "version": "0.9.3", "protocol": 22, "workspaces": [], "tabs": [], "layouts": [],
            "agents": [], "panes": [{"pane_id": PANE, "terminal_id": "t1", "workspace_id": "w1",
                "tab_id": "w1:1", "focused": false, "agent_status": "idle", "revision": 7}]
        }}),
        _ => json!({"type": "ok"}),
    };
    json!({"id": request["id"], "result": result})
}

fn answer(stream: UnixStream, index: usize, conns: &Mutex<BTreeMap<usize, Seen>>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let request: Option<Value> = serde_json::from_str(&line).ok();
    conns.lock().unwrap().get_mut(&index).unwrap().request = request.clone();
    if let Some(request) = request {
        let _ = writer.write_all(format!("{}\n", reply(&request)).as_bytes());
    }
    // One reply, then Herdr's side is closed; later bytes are only counted.
    let _ = writer.shutdown(Shutdown::Write);
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
    conns.lock().unwrap().get_mut(&index).unwrap().after_reply = rest.len();
}

impl Server {
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
        logical_key: "AgentDesk-claude-herdr".into(),
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

/// A test runtime root holding the launch context, admission open, and the server.
struct Rig {
    server: Server,
    context: PathBuf,
    _admission: herdr_admission::ForcedAdmission,
    _root: TestRuntimeRootGuard,
}

fn rig() -> Rig {
    let root = TestRuntimeRootGuard::new();
    let stop_file = std::env::temp_dir().join(format!("adk-hg-none-{}", uuid::Uuid::new_v4()));
    Rig {
        server: serve(),
        context: context(NONCE),
        _admission: herdr_admission::force_for_test(Admission::new(None, Some(stop_file))),
        _root: root,
    }
}

type Os = fn(&Rig) -> Arc<dyn ProcessOs>;

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
    for written in [
        host.send_text(pane, "x"),
        host.send_keys(pane, &["Enter"]),
        host.interrupt(pane),
    ] {
        assert!(
            matches!(
                written,
                Ok(HostMutation::Refused(HostRefusal::Precondition(_)))
            ),
            "{written:?}"
        );
    }
    assert!(rig.server.sends().is_empty());
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
