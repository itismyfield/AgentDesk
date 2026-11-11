use std::path::Path;

use super::runtime_store::org_schema_path_for_root;
#[cfg(test)]
use crate::services::provider_auth_profile::ProviderAuthProfileDef;
use crate::services::provider_auth_profile::repository::document::{
    OrgChannelsConfig, load_org_document,
};

pub(crate) use crate::services::provider_auth_profile::repository::{
    append_provider_auth_profile, remove_provider_auth_profile, set_agent_auth_profile,
    set_channel_auth_profile, set_provider_primary_profile,
};
#[cfg(test)]
pub(crate) use crate::services::provider_auth_profile::repository::{
    append_provider_auth_profile_at, remove_provider_auth_profile_at, set_agent_auth_profile_at,
    set_channel_auth_profile_at, set_provider_primary_profile_at,
};
#[cfg(test)]
use std::fs;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn fallback_policy_survives_writes_and_protects_referenced_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("org.yaml");
        for field in ["fallback_profile: work", "priority: [work]"] {
            fs::write(
                &path,
                format!(
                    "version: 1\nagents: {{}}\nprovider_auth_fallbacks:\n  codex:\n    {field}\n"
                ),
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
            assert!(
                remove_provider_auth_profile_at(&path, "work", "codex")
                    .unwrap_err()
                    .contains("still bound")
            );
            assert!(
                fs::read_to_string(&path)
                    .unwrap()
                    .contains("provider_auth_fallbacks:")
            );
        }
    }

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

    #[test]
    fn primary_profile_cannot_be_unlinked_until_reset_to_system_default() {
        let dir = tempfile::tempdir().expect("org dir");
        let path = dir.path().join("org.yaml");
        fs::write(&path, "version: 1\nagents: {}\n").unwrap();
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
        set_provider_primary_profile_at(&path, "codex", "work").unwrap();
        let err = remove_provider_auth_profile_at(&path, "work", "codex").unwrap_err();
        assert!(err.contains("provider primary"));
        set_provider_primary_profile_at(&path, "codex", "default").unwrap();
        remove_provider_auth_profile_at(&path, "work", "codex").unwrap();
    }
}
