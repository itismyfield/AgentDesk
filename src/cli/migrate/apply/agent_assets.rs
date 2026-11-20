use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{self, AgentChannels, AgentDef as RuntimeAgentDef};
use crate::runtime_layout;

use super::super::OpenClawMigrateArgs;
use super::super::plan::{ImportAgentPlan, ImportPlan};
use super::BOOTSTRAP_PROMPT_SECTIONS;
use super::fs_write::write_text_file;

pub(super) fn merge_imported_agents(
    config: &mut config::Config,
    plan: &ImportPlan,
    overwrite: bool,
) -> Result<(), String> {
    for agent in plan.agents.iter().filter(|agent| agent.eligible_for_v1) {
        let provider = agent
            .mapped_provider
            .clone()
            .ok_or_else(|| format!("agent '{}' is missing a mapped provider", agent.source_id))?;
        let incoming = RuntimeAgentDef {
            id: agent.final_role_id.clone(),
            name: agent.display_name.clone(),
            name_ko: None,
            provider,
            channels: AgentChannels::default(),
            peer_keywords: Vec::new(),
            peer_directory: true,
            keywords: Vec::new(),
            department: None,
            avatar_emoji: agent.avatar_emoji.clone(),
            preferred_intake_node_labels: None,
            memory_group: None,
        };

        if let Some(existing) = config
            .agents
            .iter_mut()
            .find(|existing| existing.id == incoming.id)
        {
            let same_agent = existing.name == incoming.name
                && existing.provider == incoming.provider
                && existing.avatar_emoji == incoming.avatar_emoji
                && existing.channels == incoming.channels
                && existing.department == incoming.department;
            if same_agent {
                continue;
            }
            if !overwrite {
                return Err(format!(
                    "Target agent '{}' already exists in agentdesk.yaml. Re-run with --overwrite to replace generated assets.",
                    incoming.id
                ));
            }
            *existing = incoming;
        } else {
            config.agents.push(incoming);
        }
    }

    config.agents.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(())
}

pub(super) fn render_imported_prompt(
    agent: &ImportAgentPlan,
    runtime_root: &Path,
    args: &OpenClawMigrateArgs,
) -> String {
    let workspace = Path::new(&agent.workspace_source);
    let memory_dir = runtime_layout::long_term_memory_root(runtime_root).join(&agent.final_role_id);
    let workspace_dir = runtime_root
        .join("openclaw")
        .join("workspaces")
        .join(&agent.final_role_id);
    let mut sections = vec![format!(
        "# Imported OpenClaw Role\n\n- role_id: `{}`\n- source_agent: `{}`\n- agentdesk_memory_dir: `{}`\n- agentdesk_workspace_dir: `{}`\n- source_workspace: `{}`\n\n## AgentDesk Runtime References\n\nUse AgentDesk-managed runtime paths when they exist. Treat the original OpenClaw workspace as provenance, not the default live runtime state.",
        agent.final_role_id,
        agent.source_id,
        if args.no_memory {
            "not imported (--no-memory)".to_string()
        } else {
            memory_dir.display().to_string()
        },
        if args.no_workspace {
            "not copied (--no-workspace)".to_string()
        } else {
            workspace_dir.display().to_string()
        },
        agent.workspace_source
    )];

    for (file_name, heading) in BOOTSTRAP_PROMPT_SECTIONS {
        let path = workspace.join(file_name);
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        sections.push(format!("## {heading}\n\n{trimmed}"));
    }

    sections.join("\n\n")
}

pub(super) fn import_memory_files(
    agent: &ImportAgentPlan,
    destination_root: &Path,
    runtime_root: &Path,
    backups_root: &Path,
    overwrite: bool,
) -> Result<Vec<PathBuf>, String> {
    fs::create_dir_all(destination_root)
        .map_err(|e| format!("Failed to create '{}': {e}", destination_root.display()))?;

    let workspace = Path::new(&agent.workspace_source);
    let mut outputs = Vec::new();

    let memory_md = workspace.join("MEMORY.md");
    if memory_md.is_file() {
        let content = fs::read_to_string(&memory_md)
            .map_err(|e| format!("Failed to read '{}': {e}", memory_md.display()))?;
        let destination = destination_root.join("MEMORY.md");
        write_text_file(
            &destination,
            &content,
            runtime_root,
            backups_root,
            overwrite,
        )?;
        outputs.push(destination);
    }

    let daily_root = workspace.join("memory");
    if daily_root.is_dir() {
        let mut queue = VecDeque::from([daily_root.clone()]);
        while let Some(dir) = queue.pop_front() {
            let entries = fs::read_dir(&dir)
                .map_err(|e| format!("Failed to read '{}': {e}", dir.display()))?;
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
                if !file_type.is_file()
                    || path.extension().and_then(|ext| ext.to_str()) != Some("md")
                {
                    continue;
                }

                let relative = path
                    .strip_prefix(workspace)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let daily_name = format!(
                    "daily-{}",
                    path.strip_prefix(&daily_root)
                        .unwrap_or(&path)
                        .display()
                        .to_string()
                        .replace('/', "-")
                        .replace('\\', "-")
                );
                let content = fs::read_to_string(&path)
                    .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
                let rendered = format!(
                    "---\nimported_from: openclaw\nsource_agent: {}\nsource_path: {}\n---\n\n{}",
                    agent.source_id, relative, content
                );
                let destination = destination_root.join(daily_name);
                write_text_file(
                    &destination,
                    &rendered,
                    runtime_root,
                    backups_root,
                    overwrite,
                )?;
                outputs.push(destination);
            }
        }
    }

    outputs.sort();
    Ok(outputs)
}
