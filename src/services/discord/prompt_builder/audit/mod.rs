use chrono::{DateTime, Utc};
use poise::serenity_prelude::ChannelId;

use crate::config::Config;
use crate::services::discord::{SharedData, mailbox_snapshot};
use crate::services::provider::{ProviderKind, session_prompt};
use crate::utils::{format::expand_tilde_path, input_validation::validate_agent_id};

use super::super::{
    agentdesk_config::{default_prompt_path, shared_prompt_fallback_path},
    settings::read_canon_prompt,
};

pub(crate) async fn current_session(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<String> {
    let channel = ChannelId::new(channel_id);
    if let Some((observed_provider, session)) = mailbox_snapshot(shared, channel)
        .await
        .cancel_token
        .as_deref()
        .and_then(session_prompt::observed_binding)
        && observed_provider == *provider
    {
        return Some(session);
    }
    shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .and_then(|state| state.session_id.clone())
}

pub(crate) struct CanonPrompt {
    pub name: &'static str,
    pub path: &'static str,
    pub content: String,
    pub modified_at: Option<DateTime<Utc>>,
}

pub(crate) fn canon(config: &Config, agent_id: &str, provider: &ProviderKind) -> Vec<CanonPrompt> {
    let role = config
        .agents
        .iter()
        .find(|agent| agent.id == agent_id)
        .and_then(|agent| agent.channels.get(provider.as_str()))
        .and_then(crate::config::AgentChannel::prompt_file)
        .or_else(|| {
            validate_agent_id(agent_id).ok()?;
            default_prompt_path(agent_id)
        });
    [
        (
            "shared_agent_rules",
            "agents/_shared.prompt.md",
            config
                .shared_prompt
                .clone()
                .or_else(shared_prompt_fallback_path),
        ),
        ("role_prompt", "agents/role.prompt.md", role),
    ]
    .into_iter()
    .filter_map(|(name, path, filename)| {
        let filename = expand_tilde_path(&filename?);
        let (content, modified) = read_canon_prompt(&filename, name, "full")?;
        Some(CanonPrompt {
            name,
            path,
            content,
            modified_at: modified.map(DateTime::from),
        })
    })
    .collect()
}
