//! Decoding, validation and purpose-specific recovery publication for org reads.
use super::auth_projection::{configured_auth_profile, provider_primary_profile};
use crate::domain::memory::MemoryConfigOverride;
use crate::domain::provider::ProviderKind;
use crate::services::provider_auth_profile::fallback::{self, FallbackPolicyRuntime};
use crate::services::provider_auth_profile::{ProviderAuthProfileDef, validate_catalog};
use serde::Deserialize;
use std::{collections::HashMap, path::Path};

#[derive(Debug, Deserialize)]
pub(crate) struct OrgSchema {
    #[serde(default)]
    pub provider_auth_fallbacks: HashMap<String, fallback::FallbackPolicy>,
    // Retain wire-only fields without adding dead-code suppressions at the new owner.
    #[serde(rename = "version")]
    pub _version: u32,
    #[serde(rename = "name")]
    pub _name: Option<String>,
    #[serde(rename = "shared_prompt")]
    pub _shared_prompt: Option<String>,
    /// Root used to derive an agent's prompt path when prompt_file is absent.
    pub prompts_root: Option<String>,
    #[serde(rename = "skills_root")]
    pub _skills_root: Option<String>,
    pub agents: HashMap<String, AgentDef>,
    pub channels: Option<ChannelsConfig>,
    pub suffix_map: Option<HashMap<String, String>>,
    #[serde(default)]
    pub provider_auth_profiles: Option<HashMap<String, ProviderAuthProfileDef>>,
    #[serde(default)]
    pub provider_auth_primary_profiles: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentDef {
    pub display_name: String,
    pub prompt_file: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub workspace: Option<String>,
    pub peer_agents: Option<bool>,
    #[serde(default)]
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub memory: Option<MemoryConfigOverride>,
    #[serde(default)]
    pub recovery: Option<crate::services::agent_recovery::RecoveryConfigWire>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChannelsConfig {
    pub by_id: Option<HashMap<String, ChannelBinding>>,
    pub by_name: Option<ChannelsByName>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChannelBinding {
    pub agent: String,
    pub workspace: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub peer_agents: Option<bool>,
    #[serde(default)]
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub memory: Option<MemoryConfigOverride>,
    #[serde(default)]
    pub recovery: Option<crate::services::agent_recovery::RecoveryConfigWire>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChannelsByName {
    pub enabled: Option<bool>,
    pub mappings: Option<HashMap<String, ChannelBinding>>,
}

pub(crate) fn parse_org_schema(content: &str) -> Result<OrgSchema, String> {
    let schema: OrgSchema =
        serde_yaml::from_str(content).map_err(|error| format!("org schema yaml: {error}"))?;
    if let Some(catalog) = schema.provider_auth_profiles.as_ref() {
        validate_catalog(catalog).map_err(|error| error.to_string())?;
    }
    for (name, policy) in &schema.provider_auth_fallbacks {
        let provider = crate::services::provider_auth_profile::intern_provider(name)
            .map_err(|error| error.to_string())?;
        if name != provider.as_str() {
            return Err(format!(
                "profile fallback provider must use canonical name '{}'",
                provider.as_str()
            ));
        }
        policy.validate(
            &provider,
            &schema.provider_auth_profiles.clone().unwrap_or_default(),
        )?;
    }
    if let Some(primary) = schema.provider_auth_primary_profiles.as_ref() {
        let catalog = schema
            .provider_auth_profiles
            .as_ref()
            .cloned()
            .unwrap_or_default();
        for (provider, profile_id) in primary {
            let kind = ProviderKind::from_str(provider)
                .ok_or_else(|| format!("unknown primary profile provider '{provider}'"))?;
            if profile_id == "default" {
                continue;
            }
            let profile = catalog.get(profile_id).ok_or_else(|| {
                format!(
                    "provider primary '{provider}' references unknown auth_profile '{profile_id}'"
                )
            })?;
            if !profile.provider.eq_ignore_ascii_case(kind.as_str()) {
                return Err(format!(
                    "provider primary '{provider}' auth_profile '{profile_id}' belongs to '{}'",
                    profile.provider
                ));
            }
        }
    }
    Ok(schema)
}

pub(in crate::services) fn read_document_at(path: &Path) -> Result<Option<OrgSchema>, String> {
    crate::config::test_instance::environment::validate_path(path)?;
    match std::fs::read_to_string(path) {
        Ok(content) => parse_org_schema(&content)
            .map(Some)
            .map_err(|error| format!("org schema rejected: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read org schema '{}': {error}", path.display())),
    }
}

trait RecoveryEffects {
    fn install(&mut self, publish: impl FnOnce());
    fn clear(&mut self);
}

struct LiveRecovery;
impl RecoveryEffects for LiveRecovery {
    fn install(&mut self, publish: impl FnOnce()) {
        publish();
    }
    fn clear(&mut self) {
        crate::services::agent_recovery::clear_catalog();
    }
}

pub(in crate::services) fn load_runtime_document_at(path: Option<&Path>) -> Option<OrgSchema> {
    load_runtime_with(path, &mut LiveRecovery)
}

fn load_runtime_with(path: Option<&Path>, effects: &mut impl RecoveryEffects) -> Option<OrgSchema> {
    let loaded = path.map(read_document_at).transpose().map(Option::flatten);
    match loaded {
        Ok(Some(schema)) => match install_org_recovery(&schema, effects) {
            Ok(()) => Some(schema),
            Err(error) => {
                tracing::error!(error = %error, "org.yaml recovery policy rejected");
                effects.clear();
                None
            }
        },
        Ok(None) => {
            effects.clear();
            None
        }
        Err(error) => {
            effects.clear();
            tracing::error!("{error}");
            None
        }
    }
}

fn install_org_recovery(
    schema: &OrgSchema,
    effects: &mut impl RecoveryEffects,
) -> Result<(), crate::services::agent_recovery::PolicyError> {
    let agents: Vec<crate::services::agent_recovery::OrgAgentInput> = schema
        .agents
        .iter()
        .map(|(id, def)| crate::services::agent_recovery::OrgAgentInput {
            id: id.clone(),
            provider: def.provider.clone(),
            model: def.model.clone(),
            workspace: def.workspace.clone(),
            auth_profile: configured_auth_profile(None, def.auth_profile.as_deref())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    provider_primary_profile(
                        schema.provider_auth_primary_profiles.as_ref(),
                        def.provider.as_deref(),
                    )
                }),
            recovery: def.recovery.clone(),
        })
        .collect();
    let channels: Vec<crate::services::agent_recovery::OrgChannelInput> = schema
        .channels
        .as_ref()
        .and_then(|channels| channels.by_id.as_ref())
        .map(|by_id| {
            by_id
                .iter()
                .map(
                    |(channel_id, binding)| crate::services::agent_recovery::OrgChannelInput {
                        channel_id: channel_id.clone(),
                        agent: binding.agent.clone(),
                        provider: binding.provider.clone(),
                        workspace: binding.workspace.clone(),
                        auth_profile: binding.auth_profile.clone(),
                        recovery: binding.recovery.clone(),
                    },
                )
                .collect()
        })
        .unwrap_or_default();
    let catalog = crate::services::agent_recovery::build_recovery_catalog(&agents, &channels)?;
    effects.install(|| crate::services::agent_recovery::install_catalog(catalog));
    Ok(())
}

pub(in crate::services) fn fallback_candidates(
    policy: &fallback::FallbackPolicy,
    provider: &ProviderKind,
    primary: &str,
    catalog: &HashMap<String, ProviderAuthProfileDef>,
) -> Vec<String> {
    policy.candidates(provider, primary, catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Trace(Vec<&'static str>);
    impl RecoveryEffects for Trace {
        fn install(&mut self, _: impl FnOnce()) {
            self.0.push("install");
        }
        fn clear(&mut self) {
            self.0.push("clear");
        }
    }

    #[test]
    fn runtime_and_auth_reads_preserve_missing_malformed_and_recovery_invalid_effects() {
        let mut publications = 0;
        LiveRecovery.install(|| publications += 1);
        assert_eq!(publications, 1);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("org.yaml");
        let mut trace = Trace::default();
        assert!(load_runtime_with(None, &mut trace).is_none());
        assert_eq!(trace.0, ["clear"]);
        for (content, auth_result, runtime_present, expected) in [
            (None, "missing", false, "clear"),
            (Some("agents: [broken"), "error", false, "clear"),
            (
                Some("version: 1\nagents: {}\nprovider_auth_primary_profiles: {codex: missing}\n"),
                "error",
                false,
                "clear",
            ),
            (
                Some(
                    "version: 1\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n    recovery: {enabled: true, fallback_agent_id: missing}\nchannels:\n  by_id:\n    '42': {agent: coder}\n",
                ),
                "present",
                false,
                "clear",
            ),
            (
                Some(
                    "version: 1\nagents:\n  coder: {display_name: Coder, provider: codex, auth_profile: default}\nchannels:\n  by_id:\n    '42': {agent: coder, auth_profile: default}\nprovider_auth_primary_profiles: {codex: default}\nprovider_auth_fallbacks:\n  codex: {enabled: false}\n",
                ),
                "present",
                true,
                "install",
            ),
        ] {
            if let Some(content) = content {
                std::fs::write(&path, content).unwrap();
            }
            let auth = crate::services::org_catalog::read_auth_catalog_at(&path);
            match auth_result {
                "missing" => assert!(auth.unwrap().is_none()),
                "error" => assert!(auth.is_err()),
                _ => assert!(auth.unwrap().is_some()),
            }
            trace.0.clear();
            let runtime = load_runtime_with(Some(&path), &mut trace);
            assert_eq!(runtime.is_some(), runtime_present, "{content:?}");
            assert_eq!(trace.0, [expected], "{content:?}");
            if let Some(runtime) = runtime {
                let runtime = crate::services::org_catalog::AuthCatalogSnapshot::from(runtime);
                let auth = crate::services::org_catalog::read_auth_catalog_at(&path)
                    .unwrap()
                    .unwrap();
                assert_eq!(runtime.profiles, auth.profiles);
                assert_eq!(runtime.primary_profiles, auth.primary_profiles);
                assert_eq!(runtime.fallback_policies, auth.fallback_policies);
                let bindings = |snapshot: crate::services::org_catalog::AuthCatalogSnapshot| {
                    let mut rows: Vec<_> = snapshot
                        .bindings
                        .into_iter()
                        .map(|b| {
                            (
                                b.agent_id,
                                b.channel_id,
                                b.provider,
                                b.agent_profile,
                                b.channel_profile,
                            )
                        })
                        .collect();
                    rows.sort();
                    rows
                };
                assert_eq!(bindings(runtime), bindings(auth));
            }
        }
        // A directory produces a read error rather than being treated as a missing file.
        trace.0.clear();
        assert!(crate::services::org_catalog::read_auth_catalog_at(dir.path()).is_err());
        assert!(load_runtime_with(Some(dir.path()), &mut trace).is_none());
        assert_eq!(trace.0, ["clear"]);
    }
}
