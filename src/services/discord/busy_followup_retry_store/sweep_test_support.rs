use std::path::Path;

use super::{BusyFollowupRetryState, ProviderKind};

pub(in crate::services::discord) fn legacy(
    notice_message_id: u64,
    count: u32,
    first_ms: u64,
) -> BusyFollowupRetryState {
    let json = serde_json::json!({
        "notice_message_id": notice_message_id,
        "busy_retry_count": count,
        "first_busy_retry_at_ms": first_ms,
    });
    serde_json::from_value(json).expect("legacy record")
}

pub(in crate::services::discord) fn with_root(test: impl FnOnce()) {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().expect("runtime root");
    let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    test();
}

pub(in crate::services::discord) fn save_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    state: &BusyFollowupRetryState,
) -> Result<(), String> {
    super::save_in_root(root, provider, channel_id, user_msg_id, state)
}

pub(in crate::services::discord) fn sweep_expired_in_root_at(root: &Path, now_ms: u64) -> usize {
    super::sweep_expired_in_root_at(root, now_ms)
}

pub(in crate::services::discord) fn load_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> Option<BusyFollowupRetryState> {
    super::load_in_root(root, provider, channel_id, user_msg_id)
}
