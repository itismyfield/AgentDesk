//! Channel role projections and memory settings independent of Discord assembly.
pub(crate) mod config_projection;
mod memory_policy;

use crate::domain::memory::{MemoryBackendKind, MemoryConfigOverride, ResolvedMemorySettings};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;

pub(crate) struct RoleProjection {
    pub role_id: String,
    pub prompt_file: String,
    pub provider: Option<ProviderKind>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub peer_agents_enabled: bool,
    pub auth_profile: String,
    pub memory: ResolvedMemorySettings,
}

fn org_schema_path() -> Option<std::path::PathBuf> {
    crate::config::runtime_root().map(|root| crate::runtime_layout::org_schema_path(&root))
}

fn load_org_document() -> Option<crate::services::org_catalog::OrgSchema> {
    crate::services::org_catalog::load_runtime_document_at(org_schema_path().as_deref())
}

pub(crate) fn resolve_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleProjection> {
    if let Some(binding) = config_projection::config_role_binding(channel_id, channel_name) {
        return Some(binding);
    }
    if org_schema_path().is_some_and(|path| path.exists()) {
        if let Some(binding) = config_projection::org_role_binding(channel_id, channel_name) {
            return Some(binding);
        }
    }
    None
}

pub(crate) fn resolve_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    if let Some(workspace) = config_projection::config_workspace(channel_id, channel_name) {
        return Some(workspace);
    }
    if org_schema_path().is_some_and(|path| path.exists()) {
        if let Some(workspace) = config_projection::org_workspace(channel_id, channel_name) {
            return Some(workspace);
        }
    }
    None
}

pub(crate) fn resolve_memory_settings(
    base: Option<&MemoryConfigOverride>,
    override_cfg: Option<&MemoryConfigOverride>,
) -> ResolvedMemorySettings {
    let merged = memory_policy::merge_memory_config(base, override_cfg);
    let configured = crate::config::runtime_root()
        .map(|root| crate::runtime_layout::load_memory_backend(&root).backend);
    let requested =
        memory_policy::requested_backend(merged.backend.as_deref(), configured.as_deref());
    let active = matches!(requested, "auto" | "memento")
        && crate::services::memory::backend_is_active(MemoryBackendKind::Memento);
    // A failed explicit request has always made a second, fresh state observation.
    let unavailable_state = if requested == "memento" && !active {
        crate::services::memory::backend_state(MemoryBackendKind::Memento)
            .map(|state| (state.configured, state.consecutive_failures))
    } else {
        None
    };
    memory_policy::resolve_memory_settings(
        &merged,
        requested,
        memory_policy::BackendAvailability {
            active,
            unavailable_state,
        },
    )
}

pub(crate) fn resolve_agent_memory_settings(
    agent: Option<&crate::config::AgentDef>,
) -> ResolvedMemorySettings {
    ResolvedMemorySettings {
        memory_group: agent.and_then(|agent| agent.memory_group.clone()),
        ..resolve_memory_settings(None, None)
    }
}

pub(crate) fn memory_settings_for_role(role_id: &str) -> ResolvedMemorySettings {
    let config = crate::config::runtime_config_optional();
    let agent = config
        .as_ref()
        .and_then(|config| config.agents.iter().find(|agent| agent.id == role_id));
    resolve_agent_memory_settings(agent)
}

