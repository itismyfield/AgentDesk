//! Real Unix-socket round trips against an in-process scripted server that, like Herdr,
//! answers the one request a connection carries and closes it.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Barrier};
use std::thread::{self, JoinHandle};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

use super::*;
use crate::services::session_host::herdr::model::ExecutionState;
use crate::services::session_host::herdr::observe::RestoreResume;
use crate::services::session_host::herdr::provenance::StartIdentity;
use crate::services::session_host::herdr_host::HerdrHost;
use crate::services::session_host::model::{
    HostError, HostLiveness, HostMutation, HostPresence, HostSessionRef,
};
use crate::services::session_host::traits::InteractiveSessionHost;

const PANE: &str = "w1-1";

/// What the server does with the one request its connection carries.
#[derive(Clone)]
enum Turn {
    /// Answers with this result, echoing the request id.
    Result(Value),
    /// Answers with a schema error body.
    Remote(&'static str),
    /// Writes these bytes verbatim, whatever the request.
    Raw(String),
    /// Reads the request and closes without answering.
    Close,
    /// Reads the request, waits, then answers `ok`.
    Late(Duration),
    /// Never reads, so the client's writes back up.
    Stall(Duration),
    /// Reads the request, waits, notes whether another request was in flight, answers `ok`.
    Peek(Duration),
}

/// One accepted connection: the request it carried, if any, and every byte it sent.
#[derive(Debug, Clone, Default)]
struct Seen {
    request: Option<Value>,
    bytes: usize,
}

#[derive(Default)]
struct Shared {
    conns: Mutex<BTreeMap<usize, Seen>>,
    in_flight: AtomicUsize,
    overlaps: AtomicUsize,
}

struct Server {
    path: PathBuf,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

static SOCKETS: AtomicUsize = AtomicUsize::new(0);

fn socket_path() -> PathBuf {
    let dir = std::env::temp_dir();
    let dir = if dir.as_os_str().len() > 70 {
        PathBuf::from("/tmp")
    } else {
        dir
    };
    let n = SOCKETS.fetch_add(1, Ordering::SeqCst);
    dir.join(format!("adk-hd-{}-{n}.sock", std::process::id()))
}

/// Serves connection `n` with `turns[n]`, the last turn repeating, each on its own thread.
fn serve(turns: Vec<Turn>) -> Server {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let shared = Arc::new(Shared::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (state, stopped) = (shared.clone(), stop.clone());
    let thread = thread::spawn(move || {
        let mut handlers = Vec::new();
        let mut index = 0;
        while !stopped.load(Ordering::SeqCst) {
            let Ok((stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let turn = turns[index.min(turns.len() - 1)].clone();
            state.conns.lock().unwrap().insert(index, Seen::default());
            let state = state.clone();
            handlers.push(thread::spawn(move || run_conn(stream, index, turn, &state)));
            index += 1;
        }
        for handler in handlers {
            let _ = handler.join();
        }
    });
    Server {
        path,
        shared,
        stop,
        thread: Some(thread),
    }
}

fn run_conn(stream: UnixStream, index: usize, turn: Turn, shared: &Shared) {
    // Fails with EINVAL once the client has already hung up; the read then sees EOF.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    if let Turn::Stall(pause) = turn {
        thread::sleep(pause);
        return;
    }
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let request: Option<Value> = serde_json::from_str(&line).ok();
    let record = |extra: usize| {
        let mut conns = shared.conns.lock().unwrap();
        let seen = conns.entry(index).or_default();
        seen.request = request.clone();
        seen.bytes = line.len() + extra;
    };
    record(0);
    let Some(id) = request.as_ref().map(|request| request["id"].clone()) else {
        return;
    };
    let mut send = |body: Value| {
        let _ = writer.write_all(format!("{body}\n").as_bytes());
    };
    match turn {
        Turn::Result(result) => send(json!({"id": id, "result": result})),
        Turn::Remote(code) => {
            send(json!({"id": id, "error": {"code": code, "message": "no such pane"}}));
        }
        Turn::Raw(bytes) => {
            let _ = writer.write_all(bytes.as_bytes());
        }
        Turn::Close => return,
        Turn::Late(pause) => {
            thread::sleep(pause);
            send(json!({"id": id, "result": {"type": "ok"}}));
        }
        Turn::Peek(pause) => {
            shared.in_flight.fetch_add(1, Ordering::SeqCst);
            thread::sleep(pause);
            if shared.in_flight.load(Ordering::SeqCst) > 1 {
                shared.overlaps.fetch_add(1, Ordering::SeqCst);
            }
            shared.in_flight.fetch_sub(1, Ordering::SeqCst);
            send(json!({"id": id, "result": {"type": "ok"}}));
        }
        Turn::Stall(_) => unreachable!(),
    }
    // Like Herdr: one reply, then the server's side is closed; later bytes are only counted.
    let _ = writer.shutdown(Shutdown::Write);
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
    record(rest.len());
}

impl Server {
    fn conns(&self) -> Vec<Seen> {
        self.shared
            .conns
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    /// Requests in accept order, with the connection that carried each.
    fn requests(&self) -> Vec<(usize, Value)> {
        let conns = self.conns().into_iter().enumerate();
        conns
            .filter_map(|(conn, seen)| seen.request.map(|request| (conn, request)))
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        let requests = self.requests().into_iter();
        requests
            .map(|(_, request)| request["method"].as_str().unwrap_or("?").to_string())
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

fn config() -> HerdrSocketConfig {
    HerdrSocketConfig {
        io_timeout: Duration::from_millis(300),
        read_deadline: Duration::from_millis(600),
        retry_backoff: Duration::from_millis(50),
        max_frame_bytes: MAX_FRAME_BYTES,
    }
}

fn endpoint(server: &Server) -> HerdrEndpoint {
    HerdrEndpoint::new("mac-mini", "pilot", &server.path, "adk").unwrap()
}

fn start(seconds: u64) -> ProcessStart {
    ProcessStart {
        identity: StartIdentity::Darwin { seconds, micros: 0 },
        wall_clock: UNIX_EPOCH + Duration::from_secs(seconds),
    }
}

type Peer = Result<(u32, ProcessStart), RestoreUnverified>;

/// The server each dial reaches, in dial order; the last one repeats.
fn peers(peers: Vec<Peer>) -> PeerReader {
    let peers = Mutex::new(peers);
    Box::new(move |_stream| {
        let mut peers = peers.lock().unwrap();
        if peers.len() > 1 {
            peers.remove(0)
        } else {
            peers[0]
        }
    })
}

const SEVEN: Peer = Ok((
    7,
    ProcessStart {
        identity: StartIdentity::Darwin {
            seconds: 1_000,
            micros: 0,
        },
        wall_clock: UNIX_EPOCH,
    },
));

fn witness(server: &Server, pid: u32, seconds: u64) -> ServerWitness {
    ServerWitness {
        socket: server.path.clone(),
        pid,
        start: start(seconds).identity,
    }
}

fn transport_with(
    server: &Server,
    config: HerdrSocketConfig,
    seen: Vec<Peer>,
) -> HerdrSocketTransport {
    HerdrSocketTransport::new(&endpoint(server), config, LineJsonFraming)
        .with_peer_reader(peers(seen))
}

/// Stands in for E7 having named server 7 on this endpoint.
fn restore_off_on_seven(
    _transport: &HerdrSocketTransport,
    endpoint: &HerdrEndpoint,
) -> RestoreResume {
    RestoreResume::Off {
        witness: ServerWitness {
            socket: endpoint.socket_path().to_path_buf(),
            pid: 7,
            start: start(1_000).identity,
        },
    }
}

fn host_with(
    server: &Server,
    config: HerdrSocketConfig,
    seen: Vec<Peer>,
) -> HerdrHost<HerdrSocketTransport> {
    HerdrHost::new(endpoint(server), transport_with(server, config, seen))
        .with_restore_reader(restore_off_on_seven)
}

/// Every connection reaches server 7, the one E7 named.
fn host(server: &Server, config: HerdrSocketConfig) -> HerdrHost<HerdrSocketTransport> {
    host_with(server, config, vec![SEVEN])
}

fn pane() -> HostSessionRef<'static> {
    HostSessionRef::herdr_pane(PANE)
}

fn process_info(pane_id: &str) -> Turn {
    Turn::Result(json!({"type": "pane_process_info", "process_info": {
        "pane_id": pane_id, "shell_pid": 4242, "foreground_process_group_id": 5151
    }}))
}

fn snapshot() -> Turn {
    Turn::Result(json!({"type": "session_snapshot", "snapshot": {
        "version": "0.9.1", "protocol": 22, "workspaces": [], "tabs": [], "layouts": [],
        "agents": [], "panes": [{
            "pane_id": PANE, "terminal_id": "t1", "workspace_id": "w1", "tab_id": "w1:1",
            "focused": false, "agent_status": "idle", "revision": 7
        }]
    }}))
}

fn pong(version: &str) -> Turn {
    Turn::Result(json!({"type": "pong", "version": version, "protocol": 22}))
}

fn ok() -> Turn {
    Turn::Result(json!({"type": "ok"}))
}

fn send(id: &str) -> HerdrCall {
    HerdrCall {
        id: id.into(),
        request: HerdrRequest::PaneSendText {
            pane_id: PANE.into(),
            text: "x".into(),
        },
    }
}

fn indeterminate(outcome: Result<HostMutation, HostError>, why: &str) -> String {
    match outcome {
        Ok(HostMutation::Indeterminate(detail)) => detail,
        other => panic!("{why}: expected Indeterminate, got {other:?}"),
    }
}

#[test]
fn herdr_socket_round_trip_sends_each_schema_request_on_its_own_connection() {
    let server = serve(vec![process_info(PANE), ok()]);
    let herdr = host(&server, config());
    assert_eq!(herdr.execution_pid(pane()), Ok(Some(4242)));
    assert_eq!(herdr.send_text(pane(), "hi\n"), Ok(HostMutation::Confirmed));
    assert_eq!(
        server.requests(),
        vec![
            (
                0,
                json!({"id": "adk-1", "method": "pane.process_info", "params": {"pane_id": PANE}})
            ),
            (
                1,
                json!({"id": "adk-2", "method": "pane.send_text",
                    "params": {"pane_id": PANE, "text": "hi\n"}})
            ),
        ],
        "one request per connection and no ping in front of it"
    );
    thread::sleep(Duration::from_millis(100));
    assert!(
        server.conns().iter().all(|seen| seen.request.is_some()
            && seen.bytes == format!("{}\n", seen.request.as_ref().unwrap()).len()),
        "nothing pipelined after the request: {:?}",
        server.conns()
    );
}

#[test]
fn herdr_socket_remote_error_is_a_failed_probe() {
    let server = serve(vec![Turn::Remote("pane_not_found")]);
    let herdr = host(&server, config());
    assert_eq!(
        herdr.presence(pane()),
        HostPresence::ProbeFailed,
        "a remote error must not read as Missing"
    );
    assert_eq!(
        herdr.execution_pid(pane()),
        Err(HostError::Remote {
            code: "pane_not_found".into(),
            message: "no such pane".into()
        })
    );
}

#[test]
fn herdr_socket_unclear_replies_never_confirm_input_and_are_never_resent() {
    let small = HerdrSocketConfig {
        max_frame_bytes: 100,
        ..config()
    };
    let long = format!(
        "{{\"id\":\"adk-1\",\"result\":{{\"type\":\"ok\",\"pad\":\"{}\"}}}}\n",
        "x".repeat(200)
    );
    for (turn, reason) in [
        (Turn::Raw("{\"id\":\"adk-1\",".into()), "partial frame"),
        (Turn::Raw(long), "frame over 100 bytes"),
        (Turn::Raw("not json\n".into()), "malformed reply"),
        (Turn::Close, "closed before a reply"),
        (Turn::Late(Duration::from_millis(900)), "timed out"),
    ] {
        let server = serve(vec![turn]);
        let herdr = host(&server, small);
        let detail = indeterminate(herdr.send_text(pane(), "x"), reason);
        assert!(detail.contains(reason), "{reason}: {detail}");
        assert_eq!(server.methods(), ["pane.send_text"], "{reason}: no resend");
    }
}

#[test]
fn herdr_socket_reply_must_match_id_and_type() {
    let wrong_id = Turn::Raw("{\"id\":\"adk-9\",\"result\":{\"type\":\"ok\"}}\n".into());
    let server = serve(vec![wrong_id]);
    let herdr = host(&server, config());
    let why = "a reply for another id must not confirm input";
    let detail = indeterminate(herdr.send_text(pane(), "x"), why);
    assert!(detail.contains("reply id adk-9"), "{detail}");
    let stale = Turn::Raw(
        "{\"id\":\"adk-7\",\"result\":{\"type\":\"pane_process_info\",\"process_info\":{\"pane_id\":\"w1-1\",\"shell_pid\":1}}}\n".into(),
    );
    let server = serve(vec![stale]);
    let herdr = host(&server, config());
    assert!(
        matches!(herdr.execution_pid(pane()), Err(HostError::Protocol(_))),
        "a reply for another id must not answer this call"
    );
    let server = serve(vec![snapshot()]);
    let herdr = host(&server, config());
    let why = "a reply of another type must not confirm input";
    let detail = indeterminate(herdr.send_text(pane(), "x"), why);
    assert!(detail.contains("unexpected result"), "{why}: {detail}");
    let server = serve(vec![ok()]);
    let herdr = host(&server, config());
    assert!(matches!(
        herdr.execution_pid(pane()),
        Err(HostError::Protocol(_))
    ));
}

#[test]
fn herdr_socket_partial_write_is_indeterminate() {
    let server = serve(vec![Turn::Stall(Duration::from_millis(1500))]);
    let herdr = host(&server, config());
    let text = "x".repeat(4 * 1024 * 1024);
    let why = "a partial write must stay Indeterminate";
    let detail = indeterminate(herdr.send_text(pane(), &text), why);
    assert!(
        !detail.starts_with("0 of") && detail.contains(" bytes: "),
        "{why}: {detail}"
    );
}

#[test]
fn herdr_socket_reads_retry_within_the_deadline_on_a_new_connection() {
    let server = serve(vec![Turn::Close, process_info(PANE)]);
    let herdr = host(&server, config());
    assert_eq!(herdr.execution_pid(pane()), Ok(Some(4242)));
    let conns: Vec<usize> = server.requests().iter().map(|(conn, _)| *conn).collect();
    assert_eq!(conns, [0, 1], "the retry dials a new connection");

    let server = serve(vec![Turn::Close]);
    let herdr = host(&server, config());
    let started = Instant::now();
    assert!(matches!(
        herdr.execution_pid(pane()),
        Err(HostError::Transport(_))
    ));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(1500),
        "deadline overrun: {elapsed:?}"
    );
    let reads = server
        .methods()
        .iter()
        .filter(|m| *m == "pane.process_info")
        .count();
    assert!(reads >= 2, "reads retry: {reads}");
}

// The hello is a ping on its own connection; it checks protocol, and E7 the version.
#[test]
fn herdr_socket_hello_checks_protocol_not_version() {
    let server = serve(vec![pong("1.4.0-dev")]);
    let accepted = transport_with(&server, config(), vec![SEVEN]);
    let hello = accepted
        .hello()
        .expect("a version difference alone is accepted");
    assert_eq!(hello.version, "1.4.0-dev");
    assert_eq!(hello.witness, witness(&server, 7, 1_000));
    assert_eq!(server.methods(), ["ping"]);
    for reply in [
        json!({"type": "pong", "version": "0.9.1", "protocol": 21}),
        json!({"type": "pong", "version": "0.9.1"}),
        json!({"type": "ok"}),
    ] {
        let server = serve(vec![Turn::Result(reply.clone())]);
        let rejected = transport_with(&server, config(), vec![SEVEN]);
        assert_eq!(rejected.hello(), Err(RestoreUnverified::NoPeer), "{reply}");
        assert_eq!(server.methods(), ["ping"], "{reply}");
    }
    let server = serve(vec![pong("0.9.3")]);
    let unnamed = transport_with(
        &server,
        config(),
        vec![Err(RestoreUnverified::ProcessUnreadable)],
    );
    assert_eq!(unnamed.hello(), Err(RestoreUnverified::ProcessUnreadable));
}

#[test]
fn herdr_socket_serializes_mutations_per_transport() {
    let server = serve(vec![Turn::Peek(Duration::from_millis(250))]);
    let patient = HerdrSocketConfig {
        io_timeout: Duration::from_secs(2),
        ..config()
    };
    let herdr = host(&server, patient);
    let barrier = Barrier::new(2);
    let outcomes: Vec<_> = thread::scope(|scope| {
        let sends: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|text| {
                let (herdr, barrier) = (&herdr, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    herdr.send_text(pane(), text)
                })
            })
            .collect();
        sends.into_iter().map(|send| send.join().unwrap()).collect()
    });
    assert_eq!(
        server.shared.overlaps.load(Ordering::SeqCst),
        0,
        "one mutation outstanding per transport"
    );
    assert_eq!(
        outcomes,
        [Ok(HostMutation::Confirmed), Ok(HostMutation::Confirmed)]
    );
    assert_eq!(server.requests().len(), 2);
}

// Execution evidence joins a snapshot only when both connections named the same server.
#[test]
fn herdr_socket_observation_joins_only_replies_from_one_known_server() {
    let other_pid = Ok((8, start(1_000)));
    let restarted = Ok((7, start(1_001)));
    let unread = Err(RestoreUnverified::ProcessUnreadable);
    for (seen, joined) in [
        (vec![SEVEN, SEVEN], true),
        (vec![SEVEN, other_pid], false),
        (vec![SEVEN, restarted], false),
        (vec![SEVEN, unread], false),
        (vec![unread, SEVEN], false),
        (vec![unread, unread], false),
    ] {
        let server = serve(vec![snapshot(), process_info(PANE)]);
        let herdr = host_with(&server, config(), seen.clone());
        let observation = herdr.observe(pane());
        assert_eq!(observation.presence(), HostPresence::Present, "{seen:?}");
        let execution = (observation.execution, observation.shell_pid);
        if joined {
            assert_eq!(execution, (ExecutionState::Live, Some(4242)));
            assert_eq!(observation.liveness(), HostLiveness::Live);
        } else {
            assert_eq!(execution, (ExecutionState::Unknown, None), "{seen:?}");
            assert_eq!(observation.liveness(), HostLiveness::ProbeError, "{seen:?}");
        }
    }
}

/// Runs `between` right after a snapshot exchange returns, before the caller resumes.
struct PauseAfterSnapshot<'a> {
    inner: HerdrSocketTransport,
    between: &'a (dyn Fn() + Sync),
}

impl HerdrTransport for PauseAfterSnapshot<'_> {
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        let exchange = self.inner.call(call);
        if call.request == (HerdrRequest::SessionSnapshot {}) {
            (self.between)();
        }
        exchange
    }

