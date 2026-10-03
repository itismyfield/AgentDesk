//! Against a real Herdr 0.9.3 run from its own home under `/private/tmp/herdr-093-p9a`:
//! E7, create, input bytes, the provider env, and refusals that leave no effect.
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::*;
use crate::services::herdr_launch::unset_herdr_env_before_exec;
use crate::services::session_host::herdr::model::{HerdrResult, HerdrSnapshot};
use crate::services::session_host::herdr::observe::CANONICAL_CONFIG;
use crate::services::session_host::herdr::wire::LineJsonFraming;
use crate::services::session_host::herdr_host::HerdrHost;
use crate::services::session_host::model::{HostMutation, HostRefusal, HostSessionRef};
use crate::services::session_host::traits::InteractiveSessionHost;

const BASE: &str = "/private/tmp/herdr-093-p9a";

/// Logs raw tty bytes to `<out>` until the bytes so far end in QUITPROBE, then writes
/// `<out>.done`; `<out>.ready` marks that raw mode is on.
const KEYLOG: &str = r#"import os, sys, tty, termios
out = sys.argv[1]
fd = sys.stdin.fileno(); old = termios.tcgetattr(fd); tty.setraw(fd)
sys.stdout.write('\x1b[?2004h'); sys.stdout.flush()
open(out + '.ready', 'w').close()
seen = b''
try:
    with open(out, 'ab') as f:
        while not seen.endswith(b'QUITPROBE'):
            b = os.read(fd, 4096)
            if not b: break
            seen += b
            f.write(b); f.flush()
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
    open(out + '.done', 'w').close()
"#;

/// One isolated server directory; the server in it can be replaced by a new process on
/// the same socket. Dropping it stops that server through its own socket and reaps it.
struct RealServer {
    bin: PathBuf,
    dir: PathBuf,
    child: Arc<Mutex<Option<Child>>>,
}

fn server_command(bin: &Path, dir: &Path) -> Command {
    let mut command = Command::new(bin);
    let at = |sub: &str| dir.join(sub);
    command.env_clear().envs([
        ("PATH", PathBuf::from("/usr/bin:/bin")),
        ("HOME", at("home")),
        ("XDG_CONFIG_HOME", at("home/xdg")),
        ("XDG_STATE_HOME", at("state")),
        ("XDG_DATA_HOME", at("data")),
        ("XDG_CACHE_HOME", at("cache")),
        ("XDG_RUNTIME_DIR", at("run")),
        ("HERDR_CONFIG_PATH", at("home/config.toml")),
        ("HERDR_SOCKET_PATH", at("h.sock")),
        ("TERM", PathBuf::from("xterm-256color")),
        ("LANG", PathBuf::from("en_US.UTF-8")),
        ("SHELL", PathBuf::from("/bin/sh")),
    ]);
    command
}

/// Starts the server in `dir` and waits until it listens and is older than its config.
fn spawn_server(bin: &Path, dir: &Path) -> Child {
    let log = std::fs::File::options()
        .create(true)
        .append(true)
        .open(dir.join("server.out"))
        .unwrap();
    let child = server_command(bin, dir)
        .arg("server")
        .stdout(log)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = dir.join("h.sock");
    let ready = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < ready, "no server at {}", socket.display());
        std::thread::sleep(Duration::from_millis(100));
    }
    // The server's start must postdate the aged config by more than E7's margin.
    std::thread::sleep(Duration::from_millis(1_200));
    child
}

/// Stops the server in `dir` through its socket and reaps `child`, killing it if needed.
fn stop_server(bin: &Path, dir: &Path, mut child: Child) {
    let _ = server_command(bin, dir).args(["server", "stop"]).output();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let status = child.wait();
    eprintln!(
        "herdr server {} pid {} stopped: {status:?}",
        dir.display(),
        child.id()
    );
}

