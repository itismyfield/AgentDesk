use super::paths::{current_home_dir, expand_user_path};
use super::skill_refresh::refresh_managed_skill_dir;
use super::*;

#[derive(Debug, Clone, Default, Deserialize)]
struct SkillManifest {
    #[serde(default = "default_skill_manifest_version")]
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    global_core_skills: Vec<String>,
    #[serde(default)]
    skills: BTreeMap<String, SkillManifestEntry>,
    #[serde(flatten)]
    legacy_skills: BTreeMap<String, LegacySkillManifestEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SkillManifestEntry {
    #[serde(default)]
    providers: Vec<String>,
    #[serde(default)]
    workspaces: Vec<String>,
    #[serde(default)]
    global: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LegacySkillManifestEntry {
    #[serde(default)]
    targets: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SkillDeploymentPlan {
    global_providers: Vec<String>,
    workspace_targets: Vec<WorkspaceSkillTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceSkillTarget {
    root: PathBuf,
    providers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LegacySkillTarget {
    Global { provider: String },
    Workspace { provider: String, root: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkState {
    Created,
    Updated,
    Unchanged,
    SkippedExisting,
}

pub(super) fn ensure_managed_skills_manifest(root: &Path) -> Result<(), String> {
    let skills_root = managed_skills_root(root);
    fs::create_dir_all(&skills_root)
        .map_err(|e| format!("Failed to create '{}': {e}", skills_root.display()))?;
    let manifest_path = managed_skills_manifest_path(root);
    if manifest_path.exists() {
        return Ok(());
    }

    let rendered = serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "global_core_skills": [],
        "skills": {}
    }))
    .map_err(|e| format!("Failed to serialize '{}': {e}", manifest_path.display()))?;
    fs::write(&manifest_path, rendered)
        .map_err(|e| format!("Failed to write '{}': {e}", manifest_path.display()))
}

pub(super) fn sync_managed_skills(root: &Path) -> Result<SkillSyncReport, String> {
    ensure_managed_skills_manifest(root)?;
    let skills_root = managed_skills_root(root);
    if !skills_root.is_dir() {
        return Ok(SkillSyncReport::default());
    }

    let manifest = load_skill_manifest(root)?;
    let skill_dirs = discover_skill_dirs(root)?;
    if skill_dirs.is_empty() {
        return Ok(SkillSyncReport::default());
    }

    let workspace_names = discover_workspaces(root)?;
    let mut report = SkillSyncReport::default();

    for (skill_name, skill_dir) in skill_dirs {
        let deployment = skill_deployment_plan(&manifest, &skill_name, root, &workspace_names);

        if !deployment.global_providers.is_empty() {
            let Some(home) = current_home_dir() else {
                return Ok(report);
            };
            for provider in &deployment.global_providers {
                let target_dir = provider_target_dir(provider, &home);
                match deploy_skill_link(provider, &skill_name, &skill_dir, &target_dir) {
                    Ok(LinkState::Created) => report.created_links += 1,
                    Ok(LinkState::Updated) => report.updated_links += 1,
                    Ok(LinkState::SkippedExisting) => report.skipped_existing += 1,
                    Ok(LinkState::Unchanged) => {}
                    Err(err) => {
                        return Err(format!(
                            "Failed to deploy global skill '{}' for {}: {}",
                            skill_name, provider, err
                        ));
                    }
                }
            }
        }

        for workspace_target in &deployment.workspace_targets {
            if !workspace_target.root.is_dir() {
                continue;
            }
            let workspace_label = workspace_target.root.display().to_string();
            for provider in &workspace_target.providers {
                let target_dir = provider_target_dir(provider, &workspace_target.root);
                match deploy_skill_link(provider, &skill_name, &skill_dir, &target_dir) {
                    Ok(LinkState::Created) => report.created_links += 1,
                    Ok(LinkState::Updated) => report.updated_links += 1,
                    Ok(LinkState::SkippedExisting) => report.skipped_existing += 1,
                    Ok(LinkState::Unchanged) => {}
                    Err(err) => {
                        return Err(format!(
                            "Failed to deploy workspace skill '{}' to {} ({}) : {}",
                            skill_name, workspace_label, provider, err
                        ));
                    }
                }
            }
        }

        prune_unplanned_managed_skill_links(
            root,
            &skill_name,
            &skill_dir,
            &deployment,
            &workspace_names,
        )?;
    }

    Ok(report)
}

fn skill_deployment_plan(
    manifest: &SkillManifest,
    skill_name: &str,
    root: &Path,
    workspace_names: &[String],
) -> SkillDeploymentPlan {
    if let Some(entry) = manifest.skills.get(skill_name) {
        let providers = normalize_provider_list(&entry.providers)
            .into_iter()
            .collect::<Vec<_>>();
        let providers = if providers.is_empty() {
            all_skill_providers()
        } else {
            providers
        };
        let global_enabled = entry.global.unwrap_or_else(|| {
            if manifest.skills.is_empty()
                && manifest.global_core_skills.is_empty()
                && manifest.legacy_skills.is_empty()
            {
                true
            } else {
                manifest
                    .global_core_skills
                    .iter()
                    .any(|name| name == skill_name)
            }
        });
        let workspace_targets = if entry.workspaces.is_empty() {
            Vec::new()
        } else {
            workspace_targets_for_names(
                root,
                &normalize_workspace_names(&entry.workspaces),
                &providers,
            )
        };
        return SkillDeploymentPlan {
            global_providers: if global_enabled {
                providers.clone()
            } else {
                Vec::new()
            },
            workspace_targets,
        };
    }

    if let Some(entry) = manifest.legacy_skills.get(skill_name) {
        return legacy_skill_deployment_plan(&entry.targets);
    }

    if manifest.skills.is_empty()
        && manifest.global_core_skills.is_empty()
        && manifest.legacy_skills.is_empty()
    {
        let providers = all_skill_providers();
        return SkillDeploymentPlan {
            global_providers: providers.clone(),
            workspace_targets: workspace_targets_for_names(root, workspace_names, &providers),
        };
    }

    if !manifest.legacy_skills.is_empty() {
        return SkillDeploymentPlan {
            global_providers: all_skill_providers(),
            workspace_targets: Vec::new(),
        };
    }

    let providers = all_skill_providers();
    SkillDeploymentPlan {
        global_providers: if manifest
            .global_core_skills
            .iter()
            .any(|name| name == skill_name)
        {
            providers.clone()
        } else {
            Vec::new()
        },
        workspace_targets: Vec::new(),
    }
}

fn workspace_targets_for_names(
    root: &Path,
    workspace_names: &[String],
    providers: &[String],
) -> Vec<WorkspaceSkillTarget> {
    let mut result = Vec::new();
    for workspace_name in workspace_names {
        let workspace_root = root.join("workspaces").join(workspace_name);
        if !workspace_root.is_dir() {
            continue;
        }
        result.push(WorkspaceSkillTarget {
            root: workspace_root,
            providers: providers.to_vec(),
        });
    }
    result
}

fn prune_unplanned_managed_skill_links(
    root: &Path,
    skill_name: &str,
    skill_dir: &Path,
    deployment: &SkillDeploymentPlan,
    workspace_names: &[String],
) -> Result<(), String> {
    let mut desired_dirs = BTreeSet::new();
    if let Some(home) = current_home_dir() {
        for provider in &deployment.global_providers {
            desired_dirs.insert(provider_target_dir(provider, &home));
        }
    }
    for target in &deployment.workspace_targets {
        for provider in &target.providers {
            desired_dirs.insert(provider_target_dir(provider, &target.root));
        }
    }

    for (provider, target_dir) in all_candidate_skill_target_dirs(root, workspace_names) {
        if desired_dirs.contains(&target_dir) {
            continue;
        }
        let (source_path, link_path, _is_dir_link) =
            skill_link_paths(&provider, skill_name, skill_dir, &target_dir);
        if !path_exists(&link_path) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&link_path) else {
            continue;
        };
        if metadata.file_type().is_symlink() && same_canonical_path(&link_path, &source_path) {
            remove_link_or_path(&link_path)?;
        }
    }
    Ok(())
}

fn all_candidate_skill_target_dirs(
    root: &Path,
    workspace_names: &[String],
) -> Vec<(String, PathBuf)> {
    let mut result = Vec::new();
    if let Some(home) = current_home_dir() {
        for provider in all_skill_providers() {
            result.push((provider.clone(), provider_target_dir(&provider, &home)));
        }
    }
    for workspace_name in workspace_names {
        let workspace_root = root.join("workspaces").join(workspace_name);
        for provider in all_skill_providers() {
            result.push((
                provider.clone(),
                provider_target_dir(&provider, &workspace_root),
            ));
        }
    }
    result
}

fn legacy_skill_deployment_plan(targets: &[String]) -> SkillDeploymentPlan {
    let mut global = BTreeSet::new();
    let mut workspaces = BTreeMap::<PathBuf, BTreeSet<String>>::new();

    for target in targets {
        match parse_legacy_skill_target(target) {
            Some(LegacySkillTarget::Global { provider }) => {
                global.insert(provider);
            }
            Some(LegacySkillTarget::Workspace { provider, root }) => {
                workspaces.entry(root).or_default().insert(provider);
            }
            None => {}
        }
    }

    SkillDeploymentPlan {
        global_providers: global.into_iter().collect(),
        workspace_targets: workspaces
            .into_iter()
            .map(|(root, providers)| WorkspaceSkillTarget {
                root,
                providers: providers.into_iter().collect(),
            })
            .collect(),
    }
}

fn parse_legacy_skill_target(raw: &str) -> Option<LegacySkillTarget> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((provider_raw, root_raw)) = trimmed.split_once('@') {
        let provider = normalize_provider_name(provider_raw)?;
        let root = expand_user_path(root_raw)?;
        return Some(LegacySkillTarget::Workspace { provider, root });
    }
    normalize_provider_name(trimmed).map(|provider| LegacySkillTarget::Global { provider })
}

fn load_skill_manifest(root: &Path) -> Result<SkillManifest, String> {
    let path = managed_skills_manifest_path(root);
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse '{}': {e}", path.display()))
}

fn discover_skill_dirs(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut result = Vec::new();
    let skills_root = managed_skills_root(root);
    if !skills_root.is_dir() {
        return Ok(result);
    }
    let entries = fs::read_dir(&skills_root)
        .map_err(|e| format!("Failed to read '{}': {e}", skills_root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join("SKILL.md").is_file() {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|value| value.to_str())
            .map(ToString::to_string)
        else {
            continue;
        };
        result.push((name, path));
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

fn discover_workspaces(root: &Path) -> Result<Vec<String>, String> {
    let workspaces_root = root.join("workspaces");
    let mut result = Vec::new();
    if !workspaces_root.is_dir() {
        return Ok(result);
    }
    let entries = fs::read_dir(&workspaces_root)
        .map_err(|e| format!("Failed to read '{}': {e}", workspaces_root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        result.push(name.to_string());
    }
    result.sort();
    Ok(result)
}

fn normalize_provider_list(values: &[String]) -> Vec<String> {
    let mut providers = values
        .iter()
        .filter_map(|value| normalize_provider_name(value))
        .collect::<Vec<_>>();
    providers.sort();
    providers.dedup();
    providers
}

fn normalize_workspace_names(values: &[String]) -> Vec<String> {
    let mut workspaces = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    workspaces.sort();
    workspaces.dedup();
    workspaces
}

fn all_skill_providers() -> Vec<String> {
    vec![
        "claude".to_string(),
        "codex".to_string(),
        "gemini".to_string(),
        "qwen".to_string(),
    ]
}

pub(super) fn migrate_legacy_skill_links(root: &Path) -> Result<(), String> {
    let manifest = load_skill_manifest(root)?;
    let mut candidates = collect_legacy_skill_target_dirs(root, &manifest)?;
    candidates.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    candidates.dedup();

    for (provider, target_dir) in candidates {
        if !target_dir.is_dir() {
            continue;
        }
        migrate_legacy_skill_links_for_target(root, &provider, &target_dir)?;
    }
    Ok(())
}

fn collect_legacy_skill_target_dirs(
    root: &Path,
    manifest: &SkillManifest,
) -> Result<Vec<(String, PathBuf)>, String> {
    let mut result = Vec::new();
    if should_scan_global_skill_targets(root) {
        let Some(home) = current_home_dir() else {
            return Ok(result);
        };
        for provider in all_skill_providers() {
            result.push((provider.clone(), provider_target_dir(&provider, &home)));
        }
    }

    for workspace_name in discover_workspaces(root)? {
        let workspace_root = root.join("workspaces").join(workspace_name);
        for provider in all_skill_providers() {
            result.push((
                provider.clone(),
                provider_target_dir(&provider, &workspace_root),
            ));
        }
    }

    for entry in manifest.legacy_skills.values() {
        for target in &entry.targets {
            if let Some(LegacySkillTarget::Workspace { provider, root }) =
                parse_legacy_skill_target(target)
            {
                result.push((provider.clone(), provider_target_dir(&provider, &root)));
            }
        }
    }
    Ok(result)
}

fn should_scan_global_skill_targets(root: &Path) -> bool {
    crate::config::runtime_root()
        .map(|active_root| same_canonical_path(root, &active_root))
        .unwrap_or(false)
}

fn migrate_legacy_skill_links_for_target(
    root: &Path,
    provider: &str,
    target_dir: &Path,
) -> Result<(), String> {
    let entries = fs::read_dir(target_dir)
        .map_err(|e| format!("Failed to read '{}': {e}", target_dir.display()))?;
    for entry in entries.flatten() {
        let link_path = entry.path();
        let meta = match fs::symlink_metadata(&link_path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        let Some((skill_name, source_skill_dir)) =
            resolve_linked_skill_source(provider, &link_path)
        else {
            continue;
        };
        let managed_skill_dir = ensure_managed_skill_dir(root, &skill_name, &source_skill_dir)?;
        deploy_skill_link(provider, &skill_name, &managed_skill_dir, target_dir)?;
    }
    Ok(())
}

fn resolve_linked_skill_source(provider: &str, link_path: &Path) -> Option<(String, PathBuf)> {
    let resolved = fs::canonicalize(link_path).ok()?;
    let skill_name = if provider == "claude" {
        link_path.file_stem()?.to_str()?.to_string()
    } else {
        link_path.file_name()?.to_str()?.to_string()
    };
    let skill_dir = if resolved.is_dir() {
        resolved
    } else if resolved
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
    {
        resolved.parent()?.to_path_buf()
    } else {
        return None;
    };
    if !skill_dir.join("SKILL.md").is_file() {
        return None;
    }
    Some((skill_name, skill_dir))
}

pub(super) fn ensure_managed_skill_dir(
    root: &Path,
    skill_name: &str,
    source_skill_dir: &Path,
) -> Result<PathBuf, String> {
    let managed_dir = managed_skills_root(root).join(skill_name);
    // Never copy a skill onto itself; otherwise re-copy whenever the managed cache no
    // longer reflects the source (#4256: it used to be copy-once, ignoring later edits).
    if !same_canonical_path(&managed_dir, source_skill_dir)
        && managed_skill_dir_is_stale(skill_name, source_skill_dir, &managed_dir)?
    {
        refresh_managed_skill_dir(root, skill_name, source_skill_dir, &managed_dir)?;
    }
    rewrite_managed_skill_paths(skill_name, &managed_dir)?;
    Ok(managed_dir)
}

/// Relative paths whose managed copies are content-rewritten by
/// [`rewrite_managed_skill_paths`]; shared with the freshness check so the idempotent
/// rewrite does not make a rewritten cache look perpetually stale.
fn managed_skill_rewrite_relpaths(skill_name: &str) -> Vec<PathBuf> {
    match skill_name {
        "memory-read" | "memory-write" => vec![PathBuf::from("SKILL.md")],
        "memory-merge" => vec![
            PathBuf::from("SKILL.md"),
            PathBuf::from("references").join("architecture.md"),
            PathBuf::from("references").join("classification-guide.md"),
            PathBuf::from("references").join("phase-details.md"),
            PathBuf::from("references").join("report-template.md"),
        ],
        _ => Vec::new(),
    }
}

fn rewrite_managed_skill_paths(skill_name: &str, skill_dir: &Path) -> Result<(), String> {
    for relative in managed_skill_rewrite_relpaths(skill_name) {
        rewrite_text_file_paths(&skill_dir.join(relative))?;
    }
    Ok(())
}

/// Returns `true` when the managed cache must be re-copied: never populated, drifted
/// from the source, or carrying a file the source dropped (mirrors `rsync -aL --delete`).
fn managed_skill_dir_is_stale(
    skill_name: &str,
    source_skill_dir: &Path,
    managed_dir: &Path,
) -> Result<bool, String> {
    if !managed_dir.join("SKILL.md").is_file() {
        return Ok(true); // never copied yet
    }
    let rewrite_relpaths = managed_skill_rewrite_relpaths(skill_name);
    let mut source_relpaths = BTreeSet::new();
    if source_skill_content_differs(
        source_skill_dir,
        managed_dir,
        Path::new(""),
        &rewrite_relpaths,
        &mut source_relpaths,
    )? {
        return Ok(true);
    }
    managed_skill_has_extra_files(managed_dir, Path::new(""), &source_relpaths)
}

/// Walks the source skill dir (resolving symlinks, skipping the junk entries
/// [`copy_skill_dir_resolving_symlinks`] skips), reporting whether any source file is
/// missing from or differs from its managed copy, and recording each source relative
/// path in `source_relpaths` so the caller can detect stale extra files.
fn source_skill_content_differs(
    src: &Path,
    managed: &Path,
    relative: &Path,
    rewrite_relpaths: &[PathBuf],
    source_relpaths: &mut BTreeSet<PathBuf>,
) -> Result<bool, String> {
    if should_skip_skill_entry(src) {
        return Ok(false);
    }

    let meta = fs::symlink_metadata(src)
        .map_err(|e| format!("Failed to stat '{}': {e}", src.display()))?;
    if meta.file_type().is_symlink() {
        let resolved = match fs::canonicalize(src) {
            Ok(resolved) => resolved,
            // A dangling symlink is skipped by the copy routine, so it is not drift.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "Failed to resolve symlink '{}': {error}",
                    src.display()
                ));
            }
        };
        return source_skill_content_differs(
            &resolved,
            managed,
            relative,
            rewrite_relpaths,
            source_relpaths,
        );
    }

    if meta.is_dir() {
        let entries =
            fs::read_dir(src).map_err(|e| format!("Failed to read '{}': {e}", src.display()))?;
        for entry in entries.flatten() {
            let child = entry.path();
            let Some(name) = child.file_name() else {
                continue;
            };
            if source_skill_content_differs(
                &child,
                &managed.join(name),
                &relative.join(name),
                rewrite_relpaths,
                source_relpaths,
            )? {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    source_relpaths.insert(relative.to_path_buf());
    if !managed.is_file() {
        return Ok(true);
    }
    let managed_bytes =
        fs::read(managed).map_err(|e| format!("Failed to read '{}': {e}", managed.display()))?;

    let differs = if rewrite_relpaths.iter().any(|p| p == relative) {
        // Compare against the expected rewrite so a rewritten cache is not flagged stale.
        match fs::read_to_string(src) {
            Ok(source_text) => {
                let mut expected = source_text;
                for (from, to) in skill_path_replacements() {
                    expected = expected.replace(from, to);
                }
                expected.as_bytes() != managed_bytes.as_slice()
            }
            Err(_) => {
                let source_bytes = fs::read(src)
                    .map_err(|e| format!("Failed to read '{}': {e}", src.display()))?;
                source_bytes != managed_bytes
            }
        }
    } else {
        let source_bytes =
            fs::read(src).map_err(|e| format!("Failed to read '{}': {e}", src.display()))?;
        source_bytes != managed_bytes
    };
    Ok(differs)
}

/// Reports whether the managed cache carries a regular file deleted upstream.
fn managed_skill_has_extra_files(
    managed: &Path,
    relative: &Path,
    source_relpaths: &BTreeSet<PathBuf>,
) -> Result<bool, String> {
    let entries = match fs::read_dir(managed) {
        Ok(entries) => entries,
        Err(_) => return Ok(false),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if should_skip_skill_entry(&path) {
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        let child_relative = relative.join(name);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() && !meta.file_type().is_symlink() {
            if managed_skill_has_extra_files(&path, &child_relative, source_relpaths)? {
                return Ok(true);
            }
        } else if !source_relpaths.contains(&child_relative) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rewrite_text_file_paths(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let original = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut rewritten = original.clone();
    for (from, to) in skill_path_replacements() {
        rewritten = rewritten.replace(from, to);
    }
    if rewritten != original {
        fs::write(path, rewritten)
            .map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    }
    Ok(())
}

fn skill_path_replacements() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "~/.adk/release/shared_agent_memory/shared_knowledge.md",
            "~/.adk/release/config/memories/shared-agent-knowledge/shared_knowledge.md",
        ),
        (
            "~/.adk/release/shared_agent_memory/{role_id}.json",
            "~/.adk/release/config/memories/shared-agent-memory/{role_id}.json",
        ),
        (
            "~/.adk/release/shared_agent_memory/{roleId}.json",
            "~/.adk/release/config/memories/shared-agent-memory/{roleId}.json",
        ),
        (
            "~/.adk/release/shared_agent_memory/archive/{YYYY-MM-DD}/",
            "~/.adk/release/config/memories/archive/{YYYY-MM-DD}/",
        ),
        (
            "~/ObsidianVault/RemoteVault/agents/{role_id}/long-term-memory/",
            "~/.adk/release/config/memories/long-term/{role_id}/",
        ),
        (
            "~/ObsidianVault/RemoteVault/agents/{roleId}/long-term-memory/",
            "~/.adk/release/config/memories/long-term/{roleId}/",
        ),
        (
            "~/.claude/projects/-Users-itismyfield--adk-release-workspaces-{workspace}/memory/",
            "~/.claude/projects/*{workspace}*/memory/",
        ),
    ]
}

fn provider_target_dir(provider: &str, base: &Path) -> PathBuf {
    match provider {
        "claude" => base.join(".claude").join("commands"),
        "codex" => base.join(".codex").join("skills"),
        "gemini" => base.join(".gemini").join("skills"),
        "qwen" => base.join(".qwen").join("skills"),
        _ => base.join(".codex").join("skills"),
    }
}

fn deploy_skill_link(
    provider: &str,
    skill_name: &str,
    skill_dir: &Path,
    target_dir: &Path,
) -> Result<LinkState, String> {
    fs::create_dir_all(target_dir)
        .map_err(|e| format!("Failed to create '{}': {e}", target_dir.display()))?;

    let (source_path, link_path, is_dir_link) =
        skill_link_paths(provider, skill_name, skill_dir, target_dir);

    if !path_exists(&source_path) {
        return Ok(LinkState::SkippedExisting);
    }

    if let Ok(existing_target) = fs::read_link(&link_path) {
        let desired = relative_path_from(
            link_path.parent().unwrap_or(target_dir),
            &source_path.canonicalize().unwrap_or(source_path.clone()),
        );
        if existing_target == desired || same_canonical_path(&link_path, &source_path) {
            return Ok(LinkState::Unchanged);
        }
        if existing_skill_link_is_compatible(provider, skill_name, &link_path) {
            return Ok(LinkState::SkippedExisting);
        }
        remove_link_or_path(&link_path)?;
        create_symlink_entry(&source_path, &link_path, is_dir_link)?;
        return Ok(LinkState::Updated);
    }

    if path_exists(&link_path) {
        return Ok(LinkState::SkippedExisting);
    }

    create_symlink_entry(&source_path, &link_path, is_dir_link)?;
    Ok(LinkState::Created)
}

fn existing_skill_link_is_compatible(provider: &str, skill_name: &str, link_path: &Path) -> bool {
    let Ok(resolved) = fs::canonicalize(link_path) else {
        return false;
    };

    if provider == "claude" {
        return resolved
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
            && resolved.is_file()
            && resolved
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == skill_name);
    }

    let skill_dir = if resolved.is_dir() {
        resolved
    } else if resolved
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
    {
        match resolved.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return false,
        }
    } else {
        return false;
    };

    skill_dir.join("SKILL.md").is_file()
        && skill_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == skill_name)
}

fn skill_link_paths(
    provider: &str,
    skill_name: &str,
    skill_dir: &Path,
    target_dir: &Path,
) -> (PathBuf, PathBuf, bool) {
    if provider == "claude" {
        (
            skill_dir.join("SKILL.md"),
            target_dir.join(format!("{skill_name}.md")),
            false,
        )
    } else {
        (skill_dir.to_path_buf(), target_dir.join(skill_name), true)
    }
}

pub(super) fn copy_skill_dir_resolving_symlinks(src: &Path, dest: &Path) -> Result<(), String> {
    copy_skill_path_resolving_symlinks(src, dest)
}

fn copy_skill_path_resolving_symlinks(src: &Path, dest: &Path) -> Result<(), String> {
    if should_skip_skill_entry(src) {
        return Ok(());
    }

    let meta = fs::symlink_metadata(src)
        .map_err(|e| format!("Failed to stat '{}': {e}", src.display()))?;
    if meta.file_type().is_symlink() {
        let resolved = match fs::canonicalize(src) {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(format!(
                    "Failed to resolve symlink '{}': {error}",
                    src.display()
                ));
            }
        };
        return copy_skill_path_resolving_symlinks(&resolved, dest);
    }

    if meta.is_dir() {
        fs::create_dir_all(dest)
            .map_err(|e| format!("Failed to create '{}': {e}", dest.display()))?;
        let entries =
            fs::read_dir(src).map_err(|e| format!("Failed to read '{}': {e}", src.display()))?;
        for entry in entries.flatten() {
            let child = entry.path();
            let Some(name) = child.file_name() else {
                continue;
            };
            copy_skill_path_resolving_symlinks(&child, &dest.join(name))?;
        }
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    fs::copy(src, dest).map_err(|e| {
        format!(
            "Failed to copy '{}' -> '{}': {e}",
            src.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn should_skip_skill_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.starts_with('.')
                || matches!(name, "__pycache__" | "node_modules" | "target" | "venv")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod skill_cache_freshness_tests {
    use super::*;
    use filetime::FileTime;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Pins a file's mtime far in the past so a subsequent no-op sync can be proven
    /// to have left the file untouched (no re-copy, no rewrite).
    fn pin_mtime(path: &Path) -> FileTime {
        let past = FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_mtime(path, past).unwrap();
        past
    }

    fn mtime(path: &Path) -> FileTime {
        FileTime::from_last_modification_time(&fs::metadata(path).unwrap())
    }

    #[test]
    fn copies_initially_then_refreshes_on_content_change_and_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source-skills").join("demo");
        write_file(&source.join("SKILL.md"), "version-1\n");
        write_file(&source.join("references").join("notes.md"), "alpha\n");

        // (a) first sync performs the initial copy.
        let managed = ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(managed, managed_skills_root(root).join("demo"));
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "version-1\n"
        );
        assert_eq!(
            fs::read_to_string(managed.join("references").join("notes.md")).unwrap(),
            "alpha\n"
        );

        // (b) after the source changes, the next sync re-copies the updated content.
        write_file(&source.join("SKILL.md"), "version-2\n");
        write_file(&source.join("references").join("notes.md"), "beta\n");
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "version-2\n"
        );
        assert_eq!(
            fs::read_to_string(managed.join("references").join("notes.md")).unwrap(),
            "beta\n"
        );

        // A file deleted upstream is pruned from the managed cache on refresh.
        fs::remove_file(source.join("references").join("notes.md")).unwrap();
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert!(!managed.join("references").join("notes.md").exists());
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "version-2\n"
        );

        // No transient staging directory is left behind.
        assert!(!root.join(".skill-refresh").exists());
    }

    #[test]
    fn identical_source_does_not_churn_the_cache() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source-skills").join("demo");
        write_file(&source.join("SKILL.md"), "stable\n");

        let managed = ensure_managed_skill_dir(root, "demo", &source).unwrap();
        let skill_md = managed.join("SKILL.md");
        let pinned = pin_mtime(&skill_md);

        // (c) an unchanged source must not trigger a re-copy or rewrite.
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(fs::read_to_string(&skill_md).unwrap(), "stable\n");
        assert_eq!(mtime(&skill_md), pinned, "unchanged source must not churn");
    }

    #[test]
    fn rewritten_skill_is_not_perpetually_stale() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source-skills").join("memory-read");
        write_file(
            &source.join("SKILL.md"),
            "see ~/.adk/release/shared_agent_memory/shared_knowledge.md\n",
        );

        let managed = ensure_managed_skill_dir(root, "memory-read", &source).unwrap();
        let skill_md = managed.join("SKILL.md");
        let rewritten = fs::read_to_string(&skill_md).unwrap();
        assert!(
            rewritten.contains(
                "~/.adk/release/config/memories/shared-agent-knowledge/shared_knowledge.md"
            ),
            "managed copy should carry the rewritten path"
        );
        assert!(!rewritten.contains("release/shared_agent_memory/shared_knowledge.md"));

        // The managed copy differs from the source only because of the idempotent
        // path rewrite, so a second sync must recognize it as fresh (no churn).
        let pinned = pin_mtime(&skill_md);
        ensure_managed_skill_dir(root, "memory-read", &source).unwrap();
        assert_eq!(
            mtime(&skill_md),
            pinned,
            "rewrite-aware freshness check must not re-copy a rewritten skill"
        );
    }

    /// #4256 concurrency safety: a unique-named staging dir left behind by a crashed prior
    /// refresh must not corrupt a later one, and a lockfile held by a "concurrent" process
    /// must make the refresh skip (converge later) instead of racing the swap.
    #[test]
    fn refresh_is_concurrency_safe_lock_and_staging() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source-skills").join("demo");
        write_file(&source.join("SKILL.md"), "v1\n");

        let managed = ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "v1\n"
        );

        // A staging dir left behind by a crashed prior refresh (unique-named) must not
        // block or corrupt a later refresh: unique staging paths let the new run converge.
        let leftover = root.join(".skill-refresh").join("demo.999999.0");
        write_file(&leftover.join("SKILL.md"), "garbage\n");
        write_file(&source.join("SKILL.md"), "v2\n");
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "v2\n"
        );

