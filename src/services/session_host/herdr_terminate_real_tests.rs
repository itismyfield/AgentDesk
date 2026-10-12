//! `agentdesk herdr terminate` against real Herdr 0.9.3 scratch servers, judged by
//! `scripts/test_p11_m2_activation.py` from printed lines; X1 reuses these servers, CLI and API.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use clap::Parser;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::herdr::contract::{HerdrOutcome, HerdrTransport, ServerHello, ServerWitness, Witnessed};
use super::herdr::model::{HerdrCall, HerdrEndpoint};
use super::herdr::observe::{CANONICAL_CONFIG, RestoreUnverified};
use super::herdr::pane_probe::start_text;
use super::herdr::provenance;
use super::herdr::transport::{HerdrSocketConfig, HerdrSocketTransport};
use super::herdr_registry::{self, HerdrRegistry};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedCasOutcome, HostedExecution, HostedLookup, HostedLookupKey, HostedOwner, HostedRecord,
    HostedState, bind_pg, load_hosted_execution_pg,
};
use crate::services::discord::herdr_terminate::route_tests::{capture, results_of};
use crate::services::discord::herdr_terminate::test_turn;
use crate::services::herdr_launch::{
    HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunch, HerdrLaunchCommand, HerdrLaunchEndpoint,
    HerdrLaunchHost, HerdrLaunchOutcome, launch_herdr_session, unset_herdr_env_before_exec,
};

const NODE: &str = "mac-mini";
const SESSION: &str = "adk";
const REASON: &str = "operator test";
/// Requests the API router took on the terminate route.
pub(crate) static ROUTE_REQUESTS: AtomicUsize = AtomicUsize::new(0);

/// The scratch copy of the server binary, the run's root and the pid ledger, from the runner.
pub(crate) struct Scratch {
    bin: PathBuf,
    root: PathBuf,
    ledger: PathBuf,
}

impl Scratch {
    pub(crate) fn from_env() -> Self {
        let var = |key: &str| {
            PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("{key} is not set")))
        };
        Self {
            bin: var("ADK_TEST_HERDR_BIN"),
            root: var("ADK_TEST_HERDR_ROOT"),
            ledger: var("ADK_TEST_HERDR_PIDS"),
        }
    }
}

fn now() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%z")
        .to_string()
}

fn start_of(pid: u32) -> Option<String> {
    provenance::process_start(pid)
        .ok()
        .map(|start| start_text(start.identity))
}

/// One server run from its own directory, with every process it hosts for this test recorded
/// in the ledger. Dropping it stops that server by its pid and waits for those processes.
pub(crate) struct Real {
    pub(crate) name: String,
    dir: PathBuf,
    ledger: PathBuf,
    child: Option<Child>,
    pub(crate) pid: u32,
    pub(crate) start: String,
    pub(crate) recorded: Vec<(u32, String)>,
}

