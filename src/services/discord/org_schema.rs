use std::collections::HashMap;
use std::fs;

use poise::serenity_prelude::ChannelId;
use serde::Deserialize;

use super::meeting::{MeetingAgentConfig, MeetingConfig, SummaryAgentConfig, SummaryAgentRule};
use super::runtime_store::org_schema_path;
use super::settings::{
    MemoryConfigOverride, PeerAgentInfo, RegisteredChannelBinding, RoleBinding,
    resolve_memory_settings,
};
use crate::services::agent_identity::{self, identity_from_parts, identity_label};
use crate::services::provider::ProviderKind;
use crate::services::provider_auth_profile::{ProviderAuthProfileDef, validate_catalog};
use crate::utils::format::expand_tilde_string as expand_tilde;

// ─── YAML Schema Types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct OrgSchema {
    // #3034: serde wire fields deserialized from the org schema for forward
    // compatibility; no in-code reader today.
    #[allow(dead_code)]
    pub version: u32,
    // #3034: serde wire field deserialized from the org schema for forward
    // compatibility; no in-code reader today.
    #[allow(dead_code)]
    pub name: Option<String>,
    // serde-deserialized; consumed by `load_shared_prompt_path` (the shared-prompt
    // fallback chain whose root caller is currently dormant — see that fn).
    #[allow(dead_code)]
    pub shared_prompt: Option<String>,
    /// Root directory for prompt files (e.g. "$AGENTDESK_ROOT_DIR/prompts").
    /// When set, agent prompt_file is auto-derived as
    /// `{prompts_root}/agents/{role_id}/IDENTITY.md` if not explicitly specified.
    pub prompts_root: Option<String>,
    /// Root directory for skill files (e.g. "$AGENTDESK_ROOT_DIR/skills").
    // #3034: serde wire field; the loader was removed as dead, the config
    // surface is retained for forward compatibility.
    #[allow(dead_code)]
    pub skills_root: Option<String>,
    pub agents: HashMap<String, AgentDef>,
    pub channels: Option<ChannelsConfig>,
    pub meeting: Option<MeetingDef>,
    pub suffix_map: Option<HashMap<String, String>>,
    #[serde(default)]
    pub provider_auth_profiles: Option<HashMap<String, ProviderAuthProfileDef>>,
    #[serde(default)]
    pub provider_auth_primary_profiles: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AgentDef {
    pub display_name: String,
    pub prompt_file: Option<String>,
    pub keywords: Option<Vec<String>>,
    pub domain_summary: Option<String>,
    pub strengths: Option<Vec<String>>,
    pub task_types: Option<Vec<String>>,
    pub anti_signals: Option<Vec<String>>,
    pub provider_hint: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub workspace: Option<String>,
    pub peer_agents: Option<bool>,
    #[serde(default)]
    pub auth_profile: Option<String>,
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
    pub auth_profile: Option<String>,
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
    pub max_participants: Option<usize>,
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

// ─── Loading ────────────────────────────────────────────────────────────────

fn load_org_schema() -> Option<OrgSchema> {
    let path = org_schema_path()?;
    let content = fs::read_to_string(path).ok()?;
    match parse_org_schema(&content) {
        Ok(schema) => Some(schema),
        Err(error) => {
            tracing::error!("org schema rejected: {error}");
            None
        }
    }
}

pub(crate) fn provider_auth_catalog()
-> std::collections::HashMap<String, crate::services::provider_auth_profile::ProviderAuthProfileDef>
{
    load_org_schema()
        .and_then(|schema| schema.provider_auth_profiles)
        .unwrap_or_default()
}

pub(crate) fn provider_auth_primary_profiles() -> std::collections::HashMap<String, String> {
    load_org_schema()
        .and_then(|schema| schema.provider_auth_primary_profiles)
        .unwrap_or_default()
}

pub(crate) fn parse_org_schema(content: &str) -> Result<OrgSchema, String> {
    let schema: OrgSchema =
        serde_yaml::from_str(content).map_err(|error| format!("org schema yaml: {error}"))?;
    if let Some(catalog) = schema.provider_auth_profiles.as_ref() {
        validate_catalog(catalog).map_err(|error| error.to_string())?;
    }
    if let Some(primary) = schema.provider_auth_primary_profiles.as_ref() {
        let catalog = schema
            .provider_auth_profiles
            .as_ref()
            .cloned()
            .unwrap_or_default();
        for (provider, profile_id) in primary {
            let kind = ProviderKind::from_str(provider)
                .ok_or_else(|| format!("unknown primary profile provider '{provider}'"))?;
            if profile_id == "default" {
                continue;
            }
            let profile = catalog.get(profile_id).ok_or_else(|| {
                format!(
                    "provider primary '{provider}' references unknown auth_profile '{profile_id}'"
                )
            })?;
            if !profile.provider.eq_ignore_ascii_case(kind.as_str()) {
                return Err(format!(
                    "provider primary '{provider}' auth_profile '{profile_id}' belongs to '{}'",
                    profile.provider
                ));
            }
        }
    }
    Ok(schema)
}

fn configured_auth_profile<'a>(
    channel: Option<&'a str>,
    agent: Option<&'a str>,
) -> Option<&'a str> {
    channel
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| agent.map(str::trim).filter(|value| !value.is_empty()))
}

