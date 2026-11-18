use std::io;
use std::path::Path;

use chrono::Utc;

use super::adk_profile::{
    AdkSettingsObservation, ObservedModel, ProfileResolution, ProviderCapability,
    ResolvedAdkProfile, RoleSettingObservation,
};
use super::model::{HostGate, Role, SessionIdentity};
use super::pipeline_rules::{
    MinimalPipelineScope, PipelineSource, REQUIRED_RULE_SOURCES, resolve_minimal_adk_profile,
};
use super::profile::Observation;
use super::verification_run::VerificationSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMarker {
    Codex,
    WebGpt,
}

pub struct RoleWitness {
    pub marker: RuntimeMarker,
    pub setting: RoleSettingObservation,
    pub capability: ProviderCapability,
    pub session: SessionIdentity,
    pub authenticated: bool,
    pub fallback_enabled: bool,
    pub tool_policy_digest: String,
}

// The caller supplies direct observations from the runtime owner and its independent review target.
pub trait RoleObserver: Send + Sync {
    fn observe(&self, role: Role) -> Observation<RoleWitness>;
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("{0:?} runtime could not be observed")]
    Unobserved(Role),
    #[error("{0:?} runtime falls outside the pilot provider policy")]
    Provider(Role),
    #[error("runtime identity does not match the pinned host or role")]
    Identity,
    #[error("the rule source directory must be absolute")]
    RuleRoot,
    #[error("reading rule source {identity}: {source}")]
    Rule {
        identity: &'static str,
        source: io::Error,
    },
    #[error("{0}")]
    Unsupported(String),
}

pub struct RuntimeProfile<'a> {
    pub rule_root: &'a Path,
    pub scope: &'a MinimalPipelineScope,
    pub tool_policy_digest: &'a str,
    pub observer: &'a dyn RoleObserver,
}

pub struct ObservedProfile {
    profile: ResolvedAdkProfile,
    sessions: [SessionIdentity; 2],
}

impl ObservedProfile {
    pub fn profile(&self) -> &ResolvedAdkProfile {
        &self.profile
    }

    pub fn require_session(
        &self,
        role: Role,
        expected: &SessionIdentity,
    ) -> Result<(), RuntimeError> {
        let index = match role {
            Role::Implementation => 0,
            Role::Review => 1,
        };
        if &self.sessions[index] != expected {
            return Err(RuntimeError::Identity);
        }
        Ok(())
    }
}

impl RuntimeProfile<'_> {
    pub fn observe(
        &self,
        host: &HostGate,
        spec: &VerificationSpec,
    ) -> Result<ObservedProfile, RuntimeError> {
        if self.scope.host != host.host || self.tool_policy_digest.trim().is_empty() {
            return Err(RuntimeError::Identity);
        }
        let work = self.role(Role::Implementation, host)?;
        let review = self.role(Role::Review, host)?;
        if work.session.session == review.session.session {
            return Err(RuntimeError::Identity);
        }
        if !self.rule_root.is_absolute() {
            return Err(RuntimeError::RuleRoot);
        }
        let mut bytes = Vec::new();
        for identity in REQUIRED_RULE_SOURCES {
            bytes.push(
                std::fs::read(self.rule_root.join(identity))
                    .map_err(|source| RuntimeError::Rule { identity, source })?,
            );
        }
        let sources: Vec<_> = REQUIRED_RULE_SOURCES
            .into_iter()
            .zip(&bytes)
            .map(|(identity, bytes)| PipelineSource { identity, bytes })
            .collect();
        let settings = AdkSettingsObservation {
            roles: vec![work.setting, review.setting],
            capabilities: vec![work.capability, review.capability],
            tool_policy_digest: self.tool_policy_digest.into(),
        };
        match resolve_minimal_adk_profile(&settings, spec, self.scope.clone(), &sources) {
            ProfileResolution::Resolved { profile } => Ok(ObservedProfile {
                profile,
                sessions: [work.session, review.session],
            }),
            ProfileResolution::Unsupported { reason, .. } => Err(RuntimeError::Unsupported(reason)),
        }
    }

    fn role(&self, role: Role, host: &HostGate) -> Result<RoleWitness, RuntimeError> {
        let requested_at = Utc::now();
        let Observation::Observed {
            evidence,
            observed_at,
        } = self.observer.observe(role)
        else {
            return Err(RuntimeError::Unobserved(role));
        };
        if !evidence.authenticated || observed_at < requested_at || observed_at > Utc::now() {
            return Err(RuntimeError::Unobserved(role));
        }
        if evidence.setting.role != role
            || &evidence.session.gate != host
            || evidence.session.session.trim().is_empty()
            || evidence.session.incarnation.trim().is_empty()
            || evidence.tool_policy_digest != self.tool_policy_digest
        {
            return Err(RuntimeError::Identity);
        }
        if !supported(role, &evidence) {
            return Err(RuntimeError::Provider(role));
        }
        Ok(evidence)
    }
}

fn supported(role: Role, witness: &RoleWitness) -> bool {
    let ObservedModel::Concrete { model } = &witness.setting.model else {
        return false;
    };
    if model.is_empty()
        || model.trim() != model
        || ["default", "unknown"]
            .iter()
            .any(|value| model.eq_ignore_ascii_case(value))
    {
        return false;
    }
    let provider = witness.setting.provider.as_str();
    let effort = witness.setting.effort.as_deref();
    let supported = match role {
        Role::Implementation => {
            witness.marker == RuntimeMarker::Codex
                && provider == "codex"
                && effort == Some("xhigh")
                && codex_sol(model)
        }
        Role::Review => {
            witness.marker == RuntimeMarker::WebGpt
                && provider == "webgpt"
                && effort == Some("pro")
                && witness.capability.independent_review_conversation
        }
    };
    supported
        && witness.capability.provider == provider
        && witness.capability.server_owned_external_effects
        && !witness.fallback_enabled
        && witness.setting.account_ref.is_none()
}

fn codex_sol(model: &str) -> bool {
    model
        .strip_prefix("gpt-")
        .and_then(|value| value.strip_suffix("-sol"))
        .is_some_and(|version| {
            version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
}

#[cfg(test)]
#[path = "runtime_profile_tests.rs"]
pub(super) mod tests;
