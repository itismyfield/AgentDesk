//! One local Herdr endpoint in-process for executor tests: a socket that, like Herdr, answers the
//! one request a connection carries, registered on a thread with E7 off and a scripted process OS.
#![cfg(unix)]

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

use super::herdr::contract::HerdrTransport;
use super::herdr::model::HerdrEndpoint;
use super::herdr::observe::{RestoreResume, RestoreUnverified};
use super::herdr::pane_probe::ProcessOs;
use super::herdr::pane_probe::tests::{FakeOs, context, env_naming};
use super::herdr::provenance::{ProcessStart, StartIdentity};
use super::herdr::transport::{HerdrSocketConfig, HerdrSocketTransport};
use super::herdr::wire::{LineJsonFraming, MAX_FRAME_BYTES};
use super::herdr_registry::{self, ForcedRegistry, HerdrRegistry};
use crate::db::dispatched_sessions::hosted_execution::{ExpectedExecution, ProcessStamp};

pub(crate) const PANE: &str = "w1-1";
pub(crate) const NODE: &str = "mac-mini";
pub(crate) const KEY: &str = "mini";
pub(crate) const SESSION: &str = "agentdesk";
pub(crate) const SHELL: u32 = 10;
pub(crate) const PROVIDER: u32 = 20;

/// The process reads the gate sees; empty until a test names the running provider.
#[derive(Default)]
struct LateOs(Mutex<Option<Arc<dyn ProcessOs>>>);

impl LateOs {
    fn now(&self) -> Result<Arc<dyn ProcessOs>, RestoreUnverified> {
        let os = self.0.lock().unwrap().clone();
        os.ok_or(RestoreUnverified::ProcessUnreadable)
    }
}

impl ProcessOs for LateOs {
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.now()?.parent(pid)
    }
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        self.now()?.start(pid)
    }
    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        self.now()?.environ(pid)
    }
    fn exec_path(&self, _pid: u32) -> Option<String> {
        None
    }
}

/// Results a test put in place of the default reply, by method.
type Answers = Arc<Mutex<HashMap<String, Value>>>;

/// Prefixes an answer that takes its method's place once a pane write arrives.
const AFTER_SEND: &str = "after-send:";

