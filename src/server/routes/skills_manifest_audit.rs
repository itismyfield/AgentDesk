//! Manifest-to-agent-roster audit for managed skill manifests.
//!
//! Nothing is graded until its premise is confirmed at runtime; an unconfirmed premise
//! becomes a named skip in `audit_skipped` instead of a guess.

use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::skills_api::{SkillRootKind, skill_roots};

/// Both existence sources are empty, so every id would look dead.
const EMPTY_ROSTER: &str = "empty_roster";
/// An existence source failed. Grading on the surviving half yields false
/// positives, so a partial roster grades nothing.
const ROSTER_SOURCE_UNAVAILABLE: &str = "roster_source_unavailable";
const NO_MANIFEST: &str = "no_manifest";
const UNREADABLE_MANIFEST: &str = "unreadable_manifest";
const UNPARSABLE_MANIFEST: &str = "unparsable_manifest";
/// The flat `agents` key belongs to the Python distributor (`AGENT_WORKSPACES`,
/// which also accepts globs like `ch-*`); Rust parses `targets` only.
const FLAT_ROSTER_UNAVAILABLE: &str = "flat_distribution_roster_unavailable";
/// The distributor does not walk this workspace. A manually created, renamed
/// or deleted directory is indistinguishable from a dead id, so this is a skip.
const NESTED_ID_MISMATCH: &str = "nested_workspace_id_mismatch";
/// `<runtime_root>/workspaces/` also holds non-agent dirs (the AgentDesk checkout at
/// `workspaces/agentdesk`), so "walked but off the roster" is not evidence of a dead id.
const WORKSPACE_NAMESPACE_UNCONFIRMED: &str = "workspace_namespace_unconfirmed";

/// `None` means "source failed", which is not an empty set and never becomes one.
#[derive(Debug, Default)]
pub(super) struct ManifestAuditRequest {
    roster: Option<BTreeSet<String>>,
    distributed_workspaces: Option<BTreeSet<String>>,
    manifests: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub(super) struct ManifestAuditReport {
    findings: Vec<Value>,
    skipped: BTreeSet<&'static str>,
}

impl ManifestAuditReport {
    pub(super) fn to_json(&self) -> Value {
        json!({
            // False while any premise is unconfirmed, so empty `findings` never reads as clean.
            "audited": self.skipped.is_empty(),
            "audit_skipped": &self.skipped,
            "findings": &self.findings,
        })
    }
}

/// `version` and `global_core_skills` are declared, as in `runtime_layout::skill_sync`,
/// so `#[serde(flatten)]` does not sweep them into the legacy map and fail to type them.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct AuditManifest {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    global_core_skills: Vec<String>,
    #[serde(default)]
    skills: BTreeMap<String, AuditEntry>,
    #[serde(flatten)]
    legacy: BTreeMap<String, AuditEntry>,
}

/// `workspaces` is the nested axis Rust deploys against; `agents` is flat-only.
#[derive(Debug, Default, Deserialize)]
struct AuditEntry {
    #[serde(default)]
    workspaces: Vec<String>,
    #[serde(default)]
    agents: Vec<String>,
}

