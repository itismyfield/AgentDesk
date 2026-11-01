//! The replaced-server owner and pane-end read against a real protocol-22 herdr in an isolated
//! HOME under `/private/tmp`, granted to an old server that no longer answers.
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use super::*;
use crate::services::discord::herdr_terminate::{ExecutionEnd, execution_end};
use crate::services::session_host::StartIdentity;
use crate::services::session_host::autostart::real_tests::{FALSE, Home};
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};

/// A test that rereads this file's runtime root and the user_default endpoint of `home`.
struct Real {
    fx: Fx,
    home: Home,
    _root: (tempfile::TempDir, [crate::config::TestEnvVarGuard; 2]),
}

impl Real {
    async fn new() -> Self {
        let home = Home::new(FALSE);
        let root = crate::services::tui_prompt_dedupe::binding_context::tests::fixture();
        let runtime_root = crate::config::runtime_root().unwrap().display().to_string();
        // The launch marked its session as herdr-hosted.
        let marker = crate::services::tmux_common::session_temp_path(LOGICAL, "host_kind");
        let marker = std::path::Path::new(&marker);
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        let context = Context {
            env: home.env(),
            local_node: Some("node".into()),
            records: Some(home.records()),
            grace_rounds: 0,
            lock_wait: Duration::from_secs(15),
            ready_wait: Duration::from_secs(10),
            supported: true,
        };
        let fx = Fx::at(home.home.clone(), context, runtime_root).await;
        fx.shared
            .restart
            .reconcile_done
            .store(true, Ordering::Release);
        // The admitting server: answered once, now gone.
        fx.grant(ServerWitness {
            socket: fx.socket(),
            pid: 1,
            start: StartIdentity::Darwin {
                seconds: 1,
                micros: 0,
            },
        });
        Self {
            fx,
            home,
            _root: root,
        }
    }

    /// A server started more than the policy margin after its config was written.
    fn serve(&mut self) -> u32 {
        std::thread::sleep(Duration::from_millis(1_200));
        let socket = self.home.socket();
        self.home.serve(&[], &["server"], &socket)
    }

    fn stop(&self) {
        let _ = self
            .home
            .command(&[], &["server".into(), "stop".into()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.home.herdr_pids().is_empty() {
            assert!(Instant::now() < deadline, "the server did not stop");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    async fn recover(&self) -> Result<(), String> {
        recover(
            &self.fx.shared,
            &ProviderKind::Claude,
            ChannelId::new(CHANNEL),
        )
        .await
    }

    async fn state(&self) -> HostedState {
        match self.fx.row().await.map(|row| row.record) {
            Some(HostedRecord::Known(record)) => record.state,
            other => panic!("{other:?}"),
        }
    }

    /// The execution's launch context, made at `created`.
    fn context(&self, created: SystemTime) {
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: created.into(),
            execution_nonce: NONCE.into(),
            tmux_session: LOGICAL.into(),
            channel_id: Some(CHANNEL),
            owner_runtime_root: self.fx.runtime_root.clone(),
            host: crate::services::tui_prompt_dedupe::binding_context::stable_host_identity(),
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
        };
        PreparedIncarnation::create(context).unwrap();
    }
}

/// The recorded processes' pid runs now (this test), but they started long before this boot.
fn before_boot(edit: &mut HostedExecution) {
    let expected = edit.expected.as_mut().unwrap();
    let pid = std::process::id();
    expected.root = ProcessStamp {
        pid,
        start: "darwin:1001.000000".into(),
    };
    expected.provider_process = ProcessStamp {
        pid,
        start: "darwin:1002.000000".into(),
    };
}

fn boot() -> SystemTime {
    match crate::services::session_host::boot_time() {
        Some(StartIdentity::Darwin { seconds, .. }) => UNIX_EPOCH + Duration::from_secs(seconds),
        other => panic!("boot time {other:?}"),
    }
}

/// Without startup proof for the server answering now the owner retires nothing; once a server
/// with that proof replaced the granted one, the Bound row whose pane it lacks is retired.
#[tokio::test(flavor = "current_thread")]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_the_owner_retires_a_bound_row_only_with_startup_proof() {
    let mut real = Real::new().await;
    real.fx.insert(before_boot).await;
    crate::services::claude::herdr_turn::hold(NONCE).unwrap();
    real.serve();
    // A config written after the server started is its next start's, not this one's.
    std::fs::write(real.home.home.join(".config/herdr/config.toml"), FALSE).unwrap();
    let unproved = real.recover().await;
    assert!(
        unproved
            .as_ref()
            .is_err_and(|e| e.starts_with("no replaced server with startup proof")),
        "{unproved:?}"
    );
    assert_eq!(real.state().await, HostedState::Bound);

    real.stop();
    std::thread::sleep(Duration::from_millis(1_200));
    assert_eq!(real.recover().await, Ok(()), "started and proved by ensure");
    assert_eq!(real.state().await, HostedState::Retired);
    real.fx.close().await;
}

/// A Pending placed before this boot, on a server that replaced the granted one and lacks its
/// pane, is retired by the owner.
#[tokio::test(flavor = "current_thread")]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_the_owner_retires_a_placed_pending_from_before_the_boot() {
    let mut real = Real::new().await;
    real.fx
        .insert(|record| {
            record.state = HostedState::Pending;
            record.expected = None;
        })
        .await;
    real.context(boot() - Duration::from_secs(3_600));
    real.serve();
    assert_eq!(real.recover().await, Ok(()));
    assert_eq!(real.state().await, HostedState::Retired);
    real.fx.close().await;
}

/// A Pending that never recorded a pane is resolved by the existing unplaced resolver: held
/// while its launch is recent, retired once quiet with no pane of its channel on the server.
#[tokio::test(flavor = "current_thread")]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_the_owner_resolves_an_unplaced_pending() {
    let mut real = Real::new().await;
    real.fx
        .insert(|record| {
            record.state = HostedState::Pending;
            record.location = None;
            record.expected = None;
        })
        .await;
    real.context(SystemTime::now() - Duration::from_secs(600));
    real.serve();
    let recent = real.recover().await;
    assert!(
        recent
            .as_ref()
            .is_err_and(|e| e.contains("launch too recent")),
        "{recent:?}"
    );
    assert_eq!(real.state().await, HostedState::Pending);
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert_eq!(real.recover().await, Ok(()));
    assert_eq!(real.state().await, HostedState::Retired);
    real.fx.close().await;
}

/// A lone shell now in the recorded pane, whose recorded processes' pids run again only after a
/// reboot, reads as ended on the replacing server and not on the granted one.
#[tokio::test(flavor = "current_thread")]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_a_pane_reads_as_ended_across_a_reboot_only_on_a_replacing_server() {
    let mut real = Real::new().await;
    real.serve();
    let created = real
        .home
        .command(
            &[],
            &["workspace".into(), "create".into(), "--no-focus".into()],
        )
        .status()
        .unwrap();
    assert!(created.success());
    std::thread::sleep(Duration::from_millis(500));
    real.fx
        .insert(|record| {
            before_boot(record);
            record.location.as_mut().unwrap().pane_id = "w1:p1".into();
        })
        .await;
    let HostedRecord::Known(record) = real.fx.row().await.unwrap().record else {
        panic!("unknown row");
    };
    let end = || async {
        let target = herdr_endpoints().target(&record).unwrap();
        execution_end(target, record.clone()).await
    };
    assert_eq!(end().await, ExecutionEnd::Ended, "replacing server");
    let current = real.fx.current();
    real.fx.grant(current);
    assert_eq!(end().await, ExecutionEnd::Unconfirmed, "granted server");
    real.fx.close().await;
}
