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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum StatusPanelSingletonLoadOutcome {
    Present(StatusPanelSingletonBinding),
    Missing,
    DurabilityFailure(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum CompletedBindingCommitOutcome {
    CommittedCurrent(StatusPanelSingletonBinding),
    Superseded,
    DurabilityFailure(String),
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
) -> StatusPanelSingletonLoadOutcome {
    let path = channel_file_path_in_root(root, provider, token_hash, channel_id);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return StatusPanelSingletonLoadOutcome::Missing;
        }
        Err(error) => {
            return StatusPanelSingletonLoadOutcome::DurabilityFailure(error.to_string());
        }
    };
    match serde_json::from_str::<StatusPanelSingletonBinding>(&raw) {
        Ok(binding) if binding.panel_message_id != 0 => {
            StatusPanelSingletonLoadOutcome::Present(binding)
        }
        Ok(_) => StatusPanelSingletonLoadOutcome::DurabilityFailure(
            "status panel singleton contains a zero message id".to_string(),
        ),
        Err(error) => StatusPanelSingletonLoadOutcome::DurabilityFailure(error.to_string()),
    }
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
    bind_if_owned_guarded(
        provider,
        token_hash,
        channel_id,
        panel_message_id,
        generation,
        None,
        None,
    )
}

pub(in crate::services::discord) fn bind_if_owned_guarded(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_message_id: u64,
    generation: Option<u64>,
    identity: Option<&inflight::InflightTurnIdentity>,
    expected_generation: Option<u64>,
) -> Result<StatusPanelSingletonBinding, String> {
    let inflight_root = runtime_store::discord_inflight_root()
        .ok_or_else(|| "AgentDesk inflight runtime root unavailable".to_string())?;
    let path = inflight::inflight_state_path(&inflight_root, provider, channel_id);
    let _guard = inflight::lock_inflight_state_path(&path)?;
    let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let mut state = serde_json::from_str::<inflight::InflightTurnState>(&raw)
        .map_err(|error| error.to_string())?;
    if state.status_message_id != Some(panel_message_id)
        || identity.is_some_and(|identity| !identity.matches_state(&state))
        || expected_generation.is_some_and(|generation| generation != state.status_panel_generation)
    {
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

pub(in crate::services::discord) fn commit_confirmed_missing_replacement(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    expected_user_msg_id: u64,
    missing_panel_message_id: u64,
    expected_generation: u64,
    replacement_panel_message_id: u64,
) -> CompletedBindingCommitOutcome {
    let Some(inflight_root) = runtime_store::discord_inflight_root() else {
        return CompletedBindingCommitOutcome::DurabilityFailure(
            "AgentDesk inflight runtime root unavailable".to_string(),
        );
    };
    let Some(singleton_root) = runtime_store::discord_status_panel_singletons_root() else {
        return CompletedBindingCommitOutcome::DurabilityFailure(
            "AgentDesk runtime root unavailable".to_string(),
        );
    };
    let path = inflight::inflight_state_path(&inflight_root, provider, channel_id);
    let _inflight_guard = match inflight::lock_inflight_state_path(&path) {
        Ok(guard) => guard,
        Err(error) => return CompletedBindingCommitOutcome::DurabilityFailure(error),
    };
    let _singleton_guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    let mut inflight_state = match fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<inflight::InflightTurnState>(&raw) {
            Ok(state) => Some(state),
            Err(error) => {
                return CompletedBindingCommitOutcome::DurabilityFailure(error.to_string());
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return CompletedBindingCommitOutcome::DurabilityFailure(error.to_string()),
    };
    let inflight_owns_missing = inflight_state.as_ref().is_some_and(|state| {
        state.user_msg_id == expected_user_msg_id
            && state.status_message_id == Some(missing_panel_message_id)
            && state.status_panel_generation == expected_generation
    });
    if inflight_state.is_some() && !inflight_owns_missing {
        return CompletedBindingCommitOutcome::Superseded;
    }

    match load_in_root(&singleton_root, provider, token_hash, channel_id) {
        StatusPanelSingletonLoadOutcome::Present(binding)
            if binding.panel_message_id == replacement_panel_message_id =>
        {
            return CompletedBindingCommitOutcome::CommittedCurrent(binding);
        }
        StatusPanelSingletonLoadOutcome::Present(binding)
            if binding.panel_message_id == missing_panel_message_id
                && binding.generation == expected_generation => {}
        StatusPanelSingletonLoadOutcome::Missing if inflight_owns_missing => {}
        StatusPanelSingletonLoadOutcome::Present(_) | StatusPanelSingletonLoadOutcome::Missing => {
            return CompletedBindingCommitOutcome::Superseded;
        }
        StatusPanelSingletonLoadOutcome::DurabilityFailure(error) => {
            return CompletedBindingCommitOutcome::DurabilityFailure(error);
        }
    }

    if let Some(state) = inflight_state.as_mut() {
        state.status_message_id = Some(replacement_panel_message_id);
        let json = match serde_json::to_string_pretty(state) {
            Ok(json) => json,
            Err(error) => {
                return CompletedBindingCommitOutcome::DurabilityFailure(error.to_string());
            }
        };
        if let Err(error) = runtime_store::atomic_write(&path, &json) {
            return CompletedBindingCommitOutcome::DurabilityFailure(error);
        }
    }
    let binding = StatusPanelSingletonBinding {
        panel_message_id: replacement_panel_message_id,
        generation: expected_generation,
    };
    let singleton_path =
        channel_file_path_in_root(&singleton_root, provider, token_hash, channel_id);
    let json = match serde_json::to_string_pretty(&binding) {
        Ok(json) => json,
        Err(error) => return CompletedBindingCommitOutcome::DurabilityFailure(error.to_string()),
    };
    match runtime_store::atomic_write(&singleton_path, &json) {
        Ok(()) => CompletedBindingCommitOutcome::CommittedCurrent(binding),
        Err(error) => CompletedBindingCommitOutcome::DurabilityFailure(error),
    }
}

pub(in crate::services::discord) fn commit_if_owned_or_current(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_message_id: u64,
) -> CompletedBindingCommitOutcome {
    let Some(inflight_root) = runtime_store::discord_inflight_root() else {
        return CompletedBindingCommitOutcome::DurabilityFailure(
            "AgentDesk inflight runtime root unavailable".to_string(),
        );
    };
    let path = inflight::inflight_state_path(&inflight_root, provider, channel_id);
    let _guard = match inflight::lock_inflight_state_path(&path) {
        Ok(guard) => guard,
        Err(error) => return CompletedBindingCommitOutcome::DurabilityFailure(error),
    };
    let Some(root) = runtime_store::discord_status_panel_singletons_root() else {
        return CompletedBindingCommitOutcome::DurabilityFailure(
            "AgentDesk runtime root unavailable".to_string(),
        );
    };

    match fs::read_to_string(&path) {
        Ok(raw) => {
            let state = match serde_json::from_str::<inflight::InflightTurnState>(&raw) {
                Ok(state) => state,
                Err(error) => {
                    return CompletedBindingCommitOutcome::DurabilityFailure(error.to_string());
                }
            };
            if state.status_message_id != Some(panel_message_id) {
                // #4891: the live inflight row already moved on (next turn, or a
                // mid-turn re-anchor), but the completed panel can still be the
                // channel's current durable singleton. Check the durable binding
                // under the same inflight flock before classifying it superseded.
                return current_singleton_outcome_in_root(
                    &root,
                    provider,
                    token_hash,
                    channel_id,
                    panel_message_id,
                    false,
                );
            }
            let binding = StatusPanelSingletonBinding {
                panel_message_id,
                generation: state.status_panel_generation,
            };
            match bind_in_root(&root, provider, token_hash, channel_id, binding) {
                Ok(()) => CompletedBindingCommitOutcome::CommittedCurrent(binding),
                Err(error) => CompletedBindingCommitOutcome::DurabilityFailure(error),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            current_singleton_outcome_in_root(
                &root,
                provider,
                token_hash,
                channel_id,
                panel_message_id,
                true,
            )
        }
        Err(error) => CompletedBindingCommitOutcome::DurabilityFailure(error.to_string()),
    }
}

/// Classify `panel_message_id` against the channel's durable singleton while the
/// caller holds the inflight flock. A matching binding already carries the
/// completed panel's durable authority, so no value-identical rewrite is needed.
fn current_singleton_outcome_in_root(
    root: &Path,
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
    panel_message_id: u64,
    missing_inflight: bool,
) -> CompletedBindingCommitOutcome {
    match load_in_root(root, provider, token_hash, channel_id) {
        StatusPanelSingletonLoadOutcome::Present(binding)
            if binding.panel_message_id == panel_message_id =>
        {
            CompletedBindingCommitOutcome::CommittedCurrent(binding)
        }
        StatusPanelSingletonLoadOutcome::Present(_) => CompletedBindingCommitOutcome::Superseded,
        StatusPanelSingletonLoadOutcome::Missing if missing_inflight => {
            CompletedBindingCommitOutcome::Superseded
        }
        StatusPanelSingletonLoadOutcome::Missing => {
            CompletedBindingCommitOutcome::DurabilityFailure(
                "completed status panel singleton binding unavailable".to_string(),
            )
        }
        StatusPanelSingletonLoadOutcome::DurabilityFailure(error) => {
            CompletedBindingCommitOutcome::DurabilityFailure(error)
        }
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
    let StatusPanelSingletonLoadOutcome::Present(binding) =
        load_in_root(root, provider, token_hash, channel_id)
    else {
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
    match load_typed(provider, token_hash, channel_id) {
        StatusPanelSingletonLoadOutcome::Present(binding) => Some(binding),
        StatusPanelSingletonLoadOutcome::Missing
        | StatusPanelSingletonLoadOutcome::DurabilityFailure(_) => None,
    }
}

pub(in crate::services::discord) fn load_typed(
    provider: &ProviderKind,
    token_hash: &str,
    channel_id: u64,
) -> StatusPanelSingletonLoadOutcome {
    let Some(root) = runtime_store::discord_status_panel_singletons_root() else {
        return StatusPanelSingletonLoadOutcome::DurabilityFailure(
            "AgentDesk runtime root unavailable".to_string(),
        );
    };
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
    fn completion_with_new_inflight_owner_recommits_still_current_singleton_4891() {
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
        let completed_panel = 810;
        let next_panel = 811;

        let completed_owner = test_state(channel_id, 30, completed_panel, 8);
        inflight::save_inflight_state(&completed_owner).expect("persist completed owner");
        bind_if_owned(&provider, token_hash, channel_id, completed_panel, None)
            .expect("bind completed owner");
        let next_owner = test_state(channel_id, 40, next_panel, 9);
        inflight::save_inflight_state(&next_owner).expect("persist next inflight owner");

        assert_eq!(
            commit_if_owned_or_current(&provider, token_hash, channel_id, completed_panel),
            CompletedBindingCommitOutcome::CommittedCurrent(StatusPanelSingletonBinding {
                panel_message_id: completed_panel,
                generation: 8,
            }),
            "a moved inflight row must not reject a panel that is still the durable singleton"
        );
        assert_eq!(
            load(&provider, token_hash, channel_id),
            Some(StatusPanelSingletonBinding {
                panel_message_id: completed_panel,
                generation: 8,
            }),
            "the fallback must preserve the current binding and generation"
        );
    }

    #[test]
    fn completion_with_new_inflight_and_new_singleton_is_superseded_4891() {
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
        let channel_id = 48_912;
        let completed_panel = 820;
        let next_panel = 821;

        let next_owner = test_state(channel_id, 40, next_panel, 9);
        inflight::save_inflight_state(&next_owner).expect("persist next inflight owner");
        bind_if_owned(&provider, token_hash, channel_id, next_panel, None)
            .expect("bind next singleton owner");

        assert_eq!(
            commit_if_owned_or_current(&provider, token_hash, channel_id, completed_panel),
            CompletedBindingCommitOutcome::Superseded
        );
        assert_eq!(
            load(&provider, token_hash, channel_id),
            Some(StatusPanelSingletonBinding {
                panel_message_id: next_panel,
                generation: 9,
            })
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
            CompletedBindingCommitOutcome::CommittedCurrent(StatusPanelSingletonBinding {
                panel_message_id: current_panel,
                generation: 8,
            })
        );
        assert_eq!(
            commit_if_owned_or_current(&provider, token_hash, channel_id, 802),
            CompletedBindingCommitOutcome::Superseded,
            "an absent inflight row must not authorize replacing the current singleton"
        );
    }

    #[test]
    fn completion_without_inflight_or_singleton_is_reclaimable_superseded_4891() {
        let _env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            runtime_root.path(),
        );

        assert_eq!(
            commit_if_owned_or_current(&ProviderKind::Claude, "test-token", 48_913, 803),
            CompletedBindingCommitOutcome::Superseded,
            "a completed fallback with no owner must retain orphan retirement authority"
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
            StatusPanelSingletonLoadOutcome::Present(StatusPanelSingletonBinding {
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
            StatusPanelSingletonLoadOutcome::Missing
        );
    }

    #[test]
    fn typed_load_distinguishes_missing_malformed_zero_and_read_failure_4891() {
        let root = tempfile::tempdir().expect("singleton root");
        let provider = ProviderKind::Claude;
        let token_hash = "test-token";

        assert_eq!(
            load_in_root(root.path(), &provider, token_hash, 48_620),
            StatusPanelSingletonLoadOutcome::Missing
        );

        let malformed = channel_file_path_in_root(root.path(), &provider, token_hash, 48_621);
        fs::create_dir_all(malformed.parent().expect("singleton parent")).expect("create parent");
        fs::write(&malformed, "{malformed").expect("write malformed singleton");
        assert!(matches!(
            load_in_root(root.path(), &provider, token_hash, 48_621),
            StatusPanelSingletonLoadOutcome::DurabilityFailure(_)
        ));

        let zero = channel_file_path_in_root(root.path(), &provider, token_hash, 48_622);
        fs::write(&zero, r#"{"panel_message_id":0,"generation":1}"#).expect("write zero singleton");
        assert!(matches!(
            load_in_root(root.path(), &provider, token_hash, 48_622),
            StatusPanelSingletonLoadOutcome::DurabilityFailure(_)
        ));

        let unreadable = channel_file_path_in_root(root.path(), &provider, token_hash, 48_623);
        fs::create_dir_all(&unreadable).expect("create directory at singleton file path");
        assert!(matches!(
            load_in_root(root.path(), &provider, token_hash, 48_623),
            StatusPanelSingletonLoadOutcome::DurabilityFailure(_)
        ));
    }
}
