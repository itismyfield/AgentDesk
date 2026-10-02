//! A force-kill of a process-backend turn ends its wrapper and the CLI group the wrapper
//! started apart from its own, with real signals.

use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::discord::SharedData;
use crate::services::discord::health::HealthRegistry;
use crate::services::process::ProcessIdentity;
use crate::services::provider::cancel_token_cleanup::executor::with_real_pid_kill;
use crate::services::provider::cancel_token_cleanup::target::CapturedProcess;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_backend::SessionHandle;

const TEST: &str = "services::discord::turn_bridge::tmux_runtime::process_backend_cancel::tests::force_kill_ends_a_process_backend_wrapper_and_its_cli_group_pg";
/// Set in the re-run of this test binary that plays the wrapper: the CLI it starts.
const WRAPPER_CLI: &str = "ADK_FORCE_KILL_FIXTURE_CLI";

/// Signalable and not a zombie awaiting a reaper (`ps` reads both on macOS and Linux).
fn alive(pid: u32) -> bool {
    #[allow(unsafe_code)]
    let exists = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
    exists && !ps(pid, "stat=").starts_with('Z')
}

fn ps(pid: u32, field: &str) -> String {
    let out = Command::new("ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .output();
    out.map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

fn pgid(pid: u32) -> u32 {
    #[allow(unsafe_code)]
    let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
    pgid as u32
}

fn gone_within(pids: &[u32], within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while pids.iter().any(|pid| alive(*pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    !pids.iter().any(|pid| alive(*pid))
}

/// Process groups this test started, killed when it ends however it ends.
struct Spawned(Vec<u32>);

impl Drop for Spawned {
    fn drop(&mut self) {
        for pgid in &self.0 {
            #[allow(unsafe_code)]
            unsafe {
                libc::kill(-(*pgid as libc::pid_t), libc::SIGKILL)
            };
        }
    }
}

/// This test binary re-run as a wrapper that starts a `codex` CLI ignoring `ignored` in a group
/// of its own, as the Codex and Qwen wrappers do; returns once the CLI installed its traps.
fn wrapper_with_cli(dir: &std::path::Path, ignored: &str, spawned: &mut Spawned) -> (Child, u32) {
    let dir = dir.join(spawned.0.len().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let (cli, ready) = (dir.join("codex"), dir.join("ready"));
    let ready = ready.display();
    let body = format!(
        "#!/usr/bin/env sh\ntrap '' {ignored}\necho $$ > '{ready}.tmp' && mv '{ready}.tmp' '{ready}'\nwhile :; do sleep 1; done\n"
    );
    std::fs::write(&cli, body).unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST, "--test-threads=1"])
        .env(WRAPPER_CLI, &cli);
    let command = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let wrapper = command.process_group(0).spawn().unwrap();
    spawned.0.push(wrapper.id());
    let deadline = Instant::now() + Duration::from_secs(10);
    let cli = loop {
        let raw = std::fs::read_to_string(dir.join("ready")).unwrap_or_default();
        if let Ok(pid) = raw.trim().parse::<u32>() {
            break pid;
        }
        assert!(Instant::now() < deadline, "the CLI installed its traps");
        std::thread::sleep(Duration::from_millis(20));
    };
    spawned.0.push(cli);
    let shape = (pgid(wrapper.id()), pgid(cli), ps(cli, "ppid="));
    let expected = (wrapper.id(), cli, wrapper.id().to_string());
    assert_eq!(
        shape, expected,
        "two groups, the CLI a child of the wrapper"
    );
    (wrapper, cli)
}

/// Puts `wrapper` in the process registry under its own session name.
fn register(mut wrapper: Child) -> String {
    let pid = wrapper.id();
    let handle = SessionHandle::Process {
        child_stdin: Arc::new(Mutex::new(wrapper.stdin.take())),
        child: Arc::new(Mutex::new(Some(wrapper))),
        pid,
        output: Arc::new(Mutex::new(tempfile::tempfile().unwrap())),
    };
    let session = format!("process-force-kill-{pid}");
    crate::services::session_backend::insert_process_session(session.clone(), handle);
    session
}

/// A turn on `channel` whose token records `child` as its wrapper.
async fn turn(shared: &SharedData, channel: ChannelId, child: CapturedProcess) {
    let token = Arc::new(CancelToken::new());
    let user_msg = MessageId::new(channel.get() + 1);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(shared, channel, token.clone(), UserId::new(7), user_msg).await);
    token.store_child_process_for_test(child);
}

async fn force_kill(registry: &HealthRegistry, channel: ChannelId, tmux_name: &str) -> bool {
    let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
        provider: Some(ProviderKind::Codex),
        channel_id: Some(channel),
        tmux_name: tmux_name.to_string(),
    };
    let kill = crate::services::turn_lifecycle::force_kill_turn;
    let lifecycle = kill(
        Some(registry),
        &target,
        "operator cleanup",
        "force_kill_api",
    );
    lifecycle.await.host_guard_kept()
}

/// With real signals a force-kill ends the token's wrapper and its CLI group, even a CLI ignoring
/// SIGTERM; a token identity that is not the wrapper's, or another host, ends nothing.
#[test]
fn force_kill_ends_a_process_backend_wrapper_and_its_cli_group_pg() {
    if let Some(cli) = std::env::var_os(WRAPPER_CLI) {
        let mut cli = Command::new(cli);
        crate::services::process::configure_child_process_group(&mut cli);
        let _ = cli.stdin(Stdio::null()).spawn().unwrap().wait();
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let sigint = super::super::tests::SIGINT_TEST_LOCK.lock();
    let _sigint = sigint.unwrap_or_else(|poison| poison.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    let mut spawned = Spawned(Vec::new());
    let mut runtime = tokio::runtime::Builder::new_current_thread();
    let runtime = runtime.enable_all().build().unwrap();
    // Every step of this current-thread runtime runs here, so its signals are sent for real.
    let real_sigint = super::super::process_table::with_real_sigint;
    with_real_pid_kill(|| {
        real_sigint(|| {
            runtime.block_on(async {
                let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
                let pool = db.connect_and_migrate().await;
                let shared = crate::services::discord::host_teardown_gate::test_support::shared_on;
                let shared = shared(&pool).await;
                shared.settings.write().await.provider = ProviderKind::Codex;
                let registry = HealthRegistry::new();
                registry.register("codex".to_string(), shared.clone()).await;
                let channel = |n: u64| ChannelId::new(1_479_671_301_387_088_000 + n);
                for (n, ignored) in ["INT", "INT TERM"].into_iter().enumerate() {
                    let (wrapper, cli) = wrapper_with_cli(dir.path(), ignored, &mut spawned);
                    let pid = wrapper.id();
                    register(wrapper);
                    turn(&shared, channel(n as u64), CapturedProcess::capture(pid)).await;
                    assert!(
                        !force_kill(&registry, channel(n as u64), "").await,
                        "{ignored}"
                    );
                    let gone = gone_within(&[pid, cli], Duration::from_secs(5));
                    assert!(gone, "{ignored}: the wrapper and its CLI group are gone");
                }

                // The token alone authorizes the kill, the registry only SIGINT: a wrapper the
                // registry lacks still ends, and the one it holds survives.
                let (mut wrapper, cli) = wrapper_with_cli(dir.path(), "INT TERM", &mut spawned);
                let (held, held_cli) = wrapper_with_cli(dir.path(), "INT TERM", &mut spawned);
                let held_pid = held.id();
                let held = register(held);
                turn(&shared, channel(5), CapturedProcess::capture(wrapper.id())).await;
                assert!(!force_kill(&registry, channel(5), "").await);
                let gone = gone_within(&[wrapper.id(), cli], Duration::from_secs(5));
                assert!(gone, "the token's own wrapper and CLI group are gone");
                assert!(
                    alive(held_pid) && alive(held_cli),
                    "the registered session survives"
                );
                let pid = crate::services::session_backend::process_session_pid(&held);
                assert_eq!(pid, Some(held_pid), "and stays registered");
                let _ = wrapper.wait();

                // A token whose recorded identity is not the registered wrapper's ends nothing.
                let (wrapper, cli) = wrapper_with_cli(dir.path(), "INT TERM", &mut spawned);
                let pid = wrapper.id();
                register(wrapper);
                let start = ProcessIdentity::capture(pid).raw_starttime();
                let other = ProcessIdentity::from_raw_for_test(start.map(|s| s.wrapping_add(1)));
                let identity = Some(other);
                turn(&shared, channel(6), CapturedProcess { pid, identity }).await;
                assert!(!force_kill(&registry, channel(6), "").await);
                assert!(!gone_within(&[pid], Duration::from_millis(600)) && alive(cli));

                // Another host's session is refused before any signal.
                let (wrapper, cli) = wrapper_with_cli(dir.path(), "INT TERM", &mut spawned);
                let pid = wrapper.id();
                let name = ProviderKind::Codex.build_tmux_session_name("ua-force-kill-herdr");
                let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
                std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
                std::fs::write(&marker, "herdr").unwrap();
                register(wrapper);
                turn(&shared, channel(7), CapturedProcess::capture(pid)).await;
                assert!(
                    force_kill(&registry, channel(7), &name).await,
                    "another host is kept"
                );
                assert!(!gone_within(&[pid], Duration::from_millis(600)) && alive(cli));

                for session in [held, format!("process-force-kill-{pid}")] {
                    let kept = crate::services::session_backend::remove_process_session(&session);
                    kept.map(crate::services::session_backend::terminate_process_handle);
                }
                pool.close().await;
                db.drop().await;
            })
        })
    });
}
