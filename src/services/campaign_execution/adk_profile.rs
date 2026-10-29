use serde::{Deserialize, Serialize};

use super::definition::PROFILE_VERSION;
use super::model::{Repository, Role};
use super::profile::{EnvironmentComponent, ProfileLimits, RepositoryProfile};
use super::verification_run::{Digest, VerificationSpec, digest_of};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ObservedModel {
    Concrete { model: String },
    ProviderDefault,
    Missing,
}

// One role's settings as actually read; the resolver never guesses a model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSettingObservation {
    pub role: Role,
    pub agent_id: String,
    pub provider: String,
    pub model: ObservedModel,
    pub effort: Option<String>,
    pub account_ref: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapability {
    pub provider: String,
    pub independent_review_conversation: bool,
    pub server_owned_external_effects: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdkSettingsObservation {
    pub roles: Vec<RoleSettingObservation>,
    pub capabilities: Vec<ProviderCapability>,
    pub tool_policy_digest: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRole {
    pub role: Role,
    pub agent_id: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub account_ref: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAdkProfile {
    pub profile_version: String,
    pub profile_hash: Digest,
    pub roles: Vec<ResolvedRole>,
    pub limits: ProfileLimits,
    pub tool_policy_digest: Digest,
    pub verification_spec_hash: Digest,
    pub environment_requirements: Vec<EnvironmentComponent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProfileResolution {
    Resolved { profile: ResolvedAdkProfile },
    Unsupported { role: Option<Role>, reason: String },
}

impl ResolvedAdkProfile {
    pub fn hash(&self) -> Result<Digest, serde_json::Error> {
        digest_of(&(
            &self.profile_version,
            &self.profile_hash,
            &self.roles,
            &self.limits,
            &self.tool_policy_digest,
            &self.verification_spec_hash,
        ))
    }
}

fn unsupported(role: Option<Role>, reason: impl Into<String>) -> ProfileResolution {
    ProfileResolution::Unsupported {
        role,
        reason: reason.into(),
    }
}

// Provider-default or missing models are unsupported, not resolved; U02/U43-U48 close the real values.
pub fn resolve_adk_profile(
    settings: &AdkSettingsObservation,
    spec: &VerificationSpec,
) -> ProfileResolution {
    let base = match RepositoryProfile::for_repository(Repository::Adk) {
        Ok(profile) => profile,
        Err(error) => return unsupported(None, error.to_string()),
    };
    let spec_hash = match spec.hash() {
        Ok(hash) if spec.repository == Repository::Adk => hash,
        _ => return unsupported(None, "verification spec is not an ADK spec"),
    };
    let mut roles = Vec::new();
    for role in [Role::Implementation, Role::Review] {
        let Some(setting) = settings.roles.iter().find(|s| s.role == role) else {
            return unsupported(Some(role), "no role setting observed");
        };
        let model = match &setting.model {
            ObservedModel::Concrete { model } if !model.trim().is_empty() => model.clone(),
            ObservedModel::Concrete { .. } | ObservedModel::Missing => {
                return unsupported(Some(role), "no concrete model observed");
            }
            ObservedModel::ProviderDefault => {
                return unsupported(Some(role), "provider default is not a confirmed model");
            }
        };
        let Some(effort) = setting.effort.clone().filter(|e| !e.trim().is_empty()) else {
            return unsupported(Some(role), "no effort observed");
        };
        let Some(capability) = settings
            .capabilities
            .iter()
            .find(|c| c.provider == setting.provider)
        else {
            return unsupported(Some(role), "provider capability unobserved");
        };
        if role == Role::Review && !capability.independent_review_conversation {
            return unsupported(
                Some(role),
                "provider lacks an independent review conversation",
            );
        }
        if !capability.server_owned_external_effects {
            return unsupported(
                Some(role),
                "provider cannot keep external effects server-owned",
            );
        }
        roles.push(ResolvedRole {
            role,
            agent_id: setting.agent_id.clone(),
            provider: setting.provider.clone(),
            model,
            effort,
            account_ref: setting.account_ref.clone(),
            source: setting.source.clone(),
        });
    }
    if roles[0].agent_id == roles[1].agent_id {
        return unsupported(
            Some(Role::Review),
            "review must use a different agent than work",
        );
    }
    ProfileResolution::Resolved {
        profile: ResolvedAdkProfile {
            profile_version: PROFILE_VERSION.into(),
            profile_hash: base.hash,
            roles,
            limits: base.limits,
            tool_policy_digest: settings.tool_policy_digest.clone(),
            verification_spec_hash: spec_hash,
            environment_requirements: base.environment_requirements,
        },
    }
}

#[cfg(test)]
#[path = "adk_profile_tests.rs"]
mod tests;
