//! Durable per-channel binding for the two-message singleton status panel.
//!
//! A completed panel outlives its inflight row. This store carries only the
//! current panel message id and generation across that boundary so the next turn
//! can re-anchor the same logical panel below its answer without accumulating
//! completed cards in the channel.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::services::discord::{inflight, runtime_store};
use crate::services::provider::ProviderKind;

static STORE_WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct StatusPanelSingletonBinding {
    pub panel_message_id: u64,
    pub generation: u64,
}

fn provider_dir_in_root(root: &Path, provider: &ProviderKind, token_hash: &str) -> PathBuf {
    root.join(provider.as_str()).join(token_hash)
}

fn channel_file_path_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> PathBuf {
    provider_dir_in_root(root, provider, token_hash).join(format!("{channel_id}.json"))
}

fn load_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Option<StatusPanelSingletonBinding> {
    let raw = fs::read_to_string(channel_file_path_in_root(
        root, provider, token_hash, channel_id,
    ))
    .ok()?;
    let binding = serde_json::from_str::<StatusPanelSingletonBinding>(&raw).ok()?;
    (binding.panel_message_id != 0).then_some(binding)
}

fn bind_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    binding: StatusPanelSingletonBinding,
) -> Result<(), String> {
    if channel_id == 0 || binding.panel_message_id == 0 {
        return Err("status panel singleton ids must be non-zero".to_string());
    }
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let path = channel_file_path_in_root(root, provider, token_hash, channel_id);
    let json = serde_json::to_string_pretty(&binding).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(&path, &json)
}

pub(in crate::services::discord) fn bind_if_owned(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_message_id: u64,
    generation: Option<u64>,
) -> Result<StatusPanelSingletonBinding, String> {
    let inflight_root = runtime_store::discord_inflight_root()
        .ok_or_else(|| "AgentDesk inflight runtime root unavailable".to_string())?;
    let path = inflight::inflight_state_path(&inflight_root, provider, channel_id);
    let _guard = inflight::lock_inflight_state_path(&path)?;
    let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let mut state = serde_json::from_str::<inflight::InflightTurnState>(&raw)
        .map_err(|error| error.to_string())?;
    if state.status_message_id != Some(panel_message_id) {
        return Err("status panel singleton ownership changed".to_string());
    }
    if let Some(generation) = generation
        && generation > state.status_panel_generation
    {
        state.status_panel_generation = generation;
        let json = serde_json::to_string_pretty(&state).map_err(|error| error.to_string())?;
        runtime_store::atomic_write(&path, &json)?;
    }
    let binding = StatusPanelSingletonBinding {
        panel_message_id,
        generation: state.status_panel_generation,
    };
    let root = runtime_store::discord_status_panel_singletons_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;
    bind_in_root(&root, provider, token_hash, channel_id, binding)?;
    Ok(binding)
}

pub(in crate::services::discord) fn commit_if_owned_or_current(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_message_id: u64,
) -> Result<StatusPanelSingletonBinding, String> {
    let inflight_root = runtime_store::discord_inflight_root()
        .ok_or_else(|| "AgentDesk inflight runtime root unavailable".to_string())?;
    let path = inflight::inflight_state_path(&inflight_root, provider, channel_id);
    let _guard = inflight::lock_inflight_state_path(&path)?;
    let root = runtime_store::discord_status_panel_singletons_root()
        .ok_or_else(|| "AgentDesk runtime root unavailable".to_string())?;

    match fs::read_to_string(&path) {
        Ok(raw) => {
            let state = serde_json::from_str::<inflight::InflightTurnState>(&raw)
                .map_err(|error| error.to_string())?;
            if state.status_message_id != Some(panel_message_id) {
                return Err("status panel singleton ownership changed".to_string());
            }
            let binding = StatusPanelSingletonBinding {
                panel_message_id,
                generation: state.status_panel_generation,
            };
            bind_in_root(&root, provider, token_hash, channel_id, binding)?;
            Ok(binding)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let binding = load_in_root(&root, provider, token_hash, channel_id)
                .filter(|binding| binding.panel_message_id == panel_message_id)
                .ok_or_else(|| "completed status panel is not the current singleton".to_string())?;
            bind_in_root(&root, provider, token_hash, channel_id, binding)?;
            Ok(binding)
        }
        Err(error) => Err(error.to_string()),
    }
}

fn clear_if_current_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_message_id: u64,
) -> bool {
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(binding) = load_in_root(root, provider, token_hash, channel_id) else {
        return false;
    };
    if binding.panel_message_id != panel_message_id {
        return false;
    }
    fs::remove_file(channel_file_path_in_root(
        root, provider, token_hash, channel_id,
    ))
    .is_ok()
}