impl RealServer {
    fn start(name: &str, config: &[u8]) -> Self {
        let bin = PathBuf::from(std::env::var("ADK_TEST_HERDR_BIN").expect("ADK_TEST_HERDR_BIN"));
        let dir = Path::new(BASE).join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["home/xdg", "state", "data", "cache", "run", "work"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let config_path = dir.join("home/config.toml");
        std::fs::write(&config_path, config).unwrap();
        age(&config_path);
        let child = spawn_server(&bin, &dir);
        Self {
            bin,
            dir,
            child: Arc::new(Mutex::new(Some(child))),
        }
    }

    /// Stops this directory's server and starts a new process on the same socket.
    fn replace_handle(&self) -> impl FnOnce() + 'static {
        let (bin, dir, child) = (self.bin.clone(), self.dir.clone(), self.child.clone());
        move || {
            let old = child.lock().unwrap().take().expect("a running server");
            stop_server(&bin, &dir, old);
            *child.lock().unwrap() = Some(spawn_server(&bin, &dir));
        }
    }

    fn pid(&self) -> u32 {
        self.child.lock().unwrap().as_ref().unwrap().id()
    }

    fn endpoint(&self) -> HerdrEndpoint {
        HerdrEndpoint::new("mac-mini", "real", &self.dir.join("h.sock"), "adk")
            .unwrap()
            .with_herdr_home(&self.dir.join("home"))
            .unwrap()
    }

    fn launch_endpoint(&self) -> HerdrLaunchEndpoint {
        HerdrLaunchEndpoint {
            execution_node: "mac-mini".into(),
            config_key: "real".into(),
            socket_addr: self.dir.join("h.sock").display().to_string(),
            herdr_session: "adk".into(),
        }
    }

    fn panes(&self, transport: &HerdrSocketTransport) -> Vec<String> {
        let call = HerdrCall {
            id: "count".into(),
            request: HerdrRequest::SessionSnapshot {},
        };
        match transport.call(&call).0.map(|reply| reply.body) {
            Ok(Ok(HerdrResult::SessionSnapshot {
                snapshot: HerdrSnapshot { panes, .. },
            })) => panes.into_iter().map(|pane| pane.pane_id).collect(),
            other => panic!("snapshot {other:?}"),
        }
    }
}

impl Drop for RealServer {
    fn drop(&mut self) {
        if let Some(child) = self.child.lock().unwrap().take() {
            stop_server(&self.bin, &self.dir, child);
        }
    }
}

fn age(path: &Path) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(60))
        .unwrap();
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{what}: {} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

thread_local! {
    /// Run in order, one before each E7 reading on this thread; `None` runs nothing.
    static BEFORE_E7: RefCell<Vec<Option<Box<dyn FnOnce()>>>> = const { RefCell::new(Vec::new()) };
}

fn hooked_e7(transport: &HerdrSocketTransport, endpoint: &HerdrEndpoint) -> RestoreResume {
    let hook = BEFORE_E7.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        if hooks.is_empty() {
            None
        } else {
            hooks.remove(0)
        }
    });
    if let Some(hook) = hook {
        hook();
    }
    observe::read_restore_resume(transport, endpoint)
}

/// The next create's second E7 reading, the one after the pane exists, runs `between` first.
fn between_create_and_input(between: impl FnOnce() + 'static) {
    BEFORE_E7.with(|hooks| *hooks.borrow_mut() = vec![None, Some(Box::new(between))]);
}

fn create_request(
    server: &RealServer,
    witness: ServerWitness,
    command: String,
) -> HerdrCreateRequest {
    HerdrCreateRequest {
        endpoint: server.launch_endpoint(),
        label: "adk-real".into(),
        cwd: server.dir.join("work"),
        command,
        restore_off_witness: witness,
    }
}

fn off(host: &SocketHerdrLaunchHost, server: &RealServer) -> ServerWitness {
    let reading = host.restore_resume(&server.launch_endpoint());
    reading
        .clone()
        .admitted_witness()
        .unwrap_or_else(|| panic!("E7 {reading:?}"))
}

fn touch(marker: &Path) -> String {
    format!("touch {}", marker.display())
}

fn created(outcome: HerdrCreateOutcome) -> String {
    match outcome {
        HerdrCreateOutcome::Created { pane_id } => pane_id,
        other => panic!("{other:?}"),
    }
}