pub(crate) struct HerdrRig {
    path: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    answers: Answers,
    /// Server pids the next dials reach, the last one repeating.
    servers: Arc<Mutex<VecDeque<u32>>>,
    unanswered_sends: Arc<AtomicBool>,
    /// Pane writes still answered before the rest go unanswered; `usize::MAX` answers them all.
    answered_sends: Arc<AtomicUsize>,
    os: Arc<LateOs>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn start(seconds: u64) -> ProcessStart {
    ProcessStart {
        identity: StartIdentity::Darwin { seconds, micros: 0 },
        wall_clock: UNIX_EPOCH + Duration::from_secs(seconds),
    }
}

fn reply(request: &Value, answers: &Answers) -> Value {
    let method = request["method"].as_str().unwrap_or("");
    if let Some(result) = answers.lock().unwrap().get(method) {
        return json!({"id": request["id"], "result": result});
    }
    let result = match method {
        "pane.process_info" => json!({"type": "pane_process_info", "process_info": {
            "pane_id": PANE, "shell_pid": SHELL, "foreground_process_group_id": PROVIDER,
            "foreground_processes": [{"pid": PROVIDER, "name": "claude"}]
        }}),
        _ => json!({"type": "ok"}),
    };
    json!({"id": request["id"], "result": result})
}

/// E7 reads `Off` for whatever server a fresh connection reaches.
fn off_where_dialled(transport: &dyn HerdrTransport, _: &HerdrEndpoint) -> RestoreResume {
    match transport.server_witness() {
        Ok(witness) => RestoreResume::Off { witness },
        Err(why) => RestoreResume::Unverified(why),
    }
}

impl HerdrRig {
    pub(crate) fn start() -> Self {
        static SOCKETS: AtomicU64 = AtomicU64::new(0);
        let n = SOCKETS.fetch_add(1, Ordering::SeqCst);
        let path = PathBuf::from("/tmp").join(format!("adk-ht-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (unanswered_sends, stop) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let answered_sends = Arc::new(AtomicUsize::new(usize::MAX));
        let (log, unanswered, stopped) = (requests.clone(), unanswered_sends.clone(), stop.clone());
        let answered = answered_sends.clone();
        let answers = Answers::default();
        let scripted = answers.clone();
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                log.lock().unwrap().push(request.clone());
                let send = request["method"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("pane.send"));
                if send {
                    let mut answers = scripted.lock().unwrap();
                    let later: Vec<String> = answers.keys().cloned().collect();
                    for key in later {
                        if let Some(method) = key.strip_prefix(AFTER_SEND) {
                            let result = answers.remove(&key).unwrap();
                            answers.insert(method.to_owned(), result);
                        }
                    }
                }
                let silent = send
                    && (unanswered.load(Ordering::SeqCst)
                        || answered.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                            (left != usize::MAX && left > 0).then(|| left - 1)
                        }) == Err(0));
                if !silent {
                    let _ =
                        writer.write_all(format!("{}\n", reply(&request, &scripted)).as_bytes());
                }
                let _ = writer.shutdown(Shutdown::Write);
                let _ = reader.read_to_end(&mut Vec::new());
            }
        });
        Self {
            path,
            requests,
            answers,
            servers: Arc::new(Mutex::new(VecDeque::from([7]))),
            unanswered_sends,
            answered_sends,
            os: Arc::default(),
            stop,
            thread: Some(thread),
        }
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.path
    }

    /// This node's registry over the rig, installed on the calling thread until dropped.
    pub(crate) fn registry_on_this_thread(&self) -> ForcedRegistry {
        let endpoint = HerdrEndpoint::new(NODE, KEY, &self.path, SESSION)
            .and_then(|endpoint| endpoint.with_herdr_home(Path::new("/adk/herdr")))
            .unwrap();
        let config = HerdrSocketConfig {
            io_timeout: Duration::from_millis(500),
            read_deadline: Duration::from_millis(500),
            retry_backoff: Duration::from_millis(50),
            max_frame_bytes: MAX_FRAME_BYTES,
        };
        let servers = self.servers.clone();
        let socket = HerdrSocketTransport::new(&endpoint, config, LineJsonFraming)
            .with_peer_reader(Box::new(move |_| {
                let mut servers = servers.lock().unwrap();
                let pid = match servers.len() {
                    1 => servers[0],
                    _ => servers.pop_front().unwrap(),
                };
                Ok((pid, start(1_000)))
            }));
        let registry = HerdrRegistry::with_transport(endpoint, Arc::new(socket));
        let os: Arc<dyn ProcessOs> = self.os.clone();
        herdr_registry::force_for_test(registry.with_reads((off_where_dialled, os)))
    }

    /// Writes launch context `nonce`'s file, as a launch does, and returns its path.
    pub(crate) fn context(&self, nonce: &str) -> PathBuf {
        context(nonce)
    }

    /// The pane runs the provider of the execution whose launch context is `context`;
    /// `replaced` restarts that provider after the launch recorded it.
    pub(crate) fn run_provider(&self, context: &Path, replaced: bool) {
        let os = FakeOs::launched(env_naming(context));
        let os = if replaced {
            os.starting(PROVIDER, &[start(1_005)])
        } else {
            os
        };
        *self.os.0.lock().unwrap() = Some(Arc::new(os));
    }

    /// The pane's root shell was restarted after the launch recorded it; the provider is the same.
    pub(crate) fn restart_shell(&self, context: &Path) {
        let os = FakeOs::launched(env_naming(context)).starting(SHELL, &[start(1_009)]);
        *self.os.0.lock().unwrap() = Some(Arc::new(os));
    }

    /// Replies with `result` to every later `method` request.
    pub(crate) fn answer(&self, method: &str, result: Value) {
        self.answers.lock().unwrap().insert(method.into(), result);
    }

    /// Replies with `result` to every `method` request after the next pane write.
    pub(crate) fn answer_after_send(&self, method: &str, result: Value) {
        self.answer(&format!("{AFTER_SEND}{method}"), result);
    }

    /// Every later snapshot is complete and lists exactly `panes`.
    pub(crate) fn show_panes(&self, panes: &[&str]) {
        let panes: Vec<Value> = panes
            .iter()
            .map(|pane| {
                json!({"pane_id": pane, "terminal_id": "t1", "workspace_id": "w1",
                "tab_id": "w1:1", "focused": false, "agent_status": "idle", "revision": 7})
            })
            .collect();
        let snapshot = json!({"version": "0.9.3", "protocol": 22, "workspaces": [], "tabs": [],
            "layouts": [], "agents": [], "panes": panes});
        self.answer(
            "session.snapshot",
            json!({"type": "session_snapshot", "snapshot": snapshot}),
        );
    }

    /// Every later process read of the pane names `foreground` under its root shell.
    pub(crate) fn foreground(&self, foreground: &[u32]) {
        let listed: Vec<Value> = foreground.iter().map(|pid| json!({"pid": pid})).collect();
        let info = json!({"pane_id": PANE, "shell_pid": SHELL,
            "foreground_process_group_id": foreground.first(), "foreground_processes": listed});
        self.answer(
            "pane.process_info",
            json!({"type": "pane_process_info", "process_info": info}),
        );
    }

    /// The server pid each later dial reaches, in order; the last one repeats.
    pub(crate) fn serve_as(&self, pids: &[u32]) {
        *self.servers.lock().unwrap() = pids.iter().copied().collect();
    }

    /// The launch evidence of the pane's root shell and provider, for `nonce`.
    pub(crate) fn expected(&self, nonce: &str) -> ExpectedExecution {
        let stamp = |pid, seconds| ProcessStamp {
            pid,
            start: format!("darwin:{seconds}.000000"),
        };
        ExpectedExecution {
            binding_provider: "claude".into(),
            binding_nonce: nonce.into(),
            root: stamp(SHELL, 1_001),
            provider_process: stamp(PROVIDER, 1_002),
            provenance: "herdr_launch:ppid+env;exec=?".into(),
        }
    }

    /// Pane writes leave without a reply while `unanswered`: the client cannot tell whether
    /// they landed.
    pub(crate) fn leave_sends_unanswered(&self, unanswered: bool) {
        self.unanswered_sends.store(unanswered, Ordering::SeqCst);
        self.answered_sends.store(usize::MAX, Ordering::SeqCst);
    }

    /// The next `answered` pane writes are answered and every later one is left unanswered.
    pub(crate) fn answer_sends_then_leave_unanswered(&self, answered: usize) {
        self.answered_sends.store(answered, Ordering::SeqCst);
    }

    pub(crate) fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    /// The params of every pane write that reached the server, in order.
    pub(crate) fn sends(&self) -> Vec<Value> {
        let sends = self.requests().into_iter().filter(|r| {
            r["method"]
                .as_str()
                .is_some_and(|m| m.starts_with("pane.send"))
        });
        sends.map(|r| r["params"].clone()).collect()
    }
}

impl Drop for HerdrRig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
