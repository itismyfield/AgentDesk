use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use poise::serenity_prelude::ChannelId;

use super::meeting::{MeetingAgentConfig, MeetingConfig, SummaryAgentConfig, SummaryAgentRule};
use super::settings::{
    PeerAgentInfo, RegisteredChannelBinding, RoleBinding, resolve_memory_settings,
};
use crate::config::{
    AgentChannel, AgentChannelConfig, AgentChannels, AgentDef, Config, MeetingAgentEntry,
    MeetingSummaryAgentDef,
};
use crate::services::provider::ProviderKind;

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

fn load_agentdesk_config_with_path() -> Option<(Config, std::path::PathBuf)> {
    let root = crate::config::runtime_root()?;
    for path in [
        crate::runtime_layout::config_file_path(&root),
        crate::runtime_layout::legacy_config_file_path(&root),
    ] {
        if path.is_file() {
            return crate::config::load_from_path(&path)
                .ok()
                .map(|config| (config, path));
        }
    }
    None
}

#[derive(Clone, Debug)]
pub(crate) struct AgentSetupConfigInput {
    pub agent_id: String,
    pub provider: String,
    pub channel_id: String,
    pub prompt_file: String,
    pub workspace: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentSetupConfigMutation {
    Created,
    Unchanged,
    Conflict(String),
}

pub(crate) fn agent_setup_config_path(root: &Path) -> PathBuf {
    crate::runtime_layout::config_file_path(root)
}

pub(crate) fn load_agent_setup_config(root: &Path) -> Result<(Config, PathBuf, bool), String> {
    for path in [
        crate::runtime_layout::config_file_path(root),
        crate::runtime_layout::legacy_config_file_path(root),
    ] {
        if path.is_file() {
            let config = crate::config::load_from_path(&path)
                .map_err(|error| format!("load '{}': {error}", path.display()))?;
            return Ok((config, path, true));
        }
    }

    Ok((Config::default(), agent_setup_config_path(root), false))
}

pub(crate) fn ensure_agent_setup_config(
    config: &mut Config,
    input: &AgentSetupConfigInput,
) -> AgentSetupConfigMutation {
    if let Some(existing) = config
        .agents
        .iter()
        .find(|agent| agent.id == input.agent_id)
    {
        if agent_setup_config_matches(existing, input) {
            return AgentSetupConfigMutation::Unchanged;
        }
        return AgentSetupConfigMutation::Conflict(format!(
            "agent '{}' already exists in agentdesk.yaml with different setup data",
            input.agent_id
        ));
    }

    let Some(channel) = agent_channel_for_setup(input) else {
        return AgentSetupConfigMutation::Conflict(format!(
            "unsupported provider '{}'",
            input.provider
        ));
    };

    let mut channels = AgentChannels::default();
    match input.provider.as_str() {
        "claude" => channels.claude = Some(channel),
        "codex" => channels.codex = Some(channel),
        "gemini" => channels.gemini = Some(channel),
        "qwen" => channels.qwen = Some(channel),
        _ => {
            return AgentSetupConfigMutation::Conflict(format!(
                "unsupported provider '{}'",
                input.provider
            ));
        }
    }

    config.agents.push(AgentDef {
        id: input.agent_id.clone(),
        name: input.agent_id.clone(),
        name_ko: None,
        provider: input.provider.clone(),
        channels,
        keywords: Vec::new(),
        department: None,
        avatar_emoji: None,
    });

    AgentSetupConfigMutation::Created
}

fn agent_channel_for_setup(input: &AgentSetupConfigInput) -> Option<AgentChannel> {
    ProviderKind::from_str(&input.provider)?;
    Some(AgentChannel::Detailed(AgentChannelConfig {
        id: Some(input.channel_id.clone()),
        name: None,
        aliases: Vec::new(),
        prompt_file: Some(input.prompt_file.clone()),
        workspace: Some(input.workspace.clone()),
        provider: Some(input.provider.clone()),
        model: None,
        reasoning_effort: None,
        peer_agents: None,
        quality_feedback_injection: None,
        cache_ttl_minutes: None,
    }))
}

fn agent_setup_config_matches(agent: &AgentDef, input: &AgentSetupConfigInput) -> bool {
    if agent.provider != input.provider {
        return false;
    }
    let channel = match input.provider.as_str() {
        "claude" => agent.channels.claude.as_ref(),
        "codex" => agent.channels.codex.as_ref(),
        "gemini" => agent.channels.gemini.as_ref(),
        "qwen" => agent.channels.qwen.as_ref(),
        _ => None,
    };
    let Some(channel) = channel else {
        return false;
    };

    channel.channel_id().as_deref() == Some(input.channel_id.as_str())
        && channel.prompt_file().as_deref() == Some(input.prompt_file.as_str())
        && channel.workspace().as_deref() == Some(input.workspace.as_str())
        && channel.provider().as_deref() == Some(input.provider.as_str())
}

fn load_agentdesk_config() -> Option<Config> {
    load_agentdesk_config_with_path().map(|(config, _)| config)
}

fn meeting_available_agents_explicitly_empty(config_path: &std::path::Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&raw) else {
        return false;
    };
    value
        .get("meeting")
        .and_then(|meeting| meeting.get("available_agents"))
        .and_then(|agents| agents.as_sequence())
        .map(|agents| agents.is_empty())
        .unwrap_or(false)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ResolvedDiscordBotConfig {
    pub name: String,
    pub token: String,
    pub provider: Option<ProviderKind>,
    pub agent: Option<String>,
    pub auth: crate::config::DiscordBotAuthConfig,
    pub description: Option<String>,
    pub owner_id: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedDmDefaultAgent {
    pub role_binding: RoleBinding,
    pub workspace: String,
}

fn resolve_bot_token(bot_name: &str, bot: &crate::config::BotConfig) -> Option<String> {
    bot.token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| crate::credential::read_bot_token(bot_name))
}

fn default_prompt_path(role_id: &str) -> Option<String> {
    let root = crate::config::runtime_root()?;
    let agents_root = crate::runtime_layout::managed_agents_root(&root);
    let canonical = agents_root.join(role_id).join("IDENTITY.md");
    if canonical.exists() {
        return Some(canonical.display().to_string());
    }
    let legacy_flat = agents_root.join(format!("{role_id}.prompt.md"));
    if legacy_flat.exists() {
        return Some(legacy_flat.display().to_string());
    }
    Some(canonical.display().to_string())
}

fn default_workspace(role_id: &str) -> Option<String> {
    let root = crate::config::runtime_root()?;
    Some(root.join("workspaces").join(role_id).display().to_string())
}

fn shared_prompt_fallback_path() -> Option<String> {
    let root = crate::config::runtime_root()?;
    let canonical = crate::runtime_layout::shared_prompt_path(&root);
    canonical
        .exists()
        .then(|| canonical.display().to_string())
        .or_else(|| {
            let legacy = crate::runtime_layout::config_dir(&root).join("_shared.md");
            legacy.exists().then(|| legacy.display().to_string())
        })
}

fn agent_display_name(agent: &crate::config::AgentDef) -> String {
    agent
        .name_ko
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| agent.name.clone())
}

