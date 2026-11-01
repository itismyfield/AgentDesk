//! Starts the user's absent default herdr for an input: judge, lock the socket dir, judge again,
//! then `herdr server` in its own session; it never stops or unlinks anything, nor grants authority.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::presence::{self, HostOs, Presence};
use super::{Environment, Refusal, record};
use crate::services::herdr_launch::HerdrLaunchEndpoint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ensured {
    Present,
    Started(u32),
    Refused(Refusal),
}

/// Where one start reads and writes; production derives it from this process.
#[derive(Debug, Clone)]
pub(crate) struct Context {
    pub env: Environment,
    pub local_node: Option<String>,
    /// The start records and the started server's output; `None` refuses every start.
    pub records: Option<PathBuf>,
    pub grace_rounds: usize,
    pub lock_wait: Duration,
    pub ready_wait: Duration,
}

impl Context {
    fn for_process() -> Self {
        Self {
            env: Environment::for_home(std::env::var_os("HOME").map(PathBuf::from)),
            local_node: crate::config::session_hosts::local_node(),
            records: crate::config::runtime_root().map(|root| root.join("runtime/herdr_autostart")),
            grace_rounds: presence::GRACE_ROUNDS,
            lock_wait: Duration::from_secs(15),
            ready_wait: Duration::from_secs(10),
        }
    }
}

/// Ensures `launch`'s server is up when it is the local default user server and absent; every
/// other case returns without IO beyond reading the socket.
pub(crate) async fn ensure(launch: &HerdrLaunchEndpoint) -> Ensured {
    #[cfg(test)]
    ENSURED.with(|count| count.set(count.get() + 1));
    let socket = PathBuf::from(&launch.socket_addr);
    let context = context_for(&socket);
    if context.local_node.as_deref() != Some(launch.execution_node.as_str()) {
        return finish(&socket, Ensured::Refused("remote"), json!({}));
    }
    ensure_at(&socket, &context).await
}

pub(crate) async fn ensure_at(socket: &Path, context: &Context) -> Ensured {
    let mut value = json!({});
    let home = match super::target(socket, &context.env) {
        Ok(home) => home,
        Err(reason) => return finish(socket, Ensured::Refused(reason), value),
    };
    let gate = socket_gate(socket);
    let _in_process = gate.lock().await;
    let rounds = context.grace_rounds;
    let (seen, excused) = judge(socket, move |socket| {
        presence::settle(socket, &HostOs, rounds)
    })
    .await;
    value["presence"] = presence::describe(&seen).into();
    match seen {
        Presence::Present => return finish(socket, Ensured::Present, value),
        Presence::Unknown(_) => return finish(socket, Ensured::Refused("presence_unknown"), value),
        Presence::Absent(_) => {}
    }
    let outcome = locked_start(socket, home, context, excused, &mut value).await;
    finish(socket, outcome, value)
}

async fn locked_start(
    socket: &Path,
    home: &Path,
    context: &Context,
    excused: Vec<u32>,
    value: &mut Value,
) -> Ensured {
    let Some(records) = context.records.clone() else {
        return Ensured::Refused("record_unavailable");
    };
    let dir = home.join(".config/herdr");
    let wait = context.lock_wait;
    let lock = match tokio::task::spawn_blocking(move || lock_dir(&dir, wait)).await {
        Ok(Ok(lock)) => lock,
        _ => return Ensured::Refused("lock_timeout"),
    };
    let (again, _) = judge(socket, move |socket| {
        (
            presence::classify_except(socket, &HostOs, &excused),
            Vec::new(),
        )
    })
    .await;
    value["presence"] = presence::describe(&again).into();
    match again {
        Presence::Present => return Ensured::Present,
        Presence::Unknown(_) => return Ensured::Refused("presence_unknown"),
        Presence::Absent(_) => {}
    }
    let Ok(mut record) = record::load(&records, socket) else {
        return Ensured::Refused("record_unavailable");
    };
    if record.exhausted(record::now()) {
        return Ensured::Refused("backoff");
    }
    let found = match super::prepare(socket, home, &context.env, value).await {
        Ok(found) => found,
        Err(reason) => return Ensured::Refused(reason),
    };
    record.begin(record::now());
    if record::save(&records, &record).is_err() {
        return Ensured::Refused("record_unavailable");
    }
    let outcome = match spawn_server(&found.path, home, &records) {
        Ok(child) => ready(socket, child, context.ready_wait, value).await,
        Err(_) => Ensured::Refused("spawn_failed"),
    };
    let (pid, word) = match &outcome {
        Ensured::Started(pid) => (Some(*pid), "started"),
        Ensured::Present => (
            value["spawned"].as_u64().map(|pid| pid as u32),
            "another_server_answered",
        ),
        Ensured::Refused(reason) => (value["spawned"].as_u64().map(|pid| pid as u32), *reason),
    };
    record.finish(pid, word);
    let _ = record::save(&records, &record);
    drop(lock);
    outcome
}

