use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use super::runtime_store::org_schema_path_for_root;

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
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, Value>,
}

fn default_org_version() -> u32 {
    1
}

#[allow(dead_code)]
pub(crate) fn merge_org_agents(
    runtime_root: &Path,
    updates: &[OrgAgentUpdate],
    overwrite: bool,
) -> Result<String, String> {
    merge_org_updates(runtime_root, updates, &[], overwrite)
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

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_yaml::Value;
    use tempfile::tempdir;

    use super::{OrgAgentUpdate, OrgChannelBindingUpdate, merge_org_updates};

    #[test]
    fn merge_org_agents_preserves_existing_sections() {
        let runtime = tempdir().unwrap();
        let config_dir = runtime.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("org.yaml"),
            r#"version: 1
prompts_root: ~/.adk/prompts
channels:
  by_id:
    "123":
      agent: existing
agents:
  existing:
    display_name: Existing
    provider: claude
"#,
        )
        .unwrap();

        let rendered = merge_org_updates(
            runtime.path(),
            &[OrgAgentUpdate {
                role_id: "alpha".to_string(),
                display_name: "Alpha".to_string(),
                prompt_file: Some("/tmp/alpha.md".to_string()),
                provider: Some("codex".to_string()),
                model: Some("gpt-5.4".to_string()),
                workspace: Some("/tmp/ws".to_string()),
            }],
            &[OrgChannelBindingUpdate {
                channel_id: "555".to_string(),
                agent: "alpha".to_string(),
                workspace: Some("/tmp/ws".to_string()),
                provider: Some("codex".to_string()),
                model: Some("gpt-5.4".to_string()),
            }],
            true,
        )
        .unwrap();

        assert!(rendered.contains("prompts_root: ~/.adk/prompts"));
        let document: Value = serde_yaml::from_str(&rendered).unwrap();
        assert_eq!(document["channels"]["by_id"]["123"]["agent"], "existing");
        assert_eq!(document["agents"]["existing"]["display_name"], "Existing");
        assert_eq!(document["agents"]["alpha"]["display_name"], "Alpha");
        assert_eq!(document["agents"]["alpha"]["provider"], "codex");
        assert_eq!(document["channels"]["by_id"]["555"]["agent"], "alpha");
    }
}
