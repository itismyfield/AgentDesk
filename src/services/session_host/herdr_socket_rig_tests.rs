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
/// The root shell a restarted server gives the pane in place of the launch's.
pub(crate) const NEW_SHELL: u32 = 11;

/// The process reads the gate sees; empty until a test names the running provider. A pid a test
/// set answers whether it exists as set, any other as the named OS does; every such read is counted.
#[derive(Default)]
struct LateOs {
    os: Mutex<Option<Arc<dyn ProcessOs>>>,
    exists: Mutex<HashMap<u32, bool>>,
    exists_reads: AtomicUsize,
    started_at: Mutex<Option<std::time::SystemTime>>,
}

impl LateOs {
    fn now(&self) -> Result<Arc<dyn ProcessOs>, RestoreUnverified> {
        let os = self.os.lock().unwrap().clone();
        os.ok_or(RestoreUnverified::ProcessUnreadable)
    }
    fn set(&self, os: FakeOs) {
        *self.started_at.lock().unwrap() = Some(std::time::SystemTime::now());
        *self.os.lock().unwrap() = Some(Arc::new(os));
    }
}

impl ProcessOs for LateOs {
    fn exists(&self, pid: u32) -> Result<bool, RestoreUnverified> {
        self.exists_reads.fetch_add(1, Ordering::SeqCst);
        match self.exists.lock().unwrap().get(&pid).copied() {
            Some(exists) => Ok(exists),
            None => self.now()?.exists(pid),
        }
    }
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
type Answers = Arc<Mutex<HashMap<String, VecDeque<Value>>>>;

/// What a pane write's params do to the pane before the server answers it.
type OnSend = Arc<Mutex<Option<Box<dyn FnMut(&Value) + Send>>>>;

/// Prefixes an answer that takes its method's place once a pane write arrives.
const AFTER_SEND: &str = "after-send:";

/// A launched pane's processes that also answer whether each exists: the provider as `alive`
/// says, every other one present.
struct Seen(FakeOs, bool);

impl ProcessOs for Seen {
    fn exists(&self, pid: u32) -> Result<bool, RestoreUnverified> {
        Ok(pid != PROVIDER || self.1)
    }
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.0.parent(pid)
    }
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        self.0.start(pid)
    }
    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        self.0.environ(pid)
    }
    fn exec_path(&self, pid: u32) -> Option<String> {
        self.0.exec_path(pid)
    }
}

pub(crate) struct HerdrRig {
    path: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    answers: Answers,
    /// Server pids the next dials reach, the last one repeating.
    servers: Arc<Mutex<VecDeque<u32>>>,
    unanswered_sends: Arc<AtomicBool>,
    /// Pane writes still answered before the rest go unanswered; `usize::MAX` answers them all.
    answered_sends: Arc<AtomicUsize>,
    on_send: OnSend,
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
    if let Some(results) = answers.lock().unwrap().get_mut(method) {
        let result = if results.len() > 1 {
            results.pop_front().unwrap()
        } else {
            results.front().unwrap().clone()
        };
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
        let on_send = OnSend::default();
        let pane = on_send.clone();
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
                    if let Some(hook) = pane.lock().unwrap().as_mut() {
                        hook(&request["params"]);
                    }
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
            on_send,
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
        self.registry_as_on_this_thread(NODE, KEY, &self.path, SESSION)
    }

    /// The registry of `node`'s endpoint `key` at `at`, as a channel's config names it, with every
    /// dial reaching the rig; installed on the calling thread until dropped.
    pub(crate) fn registry_as_on_this_thread(
        &self,
        node: &str,
        key: &str,
        at: &Path,
        session: &str,
    ) -> ForcedRegistry {
        herdr_registry::force_for_test(self.registry_builder(node, key, at, session)())
    }