/// Trim, drop blanks and the bare `*` wildcard. `*` is matched before trimming, as the
/// Python distributor does: there ` * ` matches no agent and must not become the wildcard.
fn pinned_agent_ids(raw: &[String]) -> BTreeSet<String> {
    raw.iter()
        .filter(|value| value.as_str() != "*")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Only `Directory` roots carry a `manifest.json`; the markdown-file root must not.
/// Only a definite `Ok(false)` drops a path: a probe error stays in as unreadable.
fn skill_manifest_paths(runtime_root: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    skill_roots(runtime_root, home)
        .into_iter()
        .filter(|(_, kind)| *kind == SkillRootKind::Directory)
        .map(|(root, _)| root.join("manifest.json"))
        .filter(|path| path.try_exists().unwrap_or(true))
        .collect()
}

/// Both manifest path sources, read together: the home half carries three of the four
/// directory roots, so a dropped home would silently shrink the audit to the runtime root.
fn manifest_path_sources() -> (Option<PathBuf>, Option<PathBuf>) {
    (crate::config::runtime_root(), dirs::home_dir())
}

fn default_manifest_paths() -> Vec<PathBuf> {
    let (runtime_root, home) = manifest_path_sources();
    skill_manifest_paths(runtime_root, home)
}

/// Union of both "an agent by this id exists" authorities, split from the async
/// caller so it is testable without a database.
fn build_manifest_audit_request(
    config_agent_ids: BTreeSet<String>,
    db_agent_ids: Option<BTreeSet<String>>,
    distributed_workspaces: Option<BTreeSet<String>>,
    manifests: Vec<PathBuf>,
) -> ManifestAuditRequest {
    ManifestAuditRequest {
        roster: db_agent_ids.map(|db| config_agent_ids.union(&db).cloned().collect()),
        distributed_workspaces,
        manifests,
    }
}

/// The distributor's own enumeration - the walk `skill_deployment_plan` resolves
/// manifest workspace names against - rather than a directory guess made here.
fn distributed_workspaces() -> Option<BTreeSet<String>> {
    let root = crate::config::runtime_root()?;
    crate::runtime_layout::distributed_workspace_names(&root)
        .ok()
        .map(|names| names.into_iter().collect())
}

pub(super) async fn manifest_audit_request(
    config: &crate::config::Config,
    pool: &PgPool,
) -> ManifestAuditRequest {
    let db_agent_ids = crate::db::agents::load_all_agent_channel_bindings_pg(pool)
        .await
        .ok()
        .map(|bindings| bindings.into_keys().collect());
    build_manifest_audit_request(
        config.agents.iter().map(|agent| agent.id.clone()).collect(),
        db_agent_ids,
        distributed_workspaces(),
        default_manifest_paths(),
    )
}

pub(super) fn audit_skill_manifest_agents(request: ManifestAuditRequest) -> ManifestAuditReport {
    let mut report = ManifestAuditReport::default();
    match request.roster.as_ref() {
        None => report.skipped.insert(ROSTER_SOURCE_UNAVAILABLE),
        Some(roster) if roster.is_empty() => report.skipped.insert(EMPTY_ROSTER),
        Some(_) => false,
    };
    let roster = request.roster.as_ref().filter(|roster| !roster.is_empty());
    // The workspace namespace counts as agent-only only while the roster answers for
    // every walked directory (see `WORKSPACE_NAMESPACE_UNCONFIRMED`).
    let namespace_unconfirmed = roster.is_some_and(|roster| {
        request
            .distributed_workspaces
            .as_ref()
            .is_some_and(|dirs| dirs.iter().any(|dir| !roster.contains(dir)))
    });
    if request.manifests.is_empty() {
        report.skipped.insert(NO_MANIFEST);
    }
    for path in &request.manifests {
        let Ok(raw) = std::fs::read_to_string(path) else {
            report.skipped.insert(UNREADABLE_MANIFEST);
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<AuditManifest>(&raw) else {
            report.skipped.insert(UNPARSABLE_MANIFEST);
            continue;
        };
        // `agents` is flat and `workspaces` nested in both maps; the flat skip does not
        // stop grading, so a mixed manifest keeps both verdicts.
        for (skill, entry) in manifest.skills.iter().chain(manifest.legacy.iter()) {
            if !pinned_agent_ids(&entry.agents).is_empty() {
                report.skipped.insert(FLAT_ROSTER_UNAVAILABLE);
            }
            for agent_id in pinned_agent_ids(&entry.workspaces) {
                let walked = request
                    .distributed_workspaces
                    .as_ref()
                    .is_some_and(|dirs| dirs.contains(&agent_id));
                if !walked {
                    report.skipped.insert(NESTED_ID_MISMATCH);
                } else if namespace_unconfirmed {
                    report.skipped.insert(WORKSPACE_NAMESPACE_UNCONFIRMED);
                } else if roster.is_some_and(|roster| !roster.contains(&agent_id)) {
                    // Unreachable: a walked off-roster id sets `namespace_unconfirmed`
                    // first. Dormant until an independent namespace authority exists.
                    report.findings.push(json!({
                        "manifest": path.display().to_string(),
                        "skill": skill,
                        "agent_id": agent_id,
                    }));
                }
            }
        }
    }
    report
}

#[cfg(test)]
#[path = "skills_manifest_audit_tests.rs"]
mod tests;