fn match_channel(
    channel: &AgentChannel,
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<u8> {
    let channel_id_str = channel_id.get().to_string();
    if let Some(explicit_channel_id) = channel.channel_id() {
        return (explicit_channel_id == channel_id_str).then_some(2);
    }

    let channel_name = channel_name?.trim();
    if channel_name.is_empty() {
        return None;
    }
    channel
        .aliases()
        .iter()
        .any(|alias| alias == channel_name)
        .then_some(1)
}

fn find_channel_binding<'a>(
    config: &'a Config,
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<(&'a crate::config::AgentDef, &'static str, &'a AgentChannel)> {
    let mut best: Option<(&crate::config::AgentDef, &'static str, &AgentChannel, u8)> = None;

    for agent in &config.agents {
        for (provider_key, maybe_channel) in agent.channels.iter() {
            let Some(channel) = maybe_channel else {
                continue;
            };
            let Some(score) = match_channel(channel, channel_id, channel_name) else {
                continue;
            };

            if best
                .as_ref()
                .map(|(_, _, _, best_score)| score > *best_score)
                .unwrap_or(true)
            {
                best = Some((agent, provider_key, channel, score));
            }
        }
    }

    best.map(|(agent, provider_key, channel, _)| (agent, provider_key, channel))
}

fn binding_provider(
    agent: &crate::config::AgentDef,
    provider_key: &str,
    channel: &AgentChannel,
) -> Option<ProviderKind> {
    channel
        .provider()
        .or_else(|| Some(agent.provider.clone()))
        .or_else(|| Some(provider_key.to_string()))
        .and_then(|raw| ProviderKind::from_str(&raw))
}

fn role_binding_from_channel(
    agent: &crate::config::AgentDef,
    provider_key: &str,
    channel: &AgentChannel,
) -> RoleBinding {
    RoleBinding {
        role_id: agent.id.clone(),
        prompt_file: channel
            .prompt_file()
            .map(|value| expand_tilde(&value))
            .or_else(|| default_prompt_path(&agent.id))
            .unwrap_or_default(),
        provider: binding_provider(agent, provider_key, channel),
        model: channel.model(),
        reasoning_effort: channel.reasoning_effort(),
        peer_agents_enabled: channel.peer_agents().unwrap_or(true),
        quality_feedback_injection_enabled: channel.quality_feedback_injection().unwrap_or(true),
        memory: resolve_memory_settings(None, None),
    }
}

fn role_binding_from_agent(agent: &crate::config::AgentDef, provider: ProviderKind) -> RoleBinding {
    RoleBinding {
        role_id: agent.id.clone(),
        prompt_file: default_prompt_path(&agent.id).unwrap_or_default(),
        provider: Some(provider),
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: true,
        quality_feedback_injection_enabled: true,
        memory: resolve_memory_settings(None, None),
    }
}

fn find_agent_channel_for_provider<'a>(
    agent: &'a crate::config::AgentDef,
    provider: &ProviderKind,
) -> Option<(&'static str, &'a AgentChannel)> {
    agent
        .channels
        .iter()
        .into_iter()
        .find_map(|(provider_key, maybe_channel)| {
            let channel = maybe_channel?;
            (binding_provider(agent, provider_key, channel).as_ref() == Some(provider))
                .then_some((provider_key, channel))
        })
}

pub(super) fn resolve_dm_default_agent(provider: &ProviderKind) -> Option<ResolvedDmDefaultAgent> {
    let config = load_agentdesk_config()?;
    let agent_id = config
        .discord
        .dm_default_agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let agent = match config.agents.iter().find(|agent| agent.id == agent_id) {
        Some(agent) => agent,
        None => {
            tracing::warn!("  [dm-default] configured agent '{}' not found", agent_id);
            return None;
        }
    };

    if let Some((provider_key, channel)) = find_agent_channel_for_provider(agent, provider) {
        let role_binding = role_binding_from_channel(agent, provider_key, channel);
        let workspace = channel
            .workspace()
            .map(|value| expand_tilde(&value))
            .or_else(|| default_workspace(&agent.id))?;
        return Some(ResolvedDmDefaultAgent {
            role_binding,
            workspace,
        });
    }

    let agent_provider = match ProviderKind::from_str(&agent.provider) {
        Some(agent_provider) => agent_provider,
        None => {
            tracing::warn!(
                "  [dm-default] agent '{}' has unsupported provider '{}'",
                agent.id,
                agent.provider
            );
            return None;
        }
    };
    if &agent_provider != provider {
        tracing::info!(
            "  [dm-default] skipping agent '{}' for provider {} (configured provider {})",
            agent.id,
            provider.as_str(),
            agent_provider.as_str()
        );
        return None;
    }

    let workspace = match default_workspace(&agent.id) {
        Some(workspace) => workspace,
        None => {
            tracing::warn!(
                "  [dm-default] agent '{}' has no default workspace",
                agent.id
            );
            return None;
        }
    };

    Some(ResolvedDmDefaultAgent {
        role_binding: role_binding_from_agent(agent, agent_provider),
        workspace,
    })
}

fn meeting_agent_from_entry(
    config: &Config,
    entry: &MeetingAgentEntry,
) -> Option<MeetingAgentConfig> {
    match entry {
        MeetingAgentEntry::RoleId(role_id) => {
            let agent = config.agents.iter().find(|agent| agent.id == *role_id);
            Some(MeetingAgentConfig {
                role_id: role_id.clone(),
                display_name: agent
                    .map(agent_display_name)
                    .unwrap_or_else(|| role_id.clone()),
                keywords: agent
                    .map(|agent| agent.keywords.clone())
                    .unwrap_or_default(),
                prompt_file: default_prompt_path(role_id).unwrap_or_default(),
                domain_summary: None,
                strengths: Vec::new(),
                task_types: Vec::new(),
                anti_signals: Vec::new(),
                provider_hint: agent.map(|agent| agent.provider.clone()),
                provider: agent.and_then(|agent| ProviderKind::from_str(&agent.provider)),
                model: None,
                reasoning_effort: None,
                workspace: default_workspace(role_id),
                peer_agents_enabled: true,
                memory: resolve_memory_settings(None, None),
            })
        }
        MeetingAgentEntry::Detailed(def) => {
            let agent = config.agents.iter().find(|agent| agent.id == def.role_id);
            Some(MeetingAgentConfig {
                role_id: def.role_id.clone(),
                display_name: def
                    .display_name
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| agent.map(agent_display_name))
                    .unwrap_or_else(|| def.role_id.clone()),
                keywords: if def.keywords.is_empty() {
                    agent
                        .map(|agent| agent.keywords.clone())
                        .unwrap_or_default()
                } else {
                    def.keywords.clone()
                },
                prompt_file: def
                    .prompt_file
                    .as_deref()
                    .map(expand_tilde)
                    .or_else(|| default_prompt_path(&def.role_id))
                    .unwrap_or_default(),
                domain_summary: def.domain_summary.clone(),
                strengths: def.strengths.clone(),
                task_types: def.task_types.clone(),
                anti_signals: def.anti_signals.clone(),
                provider_hint: def
                    .provider_hint
                    .clone()
                    .or_else(|| agent.map(|agent| agent.provider.clone())),
                provider: agent.and_then(|agent| ProviderKind::from_str(&agent.provider)),
                model: None,
                reasoning_effort: None,
                workspace: default_workspace(&def.role_id),
                peer_agents_enabled: true,
                memory: resolve_memory_settings(None, None),
            })
        }
    }
}

