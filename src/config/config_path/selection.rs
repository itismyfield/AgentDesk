//! File selection shared by the daemon and the deployment preflight tool.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub(crate) fn config_file_path(root: &Path) -> PathBuf {
    root.join("config").join("agentdesk.yaml")
}

pub(crate) fn legacy_config_file_path(root: &Path) -> PathBuf {
    root.join("agentdesk.yaml")
}

/// `$AGENTDESK_CONFIG` selects an explicit config path before the normal root search.
/// An unset or empty value leaves the normal search in effect.
pub(super) fn explicit_env() -> Option<PathBuf> {
    std::env::var_os("AGENTDESK_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(super) fn resolve_with(
    explicit: Option<PathBuf>,
    runtime_root: Option<PathBuf>,
    cwd: Option<PathBuf>,
    home: Option<PathBuf>,
) -> PathBuf {
    let release_root = home.map(|home| home.join(".adk").join("release"));
    let roots: Vec<PathBuf> = [runtime_root.clone(), cwd, release_root]
        .into_iter()
        .flatten()
        .collect();
    search(explicit, &roots).unwrap_or_else(|| match runtime_root {
        Some(root) => config_file_path(&root),
        None => config_file_path(Path::new("")),
    })
}

/// An explicit path always decides (its other-layout sibling stands in only when the
/// path itself is missing); otherwise the first existing file under `roots`.
pub(super) fn search(explicit: Option<PathBuf>, roots: &[PathBuf]) -> Option<PathBuf> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Some(path);
        }
        let sibling = other_layout(&path).filter(|sibling| sibling.is_file());
        return Some(sibling.unwrap_or(path));
    }
    roots
        .iter()
        .flat_map(|root| [config_file_path(root), legacy_config_file_path(root)])
        .find(|path| path.is_file())
}

/// `<root>/config/agentdesk.yaml` and `<root>/agentdesk.yaml` name the same file in the
/// two layouts.
fn other_layout(path: &Path) -> Option<PathBuf> {
    if path.file_name()? != OsStr::new("agentdesk.yaml") {
        return None;
    }
    let parent = path.parent()?;
    if parent.file_name() == Some(OsStr::new("config")) {
        parent.parent().map(legacy_config_file_path)
    } else {
        Some(config_file_path(parent))
    }
}
