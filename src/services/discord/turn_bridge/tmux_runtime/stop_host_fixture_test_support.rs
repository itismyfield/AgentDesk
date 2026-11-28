//! Stop host verdicts at the stop, bind, cleanup, release and runtime-clear entry points.
#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};

use super::super::process_table;
use super::*;
use crate::services::claude_tui::host_input::InputRefusal;
use crate::services::session_host::{
    HostCapabilities, HostError, HostLiveness, HostMutation, HostPresence, HostSessionRef,
    SessionTargetInput, TargetSource,
};

/// How the PATH-first tmux answers after logging a call.
#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) enum Server {
    /// Every session exists with a live pane showing a busy Claude turn.
    Live,
    /// The binary cannot run: exit 127.
    Missing,
    /// The binary runs but finds no server socket.
    NoSocket,
}

pub(in crate::services::discord) const SERVERS: [Server; 3] =
    [Server::Live, Server::Missing, Server::NoSocket];

/// What the `.host_kind` marker holds; `Unreadable` is a directory in its place.
#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) enum Mark {
    Absent,
    Herdr,
    Process,
    Zellij,
    Unreadable,
}

pub(in crate::services::discord) const NOT_TMUX: [Mark; 4] =
    [Mark::Herdr, Mark::Process, Mark::Zellij, Mark::Unreadable];

pub(in crate::services::discord) fn mark(name: &str, mark: Mark) {
    let path = crate::services::tmux_common::session_temp_path(name, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&path);
    match mark {
        Mark::Absent => {}
        Mark::Herdr => std::fs::write(&path, "herdr").unwrap(),
        Mark::Process => std::fs::write(&path, "process").unwrap(),
        Mark::Zellij => std::fs::write(&path, "zellij").unwrap(),
        Mark::Unreadable => std::fs::create_dir(&path).unwrap(),
    }
}