pub(super) fn resolve_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleBinding> {
    let config = load_agentdesk_config()?;
    let (agent, provider_key, channel) = find_channel_binding(&config, channel_id, channel_name)?;
    Some(role_binding_from_channel(agent, provider_key, channel))
}

/// Resolve the prompt-cache TTL bucket (#1088) for a Discord channel based on
/// the configured `cache_ttl_minutes` field on its `AgentChannelConfig`.
/// Returns the normalized minutes value (5 or 60) or `None` for the default.
pub(crate) fn resolve_cache_ttl_minutes(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<u32> {
    let config = load_agentdesk_config()?;
    let (_agent, _provider_key, channel) = find_channel_binding(&config, channel_id, channel_name)?;
    channel.cache_ttl_minutes()
}

pub(super) fn resolve_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    let config = load_agentdesk_config()?;
    let (agent, _provider_key, channel) = find_channel_binding(&config, channel_id, channel_name)?;
    channel
        .workspace()
        .map(|value| expand_tilde(&value))
        .or_else(|| default_workspace(&agent.id))
}

pub(super) fn load_shared_prompt_path() -> Option<String> {
    load_agentdesk_config()
        .and_then(|config| config.shared_prompt.as_deref().map(expand_tilde))
        .or_else(shared_prompt_fallback_path)
}

