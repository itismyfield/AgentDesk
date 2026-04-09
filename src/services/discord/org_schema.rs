use std::collections::HashMap;
use std::fs;

use poise::serenity_prelude::ChannelId;
use serde::Deserialize;

use super::meeting::{MeetingAgentConfig, MeetingConfig, SummaryAgentConfig, SummaryAgentRule};
use super::runtime_store::org_schema_path;
use super::settings::{MemoryConfigOverride, PeerAgentInfo, RoleBinding, resolve_memory_settings};
use crate::services::provider::ProviderKind;

// ─── YAML Schema Types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct OrgSchema {
    #[allow(dead_code)]
    pub version: u32,
    #[allow(dead_code)]
    pub name: Option<String>,
    pub shared_prompt: Option<String>,
    /// Root directory for prompt files (e.g. "$AGENTDESK_ROOT_DIR/prompts").
    /// When set, agent prompt_file is auto-derived as
    /// `{prompts_root}/agents/{role_id}/IDENTITY.md` if not explicitly specified.
    pub prompts_root: Option<String>,
    /// Root directory for skill files (e.g. "$AGENTDESK_ROOT_DIR/skills").
    #[allow(dead_code)]
    pub skills_root: Option<String>,
    pub agents: HashMap<String, AgentDef>,
    pub channels: Option<ChannelsConfig>,
    pub meeting: Option<MeetingDef>,
    pub suffix_map: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AgentDef {
    pub display_name: String,
    pub prompt_file: Option<String>,
    pub keywords: Option<Vec<String>>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub workspace: Option<String>,
    pub peer_agents: Option<bool>,
    #[serde(default)]
    pub memory: Option<MemoryConfigOverride>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ChannelsConfig {
    pub by_id: Option<HashMap<String, ChannelBinding>>,
    pub by_name: Option<ChannelsByName>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ChannelBinding {
    pub agent: String,
    pub workspace: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub peer_agents: Option<bool>,
    #[serde(default)]
    pub memory: Option<MemoryConfigOverride>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ChannelsByName {
    pub enabled: Option<bool>,
    pub mappings: Option<HashMap<String, ChannelBinding>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MeetingDef {
    pub channel_name: String,
    pub max_rounds: Option<u32>,
    pub summary_agent: Option<SummaryAgentDef>,
    /// Explicit list of agent role_ids eligible for meetings.
    /// When omitted, all agents in the schema are eligible.
    pub available_agents: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum SummaryAgentDef {
    Static(String),
    Dynamic {
        rules: Option<Vec<SummaryRuleDef>>,
        default: String,
    },
}

#[derive(Debug, Deserialize)]
pub(super) struct SummaryRuleDef {
    pub keywords: Vec<String>,
    pub agent: String,
}

// ─── Tilde expansion ────────────────────────────────────────────────────────

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

// ─── Loading ────────────────────────────────────────────────────────────────

fn load_org_schema() -> Option<OrgSchema> {
    let path = org_schema_path()?;
    let content = fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&content).ok()
}

pub(super) fn org_schema_exists() -> bool {
    org_schema_path().map(|p| p.exists()).unwrap_or(false)
}

/// Check if a role_id exists in the org schema's agents map.
pub(super) fn is_known_agent(role_id: &str) -> Option<bool> {
    let schema = load_org_schema()?;
    Some(schema.agents.contains_key(role_id))
}

// ─── Resolution functions (mirror role_map.rs API) ──────────────────────────

/// Resolve a channel binding from org schema, returning the ChannelBinding
/// and the agent definition it refers to.
fn resolve_channel_binding<'a>(
    schema: &'a OrgSchema,
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<(&'a ChannelBinding, &'a AgentDef)> {
    let channels = schema.channels.as_ref()?;

    // 1. Try by_id
    if let Some(by_id) = &channels.by_id {
        let key = channel_id.get().to_string();
        if let Some(binding) = by_id.get(&key) {
            if let Some(agent_def) = schema.agents.get(&binding.agent) {
                return Some((binding, agent_def));
            }
        }
    }

    // 2. Try by_name (if enabled)
    if let Some(by_name) = &channels.by_name {
        let enabled = by_name.enabled.unwrap_or(false);
        if enabled {
            if let (Some(mappings), Some(cname)) = (&by_name.mappings, channel_name) {
                if let Some(binding) = mappings.get(cname) {
                    if let Some(agent_def) = schema.agents.get(&binding.agent) {
                        return Some((binding, agent_def));
                    }
                }
            }
        }
    }

    None
}

pub(super) fn resolve_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleBinding> {
    let schema = load_org_schema()?;
    let (ch_binding, agent_def) = resolve_channel_binding(&schema, channel_id, channel_name)?;

    // Channel-level overrides take priority over agent-level defaults
    let provider = ch_binding
        .provider
        .as_deref()
        .or(agent_def.provider.as_deref())
        .and_then(ProviderKind::from_str);

    let model = ch_binding.model.clone().or_else(|| agent_def.model.clone());
    let peer_agents_enabled = ch_binding
        .peer_agents
        .or(agent_def.peer_agents)
        .unwrap_or(true);
    let memory = resolve_memory_settings(agent_def.memory.as_ref(), ch_binding.memory.as_ref());

    // Explicit prompt_file > auto-derived from prompts_root > empty
    let prompt_file = agent_def
        .prompt_file
        .as_deref()
        .map(expand_tilde)
        .or_else(|| {
            schema.prompts_root.as_deref().map(|root| {
                let base = expand_tilde(root);
                format!("{}/agents/{}/IDENTITY.md", base, ch_binding.agent)
            })
        })
        .unwrap_or_default();

    Some(RoleBinding {
        role_id: ch_binding.agent.clone(),
        prompt_file,
        provider,
        model,
        reasoning_effort: None,
        peer_agents_enabled,
        memory,
    })
}

pub(super) fn resolve_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    let schema = load_org_schema()?;
    let (ch_binding, agent_def) = resolve_channel_binding(&schema, channel_id, channel_name)?;

    // Channel-level workspace overrides agent-level default
    let ws = ch_binding
        .workspace
        .as_deref()
        .or(agent_def.workspace.as_deref())?;

    Some(expand_tilde(ws))
}

pub(super) fn load_shared_prompt_path() -> Option<String> {
    let schema = load_org_schema()?;
    // Explicit shared_prompt > auto-derived from prompts_root/agents/_shared.prompt.md
    schema
        .shared_prompt
        .as_deref()
        .map(expand_tilde)
        .or_else(|| {
            let root = expand_tilde(schema.prompts_root.as_deref()?);
            let canonical = format!("{}/agents/_shared.prompt.md", root);
            if std::path::Path::new(&canonical).exists() {
                return Some(canonical);
            }
            let legacy = format!("{}/_shared.md", root);
            std::path::Path::new(&legacy).exists().then_some(legacy)
        })
}

/// Return the configured skills_root path (expanded).
#[allow(dead_code)]
pub(super) fn load_skills_root() -> Option<String> {
    let schema = load_org_schema()?;
    schema.skills_root.as_deref().map(expand_tilde)
}

pub(super) fn load_peer_agents() -> Vec<PeerAgentInfo> {
    let Some(schema) = load_org_schema() else {
        return Vec::new();
    };

    let mut result = Vec::new();
    for (role_id, def) in &schema.agents {
        result.push(PeerAgentInfo {
            role_id: role_id.clone(),
            display_name: def.display_name.clone(),
            keywords: def.keywords.clone().unwrap_or_default(),
        });
    }

    // Sort by role_id for stable ordering
    result.sort_by(|a, b| a.role_id.cmp(&b.role_id));
    result
}

pub(super) fn load_meeting_config() -> Option<MeetingConfig> {
    let schema = load_org_schema()?;
    let meeting_def = schema.meeting.as_ref()?;

    let summary_agent = match &meeting_def.summary_agent {
        Some(SummaryAgentDef::Static(agent)) => SummaryAgentConfig::Static(agent.clone()),
        Some(SummaryAgentDef::Dynamic { rules, default }) => {
            let parsed_rules = rules
                .as_ref()
                .map(|rs| {
                    rs.iter()
                        .map(|r| SummaryAgentRule {
                            keywords: r.keywords.clone(),
                            agent: r.agent.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            SummaryAgentConfig::Dynamic {
                rules: parsed_rules,
                default: default.clone(),
            }
        }
        None => return None,
    };

    let prompts_root = schema.prompts_root.as_deref().map(expand_tilde);
    // Use explicit meeting.available_agents if set, otherwise all agents
    let eligible_agents: Box<dyn Iterator<Item = (&String, &AgentDef)>> =
        if let Some(ref explicit_list) = meeting_def.available_agents {
            Box::new(
                schema
                    .agents
                    .iter()
                    .filter(|(role_id, _)| explicit_list.contains(role_id)),
            )
        } else {
            Box::new(schema.agents.iter())
        };
    let available_agents: Vec<MeetingAgentConfig> = eligible_agents
        .map(|(role_id, def)| {
            let prompt_file = def
                .prompt_file
                .as_deref()
                .map(expand_tilde)
                .or_else(|| {
                    prompts_root
                        .as_ref()
                        .map(|root| format!("{}/agents/{}/IDENTITY.md", root, role_id))
                })
                .unwrap_or_default();
            MeetingAgentConfig {
                role_id: role_id.clone(),
                display_name: def.display_name.clone(),
                keywords: def.keywords.clone().unwrap_or_default(),
                prompt_file,
            }
        })
        .collect();

    Some(MeetingConfig {
        channel_name: meeting_def.channel_name.clone(),
        max_rounds: meeting_def.max_rounds.unwrap_or(3),
        summary_agent,
        available_agents,
    })
}

/// Look up the provider for a channel name suffix from org schema suffix_map.
pub(super) fn lookup_suffix_provider(channel_name: &str) -> Option<ProviderKind> {
    let schema = load_org_schema()?;
    let suffix_map = schema.suffix_map.as_ref()?;
    for (suffix, provider_str) in suffix_map {
        if channel_name.ends_with(suffix.as_str()) {
            return Some(ProviderKind::from_str_or_unsupported(provider_str));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use poise::serenity_prelude::ChannelId;
    use std::fs;
    use tempfile::TempDir;

    use super::*;

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
        fs::create_dir_all(&root).unwrap();
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        f(&temp);
        match prev {
            Some(v) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", v) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    fn write_org_yaml(dir: &std::path::Path, content: &str) {
        let settings_dir = dir.join(".adk").join("config");
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(settings_dir.join("org.yaml"), content).unwrap();
    }

    #[test]
    fn test_resolve_role_binding_from_org_schema() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
name: Test Org
agents:
  ch-td:
    display_name: "TD (테크니컬 디렉터)"
    prompt_file: "~/prompts/ch-td.md"
    keywords: ["아키텍처", "코드"]
    provider: claude
channels:
  by_id:
    "123":
      agent: ch-td
"#,
            );

            let binding = resolve_role_binding(ChannelId::new(123), None).unwrap();
            assert_eq!(binding.role_id, "ch-td");
            assert!(binding.prompt_file.ends_with("/prompts/ch-td.md"));
            assert_eq!(binding.provider, Some(ProviderKind::Claude));
        });
    }

    #[test]
    fn test_resolve_by_channel_name() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents:
  ch-pd:
    display_name: "PD"
    prompt_file: "~/prompts/ch-pd.md"
channels:
  by_name:
    enabled: true
    mappings:
      "dev-chat":
        agent: ch-pd
        workspace: ~/dev
"#,
            );

            let binding = resolve_role_binding(ChannelId::new(999), Some("dev-chat")).unwrap();
            assert_eq!(binding.role_id, "ch-pd");

            let ws = resolve_workspace(ChannelId::new(999), Some("dev-chat")).unwrap();
            assert!(ws.ends_with("/dev"));
        });
    }

    #[test]
    fn test_channel_binding_overrides_agent_defaults() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents:
  my-agent:
    display_name: "My Agent"
    prompt_file: "~/prompts/my.md"
    provider: claude
    workspace: ~/default-ws
channels:
  by_id:
    "100":
      agent: my-agent
      provider: codex
      workspace: ~/override-ws
"#,
            );

            let binding = resolve_role_binding(ChannelId::new(100), None).unwrap();
            assert_eq!(binding.provider, Some(ProviderKind::Codex));

            let ws = resolve_workspace(ChannelId::new(100), None).unwrap();
            assert!(ws.ends_with("/override-ws"));
        });
    }

    #[test]
    fn test_channel_binding_can_disable_peer_agents() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents:
  spark:
    display_name: "Spark"
    prompt_file: "~/prompts/spark.md"
    peer_agents: true
