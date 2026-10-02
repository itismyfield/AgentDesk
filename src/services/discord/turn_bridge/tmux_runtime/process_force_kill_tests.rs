//! A force-kill of a process-backend turn ends its wrapper and the CLI group the wrapper
//! started apart from its own, for real.

use std::os::unix::process::CommandExt as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::discord::SharedData;
use crate::services::discord::health::HealthRegistry;
use crate::services::provider::cancel_token_cleanup::executor::with_real_pid_kill;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_backend::SessionHandle;

/// Signalable and not a zombie awaiting a reaper (`ps` reads both on macOS and Linux).
fn alive(pid: u32) -> bool {
    #[allow(unsafe_code)]
    let exists = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
    let mut ps = std::process::Command::new("ps");
    let stat = ps.args(["-o", "stat=", "-p", &pid.to_string()]).output();
    let stat = stat.map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    exists && !stat.unwrap_or_default().starts_with('Z')
}

fn pgid(pid: u32) -> u32 {
    #[allow(unsafe_code)]
    let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
    pgid as u32
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

/// A wrapper leading its own group that starts a CLI ignoring `ignored` in a second group, as
/// the Codex and Qwen wrappers do; returns the wrapper and the CLI PID.
fn wrapper_with_cli(
    dir: &std::path::Path,
    ignored: &str,
    spawned: &mut Spawned,
) -> (std::process::Child, u32) {
    let file = dir.join(format!("cli-{}.pid", spawned.0.len()));
    let cli = format!("sh -c \"trap '' {ignored}; exec sleep 600\"");
    let script = format!("set -m; {cli} & echo $! > '{}'; wait", file.display());
    let mut command = std::process::Command::new("sh");
    let command = command.arg("-c").arg(script).process_group(0);
    let wrapper = command.stdin(std::process::Stdio::piped()).spawn().unwrap();
    spawned.0.push(wrapper.id());
    let deadline = Instant::now() + Duration::from_secs(5);
    let cli = loop {
        let raw = std::fs::read_to_string(&file).unwrap_or_default();
        if let Ok(pid) = raw.trim().parse::<u32>() {
            break pid;
        }
        assert!(Instant::now() < deadline, "the CLI started");
        std::thread::sleep(Duration::from_millis(20));
    };
    spawned.0.push(cli);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(pgid(cli), cli, "the CLI leads its own group");
    assert_ne!(pgid(cli), pgid(wrapper.id()), "apart from the wrapper's");
    (wrapper, cli)
}

/// Registers `wrapper` as a channel's process session, as the launch does, and starts a turn.
async fn process_turn(
    shared: &SharedData,
    channel: ChannelId,
    mut wrapper: std::process::Child,
) -> Arc<CancelToken> {
    let pid = wrapper.id();
    let handle = SessionHandle::Process {
        child_stdin: Arc::new(Mutex::new(wrapper.stdin.take())),
        child: Arc::new(Mutex::new(Some(wrapper))),
        pid,
        output: Arc::new(Mutex::new(tempfile::tempfile().unwrap())),
    };
    let session = format!("process-force-kill-{pid}");
    crate::services::session_backend::insert_process_session(session, handle);
    let token = Arc::new(CancelToken::new());
    let user_msg = MessageId::new(channel.get() + 1);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(shared, channel, token.clone(), UserId::new(7), user_msg).await);
    crate::services::provider::register_child_pid(Some(&token), pid);
    token
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

fn gone_within(pids: &[u32], within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while pids.iter().any(|pid| alive(*pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    !pids.iter().any(|pid| alive(*pid))
}

/// A force-kill of a process-backend turn ends the wrapper and its separate CLI group, also
/// when the CLI ignores SIGTERM; another host's session and an unrelated process survive.
#[test]
fn force_kill_ends_a_process_backend_wrapper_and_its_cli_group_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let sigint = super::super::tests::SIGINT_TEST_LOCK.lock();
    let _sigint = sigint.unwrap_or_else(|poison| poison.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    let mut spawned = Spawned(Vec::new());
    let mut bystander = std::process::Command::new("sleep");
    let mut bystander = bystander.arg("600").process_group(0).spawn().unwrap();
    spawned.0.push(bystander.id());
    let mut runtime = tokio::runtime::Builder::new_current_thread();
    let runtime = runtime.enable_all().build().unwrap();
    // Every step of this current-thread runtime runs here, so its PID kills are sent for real.
    with_real_pid_kill(|| {
        runtime.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let shared = crate::services::discord::host_teardown_gate::test_support::shared_on;
            let shared = shared(&pool).await;
            shared.settings.write().await.provider = ProviderKind::Codex;
            let registry = HealthRegistry::new();
            registry.register("codex".to_string(), shared.clone()).await;
            let registry = &registry;
            for (n, ignored) in ["INT", "INT TERM"].into_iter().enumerate() {
                let channel = ChannelId::new(1_479_671_301_387_088_001 + n as u64);
                let (wrapper, cli) = wrapper_with_cli(dir.path(), ignored, &mut spawned);
                let pid = wrapper.id();
                process_turn(&shared, channel, wrapper).await;
                assert!(!force_kill(registry, channel, "").await, "{ignored}");
                let gone = gone_within(&[pid, cli], Duration::from_secs(5));
                assert!(gone, "{ignored}: the wrapper and its CLI group are gone");
            }

            // A session another host owns is refused before any signal.
            let channel = ChannelId::new(1_479_671_301_387_088_009);
            let (wrapper, cli) = wrapper_with_cli(dir.path(), "INT TERM", &mut spawned);
            let pid = wrapper.id();
            let name = ProviderKind::Codex.build_tmux_session_name("ua-force-kill-herdr");
            let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(&marker, "herdr").unwrap();
            process_turn(&shared, channel, wrapper).await;
            assert!(
                force_kill(registry, channel, &name).await,
                "another host is kept"
            );
            assert!(!gone_within(&[pid], Duration::from_secs(1)) && alive(cli));
            let session = format!("process-force-kill-{pid}");
            let kept = crate::services::session_backend::remove_process_session(&session);
            kept.map(crate::services::session_backend::terminate_process_handle);
            pool.close().await;
            db.drop().await;
        })
    });
    let outside = bystander.try_wait().unwrap();
    assert_eq!(outside, None, "a process outside the groups survives");
    let _ = bystander.kill();
    let _ = bystander.wait();
}