pub(super) fn load_discord_bot_configs() -> Vec<ResolvedDiscordBotConfig> {
    let Some(config) = load_agentdesk_config() else {
        return Vec::new();
    };

    let owner_id = config.discord.owner_id;
    let mut bots = config
        .discord
        .bots
        .into_iter()
        .filter_map(|(name, bot)| {
            let token = resolve_bot_token(&name, &bot)?;
            Some(ResolvedDiscordBotConfig {
                name,
                token,
                provider: bot
                    .provider
                    .as_deref()
                    .and_then(crate::services::provider::ProviderKind::from_str),
                agent: bot.agent,
                auth: bot.auth,
                description: bot.description,
                owner_id,
            })
        })
        .collect::<Vec<_>>();
    bots.sort_by(|left, right| left.name.cmp(&right.name));
    bots
}

pub(super) fn find_discord_bot_by_token(token: &str) -> Option<ResolvedDiscordBotConfig> {
    load_discord_bot_configs()
        .into_iter()
        .find(|bot| bot.token == token)
}

/// Collect bot names that are actually referenced by agent channel configs.
/// Only these bots should be launched as full agent bots via `run_bot()`.
/// Utility bots (e.g. announce, notify) that aren't mapped to any agent channel
/// are excluded, preventing them from processing agent messages.
///
/// Supported launchable shapes:
/// - legacy provider-named bots (`claude`, `codex`, ...)
/// - onboarding-managed alias bots (`command`, `command_2`, ...)
/// - bots explicitly scoped to agent channel IDs
/// - bots explicitly pinned to a concrete agent id
pub(super) fn collect_agent_bot_names() -> HashSet<String> {
    let Some(config) = load_agentdesk_config() else {
        return HashSet::new();
    };

    let mut provider_keys = HashSet::new();
    let mut agent_ids = HashSet::new();
    let mut concrete_channel_ids = HashSet::new();
    for agent in &config.agents {
        agent_ids.insert(agent.id.trim().to_ascii_lowercase());
        for (provider_key, channel) in agent.channels.iter() {
            let Some(channel) = channel else {
                continue;
            };
            if channel.target().is_some() {
                provider_keys.insert(provider_key.to_ascii_lowercase());
            }
            if let Some(channel_id) = channel
                .channel_id()
                .and_then(|value| value.parse::<u64>().ok())
            {
                concrete_channel_ids.insert(channel_id);
            }
        }
    }

    config
        .discord
        .bots
        .iter()
        .filter_map(|(name, bot)| {
            let normalized_name = name.trim().to_ascii_lowercase();
            let normalized_provider = bot
                .provider
                .as_deref()
                .map(|value| value.trim().to_ascii_lowercase());
            let normalized_agent = bot
                .agent
                .as_deref()
                .map(|value| value.trim().to_ascii_lowercase());
            let is_provider_named_bot = provider_keys.contains(&normalized_name);
            let is_command_alias = matches!(
                normalized_provider.as_deref(),
                Some(provider) if provider_keys.contains(provider)
            ) && (normalized_name == "command"
                || normalized_name.starts_with("command_"));
            let has_explicit_agent = normalized_agent
                .as_deref()
                .is_some_and(|agent_id| agent_ids.contains(agent_id));
            let has_agent_channel_allowlist = bot
                .auth
                .allowed_channel_ids
                .as_ref()
                .is_some_and(|ids| ids.iter().any(|id| concrete_channel_ids.contains(id)));

            (is_provider_named_bot
                || is_command_alias
                || has_explicit_agent
                || has_agent_channel_allowlist)
                .then(|| name.clone())
        })
        .collect()
}

