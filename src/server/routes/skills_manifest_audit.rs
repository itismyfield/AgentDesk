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
const UNREADABLE_SKILL_DIRECTORY: &str = "unreadable_skill_directory";
/// The distributor does not walk this workspace. A manually created, renamed
/// or deleted directory is indistinguishable from a dead id, so this is a skip.
const NESTED_ID_MISMATCH: &str = "nested_workspace_id_mismatch";
/// `None` means "source failed", which is not an empty set and never becomes one.
#[derive(Debug, Default)]
pub(crate) struct ManifestAuditRequest {
    roster: Option<BTreeSet<String>>,
    distributed_workspaces: Option<BTreeSet<String>>,
    manifests: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub(crate) struct ManifestAuditReport {
    findings: Vec<Value>,
    skipped: BTreeSet<&'static str>,
    inspection_errors: Vec<Value>,
}

impl ManifestAuditReport {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            // False while any premise is unconfirmed, so empty `findings` never reads as clean.
            "audited": self.skipped.is_empty(),
            "audit_skipped": &self.skipped,
            "findings": &self.findings,
            "inspection_errors": &self.inspection_errors,
        })
    }

    pub(crate) fn warning(&self) -> Option<String> {
        if self.findings.is_empty() && self.skipped.is_empty() {
            return None;
        }
        Some(format!("skill manifest audit: {}", self.to_json()))
    }

    fn unavailable(&mut self, reason: &'static str, path: &std::path::Path) {
        self.skipped.insert(reason);
        self.inspection_errors
            .push(json!({"reason": reason, "path": path.display().to_string()}));
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

/// Workspace selectors and agent selectors have separate namespaces.
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

/// Only directory roots carry a `manifest.json`; the markdown-file root must not.
/// The runtime manifest is required; an installed vault source also needs its manifest.
fn skill_manifest_paths(runtime_root: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let required = runtime_root
        .as_ref()
        .map(|root| crate::runtime_layout::managed_skills_manifest_path(root));
    skill_roots(runtime_root, home)
        .into_iter()
        .filter(|(_, kind)| *kind != SkillRootKind::MarkdownFile)
        .filter_map(|(root, kind)| {
            let path = root.join("manifest.json");
            (Some(&path) == required.as_ref()
                || path.try_exists().unwrap_or(true)
                || (kind == SkillRootKind::ManagedDirectory && root.try_exists().unwrap_or(true)))
            .then_some(path)
        })
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
pub(crate) fn build_manifest_audit_request(
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

pub(crate) async fn manifest_audit_request(
    config: &crate::config::Config,
    pool: &PgPool,
) -> ManifestAuditRequest {
    let db_agent_ids = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        crate::db::agents::load_all_agent_channel_bindings_pg(pool),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .map(|bindings| bindings.into_keys().collect());
    let config_agent_ids = config.agents.iter().map(|agent| agent.id.clone()).collect();
    tokio::task::spawn_blocking(move || {
        runtime_manifest_audit_request(config_agent_ids, db_agent_ids)
    })
    .await
    .unwrap_or_default()
}

fn runtime_manifest_audit_request(
    config_agent_ids: BTreeSet<String>,
    db_agent_ids: Option<BTreeSet<String>>,
) -> ManifestAuditRequest {
    build_manifest_audit_request(
        config_agent_ids,
        db_agent_ids,
        distributed_workspaces(),
        default_manifest_paths(),
    )
}

pub(crate) fn unavailable_manifest_audit_request(
    config: &crate::config::Config,
) -> ManifestAuditRequest {
    runtime_manifest_audit_request(
        config.agents.iter().map(|agent| agent.id.clone()).collect(),
        None,
    )
}

pub(crate) fn audit_skill_manifest_agents(request: ManifestAuditRequest) -> ManifestAuditReport {
    let mut report = ManifestAuditReport::default();
    match request.roster.as_ref() {
        None => report.skipped.insert(ROSTER_SOURCE_UNAVAILABLE),
        Some(roster) if roster.is_empty() => report.skipped.insert(EMPTY_ROSTER),
        Some(_) => false,
    };
    let roster = request.roster.as_ref().filter(|roster| !roster.is_empty());
    if request.manifests.is_empty() {
        report.skipped.insert(NO_MANIFEST);
    }
    for path in &request.manifests {
        let Ok(raw) = std::fs::read_to_string(path) else {
            report.unavailable(
                if matches!(path.try_exists(), Ok(false)) {
                    NO_MANIFEST
                } else {
                    UNREADABLE_MANIFEST
                },
                path,
            );
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<AuditManifest>(&raw) else {
            report.unavailable(UNPARSABLE_MANIFEST, path);
            continue;
        };
        audit_skill_directories(path, &manifest, &mut report);
        for (skill, entry) in manifest.skills.iter().chain(manifest.legacy.iter()) {
            if let Some(roster) = roster {
                for selector in entry
                    .agents
                    .iter()
                    .filter(|id| !id.trim().is_empty() && id.as_str() != "*")
                {
                    let matches = glob::Pattern::new(selector)
                        .ok()
                        .is_some_and(|pattern| roster.iter().any(|id| pattern.matches(id)));
                    if !matches {
                        report.findings.push(json!({
                            "kind": "unknown_agent", "manifest": path.display().to_string(),
                            "skill": skill, "agent_id": selector,
                        }));
                    }
                }
            }
            for agent_id in pinned_agent_ids(&entry.workspaces) {
                let walked = request
                    .distributed_workspaces
                    .as_ref()
                    .is_some_and(|dirs| dirs.contains(&agent_id));
                if !walked {
                    report.skipped.insert(NESTED_ID_MISMATCH);
                }
            }
        }
    }
    report
}

fn audit_skill_directories(
    path: &std::path::Path,
    manifest: &AuditManifest,
    report: &mut ManifestAuditReport,
) {
    let Some(root) = path.parent() else { return };
    let Ok(entries) = std::fs::read_dir(root) else {
        report.unavailable(UNREADABLE_SKILL_DIRECTORY, root);
        return;
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(_) => report.unavailable(UNREADABLE_SKILL_DIRECTORY, root),
        }
    }
    paths.sort();
    for dir in paths {
        let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "_shared" || name.starts_with('.') {
            continue;
        }
        match std::fs::metadata(dir.join("SKILL.md")) {
            Ok(metadata)
                if metadata.is_file()
                    && !manifest.skills.contains_key(name)
                    && !manifest.legacy.contains_key(name)
                    && !manifest
                        .global_core_skills
                        .iter()
                        .any(|skill| skill == name) =>
            {
                report.findings.push(
                    json!({"kind": "unlisted_skill", "manifest": path.display().to_string(),
                    "skill": name, "path": dir.display().to_string()}),
                );
            }
            Err(error)
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                report.unavailable(UNREADABLE_SKILL_DIRECTORY, &dir)
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "skills_manifest_audit_tests.rs"]
mod tests;