/// A runtime root, a logging PATH-first tmux and a live `codex` it reports as every pane's
/// process. Fields drop in order: PATH is restored before the env and SIGINT locks are released.
pub(in crate::services::discord) struct Fixture {
    _env: crate::config::TestEnvVarGuard,
    dir: tempfile::TempDir,
    codex: std::process::Child,
    _sigint: std::sync::MutexGuard<'static, ()>,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Fixture {
    pub(in crate::services::discord) fn new() -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let lock =
            crate::services::discord::turn_bridge::tmux_runtime::tests::SIGINT_TEST_LOCK.lock();
        let sigint = lock.unwrap_or_else(|error| error.into_inner());
        let _ = process_table::take_sigint_test_events();
        let dir = tempfile::TempDir::new().expect("tmux dir");
        let binary = dir.path().join("tmux");
        let mut file = std::fs::File::create(&binary).expect("tmux");
        let identity = crate::services::session_identity_metadata::runtime_identity("stop-fixture")
            .unwrap()
            .environment_assignment()
            .unwrap();
        let identity = crate::services::process::shell_escape(&identity);
        writeln!(
            file,
            "#!/bin/sh\n[ \"$1\" = -u ] && shift\nd=\"$(dirname \"$0\")\"\n\
             case \"$*\" in *show-environment*AGENTDESK_IDENTITY*)\n\
             case \"$(cat \"$d/mode\")\" in\n\
             live) printf '%s\\n' {identity}; exit 0 ;;\n\
             missing) exit 127 ;;\n\
             *) echo \"no server running on $d/socket\" >&2; exit 1 ;;\nesac ;;\nesac\n\
             echo \"$*\" >> \"$d/calls\"\ncase \"$(cat \"$d/mode\")\" in\n\
             missing) exit 127 ;;\n\
             nosocket) echo \"no server running on $d/socket\" >&2; exit 1 ;;\nesac\n\
             case \"$1\" in\nsend-keys) [ -f \"$d/reject_escape\" ] && exit 1; exit 0 ;;\ndisplay-message) cat \"$d/pane_pid\"; exit 0 ;;\n\
             list-panes) echo 0; exit 0 ;;\n\
             capture-pane) [ -f \"$d/reject_capture\" ] && exit 1; r=\"$(cat \"$d/ready\" 2>/dev/null)\"\n\
             [ -n \"$r\" ] && case \"$*\" in *\"-t $r -S\"*) echo 'Ready for input (type message + Enter)'; exit 0 ;; esac\n\
             [ -f \"$d/pane\" ] && {{ cat \"$d/pane\"; exit 0; }}\n\
             echo '· Actioning… (4m 7s · esc to interrupt)'; exit 0 ;;\nesac\nexit 0"
        )
        .expect("tmux body");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let codex_path = dir.path().join("codex");
        std::os::unix::fs::symlink("/bin/sleep", &codex_path).unwrap();
        let codex = std::process::Command::new(&codex_path)
            .arg("600")
            .spawn()
            .expect("codex stand-in");
        // The PID lookups read `ps`; wait until it lists the stand-in by name.
        let listed = || {
            let ps = std::process::Command::new("ps")
                .args(["-p", &codex.id().to_string(), "-o", "command="])
                .output();
            ps.is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("codex"))
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !listed() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::write(dir.path().join("pane_pid"), codex.id().to_string()).unwrap();
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let path = std::env::join_paths(paths).expect("join PATH");
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let env = set("PATH", std::path::Path::new(&path));
        let fixture = Self {
            _root: root,
            _sigint: sigint,
            dir,
            _env: env,
            codex,
        };
        fixture.serve(Server::Live);
        fixture
    }

    pub(in crate::services::discord) fn serve(&self, server: Server) {
        let mode = match server {
            Server::Live => "live",
            Server::Missing => "missing",
            Server::NoSocket => "nosocket",
        };
        std::fs::write(self.dir.path().join("mode"), mode).unwrap();
    }

    /// Makes the pane of `session` read as a Claude prompt ready for input; every other stays busy.
    pub(in crate::services::discord) fn ready(&self, session: &str) {
        std::fs::write(self.dir.path().join("ready"), session).unwrap();
    }

    /// Makes every other pane print `text` in place of the default busy frame.
    pub(in crate::services::discord) fn pane(&self, text: &str) {
        std::fs::write(self.dir.path().join("pane"), text).unwrap();
    }

    pub(in crate::services::discord) fn reject_capture(&self, reject: bool) {
        let path = self.dir.path().join("reject_capture");
        if reject {
            std::fs::write(path, "reject").unwrap();
        } else {
            let _ = std::fs::remove_file(path);
        }
    }

    pub(in crate::services::discord) fn reject_escape(&self, reject: bool) {
        let path = self.dir.path().join("reject_escape");
        if reject {
            std::fs::write(path, "reject").unwrap();
        } else {
            let _ = std::fs::remove_file(path);
        }
    }

    pub(in crate::services::discord) fn pid(&self) -> u32 {
        self.codex.id()
    }

    pub(in crate::services::discord) fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The logged tmux calls, oldest first, and clears the log after writers finish.
    pub(in crate::services::discord) fn take_calls(&self) -> Vec<String> {
        let calls = self.calls();
        let _ = std::fs::remove_file(self.dir.path().join("calls"));
        calls
    }

    /// The SIGINTs delivered to the stand-in since the last call.
    pub(in crate::services::discord) fn take_sigints(&self) -> usize {
        let events = process_table::take_sigint_test_events();
        events.into_iter().filter(|pid| *pid == self.pid()).count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.codex.kill();
        let _ = self.codex.wait();
    }
}

pub(in crate::services::discord) fn run<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(future)
}

pub(in crate::services::discord) fn bound_token(
    provider: &ProviderKind,
    name: &str,
) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    if matches!(provider, ProviderKind::Claude) {
        token.bind_claude_tmux_session(name);
    } else {
        token.bind_unmanaged_session_name(name);
    }
    token
}

pub(in crate::services::discord) struct FakeHerdr {
    pub(in crate::services::discord) ops: Mutex<Vec<String>>,
    pub(in crate::services::discord) capture: Result<String, HostError>,
    pub(in crate::services::discord) keys: Result<HostMutation, HostError>,
}

impl FakeHerdr {
    pub(in crate::services::discord) fn new(keys: Result<HostMutation, HostError>) -> Arc<Self> {
        Arc::new(Self {
            ops: Mutex::default(),
            capture: Ok("· Actioning… (4m 7s · esc to interrupt)".to_string()),
            keys,
        })
    }