fn unconfirmed(outcome: &HerdrCreateOutcome) -> String {
    match outcome {
        HerdrCreateOutcome::CreatedUnconfirmed { pane_id, .. } => pane_id.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
#[ignore = "needs ADK_TEST_HERDR_BIN: an isolated Herdr 0.9.3 binary"]
fn herdr_real_server_launch_input_bytes_and_refusals() {
    let server = RealServer::start("r2", CANONICAL_CONFIG);
    let dir = server.dir.clone();
    let mut host =
        SocketHerdrLaunchHost::new(vec![server.endpoint()], HerdrSocketConfig::default());
    host.read_restore = hooked_e7;
    let transport = &host.endpoints[0].1;

    // E7 names the server process, and each new connection to it names the same one.
    let first = off(&host, &server);
    assert_eq!(first.pid, server.pid());
    assert_eq!(transport.server_witness(), Ok(first.clone()));
    assert_eq!(transport.server_witness(), Ok(first.clone()));

    // The provider sees no Herdr pane variable, and keeps AgentDesk and auth variables.
    let script = "#!/bin/sh\nexport CLAUDE_CONFIG_DIR=/cfg\nexport ANTHROPIC_API_KEY=k\n\
                  export AGENTDESK_BINDING_CONTEXT=/b\nexec /usr/bin/env > ENV\n";
    let script = unset_herdr_env_before_exec(
        &script.replace("ENV", &dir.join("env.txt").display().to_string()),
    );
    std::fs::write(dir.join("launch.sh"), script.unwrap()).unwrap();
    let command = format!("sh {}", dir.join("launch.sh").display());
    created(host.create(&create_request(&server, off(&host, &server), command)));
    wait_for(&dir.join("env.txt"), "provider env");
    std::thread::sleep(Duration::from_millis(200));
    let env = std::fs::read_to_string(dir.join("env.txt")).unwrap();
    let herdr: Vec<&str> = env
        .lines()
        .filter(|line| line.starts_with("HERDR_"))
        .collect();
    assert!(
        herdr.is_empty(),
        "Herdr pane env reached the provider: {herdr:?}"
    );
    for kept in [
        "CLAUDE_CONFIG_DIR=/cfg",
        "ANTHROPIC_API_KEY=k",
        "AGENTDESK_BINDING_CONTEXT=/b",
        "TERM_PROGRAM=herdr",
    ] {
        assert!(
            env.lines().any(|line| line == kept),
            "{kept} missing: {env}"
        );
    }

    // Keys, interrupt, paste, a line and text arrive as exactly these bytes, in order.
    std::fs::write(dir.join("keylog.py"), KEYLOG).unwrap();
    let log = dir.join("kl");
    let command = format!(
        "python3 {} {}",
        dir.join("keylog.py").display(),
        log.display()
    );
    let pane = created(host.create(&create_request(&server, off(&host, &server), command)));
    wait_for(&dir.join("kl.ready"), "keylog raw mode");
    let input = HerdrHost::new(
        server.endpoint(),
        HerdrSocketTransport::new(
            &server.endpoint(),
            HerdrSocketConfig::default(),
            LineJsonFraming,
        ),
    );
    let target = HostSessionRef::herdr_pane(&pane);
    let keys = ["Enter", "Escape", "C-u", "C-e", "Left", "Right", "BSpace"];
    assert_eq!(input.send_keys(target, &keys), Ok(HostMutation::Confirmed));
    assert_eq!(input.interrupt(target), Ok(HostMutation::Confirmed));
    assert_eq!(
        input.send_paste(target, "첫줄\n둘째줄 ✓"),
        Ok(HostMutation::Confirmed)
    );
    assert_eq!(
        input.send_line(target, "/clear"),
        Ok(HostMutation::Confirmed)
    );
    assert_eq!(
        input.send_text(target, "QUITPROBE"),
        Ok(HostMutation::Confirmed)
    );
    wait_for(&dir.join("kl.done"), "keylog QUITPROBE");
    let mut expected = b"\r\x1b\x15\x05\x1b[D\x1b[C\x7f\x03".to_vec();
    expected.extend_from_slice("\x1b[200~첫줄\n둘째줄 ✓\x1b[201~".as_bytes());
    // `pane.send_input` text arrives as a paste because the program turned bracketed paste on.
    expected.extend_from_slice(b"\x1b[200~/clear\x1b[201~\rQUITPROBE");
    assert_eq!(std::fs::read(&log).unwrap(), expected, "received bytes");

    // A config changed between create and input: the pane stays, the command never runs.
    let config = dir.join("home/config.toml");
    let changed = dir.join("config-changed-ran");
    let rewrite = config.clone();
    let request = create_request(&server, off(&host, &server), touch(&changed));
    between_create_and_input(move || std::fs::write(&rewrite, CANONICAL_CONFIG).unwrap());
    let outcome = host.create(&request);
    let kept = unconfirmed(&outcome);
    assert!(server.panes(transport).contains(&kept), "pane {kept} kept");

    // With the config newer than the server, no create or input is sent at all.
    let panes = server.panes(transport);
    assert_eq!(
        host.restore_resume(&server.launch_endpoint()),
        RestoreResume::Unverified(RestoreUnverified::ConfigChangedSinceStart)
    );
    let refused = dir.join("refused-ran");
    let outcome = host.create(&create_request(&server, first.clone(), touch(&refused)));
    assert!(
        matches!(outcome, HerdrCreateOutcome::NotSent(_)),
        "{outcome:?}"
    );
    let not_off = Ok(HostMutation::Refused(HostRefusal::Precondition(
        RESTORE_RESUME_NOT_OFF.into(),
    )));
    assert_eq!(input.send_text(target, "NOPE"), not_off);
    assert_eq!(input.send_keys(target, &["Enter"]), not_off);
    assert_eq!(server.panes(transport), panes, "nothing created");
    age(&config);

    // A missing cwd is refused before any call; a pane Herdr opened elsewhere (HOME) is
    // kept but never gets the command.
    let missing = dir.join("missing-ran");
    let mut lost = create_request(&server, off(&host, &server), touch(&missing));
    lost.cwd = dir.join("missing");
    let outcome = host.create(&lost);
    assert!(
        matches!(outcome, HerdrCreateOutcome::NotSent(_)),
        "{outcome:?}"
    );
    assert_eq!(server.panes(transport), panes, "nothing created");
    let outcome = host.create_eligible(&lost);
    let home_pane = unconfirmed(&outcome);
    assert!(server.panes(transport).contains(&home_pane));

    // The server replaced between create and input: the new one never gets the command,
    // nor any create or input carrying the old server's witness.
    let old = off(&host, &server);
    let replaced = dir.join("replaced-ran");
    between_create_and_input(server.replace_handle());
    let outcome = host.create(&create_request(&server, old.clone(), touch(&replaced)));
    unconfirmed(&outcome);
    let new = off(&host, &server);
    assert_eq!(new.pid, server.pid());
    assert_ne!(new, old, "a new server process");
    let panes = server.panes(transport);
    let stale = dir.join("stale-ran");
    let outcome = host.create(&create_request(&server, old.clone(), touch(&stale)));
    assert!(
        matches!(outcome, HerdrCreateOutcome::NotSent(_)),
        "{outcome:?}"
    );
    let call = HerdrCall {
        id: "stale-input".into(),
        request: HerdrRequest::PaneSendInput {
            pane_id: panes.first().cloned().unwrap_or_else(|| pane.clone()),
            text: touch(&stale),
            keys: vec!["enter".into()],
        },
    };
    let outcome = transport.call_with_witness(&call, &old);
    assert!(
        matches!(outcome, Err(contract::HerdrTransportError::NotSent(_))),
        "{outcome:?}"
    );
    assert_eq!(
        server.panes(transport),
        panes,
        "nothing created on the new server"
    );

    std::thread::sleep(Duration::from_millis(1_500));
    for marker in [&changed, &refused, &missing, &replaced, &stale] {
        assert!(!marker.exists(), "{} ran", marker.display());
    }
    drop(server);

    // A non-canonical config, here a broken file whose text still says false, is never Off.
    let broken = b"[session]\nresume_agents_on_restore = false\n[[[broken\n";
    let server = RealServer::start("r2-broken", broken);
    let host = SocketHerdrLaunchHost::new(vec![server.endpoint()], HerdrSocketConfig::default());
    assert_eq!(
        host.restore_resume(&server.launch_endpoint()),
        RestoreResume::Unverified(RestoreUnverified::ConfigNotCanonical)
    );
    let marker = server.dir.join("broken-ran");
    let refused = host.create(&create_request(&server, first, touch(&marker)));
    assert!(
        matches!(refused, HerdrCreateOutcome::NotSent(_)),
        "{refused:?}"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists());
}
