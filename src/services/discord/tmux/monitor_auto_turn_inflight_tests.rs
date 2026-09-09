// #5191 A1 precursor P1 — receipt contract for `ensure_monitor_auto_turn_inflight`.
// Locks the three arms a caller can observe: create-new hands back the identity
// of the row it just wrote, while BOTH non-creating arms (a row that was already
// there, and a persist failure) hand back `None`. The pre-existing arm also
// proves the helper stays a no-op on disk — a receipt must not become a write.
//
// These are sync `#[test]`s driving a private current-thread runtime rather than
// `#[tokio::test]`s: the shared test-env guard must span the whole case to
// serialize `AGENTDESK_ROOT_DIR`, and keeping it outside the async body means no
// `MutexGuard` is ever held across an await point (crate-wide `await_holding_lock`
// deny), so no suppression and no ratchet movement is needed.
use super::*;

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

fn state_path(provider: &ProviderKind, channel_id: ChannelId) -> std::path::PathBuf {
    inflight::inflight_runtime_root()
        .expect("inflight runtime root")
        .join(provider.as_str())
        .join(format!("{}.json", channel_id.get()))
}

#[test]
fn create_new_returns_the_identity_of_the_row_it_wrote() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env = EnvGuard::set_root(root.path());

    block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(5_191_101);

        let receipt = ensure(&shared, &provider, channel_id, 128)
            .await
            .expect("create-new must hand back a receipt");

        let persisted = inflight::load_inflight_state(&provider, channel_id.get())
            .expect("monitor inflight must persist");
        assert!(
            receipt.matches_state(&persisted),
            "receipt must name the row this call created"
        );
        assert_eq!(receipt.turn_start_offset, Some(128));
    });
}

#[test]
fn pre_existing_row_returns_none_and_is_left_untouched() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env = EnvGuard::set_root(root.path());

    block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(5_191_102);
        assert!(
            ensure(&shared, &provider, channel_id, 128).await.is_some(),
            "first call is the create-new path"
        );
        let path = state_path(&provider, channel_id);
        let before = std::fs::read(&path).expect("row on disk after create");

        let second = ensure(&shared, &provider, channel_id, 4_096).await;

        assert!(second.is_none(), "a row already there is not our create");
        assert_eq!(
            std::fs::read(&path).expect("row still on disk"),
            before,
            "the no-op arm must not rewrite the row"
        );
    });
}

#[test]
fn persist_failure_returns_none_and_writes_nothing() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env = EnvGuard::set_root(root.path());
    // Occupy the inflight root with a regular file so `create_dir_all` of the
    // provider directory underneath it fails => `CreateNewInflightError::Internal`.
    let inflight_root = inflight::inflight_runtime_root().expect("inflight runtime root");
    std::fs::create_dir_all(inflight_root.parent().expect("runtime dir")).expect("runtime dir");
    std::fs::write(&inflight_root, b"not a directory").expect("block the inflight root");

    block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(5_191_103);

        let receipt = ensure(&shared, &provider, channel_id, 128).await;

        assert!(
            receipt.is_none(),
            "a failed persist is not a create receipt"
        );
        assert!(
            inflight::load_inflight_state(&provider, channel_id.get()).is_none(),
            "nothing may be readable after a failed persist"
        );
    });
}