impl Real {
    pub(crate) fn start(scratch: &Scratch, name: &str) -> Self {
        let dir = scratch.root.join(name);
        let _ = std::fs::remove_dir_all(&dir);
        for sub in [
            "home/xdg",
            "state",
            "data",
            "cache",
            "run",
            "tmp",
            "work",
            "worktrees",
            "pathbin",
        ] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        // No `curl` on the server's PATH: its update checks fail before any request leaves.
        for (tool, target) in [("git", "/usr/bin/git"), ("sh", "/bin/sh")] {
            std::os::unix::fs::symlink(target, dir.join("pathbin").join(tool)).unwrap();
        }
        let config = dir.join("home/config.toml");
        std::fs::write(&config, CANONICAL_CONFIG).unwrap();
        let file = std::fs::File::options().write(true).open(&config).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let socket = dir.join("run/h.sock");
        let out = std::fs::File::create(dir.join("server.out")).unwrap();
        let at = |sub: &str| dir.join(sub);
        let mut tmp = at("tmp").into_os_string();
        tmp.push("/");
        let child = Command::new(&scratch.bin)
            .arg("server")
            .env_clear()
            .env("PATH", at("pathbin"))
            .env("HOME", at("home"))
            .env("XDG_CONFIG_HOME", at("home/xdg"))
            .env("XDG_DATA_HOME", at("data"))
            .env("XDG_STATE_HOME", at("state"))
            .env("XDG_CACHE_HOME", at("cache"))
            .env("XDG_RUNTIME_DIR", at("run"))
            .env("TMPDIR", tmp)
            .env("HERDR_CONFIG_PATH", &config)
            .env("HERDR_SOCKET_PATH", &socket)
            .env("TERM", "xterm-256color")
            .env("LANG", "en_US.UTF-8")
            .env("SHELL", "/bin/sh")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut real = Self {
            name: name.into(),
            dir,
            ledger: scratch.ledger.clone(),
            child: Some(child),
            pid,
            start: String::new(),
            recorded: Vec::new(),
        };
        let ready = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < ready, "no server at {}", socket.display());
            std::thread::sleep(Duration::from_millis(100));
        }
        // The group is read again once the socket is up, when the server has set it.
        let pgid = ps(&["-o", "pgid=", "-p", &pid.to_string()]);
        assert_eq!(
            pgid.trim(),
            pid.to_string(),
            "the server leads its own group"
        );
        real.ledger(&format!(
            "{pid} x12-{name}-herdr {pgid} {} (socket up)",
            now(),
            pgid = pgid.trim()
        ));
        real.start = start_of(pid).expect("the server's start");
        println!(
            "X_SERVER name={name} pid={pid} pgid={} socket={} start={} log={}",
            pgid.trim(),
            socket.display(),
            real.start,
            real.log_path().display()
        );
        real
    }

    fn ledger(&self, line: &str) {
        let mut file = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&self.ledger)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }

    /// Records a process this server hosts, to be checked gone after it stops.
    fn record(&mut self, pid: u32, what: &str) {
        let start = start_of(pid).unwrap_or_else(|| panic!("{what} {pid} is not running"));
        let pgid = ps(&["-o", "pgid=", "-p", &pid.to_string()]);
        self.ledger(&format!(
            "{pid} x12-{}-{what} {} {} (hosted by {})",
            self.name,
            pgid.trim(),
            now(),
            self.pid
        ));
        self.recorded.push((pid, start));
    }

    pub(crate) fn socket(&self) -> PathBuf {
        self.dir.join("run/h.sock")
    }

    pub(crate) fn work(&self, name: &str) -> PathBuf {
        let path = self.dir.join("work").join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join("home/xdg/herdr/herdr-server.log")
    }

    pub(crate) fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    pub(crate) fn endpoint(&self) -> HerdrEndpoint {
        HerdrEndpoint::new(NODE, &self.name, &self.socket(), SESSION)
            .unwrap()
            .with_herdr_home(&self.dir.join("home"))
            .unwrap()
    }

    fn launch_endpoint(&self) -> HerdrLaunchEndpoint {
        HerdrLaunchEndpoint {
            execution_node: NODE.into(),
            config_key: self.name.clone(),
            socket_addr: self.socket().display().to_string(),
            herdr_session: SESSION.into(),
        }
    }

    /// One request on a fresh connection, as an operator's own client sends it.
    fn rpc(&self, id: &str, method: &str, params: Value) -> Value {
        let mut stream = UnixStream::connect(self.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let request = json!({"id": id, "method": method, "params": params});
        stream.write_all(format!("{request}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("{method}: {line}"))
    }

    pub(crate) fn snapshot(&self) -> Value {
        let reply = self.rpc("x12-snapshot", "session.snapshot", json!({}));
        reply["result"]["snapshot"].clone()
    }

    /// Each pane with its workspace, terminal, root shell and foreground processes.
    pub(crate) fn panes(&self) -> Vec<Value> {
        let snapshot = self.snapshot();
        let mut panes: Vec<Value> = snapshot["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pane| {
                let id = pane["pane_id"].as_str().unwrap();
                let info = self.rpc("x12-info", "pane.process_info", json!({"pane_id": id}));
                let info = &info["result"]["process_info"];
                let foreground: Vec<u64> = info["foreground_processes"]
                    .as_array()
                    .map(|all| all.iter().filter_map(|p| p["pid"].as_u64()).collect())
                    .unwrap_or_default();
                json!({
                    "pane_id": id,
                    "workspace_id": pane["workspace_id"],
                    "terminal_id": pane["terminal_id"],
                    "shell_pid": info["shell_pid"],
                    "foreground": foreground,
                })
            })
            .collect();
        panes.sort_by_key(|pane| pane["pane_id"].as_str().unwrap().to_string());
        panes
    }

    /// The pane's root shell and its one foreground child, once the child has started.
    fn shell_and_child(&self, pane: &str) -> (u32, u32) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let info = self.rpc("x12-info", "pane.process_info", json!({"pane_id": pane}));
            let info = &info["result"]["process_info"];
            let shell = info["shell_pid"].as_u64().unwrap() as u32;
            let others: Vec<u32> = info["foreground_processes"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|p| p["pid"].as_u64())
                .map(|pid| pid as u32)
                .filter(|pid| *pid != shell)
                .collect();
            if let [child] = others.as_slice() {
                return (shell, *child);
            }
            assert!(Instant::now() < deadline, "{pane}: {info}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Server processes running `curl` under this server, by parent or group.
    fn curl_children(&self) -> usize {
        ps(&["-ax", "-o", "pid=,ppid=,pgid=,command="])
            .lines()
            .filter(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                fields.len() > 3
                    && (fields[1] == self.pid.to_string() || fields[2] == self.pid.to_string())
                    && fields[3..].join(" ").contains("curl")
            })
            .count()
    }

    /// The log's API requests after `offset`, as `(request_id, line)`.
    pub(crate) fn requests_since(&self, offset: usize) -> Vec<(String, String)> {
        self.log()[offset..]
            .lines()
            .filter(|line| line.contains("event=\"api.request.start\""))
            .map(|line| (field(line, "request_id"), line.to_string()))
            .collect()
    }

    /// Asserts nothing reached the network and prints how the update checks ended.
    pub(crate) fn assert_offline(&self) {
        let curl = self.curl_children();
        let log = self.log();
        let failed = log.lines().any(|line| {
            line.contains("event=\"update.check.complete\"")
                && line.contains("outcome=\"error\"")
                && line.contains("curl failed: No such file")
        });
        println!(
            "X_CURL server={} children={curl} update_check={}",
            self.name,
            if failed { "spawn_failed" } else { "unknown" }
        );
        assert_eq!(curl, 0, "no curl under {}", self.name);
        assert!(
            failed,
            "{}: the update check could not spawn curl",
            self.name
        );
    }
}

impl Drop for Real {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &self.pid.to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let forced = child.try_wait().ok().flatten().is_none();
        if forced {
            let _ = child.kill();
        }
        let _ = child.wait();
        let deadline = Instant::now() + Duration::from_secs(10);
        let left = |recorded: &[(u32, String)]| -> Vec<u32> {
            recorded
                .iter()
                .filter(|(pid, start)| start_of(*pid).as_ref() == Some(start))
                .map(|(pid, _)| *pid)
                .collect()
        };
        while !left(&self.recorded).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let left = left(&self.recorded);
        self.ledger(&format!(
            "{} killed {} (x12-{}: kill -TERM {} only{}; hosted left {left:?})",
            self.pid,
            now(),
            self.name,
            self.pid,
            if forced { ", then SIGKILL" } else { "" }
        ));
        println!(
            "X_STOPPED name={} pid={} forced={forced} hosted_left={left:?}",
            self.name, self.pid
        );
    }
}

