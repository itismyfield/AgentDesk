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
    if path == "~" || path.starts_with("~/") {
        if let Some(expanded) = crate::runtime_layout::expand_user_path(path) {
            return expanded.to_string_lossy().into_owned();
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

/// Stable sentinel prefix for the "no `repo_dirs` mapping" resolver error.
///
/// This is the *only* resolver failure that is purely a "not configured" signal
/// (no entry in `github.repo_dirs`, and the default repo's remote doesn't match).
/// Other failures from [`resolve_repo_dir_for_id`] / [`resolve_repo_dir_for_target`]
/// — invalid mapped dir, non-git worktree, wrong remote — are persistent
/// misconfiguration and must surface as real WARNs (#3566). Callers that want to
/// rate-suppress only the "not configured" case match on this via
/// [`is_no_repo_mapping_error`].
const NO_REPO_MAPPING_ERROR_PREFIX: &str = "No local repo mapping for";

/// Returns `true` when `error` is the "no `repo_dirs` mapping" resolver failure
/// (i.e. the repo simply isn't configured), as opposed to a persistent
/// misconfiguration error (invalid dir / non-git worktree / wrong remote).
///
/// Used by log call sites to rate-suppress only the benign "not configured"
/// case while keeping genuine configuration errors at WARN (#3566).
pub fn is_no_repo_mapping_error(error: &str) -> bool {
    error.starts_with(NO_REPO_MAPPING_ERROR_PREFIX)
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
        "{NO_REPO_MAPPING_ERROR_PREFIX} '{}'; configure github.repo_dirs.{} in agentdesk config",
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
mod no_repo_mapping_classification_tests {
    use super::{NO_REPO_MAPPING_ERROR_PREFIX, is_no_repo_mapping_error};

    #[test]
    fn classifies_the_no_mapping_error_as_suppressible() {
        // Mirrors the exact error produced by resolve_repo_dir_for_id when no
        // repo_dirs entry exists and the default remote doesn't match.
        let err = format!(
            "{NO_REPO_MAPPING_ERROR_PREFIX} 'owner/repo'; configure github.repo_dirs.owner/repo in agentdesk config"
        );
        assert!(is_no_repo_mapping_error(&err));
    }

    #[test]
    fn keeps_persistent_misconfiguration_errors_as_real_warns() {
        // These are persistent setup errors and must NOT be classified as the
        // benign "no mapping" case (#3566 over-suppress fix).
        assert!(!is_no_repo_mapping_error(
            "'/some/dir' is not a git worktree"
        ));
        assert!(!is_no_repo_mapping_error(
            "Configured repo dir '/some/dir' resolves to 'a/b' instead of requested 'c/d'"
        ));
        assert!(!is_no_repo_mapping_error(
            "git rev-parse failed for '/some/dir': boom"
        ));
        assert!(!is_no_repo_mapping_error(
            "cannot resolve repo path 'rel/path': boom"
        ));
    }
}
