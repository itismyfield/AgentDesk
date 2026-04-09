use std::collections::HashSet;

use poise::serenity_prelude::ChannelId;

use super::meeting::{MeetingAgentConfig, MeetingConfig, SummaryAgentConfig, SummaryAgentRule};
use super::settings::{PeerAgentInfo, RoleBinding, resolve_memory_settings};
use crate::config::{AgentChannel, Config, MeetingAgentEntry, MeetingSummaryAgentDef};
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

fn load_agentdesk_config() -> Option<Config> {
    let root = crate::config::runtime_root()?;
    for path in [
        crate::runtime_layout::config_file_path(&root),
        crate::runtime_layout::legacy_config_file_path(&root),
    ] {
        if path.is_file() {
            return crate::config::load_from_path(&path).ok();
        }
    }
    None
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
    let channel_target = channel.target();
    if channel.channel_id().as_deref() == Some(channel_id_str.as_str())
        || channel_target.as_deref() == Some(channel_id_str.as_str())
    {
        return Some(2);
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
        memory: resolve_memory_settings(None, None),
    }
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

pub(super) fn is_known_agent(role_id: &str) -> Option<bool> {
    let config = load_agentdesk_config()?;
    Some(config.agents.iter().any(|agent| agent.id == role_id))
}

pub(super) fn load_peer_agents() -> Vec<PeerAgentInfo> {
    let Some(config) = load_agentdesk_config() else {
        return Vec::new();
    };

    if let Some(meeting) = &config.meeting
        && !meeting.available_agents.is_empty()
    {
        let mut peers = Vec::new();
        let mut seen = HashSet::new();
        for entry in &meeting.available_agents {
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
    let config = load_agentdesk_config()?;
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

    let available_agents = if meeting.available_agents.is_empty() {
        config
            .agents
            .iter()
            .map(|agent| MeetingAgentConfig {
                role_id: agent.id.clone(),
                display_name: agent_display_name(agent),
                keywords: agent.keywords.clone(),
                prompt_file: default_prompt_path(&agent.id).unwrap_or_default(),
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
        summary_agent,
        available_agents,
    })
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
}