async fn judge(
    socket: &Path,
    read: impl FnOnce(&Path) -> (Presence, Vec<u32>) + Send + 'static,
) -> (Presence, Vec<u32>) {
    let owned = socket.to_path_buf();
    tokio::task::spawn_blocking(move || read(&owned))
        .await
        .unwrap_or((
            Presence::Unknown(presence::Unknown::ProcessTable("reader panicked".into())),
            Vec::new(),
        ))
}

fn finish(socket: &Path, outcome: Ensured, mut value: Value) -> Ensured {
    let reason = match &outcome {
        Ensured::Present => "not_needed_present",
        Ensured::Started(_) => "started",
        Ensured::Refused(reason) => *reason,
    };
    value["reason"] = reason.into();
    value["action"] = super::action(reason).map_or(Value::Null, Value::from);
    if let Ensured::Started(pid) = outcome {
        value["pid"] = pid.into();
    }
    super::remember(socket, &value);
    outcome
}

type Gate = Arc<tokio::sync::Mutex<()>>;
static GATES: Mutex<BTreeMap<PathBuf, Gate>> = Mutex::new(BTreeMap::new());

fn socket_gate(socket: &Path) -> Gate {
    let mut gates = GATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    gates.entry(socket.into()).or_default().clone()
}

/// An exclusive `flock` on the socket dir, shared by every AgentDesk on this host; the kernel
/// releases it with the descriptor, so no file is created or left behind.
fn lock_dir(dir: &Path, wait: Duration) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let dir = std::fs::File::open(dir)?;
    let deadline = Instant::now() + wait;
    loop {
        // SAFETY: `dir` is an open descriptor for the duration of the call.
        if unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(dir);
        }
        if Instant::now() > deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The started server, whose exit a reaper thread records; nothing ever kills it.
struct Child {
    pid: u32,
    exited: Arc<Mutex<Option<std::process::ExitStatus>>>,
}

fn spawn_server(binary: &Path, home: &Path, records: &Path) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(records.join("server.log"));
    let (out, err) = match log.and_then(|log| Ok((log.try_clone()?, log))) {
        Ok((out, err)) => (out.into(), err.into()),
        Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
    };
    let mut command = std::process::Command::new(binary);
    command
        .arg("server")
        .env_clear()
        .envs(server_env(home))
        .current_dir(home)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id();
    let exited = Arc::new(Mutex::new(None));
    let reaped = exited.clone();
    std::thread::Builder::new()
        .name(format!("herdr-autostart-reaper-{pid}"))
        .spawn(move || {
            if let Ok(status) = child.wait()
                && let Ok(mut slot) = reaped.lock()
            {
                *slot = Some(status);
            }
        })?;
    Ok(Child { pid, exited })
}

/// The environment the server and its panes inherit: identity, a fixed PATH and locale only.
pub(crate) fn server_env(home: &Path) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".to_owned(), home.display().to_string()),
        (
            "PATH".to_owned(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_owned(),
        ),
        ("TERM".to_owned(), "xterm-256color".to_owned()),
    ];
    if let Some((user, shell)) = passwd() {
        env.push(("USER".to_owned(), user.clone()));
        env.push(("LOGNAME".to_owned(), user));
        env.push(("SHELL".to_owned(), shell));
    }
    for key in ["TMPDIR", "LANG", "LC_ALL"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_owned(), value));
        }
    }
    if let Ok(agent) = std::env::var("SSH_AUTH_SOCK") {
        use std::os::unix::fs::FileTypeExt;
        if std::fs::metadata(&agent).is_ok_and(|meta| meta.file_type().is_socket()) {
            env.push(("SSH_AUTH_SOCK".to_owned(), agent));
        }
    }
    env
}

