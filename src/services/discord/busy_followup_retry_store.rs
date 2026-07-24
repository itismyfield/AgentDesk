//! Durable per-input busy-notice binding and aggregate retry budget.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

pub(in crate::services::discord) const MAX_BUSY_RETRY_COUNT: u32 = 6;
pub(in crate::services::discord) const MAX_BUSY_RETRY_ELAPSED: Duration =
    Duration::from_secs(5 * 60);

static STORE_WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct BusyFollowupRetryState {
    pub notice_message_id: u64,
    pub busy_retry_count: u32,
    pub first_busy_retry_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct BusyRetryDecision {
    pub state: BusyFollowupRetryState,
    pub capped: bool,
}

fn input_file_path_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> PathBuf {
    root.join(provider.as_str())
        .join(channel_id.to_string())
        .join(format!("{user_msg_id}.json"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn load_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> Option<BusyFollowupRetryState> {
    let raw = fs::read_to_string(input_file_path_in_root(
        root,
        provider,
        channel_id,
        user_msg_id,
    ))
    .ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    state: BusyFollowupRetryState,
) -> Result<(), String> {
    if channel_id == 0 || user_msg_id == 0 || state.notice_message_id == 0 {
        return Err("busy follow-up retry ids must be non-zero".to_string());
    }
    let json = serde_json::to_string_pretty(&state).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(
        &input_file_path_in_root(root, provider, channel_id, user_msg_id),
        &json,
    )
}

pub(in crate::services::discord) fn load(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> Option<BusyFollowupRetryState> {
    let root = runtime_store::discord_busy_followup_retries_root()?;
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    load_in_root(&root, provider, channel_id, user_msg_id)
}

pub(in crate::services::discord) fn is_capped(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> bool {
    let Some(state) = load(provider, channel_id, user_msg_id) else {
        return false;
    };
    let elapsed_ms = now_ms().saturating_sub(state.first_busy_retry_at_ms);
    state.busy_retry_count >= MAX_BUSY_RETRY_COUNT
        || (state.first_busy_retry_at_ms != 0
            && elapsed_ms >= MAX_BUSY_RETRY_ELAPSED.as_millis() as u64)
}

/// Bind the first posted placeholder. A stale attempt cannot replace an existing
/// input binding; callers edit the returned current message instead.
pub(in crate::services::discord) fn bind_notice_if_absent(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> Result<BusyFollowupRetryState, String> {
    let root = runtime_store::discord_busy_followup_retries_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(current) = load_in_root(&root, provider, channel_id, user_msg_id) {
        return Ok(current);
    }
    let state = BusyFollowupRetryState {
        notice_message_id,
        busy_retry_count: 0,
        first_busy_retry_at_ms: 0,
    };
    save_in_root(&root, provider, channel_id, user_msg_id, state)?;
    Ok(state)
}

pub(in crate::services::discord) fn record_busy_retry(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> Result<BusyRetryDecision, String> {
    record_busy_retry_at(
        provider,
        channel_id,
        user_msg_id,
        notice_message_id,
        now_ms(),
    )
}

fn record_busy_retry_at(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
    now_ms: u64,
) -> Result<BusyRetryDecision, String> {
    let root = runtime_store::discord_busy_followup_retries_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut state =
        load_in_root(&root, provider, channel_id, user_msg_id).unwrap_or(BusyFollowupRetryState {
            notice_message_id,
            busy_retry_count: 0,
            first_busy_retry_at_ms: now_ms,
        });
    if state.notice_message_id == 0 {
        state.notice_message_id = notice_message_id;
    }
    if state.first_busy_retry_at_ms == 0 {
        state.first_busy_retry_at_ms = now_ms;
    }
    state.busy_retry_count = state.busy_retry_count.saturating_add(1);
    let elapsed_ms = now_ms.saturating_sub(state.first_busy_retry_at_ms);
    let max_elapsed_ms = MAX_BUSY_RETRY_ELAPSED.as_millis() as u64;
    let capped = state.busy_retry_count >= MAX_BUSY_RETRY_COUNT || elapsed_ms >= max_elapsed_ms;
    save_in_root(&root, provider, channel_id, user_msg_id, state)?;
    Ok(BusyRetryDecision { state, capped })
}

pub(in crate::services::discord) fn clear_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> bool {
    let Some(state) = load(provider, channel_id, user_msg_id) else {
        return false;
    };
    clear_if_current(provider, channel_id, user_msg_id, state.notice_message_id)
}

pub(in crate::services::discord) fn clear_if_current(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    notice_message_id: u64,
) -> bool {
    let Some(root) = runtime_store::discord_busy_followup_retries_root() else {
        return false;
    };
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(current) = load_in_root(&root, provider, channel_id, user_msg_id) else {
        return false;
    };
    if current.notice_message_id != notice_message_id {
        return false;
    }
    fs::remove_file(input_file_path_in_root(
        &root,
        provider,
        channel_id,
        user_msg_id,
    ))
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_root(test: impl FnOnce()) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        test();
    }

    #[test]
    fn repeated_busy_attempts_keep_one_notice_binding_and_cap_4888() {
        with_root(|| {
            let provider = ProviderKind::Claude;
            let channel_id = 48_880;
            let user_msg_id = 48_881;
            assert_eq!(
                bind_notice_if_absent(&provider, channel_id, user_msg_id, 700)
                    .expect("first bind")
                    .notice_message_id,
                700
            );
            assert_eq!(
                bind_notice_if_absent(&provider, channel_id, user_msg_id, 701)
                    .expect("second bind")
                    .notice_message_id,
                700,
                "retry must edit the existing card instead of binding a new POST"
            );
            for count in 1..=MAX_BUSY_RETRY_COUNT {
                let decision = record_busy_retry_at(
                    &provider,
                    channel_id,
                    user_msg_id,
                    701,
                    1_000 + u64::from(count),
                )
                .expect("record retry");
                assert_eq!(decision.state.notice_message_id, 700);
                assert_eq!(decision.capped, count == MAX_BUSY_RETRY_COUNT);
            }
            let persisted = load(&provider, channel_id, user_msg_id).expect("persisted state");
            assert_eq!(persisted.busy_retry_count, MAX_BUSY_RETRY_COUNT);
        });
    }

    #[test]
    fn elapsed_cap_and_identity_guarded_clear_preserve_current_binding_4888() {
        with_root(|| {
            let provider = ProviderKind::Claude;
            let channel_id = 48_882;
            let user_msg_id = 48_883;
            bind_notice_if_absent(&provider, channel_id, user_msg_id, 800).expect("bind");
            record_busy_retry_at(&provider, channel_id, user_msg_id, 800, 1_000)
                .expect("first retry");
            let decision = record_busy_retry_at(
                &provider,
                channel_id,
                user_msg_id,
                800,
                1_000 + MAX_BUSY_RETRY_ELAPSED.as_millis() as u64,
            )
            .expect("elapsed retry");
            assert!(decision.capped);
            assert!(!clear_if_current(&provider, channel_id, user_msg_id, 801));
            assert!(load(&provider, channel_id, user_msg_id).is_some());
            assert!(clear_if_current(&provider, channel_id, user_msg_id, 800));
            assert!(load(&provider, channel_id, user_msg_id).is_none());
        });
    }
}