pub(super) fn is_known_agent(role_id: &str) -> Option<bool> {
    let config = load_agentdesk_config()?;
    Some(config.agents.iter().any(|agent| agent.id == role_id))
}

pub(super) fn load_peer_agents() -> Vec<PeerAgentInfo> {
    let Some(config) = load_agentdesk_config() else {
        return Vec::new();
    };

    if let Some(meeting) = &config.meeting {
        let available_agents = &meeting.available_agents;
        if !available_agents.is_empty() {
            let mut peers = Vec::new();
            let mut seen = HashSet::new();
            for entry in available_agents {
                let Some(agent) = meeting_agent_from_entry(&config, entry) else {
                    continue;
                };
                if !seen.insert(agent.role_id.clone()) {
                    continue;
                }
                peers.push(PeerAgentInfo {
                    role_id: agent.role_id,
                    display_name: agent.display_name,
                    keywords: agent.keywords,
                });
            }
            return peers;
        }
    }

    let mut peers = config
        .agents
        .iter()
        .map(|agent| PeerAgentInfo {
            role_id: agent.id.clone(),
            display_name: agent_display_name(agent),
            keywords: agent.keywords.clone(),
        })
        .collect::<Vec<_>>();
    peers.sort_by(|left, right| left.role_id.cmp(&right.role_id));
    peers
}

