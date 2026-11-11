//! Org document storage preserves unknown fields and the existing write lock/publication steps.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::domain::provider_auth_profile::ProviderAuthProfileDef;
use crate::services::discord::runtime_store::org_schema_path_for_root;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct OrgDocument {
    #[serde(default = "default_org_version")]
    pub(crate) version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) shared_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prompts_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) skills_root: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) agents: BTreeMap<String, OrgAgentDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) channels: Option<OrgChannelsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) suffix_map: Option<Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) provider_auth_profiles: BTreeMap<String, ProviderAuthProfileDef>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) provider_auth_primary_profiles: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) provider_auth_fallbacks:
        BTreeMap<String, crate::domain::provider_auth_profile::FallbackPolicy>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct OrgAgentDef {
    #[serde(default)]
    pub(crate) display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) keywords: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) peer_agents: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auth_profile: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct OrgChannelsConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) by_id: BTreeMap<String, OrgChannelBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) by_name: Option<Value>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct OrgChannelBinding {
    #[serde(default)]
    pub(crate) agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) peer_agents: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auth_profile: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) extra: BTreeMap<String, Value>,
}

pub(crate) fn default_org_version() -> u32 {
    1
}

pub(crate) fn load_org_document(path: &Path) -> Result<OrgDocument, String> {
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

pub(crate) static ORG_PROFILE_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn persist_org_document(path: &Path, document: &OrgDocument) -> Result<(), String> {
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

pub(crate) fn org_yaml_path() -> Result<PathBuf, String> {
    let root =
        crate::config::runtime_root().ok_or_else(|| "runtime root not configured".to_string())?;
    Ok(org_schema_path_for_root(&root))
}