channels:
  by_id:
    "1488022491992424448":
      agent: spark
      peer_agents: false
"#,
            );

            let binding = resolve_role_binding(ChannelId::new(1488022491992424448), None).unwrap();
            assert!(!binding.peer_agents_enabled);
        });
    }

    #[test]
    fn test_channel_binding_memory_overrides_agent_memory_defaults() {
        with_temp_root(|temp_home: &TempDir| {
            let _mem0_env = Mem0EnvGuard::install();
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents:
  spark:
    display_name: "Spark"
    prompt_file: "~/prompts/spark.md"
    memory:
      backend: mem0
      recall_timeout_ms: 900
      capture_timeout_ms: 7000
      mem0:
        profile: default
        ingestion:
          infer: false
channels:
  by_id:
    "1488022491992424448":
      agent: spark
      memory:
        recall_timeout_ms: 50
        mem0:
          profile: strict
          ingestion:
            custom_instructions: "Prefer high-confidence facts"
"#,
            );

            let binding = resolve_role_binding(ChannelId::new(1488022491992424448), None).unwrap();
            assert_eq!(
                binding.memory.backend,
                super::super::settings::MemoryBackendKind::Mem0
            );
            assert_eq!(binding.memory.recall_timeout_ms, 100);
            assert_eq!(binding.memory.capture_timeout_ms, 7000);
            assert_eq!(binding.memory.mem0.profile, "strict");
            assert_eq!(binding.memory.mem0.ingestion.infer, Some(false));
            assert_eq!(
                binding.memory.mem0.ingestion.custom_instructions.as_deref(),
                Some("Prefer high-confidence facts")
            );
        });
    }

    #[test]
    fn test_load_peer_agents_from_org_schema() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents:
  ch-td:
    display_name: "TD"
    keywords: ["코드"]
  ch-pd:
    display_name: "PD"
    keywords: ["제품"]
