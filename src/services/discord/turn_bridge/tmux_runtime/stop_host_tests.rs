//! Stop host verdicts at the stop, bind, cleanup, release and runtime-clear entry points.
#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::super::{bind_cancel_token_tmux_runtime, process_table, stop_active_turn_on};
use super::*;
use crate::services::claude_tui::host_input::InputRefusal;
use crate::services::discord::InflightRestartMode;
use crate::services::provider::cancel_token_cleanup::executor::{
    TmuxCleanupIntent, pid_kill_dispatches_for_test, take_requested_intents_for_test,
    tmux_kill_dispatches_for_test, with_executor_dispatch_seam,
};
use crate::services::session_host::{
    HostCapabilities, HostError, HostLiveness, HostMutation, HostPresence, HostRefusal,
    HostSessionRef, SessionTargetInput, TargetSource,
};

/// How the PATH-first tmux answers after logging a call.
#[derive(Clone, Copy, Debug)]
pub(in super::super) enum Server {
    /// Every session exists with a live pane showing a busy Claude turn.
    Live,
    /// The binary cannot run: exit 127.
    Missing,
    /// The binary runs but finds no server socket.
    NoSocket,
}

pub(in super::super) const SERVERS: [Server; 3] = [Server::Live, Server::Missing, Server::NoSocket];

/// What the `.host_kind` marker holds; `Unreadable` is a directory in its place.
#[derive(Clone, Copy, Debug)]
pub(in super::super) enum Mark {
    Absent,
    Herdr,
    Process,
    Zellij,
    Unreadable,
}

pub(in super::super) const NOT_TMUX: [Mark; 4] =
    [Mark::Herdr, Mark::Process, Mark::Zellij, Mark::Unreadable];