    fn call_with_witness(&self, call: &HerdrCall, expected: &ServerWitness) -> HerdrOutcome {
        self.inner.call_with_witness(call, expected)
    }
}

// A read that reached another server in between does not relabel either reply.
#[test]
fn herdr_socket_observation_keeps_each_reply_server_across_a_concurrent_read() {
    let server = serve(vec![snapshot(), process_info(PANE)]);
    let (go, done) = (Barrier::new(2), Barrier::new(2));
    let between = || {
        go.wait();
        done.wait();
    };
    let transport = PauseAfterSnapshot {
        inner: transport_with(&server, config(), vec![SEVEN, Ok((8, start(1_000))), SEVEN]),
        between: &between,
    };
    let herdr = HerdrHost::new(endpoint(&server), transport);
    let observation = thread::scope(|scope| {
        let other = scope.spawn(|| {
            go.wait();
            let pid = herdr.execution_pid(pane());
            done.wait();
            pid
        });
        let observation = herdr.observe(pane());
        assert_eq!(
            other.join().unwrap(),
            Ok(Some(4242)),
            "the other read reached server 8"
        );
        observation
    });
    assert_eq!(
        (observation.execution, observation.shell_pid),
        (ExecutionState::Live, Some(4242)),
        "server 8 in between must not relabel two replies from server 7"
    );
}

