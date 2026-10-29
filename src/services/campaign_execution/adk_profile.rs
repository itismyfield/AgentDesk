use serde::{Deserialize, Serialize};

use super::model::Role;
use super::profile::{EnvironmentComponent, ProfileLimits};
use super::verification_run::{Digest, digest_of};

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

#[cfg(test)]
#[path = "adk_profile_tests.rs"]
mod tests;
