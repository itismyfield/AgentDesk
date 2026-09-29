//! Real Unix-socket round trips against an in-process scripted server.
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Barrier};
use std::thread::{self, JoinHandle};

use serde_json::{Value, json};

use super::*;
use crate::services::session_host::herdr::model::ExecutionState;
use crate::services::session_host::herdr_host::HerdrHost;
use crate::services::session_host::model::{
    HostLiveness, HostMutation, HostPresence, HostSessionRef,
};
use crate::services::session_host::traits::InteractiveSessionHost;

const PANE: &str = "w1-1";

/// What the server does with one request after the handshake.
#[derive(Clone)]
enum Turn {
    /// Answers with this result, echoing the request id.
    Result(Value),
    /// Answers with a schema error body.
    Remote(&'static str),
    /// Writes these bytes verbatim, whatever the request, then closes.
    Raw(String),
    /// Reads the request and closes without answering.
    Close,
    /// Reads the request, waits, then answers `ok`.
    Late(Duration),
    /// Stops reading, so the client's writes back up.
    Stall(Duration),
    /// Reads the request, waits, reports whether another one was already pending.
    Peek(Duration),
}

#[derive(Clone)]
struct Conn {
    pong: Value,
    turns: Vec<Turn>,
}

fn conn(turns: Vec<Turn>) -> Conn {
    Conn {
        pong: json!({"type": "pong", "version": "0.9.1", "protocol": 22}),
        turns,
    }
}

struct Server {
    path: PathBuf,
    requests: Arc<Mutex<Vec<(usize, Value)>>>,
    overlaps: Arc<AtomicUsize>,
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

/// Serves `conns` in accept order; the last one repeats for later reconnects.
fn serve(conns: Vec<Conn>) -> Server {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let overlaps = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (seen, overlap, stopped) = (requests.clone(), overlaps.clone(), stop.clone());
    let thread = thread::spawn(move || {
        let mut index = 0;
        while !stopped.load(Ordering::SeqCst) {
            let Ok((stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let script = conns[index.min(conns.len() - 1)].clone();
            run_conn(stream, index, script, &seen, &overlap);
            index += 1;
        }
    });
    Server {
        path,
        requests,
        overlaps,
        stop,
        thread: Some(thread),
    }
}

fn run_conn(
    stream: UnixStream,
    index: usize,
    script: Conn,
    seen: &Mutex<Vec<(usize, Value)>>,
    overlap: &AtomicUsize,
) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let next = |reader: &mut BufReader<UnixStream>| {
        let mut line = String::new();
        let request: Value = match reader.read_line(&mut line) {
            Ok(n) if n > 0 => serde_json::from_str(&line).ok()?,
            _ => return None,
        };
        seen.lock().unwrap().push((index, request.clone()));
        Some(request)
    };
    let mut send = |bytes: String| {
        let _ = writer.write_all(bytes.as_bytes());
    };
    let line = |value: Value| format!("{value}\n");
    let Some(ping) = next(&mut reader) else {
        return;
    };
    send(line(json!({"id": ping["id"], "result": script.pong})));
    for turn in script.turns {
        if let Turn::Stall(pause) = turn {
            thread::sleep(pause);
            return;
        }
        let Some(request) = next(&mut reader) else {
            return;
        };
        let id = &request["id"];
        match turn {
            Turn::Result(result) => send(line(json!({"id": id, "result": result}))),
            Turn::Remote(code) => send(line(
                json!({"id": id, "error": {"code": code, "message": "no such pane"}}),
            )),
            Turn::Raw(bytes) => {
                send(bytes);
                return;
            }
            Turn::Close => return,
            Turn::Late(pause) => {
                thread::sleep(pause);
                send(line(json!({"id": id, "result": {"type": "ok"}})));
            }
            Turn::Peek(pause) => {
                thread::sleep(pause);
                if !reader.buffer().is_empty() || pending(reader.get_ref()) {
                    overlap.fetch_add(1, Ordering::SeqCst);
                }
                send(line(json!({"id": id, "result": {"type": "ok"}})));
            }
            Turn::Stall(_) => unreachable!(),
        }
    }
    // Hold the connection until the client drops it.
    let _ = reader.read_to_end(&mut Vec::new());
}

/// Whether the client already sent more bytes, without blocking or consuming them.
fn pending(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut byte = 0u8;
    let flags = libc::MSG_PEEK | libc::MSG_DONTWAIT;
    // SAFETY: a one-byte peek into a live local buffer on an open socket.
    let read = unsafe { libc::recv(stream.as_raw_fd(), (&raw mut byte).cast(), 1, flags) };
    read > 0
}

impl Server {
    fn requests(&self) -> Vec<(usize, Value)> {
        self.requests.lock().unwrap().clone()
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

fn transport(server: &Server, config: HerdrSocketConfig) -> HerdrSocketTransport {
    HerdrSocketTransport::new(&endpoint(server), config, LineJsonFraming)
}

fn host(server: &Server, config: HerdrSocketConfig) -> HerdrHost<HerdrSocketTransport> {
    HerdrHost::new(endpoint(server), transport(server, config))
}

/// A host whose connection is already open, as mutations never open one.
fn connected(server: &Server, config: HerdrSocketConfig) -> HerdrHost<HerdrSocketTransport> {
    let transport = transport(server, config);
    transport.connect().expect("handshake");
    HerdrHost::new(endpoint(server), transport)
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

fn ok() -> Turn {
    Turn::Result(json!({"type": "ok"}))
}

fn indeterminate(outcome: Result<HostMutation, HostError>, why: &str) -> String {
    match outcome {
        Ok(HostMutation::Indeterminate(detail)) => detail,
        other => panic!("{why}: expected Indeterminate, got {other:?}"),
    }
}

#[test]
fn herdr_socket_round_trip_sends_schema_requests() {
    let server = serve(vec![conn(vec![process_info(PANE), ok()])]);
    let herdr = host(&server, config());
    assert_eq!(herdr.execution_pid(pane()), Ok(Some(4242)));
    assert_eq!(herdr.send_text(pane(), "hi\n"), Ok(HostMutation::Confirmed));
    let requests = server.requests();
    assert_eq!(
        requests
            .iter()
            .map(|(_, request)| request.clone())
            .collect::<Vec<_>>(),
        vec![
            json!({"id": "adk-hello-1", "method": "ping", "params": {}}),
            json!({"id": "adk-1", "method": "pane.process_info", "params": {"pane_id": PANE}}),
            json!({"id": "adk-2", "method": "pane.send_text",
                "params": {"pane_id": PANE, "text": "hi\n"}}),
        ]
    );
    assert!(
        requests.iter().all(|(conn, _)| *conn == 0),
        "one connection"
    );
}

#[test]
fn herdr_socket_remote_error_is_a_failed_probe() {
    let server = serve(vec![conn(vec![Turn::Remote("pane_not_found"); 2])]);
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
fn herdr_socket_unclear_replies_never_confirm_input() {
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
        let server = serve(vec![conn(vec![turn])]);
        let herdr = connected(&server, small);
        let detail = indeterminate(herdr.send_text(pane(), "x"), reason);
        assert!(detail.contains(reason), "{reason}: {detail}");
        assert_eq!(
            herdr.send_text(pane(), "y"),
            Err(HostError::Transport("no herdr connection".into())),
            "after an unclear exchange a mutation must not reuse or reopen the socket"
        );
        assert_eq!(
            server.methods(),
            ["ping", "pane.send_text"],
            "{reason}: no resend"
        );
    }
}

#[test]
fn herdr_socket_reply_must_match_id_and_type() {
    let wrong_id = Turn::Raw("{\"id\":\"adk-9\",\"result\":{\"type\":\"ok\"}}\n".into());
    let server = serve(vec![conn(vec![wrong_id])]);
    let herdr = connected(&server, config());
    let why = "a reply for another id must not confirm input";
    let detail = indeterminate(herdr.send_text(pane(), "x"), why);
    assert!(detail.contains("reply id adk-9"), "{detail}");
    let stale = Turn::Raw(
        "{\"id\":\"adk-7\",\"result\":{\"type\":\"pane_process_info\",\"process_info\":{\"pane_id\":\"w1-1\",\"shell_pid\":1}}}\n".into(),
    );
    let server = serve(vec![conn(vec![stale])]);
    let herdr = host(&server, config());
    assert!(
        matches!(herdr.execution_pid(pane()), Err(HostError::Protocol(_))),
        "a reply for another id must not answer this call"
    );
    let server = serve(vec![conn(vec![snapshot()])]);
    let herdr = connected(&server, config());
    let why = "a reply of another type must not confirm input";
    let detail = indeterminate(herdr.send_text(pane(), "x"), why);
    assert!(detail.contains("unexpected result"), "{why}: {detail}");
    let server = serve(vec![conn(vec![ok()])]);
    let herdr = host(&server, config());
    assert!(matches!(
        herdr.execution_pid(pane()),
        Err(HostError::Protocol(_))
    ));
}

#[test]
fn herdr_socket_partial_write_is_indeterminate() {
    let server = serve(vec![conn(vec![Turn::Stall(Duration::from_millis(1500))])]);
    let herdr = connected(&server, config());
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
    let server = serve(vec![
        conn(vec![Turn::Close]),
        conn(vec![process_info(PANE)]),
    ]);
    let herdr = host(&server, config());
    assert_eq!(herdr.execution_pid(pane()), Ok(Some(4242)));
    let conns: Vec<usize> = server.requests().iter().map(|(conn, _)| *conn).collect();
    assert_eq!(conns, [0, 0, 1, 1], "ping+read, then a fresh ping+read");

    let server = serve(vec![conn(vec![Turn::Close])]);
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

#[test]
fn herdr_socket_handshake_checks_protocol_not_version() {
    let pong = |pong: Value| Conn {
        pong,
        turns: Vec::new(),
    };
    let other_version = json!({"type": "pong", "version": "1.4.0-dev", "protocol": 22});
    let server = serve(vec![pong(other_version)]);
    let accepted = transport(&server, config());
    let hello = accepted
        .connect()
        .expect("a version difference alone is accepted");
    assert_eq!((hello.version.as_str(), hello.protocol), ("1.4.0-dev", 22));
    assert_eq!(accepted.generation(), 1);
    for (pong_body, reason) in [
        (
            json!({"type": "pong", "version": "0.9.1", "protocol": 21}),
            "protocol 21",
        ),
        (
            json!({"type": "pong", "version": "0.9.1"}),
            "malformed reply",
        ),
        (json!({"type": "ok"}), "ping answered with"),
    ] {
        let server = serve(vec![pong(pong_body)]);
        let rejected = transport(&server, config());
        let error = format!("{:?}", rejected.connect().unwrap_err());
        assert!(error.contains(reason), "{reason}: {error}");
        assert_eq!(rejected.generation(), 0, "{reason}: no connection kept");
    }
}

#[test]
fn herdr_socket_serializes_mutations_per_connection() {
    let peek = Turn::Peek(Duration::from_millis(250));
    let server = serve(vec![conn(vec![peek.clone(), peek])]);
    let patient = HerdrSocketConfig {
        io_timeout: Duration::from_secs(2),
        ..config()
    };
    let herdr = connected(&server, patient);
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
        server.overlaps.load(Ordering::SeqCst),
        0,
        "one mutation outstanding per connection"
    );
    assert_eq!(
        outcomes,
        [Ok(HostMutation::Confirmed), Ok(HostMutation::Confirmed)]
    );
}

#[test]
fn herdr_socket_observation_never_spans_a_reconnect() {
    let server = serve(vec![
        conn(vec![snapshot(), Turn::Close]),
        conn(vec![process_info(PANE)]),
    ]);
    let herdr = host(&server, config());
    let observation = herdr.observe(pane());
    assert_eq!(observation.presence(), HostPresence::Present);
    assert_eq!(
        (observation.execution, observation.shell_pid),
        (ExecutionState::Unknown, None),
        "a pid read on a new connection must not join the old snapshot"
    );
    assert_eq!(observation.liveness(), HostLiveness::ProbeError);
    let server = serve(vec![conn(vec![snapshot(), process_info(PANE)])]);
    let herdr = host(&server, config());
    assert_eq!(herdr.liveness(pane()), HostLiveness::Live);
}

/// Runs `between` right after a snapshot exchange returns, before the caller resumes.
struct PauseAfterSnapshot<'a> {
    inner: HerdrSocketTransport,
    between: &'a (dyn Fn() + Sync),
}

impl HerdrTransport for PauseAfterSnapshot<'_> {
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, u64) {
        let exchange = self.inner.call(call);
        if call.request == (HerdrRequest::SessionSnapshot {}) {
            (self.between)();
        }
        exchange
    }
}

#[test]
fn herdr_socket_observation_keeps_the_snapshot_generation_across_a_concurrent_reconnect() {
    let server = serve(vec![
        conn(vec![snapshot(), Turn::Close]),
        conn(vec![process_info(PANE); 2]),
    ]);
    let (go, done) = (Barrier::new(2), Barrier::new(2));
    let between = || {
        go.wait();
        done.wait();
    };
    let transport = PauseAfterSnapshot {
        inner: transport(&server, config()),
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
            "the other read reconnected"
        );
        observation
    });
    assert_eq!(observation.presence(), HostPresence::Present);
    assert_eq!(
        (observation.execution, observation.shell_pid),
        (ExecutionState::Unknown, None),
        "a snapshot from the first connection must not join a pid from the second"
    );
}