"#,
            );

            let peers = load_peer_agents();
            assert_eq!(peers.len(), 2);
        });
    }

    #[test]
    fn test_suffix_map_from_org_schema() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents: {}
suffix_map:
  "-cc": claude
  "-cdx": codex
  "-qw": qwen
"#,
            );

            assert_eq!(
                lookup_suffix_provider("test-cc"),
                Some(ProviderKind::Claude)
            );
            assert_eq!(
                lookup_suffix_provider("test-cdx"),
                Some(ProviderKind::Codex)
            );
            assert_eq!(lookup_suffix_provider("test-qw"), Some(ProviderKind::Qwen));
            assert_eq!(lookup_suffix_provider("test-other"), None);
        });
    }

    #[test]
    fn test_expand_tilde_bare() {
        let expanded = expand_tilde("~");
        assert_ne!(expanded, "~");
        // Unix: starts with '/', Windows: starts with drive letter (e.g. C:\)
        assert!(expanded.starts_with('/') || expanded.chars().nth(1) == Some(':'));
    }

    #[test]
    fn test_meeting_available_agents_subset() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents:
  td:
    display_name: "TD"
    keywords: ["code"]
  pd:
    display_name: "PD"
    keywords: ["product"]
  qad:
    display_name: "QAD"
    keywords: ["test"]
