use super::paths::{current_home_dir, expand_user_path};
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

        for workspace_target in deployment.workspace_targets {
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
            workspace_targets_for_names(root, workspace_names, &providers)
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
        workspace_targets: workspace_targets_for_names(root, workspace_names, &providers),
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
    if !same_canonical_path(&managed_dir, source_skill_dir)
        && !managed_dir.join("SKILL.md").is_file()
    {
        copy_skill_dir_resolving_symlinks(source_skill_dir, &managed_dir)?;
    }
    rewrite_managed_skill_paths(skill_name, &managed_dir)?;
    Ok(managed_dir)
}

fn rewrite_managed_skill_paths(skill_name: &str, skill_dir: &Path) -> Result<(), String> {
    let files = match skill_name {
        "memory-read" | "memory-write" => vec![skill_dir.join("SKILL.md")],
        "memory-merge" => vec![
            skill_dir.join("SKILL.md"),
            skill_dir.join("references").join("architecture.md"),
            skill_dir.join("references").join("classification-guide.md"),
            skill_dir.join("references").join("phase-details.md"),
            skill_dir.join("references").join("report-template.md"),
        ],
        _ => return Ok(()),
    };

    for path in files {
        rewrite_text_file_paths(&path)?;
    }
    Ok(())
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

    let (source_path, link_path, is_dir_link) = if provider == "claude" {
        (
            skill_dir.join("SKILL.md"),
            target_dir.join(format!("{skill_name}.md")),
            false,
        )
    } else {
        (skill_dir.to_path_buf(), target_dir.join(skill_name), true)
    };

    if !path_exists(&source_path) {
        return Ok(LinkState::SkippedExisting);
    }

    if let Ok(existing_target) = fs::read_link(&link_path) {
        let desired = relative_path_from(
            link_path.parent().unwrap_or(target_dir),
            &source_path.canonicalize().unwrap_or(source_path.clone()),
        );
        if existing_target == desired {
            return Ok(LinkState::Unchanged);
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

fn copy_skill_dir_resolving_symlinks(src: &Path, dest: &Path) -> Result<(), String> {
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