fn ps(args: &[&str]) -> String {
    let output = Command::new("/bin/ps").args(args).output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `name="value"` or `name=value` of one log line.
pub(crate) fn field(line: &str, name: &str) -> String {
    let Some((_, rest)) = line.split_once(&format!(" {name}=")) else {
        return String::new();
    };
    match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next().unwrap_or("").to_string(),
        None => rest.split_whitespace().next().unwrap_or("").to_string(),
    }
}

/// One call the registered transport made, and the server that answered it.
pub(crate) struct Seen {
    id: String,
    method: String,
    witness: Result<ServerWitness, String>,
    outcome: String,
}

/// The registered endpoint's socket transport, keeping the server of each call; a write is sent
/// only on a connection to `expected`, so a sent write was answered by that server.
pub(crate) struct Witnessing {
    inner: HerdrSocketTransport,
    seen: Arc<Mutex<Vec<Seen>>>,
    pub(crate) writes: Arc<Mutex<Vec<Seen>>>,
}

impl Witnessing {
    pub(crate) fn new(endpoint: &HerdrEndpoint) -> Self {
        Self {
            inner: HerdrSocketTransport::new(
                endpoint,
                HerdrSocketConfig::default(),
                super::herdr::wire::LineJsonFraming,
            ),
            seen: Arc::default(),
            writes: Arc::default(),
        }
    }