fn provider_primary_profile(schema: &OrgSchema, provider: Option<&str>) -> String {
    provider
        .and_then(|provider| {
            let provider = provider.to_ascii_lowercase();
            schema
                .provider_auth_primary_profiles
                .as_ref()
                .and_then(|profiles| profiles.get(&provider))
        })
        .cloned()
        .unwrap_or_else(|| "default".to_string())
}

pub(crate) fn spawn_auth_overlay(
    provider: ProviderKind,
    channel_id: Option<u64>,
) -> Result<crate::services::provider_auth_profile::ProviderAuthOverlay, String> {
    use crate::services::provider_auth_profile::resolve;

    let schema = load_org_schema();
    let catalog = schema
        .as_ref()
        .and_then(|schema| schema.provider_auth_profiles.clone())
        .unwrap_or_default();
    let profile = channel_id.and_then(|id| {
        resolve_role_binding(ChannelId::new(id), None).map(|binding| binding.auth_profile)
    });
    let overlay = resolve(provider.clone(), profile.as_deref(), None, &catalog)
        .map_err(|error| error.to_string())?;
    if let Some(binding) = channel_id.and_then(|id| resolve_role_binding(ChannelId::new(id), None))
    {
        if let Some(bound_provider) = binding.provider.clone() {
            let identity = identity_from_parts(
                &binding.role_id,
                &binding.role_id,
                bound_provider,
                binding.model.clone(),
                Some(binding.auth_profile.as_str()),
            );
            tracing::info!(
                identity = %identity_label(&identity),
                profile_id = %overlay.profile_id,
                home = overlay.home.as_ref().map(|path| path.display().to_string()),
                "provider spawn identity"
            );
        }
    }
    Ok(overlay)
}

pub(crate) fn overlay_from_tmux_session(
    provider: ProviderKind,
    session_name: &str,
) -> Result<crate::services::provider_auth_profile::ProviderAuthOverlay, String> {
    let channel_id =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(session_name)
            .and_then(|(_, channel)| channel.parse().ok());
    spawn_auth_overlay(provider, channel_id)
}

#[derive(Clone, Debug)]
pub(crate) struct ProfileBinding {
    pub agent_id: String,
    pub provider: String,
    pub profile_id: String,
    /// `None` is an agent-level effective profile. `Some` identifies a channel
    /// whose effective profile differs independently from the agent setting.
    pub channel_id: Option<String>,
    /// Whether this effective profile was explicitly pinned at the agent or
    /// channel level rather than inherited from the provider primary.
    pub is_explicit: bool,
}

pub(crate) fn list_profile_bindings() -> Vec<ProfileBinding> {
    let Some(schema) = load_org_schema() else {
        return Vec::new();
    };
    list_profile_bindings_from_schema(&schema)
}