pub(in super::super) fn mark(name: &str, mark: Mark) {
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
pub(in super::super) struct Fixture {
    _env: crate::config::TestEnvVarGuard,
    dir: tempfile::TempDir,
    codex: std::process::Child,
    _sigint: std::sync::MutexGuard<'static, ()>,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Fixture {
    pub(in super::super) fn new() -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let lock = super::super::tests::SIGINT_TEST_LOCK.lock();
        let sigint = lock.unwrap_or_else(|error| error.into_inner());
        let _ = process_table::take_sigint_test_events();
        let dir = tempfile::TempDir::new().expect("tmux dir");
        let binary = dir.path().join("tmux");
        let mut file = std::fs::File::create(&binary).expect("tmux");
        writeln!(
            file,
            "#!/bin/sh\n[ \"$1\" = -u ] && shift\nd=\"$(dirname \"$0\")\"\n\
             echo \"$*\" >> \"$d/calls\"\ncase \"$(cat \"$d/mode\")\" in\n\
             missing) exit 127 ;;\n\
             nosocket) echo \"no server running on $d/socket\" >&2; exit 1 ;;\nesac\n\
             case \"$1\" in\ndisplay-message) cat \"$d/pane_pid\"; exit 0 ;;\n\
             capture-pane) r=\"$(cat \"$d/ready\" 2>/dev/null)\"\n\
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

    pub(in super::super) fn serve(&self, server: Server) {
        let mode = match server {
            Server::Live => "live",
            Server::Missing => "missing",
            Server::NoSocket => "nosocket",
        };
        std::fs::write(self.dir.path().join("mode"), mode).unwrap();
    }

    /// Makes the pane of `session` read as a Claude prompt ready for input; every other stays busy.
    pub(in super::super) fn ready(&self, session: &str) {
        std::fs::write(self.dir.path().join("ready"), session).unwrap();
    }

    /// Makes every other pane print `text` in place of the default busy frame.
    pub(in super::super) fn pane(&self, text: &str) {
        std::fs::write(self.dir.path().join("pane"), text).unwrap();
    }

    pub(in super::super) fn pid(&self) -> u32 {
        self.codex.id()
    }

    /// The logged tmux calls, oldest first, and clears the log.
    pub(in super::super) fn take_calls(&self) -> Vec<String> {
        let log = self.dir.path().join("calls");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(log);
        calls.lines().map(str::to_string).collect()
    }

    /// The SIGINTs delivered to the stand-in since the last call.
    fn take_sigints(&self) -> usize {
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

pub(in super::super) fn run<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(future)
}

pub(in super::super) fn bound_token(provider: &ProviderKind, name: &str) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    if matches!(provider, ProviderKind::Claude) {
        token.bind_claude_tmux_session(name);
    } else {
        token.bind_unmanaged_session_name(name);
    }
    token
}

// A stop on a session whose marker is not tmux sends no key, signal, PID probe or kill on any
// tmux condition, still raises the cancel flag, and asks the executor only to preserve.
#[test]
fn a_stop_on_another_host_reaches_no_tmux_signal_or_kill() {
    let fx = Fixture::new();
    let drain = InflightRestartMode::DrainRestart;
    let policies = [
        TmuxCleanupPolicy::PreserveSession,
        TmuxCleanupPolicy::PreserveSessionAndInflight {
            restart_mode: drain,
        },
        TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some("test_stop"),
        },
    ];
    let providers = [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Qwen,
    ];
    with_executor_dispatch_seam(|| {
        run(async {
            let mut n = 0;
            for server in SERVERS {
                fx.serve(server);
                for host in NOT_TMUX {
                    for provider in &providers {
                        for policy in policies {
                            n += 1;
                            let name = format!("AgentDesk-{}-p6ao-stop-{n}", provider.as_str());
                            mark(&name, host);
                            let token = bound_token(provider, &name);
                            token.store_child_pid(fx.pid());
                            let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
                            let handle =
                                crate::services::session_backend::SessionHandle::TestProcess {
                                    pid: fx.pid(),
                                    alive: alive.clone(),
                                };
                            crate::services::session_backend::insert_process_session(
                                name.clone(),
                                handle,
                            );
                            take_requested_intents_for_test();

                            let recorded = super::super::stop_active_turn(
                                provider,
                                &token,
                                policy,
                                "explicit_stop",
                            )
                            .await;

                            let case = format!("{server:?} {host:?} {provider:?} {policy:?}");
                            assert!(!recorded, "{case}");
                            assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case}");
                            assert_eq!(fx.take_sigints(), 0, "{case}");
                            assert!(alive.load(Ordering::SeqCst), "{case}");
                            assert!(
                                !crate::services::session_backend::process_session_was_stopped(
                                    &name
                                ),
                                "{case}"
                            );
                            assert!(token.cancelled.load(Ordering::SeqCst), "{case}");
                            assert_eq!(token.restart_mode(), policy.preserves_inflight(), "{case}");
                            let intents = take_requested_intents_for_test();
                            assert_eq!(intents, vec![TmuxCleanupIntent::PreserveSession], "{case}");
                            assert_eq!(token.child_pid_value(), Some(fx.pid()), "{case}");
                            crate::services::session_backend::remove_process_session(&name);
                        }
                    }
                }
            }
            assert_eq!(pid_kill_dispatches_for_test(), 0);
            assert_eq!(tmux_kill_dispatches_for_test(), 0);

            // The same stop on a legacy session reaches the pane, so the log above saw no call.
            for server in SERVERS {
                fx.serve(server);
                let name = format!("AgentDesk-codex-p6ao-legacy-{server:?}");
                mark(&name, Mark::Absent);
                let token = bound_token(&ProviderKind::Codex, &name);
                let policy = TmuxCleanupPolicy::PreserveSession;
                super::super::stop_active_turn(
                    &ProviderKind::Codex,
                    &token,
                    policy,
                    "explicit_stop",
                )
                .await;
                let calls = fx.take_calls();
                assert!(
                    calls.iter().any(|call| call.starts_with("send-keys")),
                    "{server:?} {calls:?}"
                );
            }
        })
    });
}

