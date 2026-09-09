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

    let binding = match fs::read_to_string(&path) {
        Ok(raw) => {
            let state = serde_json::from_str::<inflight::InflightTurnState>(&raw)
                .map_err(|error| error.to_string())?;
            if state.status_message_id == Some(panel_message_id) {
                StatusPanelSingletonBinding {
                    panel_message_id,
                    generation: state.status_panel_generation,
                }
            } else {
                // #4891: an inflight row that no longer names this panel is
                // not proof of supersession — the NEXT turn opens its row (and
                // points it at its own new panel) before the previous turn
                // commits. Ask the `NotFound` arm's question instead of failing
                // closed; a truly superseded panel is rejected there because the
                // newer owner's `bind_if_owned` already moved the binding.
                current_durable_singleton(
                    &root,
                    provider,
                    token_hash,
                    channel_id,
                    panel_message_id,
                )?
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            current_durable_singleton(&root, provider, token_hash, channel_id, panel_message_id)?
        }
        Err(error) => return Err(error.to_string()),
    };
    bind_in_root(&root, provider, token_hash, channel_id, binding)?;
    Ok(binding)
}

/// #4891: the shared "is this completed panel still the channel's durable
/// singleton?" check, used by every `commit_if_owned_or_current` arm that cannot
/// read the panel's generation off a matching inflight row.
fn current_durable_singleton(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_message_id: u64,
) -> Result<StatusPanelSingletonBinding, String> {
    load_in_root(root, provider, token_hash, channel_id)
        .filter(|binding| binding.panel_message_id == panel_message_id)
        .ok_or_else(|| "completed status panel is not the current singleton".to_string())
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

    /// #4891: the NEXT turn's inflight row already points at a DIFFERENT panel
    /// while the durable singleton still names THIS completed panel. The commit
    /// must fall back to the durable singleton exactly like the `NotFound` arm;
    /// failing closed here is what promoted a ledger miss into a completion
    /// failure and got the live panel deleted as an orphan.
    #[test]
    fn commit_falls_back_to_durable_singleton_when_inflight_row_moved_on_4891() {
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
        let channel_id = 48_910;
        let completed_panel = 1_530_266_420_234_031_306;
        let next_turn_panel = 1_530_266_449_355_210_913;

        let owner = test_state(channel_id, 40, completed_panel, 11);
        inflight::save_inflight_state(&owner).expect("persist completing owner");
        bind_if_owned(&provider, token_hash, channel_id, completed_panel, None)
            .expect("bind completing owner");

        // The next turn opens its own row on the same channel and points it at a
        // brand-new panel BEFORE the previous turn's completion commits. It has
        // not adopted the durable singleton yet.
        let next_turn = test_state(channel_id, 41, next_turn_panel, 12);
        inflight::save_inflight_state(&next_turn).expect("persist next turn row");

        assert_eq!(
            commit_if_owned_or_current(&provider, token_hash, channel_id, completed_panel),
            Ok(StatusPanelSingletonBinding {
                panel_message_id: completed_panel,
                generation: 11,
            }),
            "a completed panel that is still the durable singleton must commit even though the inflight row moved on"
        );
    }

    /// #4891 counterpart: the fallback must stay ownership-scoped. Once the
    /// newer turn has actually adopted the singleton, the superseded panel is
    /// genuinely stale and must still fail closed.
    #[test]
    fn commit_still_fails_closed_for_a_truly_superseded_panel_4891() {
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
        let channel_id = 48_911;
        let superseded_panel = 910;
        let current_panel = 911;

        let old_owner = test_state(channel_id, 50, superseded_panel, 3);
        inflight::save_inflight_state(&old_owner).expect("persist old owner");
        bind_if_owned(&provider, token_hash, channel_id, superseded_panel, None)
            .expect("bind old owner");

        let new_owner = test_state(channel_id, 51, current_panel, 4);
        inflight::save_inflight_state(&new_owner).expect("persist new owner");
        bind_if_owned(&provider, token_hash, channel_id, current_panel, None)
            .expect("bind new owner");

        assert!(
            commit_if_owned_or_current(&provider, token_hash, channel_id, superseded_panel)
                .is_err(),
            "a panel the newer turn already replaced in the durable singleton must still fail closed"
        );
        assert_eq!(
            load(&provider, token_hash, channel_id).map(|b| b.panel_message_id),
            Some(current_panel),
            "the superseded commit must not overwrite the current singleton"
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
