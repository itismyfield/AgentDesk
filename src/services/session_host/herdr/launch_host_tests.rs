//! SocketHerdrLaunchHost against a scripted socket server that, like Herdr, answers one
//! request per connection, and whose serving process a test can replace.
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use serde_json::{Value, json};

use super::*;
use crate::services::session_host::herdr::observe::{
    CANONICAL_CONFIG, ConfigRead, ServerProvenance, read_restore_resume_with,
};
use crate::services::session_host::herdr::provenance::{ProcessStart, StartIdentity};
use crate::services::session_host::herdr::wire::LineJsonFraming;

const HOME: &str = "/srv/herdr-home";

/// Each serving pid has its own start, so a replaced server never reads as the old one.
fn start_of(pid: u32) -> ProcessStart {
    ProcessStart {
        identity: StartIdentity::Darwin {
            seconds: 1_000 + u64::from(pid),
            micros: 0,
        },
        wall_clock: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
    }
}

/// Every process is a bootstrapped server; the scripted socket carries the rest.
struct Bootstrapped;

impl ServerProvenance for Bootstrapped {
    fn process_start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        Ok(start_of(pid))
    }

    fn process_env(&self, _pid: u32, key: &str) -> Result<Vec<String>, RestoreUnverified> {
        let value = match key {
            "HERDR_CONFIG_PATH" => format!("{HOME}/config.toml"),
            _ => format!("{HOME}/xdg"),
        };
        Ok(vec![value])
    }

    fn read_config(&self, _path: &Path) -> Result<ConfigRead, RestoreUnverified> {
        Ok(ConfigRead {
            bytes: CANONICAL_CONFIG.to_vec(),
            modified: SystemTime::UNIX_EPOCH,
        })
    }
}

thread_local! {
    /// After this many real E7 readings on this thread, the next one reads as scripted.
    static SCRIPTED_E7: RefCell<Option<(usize, RestoreResume)>> = const { RefCell::new(None) };
}

fn scripted_e7(transport: &HerdrSocketTransport, endpoint: &HerdrEndpoint) -> RestoreResume {
    let scripted = SCRIPTED_E7.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.take() {
            Some((0, reading)) => Some(reading),
            Some((left, reading)) => {
                *slot = Some((left - 1, reading));
                None
            }
            None => None,
        }
    });
    scripted.unwrap_or_else(|| read_restore_resume_with(transport, endpoint, &Bootstrapped))
}

/// The result for one request; it may replace the serving process as it answers.
type Reply = Box<dyn Fn(&Value, &AtomicU32) -> Value + Send + Sync>;

/// One accepted connection: the request it carried, if any, and every byte it sent.
type Conn = (Option<Value>, usize);