    fn seen(
        call: &HerdrCall,
        witness: Result<ServerWitness, String>,
        outcome: &HerdrOutcome,
    ) -> Seen {
        let value = serde_json::to_value(call).unwrap();
        Seen {
            id: call.id.clone(),
            method: value["method"].as_str().unwrap_or("?").to_string(),
            witness,
            outcome: match outcome {
                Ok(reply) => match &reply.body {
                    Ok(_) => "ok".into(),
                    Err(error) => format!("error:{}", error.code),
                },
                Err(error) => format!("transport:{error:?}"),
            },
        }
    }
}

impl HerdrTransport for Witnessing {
    fn call(&self, call: &HerdrCall) -> (HerdrOutcome, Witnessed) {
        let (outcome, witnessed) = self.inner.call(call);
        let witness = witnessed.clone().map_err(|why| format!("{why:?}"));
        self.seen
            .lock()
            .unwrap()
            .push(Self::seen(call, witness, &outcome));
        (outcome, witnessed)
    }

    fn call_with_witness(&self, call: &HerdrCall, expected: &ServerWitness) -> HerdrOutcome {
        let outcome = self.inner.call_with_witness(call, expected);
        let unsent = matches!(
            &outcome,
            Err(super::herdr::contract::HerdrTransportError::NotSent(_))
        );
        let witness = if unsent {
            Err("not sent".into())
        } else {
            Ok(expected.clone())
        };
        self.writes
            .lock()
            .unwrap()
            .push(Self::seen(call, witness, &outcome));
        outcome
    }

    fn hello(&self) -> Result<ServerHello, RestoreUnverified> {
        self.inner.hello()
    }

    fn server_witness(&self) -> Witnessed {
        self.inner.server_witness()
    }
}

/// Every write the run sent, printed with the server that took it and that server's binary.
fn print_writes(transport: &Witnessing) -> Vec<(String, String, ServerWitness)> {
    let writes = transport.writes.lock().unwrap();
    writes
        .iter()
        .map(|write| {
            let witness = write.witness.clone().expect("a sent write");
            let start = start_text(witness.start);
            println!(
                "X_WRITE id={} method={} outcome={} socket={} pid={} start={start}",
                write.id,
                write.method,
                write.outcome,
                witness.socket.display(),
                witness.pid
            );
            let exe = provenance::process_exec_path(witness.pid).unwrap_or_default();
            let mut bytes = Vec::new();
            std::fs::File::open(&exe)
                .and_then(|mut file| file.read_to_end(&mut bytes))
                .unwrap();
            let sha = format!("{:x}", Sha256::digest(&bytes));
            println!("X_EXE pid={} path={exe} sha256={sha}", witness.pid);
            (write.id.clone(), write.method.clone(), witness)
        })
        .collect()
}