// Every stage of a stop acts on the session it decided on: a token rebound to another name
// afterwards gets no probe, key or kill there, and the executor refuses the moved cleanup.
#[test]
fn a_stop_acts_only_on_the_session_it_decided() {
    let fx = Fixture::new();
    let cleanup = TmuxCleanupPolicy::CleanupSession {
        termination_reason_code: Some("test_stop"),
    };
    with_executor_dispatch_seam(|| {
        run(async {
            for (policy, moved) in [
                (TmuxCleanupPolicy::PreserveSession, true),
                (cleanup, true),
                (cleanup, false),
            ] {
                let first = format!(
                    "AgentDesk-codex-p6ao-first-{moved}-{}",
                    policy.should_cleanup_tmux()
                );
                let second = format!("{first}-second");
                mark(&first, Mark::Absent);
                mark(&second, Mark::Absent);
                let token = bound_token(&ProviderKind::Codex, &first);
                token.store_child_pid(fx.pid());
                let target = StopTarget::for_token(&token);
                if moved {
                    token.bind_unmanaged_session_name(&second);
                }
                let (pids, names) = (
                    pid_kill_dispatches_for_test(),
                    tmux_kill_dispatches_for_test(),
                );

                stop_active_turn_on(
                    &target,
                    &ProviderKind::Codex,
                    &token,
                    policy,
                    "explicit_stop",
                )
                .await;

                let calls = fx.take_calls();
                assert!(
                    calls
                        .iter()
                        .any(|call| call.contains(&format!("={first}:"))),
                    "{calls:?}"
                );
                assert!(
                    !calls.iter().any(|call| call.contains(&second)),
                    "{calls:?}"
                );
                let killed = (
                    pid_kill_dispatches_for_test() - pids,
                    tmux_kill_dispatches_for_test() - names,
                );
                let expected = if policy.should_cleanup_tmux() && !moved {
                    (1, 1)
                } else {
                    (0, 0)
                };
                assert_eq!(killed, expected, "{policy:?} moved={moved}");
                let _ = fx.take_sigints();
            }
        })
    });
}

// Binding a token to a session another host holds records its name but never searches that
// pane for a provider PID, so no later kill can target one; a legacy name registers as before.
#[test]
fn bind_registers_no_pid_for_another_hosts_session() {
    let fx = Fixture::new();
    for server in SERVERS {
        fx.serve(server);
        for host in NOT_TMUX {
            let name = format!("AgentDesk-codex-p6ao-bind-{server:?}-{host:?}");
            mark(&name, host);
            let token = Arc::new(CancelToken::new());
            let pid = bind_cancel_token_tmux_runtime(&ProviderKind::Codex, &token, &name, "test");
            assert_eq!(
                (pid, token.child_pid_value()),
                (None, None),
                "{server:?} {host:?}"
            );
            assert_eq!(token.tmux_session_name().as_deref(), Some(name.as_str()));
            assert_eq!(fx.take_calls(), Vec::<String>::new(), "{server:?} {host:?}");
        }
    }
    fx.serve(Server::Live);
    let name = "AgentDesk-codex-p6ao-bind-legacy";
    mark(name, Mark::Absent);
    let token = Arc::new(CancelToken::new());
    let pid = bind_cancel_token_tmux_runtime(&ProviderKind::Codex, &token, name, "test");
    assert_eq!(
        (pid, token.child_pid_value()),
        (Some(fx.pid()), Some(fx.pid()))
    );
    assert!(
        fx.take_calls()
            .iter()
            .any(|call| call.starts_with("display-message"))
    );
}

/// A legacy-bound token holding the stand-in's PID, whose marker then turns to `host`.
fn registered_then_moved(fx: &Fixture, name: &str, host: Mark) -> Arc<CancelToken> {
    fx.serve(Server::Live);
    mark(name, Mark::Absent);
    let token = Arc::new(CancelToken::new());
    bind_cancel_token_tmux_runtime(&ProviderKind::Codex, &token, name, "test");
    assert_eq!(
        token.child_pid_value(),
        Some(fx.pid()),
        "the bind registered the PID"
    );
    let _ = fx.take_calls();
    mark(name, host);
    token
}

