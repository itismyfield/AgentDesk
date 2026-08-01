use super::formatting::{BUILTIN_SKILLS, extract_skill_description};
use crate::services::provider::ProviderKind;
use std::fs;
use std::path::Path;

/// Scan for provider-specific skills available to this bot.
pub(in crate::services) fn scan_skills(
    provider: &ProviderKind,
    project_path: Option<&str>,
) -> Vec<(String, String)> {
    if let Some(root) = crate::config::runtime_root() {
        let _ = crate::runtime_layout::sync_managed_skills(&root);
    }

    let mut skills: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    match provider {
        ProviderKind::Claude => {
            for (name, desc) in BUILTIN_SKILLS {
                seen.insert(name.to_string());
                skills.push((name.to_string(), desc.to_string()));
            }

            let dirs_to_scan = collect_provider_skill_roots(provider, project_path);

            for dir in dirs_to_scan {
                if !dir.is_dir() {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false) {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            let name = stem.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&path)
                                    .ok()
                                    .map(|content| extract_skill_description(&content))
                                    .unwrap_or_else(|| format!("Skill: {}", name));
                                skills.push((name, desc));
                            }
                        }
                    }
                }
            }
        }
        ProviderKind::Codex
        | ProviderKind::Gemini
        | ProviderKind::OpenCode
        | ProviderKind::Qwen => {
            scan_directory_skills(
                collect_provider_skill_roots(provider, project_path),
                &mut seen,
                &mut skills,
            );
        }
        ProviderKind::Unsupported(_) => {}
    }

    skills.sort_by(|a, b| a.0.cmp(&b.0));
    skills
}

/// Compute a lightweight fingerprint of skill directories: (file_count, max_mtime_epoch).
/// Used by the hot-reload poll to detect additions, modifications, and deletions.
fn skill_dir_fingerprint(provider: &ProviderKind) -> (usize, u64) {
    let mut count = 0usize;
    let mut max_mtime = 0u64;

    let mut dirs = collect_provider_skill_roots(provider, None);
    if provider_supports_directory_skills(provider) {
        if let Some(root) = crate::config::runtime_root() {
            dirs.push(crate::runtime_layout::managed_skills_root(&root));
        }
    }

    fn walk_mtime(dir: &Path, count: &mut usize, max_mtime: &mut u64) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_mtime(&path, count, max_mtime);
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                *count += 1;
                if let Ok(meta) = fs::metadata(&path) {
                    if let Ok(mt) = meta.modified() {
                        let epoch = mt
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        if epoch > *max_mtime {
                            *max_mtime = epoch;
                        }
                    }
                }
            }
        }
    }

    for dir in &dirs {
        walk_mtime(dir, &mut count, &mut max_mtime);
    }

    (count, max_mtime)
}

/// Like `skill_dir_fingerprint` but also includes project-level skill directories.
pub(in crate::services::discord) fn skill_dir_fingerprint_with_projects(
    provider: &ProviderKind,
    project_paths: &[String],
) -> (usize, u64) {
    let (mut count, mut max_mtime) = skill_dir_fingerprint(provider);

    fn walk_mtime(dir: &Path, count: &mut usize, max_mtime: &mut u64) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_mtime(&path, count, max_mtime);
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                *count += 1;
                if let Ok(meta) = fs::metadata(&path) {
                    if let Ok(mt) = meta.modified() {
                        let epoch = mt
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        if epoch > *max_mtime {
                            *max_mtime = epoch;
                        }
                    }
                }
            }
        }
    }

    for path in project_paths {
        let Some(proj_dir) = provider_project_skill_dir(provider, path) else {
            continue;
        };
        if proj_dir.is_dir() {
            walk_mtime(&proj_dir, &mut count, &mut max_mtime);
        }
    }

    (count, max_mtime)
}

fn provider_supports_directory_skills(provider: &ProviderKind) -> bool {
    matches!(
        provider,
        ProviderKind::Claude
            | ProviderKind::Codex
            | ProviderKind::Gemini
            | ProviderKind::OpenCode
            | ProviderKind::Qwen
    )
}

fn provider_home_skill_dir(provider: &ProviderKind, home: &Path) -> Option<std::path::PathBuf> {
    match provider {
        ProviderKind::Claude => Some(home.join(".claude").join("commands")),
        ProviderKind::Codex => Some(home.join(".codex").join("skills")),
        ProviderKind::Gemini => Some(home.join(".gemini").join("skills")),
        ProviderKind::OpenCode => Some(home.join(".opencode").join("skills")),
        ProviderKind::Qwen => Some(home.join(".qwen").join("skills")),
        ProviderKind::Unsupported(_) => None,
    }
}

fn provider_project_skill_dir(
    provider: &ProviderKind,
    project_path: &str,
) -> Option<std::path::PathBuf> {
    let project_root = Path::new(project_path);
    match provider {
        ProviderKind::Claude => Some(project_root.join(".claude").join("commands")),
        ProviderKind::Codex => Some(project_root.join(".codex").join("skills")),
        ProviderKind::Gemini => Some(project_root.join(".gemini").join("skills")),
        ProviderKind::OpenCode => Some(project_root.join(".opencode").join("skills")),
        ProviderKind::Qwen => Some(project_root.join(".qwen").join("skills")),
        ProviderKind::Unsupported(_) => None,
    }
}

fn collect_provider_skill_roots(
    provider: &ProviderKind,
    project_path: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        if let Some(path) = provider_home_skill_dir(provider, &home) {
            roots.push(path);
        }
    }
    if let Some(project_path) = project_path {
        if let Some(path) = provider_project_skill_dir(provider, project_path) {
            roots.push(path);
        }
    }
    roots
}

fn scan_directory_skills(
    roots: Vec<std::path::PathBuf>,
    seen: &mut std::collections::HashSet<String>,
    skills: &mut Vec<(String, String)>,
) {
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            collect_directory_skill(&path, seen, skills);

            if !path.is_dir() {
                continue;
            }
            let Ok(nested) = fs::read_dir(&path) else {
                continue;
            };
            for child in nested.filter_map(|e| e.ok()) {
                collect_directory_skill(&child.path(), seen, skills);
            }
        }
    }
}

fn collect_directory_skill(
    path: &Path,
    seen: &mut std::collections::HashSet<String>,
    skills: &mut Vec<(String, String)>,
) {
    let Some(skill_path) = resolve_codex_skill_file(path) else {
        return;
    };
    let Some(name) = skill_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
    else {
        return;
    };
    let name = name.to_string();
    if !seen.insert(name.clone()) {
        return;
    }
    let desc = fs::read_to_string(&skill_path)
        .ok()
        .map(|content| extract_skill_description(&content))
        .unwrap_or_else(|| format!("Skill: {}", name));
    skills.push((name, desc));
}

fn resolve_codex_skill_file(path: &Path) -> Option<std::path::PathBuf> {
    if path.is_dir() {
        let skill_path = path.join("SKILL.md");
        if skill_path.is_file() {
            return Some(skill_path);
        }
    }
    None
}
