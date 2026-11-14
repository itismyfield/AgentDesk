use poise::serenity_prelude::ChannelId;

use super::{RoleProjection, resolve_agent_memory_settings, resolve_memory_settings};
use crate::config::{AgentChannel, Config};
use crate::services::org_catalog::{
    AgentDef, ChannelBinding, OrgSchema, configured_auth_profile, provider_primary_profile,
};
use crate::services::provider::ProviderKind;
use crate::utils::format::expand_tilde_string as expand_tilde;

pub(crate) fn default_prompt_path(role_id: &str) -> Option<String> {
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

pub(crate) fn default_workspace(role_id: &str) -> Option<String> {
    let root = crate::config::runtime_root()?;
    Some(root.join("workspaces").join(role_id).display().to_string())
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

pub(crate) fn find_channel_binding<'a>(
    config: &'a Config,
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<(&'a crate::config::AgentDef, &'static str, &'a AgentChannel)> {
    let mut best: Option<(&crate::config::AgentDef, &'static str, &AgentChannel, u8)> = None;

    for agent in &config.agents {
        for (provider_key, channel) in agent.channels.iter() {
            let Some(provider_key) = crate::services::provider::intern_provider_id(provider_key)
            else {
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

pub(crate) fn binding_provider(
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

pub(crate) fn role_projection_from_channel(
    agent: &crate::config::AgentDef,
    provider_key: &str,
    channel: &AgentChannel,
) -> RoleProjection {
    RoleProjection {
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
        auth_profile: "default".to_string(),
        memory: resolve_agent_memory_settings(Some(agent)),
    }
}

pub(crate) fn config_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleProjection> {
    let config = crate::config::runtime_config_optional()?;
    let (agent, provider_key, channel) = find_channel_binding(&config, channel_id, channel_name)?;
    Some(role_projection_from_channel(agent, provider_key, channel))
}

pub(crate) fn config_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    let config = crate::config::runtime_config_optional()?;
    let (agent, _provider_key, channel) = find_channel_binding(&config, channel_id, channel_name)?;
    channel
        .workspace()
        .map(|value| expand_tilde(&value))
        .or_else(|| default_workspace(&agent.id))
}

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

pub(crate) fn org_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleProjection> {
    let schema = super::load_org_document()?;
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
        provider_primary_profile(
            schema.provider_auth_primary_profiles.as_ref(),
            provider.as_ref().map(ProviderKind::as_str),
        )
    });

    Some(RoleProjection {
        role_id: ch_binding.agent.clone(),
        prompt_file,
        provider,
        model,
        reasoning_effort: None,
        peer_agents_enabled,
        auth_profile,
        memory,
    })
}

pub(crate) fn org_workspace(channel_id: ChannelId, channel_name: Option<&str>) -> Option<String> {
    let schema = super::load_org_document()?;
    let (ch_binding, agent_def) = resolve_channel_binding(&schema, channel_id, channel_name)?;

    // Channel-level workspace overrides agent-level default
    let ws = ch_binding
        .workspace
        .as_deref()
        .or(agent_def.workspace.as_deref())?;

    Some(expand_tilde(ws))
}