        // A held lockfile makes a concurrent refresh SKIP rather than race the holder, so
        // the source change is not applied while the lock is held.
        let refresh_dir = root.join(".skill-refresh");
        fs::create_dir_all(&refresh_dir).unwrap();
        let lock = refresh_dir.join("demo.lock");
        fs::write(&lock, b"").unwrap();
        write_file(&source.join("SKILL.md"), "v3\n");
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "v2\n",
            "a held lock must cause the refresh to skip, not race"
        );

        // Once the lock is released the next refresh converges to the new content.
        fs::remove_file(&lock).unwrap();
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "v3\n"
        );
    }

    /// #4256: a failure during the swap must not leak the staging dir.
    #[test]
    fn refresh_cleans_up_staging_on_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source-skills").join("demo");
        write_file(&source.join("SKILL.md"), "x\n");

        // Make the managed parent a regular file so the swap fails *after* the staging copy
        // succeeds, exercising the error cleanup path.
        let managed_parent = managed_skills_root(root);
        write_file(&managed_parent, "not a dir\n");
        let managed_dir = managed_parent.join("demo");

        assert!(
            refresh_managed_skill_dir(root, "demo", &source, &managed_dir).is_err(),
            "swap must fail when the managed parent is a file"
        );

        // No staging dir may leak on the error path.
        let refresh_dir = root.join(".skill-refresh");
        let leaked: Vec<_> = fs::read_dir(&refresh_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .collect();
        assert!(leaked.is_empty(), "staging dir leaked on error: {leaked:?}");
    }

    /// #4256: a lock abandoned by a crashed process (a dead PID) must NOT wedge refresh
    /// forever. It is recovered on acquire, and initial population of an absent managed dir
    /// still proceeds — the exact permanent-staleness class #4256 must eliminate.
    #[test]
    fn dead_holder_lock_is_recovered_on_first_install() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source-skills").join("demo");
        write_file(&source.join("SKILL.md"), "fresh\n");

        // A leftover lock owned by a PID that cannot exist (well above any OS pid_max, still
        // positive as pid_t so kill() reports ESRCH rather than targeting a process group).
        let refresh_dir = root.join(".skill-refresh");
        fs::create_dir_all(&refresh_dir).unwrap();
        fs::write(refresh_dir.join("demo.lock"), b"999999999\n").unwrap();

        // managed does not exist yet: recovery must not block the initial copy.
        let managed = ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "fresh\n",
            "a dead holder's lock must be recovered so first-install populates the cache"
        );
    }

    /// #4256: a lock held by a genuinely live process must still cause a skip, never a steal.
    #[test]
    fn live_holder_lock_causes_skip() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source-skills").join("demo");
        write_file(&source.join("SKILL.md"), "v1\n");
        let managed = ensure_managed_skill_dir(root, "demo", &source).unwrap();

        // Our own (live) PID in a freshly written lock: alive and well within the TTL.
        let refresh_dir = root.join(".skill-refresh");
        fs::create_dir_all(&refresh_dir).unwrap();
        let lock = refresh_dir.join("demo.lock");
        fs::write(&lock, std::process::id().to_string().as_bytes()).unwrap();

        write_file(&source.join("SKILL.md"), "v2\n");
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "v1\n",
            "a live holder's lock must cause skip, not a steal-and-race"
        );

        // Releasing the live holder's lock lets the next refresh converge.
        fs::remove_file(&lock).unwrap();
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "v2\n"
        );
    }

    /// #4256: an old lock (mtime far past the TTL) owned by a LIVE pid must NOT be stolen --
    /// liveness is authoritative over the mtime-TTL backstop, so a slow-but-active holder is
    /// never stolen out from under its own copy/swap.
    #[test]
    fn old_lock_with_live_holder_is_not_stolen() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let source = root.join("source-skills").join("demo");
        write_file(&source.join("SKILL.md"), "v1\n");
        let managed = ensure_managed_skill_dir(root, "demo", &source).unwrap();

        // Ancient mtime, but the recorded pid (ours) is alive.
        let refresh_dir = root.join(".skill-refresh");
        fs::create_dir_all(&refresh_dir).unwrap();
        let lock = refresh_dir.join("demo.lock");
        fs::write(&lock, format!("{}:0", std::process::id())).unwrap();
        filetime::set_file_mtime(&lock, FileTime::from_unix_time(1_000_000, 0)).unwrap();

        write_file(&source.join("SKILL.md"), "v2\n");
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "v1\n",
            "an old lock held by a live pid must not be stolen, regardless of age"
        );

        // Once released, the next refresh converges.
        fs::remove_file(&lock).unwrap();
        ensure_managed_skill_dir(root, "demo", &source).unwrap();
        assert_eq!(
            fs::read_to_string(managed.join("SKILL.md")).unwrap(),
            "v2\n"
        );
    }
}
