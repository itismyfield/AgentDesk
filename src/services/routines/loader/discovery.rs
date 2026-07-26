use super::{LoadedRoutineScript, RoutineScriptCandidate, full_source_version};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

pub(super) fn stable_absolute_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn root_config_identity(root: &Path) -> PathBuf {
    stable_absolute_path(root)
}

pub(super) fn candidate_failure_key(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| stable_absolute_path(path))
}

pub(super) fn routine_roots_identity(roots: &[PathBuf]) -> PathBuf {
    let identities = roots
        .iter()
        .map(|root| root_config_identity(root).to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\0");
    PathBuf::from(full_source_version(&identities))
}

pub(super) fn script_ref(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn add_cached_candidates_for_root(
    existing_scripts: &HashMap<String, LoadedRoutineScript>,
    candidates_by_ref: &mut BTreeMap<String, Vec<RoutineScriptCandidate>>,
    seen_refs: &mut HashSet<String>,
    root_index: usize,
    root: &Path,
) {
    for (script_ref, script) in existing_scripts
        .iter()
        .filter(|(_, script)| script.file.starts_with(root))
    {
        seen_refs.insert(script_ref.clone());
        candidates_by_ref
            .entry(script_ref.clone())
            .or_default()
            .push(RoutineScriptCandidate {
                root_index,
                root: root.to_path_buf(),
                path: script.file.clone(),
                cached: Some(script.clone()),
            });
    }
}

pub(super) fn collect_routine_script_paths(
    root: &Path,
    exclude_bundled_node_helpers: bool,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    collect_routine_script_paths_inner(root, root, exclude_bundled_node_helpers, out)
}

fn collect_routine_script_paths_inner(
    root: &Path,
    current_dir: &Path,
    exclude_bundled_node_helpers: bool,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(current_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_routine_script_paths_inner(root, &path, exclude_bundled_node_helpers, out)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "js") {
            if exclude_bundled_node_helpers && is_bundled_node_only_helper(root, &path) {
                tracing::debug!(
                    routine_script = %path.display(),
                    "excluded Node-only worktree inventory helper from QuickJS discovery"
                );
            } else {
                out.push(path);
            }
        }
    }
    Ok(())
}

// #4900/#4902: `local-worktree-gc.js` executes this bundled read-only helper with Node.
fn is_bundled_node_only_helper(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        relative == Path::new("monitoring").join("local_worktree_inventory.js")
    })
}