fn list_profile_bindings_from_schema(schema: &OrgSchema) -> Vec<ProfileBinding> {
    let mut bindings = Vec::new();
    for (agent_id, def) in &schema.agents {
        let provider = def
            .provider
            .as_deref()
            .and_then(ProviderKind::from_str)
            .map(|kind| kind.as_str().to_string())
            .unwrap_or_else(|| {
                def.provider
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string())
            });
        let explicit_profile = configured_auth_profile(None, def.auth_profile.as_deref());
        let profile_id = explicit_profile
            .map(str::to_string)
            .unwrap_or_else(|| provider_primary_profile(&schema, def.provider.as_deref()));
        bindings.push(ProfileBinding {
            agent_id: agent_id.clone(),
            provider,
            profile_id,
            channel_id: None,
            is_explicit: explicit_profile.is_some(),
        });
    }
    if let Some(channels) = schema
        .channels
        .as_ref()
        .and_then(|channels| channels.by_id.as_ref())
    {
        for (channel_id, binding) in channels {
            let agent_def = schema.agents.get(&binding.agent);
            let provider = binding
                .provider
                .as_deref()
                .or(agent_def.and_then(|def| def.provider.as_deref()))
                .and_then(ProviderKind::from_str)
                .map(|kind| kind.as_str().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let explicit_profile = configured_auth_profile(
                binding.auth_profile.as_deref(),
                agent_def.and_then(|def| def.auth_profile.as_deref()),
            );
            let profile_id = explicit_profile.map(str::to_string).unwrap_or_else(|| {
                provider_primary_profile(
                    &schema,
                    binding
                        .provider
                        .as_deref()
                        .or(agent_def.and_then(|def| def.provider.as_deref())),
                )
            });
            bindings.push(ProfileBinding {
                agent_id: binding.agent.clone(),
                provider,
                profile_id,
                channel_id: Some(channel_id.clone()),
                is_explicit: explicit_profile.is_some(),
            });
        }
    }
    bindings
}

pub(crate) fn api_agent_identity(
    agent_id: &str,
    db_provider: Option<&str>,
    db_name: Option<&str>,
) -> serde_json::Value {
    let schema = load_org_schema();
    let def = schema
        .as_ref()
        .and_then(|schema| schema.agents.get(agent_id));
    let provider = def
        .and_then(|def| def.provider.as_deref())
        .or(db_provider)
        .and_then(ProviderKind::from_str)
        .unwrap_or_else(|| ProviderKind::Unsupported(db_provider.unwrap_or("unknown").to_string()));
    let identity = identity_from_parts(
        agent_id,
        def.map(|def| def.display_name.clone())
            .or_else(|| db_name.map(str::to_string))
            .unwrap_or_else(|| agent_id.to_string()),
        provider,
        def.and_then(|def| def.model.clone()),
        def.and_then(|def| def.auth_profile.as_deref()),
    );
    agent_identity::identity_json(&identity)
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

    let auth_profile = configured_auth_profile(
        ch_binding.auth_profile.as_deref(),
        agent_def.auth_profile.as_deref(),
    )
    .map(str::to_string)
    .unwrap_or_else(|| {
        provider_primary_profile(&schema, provider.as_ref().map(ProviderKind::as_str))
    });

    Some(RoleBinding {
        role_id: ch_binding.agent.clone(),
        prompt_file,
        provider,
        model,
        reasoning_effort: None,
        peer_agents_enabled,
        quality_feedback_injection_enabled: true,
        auth_profile,
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
            let root = std::path::Path::new(&root);
            let canonical = root.join("agents").join("_shared.prompt.md");
            if canonical.exists() {
                return Some(canonical.display().to_string());
            }
            let legacy = root.join("_shared.md");
            legacy.exists().then(|| legacy.display().to_string())
        })
}

