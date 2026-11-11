//! Existing org auth projections, including the org loader's recovery registration.
use std::collections::HashMap;

use super::fallback::FallbackPolicy;
use crate::domain::provider::ProviderKind;
use crate::domain::provider_auth_profile::{ProfileBinding, ProviderAuthProfileDef};
use crate::services::discord::org_schema::{
    OrgSchema, configured_auth_profile, load_org_schema, provider_primary_profile,
};

pub(crate) fn provider_auth_catalog() -> HashMap<String, ProviderAuthProfileDef> {
    load_org_schema()
        .and_then(|schema| schema.provider_auth_profiles)
        .unwrap_or_default()
}

pub(crate) fn provider_auth_primary_profiles() -> HashMap<String, String> {
    load_org_schema()
        .and_then(|schema| schema.provider_auth_primary_profiles)
        .unwrap_or_default()
}

pub(crate) fn provider_auth_fallback_policies() -> HashMap<String, FallbackPolicy> {
    load_org_schema()
        .map(|schema| schema.provider_auth_fallbacks)
        .unwrap_or_default()
}

pub(crate) fn list_profile_bindings() -> Vec<ProfileBinding> {
    let Some(schema) = load_org_schema() else {
        return Vec::new();
    };
    list_profile_bindings_from_schema(&schema)
}

pub(crate) fn list_profile_bindings_from_schema(schema: &OrgSchema) -> Vec<ProfileBinding> {
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
