//! SocketHerdrLaunchHost against a scripted socket server.
use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

use serde_json::{Value, json};

use super::*;
use crate::services::session_host::herdr::observe::{
    CANONICAL_CONFIG, ConfigRead, ServerProvenance, read_restore_resume_with,
};

const HOME: &str = "/srv/herdr-home";

/// Every process is the bootstrapped server; the scripted socket carries the rest.
struct Bootstrapped;

impl ServerProvenance for Bootstrapped {
    fn process_start(&self, _pid: u32) -> Result<SystemTime, RestoreUnverified> {
        Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000))
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
    /// E7 readings on this thread before the reader reports the config changed.
    static OFF_READINGS: Cell<usize> = const { Cell::new(usize::MAX) };
}

fn scripted_e7(transport: &HerdrSocketTransport, endpoint: &HerdrEndpoint) -> RestoreResume {
    let left = OFF_READINGS.with(|left| left.replace(left.get().saturating_sub(1)));
    if left == 0 {
        return RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart);
    }
    read_restore_resume_with(transport, endpoint, &Bootstrapped)
}

type Reply = Box<dyn Fn(&Value) -> Value + Send + Sync>;

struct Server {
    path: PathBuf,
    requests: Arc<Mutex<Vec<(usize, Value)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Answers each request line with `reply`'s result, on every connection in turn.
fn serve(reply: Reply) -> Server {
    let name = format!("adk-lh-{}-{}.sock", std::process::id(), rand_suffix());
    let path = std::env::temp_dir().join(&name);
    let path = if path.as_os_str().len() > 90 {
        Path::new("/tmp").join(name)
    } else {
        path
    };
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (seen, stopped) = (requests.clone(), stop.clone());
    let thread = std::thread::spawn(move || {
        let mut index = 0;
        while !stopped.load(Ordering::SeqCst) {
            let Ok((stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            answer(stream, index, &reply, &seen, &stopped);
            index += 1;
        }
    });
    Server {
        path,
        requests,
        stop,
        thread: Some(thread),
    }
}

fn rand_suffix() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn answer(
    stream: UnixStream,
    index: usize,
    reply: &Reply,
    seen: &Mutex<Vec<(usize, Value)>>,
    stop: &AtomicBool,
) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while !stop.load(Ordering::SeqCst) {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => continue,
        }
        let request: Value = serde_json::from_str(&line).unwrap();
        seen.lock().unwrap().push((index, request.clone()));
        let result = match request["method"].as_str() {
            Some("ping") => json!({"type": "pong", "version": "0.9.3", "protocol": 22}),
            _ => reply(&request),
        };
        let body = json!({"id": request["id"], "result": result});
        let _ = writer.write_all(format!("{body}\n").as_bytes());
    }
}

impl Server {
    fn requests(&self) -> Vec<(usize, String)> {
        let requests = self.requests.lock().unwrap();
        let method = |request: &Value| request["method"].as_str().unwrap_or("?").to_string();
        requests
            .iter()
            .map(|(conn, request)| (*conn, method(request)))
            .collect()
    }

    fn request(&self, method: &str) -> Option<Value> {
        let requests = self.requests.lock().unwrap();
        let found = requests
            .iter()
            .find(|(_, request)| request["method"] == method);
        found.map(|(_, request)| request["params"].clone())
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

fn launch_host(server: &Server) -> SocketHerdrLaunchHost {
    let endpoint = HerdrEndpoint::new("mac-mini", "pilot", &server.path, "adk")
        .unwrap()
        .with_herdr_home(Path::new(HOME))
        .unwrap();
    let config = HerdrSocketConfig {
        io_timeout: Duration::from_millis(500),
        ..HerdrSocketConfig::default()
    };
    SocketHerdrLaunchHost::new(vec![endpoint], config).with_restore_reader(scripted_e7)
}

fn workdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(dir.path()).unwrap();
    (dir, real)
}

fn created(cwd: String) -> Reply {
    Box::new(move |request| match request["method"].as_str() {
        Some("workspace.create") => json!({"type": "workspace_created", "root_pane": {
            "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1", "revision": 0, "cwd": cwd
        }}),
        _ => json!({"type": "ok"}),
    })
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
        restore_off_generation: reading.admitted_generation().expect("E7 off"),
    }
}

// The command is typed only on the connection that created the pane and passed E7.
#[test]
fn socket_launch_types_the_command_on_the_connection_that_created_the_pane() {
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
    let sent = [(0, "ping"), (0, "workspace.create"), (0, "pane.send_input")];
    let sent = sent.map(|(conn, method)| (conn, method.to_string()));
    assert_eq!(server.requests(), sent);
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

// A pane opened anywhere but the asked cwd, or after E7 is lost, never gets the command;
// a reading from before a reconnect, an unknown endpoint or a bad request creates nothing.
#[test]
fn socket_launch_withholds_the_command_unless_cwd_and_e7_hold_on_one_connection() {
    let (_dir, cwd) = workdir();
    let server = serve(created("/Users/elsewhere".into()));
    let host = launch_host(&server);
    let outcome = host.create(&verified_request(&server, &host, &cwd));
    assert!(
        matches!(
            outcome,
            HerdrCreateOutcome::CreatedUnconfirmed { ref pane_id, .. } if pane_id == "w1:p1"
        ),
        "{outcome:?}"
    );
    assert_eq!(server.request("pane.send_input"), None);

    let server = serve(created(cwd.display().to_string()));
    let host = launch_host(&server);
    let request = verified_request(&server, &host, &cwd);
    OFF_READINGS.with(|left| left.set(1));
    let outcome = host.create(&request);
    OFF_READINGS.with(|left| left.set(usize::MAX));
    assert!(
        matches!(outcome, HerdrCreateOutcome::CreatedUnconfirmed { .. }),
        "{outcome:?}"
    );
    assert_eq!(server.request("pane.send_input"), None);

    let server = serve(created(cwd.display().to_string()));
    let host = launch_host(&server);
    let request = verified_request(&server, &host, &cwd);
    host.endpoints[0].1.connect().expect("reconnect");
    assert!(matches!(
        host.create(&request),
        HerdrCreateOutcome::NotSent(_)
    ));
    assert_eq!(
        server.request("workspace.create"),
        None,
        "verified then reconnected"
    );

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
    for (cwd, command) in [
        (Path::new("tmp"), "x"),
        (Path::new("/no/such/dir"), "x"),
        (cwd.as_path(), "a\nb"),
        (cwd.as_path(), " "),
    ] {
        let mut bad = verified_request(&server, &host, &cwd);
        (bad.cwd, bad.command) = (cwd.to_path_buf(), command.to_string());
        assert!(
            matches!(host.create(&bad), HerdrCreateOutcome::NotSent(_)),
            "{command:?}"
        );
    }
    assert_eq!(server.request("workspace.create"), None);
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
        let server = serve(Box::new(move |_| {
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
    let server = serve(Box::new(|_| json!({"type": "ok"})));
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
