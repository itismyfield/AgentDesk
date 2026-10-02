//! A force-kill of a process-backend turn reaches the cleanup executor's PID-tree kill.

use std::os::unix::process::CommandExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::provider::cancel_token_cleanup::executor::{
    pid_kill_dispatches_for_test, with_executor_dispatch_seam,
};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_backend::SessionHandle;

/// The spawned CLI stand-in, killed and reaped even when an assertion fails.
struct Spawned(std::process::Child);

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The test build counts the executor's identity-guarded SIGTERM-then-SIGKILL tree kill, so a
/// CLI ignoring SIGINT ends in production only if the force-kill dispatches it once.
#[test]
fn force_kill_of_a_process_backend_turn_dispatches_its_pid_kill_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let sigint = super::super::tests::SIGINT_TEST_LOCK.lock();
    let _sigint = sigint.unwrap_or_else(|poison| poison.into_inner());
    let _ = super::super::process_table::take_sigint_test_events();
    let mut command = std::process::Command::new("sleep");
    let child = Spawned(command.arg("600").process_group(0).spawn().unwrap());
    let pid = child.0.id();
    with_executor_dispatch_seam(|| {
        let mut runtime = tokio::runtime::Builder::new_current_thread();
        let runtime = runtime.enable_all().build().unwrap();
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
            let session = format!("process-force-kill-{pid}");
            let alive = Arc::new(AtomicBool::new(true));
            let handle = SessionHandle::TestProcess {
                pid,
                alive: alive.clone(),
            };
            crate::services::session_backend::insert_process_session(session.clone(), handle);
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
            let sigints = super::super::process_table::take_sigint_test_events();
            assert_eq!(sigints, [pid], "the verified CLI gets SIGINT first");
            assert_eq!(pid_kill_dispatches_for_test(), 1, "then its tree kill");
            assert!(
                !alive.load(Ordering::SeqCst),
                "the registry handle is stopped"
            );
            let entry = crate::services::session_backend::remove_process_session(&session);
            assert!(entry.is_none());
            pool.close().await;
            db.drop().await;
        });
    });
}