// A PID a legacy bind registered is not killed once the session's marker stops reading tmux:
// stall cleanup, the legacy cleanup adapter and the cancel watchdog take no claim and no kill.
#[test]
fn a_registered_pid_is_not_killed_once_the_host_is_not_tmux() {
    let fx = Fixture::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    with_executor_dispatch_seam(|| {
        let mut n = 0;
        for server in SERVERS {
            for host in [Mark::Unreadable, Mark::Herdr] {
                for entry in ["stall", "adapter", "watchdog"] {
                    n += 1;
                    let name = format!("AgentDesk-codex-p6ao-registered-{n}");
                    let token = registered_then_moved(&fx, &name, host);
                    fx.serve(server);
                    let case = format!("{server:?} {host:?} {entry}");
                    match entry {
                        "stall" => {
                            crate::services::discord::stall_recovery::finalize_orphaned_clear(
                                &shared,
                                ChannelId::new(5_340_000 + n),
                                Some(token.clone()),
                                "stall_test",
                            )
                        }
                        "adapter" => token.cancel_with_tmux_cleanup(),
                        _ => {
                            token.cancelled.store(true, Ordering::SeqCst);
                            let spawn = crate::services::provider::spawn_cancel_watchdog;
                            let watchdog = spawn(Some(token.clone()), "p6ao");
                            std::thread::sleep(std::time::Duration::from_millis(350));
                            drop(watchdog);
                        }
                    }
                    assert_eq!(pid_kill_dispatches_for_test(), 0, "{case}");
                    assert_eq!(tmux_kill_dispatches_for_test(), 0, "{case}");
                    assert_eq!(token.pid_kill_claim.load(Ordering::SeqCst), 0, "{case}");
                    assert_eq!(token.name_kill_claim.load(Ordering::SeqCst), 0, "{case}");
                    assert!(token.cancelled.load(Ordering::SeqCst), "{case}");
                    assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case}");
                }
            }
        }
        let token =
            registered_then_moved(&fx, "AgentDesk-codex-p6ao-registered-legacy", Mark::Absent);
        token.cancel_with_tmux_cleanup();
        let killed = (
            pid_kill_dispatches_for_test(),
            tmux_kill_dispatches_for_test(),
        );
        assert_eq!(killed, (1, 1), "a legacy registration is killed as before");
    });
}

// A stopped turn on another host's session is never released as idle from a tmux reading,
// whatever tmux answers; a legacy session tmux reports missing is idle as before.
#[test]
fn zombie_release_never_reads_another_hosts_session_as_idle() {
    let fx = Fixture::new();
    let idle = crate::services::discord::zombie_foreground_release::tui_structurally_idle;
    for server in SERVERS {
        fx.serve(server);
        for host in NOT_TMUX {
            let name = format!("AgentDesk-claude-p6ao-zombie-{server:?}-{host:?}");
            mark(&name, host);
            let token = bound_token(&ProviderKind::Claude, &name);
            assert!(!idle(&ProviderKind::Claude, &token), "{server:?} {host:?}");
            assert_eq!(fx.take_calls(), Vec::<String>::new(), "{server:?} {host:?}");
        }
    }
    fx.serve(Server::NoSocket);
    let name = "AgentDesk-claude-p6ao-zombie-legacy";
    mark(name, Mark::Absent);
    let token = bound_token(&ProviderKind::Claude, name);
    assert!(idle(&ProviderKind::Claude, &token));
}

// The runtime cancel reads no pane of another host and keeps its turn: a non-tmux session gets
// no capture or any other tmux call and is not cancelled; a legacy session is stopped as in main.
#[test]
fn runtime_cancel_reads_no_pane_of_another_host() {
    let fx = Fixture::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    run(async {
        registry
            .register("claude".to_string(), shared.clone())
            .await;
        for (n, host) in NOT_TMUX.into_iter().chain([Mark::Absent]).enumerate() {
            let channel = ChannelId::new(5_340_100 + n as u64);
            let name = format!("AgentDesk-claude-p6ao-runtime-{n}");
            mark(&name, host);
            let token = bound_token(&ProviderKind::Claude, &name);
            let start = crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                UserId::new(7),
                MessageId::new(channel.get() + 1),
            );
            assert!(start.await);
            let stop = crate::services::discord::health::stop_provider_channel_runtime_with_policy;
            let policy = TmuxCleanupPolicy::PreserveSession;
            let result = stop(&registry, "claude", channel, "p6ao", policy).await;
            let kept = crate::services::discord::health::InflightDisposition::PreservedByHostGuard;
            let legacy = matches!(host, Mark::Absent);
            assert_eq!(result.unwrap().inflight == kept, !legacy, "{host:?}");
            assert_eq!(token.cancelled.load(Ordering::SeqCst), legacy, "{host:?}");
            let calls = fx.take_calls();
            let reached = calls.iter().any(|call| call.contains(&name));
            assert_eq!(reached, legacy, "{host:?} {calls:?}");
            assert_eq!(calls.is_empty(), !legacy, "{host:?} {calls:?}");
        }
    });
}

