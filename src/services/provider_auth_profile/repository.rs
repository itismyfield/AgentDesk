//! Provider auth persistence. Selection and recovery side effects belong to callers.
use std::path::Path;

use super::{ProviderAuthProfileDef, validate_profile_def};

pub(in crate::services) mod document;
use document::{ORG_PROFILE_WRITE_LOCK, load_org_document, org_yaml_path, persist_org_document};

fn normalized_auth_profile(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(crate) fn set_provider_primary_profile(provider: &str, profile_id: &str) -> Result<(), String> {
    set_provider_primary_profile_at(&org_yaml_path()?, provider, profile_id)
}

pub(crate) fn set_provider_primary_profile_at(
    org_path: &Path,
    provider: &str,
    profile_id: &str,
) -> Result<(), String> {
    let provider = provider.trim().to_ascii_lowercase();
    let profile_id = profile_id.trim();
    let _guard = ORG_PROFILE_WRITE_LOCK
        .lock()
        .map_err(|_| "org profile writer lock poisoned".to_string())?;
    let mut document = load_org_document(org_path)?;
    if profile_id.is_empty() || profile_id == "default" {
        document.provider_auth_primary_profiles.remove(&provider);
        return persist_org_document(org_path, &document);
    }
    let Some(profile) = document.provider_auth_profiles.get(profile_id) else {
        return Err(format!("unknown auth_profile '{profile_id}'"));
    };
    if !profile.provider.eq_ignore_ascii_case(&provider) {
        return Err(format!(
            "auth_profile '{profile_id}' provider '{}' does not match provider '{provider}'",
            profile.provider
        ));
    }
    document
        .provider_auth_primary_profiles
        .insert(provider, profile_id.to_string());
    persist_org_document(org_path, &document)
}

pub(crate) fn append_provider_auth_profile(
    profile_id: &str,
    def: ProviderAuthProfileDef,
) -> Result<(), String> {
    append_provider_auth_profile_at(&org_yaml_path()?, profile_id, def)
}

pub(crate) fn append_provider_auth_profile_at(
    org_path: &Path,
    profile_id: &str,
    def: ProviderAuthProfileDef,
) -> Result<(), String> {
    validate_profile_def(profile_id, &def).map_err(|error| error.to_string())?;
    let _guard = ORG_PROFILE_WRITE_LOCK
        .lock()
        .map_err(|_| "org profile writer lock poisoned".to_string())?;
    let mut document = load_org_document(org_path)?;
    if let Some(existing) = document.provider_auth_profiles.get(profile_id) {
        if existing == &def {
            return Ok(());
        }
        return Err(format!("auth profile '{profile_id}' already exists"));
    }
    document
        .provider_auth_profiles
        .insert(profile_id.to_string(), def);
    persist_org_document(org_path, &document)
}

pub(crate) fn set_agent_auth_profile(
    role_id: &str,
    auth_profile: Option<&str>,
) -> Result<(), String> {
    set_agent_auth_profile_at(&org_yaml_path()?, role_id, auth_profile)
}

pub(crate) fn set_agent_auth_profile_at(
    org_path: &Path,
    role_id: &str,
    auth_profile: Option<&str>,
) -> Result<(), String> {
    let _guard = ORG_PROFILE_WRITE_LOCK
        .lock()
        .map_err(|_| "org profile writer lock poisoned".to_string())?;
    let mut document = load_org_document(org_path)?;
    if !document.agents.contains_key(role_id) {
        return Err(format!(
            "agent '{role_id}' not found in '{}'",
            org_path.display()
        ));
    }
    let next = match normalized_auth_profile(auth_profile) {
        Some(profile_id) if profile_id == "default" => Some(profile_id),
        Some(profile_id) => {
            let Some(profile) = document.provider_auth_profiles.get(&profile_id) else {
                return Err(format!("unknown auth_profile '{profile_id}'"));
            };
            let Some(agent) = document.agents.get(role_id) else {
                return Err(format!("agent '{role_id}' not found"));
            };
            if let Some(agent_provider) = agent.provider.as_deref()
                && !profile.provider.eq_ignore_ascii_case(agent_provider)
            {
                return Err(format!(
                    "auth_profile '{profile_id}' provider '{}' does not match agent '{role_id}' provider '{agent_provider}'",
                    profile.provider
                ));
            }
            Some(profile_id)
        }
        None => None,
    };
    if let Some(agent) = document.agents.get_mut(role_id) {
        agent.auth_profile = next;
    }
    persist_org_document(org_path, &document)
}

pub(crate) fn set_channel_auth_profile(
    channel_id: &str,
    auth_profile: Option<&str>,
) -> Result<(), String> {
    set_channel_auth_profile_at(&org_yaml_path()?, channel_id, auth_profile)
}

pub(crate) fn set_channel_auth_profile_at(
    org_path: &Path,
    channel_id: &str,
    auth_profile: Option<&str>,
) -> Result<(), String> {
    let _guard = ORG_PROFILE_WRITE_LOCK
        .lock()
        .map_err(|_| "org profile writer lock poisoned".to_string())?;
    let mut document = load_org_document(org_path)?;
    let has_binding = document
        .channels
        .as_ref()
        .is_some_and(|channels| channels.by_id.contains_key(channel_id));
    if !has_binding {
        return Err(format!(
            "channel '{channel_id}' not found in '{}'",
            org_path.display()
        ));
    }
    let next = match normalized_auth_profile(auth_profile) {
        Some(profile_id) if profile_id == "default" => Some(profile_id),
        Some(profile_id) => {
            let Some(profile) = document.provider_auth_profiles.get(&profile_id) else {
                return Err(format!("unknown auth_profile '{profile_id}'"));
            };
            let binding = document
                .channels
                .as_ref()
                .and_then(|channels| channels.by_id.get(channel_id))
                .expect("checked channel binding exists");
            let channel_provider = binding.provider.as_deref().or_else(|| {
                document
                    .agents
                    .get(binding.agent.as_str())
                    .and_then(|agent| agent.provider.as_deref())
            });
            if let Some(channel_provider) = channel_provider
                && !profile.provider.eq_ignore_ascii_case(channel_provider)
            {
                return Err(format!(
                    "auth_profile '{profile_id}' provider '{}' does not match channel '{channel_id}' provider '{channel_provider}'",
                    profile.provider
                ));
            }
            Some(profile_id)
        }
        None => None,
    };
    if let Some(binding) = document
        .channels
        .as_mut()
        .and_then(|channels| channels.by_id.get_mut(channel_id))
    {
        binding.auth_profile = next;
    }
    persist_org_document(org_path, &document)
}

/// Removes only an unbound catalog entry, preserving credential material in its managed home.
pub(crate) fn remove_provider_auth_profile(profile_id: &str, provider: &str) -> Result<(), String> {
    remove_provider_auth_profile_at(&org_yaml_path()?, profile_id, provider)
}

pub(crate) fn remove_provider_auth_profile_at(
    org_path: &Path,
    profile_id: &str,
    provider: &str,
) -> Result<(), String> {
    let _guard = ORG_PROFILE_WRITE_LOCK
        .lock()
        .map_err(|_| "org profile writer lock poisoned".to_string())?;
    let mut document = load_org_document(org_path)?;
    let Some(profile) = document.provider_auth_profiles.get(profile_id) else {
        return Err(format!("auth profile '{profile_id}' not found"));
    };
    if !profile.provider.eq_ignore_ascii_case(provider) {
        return Err(format!(
            "auth profile '{profile_id}' belongs to provider '{}', not '{provider}'",
            profile.provider
        ));
    }
    let bound_agents = document
        .agents
        .iter()
        .filter(|(_, agent)| agent.auth_profile.as_deref() == Some(profile_id))
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    let bound_channels = document
        .channels
        .as_ref()
        .into_iter()
        .flat_map(|channels| channels.by_id.iter())
        .filter(|(_, binding)| binding.auth_profile.as_deref() == Some(profile_id))
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    if !bound_agents.is_empty() || !bound_channels.is_empty() {
        return Err(format!(
            "auth profile '{profile_id}' is still bound to agents [{}] or channels [{}]; select default first",
            bound_agents.join(", "),
            bound_channels.join(", ")
        ));
    }
    if document
        .provider_auth_primary_profiles
        .get(provider)
        .is_some_and(|primary| primary == profile_id)
    {
        return Err(format!(
            "auth profile '{profile_id}' is the provider primary; select another primary first"
        ));
    }
    if document.provider_auth_fallbacks.values().any(|policy| {
        policy.fallback_profile.as_deref() == Some(profile_id)
            || policy.priority.iter().any(|id| id == profile_id)
    }) {
        return Err(format!(
            "auth profile '{profile_id}' is still bound to a fallback policy; detach it first"
        ));
    }
    document.provider_auth_profiles.remove(profile_id);
    persist_org_document(org_path, &document)
}

#[cfg(test)]
use crate::services::org_catalog::read_auth_catalog_at as read_org_schema;
#[cfg(test)]
use std::fs;

#[cfg(test)]
mod tests;
