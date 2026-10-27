//! Routine metadata and session identity helpers for headless turns.
use super::*;

pub(super) fn valid_routine_metadata(
    metadata: Option<&serde_json::Value>,
) -> Option<&serde_json::Value> {
    let metadata = metadata?;
    metadata
        .get("routine_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(metadata)
}
pub(super) fn routine_metadata_agent_id(metadata: Option<&serde_json::Value>) -> Option<&str> {
    valid_routine_metadata(metadata)?
        .get("agent_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
/// #4658: the isolated session-key basis for a scheduled-snapshot turn. When
/// present, the turn (a) derives its ADK session key from this label instead of
/// the channel name and (b) severs provider/transcript continuity so the frozen
/// snapshot is the only conversation context. Absent for every other caller.
pub(super) fn scheduled_snapshot_session_label(
    metadata: Option<&serde_json::Value>,
) -> Option<String> {
    metadata?
        .get("scheduled_snapshot_session_label")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
/// Select the session-key basis for isolated headless turns. Fresh routine
/// turns already receive a synthetic per-routine tmux label; using that same
/// label for the persisted session key prevents an old agent-channel row from
/// owning the new routine's heartbeat and model metadata.
pub(super) fn session_key_basis_override<'a>(
    scheduled_snapshot_label: Option<&'a str>,
    metadata: Option<&serde_json::Value>,
    tmux_session_label: Option<&'a str>,
) -> Option<&'a str> {
    scheduled_snapshot_label.or_else(|| {
        fresh_routine_turn(metadata)
            .then_some(tmux_session_label)
            .flatten()
    })
}
/// Whether an explicit routine turn must sever provider and transcript continuity.
/// Only `persistent` routines retain continuity; absent strategy preserves the
/// legacy routine default of `fresh`. Non-routine metadata must never reset a
/// provider session.
pub(super) fn fresh_routine_turn(metadata: Option<&serde_json::Value>) -> bool {
    let Some(metadata) = valid_routine_metadata(metadata) else {
        return false;
    };
    metadata
        .get("execution_strategy")
        .and_then(|value| value.as_str())
        != Some("persistent")
}
/// Process mode is explicit and only honored on fresh routine metadata.
pub(super) fn routine_process_turn(metadata: Option<&serde_json::Value>) -> bool {
    fresh_routine_turn(metadata)
        && valid_routine_metadata(metadata)
            .and_then(|value| value.get("execution_mode"))
            .and_then(serde_json::Value::as_str)
            == Some("process")
}

pub(super) fn routine_tmux_session_name(
    provider: &ProviderKind,
    metadata: Option<&serde_json::Value>,
    label: Option<&str>,
    channel_name: Option<&str>,
) -> Option<String> {
    (provider.uses_managed_tmux_backend() && !routine_process_turn(metadata))
        .then(|| {
            label
                .or(channel_name)
                .map(|name| provider.build_tmux_session_name(name))
        })
        .flatten()
}

pub(super) fn routine_metadata_role_binding(
    metadata: Option<&serde_json::Value>,
    provider: &ProviderKind,
) -> Option<settings::RoleBinding> {
    let metadata = valid_routine_metadata(metadata)?;
    let agent_id = routine_metadata_agent_id(Some(metadata))?;
    // Resolve the agent's configured prompt path instead of hardcoding
    // IDENTITY.md under config/agents: `default_prompt_path` reads the managed
    // agents root and falls back to the legacy `<id>.prompt.md` layout, so
    // agents on either layout still get their role prompt for routine turns
    // (#3463). Falls back to the canonical IDENTITY.md path when unset.
    let prompt_file = crate::services::discord::agentdesk_config::default_prompt_path(agent_id)
        .unwrap_or_default();

    Some(settings::RoleBinding {
        role_id: agent_id.to_string(),
        prompt_file,
        provider: Some(provider.clone()),
        auth_profile: "default".to_string(),
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: true,
        memory: settings::memory_settings_for_role(agent_id),
    })
}

pub(super) async fn persist_boundary_before_provider_clear<B, BFut, C, CFut, E>(
    persist_boundary: bool,
    clear_provider: bool,
    boundary: B,
    clear: C,
) -> Result<(), E>
where
    B: FnOnce() -> BFut,
    BFut: std::future::Future<Output = Result<(), E>>,
    C: FnOnce() -> CFut,
    CFut: std::future::Future<Output = ()>,
{
    if persist_boundary {
        boundary().await?;
    }
    if clear_provider {
        clear().await;
    }
    Ok(())
}

#[cfg(test)]
mod memory_scope_tests {
    use super::*;
    use crate::services::discord::prompt_builder::{DispatchProfile, build_system_prompt};
    use crate::services::discord::settings::MemoryBackendKind;

    fn routine_binding_and_hint(group: Option<&str>) -> (settings::RoleBinding, String) {
        let root = tempfile::tempdir().unwrap();
        let _guard = crate::config::set_agentdesk_root_for_test(root.path());
        let mut config = crate::config::Config::default();
        let mut agent: crate::config::AgentDef = serde_yaml::from_str(
            "id: ch-td\nname: Test\nchannels:\n  claude:\n    id: '4242'\n    workspace: /fixture/ch-td\n",
        )
        .unwrap();
        agent.memory_group = group.map(str::to_owned);
        config.agents.push(agent);
        let path = crate::runtime_layout::config_file_path(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_yaml::to_string(&config).unwrap()).unwrap();
        let metadata = serde_json::json!({"routine_id": "routine-1", "agent_id": " ch-td "});
        let mut binding =
            routine_metadata_role_binding(Some(&metadata), &ProviderKind::Claude).unwrap();
        binding.memory.backend = MemoryBackendKind::Memento;
        let prompt = build_system_prompt(
            "ctx",
            &[],
            "/fixture/ch-td",
            ChannelId::new(4242),
            "tok",
            Some(&binding),
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
            Some(&binding.memory),
            true,
            false,
        );
        let hint = prompt
            .lines()
            .find(|line| line.starts_with("- scope hints:"))
            .unwrap()
            .to_owned();
        (binding, hint)
    }

    #[test]
    fn routine_memory_group_reaches_binding_and_three_tier_hint() {
        let (binding, hint) = routine_binding_and_hint(Some("cookingheart"));
        assert_eq!(binding.memory.memory_group.as_deref(), Some("cookingheart"));
        assert_eq!(
            hint,
            "- scope hints: read=`workspace=cookingheart, agentId=ch-td`; default write (private)=`workspace=cookingheart, agentId=ch-td`; group shared=`workspace=cookingheart, agentId=default`; global=`agentId=default` (omit workspace)."
        );
        assert!(!hint.contains("agent-private="));
    }

    #[test]
    fn routine_memory_group_absent_hint_preserves_main_bytes() {
        let (binding, hint) = routine_binding_and_hint(None);
        assert_eq!(binding.memory.memory_group, None);
        assert_eq!(
            hint,
            "- scope hints: project=`workspace=ch-td, agentId=default`; agent-private=`workspace=agentdesk-ch-td, agentId=ch-td`."
        );
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fresh_process_turn_suppresses_tmux_but_keeps_isolated_session_key() {
        let metadata =
            json!({"routine_id":"r-1", "execution_strategy":"fresh", "execution_mode":"process"});
        for provider in [ProviderKind::Claude, ProviderKind::Codex] {
            assert_eq!(
                routine_tmux_session_name(
                    &provider,
                    Some(&metadata),
                    Some("routine unique attempt"),
                    Some("real channel")
                ),
                None
            );
            assert_eq!(
                session_key_basis_override(None, Some(&metadata), Some("routine unique attempt")),
                Some("routine unique attempt")
            );
            let kind =
                super::super::super::provider_isolation::prelaunch_runtime_kind_for_managed_session(
                    &provider, true, false, None,
                );
            assert_eq!(
                kind, None,
                "process mode must not register a managed tmux runtime"
            );
            assert_eq!(
                super::super::super::provider_isolation::prelaunch_inflight_runtime_seed_without_codex_raw(&provider, true, None, kind),
                (None, None, None, 0),
                "a process attempt must not seed wrapper files or a tmux watcher target",
            );
        }
    }

    #[test]
    fn persistent_and_non_routine_metadata_keep_tmux_route() {
        for metadata in [
            json!({"routine_id":"r-1", "execution_strategy":"persistent", "execution_mode":"process"}),
            json!({"execution_strategy":"fresh", "execution_mode":"process"}),
            json!({"routine_id":"r-1", "execution_strategy":"fresh"}),
        ] {
            assert_eq!(
                routine_tmux_session_name(
                    &ProviderKind::Claude,
                    Some(&metadata),
                    Some("routine label"),
                    Some("channel")
                ),
                Some(ProviderKind::Claude.build_tmux_session_name("routine label"))
            );
        }
    }
}