/// A Herdr host recording each operation, answering with the scripted capture and key result.
pub(in super::super) struct FakeHerdr {
    ops: Mutex<Vec<String>>,
    capture: Result<String, HostError>,
    keys: Result<HostMutation, HostError>,
}

impl FakeHerdr {
    pub(in super::super) fn new(keys: Result<HostMutation, HostError>) -> Arc<Self> {
        Arc::new(Self {
            ops: Mutex::default(),
            capture: Ok("· Actioning… (4m 7s · esc to interrupt)".to_string()),
            keys,
        })
    }

    fn record(&self, op: String) {
        self.ops.lock().unwrap().push(op);
    }

    fn escapes(&self) -> usize {
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

pub(in super::super) fn herdr_target(
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

/// A Claude turn bound to `session` whose transcript shows it generating.
pub(in super::super) fn generating_turn(
    fx: &Fixture,
    session: &str,
) -> (Arc<CancelToken>, std::path::PathBuf) {
    let transcript = fx.dir.path().join(format!("{session}.jsonl"));
    let user = serde_json::json!({"type": "user", "message": {"role": "user", "content": "go"}});
    let assistant = serde_json::json!({"type": "assistant", "message": {"content": []}});
    std::fs::write(&transcript, format!("{user}\n{assistant}\n")).unwrap();
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

async fn herdr_stop(target: &StopTarget, token: &Arc<CancelToken>) {
    let policy = TmuxCleanupPolicy::PreserveSession;
    stop_active_turn_on(
        target,
        &ProviderKind::Claude,
        token,
        policy,
        "explicit_stop",
    )
    .await;
}

// A verified Herdr Claude turn takes one Escape on its own pane, never the C-c interrupt; a
// sent or possibly sent Escape spends the claim, and only a surely unsent one is retried.
#[test]
fn a_herdr_claude_stop_sends_one_escape_and_spends_the_claim_only_once_sent() {
    let fx = Fixture::new();
    let precondition = HostRefusal::Precondition("restore resume not off".to_string());
    let unsupported = HostRefusal::Unsupported {
        kind: HostKind::Herdr,
        op: "send_keys",
    };
    with_executor_dispatch_seam(|| {
        run(async {
            for (n, (keys, spent)) in [
                (Ok(HostMutation::Confirmed), true),
                (
                    Ok(HostMutation::Indeterminate("ack lost".to_string())),
                    true,
                ),
                (Ok(HostMutation::Refused(precondition.clone())), false),
                (Ok(HostMutation::Refused(unsupported.clone())), false),
                (Err(HostError::Transport("not sent".to_string())), false),
            ]
            .into_iter()
            .enumerate()
            {
                let session = format!("AgentDesk-claude-p6ao-herdr-{n}");
                let (token, _) = generating_turn(&fx, &session);
                let host = FakeHerdr::new(keys.clone());
                let target = herdr_target(&ProviderKind::Claude, &session, host.clone(), Ok(()));
                let confirmed = matches!(keys, Ok(HostMutation::Confirmed));
                let outcome =
                    interrupt_unhosted(&target, &ProviderKind::Claude, &token, "explicit_stop")
                        .await;
                assert_eq!(outcome.sent_keys, confirmed, "{keys:?}");
                herdr_stop(&target, &token).await;
                let ops = host.ops.lock().unwrap().clone();
                assert!(!ops.iter().any(|op| op.starts_with("interrupt")), "{ops:?}");
                let expected = if spent { 1 } else { 2 };
                assert_eq!(host.escapes(), expected, "{keys:?}: {ops:?}");
                assert_eq!(fx.take_calls(), Vec::<String>::new());
                crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
            }
            assert_eq!(fx.take_sigints(), 0);
            assert_eq!(pid_kill_dispatches_for_test(), 0);
            assert_eq!(tmux_kill_dispatches_for_test(), 0);
        })
    });
}

// No Escape leaves for a Herdr turn the gate refuses, whose pane cannot be read or whose
// transcript moved to a newer turn; each keeps the claim for a later stop.
#[test]
fn a_herdr_claude_stop_writes_nothing_unless_every_fence_passes() {
    let fx = Fixture::new();
    run(async {
        let session = "AgentDesk-claude-p6ao-herdr-gate";
        let (token, _) = generating_turn(&fx, session);
        let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
        let refused = herdr_target(
            &ProviderKind::Claude,
            session,
            host.clone(),
            Err(InputRefusal::Unknown),
        );
        herdr_stop(&refused, &token).await;
        assert_eq!(host.escapes(), 0, "the gate refused");
        let admitted = herdr_target(&ProviderKind::Claude, session, host.clone(), Ok(()));
        herdr_stop(&admitted, &token).await;
        assert_eq!(host.escapes(), 1, "the refused stop kept the claim");

        let session = "AgentDesk-claude-p6ao-herdr-capture";
        let (token, _) = generating_turn(&fx, session);
        let blind = Arc::new(FakeHerdr {
            ops: Mutex::default(),
            capture: Err(HostError::Timeout),
            keys: Ok(HostMutation::Confirmed),
        });
        herdr_stop(
            &herdr_target(&ProviderKind::Claude, session, blind.clone(), Ok(())),
            &token,
        )
        .await;
        assert_eq!(blind.escapes(), 0, "an unread pane is ambiguous");

        let session = "AgentDesk-claude-p6ao-herdr-unbound";
        let token = bound_token(&ProviderKind::Claude, session);
        let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
        herdr_stop(
            &herdr_target(&ProviderKind::Claude, session, host.clone(), Ok(())),
            &token,
        )
        .await;
        assert_eq!(host.escapes(), 0, "no binding, no phase");

        let session = "AgentDesk-claude-p6ao-herdr-newer";
        let (token, transcript) = generating_turn(&fx, session);
        let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
        let target = herdr_target(&ProviderKind::Claude, session, host.clone(), Ok(()));
        let (parked, release) = (
            Arc::new(std::sync::Barrier::new(2)),
            Arc::new(std::sync::Barrier::new(2)),
        );
        let (held, go) = (parked.clone(), release.clone());
        let holder = std::thread::spawn(move || {
            crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session, || {
                held.wait();
                go.wait();
            })
        });
        parked.wait();
        let stop = tokio::spawn({
            let (target, token) = (target.clone(), token.clone());
            async move { herdr_stop(&target, &token).await }
        });
        while !host
            .ops
            .lock()
            .unwrap()
            .iter()
            .any(|op| op.starts_with("capture"))
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let newer =
            serde_json::json!({"type": "user", "message": {"role": "user", "content": "next"}});
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(file, "{newer}").unwrap();
        release.wait();
        holder.join().unwrap();
        stop.await.unwrap();
        assert_eq!(
            host.escapes(),
            0,
            "a newer turn landed while the stop waited"
        );
        assert_eq!(fx.take_calls(), Vec::<String>::new());
    });
}

// Codex and Qwen on Herdr are refused before any I/O: no host, E7 or tmux call.
#[test]
fn a_herdr_stop_for_another_provider_is_refused_before_any_io() {
    let fx = Fixture::new();
    run(async {
        for provider in [ProviderKind::Codex, ProviderKind::Qwen] {
            let session = format!("AgentDesk-{}-p6ao-herdr", provider.as_str());
            let token = bound_token(&provider, &session);
            let host = FakeHerdr::new(Ok(HostMutation::Confirmed));
            let target = herdr_target(&provider, &session, host.clone(), Ok(()));
            assert!(matches!(target, StopTarget::Refused { .. }), "{provider:?}");
            let policy = TmuxCleanupPolicy::PreserveSession;
            stop_active_turn_on(&target, &provider, &token, policy, "explicit_stop").await;
            assert!(host.ops.lock().unwrap().is_empty(), "{provider:?}");
            assert!(token.cancelled.load(Ordering::SeqCst));
            assert_eq!(fx.take_calls(), Vec::<String>::new());
        }
    });
}

/// A channel `channel_name` maps to, holding a provider session, history and a busy turn
/// bound to its tmux name, plus a live process session the reset would kill.
async fn seeded_runtime_channel(
    shared: &crate::services::discord::SharedData,
    channel: ChannelId,
    channel_name: &str,
    pid: u32,
) -> (String, Arc<CancelToken>, Arc<std::sync::atomic::AtomicBool>) {
    use crate::services::discord::host_defer_gate::tests::map_channel;
    map_channel(shared, channel, channel_name).await;
    let name = ProviderKind::Claude.build_tmux_session_name(channel_name);
    let mut core = shared.core.lock().await;
    let session = core.sessions.get_mut(&channel).unwrap();
    session.session_id = Some("sid".into());
    session.history.push(crate::ui::ai_screen::HistoryItem {
        item_type: crate::ui::ai_screen::HistoryType::User,
        content: "kept".into(),
    });
    drop(core);
    let token = bound_token(&ProviderKind::Claude, &name);
    let start = crate::services::discord::mailbox_try_start_turn(
        shared,
        channel,
        token.clone(),
        UserId::new(7),
        MessageId::new(channel.get() + 1),
    );
    assert!(start.await, "the seeded turn starts");
    let alive = crate::services::discord::admin_host_guard::tests::process(&name, pid);
    (name, token, alive)
}

/// Whether the channel still holds its session, history, uncleared flag and busy turn.
async fn runtime_kept(
    shared: &crate::services::discord::SharedData,
    channel: ChannelId,
    token: &CancelToken,
) -> bool {
    let snapshot = crate::services::discord::mailbox_snapshot(shared, channel).await;
    let core = shared.core.lock().await;
    let session = core.sessions.get(&channel).unwrap();
    session.session_id.as_deref() == Some("sid")
        && session.history.len() == 1
        && !session.cleared
        && snapshot.cancel_token.is_some()
        && !token.cancelled.load(Ordering::SeqCst)
}

// A runtime clear of a non-legacy session changes nothing, by routine reset or by an auto-queue
// slot clear the safety filter let through; a legacy session clears and kills as in main.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_clear_refuses_another_hosts_session_before_any_change_pg() {
    use crate::services::discord::admin_host_guard::ManagedReset;
    use crate::services::discord::host_defer_gate::tests::{Case, postgres};
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, busy_turn, channel_key, runtime, seed,
    };
    use crate::services::routines::session_control::{
        RoutineSessionCommand, RoutineSessionController,
    };
    let fx = Fixture::new();
    let (db, pool) = postgres().await;
    let (shared, registry) = runtime(&pool).await;
    let controller = RoutineSessionController::new(Arc::new(pool.clone()), Some(registry.clone()));
    for (n, case) in Case::ALL.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_302_534_000 + n as u64);
        let (agent, channel_name) = (format!("agent-p6ao-{n}"), format!("p6ao-reset-{n}"));
        sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_cc) VALUES ($1, $1, 'claude', $2)")
            .bind(&agent)
            .bind(channel.get().to_string())
            .execute(&pool)
            .await
            .unwrap();
        let pid = 65_000 + n as u32;
        let (name, token, alive) =
            seeded_runtime_channel(&shared, channel, &channel_name, pid).await;
        case.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
            .await;
        let active = shared.restart.global_active.load(Ordering::SeqCst);
        let _ = fx.take_calls();
        let routine = crate::services::routines::store::RoutineRecord {
            id: format!("routine-p6ao-{n}"),
            agent_id: Some(agent),
            fallback_agent_id: None,
            max_retries: 0,
            script_ref: "script".into(),
            name: "p6ao".into(),
            status: "enabled".into(),
            execution_strategy: "persistent".into(),
            schedule: None,
            next_due_at: None,
            last_run_at: None,
            last_result: None,
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: None,
            in_flight_run_id: None,
            pause_reason: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let reset =
            controller.control_persistent_session(&routine, RoutineSessionCommand::Reset, "p6ao");
        let result = reset.await.unwrap();
        if case.admitted() {
            assert_eq!(result.lifecycle_path, "runtime-clear", "{case:?}");
            assert!(!alive.load(Ordering::SeqCst), "{case:?}: main kills");
            continue;
        }
        assert_eq!(result.lifecycle_path, "runtime-clear-refused", "{case:?}");
        assert!(!result.runtime_cleared, "{case:?}");
        assert!(runtime_kept(&shared, channel, &token).await, "{case:?}");
        assert_eq!(
            shared.restart.global_active.load(Ordering::SeqCst),
            active,
            "{case:?}"
        );
        assert!(
            alive.load(Ordering::SeqCst),
            "{case:?}: the process is kept"
        );
        assert_eq!(fx.take_calls(), Vec::<String>::new(), "{case:?}");
        crate::services::session_backend::remove_process_session(&name);
    }

    // One slot, three threads: the slot filter keeps both hosted threads from any change and
    // only the legacy third reaches the runtime clear.
    sqlx::query(
        "INSERT INTO agents (id, name, provider) VALUES ('agent-p6ao-slot', 'slot', 'claude')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let threads: Vec<ChannelId> = (0..3)
        .map(|n| ChannelId::new(1_479_671_302_535_000 + n))
        .collect();
    let map: serde_json::Map<String, serde_json::Value> = threads
        .iter()
        .enumerate()
        .map(|(n, thread)| (n.to_string(), serde_json::json!(thread.get().to_string())))
        .collect();
    sqlx::query("INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map) VALUES ('agent-p6ao-slot', 0, $1)")
        .bind(serde_json::Value::Object(map))
        .execute(&pool)
        .await
        .unwrap();
    let mut seeded = Vec::new();
    for (n, (thread, stored)) in threads
        .iter()
        .zip([Stored::Hosted, Stored::Hosted, Stored::Legacy])
        .enumerate()
    {
        let channel_name = format!("p6ao-slot-{n}");
        let (name, token, alive) = if n == 0 {
            crate::services::discord::host_defer_gate::tests::map_channel(
                &shared,
                *thread,
                &channel_name,
            )
            .await;
            let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
            let token = busy_turn(&shared, *thread, &name).await;
            let alive = crate::services::discord::admin_host_guard::tests::process(&name, 66_000);
            (name, token, alive)
        } else {
            seeded_runtime_channel(&shared, *thread, &channel_name, 66_000 + n as u32).await
        };
        let key = channel_key(&shared, &name);
        seed(&pool, &key, &name, thread.get(), stored).await;
        sqlx::query("UPDATE sessions SET thread_channel_id = $2 WHERE session_key = $1")
            .bind(&key)
            .bind(thread.get().to_string())
            .execute(&pool)
            .await
            .unwrap();
        seeded.push((name, token, alive));
    }
    let _ = fx.take_calls();
    let clear = crate::services::auto_queue::runtime::clear_slot_threads_for_slot_pg;
    clear(Some(registry.clone()), &pool, "agent-p6ao-slot", 0)
        .await
        .unwrap();
    let ids: Vec<u64> = threads.iter().map(|thread| thread.get()).collect();
    let after =
        crate::services::auto_queue::runtime::slot_reset_host_pg_tests::runtime_clears_after_done;
    let clears = after(&ids).await;
    assert!(
        matches!(clears.as_slice(), [(thread, Some(ManagedReset::Applied(_)))] if *thread == ids[2]),
        "{clears:?}"
    );
    assert!(runtime_kept(&shared, threads[1], &seeded[1].1).await);
    assert!(
        seeded[1].2.load(Ordering::SeqCst),
        "the refused thread's process is kept"
    );
    assert!(
        !seeded[2].2.load(Ordering::SeqCst),
        "the legacy thread is killed as in main"
    );
    for (name, _, _) in &seeded {
        crate::services::session_backend::remove_process_session(name);
    }
    pool.close().await;
    db.drop().await;
}
