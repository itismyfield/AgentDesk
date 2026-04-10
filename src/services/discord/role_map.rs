use std::collections::BTreeMap;
use std::fs;

use poise::serenity_prelude::ChannelId;

use super::meeting::{
    MeetingAgentConfig, MeetingConfig, SummaryAgentConfig, SummaryAgentRule,
    derive_agent_metadata_quality,
};
use super::runtime_store::role_map_path;
use super::settings::{
    MemoryConfigOverride, PeerAgentInfo, RegisteredChannelBinding, RoleBinding,
    resolve_memory_settings,
};
use crate::services::provider::ProviderKind;

/// Expand `~` or `~/` prefix to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if path == "~" {
            return home.display().to_string();
        }
        if path.starts_with("~/") {
            return format!("{}{}", home.display(), &path[1..]);
        }
    }
    path.to_string()
}

fn load_role_map_json() -> Option<serde_json::Value> {
    let path = role_map_path()?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn parse_role_binding(value: &serde_json::Value) -> Option<RoleBinding> {
    let obj = value.as_object()?;
    let role_id = obj.get("roleId")?.as_str()?.to_string();
    let prompt_file = expand_tilde(obj.get("promptFile")?.as_str()?);
    let provider = obj
        .get("provider")
        .and_then(|v| v.as_str())
        .and_then(ProviderKind::from_str);
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let reasoning_effort = obj
        .get("reasoningEffort")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let peer_agents_enabled = obj
        .get("peerAgents")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let memory_override = obj.get("memory").and_then(|raw| {
        serde_json::from_value::<MemoryConfigOverride>(raw.clone())
            .map_err(|err| {
                eprintln!("  [memory] Warning: invalid role_map memory block: {err}");
                err
            })
            .ok()
    });
    Some(RoleBinding {
        role_id,
        prompt_file,
        provider,
        model,
        reasoning_effort,
        peer_agents_enabled,
        memory: resolve_memory_settings(None, memory_override.as_ref()),
    })
}

fn parse_meeting_role_binding(value: &serde_json::Value) -> Option<RoleBinding> {
    let obj = value.as_object()?;
    let role_id = obj
        .get("role_id")
        .or_else(|| obj.get("roleId"))?
        .as_str()?
        .to_string();
    let prompt_file = obj
        .get("prompt_file")
        .or_else(|| obj.get("promptFile"))
        .and_then(|v| v.as_str())
        .map(expand_tilde)?;
    let provider = obj
        .get("provider")
        .and_then(|v| v.as_str())
        .and_then(ProviderKind::from_str);
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let reasoning_effort = obj
        .get("reasoning_effort")
        .or_else(|| obj.get("reasoningEffort"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let peer_agents_enabled = obj
        .get("peer_agents")
        .or_else(|| obj.get("peerAgents"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let memory_override = obj.get("memory").and_then(|raw| {
        serde_json::from_value::<MemoryConfigOverride>(raw.clone())
            .map_err(|err| {
                eprintln!(
                    "  [memory] Warning: invalid meeting.available_agents memory block: {err}"
                );
                err
            })
            .ok()
    });
    Some(RoleBinding {
        role_id,
        prompt_file,
        provider,
        model,
        reasoning_effort,
        peer_agents_enabled,
        memory: resolve_memory_settings(None, memory_override.as_ref()),
    })
}

fn parse_meeting_agent_metadata(value: &serde_json::Value) -> Option<ParsedMeetingAgentMetadata> {
    let obj = value.as_object()?;
    let role_id = obj.get("role_id")?.as_str()?.to_string();
    let display_name = obj.get("display_name")?.as_str()?.to_string();
    let keywords = obj
        .get("keywords")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let domain_summary = obj
        .get("domain_summary")
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());
    let strengths = obj
        .get("strengths")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let task_types = obj
        .get("task_types")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let anti_signals = obj
        .get("anti_signals")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let provider_hint = obj
        .get("provider_hint")
        .and_then(|v| v.as_str())
        .map(|value| value.to_string());
    let (metadata_missing, metadata_confidence) = derive_agent_metadata_quality(
        domain_summary.as_deref(),
        &strengths,
        &task_types,
        &anti_signals,
    );
    Some(ParsedMeetingAgentMetadata {
        role_id,
        display_name,
        keywords,
        domain_summary,
        strengths,
        task_types,
        anti_signals,
        provider_hint,
        metadata_missing,
        metadata_confidence,
    })
}

fn collect_registry_entry(
    registry: &mut BTreeMap<String, MeetingAgentConfig>,
    value: &serde_json::Value,
    metadata: &BTreeMap<String, ParsedMeetingAgentMetadata>,
) {
    let Some(binding) = parse_role_binding(value) else {
        return;
    };

    let role_id = binding.role_id.clone();
    let workspace = value
        .get("workspace")
        .and_then(|v| v.as_str())
        .map(expand_tilde);

    let metadata = metadata
        .get(&role_id)
        .cloned()
        .unwrap_or_else(|| fallback_meeting_agent_metadata(&role_id));

    registry
        .entry(role_id.clone())
        .or_insert_with(|| meeting_agent_config(role_id, binding, metadata, workspace));
}

fn fallback_meeting_agent_metadata(role_id: &str) -> ParsedMeetingAgentMetadata {
    ParsedMeetingAgentMetadata {
        role_id: role_id.to_string(),
        display_name: role_id.to_string(),
        keywords: Vec::new(),
        domain_summary: None,
        strengths: Vec::new(),
        task_types: Vec::new(),
        anti_signals: Vec::new(),
        provider_hint: None,
        metadata_missing: true,
        metadata_confidence: "low".to_string(),
    }
}

fn meeting_agent_config(
    role_id: String,
    binding: RoleBinding,
    metadata: ParsedMeetingAgentMetadata,
    workspace: Option<String>,
) -> MeetingAgentConfig {
    MeetingAgentConfig {
        role_id,
        display_name: metadata.display_name,
        keywords: metadata.keywords,
        domain_summary: metadata.domain_summary,
        strengths: metadata.strengths,
        task_types: metadata.task_types,
        anti_signals: metadata.anti_signals,
        provider_hint: metadata.provider_hint,
        metadata_missing: metadata.metadata_missing,
        metadata_confidence: metadata.metadata_confidence,
        binding,
        workspace,
    }
}

fn meeting_available_agent_config(
    agent: &serde_json::Value,
    binding: RoleBinding,
    metadata: &BTreeMap<String, ParsedMeetingAgentMetadata>,
) -> MeetingAgentConfig {
    let role_id = binding.role_id.clone();
    let workspace = agent
        .get("workspace")
        .and_then(|v| v.as_str())
        .map(expand_tilde);
    let metadata = metadata
        .get(&role_id)
        .cloned()
        .unwrap_or_else(|| fallback_meeting_agent_metadata(&role_id));

    meeting_agent_config(role_id, binding, metadata, workspace)
}

#[derive(Clone, Debug)]
struct ParsedMeetingAgentMetadata {
    role_id: String,
    display_name: String,
    keywords: Vec<String>,
    domain_summary: Option<String>,
    strengths: Vec<String>,
    task_types: Vec<String>,
    anti_signals: Vec<String>,
    provider_hint: Option<String>,
    metadata_missing: bool,
    metadata_confidence: String,
}

fn fallback_enabled(json: &serde_json::Value) -> bool {
    json.get("fallbackByChannelName")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub(super) fn resolve_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleBinding> {
    let json = load_role_map_json()?;

    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        let key = channel_id.get().to_string();
        if let Some(binding) = by_id.get(&key).and_then(parse_role_binding) {
            return Some(binding);
        }
    }

    if !fallback_enabled(&json) {
        return None;
    }

    let cname = channel_name?;
    let by_name = json.get("byChannelName").and_then(|v| v.as_object())?;
    by_name.get(cname).and_then(parse_role_binding)
}

pub(super) fn resolve_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    let json = load_role_map_json()?;

    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        let key = channel_id.get().to_string();
        if let Some(entry) = by_id.get(&key) {
            if let Some(ws) = entry.get("workspace").and_then(|v| v.as_str()) {
                return Some(expand_tilde(ws));
            }
        }
    }

    if !fallback_enabled(&json) {
        return None;
    }

    let cname = channel_name?;
    let by_name = json.get("byChannelName").and_then(|v| v.as_object())?;
    by_name
        .get(cname)
        .and_then(|entry| entry.get("workspace"))
        .and_then(|v| v.as_str())
        .map(|s| expand_tilde(s))
}

pub(super) fn load_shared_prompt_path() -> Option<String> {
    let json = load_role_map_json()?;
    json.get("sharedPromptFile")
        .and_then(|v| v.as_str())
        .map(|s| expand_tilde(s))
}

/// Check if a role_id exists in any channel binding in role_map.json.
pub(super) fn is_known_agent(role_id: &str) -> bool {
    let Some(json) = load_role_map_json() else {
        return false;
    };
    // Check byChannelId
    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        for entry in by_id.values() {
            if entry.get("roleId").and_then(|v| v.as_str()) == Some(role_id) {
                return true;
            }
        }
    }
    // Check byChannelName
    if let Some(by_name) = json.get("byChannelName").and_then(|v| v.as_object()) {
        for entry in by_name.values() {
            if entry.get("roleId").and_then(|v| v.as_str()) == Some(role_id) {
                return true;
            }
        }
    }
    false
}

pub(super) fn load_peer_agents() -> Vec<PeerAgentInfo> {
    let Some(json) = load_role_map_json() else {
        return Vec::new();
    };
    let Some(agents) = json
        .get("meeting")
        .and_then(|meeting| meeting.get("available_agents"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for agent in agents {
        let Some(obj) = agent.as_object() else {
            continue;
        };
        let Some(role_id) = obj.get("role_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !seen.insert(role_id.to_string()) {
            continue;
        }
        let Some(display_name) = obj.get("display_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let keywords = obj
            .get("keywords")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        result.push(PeerAgentInfo {
            role_id: role_id.to_string(),
            display_name: display_name.to_string(),
            keywords,
        });
    }

    result
}

fn parse_summary_agent_config(value: &serde_json::Value) -> Option<SummaryAgentConfig> {
    // Backward-compatible: accept plain string
    if let Some(s) = value.as_str() {
        return Some(SummaryAgentConfig::Static(s.to_string()));
    }

    // Rule-based: { "rules": [...], "default": "..." }
    let obj = value.as_object()?;
    let default_agent = obj.get("default")?.as_str()?.to_string();
    let rules_arr = obj.get("rules").and_then(|v| v.as_array());
    let mut rules = Vec::new();
    if let Some(arr) = rules_arr {
        for rule in arr {
            let agent = rule.get("agent").and_then(|v| v.as_str());
            let keywords = rule.get("keywords").and_then(|v| v.as_array()).map(|kws| {
                kws.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            });
            if let (Some(agent), Some(keywords)) = (agent, keywords) {
                rules.push(SummaryAgentRule {
                    keywords,
                    agent: agent.to_string(),
                });
            }
        }
    }
    Some(SummaryAgentConfig::Dynamic {
        rules,
        default: default_agent,
    })
}

pub(super) fn load_meeting_config() -> Option<MeetingConfig> {
    let json = load_role_map_json()?;
    let meeting = json.get("meeting")?;

    let channel_name = meeting.get("channel_name")?.as_str()?.to_string();
    let max_rounds = meeting
        .get("max_rounds")
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as u32;
    let summary_agent = parse_summary_agent_config(meeting.get("summary_agent")?)?;

    let explicit_agents = meeting
        .get("available_agents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut metadata = BTreeMap::new();
    for agent in &explicit_agents {
        if let Some(parsed) = parse_meeting_agent_metadata(agent) {
            metadata.insert(parsed.role_id.clone(), parsed);
        }
    }

    let mut registry = BTreeMap::new();
    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        for entry in by_id.values() {
            collect_registry_entry(&mut registry, entry, &metadata);
        }
    }
    if let Some(by_name) = json.get("byChannelName").and_then(|v| v.as_object()) {
        for (key, entry) in by_name {
            if key == "enabled" {
                continue;
            }
            collect_registry_entry(&mut registry, entry, &metadata);
        }
    }
    for agent in &explicit_agents {
        if let Some(binding) = parse_meeting_role_binding(agent) {
            let role_id = binding.role_id.clone();
            registry.insert(
                role_id,
                meeting_available_agent_config(agent, binding, &metadata),
            );
        }
    }

    let agent_registry: Vec<MeetingAgentConfig> = registry.into_values().collect();
    let available_agents = if explicit_agents.is_empty() {
        agent_registry.clone()
    } else {
        explicit_agents
            .iter()
            .filter_map(|agent| {
                let role_id = agent.get("role_id").and_then(|v| v.as_str())?;
                agent_registry
                    .iter()
                    .find(|candidate| candidate.role_id == role_id)
                    .cloned()
            })
            .collect()
    };

    Some(MeetingConfig {
        channel_name,
        max_rounds,
        summary_agent,
        available_agents,
        agent_registry,
    })
}

pub(super) fn list_registered_channel_bindings() -> Vec<RegisteredChannelBinding> {
    let Some(json) = load_role_map_json() else {
        return Vec::new();
    };

    let mut bindings = Vec::new();
    if let Some(by_id) = json.get("byChannelId").and_then(|value| value.as_object()) {
        for (channel_id_raw, entry) in by_id {
            let Ok(channel_id) = channel_id_raw.parse::<u64>() else {
                continue;
            };
            let Some(binding) = parse_role_binding(entry) else {
                continue;
            };
            let Some(owner_provider) = binding.provider.filter(ProviderKind::is_supported) else {
                continue;
            };
            bindings.push(RegisteredChannelBinding {
                channel_id,
                owner_provider,
                fallback_name: None,
            });
        }
    }

    bindings.sort_by_key(|binding| binding.channel_id);
    bindings
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Mem0EnvGuard {
        prev_api_key: Option<std::ffi::OsString>,
        prev_base_url: Option<std::ffi::OsString>,
    }

    impl Mem0EnvGuard {
        fn install() -> Self {
            crate::services::memory::reset_backend_health_for_tests();
            let prev_api_key = std::env::var_os("MEM0_API_KEY");
            let prev_base_url = std::env::var_os("MEM0_BASE_URL");
            unsafe {
                std::env::set_var("MEM0_API_KEY", "test-key");
                std::env::set_var("MEM0_BASE_URL", "http://mem0.local");
            }
            Self {
                prev_api_key,
                prev_base_url,
            }
        }
    }

    impl Drop for Mem0EnvGuard {
        fn drop(&mut self) {
            match self.prev_api_key.take() {
                Some(value) => unsafe { std::env::set_var("MEM0_API_KEY", value) },
                None => unsafe { std::env::remove_var("MEM0_API_KEY") },
            }
            match self.prev_base_url.take() {
                Some(value) => unsafe { std::env::set_var("MEM0_BASE_URL", value) },
                None => unsafe { std::env::remove_var("MEM0_BASE_URL") },
            }
            crate::services::memory::reset_backend_health_for_tests();
        }
    }

    fn with_temp_root<F>(f: F)
    where
        F: FnOnce(&TempDir),
    {
        let _guard = super::super::runtime_store::lock_test_env();
        let temp = TempDir::new().unwrap();
        let root = temp.path().join(".adk");
        std::fs::create_dir_all(&root).unwrap();
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        f(&temp);
        match prev {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    fn write_role_map(dir: &std::path::Path, content: &str) {
        let settings_dir = dir.join(".adk").join("config");
        std::fs::create_dir_all(&settings_dir).unwrap();
        std::fs::write(settings_dir.join("role_map.json"), content).unwrap();
    }

    #[test]
    fn test_resolve_role_binding_reads_memory_block_from_role_map() {
        with_temp_root(|temp_home: &TempDir| {
            let _mem0_env = Mem0EnvGuard::install();
            write_role_map(
                temp_home.path(),
                r#"{
  "byChannelId": {
    "123": {
      "roleId": "codex",
      "promptFile": "~/prompts/codex.md",
      "provider": "codex",
      "memory": {
        "backend": "mem0",
        "recall_timeout_ms": 50,
        "capture_timeout_ms": 8000,
        "mem0": {
          "profile": "strict",
          "ingestion": {
            "infer": true,
            "custom_instructions": "Remember deployment facts"
          }
        }
      }
    }
  }
}"#,
            );

            let binding = resolve_role_binding(ChannelId::new(123), None).unwrap();
            assert_eq!(
                binding.memory.backend,
                super::super::settings::MemoryBackendKind::Mem0
            );
            assert_eq!(binding.memory.recall_timeout_ms, 100);
            assert_eq!(binding.memory.capture_timeout_ms, 8000);
            assert_eq!(binding.memory.mem0.profile, "strict");
            assert_eq!(binding.memory.mem0.ingestion.infer, Some(true));
            assert_eq!(
                binding.memory.mem0.ingestion.custom_instructions.as_deref(),
                Some("Remember deployment facts")
            );
        });
    }

    #[test]
    fn test_load_meeting_config_uses_registry_and_available_agents_subset() {
        with_temp_root(|temp_home: &TempDir| {
            write_role_map(
                temp_home.path(),
                r#"{
  "byChannelId": {
    "123": {
      "roleId": "ch-td",
      "promptFile": "~/prompts/ch-td.md",
      "provider": "claude",
      "workspace": "~/workspaces/td"
    },
    "456": {
      "roleId": "ch-pd",
      "promptFile": "~/prompts/ch-pd.md",
      "provider": "codex"
    }
  },
  "meeting": {
    "channel_name": "meeting-room",
    "max_rounds": 4,
    "summary_agent": "ch-td",
    "available_agents": [
      {
        "role_id": "ch-pd",
        "display_name": "PD",
        "keywords": ["제품"],
        "domain_summary": "제품 방향과 사용자 가치 판단",
        "strengths": ["우선순위", "문제 정의"],
        "task_types": ["기획", "검토"],
        "anti_signals": ["코드 세부 구현 단독 담당"],
        "provider_hint": "gemini"
      }
    ]
  }
}"#,
            );

            let config = load_meeting_config().expect("meeting config should load");
            assert_eq!(config.max_rounds, 4);
            assert_eq!(config.agent_registry.len(), 2);
            assert_eq!(config.available_agents.len(), 1);
            assert_eq!(config.available_agents[0].role_id, "ch-pd");
            assert_eq!(config.available_agents[0].display_name, "PD");
            assert_eq!(
                config.available_agents[0].domain_summary.as_deref(),
                Some("제품 방향과 사용자 가치 판단")
            );
            assert_eq!(
                config.available_agents[0].strengths,
                vec!["우선순위", "문제 정의"]
            );
            assert_eq!(config.available_agents[0].task_types, vec!["기획", "검토"]);
            assert_eq!(
                config.available_agents[0].anti_signals,
                vec!["코드 세부 구현 단독 담당"]
            );
            assert_eq!(
                config.available_agents[0].provider_hint.as_deref(),
                Some("gemini")
            );
            assert!(!config.available_agents[0].metadata_missing);
            assert_eq!(config.available_agents[0].metadata_confidence, "high");
            assert_eq!(
                config.available_agents[0].binding.provider,
                Some(ProviderKind::Codex)
            );
            assert_eq!(
                config
                    .agent_registry
                    .iter()
                    .find(|agent| agent.role_id == "ch-td")
                    .and_then(|agent| agent.workspace.as_deref()),
                Some(&expand_tilde("~/workspaces/td")[..])
            );
        });
    }

    #[test]
    fn test_load_meeting_config_meeting_available_agent_overrides_channel_binding() {
        with_temp_root(|temp_home: &TempDir| {
            write_role_map(
                temp_home.path(),
                r#"{
  "byChannelId": {
    "456": {
      "roleId": "ch-pd",
      "promptFile": "~/prompts/channel-pd.md",
      "provider": "codex",
      "workspace": "~/workspaces/channel"
    }
  },
  "meeting": {
    "channel_name": "meeting-room",
    "summary_agent": "ch-pd",
    "available_agents": [
      {
        "role_id": "ch-pd",
        "display_name": "Meeting PD",
        "keywords": ["회의"],
        "domain_summary": "회의 전용 제품 판단",
        "promptFile": "~/prompts/meeting-pd.md",
        "provider": "gemini",
        "workspace": "~/workspaces/meeting"
      }
    ]
  }
}"#,
            );

            let config = load_meeting_config().expect("meeting config should load");
            let agent = config
                .available_agents
                .iter()
                .find(|agent| agent.role_id == "ch-pd")
                .expect("meeting agent should be present");

            assert_eq!(agent.display_name, "Meeting PD");
            assert_eq!(
                agent.binding.prompt_file,
                expand_tilde("~/prompts/meeting-pd.md")
            );
            assert_eq!(agent.binding.provider, Some(ProviderKind::Gemini));
            assert_eq!(
                agent.workspace.as_deref(),
                Some(&expand_tilde("~/workspaces/meeting")[..])
            );
        });
    }

    #[test]
    fn test_load_meeting_config_empty_available_agents_falls_back_to_registry() {
        with_temp_root(|temp_home: &TempDir| {
            write_role_map(
                temp_home.path(),
                r#"{
  "byChannelId": {
    "123": {
      "roleId": "ch-td",
      "promptFile": "~/prompts/ch-td.md",
      "provider": "claude"
    },
    "456": {
      "roleId": "ch-pd",
      "promptFile": "~/prompts/ch-pd.md",
      "provider": "codex"
    }
  },
  "meeting": {
    "channel_name": "meeting-room",
    "summary_agent": "ch-td",
    "available_agents": []
  }
}"#,
            );

            let config = load_meeting_config().expect("meeting config should load");
            let role_ids: Vec<&str> = config
                .available_agents
                .iter()
                .map(|agent| agent.role_id.as_str())
                .collect();

            assert_eq!(config.available_agents.len(), config.agent_registry.len());
            assert!(role_ids.contains(&"ch-td"));
            assert!(role_ids.contains(&"ch-pd"));
        });
    }

    #[test]
    fn test_load_meeting_config_registers_meeting_only_available_agent() {
        with_temp_root(|temp_home: &TempDir| {
            write_role_map(
                temp_home.path(),
                r#"{
  "byChannelId": {},
  "meeting": {
    "channel_name": "meeting-room",
    "summary_agent": "standalone",
    "available_agents": [
      {
        "role_id": "standalone",
        "display_name": "Standalone Expert",
        "keywords": ["strategy"],
        "domain_summary": "채널 바인딩 없이 회의에서만 쓰는 전문가",
        "promptFile": "~/prompts/standalone.md",
        "provider": "gemini",
        "workspace": "~/workspaces/standalone"
      }
    ]
  }
}"#,
            );

            let config = load_meeting_config().expect("meeting config should load");

            assert_eq!(config.agent_registry.len(), 1);
            assert_eq!(config.available_agents.len(), 1);
            let agent = &config.available_agents[0];
            assert_eq!(agent.role_id, "standalone");
            assert_eq!(agent.display_name, "Standalone Expert");
            assert_eq!(
                agent.domain_summary.as_deref(),
                Some("채널 바인딩 없이 회의에서만 쓰는 전문가")
            );
            assert_eq!(agent.binding.provider, Some(ProviderKind::Gemini));
            assert_eq!(
                agent.binding.prompt_file,
                expand_tilde("~/prompts/standalone.md")
            );
            assert_eq!(
                agent.workspace.as_deref(),
                Some(&expand_tilde("~/workspaces/standalone")[..])
            );
        });
    }
}