    /// Installs the same fake endpoint on each async worker and blocking executor thread.
    pub(crate) fn thread_installer(
        &self,
        node: &str,
        key: &str,
        at: &Path,
        session: &str,
    ) -> impl Fn() + Send + Sync + 'static {
        let build = self.registry_builder(node, key, at, session);
        move || std::mem::forget(herdr_registry::force_for_test(build()))
    }

    fn registry_builder(
        &self,
        node: &str,
        key: &str,
        at: &Path,
        session: &str,
    ) -> impl Fn() -> HerdrRegistry + Send + Sync + 'static {
        let (node, key, at, session) = (
            node.to_owned(),
            key.to_owned(),
            at.to_owned(),
            session.to_owned(),
        );
        let (path, servers, os) = (self.path.clone(), self.servers.clone(), self.os.clone());
        move || {
            let endpoint = |socket: &Path| {
                HerdrEndpoint::new(&node, &key, socket, &session)
                    .and_then(|endpoint| endpoint.with_herdr_home(Path::new("/adk/herdr")))
                    .unwrap()
            };
            let config = HerdrSocketConfig {
                io_timeout: Duration::from_millis(500),
                read_deadline: Duration::from_millis(500),
                retry_backoff: Duration::from_millis(50),
                max_frame_bytes: MAX_FRAME_BYTES,
            };
            let servers = servers.clone();
            let socket = HerdrSocketTransport::new(&endpoint(&path), config, LineJsonFraming)
                .with_peer_reader(Box::new(move |_| {
                    let mut servers = servers.lock().unwrap();
                    let pid = match servers.len() {
                        1 => servers[0],
                        _ => servers.pop_front().unwrap(),
                    };
                    Ok((pid, start(1_000)))
                }));
            let registry = HerdrRegistry::with_transport(endpoint(&at), Arc::new(socket));
            let os: Arc<dyn ProcessOs> = os.clone();
            registry.with_reads((off_where_dialled, os))
        }
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
        self.os.set(os);
    }

    /// The pane runs the launch at `context`, and its provider process is seen to exist or not.
    pub(crate) fn provider_seen(&self, context: &Path, alive: bool) {
        let os = Seen(FakeOs::launched(env_naming(context)), alive);
        *self.os.os.lock().unwrap() = Some(Arc::new(os));
    }

    /// The pane's root shell was restarted after the launch recorded it; the provider is the same.
    pub(crate) fn restart_shell(&self, context: &Path) {
        self.os
            .set(FakeOs::launched(env_naming(context)).starting(SHELL, &[start(1_009)]));
    }

    /// A restarted server restored the pane as shell `NEW_SHELL` alone.
    pub(crate) fn restore_as_new_shell(&self, context: &Path) {
        let os = FakeOs::launched(env_naming(context));
        self.os.set(os.starting(NEW_SHELL, &[start(1_100)]));
        let info = json!({"pane_id": PANE, "shell_pid": NEW_SHELL,
            "foreground_process_group_id": NEW_SHELL,
            "foreground_processes": [{"pid": NEW_SHELL}]});
        let info = json!({"type": "pane_process_info", "process_info": info});
        self.answer("pane.process_info", info);
    }

    /// Every later read of whether `pid` exists answers `exists`.
    pub(crate) fn exists(&self, pid: u32, exists: bool) {
        self.os.exists.lock().unwrap().insert(pid, exists);
    }

    /// How many reads of whether a pid exists the gate has made.
    pub(crate) fn exists_reads(&self) -> usize {
        self.os.exists_reads.load(Ordering::SeqCst)
    }

    /// Replies with `result` to every later `method` request.
    pub(crate) fn answer(&self, method: &str, result: Value) {
        self.answer_sequence(method, [result]);
    }

    /// Replies in order, then repeats the last response for every later request.
    pub(crate) fn answer_sequence(&self, method: &str, results: impl IntoIterator<Item = Value>) {
        let results: VecDeque<_> = results.into_iter().collect();
        assert!(!results.is_empty(), "a scripted response must not be empty");
        self.answers.lock().unwrap().insert(method.into(), results);
    }

    /// Replies with `result` to every `method` request after the next pane write.
    pub(crate) fn answer_after_send(&self, method: &str, result: Value) {
        self.answer(&format!("{AFTER_SEND}{method}"), result);
    }

    /// Runs `hook` on the params of every later pane write before the server answers it.
    pub(crate) fn on_send(&self, hook: impl FnMut(&Value) + Send + 'static) {
        *self.on_send.lock().unwrap() = Some(Box::new(hook));
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

/// A separate process keeps Linux peer provenance tied to its exec-time environment.
pub(crate) struct NativeHerdrRig {
    root: tempfile::TempDir,
    child: std::process::Child,
}

impl NativeHerdrRig {
    pub(crate) fn start(test: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix("adk-hn-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("home");
        std::fs::create_dir_all(home.join("xdg")).unwrap();
        let config = home.join("config.toml");
        std::fs::write(&config, super::herdr::observe::CANONICAL_CONFIG).unwrap();
        std::fs::File::open(&config)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600)),
            )
            .unwrap();
        let output = std::fs::File::create(root.path().join("server.log")).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("ADK_TEST_NATIVE_HERDR_DIR", root.path())
            .env("HERDR_CONFIG_PATH", config)
            .env("XDG_CONFIG_HOME", home.join("xdg"))
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        let mut rig = Self { root, child };
        for _ in 0..1000 {
            if rig.socket().exists() {
                return rig;
            }
            assert!(
                rig.child.try_wait().unwrap().is_none(),
                "native server exited"
            );
            thread::sleep(Duration::from_millis(5));
        }
        panic!("native server did not bind");
    }

    pub(crate) fn socket(&self) -> PathBuf {
        self.root.path().join("herdr.sock")
    }
    pub(crate) fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }
    pub(crate) fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    pub(crate) fn prove_native_off(&self) {
        let endpoint = HerdrEndpoint::new("test-node", "native", &self.socket(), SESSION)
            .unwrap()
            .with_herdr_home(&self.home())
            .unwrap();
        let transport =
            HerdrSocketTransport::new(&endpoint, HerdrSocketConfig::default(), LineJsonFraming);
        let reading = super::herdr::observe::read_restore_resume(&transport, &endpoint);
        assert!(
            matches!(&reading, RestoreResume::Off { witness } if witness.pid == self.child.id()),
            "{reading:?}"
        );
    }
}