pub(super) fn load_meeting_config() -> Option<MeetingConfig> {
    let (config, config_path) = load_agentdesk_config_with_path()?;
    let meeting = config.meeting.as_ref()?;
    let summary_agent = match meeting.summary_agent.as_ref()? {
        MeetingSummaryAgentDef::Static(agent) => SummaryAgentConfig::Static(agent.clone()),
        MeetingSummaryAgentDef::Dynamic { rules, default } => SummaryAgentConfig::Dynamic {
            rules: rules
                .iter()
                .map(|rule| SummaryAgentRule {
                    keywords: rule.keywords.clone(),
                    agent: rule.agent.clone(),
                })
                .collect(),
            default: default.clone(),
        },
    };

    let explicit_empty_available_agents =
        meeting_available_agents_explicitly_empty(config_path.as_path());
    let available_agents =
        if meeting.available_agents.is_empty() && !explicit_empty_available_agents {
            config
                .agents
                .iter()
                .map(|agent| MeetingAgentConfig {
                    role_id: agent.id.clone(),
                    display_name: agent_display_name(agent),
                    keywords: agent.keywords.clone(),
                    prompt_file: default_prompt_path(&agent.id).unwrap_or_default(),
                    domain_summary: None,
                    strengths: Vec::new(),
                    task_types: Vec::new(),
                    anti_signals: Vec::new(),
                    provider_hint: Some(agent.provider.clone()),
                    provider: ProviderKind::from_str(&agent.provider),
                    model: None,
                    reasoning_effort: None,
                    workspace: default_workspace(&agent.id),
                    peer_agents_enabled: true,
                    memory: resolve_memory_settings(None, None),
                })
                .collect()
        } else {
            meeting
                .available_agents
                .iter()
                .filter_map(|entry| meeting_agent_from_entry(&config, entry))
                .collect()
        };

    Some(MeetingConfig {
        channel_name: meeting.channel_name.clone(),
        max_rounds: meeting.max_rounds.unwrap_or(3),
        max_participants: meeting.max_participants.unwrap_or(5).clamp(2, 5),
        summary_agent,
        available_agents,
    })
}

pub(super) fn list_registered_channel_bindings() -> Vec<RegisteredChannelBinding> {
    let Some(config) = load_agentdesk_config() else {
        return Vec::new();
    };

    let mut bindings = BTreeMap::<u64, RegisteredChannelBinding>::new();
    for agent in &config.agents {
        for (provider_key, maybe_channel) in agent.channels.iter() {
            let Some(channel) = maybe_channel else {
                continue;
            };
            let Some(channel_id) = channel
                .channel_id()
                .and_then(|value| value.parse::<u64>().ok())
            else {
                continue;
            };
            let Some(owner_provider) =
                binding_provider(agent, provider_key, channel).filter(ProviderKind::is_supported)
            else {
                continue;
            };
            let fallback_name = channel
                .channel_name()
                .or_else(|| channel.aliases().into_iter().next());
            bindings.insert(
                channel_id,
                RegisteredChannelBinding {
                    channel_id,
                    owner_provider,
                    fallback_name,
                },
            );
        }
    }

    bindings.into_values().collect()
}