struct Server {
    path: PathBuf,
    /// The pid of the process accepting connections, as the peer reader reports it.
    serving: Arc<AtomicU32>,
    /// Every connection in accept order: the method it carried, if any, and its bytes.
    conns: Arc<Mutex<Vec<Conn>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Answers the one request on each connection with `reply`'s result, then closes it.
fn serve(reply: Reply) -> Server {
    let name = format!("adk-lh-{}-{}.sock", std::process::id(), socket_number());
    let path = std::env::temp_dir().join(&name);
    let path = if path.as_os_str().len() > 90 {
        Path::new("/tmp").join(name)
    } else {
        path
    };
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let serving = Arc::new(AtomicU32::new(7));
    let conns = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (process, seen, stopped) = (serving.clone(), conns.clone(), stop.clone());
    let thread = std::thread::spawn(move || {
        while !stopped.load(Ordering::SeqCst) {
            let Ok((stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            let index = {
                let mut seen = seen.lock().unwrap();
                seen.push((None, 0));
                seen.len() - 1
            };
            answer(stream, &reply, &process, &|conn| {
                seen.lock().unwrap()[index] = conn
            });
        }
    });
    Server {
        path,
        serving,
        conns,
        stop,
        thread: Some(thread),
    }
}

fn socket_number() -> u64 {
    static SOCKETS: AtomicU64 = AtomicU64::new(0);
    SOCKETS.fetch_add(1, Ordering::SeqCst)
}

/// `record` runs before the reply goes out, so a client that has its reply finds it logged.
fn answer(
    stream: UnixStream,
    reply: &Reply,
    serving: &AtomicU32,
    record: &dyn Fn((Option<Value>, usize)),
) {
    // Fails with EINVAL once the client has already hung up; the read then sees EOF.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let Ok(request) = serde_json::from_str::<Value>(&line) else {
        return record((None, line.len()));
    };
    record((Some(request.clone()), line.len()));
    let result = match request["method"].as_str() {
        Some("ping") => json!({"type": "pong", "version": "0.9.3", "protocol": 22}),
        _ => reply(&request, serving),
    };
    let body = json!({"id": request["id"], "result": result});
    let _ = writer.write_all(format!("{body}\n").as_bytes());
    let _ = writer.shutdown(Shutdown::Write);
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
    record((Some(request), line.len() + rest.len()));
}

impl Server {
    fn methods(&self) -> Vec<String> {
        let conns = self.conns.lock().unwrap();
        let method = |request: &Value| request["method"].as_str().unwrap_or("?").to_string();
        conns
            .iter()
            .filter_map(|(request, _)| request.as_ref().map(method))
            .collect()
    }

    fn request(&self, method: &str) -> Option<Value> {
        let conns = self.conns.lock().unwrap();
        let found = conns
            .iter()
            .filter_map(|(request, _)| request.as_ref())
            .find(|request| request["method"] == method);
        found.map(|request| request["params"].clone())
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

fn launch_endpoint(server: &Server) -> HerdrLaunchEndpoint {
    HerdrLaunchEndpoint {
        execution_node: "mac-mini".into(),
        config_key: "pilot".into(),
        socket_addr: server.path.display().to_string(),
        herdr_session: "adk".into(),
    }
}

/// A host whose connections name the server's current serving pid as their peer.
fn launch_host(server: &Server) -> SocketHerdrLaunchHost {
    let endpoint = HerdrEndpoint::new("mac-mini", "pilot", &server.path, "adk")
        .unwrap()
        .with_herdr_home(Path::new(HOME))
        .unwrap();
    let config = HerdrSocketConfig {
        io_timeout: Duration::from_millis(500),
        ..HerdrSocketConfig::default()
    };
    let mut host = SocketHerdrLaunchHost::new(vec![endpoint], config);
    host.read_restore = scripted_e7;
    let (endpoint, transport) = host.endpoints.pop().unwrap();
    let serving = server.serving.clone();
    let transport = transport.with_peer_reader(Box::new(move |_stream| {
        let pid = serving.load(Ordering::SeqCst);
        Ok((pid, start_of(pid)))
    }));
    host.endpoints.push((endpoint, transport));
    host
}

fn workdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    (dir, real)
}

/// Answers `workspace.create` with a root pane in `cwd`, after `on_create` has run.
fn created_then(cwd: String, on_create: fn(&AtomicU32)) -> Reply {
    Box::new(move |request, serving| match request["method"].as_str() {
        Some("workspace.create") => {
            on_create(serving);
            json!({"type": "workspace_created", "root_pane": {
                "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "revision": 0,
                "cwd": cwd
            }})
        }
        _ => json!({"type": "ok"}),
    })
}

fn created(cwd: String) -> Reply {
    created_then(cwd, |_| {})
}

fn verified_request(
    server: &Server,
    host: &SocketHerdrLaunchHost,
    cwd: &Path,
) -> HerdrCreateRequest {
    let endpoint = launch_endpoint(server);
    let reading = host.restore_resume(&endpoint);
    HerdrCreateRequest {
        endpoint,
        label: "AgentDesk-claude-x".into(),
        cwd: cwd.to_path_buf(),
        command: "bash /run/launch.sh".into(),
        restore_off_witness: reading.admitted_witness().expect("E7 off"),
    }
}

fn unconfirmed_pane(outcome: &HerdrCreateOutcome) -> Option<&str> {
    match outcome {
        HerdrCreateOutcome::CreatedUnconfirmed { pane_id, .. } => Some(pane_id),
        _ => None,
    }
}

// Ping, create and input each go out on their own connection, all to the one server.
#[test]
fn socket_launch_types_the_command_into_the_server_that_created_the_pane() {
    let (_dir, cwd) = workdir();
    let server = serve(created(cwd.display().to_string()));
    let host = launch_host(&server);
    let request = verified_request(&server, &host, &cwd);
    assert_eq!(
        host.create(&request),
        HerdrCreateOutcome::Created {
            pane_id: "w1:p1".into()
        }
    );
    assert_eq!(
        server.methods(),
        [
            "ping",
            "ping",
            "workspace.create",
            "ping",
            "pane.send_input"
        ]
    );
    std::thread::sleep(Duration::from_millis(200));
    for (request, bytes) in server.conns.lock().unwrap().iter() {
        let carried = request
            .as_ref()
            .map_or(0, |request| format!("{request}").len() + 1);
        assert_eq!(
            *bytes, carried,
            "one request at most per connection: {request:?}"
        );
    }
    assert_eq!(
        server.request("pane.send_input"),
        Some(json!({"pane_id": "w1:p1", "text": "bash /run/launch.sh", "keys": ["enter"]}))
    );
    assert_eq!(
        server.request("workspace.create"),
        Some(
            json!({"cwd": cwd.display().to_string(), "label": "AgentDesk-claude-x", "focus": false})
        )
    );
}

// Once the pane exists, a replaced server, a lost or foreign E7 reading, or another cwd
// withholds the command and keeps the pane; nothing is created on a replaced server.
#[test]
fn socket_launch_types_nothing_into_a_replaced_server_and_keeps_the_pane() {
    let (_dir, cwd) = workdir();
    let replace = |serving: &AtomicU32| serving.store(8, Ordering::SeqCst);
    let server = serve(created_then(cwd.display().to_string(), replace));
    let host = launch_host(&server);
    let outcome = host.create(&verified_request(&server, &host, &cwd));
    assert_eq!(unconfirmed_pane(&outcome), Some("w1:p1"), "{outcome:?}");
    assert_eq!(
        server.request("pane.send_input"),
        None,
        "replaced after create"
    );

    let foreign = |server: &Server| RestoreResume::Off {
        witness: ServerWitness {
            socket: server.path.clone(),
            pid: 8,
            start: start_of(8).identity,
        },
    };
    let lost = |_: &Server| RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart);
    for (reading, why) in [
        (
            foreign as fn(&Server) -> RestoreResume,
            "E7 names another server",
        ),
        (lost, "config changed after create"),
    ] {
        let server = serve(created(cwd.display().to_string()));
        let host = launch_host(&server);
        let request = verified_request(&server, &host, &cwd);
        SCRIPTED_E7.with(|slot| *slot.borrow_mut() = Some((1, reading(&server))));
        let outcome = host.create(&request);
        SCRIPTED_E7.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(
            unconfirmed_pane(&outcome),
            Some("w1:p1"),
            "{why}: {outcome:?}"
        );
        assert_eq!(server.request("pane.send_input"), None, "{why}");
    }

    let server = serve(created("/Users/elsewhere".into()));
    let host = launch_host(&server);
    let outcome = host.create(&verified_request(&server, &host, &cwd));
    assert_eq!(unconfirmed_pane(&outcome), Some("w1:p1"), "{outcome:?}");
    assert_eq!(server.request("pane.send_input"), None, "opened elsewhere");

    let server = serve(created(cwd.display().to_string()));
    let host = launch_host(&server);
    let request = verified_request(&server, &host, &cwd);
    server.serving.store(8, Ordering::SeqCst);
    assert!(matches!(
        host.create(&request),
        HerdrCreateOutcome::NotSent(_)
    ));
    assert_eq!(
        server.request("workspace.create"),
        None,
        "verified, then the server was replaced"
    );
}

// An unknown endpoint or a request that fails the local checks reaches no server.
#[test]
fn socket_launch_refuses_unknown_endpoints_and_bad_requests_before_any_call() {
    let (_dir, cwd) = workdir();
    let file = cwd.join("file");
    std::fs::write(&file, b"x").unwrap();
    let server = serve(created(cwd.display().to_string()));
    let host = launch_host(&server);
    let mut unknown = launch_endpoint(&server);
    unknown.config_key = "other".into();
    assert_eq!(
        host.restore_resume(&unknown),
        RestoreResume::Unverified(RestoreUnverified::NotBootstrapped)
    );
    let mut bad = verified_request(&server, &host, &cwd);
    bad.endpoint = unknown;
    assert!(matches!(host.create(&bad), HerdrCreateOutcome::NotSent(_)));
    let verified = verified_request(&server, &host, &cwd);
    // The server records a connection once it closes, after the client has moved on.
    let settled = || {
        std::thread::sleep(Duration::from_millis(200));
        server.conns.lock().unwrap().len()
    };
    let calls = settled();
    for (cwd, command) in [
        (Path::new("tmp"), "x"),
        (Path::new("/no/such/dir"), "x"),
        (file.as_path(), "x"),
        (cwd.as_path(), "a\nb"),
        (cwd.as_path(), " "),
    ] {
        let mut bad = verified.clone();
        (bad.cwd, bad.command) = (cwd.to_path_buf(), command.to_string());
        assert!(
            matches!(host.create(&bad), HerdrCreateOutcome::NotSent(_)),
            "{cwd:?} {command:?}"
        );
    }
    assert_eq!(server.request("workspace.create"), None);
    assert_eq!(settled(), calls, "no connection at all");
}

fn process_info(shell: Value, foreground: Option<&[u32]>) -> Value {
    let mut info =
        json!({"pane_id": "w1:p1", "shell_pid": shell, "foreground_process_group_id": 1});
    if let Some(pids) = foreground {
        let processes: Vec<Value> = pids
            .iter()
            .map(|pid| json!({"pid": pid, "name": "p"}))
            .collect();
        info["foreground_processes"] = json!(processes);
    }
    json!({"type": "pane_process_info", "process_info": info})
}

// A provider is only a candidate, and every non-candidate reading keeps its reason.
#[test]
fn provider_candidate_names_one_foreground_process_or_why_not() {
    let location = |server: &Server| HostedLocation {
        host: "herdr".into(),
        execution_node: "mac-mini".into(),
        endpoint_config_key: "pilot".into(),
        socket_addr: server.path.display().to_string(),
        named_session: "adk".into(),
        pane_id: "w1:p1".into(),
    };
    let cases: Vec<(Vec<Value>, ProviderCandidate)> = vec![
        (
            vec![
                process_info(json!(10), Some(&[10])),
                process_info(json!(10), Some(&[10, 20])),
            ],
            ProviderCandidate::One {
                root: 10,
                provider: 20,
            },
        ),
        (
            vec![process_info(json!(10), Some(&[20, 30]))],
            ProviderCandidate::Multiple(vec![20, 30]),
        ),
        (
            vec![process_info(json!(10), None)],
            ProviderCandidate::Unreported,
        ),
        (
            vec![process_info(json!(null), Some(&[20]))],
            ProviderCandidate::NoRoot,
        ),
    ];
    for (replies, expected) in cases {
        let replies = Mutex::new(replies);
        let server = serve(Box::new(move |_, _| {
            let mut replies = replies.lock().unwrap();
            if replies.len() > 1 {
                replies.remove(0)
            } else {
                replies[0].clone()
            }
        }));
        let host = launch_host(&server);
        assert_eq!(host.provider_candidate(&location(&server)), expected);
        assert_eq!(
            host.launch_evidence(&location(&server)),
            None,
            "a candidate is not evidence"
        );
    }
    let server = serve(Box::new(|_, _| json!({"type": "ok"})));
    let host = launch_host(&server);
    assert!(matches!(
        host.provider_candidate(&location(&server)),
        ProviderCandidate::ReadFailed(_)
    ));
    let mut elsewhere = location(&server);
    elsewhere.endpoint_config_key = "other".into();
    assert_eq!(
        host.provider_candidate(&elsewhere),
        ProviderCandidate::UnknownEndpoint
    );
}