// A mutation is written only on a connection whose server is the verified one.
#[test]
fn herdr_socket_writes_a_mutation_only_to_the_verified_server() {
    let verified = |server: &Server| witness(server, 7, 1_000);
    let server = serve(vec![ok()]);
    let transport = transport_with(&server, config(), vec![SEVEN]);
    let outcome = transport.call_with_witness(&send("adk-1"), &verified(&server));
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(server.methods(), ["pane.send_text"]);

    for (reached, why) in [
        (Ok((8, start(1_000))), "another pid"),
        (Ok((7, start(1_001))), "the same pid started again"),
        (
            Err(RestoreUnverified::ProcessUnreadable),
            "an unreadable start",
        ),
        (Err(RestoreUnverified::NoPeer), "no peer pid"),
    ] {
        let server = serve(vec![ok()]);
        let transport = transport_with(&server, config(), vec![reached]);
        let outcome = transport.call_with_witness(&send("adk-1"), &verified(&server));
        assert!(
            matches!(outcome, Err(HerdrTransportError::NotSent(_))),
            "{why}: {outcome:?}"
        );
        thread::sleep(Duration::from_millis(50));
        let conns = server.conns();
        assert_eq!(conns.len(), 1, "{why}: dialled once");
        assert_eq!(conns[0].bytes, 0, "{why}: 0 bytes written");
    }

    let server = serve(vec![ok()]);
    let transport = transport_with(&server, config(), vec![SEVEN]);
    let mut elsewhere = verified(&server);
    elsewhere.socket = PathBuf::from("/tmp/another-endpoint.sock");
    let outcome = transport.call_with_witness(&send("adk-1"), &elsewhere);
    assert!(
        matches!(outcome, Err(HerdrTransportError::NotSent(_))),
        "{outcome:?}"
    );
    let (outcome, _) = transport.call(&send("adk-2"));
    assert!(
        matches!(outcome, Err(HerdrTransportError::NotSent(_))),
        "a mutation without a witness is refused: {outcome:?}"
    );
    thread::sleep(Duration::from_millis(50));
    assert!(server.conns().is_empty(), "neither dialled");
}

// On this OS the same server reads as one witness on every connection, so its new
// connections carry input, and a request-free check sends nothing.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn herdr_socket_names_the_same_accepting_process_on_every_connection() {
    let server = serve(vec![pong("0.9.3"), ok(), ok()]);
    let transport = HerdrSocketTransport::new(&endpoint(&server), config(), LineJsonFraming);
    let hello = transport.hello().expect("hello");
    assert_eq!(hello.witness.pid, std::process::id());
    assert_eq!(transport.server_witness(), Ok(hello.witness.clone()));
    let outcome = transport.call_with_witness(&send("adk-1"), &hello.witness);
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(server.methods(), ["ping", "pane.send_text"]);
    thread::sleep(Duration::from_millis(50));
    let conns = server.conns();
    assert_eq!(conns.len(), 3);
    assert_eq!(
        (conns[1].request.clone(), conns[1].bytes),
        (None, 0),
        "server_witness sends no request"
    );
}
