use std::collections::BTreeSet;

pub use crate::domain::campaign::pipeline_rules::*;

use super::adk_profile::{AdkSettingsObservation, ProfileResolution, resolve_adk_profile};
use super::model::Role;
use super::verification_run::VerificationSpec;

pub const REQUIRED_RULE_SOURCES: [&str; 8] = [
    "SKILL.md",
    "references/completion-and-review.md",
    "references/execution.md",
    "scripts/prepush_gates.sh",
    "scripts/pr_cap_prod.py",
    "scripts/wait_headroom.sh",
    "scripts/seed_target.sh",
    "scripts/cargo-shim/cargo",
];

pub struct PipelineSource<'a> {
    pub identity: &'a str,
    pub bytes: &'a [u8],
}

// Callers supply freshly read rule bytes; only identities and digests enter the snapshot.
pub fn resolve_minimal_adk_profile(
    settings: &AdkSettingsObservation,
    spec: &VerificationSpec,
    scope: MinimalPipelineScope,
    sources: &[PipelineSource<'_>],
) -> ProfileResolution {
    let unsupported = |reason: &str| ProfileResolution::Unsupported {
        role: None,
        reason: reason.into(),
    };
    if scope.issue_number == 0
        || scope.host.trim().is_empty()
        || scope.decision_refs.iter().any(|r| r.trim().is_empty())
        || scope.decision_refs[0] == scope.decision_refs[1]
        || scope
            != MinimalPipelineScope::minimal(
                scope.issue_number,
                scope.host.clone(),
                scope.decision_refs.clone(),
            )
    {
        return unsupported("scope is outside the approved minimal contract");
    }
    let mut identities = BTreeSet::new();
    let mut digests = Vec::new();
    for source in sources {
        if source.identity.trim().is_empty()
            || source.bytes.is_empty()
            || !identities.insert(source.identity)
        {
            return unsupported("rule source is missing, empty or duplicated");
        }
        digests.push(RuleSourceDigest {
            identity: source.identity.into(),
            digest: blake3::hash(source.bytes).to_hex().to_string(),
        });
    }
    if REQUIRED_RULE_SOURCES
        .iter()
        .any(|s| !identities.contains(s))
    {
        return unsupported("required rule source is unobserved");
    }
    digests.sort_by(|a, b| a.identity.cmp(&b.identity));
    if settings.roles.len() != 2 {
        return unsupported("minimal profile requires exactly two observed roles");
    }
    let mut profile = match resolve_adk_profile(settings, spec) {
        ProfileResolution::Resolved { profile } => profile,
        unsupported => return unsupported,
    };
    for (role, provider) in [(Role::Implementation, "codex"), (Role::Review, "webgpt")] {
        let Some(resolved) = profile.roles.iter().find(|r| r.role == role) else {
            return unsupported("minimal role is unobserved");
        };
        if resolved.provider != provider
            || resolved.agent_id.trim().is_empty()
            || resolved.source.trim().is_empty()
            || resolved.account_ref.is_some()
            || matches!(resolved.model.as_str(), "unknown" | "default")
        {
            return ProfileResolution::Unsupported {
                role: Some(role),
                reason: "minimal profile requires observed Codex/WebGPT roles without fallback"
                    .into(),
            };
        }
    }
    profile.limits.concurrent_tasks = 1;
    profile.pipeline_rules = Some(Box::new(PipelineRulesSnapshot {
        sources: digests,
        scope,
        roles: profile
            .roles
            .iter()
            .map(|r| PipelineRole {
                role: r.role,
                provider: r.provider.clone(),
                model: r.model.clone(),
                effort: r.effort.clone(),
            })
            .collect(),
    }));
    ProfileResolution::Resolved { profile }
}

#[cfg(test)]
#[path = "pipeline_rules_tests.rs"]
mod tests;