pub(crate) fn memory_settings_for_binding(
    memory: Option<&ResolvedMemorySettings>,
) -> ResolvedMemorySettings {
    memory
        .cloned()
        .unwrap_or_else(|| resolve_memory_settings(None, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::snapshot_reader_tests::{Fixture, isolated};

    #[test]
    fn config_snapshot_precedes_org_then_reload_exposes_org_overrides() {
        isolated(|| {
            let fixture = Fixture::new();
            fixture.install(
                "agents:\n  - id: yaml-agent\n    name: YAML\n    provider: codex\n    channels:\n      codex: {id: '9041', workspace: '~/r04-yaml', reasoning_effort: xhigh}\n",
                "agents: []\n",
            );
            let org = crate::runtime_layout::org_schema_path(fixture.root());
            std::fs::write(
                &org,
                r#"
version: 1
prompts_root: ~/r04-prompts
agents:
  org-agent:
    display_name: Org
    provider: claude
    model: agent-model
    workspace: ~/r04-agent
    peer_agents: true
    memory: {backend: file, recall_timeout_ms: 1100, capture_timeout_ms: 9000}
channels:
  by_id:
    '9041':
      agent: org-agent
      provider: gemini
      model: channel-model
      workspace: ~/r04-channel
      peer_agents: false
      memory: {recall_timeout_ms: 1200}
  by_name:
    enabled: true
    mappings:
      org-alias: {agent: org-agent}
"#,
            )
            .unwrap();
            let channel = ChannelId::new(9041);
            let home = dirs::home_dir().unwrap();
            let binding = resolve_role_binding(channel, None).unwrap();
            assert_eq!(binding.role_id, "yaml-agent");
            assert_eq!(binding.provider, Some(ProviderKind::Codex));
            assert_eq!(binding.reasoning_effort.as_deref(), Some("xhigh"));
            assert_eq!(
                resolve_workspace(channel, None).unwrap(),
                home.join("r04-yaml").display().to_string()
            );
            fixture.reload();
            let binding = resolve_role_binding(channel, Some("org-alias")).unwrap();
            assert_eq!(binding.role_id, "org-agent");
            assert_eq!(binding.provider, Some(ProviderKind::Gemini));
            assert_eq!(binding.model.as_deref(), Some("channel-model"));
            assert!(!binding.peer_agents_enabled);
            assert_eq!(
                binding.prompt_file,
                format!(
                    "{}/agents/org-agent/IDENTITY.md",
                    home.join("r04-prompts").display()
                )
            );
            assert_eq!(binding.memory.recall_timeout_ms, 1200);
            assert_eq!(binding.memory.capture_timeout_ms, 9000);
            assert_eq!(
                resolve_workspace(channel, None).unwrap(),
                home.join("r04-channel").display().to_string()
            );
            let named = resolve_role_binding(ChannelId::new(9042), Some("org-alias")).unwrap();
            assert_eq!(named.provider, Some(ProviderKind::Claude));
            assert_eq!(named.model.as_deref(), Some("agent-model"));
            assert!(named.peer_agents_enabled);
            assert_eq!(
                resolve_workspace(ChannelId::new(9042), Some("org-alias")).unwrap(),
                home.join("r04-agent").display().to_string()
            );
            std::fs::write(&org, "version: [").unwrap();
            assert!(resolve_role_binding(channel, None).is_none());
            assert!(resolve_workspace(channel, None).is_none());
        });
    }

    #[test]
    fn config_id_beats_alias_and_workspace_and_prompt_defaults_are_preserved() {
        isolated(|| {
            let fixture = Fixture::new();
            let yaml = r#"
agents:
  - id: alias-agent
    name: Alias
    provider: claude
    channels:
      claude: {name: shared, workspace: '~\r04-alias'}
  - id: id-agent
    name: Explicit
    provider: codex
    channels:
      codex: {id: '9051', name: shared, workspace: '~/r04-explicit', prompt_file: '~/r04-explicit.md'}
  - id: legacy-agent
    name: Legacy
    provider: claude
    channels:
      claude: legacy-name
"#;
            fixture.install(yaml, yaml);
            let home = dirs::home_dir().unwrap();
            assert_eq!(
                resolve_role_binding(ChannelId::new(9051), Some("shared"))
                    .unwrap()
                    .role_id,
                "id-agent"
            );
            assert_eq!(
                resolve_workspace(ChannelId::new(9051), Some("shared")).unwrap(),
                home.join("r04-explicit").display().to_string()
            );
            assert_eq!(
                resolve_role_binding(ChannelId::new(9051), None)
                    .unwrap()
                    .prompt_file,
                home.join("r04-explicit.md").display().to_string()
            );
            assert_eq!(
                resolve_workspace(ChannelId::new(9052), Some(" shared ")).unwrap(),
                home.join("r04-alias").display().to_string()
            );
            assert!(resolve_role_binding(ChannelId::new(9052), Some("unknown")).is_none());
            let legacy = ChannelId::new(9053);
            assert_eq!(
                resolve_workspace(legacy, Some("legacy-name")).unwrap(),
                fixture
                    .root()
                    .join("workspaces/legacy-agent")
                    .display()
                    .to_string()
            );
            let agents = crate::runtime_layout::managed_agents_root(fixture.root());
            let canonical = agents.join("legacy-agent/IDENTITY.md");
            assert_eq!(
                resolve_role_binding(legacy, Some("legacy-name"))
                    .unwrap()
                    .prompt_file,
                canonical.display().to_string()
            );
            std::fs::create_dir_all(&agents).unwrap();
            let flat = agents.join("legacy-agent.prompt.md");
            std::fs::write(&flat, "legacy identity").unwrap();
            assert_eq!(
                resolve_role_binding(legacy, Some("legacy-name"))
                    .unwrap()
                    .prompt_file,
                flat.display().to_string()
            );
            std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
            std::fs::write(&canonical, "canonical identity").unwrap();
            assert_eq!(
                resolve_role_binding(legacy, Some("legacy-name"))
                    .unwrap()
                    .prompt_file,
                canonical.display().to_string()
            );
        });
    }

    #[test]
    fn memory_backend_availability_is_observed_again_for_each_resolution() {
        isolated(|| {
            let runtime = crate::config::TestRuntimeRootGuard::new();
            let path = crate::runtime_layout::memory_backend_path(runtime.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::json!({
                "version": 2, "backend": "memento",
                "mcp": {"endpoint": "http://127.0.0.1:9040", "access_key_env": "R04_TEST_MEMENTO_KEY"}
            }).to_string()).unwrap();
            let key = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "R04_TEST_MEMENTO_KEY",
                std::path::Path::new("fixture-key"),
            );
            let bound = resolve_memory_settings(None, None);
            assert_eq!(bound.backend, MemoryBackendKind::Memento);
            assert!(!bound.memento_fallback);
            {
                let _missing = crate::config::TestEnvVarGuard::remove_after_shared_test_env_lock(
                    "R04_TEST_MEMENTO_KEY",
                );
                let fallback = resolve_memory_settings(None, None);
                assert_eq!(fallback.backend, MemoryBackendKind::File);
                assert!(fallback.memento_fallback);
                let auto = resolve_memory_settings(
                    None,
                    Some(&MemoryConfigOverride {
                        backend: Some("auto".into()),
                        ..Default::default()
                    }),
                );
                assert_eq!(auto.backend, MemoryBackendKind::File);
                assert!(!auto.memento_fallback);
                assert_eq!(memory_settings_for_binding(Some(&bound)), bound);
            }
            assert_eq!(
                resolve_memory_settings(None, None).backend,
                MemoryBackendKind::Memento
            );
            let clamped = resolve_memory_settings(
                None,
                Some(&MemoryConfigOverride {
                    backend: Some("file".into()),
                    recall_timeout_ms: Some(0),
                    capture_timeout_ms: Some(u64::MAX),
                }),
            );
            assert_eq!(
                (clamped.recall_timeout_ms, clamped.capture_timeout_ms),
                (100, 30000)
            );
            drop(key);
        });
    }
}
