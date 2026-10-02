//! A force-kill of a process-backend turn ends a CLI that ignores SIGINT, for real.

use std::os::unix::process::CommandExt as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

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

/// A group leader ignoring SIGINT and SIGTERM whose forked `sleep` inherits both; returns the
/// child and the `sleep` PID.
fn ignoring_group(dir: &std::path::Path, spawned: &mut Spawned) -> (std::process::Child, u32) {
    let file = dir.join("sleep.pid");
    let script = format!(
        "trap '' INT TERM; sleep 600 & echo $! > '{}'; wait",
        file.display()
    );
    let mut command = std::process::Command::new("sh");
    let command = command.arg("-c").arg(script).process_group(0);
    let child = command.stdin(std::process::Stdio::piped()).spawn().unwrap();
    spawned.0.push(child.id());
    let deadline = Instant::now() + Duration::from_secs(5);
    let sleep = loop {
        let raw = std::fs::read_to_string(&file).unwrap_or_default();
        if let Ok(pid) = raw.trim().parse::<u32>() {
            break pid;
        }
        assert!(Instant::now() < deadline, "the group's sleep started");
        std::thread::sleep(Duration::from_millis(20));
    };
    (child, sleep)
}

/// A force-kill of a nameless process-backend turn whose registered CLI ignores SIGINT ends
/// that CLI's whole group within the grace; a process outside the group is untouched.
#[test]
fn force_kill_ends_a_process_backend_cli_that_ignores_sigint_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let sigint = super::super::tests::SIGINT_TEST_LOCK.lock();
    let _sigint = sigint.unwrap_or_else(|poison| poison.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    let mut spawned = Spawned(Vec::new());
    let (mut child, sleep) = ignoring_group(dir.path(), &mut spawned);
    let mut bystander = std::process::Command::new("sleep");
    let mut bystander = bystander.arg("600").process_group(0).spawn().unwrap();
    spawned.0.push(bystander.id());
    let pid = child.id();
    let handle = SessionHandle::Process {
        child_stdin: Arc::new(Mutex::new(child.stdin.take())),
        child: Arc::new(Mutex::new(Some(child))),
        pid,
        output: Arc::new(Mutex::new(tempfile::tempfile().unwrap())),
    };
    let session = format!("process-force-kill-{pid}");
    crate::services::session_backend::insert_process_session(session.clone(), handle);
    let mut runtime = tokio::runtime::Builder::new_current_thread();
    let runtime = runtime.enable_all().build().unwrap();
    // Every step of this current-thread runtime runs here, so its PID kill is sent for real.
    with_real_pid_kill(|| {
        runtime.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let runtime = crate::services::discord::host_teardown_gate::test_support::runtime;
            let (shared, registry) = runtime(&pool).await;
            let channel = ChannelId::new(1_479_671_301_387_088_001);
            let token = Arc::new(CancelToken::new());
            let start = crate::services::discord::mailbox_try_start_turn;
            let user_msg = MessageId::new(channel.get() + 1);
            assert!(start(&shared, channel, token.clone(), UserId::new(7), user_msg).await);
            crate::services::provider::register_child_pid(Some(&token), pid);
            let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
                provider: Some(ProviderKind::Claude),
                channel_id: Some(channel),
                tmux_name: String::new(),
            };
            let kill = crate::services::turn_lifecycle::force_kill_turn;
            let lifecycle = kill(
                Some(&registry),
                &target,
                "operator cleanup",
                "force_kill_api",
            );
            let lifecycle = lifecycle.await;
            assert!(!lifecycle.host_guard_kept(), "{lifecycle:?}");
            pool.close().await;
            db.drop().await;
        })
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while (alive(pid) || alive(sleep)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(pid) && !alive(sleep),
        "the CLI group is gone and reaped"
    );
    let outside = bystander.try_wait().unwrap();
    assert_eq!(outside, None, "a process outside the group survives");
    let _ = crate::services::session_backend::remove_process_session(&session);
    let _ = bystander.kill();
    let _ = bystander.wait();
}