pub(in crate::services::discord) fn load(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> Option<StatusPanelSingletonBinding> {
    let root = runtime_store::discord_status_panel_singletons_root()?;
    load_in_root(&root, provider, token_hash, channel_id)
}

pub(in crate::services::discord) fn clear_if_current(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_message_id: u64,
) -> bool {
    let Some(root) = runtime_store::discord_status_panel_singletons_root() else {
        return false;
    };
    clear_if_current_in_root(
        root.as_path(),
        provider,
        token_hash,
        channel_id,
        panel_message_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(
        channel_id: u64,
        user_msg_id: u64,
        panel_message_id: u64,
        generation: u64,
    ) -> inflight::InflightTurnState {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            None,
            1,
            user_msg_id,
            user_msg_id + 1,
            "singleton ownership test".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        state.status_message_id = Some(panel_message_id);
        state.status_panel_generation = generation;
        state
    }

    #[test]
    fn stale_owner_after_flock_release_cannot_overwrite_new_singleton_4860() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token_hash = "test-token";
        let channel_id = 48_601;
        let stale_panel = 700;
        let current_panel = 701;

        let stale_owner = test_state(channel_id, 10, stale_panel, 4);
        inflight::save_inflight_state(&stale_owner).expect("persist stale owner");
        let inflight_root = runtime_store::discord_inflight_root().expect("inflight root");
        let path = inflight::inflight_state_path(&inflight_root, &provider, channel_id);
        {
            let _lock = inflight::lock_inflight_state_path(&path).expect("stale owner check flock");
            let raw = fs::read_to_string(&path).expect("read stale owner");
            let checked = serde_json::from_str::<inflight::InflightTurnState>(&raw)
                .expect("parse stale owner");
            assert_eq!(checked.status_message_id, Some(stale_panel));
        }

        let current_owner = test_state(channel_id, 20, current_panel, 5);
        inflight::save_inflight_state(&current_owner).expect("persist replacement owner");
        bind_if_owned(&provider, token_hash, channel_id, current_panel, None)
            .expect("bind current owner");

        assert!(
            bind_if_owned(&provider, token_hash, channel_id, stale_panel, Some(4),).is_err(),
            "a stale owner that resumes after releasing the flock must fail closed"
        );
        assert_eq!(
            load(&provider, token_hash, channel_id),
            Some(StatusPanelSingletonBinding {
                panel_message_id: current_panel,
                generation: 5,
            }),
            "the replacement owner's singleton must remain authoritative"
        );
    }

    #[test]
    fn completion_without_inflight_only_recommits_current_singleton_4860() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );
        let provider = ProviderKind::Claude;
        let token_hash = "test-token";
        let channel_id = 48_602;
        let current_panel = 801;
        let owner = test_state(channel_id, 30, current_panel, 8);
        inflight::save_inflight_state(&owner).expect("persist owner");
        bind_if_owned(&provider, token_hash, channel_id, current_panel, None).expect("bind owner");
        let inflight_root = runtime_store::discord_inflight_root().expect("inflight root");
        fs::remove_file(inflight::inflight_state_path(
            &inflight_root,
            &provider,
            channel_id,
        ))
        .expect("remove completed inflight row");

        assert_eq!(
            commit_if_owned_or_current(&provider, token_hash, channel_id, current_panel),
            Ok(StatusPanelSingletonBinding {
                panel_message_id: current_panel,
                generation: 8,
            })
        );
        assert!(
            commit_if_owned_or_current(&provider, token_hash, channel_id, 802).is_err(),
            "an absent inflight row must not authorize replacing the current singleton"
        );
    }

    #[test]
    fn durable_binding_survives_reload_and_guarded_clear_4860() {
        let root = tempfile::tempdir().expect("singleton root");
        let provider = ProviderKind::Claude;
        let token_hash = "test-token";
        let channel_id = 48_600;

        bind_in_root(
            root.path(),
            &provider,
            token_hash,
            channel_id,
            StatusPanelSingletonBinding {
                panel_message_id: 700,
                generation: 4,
            },
        )
        .expect("persist singleton binding");

        assert_eq!(
            load_in_root(root.path(), &provider, token_hash, channel_id),
            Some(StatusPanelSingletonBinding {
                panel_message_id: 700,
                generation: 4,
            }),
            "restart-style reload must recover the exact singleton binding"
        );
        assert!(
            !clear_if_current_in_root(root.path(), &provider, token_hash, channel_id, 701),
            "a stale panel id must not clear the current binding"
        );
        assert!(clear_if_current_in_root(
            root.path(),
            &provider,
            token_hash,
            channel_id,
            700
        ));
        assert_eq!(
            load_in_root(root.path(), &provider, token_hash, channel_id),
            None
        );
    }
}