pub(crate) fn resolve_channel_alias(alias: &str) -> Option<u64> {
    let alias = alias.trim();
    if alias.is_empty() {
        return None;
    }

    let config = load_agentdesk_config()?;
    for agent in &config.agents {
        for (_provider_key, maybe_channel) in agent.channels.iter() {
            let Some(channel) = maybe_channel else {
                continue;
            };
            if !channel.aliases().iter().any(|candidate| candidate == alias) {
                continue;
            }
            if let Some(id) = channel
                .channel_id()
                .and_then(|value| value.parse::<u64>().ok())
            {
                return Some(id);
            }
        }
    }
    None
}

pub(crate) fn configured_workspaces() -> Vec<String> {
    let Some(config) = load_agentdesk_config() else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut workspaces = Vec::new();
    for agent in &config.agents {
        for (_provider_key, maybe_channel) in agent.channels.iter() {
            let Some(channel) = maybe_channel else {
                continue;
            };
            let Some(workspace) = channel
                .workspace()
                .map(|value| expand_tilde(&value))
                .or_else(|| default_workspace(&agent.id))
            else {
                continue;
            };
            if seen.insert(workspace.clone()) {
                workspaces.push(workspace);
            }
        }
    }
    workspaces
}

#[cfg(test)]
mod tests {
    use std::fs;

    use poise::serenity_prelude::ChannelId;
    use tempfile::TempDir;

    use super::*;

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
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    fn write_agentdesk_yaml(dir: &std::path::Path, content: &str) {
        let settings_dir = dir.join(".adk").join("config");
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(settings_dir.join("agentdesk.yaml"), content).unwrap();
    }

