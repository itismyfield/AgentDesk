use std::path::{Path, PathBuf};

use super::git_command;

/// Canonicalize a path, stripping the Windows `\\?\` extended-length prefix
/// that `std::fs::canonicalize` adds on Windows. Without stripping, these
/// UNC-prefixed paths fail to match user-supplied or config-supplied paths
/// and break repo directory lookups in tests and production on Windows.
fn safe_canonicalize(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    strip_unc_prefix(canonical)
}

/// Strip the `\\?\` extended-length prefix from a Windows path.
/// On non-Windows platforms this is a no-op.
fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    let _ = &path;
    path
}

/// Resolve the AgentDesk git repository directory.
///
/// Priority: `AGENTDESK_REPO_DIR` env -> scan all known roots for a git workspace -> `~/AgentDesk`.
pub fn resolve_repo_dir() -> Option<String> {
    if let Ok(d) = std::env::var("AGENTDESK_REPO_DIR") {
        let trimmed = d.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    let is_git_dir = |p: &Path| p.join(".git").exists();

    let mut candidates = Vec::new();
    if let Some(root) = crate::config::runtime_root() {
        candidates.push(root.join("workspaces").join("agentdesk"));
    }
    for ws in &candidates {
        if is_git_dir(ws) {
            return Some(ws.to_string_lossy().into_owned());
        }
    }

    let legacy = dirs::home_dir().map(|h| h.join("AgentDesk"));
    if let Some(ref p) = legacy {
        if is_git_dir(p) {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    legacy.map(|p| p.to_string_lossy().into_owned())
}

fn expand_tilde(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if path == "~" {
            return home.display().to_string();
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return home.join(rest).display().to_string();
        }
    }
    path.to_string()
}

pub(crate) fn looks_like_explicit_repo_path(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }

    if trimmed.starts_with(r"\\?\UNC\") || trimmed.starts_with(r"\\") || trimmed.starts_with("//") {
        return true;
    }

    let trimmed = trimmed
        .strip_prefix(r"\\?\")
        .or_else(|| trimmed.strip_prefix(r"\\.\"))
        .unwrap_or(trimmed);

    if trimmed.starts_with('/')
        || trimmed.starts_with("~/")
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
    {
        return true;
    }

    let bytes = trimmed.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn load_repo_resolution_config() -> Option<(crate::config::Config, PathBuf)> {
    let explicit = std::env::var_os("AGENTDESK_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path);
    }
    if let Some(root) = crate::config::runtime_root() {
        candidates.push(crate::runtime_layout::config_file_path(&root));
        candidates.push(crate::runtime_layout::legacy_config_file_path(&root));
    }

    for path in candidates {
        if !path.is_file() {
            continue;
        }
        if let Ok(config) = crate::config::load_from_path(&path) {
            let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
            return Some((config, base_dir.to_path_buf()));
        }
    }

    None
}

fn configured_repo_dir(repo_id: &str) -> Option<String> {
    let (config, base_dir) = load_repo_resolution_config()?;
    let raw = config.github.repo_dirs.get(repo_id)?.trim();
    if raw.is_empty() {
        return None;
    }

    let expanded = expand_tilde(raw);
    let path = PathBuf::from(expanded);
    let resolved = if path.is_relative() {
        base_dir.join(path)
    } else {
        path
    };

    Some(safe_canonicalize(&resolved).to_string_lossy().into_owned())
}

pub(crate) fn repo_id_for_dir(repo_dir: &str) -> Option<String> {
    let output = git_command()
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    super::parse_github_repo_from_remote(&String::from_utf8_lossy(&output.stdout))
}

fn ensure_git_worktree(path: &str) -> Result<(), String> {
    let output = git_command()
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .output()
        .map_err(|e| format!("git rev-parse failed for '{}': {e}", path))?;
    if !output.status.success() {
        return Err(format!("'{}' is not a git worktree", path));
    }
    Ok(())
}

/// Resolve the local git directory for a specific GitHub repo id.
///
/// Resolution order:
/// 1. `github.repo_dirs[repo_id]` from runtime config
/// 2. the current default repo iff its `origin` remote matches `repo_id`
///
/// When `repo_id` is absent, this simply mirrors `resolve_repo_dir()`.
/// When `repo_id` is present but no mapping exists, this returns an error
/// instead of silently falling back to the default repo.
pub fn resolve_repo_dir_for_id(repo_id: Option<&str>) -> Result<Option<String>, String> {
    let requested = repo_id.map(str::trim).filter(|value| !value.is_empty());
    let Some(requested) = requested else {
        return Ok(resolve_repo_dir());
    };

    if let Some(mapped_dir) = configured_repo_dir(requested) {
        ensure_git_worktree(&mapped_dir)?;
        if let Some(actual_repo_id) = repo_id_for_dir(&mapped_dir) {
            if actual_repo_id != requested {
                return Err(format!(
                    "Configured repo dir '{}' resolves to '{}' instead of requested '{}'",
                    mapped_dir, actual_repo_id, requested
                ));
            }
        }
        return Ok(Some(mapped_dir));
    }

    if let Some(default_dir) = resolve_repo_dir() {
        if repo_id_for_dir(&default_dir).as_deref() == Some(requested) {
            return Ok(Some(default_dir));
        }
    }

    Err(format!(
        "No local repo mapping for '{}'; configure github.repo_dirs.{} in agentdesk config",
        requested, requested
    ))
}

pub fn resolve_repo_dir_for_target(target_repo: Option<&str>) -> Result<Option<String>, String> {
    let requested = target_repo.map(str::trim).filter(|value| !value.is_empty());
    let Some(requested) = requested else {
        return Ok(resolve_repo_dir());
    };

    if looks_like_explicit_repo_path(requested) {
        let expanded = expand_tilde(requested);
        let path = PathBuf::from(expanded);
        let resolved = if path.is_relative() {
            std::env::current_dir()
                .map_err(|e| format!("cannot resolve repo path '{}': {}", requested, e))?
                .join(path)
        } else {
            path
        };
        let canonical = safe_canonicalize(&resolved);
        let canonical_str = canonical.to_string_lossy().into_owned();
        ensure_git_worktree(&canonical_str)?;
        return Ok(Some(canonical_str));
    }

    resolve_repo_dir_for_id(Some(requested))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_repo_dir_returns_some() {
        let dir = resolve_repo_dir();
        assert!(dir.is_some(), "resolve_repo_dir should return Some");
    }

    #[test]
    fn looks_like_explicit_repo_path_accepts_windows_verbatim_paths() {
        assert!(looks_like_explicit_repo_path(r"\\?\C:\tmp\repo"));
        assert!(looks_like_explicit_repo_path(r"\\?\UNC\server\share\repo"));
    }

    #[test]
    fn resolve_repo_dir_env_override() {
        unsafe { std::env::set_var("AGENTDESK_REPO_DIR", "/tmp/fake-repo") };
        let dir = resolve_repo_dir();
        unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") };
        assert_eq!(dir, Some("/tmp/fake-repo".to_string()));
    }
}