pub(super) fn load_peer_agents() -> Vec<PeerAgentInfo> {
    let Some(schema) = load_org_schema() else {
        return Vec::new();
    };

    let mut result = Vec::new();
    for (role_id, def) in &schema.agents {
        let provider = def
            .provider
            .as_deref()
            .and_then(ProviderKind::from_str)
            .unwrap_or(ProviderKind::Unsupported("unknown".into()));
        let identity = identity_from_parts(
            role_id,
            def.display_name.clone(),
            provider.clone(),
            def.model.clone(),
            def.auth_profile.as_deref(),
        );
        result.push(PeerAgentInfo {
            role_id: role_id.clone(),
            display_name: def.display_name.clone(),
            keywords: def.keywords.clone().unwrap_or_default(),
            provider: Some(provider),
            identity_label: identity_label(&identity),
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
    // Use explicit meeting.available_agents as-is when present. Only absence
    // falls back to the full registry.
    let eligible_agents: Box<dyn Iterator<Item = (&String, &AgentDef)>> =
        if let Some(explicit_list) = meeting_def.available_agents.as_ref() {
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
                domain_summary: def.domain_summary.clone(),
                strengths: def.strengths.clone().unwrap_or_default(),
                task_types: def.task_types.clone().unwrap_or_default(),
                anti_signals: def.anti_signals.clone().unwrap_or_default(),
                provider_hint: def.provider_hint.clone().or_else(|| def.provider.clone()),
                provider: def.provider.as_deref().and_then(ProviderKind::from_str),
                model: def.model.clone(),
                reasoning_effort: None,
                workspace: def.workspace.as_deref().map(expand_tilde),
                peer_agents_enabled: def.peer_agents.unwrap_or(true),
                memory: resolve_memory_settings(def.memory.as_ref(), None),
            }
        })
        .collect();

    Some(MeetingConfig {
        channel_name: meeting_def.channel_name.clone(),
        max_rounds: meeting_def.max_rounds.unwrap_or(3),
        max_participants: meeting_def.max_participants.unwrap_or(5),
        summary_agent,
        available_agents,
    })
}

pub(super) fn list_registered_channel_bindings() -> Vec<RegisteredChannelBinding> {
    let Some(schema) = load_org_schema() else {
        return Vec::new();
    };

    let mut bindings = Vec::new();
    if let Some(by_id) = schema
        .channels
        .as_ref()
        .and_then(|channels| channels.by_id.as_ref())
    {
        for (channel_id_raw, binding) in by_id {
            let Ok(channel_id) = channel_id_raw.parse::<u64>() else {
                continue;
            };
            let owner_provider = binding
                .provider
                .as_deref()
                .or_else(|| {
                    schema
                        .agents
                        .get(&binding.agent)
                        .and_then(|agent| agent.provider.as_deref())
                })
                .and_then(ProviderKind::from_str);
            let Some(owner_provider) = owner_provider.filter(ProviderKind::is_supported) else {
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
    use super::*;
    use crate::services::agent_identity::identity_label;

    #[test]
    fn test_010_peer_identity_label_uses_formatter() {
        let yaml = r#"
version: 1
agents:
  coder:
    display_name: Coder
    provider: codex
    model: gpt-5.6-terra
    keywords: [code]
"#;
        let schema = parse_org_schema(yaml).expect("schema");
        let def = schema.agents.get("coder").expect("coder");
        let provider = ProviderKind::from_str(def.provider.as_deref().unwrap()).unwrap();
        let identity = identity_from_parts(
            "coder",
            def.display_name.clone(),
            provider,
            def.model.clone(),
            def.auth_profile.as_deref(),
        );
        assert_eq!(identity_label(&identity), "coder · codex terra · default");
        let json = agent_identity::identity_json(&identity);
        assert_eq!(json["label"], "coder · codex terra · default");
        assert!(json.get("token").is_none());
    }

    #[test]
    fn unknown_catalog_id_fails_org_load() {
        let yaml = r#"
version: 1
provider_auth_profiles:
  default:
    provider: codex
    home: ~/.adk/profiles/codex/default
agents:
  coder:
    display_name: Coder
    provider: codex
"#;
        let err = parse_org_schema(yaml).unwrap_err();
        assert!(err.contains("reserved") || err.contains("default"));
    }

    #[test]
    fn primary_profile_is_inherited_until_agent_or_channel_explicitly_overrides_it() {
        let yaml = r#"
version: 1
provider_auth_profiles:
  codex-work:
    provider: codex
    home: ~/.adk/profiles/codex/codex-work
  codex-personal:
    provider: codex
    home: ~/.adk/profiles/codex/codex-personal
provider_auth_primary_profiles:
  codex: codex-work
agents:
  coder:
    display_name: Coder
    provider: codex
channels:
  by_id:
    "123":
      agent: coder
      auth_profile: codex-personal
"#;
        let schema = parse_org_schema(yaml).expect("schema");
        let bindings = list_profile_bindings_from_schema(&schema);
        let agent = bindings
            .iter()
            .find(|binding| binding.channel_id.is_none())
            .expect("agent binding");
        assert_eq!(agent.profile_id, "codex-work");
        assert!(!agent.is_explicit);

        let channel = bindings
            .iter()
            .find(|binding| binding.channel_id.as_deref() == Some("123"))
            .expect("channel binding");
        assert_eq!(channel.profile_id, "codex-personal");
        assert!(channel.is_explicit);
    }

    #[test]
    fn provider_primary_rejects_profile_from_another_provider() {
        let yaml = r#"
version: 1
provider_auth_profiles:
  claude-work:
    provider: claude
    home: ~/.adk/profiles/claude/claude-work
provider_auth_primary_profiles:
  codex: claude-work
agents: {}
"#;
        let error = parse_org_schema(yaml).expect_err("provider mismatch must fail");
        assert!(error.contains("belongs to"));
    }
}