meeting:
  channel_name: "meeting"
  summary_agent: "td"
  available_agents: ["td", "pd"]
channels:
  by_id:
    "300":
      agent: td
"#,
            );

            let config = load_meeting_config().expect("meeting config should load");
            // Only td and pd should be eligible, not qad
            let role_ids: Vec<&str> = config
                .available_agents
                .iter()
                .map(|a| a.role_id.as_str())
                .collect();
            assert!(role_ids.contains(&"td"), "td should be in available_agents");
            assert!(role_ids.contains(&"pd"), "pd should be in available_agents");
            assert!(
                !role_ids.contains(&"qad"),
                "qad should NOT be in available_agents"
            );
            assert_eq!(config.available_agents.len(), 2);
        });
    }

    #[test]
    fn test_prompts_root_auto_derive() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
prompts_root: "~/.adk/prompts"
agents:
  my-agent:
    display_name: "Agent"
channels:
  by_id:
    "200":
      agent: my-agent
"#,
            );

            // prompt_file should be auto-derived from prompts_root
            let binding = resolve_role_binding(ChannelId::new(200), None).unwrap();
            assert!(
                binding
                    .prompt_file
                    .contains("/prompts/agents/my-agent/IDENTITY.md"),
                "Expected auto-derived prompt path, got: {}",
                binding.prompt_file
            );
        });
    }

    #[test]
    fn test_shared_prompt_auto_derive_prefers_canonical_agents_path() {
        with_temp_root(|temp_home: &TempDir| {
            let canonical = temp_home
                .path()
                .join(".adk")
                .join("config")
                .join("agents")
                .join("_shared.prompt.md");
            fs::create_dir_all(canonical.parent().unwrap()).unwrap();
            fs::write(&canonical, "# shared").unwrap();
            let prompts_root = temp_home.path().join(".adk").join("config");
            let yaml = format!(
                r#"
version: 1
prompts_root: "{}"
agents: {{}}
"#,
                prompts_root.display()
            );
            write_org_yaml(temp_home.path(), &yaml);

            let shared = load_shared_prompt_path().expect("shared prompt path");
            assert!(shared.ends_with("/config/agents/_shared.prompt.md"));
        });
    }

    // ── P0 tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_lookup_suffix_provider_cc_claude() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents: {}