impl Drop for NativeHerdrRig {
    fn drop(&mut self) {
        eprintln!(
            "S1_NATIVE_METHODS {:?}",
            self.requests()
                .iter()
                .map(|r| r["method"].as_str().unwrap_or("?"))
                .collect::<Vec<_>>()
        );
        let _ = std::fs::write(self.root.path().join("stop"), b"stop");
        for _ in 0..1000 {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

/// Installs the ordinary boot-built registry, including its production launch host.
pub(crate) fn native_registry_on_this_thread(
    boot: &crate::config::session_hosts::BootSessionHosts,
) -> impl Drop + use<> {
    herdr_registry::force_for_test(HerdrRegistry::build(boot))
}

pub(crate) fn native_server_child() -> bool {
    let Some(root) = std::env::var_os("ADK_TEST_NATIVE_HERDR_DIR").map(PathBuf::from) else {
        return false;
    };
    let listener = UnixListener::bind(root.join("herdr.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("requests.jsonl"))
        .unwrap();
    while !root.join("stop").exists() {
        let Ok((stream, _)) = listener.accept() else {
            thread::sleep(Duration::from_millis(2));
            continue;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut line = String::new();
        let _ = BufReader::new(stream).read_line(&mut line);
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        writeln!(log, "{request}").unwrap();
        log.flush().unwrap();
        let pane = json!({"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","revision":0,"cwd":request["params"]["cwd"],"foreground_cwd":null});
        let result = match request["method"].as_str().unwrap() {
            "ping" => json!({"type":"pong","version":"0.9.3","protocol":22}),
            "workspace.create" => json!({"type":"workspace_created","root_pane":pane}),
            "pane.process_info" => {
                json!({"type":"pane_process_info","process_info":{"pane_id":"w1:p1","shell_pid":std::process::id(),"foreground_process_group_id":std::process::id(),"foreground_processes":[]}})
            }
            "session.snapshot" => {
                json!({"type":"session_snapshot","snapshot":{"version":"0.9.3","protocol":22,"panes":[]}})
            }
            _ => json!({"type":"ok"}),
        };
        let response = json!({"id":request["id"],"result":result});
        let _ = writeln!(writer, "{response}");
    }
    true
}

/// Shared scripted OS observations for the gate and the production socket launch adapter.
struct SharedRigOs(Arc<LateOs>);
impl ProcessOs for SharedRigOs {
    fn exists(&self, pid: u32) -> Result<bool, RestoreUnverified> {
        self.0.exists(pid)
    }
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.0.parent(pid)
    }
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        let mut start = self.0.start(pid)?;
        start.wall_clock = self
            .0
            .started_at
            .lock()
            .unwrap()
            .ok_or(RestoreUnverified::ProcessUnreadable)?;
        Ok(start)
    }
    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        self.0.environ(pid)
    }
    fn exec_path(&self, pid: u32) -> Option<String> {
        self.0.exec_path(pid)
    }
}
impl HerdrRig {
    /// As thread_installer, retaining the production launch/create/evidence adapter over this socket.
    pub(crate) fn thread_installer_with_launch(
        &self,
        node: &str,
        key: &str,
        session: &str,
    ) -> impl Fn() + Send + Sync + 'static {
        let build = self.registry_builder(node, key, &self.path, session);
        let (node, key, session, socket, os) = (
            node.to_owned(),
            key.to_owned(),
            session.to_owned(),
            self.path.clone(),
            self.os.clone(),
        );
        move || {
            let endpoint = HerdrEndpoint::new(&node, &key, &socket, &session)
                .unwrap()
                .with_herdr_home(Path::new("/adk/herdr"))
                .unwrap();
            let host = super::SocketHerdrLaunchHost::socket_rig_for_test(
                endpoint,
                Box::new(SharedRigOs(os.clone())),
            );
            std::mem::forget(herdr_registry::force_for_test(
                build().with_launch_host(host),
            ));
        }
    }
}

#[path = "herdr_t14_client_tests.rs"]
pub(crate) mod t14_client;