    #[test]
    fn resolve_role_binding_reads_detailed_channel_config() {
        with_temp_root(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home.path(),
                r#"
server:
  port: 8791
agents:
  - id: project-agentdesk
    name: "AgentDesk"
    provider: codex
    channels:
      codex:
        id: "1479671301387059200"
        name: "adk-cdx"
        aliases: ["adk-cdx-alt"]
        prompt_file: "~/.adk/release/config/agents/project-agentdesk.prompt.md"
        workspace: "~/.adk/release/workspaces/agentdesk"
        provider: codex
"#,
            );

            let binding =
                resolve_role_binding(ChannelId::new(1479671301387059200), Some("adk-cdx"))
                    .expect("binding");
            assert_eq!(binding.role_id, "project-agentdesk");
            assert_eq!(binding.provider, Some(ProviderKind::Codex));
            assert!(binding.prompt_file.ends_with("project-agentdesk.prompt.md"));

            let workspace = resolve_workspace(ChannelId::new(1479671301387059200), Some("adk-cdx"))
                .expect("workspace");
            assert!(workspace.ends_with("/workspaces/agentdesk"));
        });
    }

    #[test]
    fn resolve_role_binding_does_not_match_different_explicit_channel_id_by_name() {
        with_temp_root(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home.path(),
                r#"
server:
  port: 8791
agents:
  - id: project-agentdesk
    name: "AgentDesk"
    provider: claude
    channels:
      claude:
        id: "1484070499783803081"
        name: "adk-cc"
        aliases: ["agentdesk-cc"]
"#,
            );

            assert!(
                resolve_role_binding(ChannelId::new(1479671298497183835), Some("adk-cc")).is_none()
            );
            assert!(
                resolve_workspace(ChannelId::new(1479671298497183835), Some("adk-cc")).is_none()
            );
        });
    }

    #[test]
    fn resolve_channel_alias_reads_primary_name_and_aliases() {
        with_temp_root(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home.path(),
                r#"
server:
  port: 8791
agents:
  - id: adk-dashboard
    name: "Dashboard"
    provider: claude
    channels:
      claude:
        id: "1490141479707086938"
        name: "adk-dash-cc"
        aliases: ["adk-dash-main"]
"#,
            );

            assert_eq!(
                resolve_channel_alias("adk-dash-cc"),
                Some(1490141479707086938)
            );
            assert_eq!(
                resolve_channel_alias("adk-dash-main"),
                Some(1490141479707086938)
            );
        });
    }

    #[test]
    fn resolve_dm_default_agent_reads_matching_provider_channel() {
        with_temp_root(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home.path(),
                r#"
server:
  port: 8791
discord:
  dm_default_agent: family-counsel
agents:
  - id: family-counsel
    name: "상담봇"
    provider: claude
    channels:
      claude:
        id: "1473922824350601297"
        name: "윤호네비서"
        prompt_file: "~/.adk/release/config/agents/family-counsel.prompt.md"
        workspace: "~/.adk/release/workspaces/family-counsel"
        provider: claude
"#,
            );

            let resolved =
                resolve_dm_default_agent(&ProviderKind::Claude).expect("dm default agent");
            assert_eq!(resolved.role_binding.role_id, "family-counsel");
            assert_eq!(resolved.role_binding.provider, Some(ProviderKind::Claude));
            assert!(
                resolved
                    .role_binding
                    .prompt_file
                    .ends_with("/config/agents/family-counsel.prompt.md")
            );
            assert!(resolved.workspace.ends_with("/workspaces/family-counsel"));
        });
    }

    #[test]
    fn resolve_dm_default_agent_skips_mismatched_provider() {
        with_temp_root(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home.path(),
                r#"
server:
  port: 8791
discord:
  dm_default_agent: family-counsel
agents:
  - id: family-counsel
    name: "상담봇"
    provider: claude
    channels:
      claude:
        id: "1473922824350601297"
        name: "윤호네비서"
        workspace: "~/.adk/release/workspaces/family-counsel"
        provider: claude
"#,
            );

            assert!(resolve_dm_default_agent(&ProviderKind::Codex).is_none());
        });
    }

    #[test]
    fn load_shared_prompt_path_prefers_yaml_then_runtime_fallback() {
        with_temp_root(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home.path(),
                r#"
server:
  port: 8791
shared_prompt: "~/.adk/release/config/agents/_shared.prompt.md"
agents: []
"#,
            );
            let shared = load_shared_prompt_path().expect("shared prompt path");
            assert!(shared.ends_with("/config/agents/_shared.prompt.md"));
        });
    }

    #[test]
    fn load_meeting_config_reads_yaml_section() {
        with_temp_root(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home.path(),
                r#"
server:
  port: 8791
agents:
  - id: ch-td
    name: "TD"
    keywords: ["코드", "아키텍처"]
  - id: ch-pd
    name: "PD"
    keywords: ["제품"]
meeting:
  channel_name: "round-table"
  max_rounds: 4
  summary_agent:
    default: "ch-td"
    rules:
      - keywords: ["제품"]
        agent: "ch-pd"
  available_agents:
    - role_id: "ch-td"
      display_name: "TD (테크니컬 디렉터)"
    - "ch-pd"
"#,
            );

            let meeting = load_meeting_config().expect("meeting config");
            assert_eq!(meeting.channel_name, "round-table");
            assert_eq!(meeting.max_rounds, 4);
            assert_eq!(meeting.available_agents.len(), 2);
            assert_eq!(meeting.summary_agent.resolve("제품 회의"), "ch-pd");
            assert_eq!(
                meeting.available_agents[0].display_name,
                "TD (테크니컬 디렉터)"
            );
        });
    }

    #[test]
    fn load_meeting_config_preserves_explicit_empty_available_agents() {
        with_temp_root(|temp_home: &TempDir| {
            write_agentdesk_yaml(
                temp_home.path(),
                r#"
server:
  port: 8791
agents:
  - id: ch-td
    name: "TD"
  - id: ch-pd
    name: "PD"
meeting:
  channel_name: "round-table"
  summary_agent: "ch-td"
  available_agents: []
"#,
            );

            let meeting = load_meeting_config().expect("meeting config");
            assert!(meeting.available_agents.is_empty());
        });
    }
}