suffix_map:
  "-cc": claude
  "-cdx": codex
"#,
            );

            assert_eq!(lookup_suffix_provider("dev-cc"), Some(ProviderKind::Claude));
        });
    }

    #[test]
    fn test_lookup_suffix_provider_cdx_codex() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents: {}
suffix_map:
  "-cc": claude
  "-cdx": codex
"#,
            );

            assert_eq!(lookup_suffix_provider("dev-cdx"), Some(ProviderKind::Codex));
        });
    }

    #[test]
    fn test_org_schema_yaml_parsing() {
        // Test that a full org schema YAML string parses correctly
        let yaml = r#"
version: 1
name: "Test Organization"
agents:
  alpha:
    display_name: "Alpha Agent"
    keywords: ["search", "index"]
    provider: claude
  beta:
    display_name: "Beta Agent"
    keywords: ["deploy"]
    provider: codex
    workspace: ~/beta-ws
channels:
  by_id:
    "100":
      agent: alpha
suffix_map:
  "-cc": claude
  "-cdx": codex
"#;
        let schema: OrgSchema = serde_yaml::from_str(yaml).expect("YAML should parse");
        assert_eq!(schema.version, 1);
        assert_eq!(schema.name.as_deref(), Some("Test Organization"));
        assert_eq!(schema.agents.len(), 2);
        assert!(schema.agents.contains_key("alpha"));
        assert!(schema.agents.contains_key("beta"));
        assert_eq!(schema.agents["alpha"].display_name, "Alpha Agent");
        assert_eq!(schema.agents["beta"].provider.as_deref(), Some("codex"));
        let suffix_map = schema.suffix_map.as_ref().unwrap();
        assert_eq!(suffix_map.get("-cc").map(String::as_str), Some("claude"));
        assert_eq!(suffix_map.get("-cdx").map(String::as_str), Some("codex"));
    }

    #[test]
    fn test_is_known_agent_from_org_schema() {
        with_temp_root(|temp_home: &TempDir| {
            write_org_yaml(
                temp_home.path(),
                r#"
version: 1
agents:
  known-agent:
    display_name: "Known"
"#,
            );

            assert_eq!(is_known_agent("known-agent"), Some(true));
            assert_eq!(is_known_agent("nonexistent-agent"), Some(false));
        });
    }
}
