use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

const COPY_PRUNE_DIR_NAMES: &[&str] =
    &[".git", "node_modules", "target", "dist", ".venv", ".cache"];

pub(super) fn copy_tree_filtered(
    source_root: &Path,
    destination_root: &Path,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    fs::create_dir_all(destination_root)
        .map_err(|e| format!("Failed to create '{}': {e}", destination_root.display()))?;

    let mut queue = VecDeque::from([(source_root.to_path_buf(), destination_root.to_path_buf())]);
    while let Some((source_dir, destination_dir)) = queue.pop_front() {
        let entries = fs::read_dir(&source_dir)
            .map_err(|e| format!("Failed to read '{}': {e}", source_dir.display()))?;
        for entry in entries.flatten() {
            let source_path = entry.path();
            let destination_path = destination_dir.join(entry.file_name());
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                warnings.push(format!(
                    "Skipped symlink during workspace copy: {}",
                    source_path.display()
                ));
                continue;
            }
            if file_type.is_dir() {
                if should_prune_workspace_dir(&source_path) {
                    warnings.push(format!(
                        "Skipped workspace directory during copy: {}",
                        source_path.display()
                    ));
                    continue;
                }
                fs::create_dir_all(&destination_path).map_err(|e| {
                    format!("Failed to create '{}': {e}", destination_path.display())
                })?;
                queue.push_back((source_path, destination_path));
                continue;
            }
            if file_type.is_file() {
                if let Some(parent) = destination_path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
                }
                fs::copy(&source_path, &destination_path).map_err(|e| {
                    format!(
                        "Failed to copy '{}' to '{}': {e}",
                        source_path.display(),
                        destination_path.display()
                    )
                })?;
            }
        }
    }

    Ok(())
}

fn should_prune_workspace_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            COPY_PRUNE_DIR_NAMES
                .iter()
                .any(|candidate| candidate == &name)
        })
        .unwrap_or(false)
}

pub(super) fn write_text_file(
    path: &Path,
    content: &str,
    runtime_root: &Path,
    backups_root: &Path,
    overwrite: bool,
) -> Result<(), String> {
    if path.exists() {
        let existing = fs::read_to_string(path).unwrap_or_default();
        if existing == content {
            return Ok(());
        }
        if !overwrite {
            return Err(format!(
                "Target file '{}' already exists. Re-run with --overwrite to replace it.",
                path.display()
            ));
        }
        backup_existing_path(path, runtime_root, backups_root)?;
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    fs::write(path, content).map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    Ok(())
}

pub(super) fn backup_existing_path(
    path: &Path,
    runtime_root: &Path,
    backups_root: &Path,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }

    let relative = path
        .strip_prefix(runtime_root)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| path.file_name().map(PathBuf::from))
        .ok_or_else(|| format!("Failed to derive backup path for '{}'.", path.display()))?;
    let backup_path = backups_root.join(relative);
    if backup_path.exists() {
        return Ok(());
    }

    if path.is_dir() {
        let mut backup_warnings = Vec::new();
        copy_tree_filtered(path, &backup_path, &mut backup_warnings)?;
    } else {
        if let Some(parent) = backup_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
        }
        fs::copy(path, &backup_path).map_err(|e| {
            format!(
                "Failed to back up '{}' to '{}': {e}",
                path.display(),
                backup_path.display()
            )
        })?;
    }

    Ok(())
}

pub(super) fn write_json_file(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    let rendered = serde_json::to_string_pretty(value)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    Ok(())
}

pub(super) fn write_yaml_file(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    let rendered = serde_yaml::to_string(value)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    Ok(())
}
