use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::super::plan::ImportPlan;
use super::super::source::ResolvedSourceRoot;
use super::super::{DiscordTokenMode, OpenClawMigrateArgs};
use super::BOOTSTRAP_PROMPT_SECTIONS;
use super::discord_auth::planned_bot_account_ids;
use super::secrets::resolve_account_token;

pub(super) fn source_fingerprint(
    source: &ResolvedSourceRoot,
    plan: &ImportPlan,
    args: &OpenClawMigrateArgs,
) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hasher.update(source.resolved_config_json.as_bytes());
    for path in &source.resolved_config_paths {
        hasher.update(path.display().to_string().as_bytes());
    }

    for agent_id in &plan.selected_agent_ids {
        hasher.update(agent_id.as_bytes());
    }

    let mut inputs = BTreeMap::<String, String>::new();
    for agent in plan.agents.iter().filter(|agent| agent.eligible_for_v1) {
        let workspace = Path::new(&agent.workspace_source);
        collect_source_file_hash(&workspace.join("MEMORY.md"), &source.root, &mut inputs)?;
        collect_markdown_tree_hashes(&workspace.join("memory"), &source.root, &mut inputs)?;
        for (file_name, _) in BOOTSTRAP_PROMPT_SECTIONS {
            collect_source_file_hash(&workspace.join(file_name), &source.root, &mut inputs)?;
        }
        if !args.no_workspace {
            collect_tree_file_hashes(workspace, &source.root, &mut inputs)?;
        }
        collect_tree_file_hashes(
            &source
                .root
                .join("agents")
                .join(&agent.source_id)
                .join("sessions"),
            &source.root,
            &mut inputs,
        )?;
    }

    collect_resolved_bot_token_hashes(source, plan, args, &mut inputs)?;

    for (path, digest) in inputs {
        hasher.update(path.as_bytes());
        hasher.update(digest.as_bytes());
    }

    Ok(hex::encode(hasher.finalize()))
}

fn collect_resolved_bot_token_hashes(
    source: &ResolvedSourceRoot,
    plan: &ImportPlan,
    args: &OpenClawMigrateArgs,
    inputs: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    if !args.write_bot_settings {
        return Ok(());
    }
    let token_mode = DiscordTokenMode::parse(&args.discord_token_mode)?;
    if token_mode == DiscordTokenMode::Report {
        return Ok(());
    }

    for account_id in planned_bot_account_ids(plan) {
        let value = match resolve_account_token(source, &account_id, token_mode) {
            Ok(Some(token)) => token,
            Ok(None) => "__missing__".to_string(),
            Err(err) => format!("__error__:{err}"),
        };
        let mut file_hasher = Sha256::new();
        file_hasher.update(value.as_bytes());
        inputs.insert(
            format!("discord-token:{account_id}"),
            hex::encode(file_hasher.finalize()),
        );
    }

    Ok(())
}

fn collect_markdown_tree_hashes(
    root: &Path,
    source_root: &Path,
    inputs: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let entries =
            fs::read_dir(&dir).map_err(|e| format!("Failed to read '{}': {e}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                queue.push_back(path);
                continue;
            }
            if file_type.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                collect_source_file_hash(&path, source_root, inputs)?;
            }
        }
    }
    Ok(())
}

fn collect_tree_file_hashes(
    root: &Path,
    source_root: &Path,
    inputs: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let entries =
            fs::read_dir(&dir).map_err(|e| format!("Failed to read '{}': {e}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                queue.push_back(path);
                continue;
            }
            if file_type.is_file() {
                collect_source_file_hash(&path, source_root, inputs)?;
            }
        }
    }
    Ok(())
}

fn collect_source_file_hash(
    path: &Path,
    source_root: &Path,
    inputs: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let bytes = fs::read(path).map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut file_hasher = Sha256::new();
    file_hasher.update(bytes);
    let relative = path
        .strip_prefix(source_root)
        .map(|value| value.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    inputs.insert(relative, hex::encode(file_hasher.finalize()));
    Ok(())
}
