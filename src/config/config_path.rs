//! The one resolver for which `agentdesk.yaml` a process reads and writes. The first
//! existing candidate wins; one that then fails to load never hands over to the next.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Result;

use super::Config;
use crate::runtime_layout::{config_file_path, legacy_config_file_path};

const EXPLICIT_ENV: &str = "AGENTDESK_CONFIG";

/// The file `load`, `load_graceful` and the config watcher use: `$AGENTDESK_CONFIG`,
/// the runtime root, the working directory, then `~/.adk/release`.
pub fn resolved_config_path() -> PathBuf {
    resolve_with(
        explicit_env(),
        super::runtime_root(),
        std::env::current_dir().ok(),
        dirs::home_dir(),
    )
}

/// The config file of `root`. `$AGENTDESK_CONFIG` applies only when `root` is this
/// process's runtime root, so work on another root never reads the process's file.
pub fn config_path_in(root: &Path) -> PathBuf {
    let explicit = explicit_env().filter(|_| super::runtime_root().as_deref() == Some(root));
    search(explicit, &[root.to_path_buf()]).unwrap_or_else(|| config_file_path(root))
}

/// Loads `path`: `Ok(None)` when it is absent, and an error when it exists but fails to
/// load. Some callers poll, so each broken file version is warned about once.
pub fn load_config_file(path: &Path) -> Result<Option<Config>> {
    if !path.is_file() {
        return Ok(None);
    }
    super::load_from_path(path)
        .map(Some)
        .inspect_err(|error| warn_unloadable(path, format_args!("{error:#}")))
}

/// One WARN per file version for a resolved config file that failed to load.
pub(crate) fn warn_unloadable(path: &Path, error: std::fmt::Arguments<'_>) {
    if first_failure_of_this_version(path) {
        tracing::warn!(
            path = %path.display(),
            "config file failed to load; no other candidate is tried: {error}"
        );
    }
}

fn first_failure_of_this_version(path: &Path) -> bool {
    static WARNED: Mutex<BTreeMap<PathBuf, Option<SystemTime>>> = Mutex::new(BTreeMap::new());
    let modified = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok();
    let mut warned = WARNED.lock().unwrap_or_else(|poison| poison.into_inner());
    warned.insert(path.to_path_buf(), modified) != Some(modified)
}

/// The runtime root's config file (or `$AGENTDESK_CONFIG`) with its path; `None` when
/// it is absent or fails to load.
pub fn load_runtime_config_file() -> Option<(Config, PathBuf)> {
    let path = config_path_in(&super::runtime_root()?);
    let config = load_config_file(&path).ok()??;
    Some((config, path))
}

fn explicit_env() -> Option<PathBuf> {
    std::env::var_os(EXPLICIT_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn resolve_with(
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
fn search(explicit: Option<PathBuf>, roots: &[PathBuf]) -> Option<PathBuf> {
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

#[cfg(test)]
#[path = "config_path/tests.rs"]
mod tests;
