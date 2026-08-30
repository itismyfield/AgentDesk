use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use super::runtime_store::org_schema_path_for_root;
use crate::services::provider_auth_profile::{ProviderAuthProfileDef, validate_profile_def};

#[derive(Clone, Debug)]
pub(crate) struct OrgAgentUpdate {
    pub(crate) role_id: String,
    pub(crate) display_name: String,
    pub(crate) prompt_file: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) workspace: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct OrgChannelBindingUpdate {
    pub(crate) channel_id: String,
    pub(crate) agent: String,
    pub(crate) workspace: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct OrgDocument {
    #[serde(default = "default_org_version")]
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shared_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompts_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skills_root: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    agents: BTreeMap<String, OrgAgentDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channels: Option<OrgChannelsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meeting: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suffix_map: Option<Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    provider_auth_profiles: BTreeMap<String, ProviderAuthProfileDef>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct OrgAgentDef {
    #[serde(default)]
    display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keywords: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer_agents: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_profile: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct OrgChannelsConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    by_id: BTreeMap<String, OrgChannelBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    by_name: Option<Value>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct OrgChannelBinding {
    #[serde(default)]
    agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer_agents: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_profile: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, Value>,
}

fn default_org_version() -> u32 {
    1
}

pub(crate) fn merge_org_updates(
    runtime_root: &Path,
    agent_updates: &[OrgAgentUpdate],
    channel_updates: &[OrgChannelBindingUpdate],
    overwrite: bool,
) -> Result<String, String> {
    let org_path = org_schema_path_for_root(runtime_root);
    let mut document = load_org_document(&org_path)?;

    for update in agent_updates {
        let existing = document.agents.get(&update.role_id).cloned();
        if existing.is_some() && !overwrite {
            return Err(format!(
                "Target org role '{}' already exists in '{}'. Re-run with --overwrite to replace it.",
                update.role_id,
                org_path.display()
            ));
        }

        let mut agent = existing.unwrap_or_default();
        agent.display_name = update.display_name.clone();
        agent.prompt_file = update.prompt_file.clone();
        agent.provider = update.provider.clone();
        agent.model = update.model.clone();
        agent.workspace = update.workspace.clone();
        document.agents.insert(update.role_id.clone(), agent);
    }

    if !channel_updates.is_empty() {
        let channels = document
            .channels
            .get_or_insert_with(OrgChannelsConfig::default);
        for update in channel_updates {
            let existing = channels.by_id.get(&update.channel_id).cloned();
            if existing.is_some() && !overwrite {
                return Err(format!(
                    "Target org channel binding '{}' already exists in '{}'. Re-run with --overwrite to replace it.",
                    update.channel_id,
                    org_path.display()
                ));
            }

            let mut binding = existing.unwrap_or_default();
            binding.agent = update.agent.clone();
            binding.workspace = update.workspace.clone();
            binding.provider = update.provider.clone();
            binding.model = update.model.clone();
            channels.by_id.insert(update.channel_id.clone(), binding);
        }
    }

    serde_yaml::to_string(&document)
        .map_err(|e| format!("Failed to serialize '{}': {e}", org_path.display()))
}

fn load_org_document(path: &Path) -> Result<OrgDocument, String> {
    if !path.exists() {
        return Ok(OrgDocument {
            version: default_org_version(),
            ..OrgDocument::default()
        });
    }

    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    serde_yaml::from_str(&content).map_err(|e| format!("Failed to parse '{}': {e}", path.display()))
}

fn persist_org_document(path: &Path, document: &OrgDocument) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    let rendered = serde_yaml::to_string(document)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    let tmp = path.with_extension("yaml.tmp");
    fs::write(&tmp, rendered).map_err(|e| format!("Failed to write '{}': {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        format!(
            "Failed to replace '{}' with '{}': {e}",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

fn org_yaml_path() -> Result<PathBuf, String> {
    let root =
        crate::config::runtime_root().ok_or_else(|| "runtime root not configured".to_string())?;
    Ok(org_schema_path_for_root(&root))
}

fn normalized_auth_profile(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "default")
        .map(str::to_string)
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
    let mut document = load_org_document(org_path)?;
    if !document.agents.contains_key(role_id) {
        return Err(format!(
            "agent '{role_id}' not found in '{}'",
            org_path.display()
        ));
    }
    let next = match normalized_auth_profile(auth_profile) {
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

/// Removes only the catalog binding. Credential material remains in the
/// managed profile home so an operator never loses an account by clicking the
/// dashboard's unlink button. A bound profile must be detached first.
pub(crate) fn remove_provider_auth_profile(profile_id: &str, provider: &str) -> Result<(), String> {
    remove_provider_auth_profile_at(&org_yaml_path()?, profile_id, provider)
}

pub(crate) fn remove_provider_auth_profile_at(
    org_path: &Path,
    profile_id: &str,
    provider: &str,
) -> Result<(), String> {
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
    document.provider_auth_profiles.remove(profile_id);
    persist_org_document(org_path, &document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn test_012_catalog_append_round_trips_extra_and_skips_secrets() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        fs::write(
            &path,
            "version: 1\nname: desk\ncustom_flag: true\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n",
        )
        .unwrap();

        append_provider_auth_profile_at(
            &path,
            "work",
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some("~/.adk/profiles/codex/work".into()),
                env: BTreeMap::new(),
            },
        )
        .unwrap();

        let rendered = fs::read_to_string(&path).unwrap();
        assert!(rendered.contains("provider_auth_profiles:"));
        assert!(rendered.contains("work:"));
        assert!(rendered.contains("custom_flag: true"));
        assert!(!rendered.contains("sk-"));
        assert!(!rendered.contains("access_token"));

        set_agent_auth_profile_at(&path, "coder", Some("work")).unwrap();
        let rendered = fs::read_to_string(&path).unwrap();
        assert!(rendered.contains("auth_profile: work"));
        set_agent_auth_profile_at(&path, "coder", None).unwrap();
        let rendered = fs::read_to_string(&path).unwrap();
        assert!(!rendered.contains("auth_profile: work"));
    }

    #[test]
    fn extra_login_does_not_overwrite_missing_catalog_on_unknown_agent() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        let err = set_agent_auth_profile_at(&path, "missing", Some("work")).unwrap_err();
        assert!(err.contains("not found"));
        assert!(
            !path.exists()
                || !fs::read_to_string(&path)
                    .unwrap_or_default()
                    .contains("auth_profile")
        );
    }

    #[test]
    fn named_profile_unlink_requires_detach_and_keeps_catalog_operation_scoped() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        fs::write(
            &path,
            "version: 1\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n",
        )
        .unwrap();
        let profile = ProviderAuthProfileDef {
            provider: "codex".into(),
            home: Some("~/.adk/profiles/codex/work".into()),
            env: BTreeMap::new(),
        };
        append_provider_auth_profile_at(&path, "work", profile.clone()).unwrap();
        set_agent_auth_profile_at(&path, "coder", Some("work")).unwrap();
        let err = remove_provider_auth_profile_at(&path, "work", "codex").unwrap_err();
        assert!(err.contains("still bound"));

        set_agent_auth_profile_at(&path, "coder", None).unwrap();
        remove_provider_auth_profile_at(&path, "work", "codex").unwrap();
        let rendered = fs::read_to_string(&path).unwrap();
        assert!(!rendered.contains("auth_profile: work"));
        assert!(!rendered.contains("\n  work:"));
    }

    #[test]
    fn named_profile_rejects_provider_mismatch_before_persisting_binding() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        fs::write(
            &path,
            "version: 1\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n",
        )
        .unwrap();
        append_provider_auth_profile_at(
            &path,
            "claude-work",
            ProviderAuthProfileDef {
                provider: "claude".into(),
                home: Some("~/.adk/profiles/claude/claude-work".into()),
                env: BTreeMap::new(),
            },
        )
        .unwrap();
        let err = set_agent_auth_profile_at(&path, "coder", Some("claude-work")).unwrap_err();
        assert!(err.contains("does not match"));
    }
}
