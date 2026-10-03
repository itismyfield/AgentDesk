use super::*;
use crate::services::discord::{inflight, mailbox_snapshot};

struct EnvGuard(Option<std::ffi::OsString>);

impl EnvGuard {
    fn set_root(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
        Self(previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.0.as_ref() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(future)
}

async fn ensure(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    turn_start_offset: u64,
) -> Option<inflight::InflightTurnIdentity> {
    ensure_monitor_auto_turn_inflight(
        shared,
        provider,
        channel_id,
        "AgentDesk-claude-5191",
        "/tmp/agentdesk-5191.jsonl",
        "/tmp/agentdesk-5191.fifo",
        Some("session-5191"),
        turn_start_offset,
        512,
    )
    .await
}

#[test]
fn n1a_monitor_producers_leave_no_row_or_lease_when_confirmed() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_root(root.path());
    block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        for (i, provider) in [ProviderKind::Claude, ProviderKind::Codex]
            .into_iter()
            .enumerate()
        {
            let channel = ChannelId::new(63250211 + i as u64);
            let confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(channel.get());
            let receipt = ensure(&shared, &provider, channel, 128).await;
            assert!(
                inflight::load_inflight_state(&provider, channel.get()).is_none(),
                "confirmed monitor producer must leave row absent"
            );
            assert!(receipt.is_none());
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let start =
                start_monitor_auto_turn_when_available(&shared, &provider, channel, 128, &cancel)
                    .await;
            assert!(
                mailbox_snapshot(&shared, channel)
                    .await
                    .cancel_token
                    .is_none()
            );
            assert!(!start.acquired);
            drop(confirmed);
            assert!(ensure(&shared, &provider, channel, 128).await.is_some());
            let start =
                start_monitor_auto_turn_when_available(&shared, &provider, channel, 128, &cancel)
                    .await;
            assert!(start.acquired);
        }
    });
}