/// The effective user's login name and shell from the password database.
fn passwd() -> Option<(String, String)> {
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut buffer = vec![0 as libc::c_char; 4096];
    let mut found = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into `entry` and `buffer`, both sized as passed, and sets `found`.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() {
        return None;
    }
    // SAFETY: a found entry's strings point into `buffer`, which outlives these reads.
    let read = |field: *const libc::c_char| unsafe {
        (!field.is_null()).then(|| {
            std::ffi::CStr::from_ptr(field)
                .to_string_lossy()
                .into_owned()
        })
    };
    // SAFETY: getpwuid_r returned 0 with `found` set, so it filled `entry`.
    let entry = unsafe { entry.assume_init() };
    Some((read(entry.pw_name)?, read(entry.pw_shell)?))
}

/// Waits for the started server to answer: its own pid is a start, another accepting server
/// means another starter won, and an early exit or the deadline is a refusal.
async fn ready(socket: &Path, child: Child, wait: Duration, value: &mut Value) -> Ensured {
    value["spawned"] = child.pid.into();
    let deadline = Instant::now() + wait;
    loop {
        let exited = child.exited.lock().ok().and_then(|slot| *slot);
        let owned = socket.to_path_buf();
        let accepting = tokio::task::spawn_blocking(move || accepting_pid(&owned))
            .await
            .ok()
            .flatten();
        match accepting {
            Some(pid) if pid == child.pid => return Ensured::Started(pid),
            Some(pid) => {
                if exited.is_none() {
                    value["anomaly"] =
                        json!({"two_servers": {"accepting": pid, "orphan": child.pid}});
                }
                return Ensured::Present;
            }
            None if exited.is_some() => return Ensured::Refused("start_failed"),
            None if Instant::now() > deadline => return Ensured::Refused("not_ready"),
            None => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// The pid the server on `socket` reports in its hello, when one answers.
fn accepting_pid(socket: &Path) -> Option<u32> {
    use super::super::contract::HerdrTransport;
    use super::super::model::HerdrEndpoint;
    use super::super::transport::{HerdrSocketConfig, HerdrSocketTransport};
    use super::super::wire::LineJsonFraming;
    let endpoint = HerdrEndpoint::new("autostart", "autostart", socket, "autostart").ok()?;
    let transport =
        HerdrSocketTransport::new(&endpoint, HerdrSocketConfig::default(), LineJsonFraming);
    transport.server_witness().ok().map(|witness| witness.pid)
}

#[cfg(test)]
thread_local! {
    static ENSURED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times this thread entered `ensure`; entry points are tested by this count.
#[cfg(test)]
pub(crate) fn ensured_on_this_thread() -> usize {
    ENSURED.with(std::cell::Cell::get)
}

#[cfg(test)]
static TEST_CONTEXTS: Mutex<BTreeMap<PathBuf, Context>> = Mutex::new(BTreeMap::new());

fn context_for(socket: &Path) -> Context {
    scoped(socket).unwrap_or_else(Context::for_process)
}

#[cfg(test)]
fn scoped(socket: &Path) -> Option<Context> {
    TEST_CONTEXTS.lock().ok()?.get(socket).cloned()
}

#[cfg(not(test))]
fn scoped(_: &Path) -> Option<Context> {
    None
}

/// Makes `ensure` use `context` for `socket` until the guard drops; sockets keep tests apart.
#[cfg(test)]
pub(crate) fn scoped_context(socket: &Path, context: Context) -> impl Drop + use<> {
    struct Scoped(PathBuf);
    impl Drop for Scoped {
        fn drop(&mut self) {
            if let Ok(mut contexts) = TEST_CONTEXTS.lock() {
                contexts.remove(&self.0);
            }
        }
    }
    if let Ok(mut contexts) = TEST_CONTEXTS.lock() {
        contexts.insert(socket.into(), context);
    }
    Scoped(socket.into())
}
