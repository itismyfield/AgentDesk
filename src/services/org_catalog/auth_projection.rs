//! Owned auth values: callers cannot retain a complete org document.
use super::document::OrgSchema;
use crate::services::provider_auth_profile::{ProviderAuthProfileDef, fallback::FallbackPolicy};
use std::collections::HashMap;

pub(crate) type PrimaryProfileMap = HashMap<String, String>;

#[derive(Debug)]
pub(crate) struct ProfileBindingInput {
    pub agent_id: String,
    pub provider: Option<String>,
    pub agent_profile: Option<String>,
    pub channel_profile: Option<String>,
    pub channel_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct AuthCatalogSnapshot {
    pub profiles: HashMap<String, ProviderAuthProfileDef>,
    pub primary_profiles: PrimaryProfileMap,
    pub fallback_policies: HashMap<String, FallbackPolicy>,
    pub bindings: Vec<ProfileBindingInput>,
}

impl From<&OrgSchema> for AuthCatalogSnapshot {
    fn from(schema: &OrgSchema) -> Self {
        let mut bindings = Vec::new();
        for (agent_id, def) in &schema.agents {
            bindings.push(ProfileBindingInput {
                agent_id: agent_id.clone(),
                provider: def.provider.clone(),
                agent_profile: def.auth_profile.clone(),
                channel_profile: None,
                channel_id: None,
            });
        }
        if let Some(by_id) = schema
            .channels
            .as_ref()
            .and_then(|channels| channels.by_id.as_ref())
        {
            for (channel_id, binding) in by_id {
                let agent = schema.agents.get(&binding.agent);
                bindings.push(ProfileBindingInput {
                    agent_id: binding.agent.clone(),
                    provider: binding
                        .provider
                        .clone()
                        .or_else(|| agent.and_then(|def| def.provider.clone())),
                    agent_profile: agent.and_then(|def| def.auth_profile.clone()),
                    channel_profile: binding.auth_profile.clone(),
                    channel_id: Some(channel_id.clone()),
                });
            }
        }
        Self {
            profiles: schema.provider_auth_profiles.clone().unwrap_or_default(),
            primary_profiles: schema
                .provider_auth_primary_profiles
                .clone()
                .unwrap_or_default(),
            fallback_policies: schema.provider_auth_fallbacks.clone(),
            bindings,
        }
    }
}

pub(crate) fn configured_auth_profile<'a>(
    channel: Option<&'a str>,
    agent: Option<&'a str>,
) -> Option<&'a str> {
    let input = crate::domain::provider_auth_profile::ProfileOverrides::new(channel, agent);
    input.channel.or(input.agent)
}

pub(crate) fn provider_primary_profile(
    profiles: Option<&PrimaryProfileMap>,
    provider: Option<&str>,
) -> String {
    provider
        .and_then(|provider| {
            let provider = provider.to_ascii_lowercase();
            profiles.and_then(|profiles| profiles.get(&provider))
        })
        .cloned()
        .unwrap_or_else(|| "default".to_string())
}

impl From<OrgSchema> for AuthCatalogSnapshot {
    fn from(schema: OrgSchema) -> Self {
        Self::from(&schema)
    }
}