/// Boots this node with `real` as its one endpoint, the witnessing registry and the terminate
/// switch on, then runs `on_thread` on every runtime thread after those overrides.
pub(crate) fn boot(
    real: &Real,
    channel: u64,
    transport: Arc<Witnessing>,
    on_thread: impl Fn() + Send + Sync + 'static,
) -> tokio::runtime::Runtime {
    let config: crate::config::Config = serde_json::from_value(json!({
        "server": {},
        "cluster": {"instance_id": NODE},
        "agents": [{"id": "p11-m2-real", "name": "P11 real", "channels": {
            "claude": {"id": channel.to_string(), "runtime": "tui"}
        }}],
        "session_hosts": {"herdr": {
            "endpoints": {real.name.clone(): {
                "execution_node": NODE,
                "socket_path": real.socket(),
                "herdr_home": real.dir.join("home"),
                "herdr_session": SESSION
            }},
            "channels": {channel.to_string(): real.name.clone()}
        }}
    }))
    .unwrap();
    crate::config::session_hosts::install(&config).unwrap();
    let mut live = crate::config_live_reload::current()
        .map(|config| (*config).clone())
        .unwrap_or_default();
    live.runtime.herdr_terminate_enabled = Some(true);
    crate::config_live_reload::install(live);
    let endpoint = real.endpoint();
    let install = move || {
        let transport: Arc<dyn HerdrTransport> = transport.clone();
        let registry = HerdrRegistry::with_transport(endpoint.clone(), transport);
        std::mem::forget(herdr_registry::force_for_test(registry));
        std::mem::forget(crate::services::herdr_launch::force_writer_accepts(Some(
            true,
        )));
        on_thread();
    };
    install();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
}

/// A launched, bound execution of `logical` on `real`: the real launch path creates its pane and
/// types the provider fixture, whose exec-time environment names the execution's own context.
pub(crate) struct Execution {
    pub(crate) key: String,
    pub(crate) nonce: String,
    pub(crate) pane: String,
    pub(crate) workspace: String,
    pub(crate) shell: (u32, String),
    pub(crate) provider: (u32, String),
}

/// The channel's Claude session row before its first Herdr launch: Legacy, with no execution.
pub(crate) async fn legacy_row(pool: &PgPool, key: &str, token_hash: &str, channel: u64) {
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3)")
        .bind(key).bind(token_hash).bind(channel.to_string())
        .execute(pool).await.unwrap();
}

pub(crate) async fn launch(
    real: &mut Real,
    pool: &PgPool,
    token_hash: &str,
    channel: u64,
    logical: &str,
    cwd: PathBuf,
) -> Execution {
    let key = format!("claude/{token_hash}/{NODE}:{logical}");
    legacy_row(pool, &key, token_hash, channel).await;
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: token_hash.into(),
        channel_id: channel.to_string(),
        logical_key: logical.into(),
        owner_node: NODE.into(),
        runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
    };
    let launch = || HerdrLaunch {
        endpoint: Some(real.launch_endpoint()),
        owner: owner.clone(),
        channel_id: Some(channel),
        expected_native_session_id: None,
        resume: false,
        provider_root: None,
    };
    let work = real.work("providers");
    // macOS keeps a platform binary's environment from other processes; python's is readable.
    let prepare = move |incarnation: &crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation| {
        let script = format!(
            "#!/bin/sh\n{}exec /usr/bin/python3 -c 'import time; time.sleep(3600)'\n",
            incarnation.env_lines()
        );
        let script = unset_herdr_env_before_exec(&script)?;
        let path = work.join(format!("{}.sh", incarnation.context.execution_nonce));
        std::fs::write(&path, script).map_err(|error| error.to_string())?;
        Ok(HerdrLaunchCommand {
            cwd,
            command: format!("/bin/sh {}", path.display()),
        })
    };
    let host: Arc<dyn HerdrLaunchHost> = Arc::new(super::SocketHerdrLaunchHost::new(
        vec![real.endpoint()],
        HerdrSocketConfig::default(),
    ));
    let mut launched = launch_herdr_session(pool, launch(), prepare, host.clone()).await;
    // Until the script's exec reaches python its environment is unreadable; the launch path
    // probes the recorded pane again on the next launch, as it does in service.
    for _ in 0..10 {
        let Ok(HerdrLaunchOutcome::Launched {
            evidence: Err(super::EvidenceGap::EnvUnreadable),
            ..
        }) = &launched
        else {
            break;
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        let again = |_: &_| Err("the pane is launched already".to_string());
        launched = launch_herdr_session(pool, launch(), again, host.clone()).await;
    }
    let Ok(HerdrLaunchOutcome::Launched {
        execution_nonce,
        location,
        evidence: Ok(()),
    }) = launched
    else {
        panic!("launch: {launched:?}");
    };
    let HostedLookup::Found(row) =
        load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(&key)).await
    else {
        panic!("no row");
    };
    assert_eq!(
        bind_pg(pool, &row, &owner, &execution_nonce).await,
        Ok(HostedCasOutcome::Written)
    );
    let record = stored(pool, &key).await;
    assert_eq!(record.state, HostedState::Bound);
    let expected = record.expected.clone().expect("launch evidence");
    let workspace = real.snapshot()["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["pane_id"] == location.pane_id.as_str())
        .map(|pane| pane["workspace_id"].as_str().unwrap().to_string())
        .expect("the launched pane");
    real.record(expected.root.pid, "pane-shell");
    real.record(expected.provider_process.pid, "provider");
    println!(
        "X_EXECUTION server={} session_key={key} nonce={execution_nonce} pane={} workspace={workspace} shell={} provider={}",
        real.name, location.pane_id, expected.root.pid, expected.provider_process.pid
    );
    Execution {
        key,
        nonce: execution_nonce,
        pane: location.pane_id,
        workspace,
        shell: (expected.root.pid, expected.root.start),
        provider: (
            expected.provider_process.pid,
            expected.provider_process.start,
        ),
    }
}