    fn record(&self, op: String) {
        self.ops.lock().unwrap().push(op);
    }

    pub(in crate::services::discord) fn escapes(&self) -> usize {
        let ops = self.ops.lock().unwrap();
        ops.iter()
            .filter(|op| *op == "send_keys pane-7 Escape")
            .count()
    }
}

impl InteractiveSessionHost for FakeHerdr {
    fn kind(&self) -> HostKind {
        HostKind::Herdr
    }
    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities::default()
    }
    fn presence(&self, _: HostSessionRef<'_>) -> HostPresence {
        HostPresence::Present
    }
    fn liveness(&self, _: HostSessionRef<'_>) -> HostLiveness {
        HostLiveness::Live
    }
    fn send_text(&self, session: HostSessionRef<'_>, _: &str) -> Result<HostMutation, HostError> {
        self.record(format!("send_text {}", session.name));
        Ok(HostMutation::Confirmed)
    }
    fn send_keys(
        &self,
        session: HostSessionRef<'_>,
        keys: &[&str],
    ) -> Result<HostMutation, HostError> {
        self.record(format!("send_keys {} {}", session.name, keys.join(",")));
        self.keys.clone()
    }
    fn interrupt(&self, session: HostSessionRef<'_>) -> Result<HostMutation, HostError> {
        self.record(format!("interrupt {}", session.name));
        Ok(HostMutation::Confirmed)
    }
    fn capture_screen(&self, session: HostSessionRef<'_>, _: i32) -> Result<String, HostError> {
        self.record(format!("capture {}", session.name));
        self.capture.clone()
    }
    fn current_working_dir(
        &self,
        _: HostSessionRef<'_>,
    ) -> Result<Option<std::path::PathBuf>, HostError> {
        Ok(None)
    }
    fn execution_pid(&self, _: HostSessionRef<'_>) -> Result<Option<u32>, HostError> {
        Ok(None)
    }
}

struct Gate(Result<(), InputRefusal>);

impl MutationGate for Gate {
    fn admit(&self, _session: &str) -> Result<(), InputRefusal> {
        self.0
    }
}

pub(in crate::services::discord) fn herdr_target(
    provider: &ProviderKind,
    session: &str,
    host: Arc<FakeHerdr>,
    gate: Result<(), InputRefusal>,
) -> StopTarget {
    let resolved = ResolvedSessionTarget {
        input: SessionTargetInput::SessionKey(session.to_string()),
        session_key: Some(session.to_string()),
        host: TargetHost::Known {
            kind: HostKind::Herdr,
            source: TargetSource::SessionRecord,
            name: "pane-7".to_string(),
        },
    };
    StopTarget::from_session_target(provider, session, &resolved, host, Arc::new(Gate(gate)))
}

/// A Claude turn bound to `session` whose transcript shows it generating its token's own input.
pub(in crate::services::discord) fn generating_turn(
    fx: &Fixture,
    session: &str,
) -> (Arc<CancelToken>, std::path::PathBuf) {
    let (token, transcript) = unpinned_generating_turn(fx, session);
    let user = std::fs::read_to_string(&transcript).unwrap();
    let end = user.find('\n').unwrap() as u64 + 1;
    #[cfg(unix)]
    if let Some(pin) = crate::services::claude_tui::input::UserReceipt::native_pin_before_tail(
        session,
        "go",
        Some("fixture-sid"),
        end,
    ) {
        token.native_stop.receipt_sink().adopt(pin);
    }
    (token, transcript)
}

/// The same generating turn whose token has no verified native input of its own.
pub(in crate::services::discord) fn unpinned_generating_turn(
    fx: &Fixture,
    session: &str,
) -> (Arc<CancelToken>, std::path::PathBuf) {
    let transcript = fx.dir.path().join(format!("{session}.jsonl"));
    let user = serde_json::json!({"type": "user", "uuid": format!("{session}-go"),
        "sessionId": "fixture-sid", "message": {"role": "user", "content": "go"}});
    let assistant = serde_json::json!({"type": "assistant", "message": {"content": []}});
    std::fs::write(&transcript, format!("{user}\n{assistant}\n")).unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(session, "spawn_nonce"),
        format!("{session}-pane"),
    )
    .unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    (bound_token(&ProviderKind::Claude, session), transcript)
}