pub(crate) async fn stored(pool: &PgPool, key: &str) -> HostedExecution {
    match load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await {
        HostedLookup::Found(row) => match row.record {
            HostedRecord::Known(record) => record,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// A pane on `real` running `/bin/sleep`, created as the launch host creates one; its shell and
/// sleep are recorded.
pub(crate) fn control(real: &mut Real, label: &str) -> String {
    let host =
        super::SocketHerdrLaunchHost::new(vec![real.endpoint()], HerdrSocketConfig::default());
    let witness = host
        .restore_resume(&real.launch_endpoint())
        .admitted_witness()
        .expect("E7 reads off");
    let request = HerdrCreateRequest {
        endpoint: real.launch_endpoint(),
        label: label.into(),
        cwd: real.work(label),
        command: "/bin/sleep 3600".into(),
        restore_off_witness: witness,
    };
    let HerdrCreateOutcome::Created { pane_id } = host.create(&request) else {
        panic!("control {label} not created");
    };
    let (shell, sleep) = real.shell_and_child(&pane_id);
    real.record(shell, "pane-shell");
    real.record(sleep, "control-process");
    pane_id
}

pub(crate) fn alive(process: &(u32, String)) -> bool {
    start_of(process.0).as_ref() == Some(&process.1)
}

pub(crate) fn print_panes(real: &Real, phase: &str, panes: &[Value]) {
    println!(
        "X_SNAPSHOT server={} phase={phase} panes={}",
        real.name,
        Value::from(panes.to_vec())
    );
}

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    herdr: crate::cli::herdr::HerdrArgs,
}

/// `agentdesk herdr terminate` run on a blocking thread of this process against `api`, the only
/// difference from the CLI binary being that process boundary.
pub(crate) async fn cli_terminate(
    api: std::net::SocketAddr,
    key: &str,
    nonce: &str,
) -> Result<(), String> {
    let config = tempfile::tempdir().unwrap();
    let path = config.path().join("agentdesk.yaml");
    std::fs::write(&path, "server:\n  port: 1\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    let base = format!("http://{api}");
    let _api = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_API_URL",
        std::ffi::OsStr::new(&base),
    );
    let args = Cli::try_parse_from([
        "herdr",
        "terminate",
        "--session-key",
        key,
        "--execution-nonce",
        nonce,
        "--reason",
        REASON,
    ])
    .unwrap();
    let result = tokio::task::spawn_blocking(move || crate::cli::herdr::run(args.herdr))
        .await
        .unwrap();
    println!(
        "X_CLI ok={} detail={:?}",
        result.is_ok(),
        result.as_ref().err()
    );
    result
}

/// The API router with its auth middleware under `/api` on a loopback listener.
pub(crate) async fn serve_api(
    registry: Arc<crate::services::discord::health::HealthRegistry>,
    pool: PgPool,
) -> std::net::SocketAddr {
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
    let tx = crate::server::ws::new_broadcast();
    let buffer = crate::server::ws::spawn_batch_flusher(tx.clone());
    let api = crate::server::routes::api_router_with_pg(
        engine,
        crate::config::Config::default(),
        tx,
        buffer,
        Some(registry),
        Some(pool),
    );
    let count = axum::middleware::from_fn(
        |request: axum::extract::Request, next: axum::middleware::Next| async move {
            if request.uri().path() == "/api/herdr/terminate" {
                ROUTE_REQUESTS.fetch_add(1, Ordering::SeqCst);
            }
            next.run(request).await
        },
    );
    let app = axum::Router::new().nest("/api", api).layer(count);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    addr
}

/// The one write a run sent, which must be its close on `real`'s own process.
pub(crate) fn the_close(real: &Real, transport: &Witnessing) -> String {
    let writes = print_writes(transport);
    let [(id, method, witness)] = writes.as_slice() else {
        panic!("one write: {writes:?}");
    };
    assert!(
        method.ends_with(".close") && method.starts_with("pane"),
        "{method}"
    );
    assert_eq!(
        (&witness.socket, witness.pid, start_text(witness.start)),
        (&real.socket(), real.pid, real.start.clone()),
        "the close went to {}'s own process",
        real.name
    );
    assert_ne!(witness.pid, std::process::id());
    id.clone()
}

pub(crate) fn print_results(
    records: Vec<std::collections::BTreeMap<String, String>>,
    key: &str,
    nonce: &str,
) -> Vec<String> {
    for record in &records {
        println!(
            "X_RESULT_LOG result={} session_key={} execution_nonce={} reason={:?} detail={:?}",
            record["result"],
            record["session_key"],
            record["execution_nonce"],
            record["reason"],
            record.get("detail")
        );
    }
    results_of(records, key, nonce)
}

#[test]
#[ignore = "needs ADK_TEST_HERDR_BIN, ADK_TEST_HERDR_ROOT, ADK_TEST_HERDR_PIDS: scripts/test_p11_m2_activation.py"]
fn a_p11_m2_confirmation_required_has_no_escalation() {
    const CHANNEL: u64 = 5_340_232;
    let scratch = Scratch::from_env();
    let _root = crate::config::TestRuntimeRootGuard::new();
    println!("X_TEST name=x2 pid={}", std::process::id());
    let mut s2 = Real::start(&scratch, "x2-s2");
    let repo = s2.work("repo");
    let git = |args: &[&str]| {
        let status = Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", s2.dir.join("home"))
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "x2\n").unwrap();
    git(&["add", "README"]);
    git(&[
        "-c",
        "user.name=x2",
        "-c",
        "user.email=x2@example.invalid",
        "commit",
        "-q",
        "-m",
        "x2 fixture",
    ]);
    let transport = Arc::new(Witnessing::new(&s2.endpoint()));
    let runtime = boot(&s2, CHANNEL, transport.clone(), || {});
    runtime.block_on(async {
        let (results, _logs) = capture();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) =
            crate::services::discord::host_teardown_gate::test_support::runtime(&pool).await;
        let channel = poise::serenity_prelude::ChannelId::new(CHANNEL);
        let target = launch(
            &mut s2,
            &pool,
            &shared.token_hash,
            CHANNEL,
            "AgentDesk-claude-x2",
            repo.clone(),
        )
        .await;
        // The operator opens a linked worktree of the target's checkout: a worktree group.
        let path = s2.dir.join("worktrees/repo/x2-wt");
        let created = s2.rpc(
            "x2-worktree",
            "worktree.create",
            json!({"workspace_id": target.workspace, "branch": "x2-wt",
                "path": path, "label": "x2-child", "focus": false}),
        );
        assert_eq!(created["result"]["type"], "worktree_created", "{created}");
        let child = created["result"]["workspace"]["workspace_id"]
            .as_str()
            .unwrap()
            .to_string();
        let child_pane = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_default();
        let keep = control(&mut s2, "x2-keep");
        let workspaces = s2.snapshot()["workspaces"].as_array().unwrap().clone();
        let of = |id: &str| {
            workspaces
                .iter()
                .find(|workspace| workspace["workspace_id"] == id)
                .cloned()
                .unwrap_or(Value::Null)
        };
        let (parent, linked) = (of(&target.workspace), of(&child));
        println!("X_PRECONDITION parent={parent} linked={linked}");
        assert_eq!(parent["worktree"]["is_linked_worktree"], false);
        assert_eq!(linked["worktree"]["is_linked_worktree"], true);
        assert_eq!(
            parent["worktree"]["repo_key"],
            linked["worktree"]["repo_key"]
        );
        assert_eq!(
            (&parent["pane_count"], &parent["tab_count"]),
            (&json!(1), &json!(1))
        );
        if !child_pane.is_empty() {
            let shell = s2.rpc(
                "x2-info",
                "pane.process_info",
                json!({"pane_id": child_pane}),
            );
            let shell = shell["result"]["process_info"]["shell_pid"]
                .as_u64()
                .unwrap() as u32;
            s2.record(shell, "pane-shell");
        }
        crate::services::claude::herdr_turn::hold(&target.nonce).unwrap();
        let token = test_turn::start(&shared, channel, 52, "turn-x2").await;
        let before = s2.panes();
        print_panes(&s2, "before", &before);
        let from = s2.log().len();
        let hosted = s2.recorded.clone();
        transport.writes.lock().unwrap().clear();

        let api = serve_api(registry, pool.clone()).await;
        let refused = cli_terminate(api, &target.key, &target.nonce).await;
        let refused = refused.expect_err("the CLI exits non-zero");
        assert!(
            refused.contains("HTTP 409") && refused.contains("confirmation_required"),
            "{refused}"
        );
        let close = the_close(&s2, &transport);
        assert_eq!(ROUTE_REQUESTS.load(Ordering::SeqCst), 1, "no request again");
        let results = print_results(results.take(), &target.key, &target.nonce);
        assert_eq!(results, ["confirmation_required"]);

        // Nothing changed on the server: every pane, terminal and process is where it was.
        let after = s2.panes();
        print_panes(&s2, "after", &after);
        assert_eq!(after, before);
        assert!(after.iter().any(|pane| pane["pane_id"] == keep.as_str()));
        for process in &hosted {
            assert!(alive(process), "{process:?} untouched");
        }
        let requests = s2.requests_since(from);
        assert_eq!(
            requests.len(),
            1,
            "one close and nothing wider: {requests:?}"
        );
        assert_eq!(requests[0].0, close);
        let refused_by_server = s2.log()[from..].lines().any(|line| {
            line.contains("event=\"api.request.complete\"")
                && field(line, "request_id") == close
                && field(line, "outcome") == "error"
        });
        assert!(
            refused_by_server,
            "the server answered the close with an error"
        );
        assert!(
            !s2.log()[from..].contains("event=\"workspace.close\""),
            "no workspace closed"
        );
        assert!(alive(&(s2.pid, s2.start.clone())));
        s2.assert_offline();

        // The execution stays bound and held, and its turn is still the channel's.
        let record = stored(&pool, &target.key).await;
        assert_eq!(
            (record.state, record.execution_nonce.as_str()),
            (HostedState::Bound, target.nonce.as_str())
        );
        let holds = crate::services::claude::herdr_turn::input_holds().unwrap();
        assert!(holds.iter().any(|(nonce, _)| *nonce == target.nonce));
        let active = test_turn::active(&shared, channel).await;
        assert!(active.is_some_and(|now| Arc::ptr_eq(&now, &token)));
        assert!(!token.cancelled.load(Ordering::Relaxed));
        println!("X_EFFECT name=x2 row=bound hold=kept turn=active");
        pool.close().await;
        db.drop().await;
    });
}
