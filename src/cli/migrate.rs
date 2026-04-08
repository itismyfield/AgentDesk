use std::path::{Path, PathBuf};

use clap::Args;

use crate::config;
use crate::utils::format::expand_tilde_path;

mod apply;
mod plan;
mod source;
#[cfg(test)]
mod tests;

use apply::apply_import_plan;
use plan::build_import_plan;
use source::resolve_source_root;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolPolicyMode {
    Report,
    BotIntersection,
    BotUnion,
}

impl ToolPolicyMode {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::BotIntersection => "bot-intersection",
            Self::BotUnion => "bot-union",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "report" => Ok(Self::Report),
            "bot-intersection" => Ok(Self::BotIntersection),
            "bot-union" => Ok(Self::BotUnion),
            _ => Err(format!(
                "Unsupported --tool-policy-mode '{}'. Expected one of: report, bot-intersection, bot-union.",
                raw
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscordTokenMode {
    Report,
    PlaintextOnly,
    ResolveEnvFile,
    ResolveAll,
}

impl DiscordTokenMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::PlaintextOnly => "plaintext-only",
            Self::ResolveEnvFile => "resolve-env-file",
            Self::ResolveAll => "resolve-all",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "report" => Ok(Self::Report),
            "plaintext-only" => Ok(Self::PlaintextOnly),
            "resolve-env-file" => Ok(Self::ResolveEnvFile),
            "resolve-all" => Ok(Self::ResolveAll),
            _ => Err(format!(
                "Unsupported --discord-token-mode '{}'. Expected one of: report, plaintext-only, resolve-env-file, resolve-all.",
                raw
            )),
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct OpenClawMigrateArgs {
    /// OpenClaw root path or openclaw.json path. Defaults to the current directory.
    #[arg(conflicts_with = "resume")]
    pub root_path: Option<String>,
    /// Override the target AgentDesk runtime root.
    #[arg(long = "agentdesk-root", value_name = "PATH")]
    pub agentdesk_root: Option<String>,
    /// Select a specific source agent id. May be repeated.
    #[arg(long = "agent", value_name = "AGENT_ID", conflicts_with = "all_agents")]
    pub agent_ids: Vec<String>,
    /// Import every source agent.
    #[arg(long, conflicts_with = "agent_ids")]
    pub all_agents: bool,
    /// Print the canonical import plan without writing files.
    #[arg(long)]
    pub dry_run: bool,
    /// Resume an unfinished import from $AGENTDESK_ROOT_DIR/openclaw/imports/<import_id>.
    #[arg(long, value_name = "IMPORT_ID", conflicts_with = "root_path")]
    pub resume: Option<String>,
    /// Fallback provider to use when the source provider is unsupported.
    #[arg(long)]
    pub fallback_provider: Option<String>,
    /// Rewrite absolute OpenClaw workspace prefixes during import planning. May be repeated as OLD=NEW.
    #[arg(long = "workspace-root-rewrite", value_name = "OLD=NEW")]
    pub workspace_root_rewrite: Vec<String>,
    /// Preview writing config/org.yaml mutations.
    #[arg(long)]
    pub write_org: bool,
    /// Preview writing config/bot_settings.json mutations.
    #[arg(long)]
    pub write_bot_settings: bool,
    /// Preview SQLite upserts.
    #[arg(long)]
    pub write_db: bool,
    /// Preview replacing generated artifacts for the selected role(s).
    #[arg(long)]
    pub overwrite: bool,
    /// Preview Discord channel binding imports.
    #[arg(long)]
    pub with_channel_bindings: bool,
    /// Preview lossy session import.
    #[arg(long)]
    pub with_sessions: bool,
    /// Preview snapshotting the source tree into audit outputs.
    #[arg(long)]
    pub snapshot_source: bool,
    /// Skip workspace copy while still inspecting source workspaces for prompt/memory.
    #[arg(long)]
    pub no_workspace: bool,
    /// Skip Markdown memory import.
    #[arg(long)]
    pub no_memory: bool,
    /// Skip merged prompt generation.
    #[arg(long)]
    pub no_prompts: bool,
    /// Tool-policy handling mode for bot_settings import.
    #[arg(long, default_value = "report")]
    pub tool_policy_mode: String,
    /// Discord token handling mode for bot_settings import.
    #[arg(long, default_value = "report")]
    pub discord_token_mode: String,
}

pub fn cmd_migrate_openclaw(args: OpenClawMigrateArgs) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| format!("Failed to read current dir: {e}"))?;
    let runtime_root = resolve_runtime_root(&args, &cwd);
    let args = resolve_resume_args(&args, runtime_root.as_deref())?;
    let source = resolve_source_root(args.root_path.as_deref(), &cwd, runtime_root.as_deref())?;
    let plan = build_import_plan(&source, &args, runtime_root.as_deref())?;

    if args.dry_run {
        return render_import_plan(&plan);
    }

    let runtime_root = runtime_root.ok_or_else(|| {
        "OpenClaw migrate apply requires a resolved AGENTDESK_ROOT_DIR runtime root.".to_string()
    })?;
    apply_import_plan(&plan, &source, &args, &runtime_root)
}

fn render_import_plan(plan: &impl serde::Serialize) -> Result<(), String> {
    let rendered = serde_json::to_string_pretty(plan)
        .map_err(|e| format!("Failed to serialize import plan: {e}"))?;
    println!("{rendered}");
    Ok(())
}

fn runtime_root_path(runtime_root: Option<&Path>) -> Option<String> {
    runtime_root.map(|path| path.display().to_string())
}

fn resolve_runtime_root(args: &OpenClawMigrateArgs, cwd: &Path) -> Option<PathBuf> {
    args.agentdesk_root
        .as_deref()
        .map(expand_tilde_path)
        .map(|path| absolutize_path(cwd, &path))
        .or_else(config::runtime_root)
}

fn absolutize_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn resolve_resume_args(
    args: &OpenClawMigrateArgs,
    runtime_root: Option<&Path>,
) -> Result<OpenClawMigrateArgs, String> {
    let Some(import_id) = args.resume.as_deref() else {
        return Ok(args.clone());
    };
    let runtime_root = runtime_root.ok_or_else(|| {
        "--resume requires a resolved AGENTDESK_ROOT_DIR runtime root.".to_string()
    })?;
    let audit_root = runtime_root
        .join("openclaw")
        .join("imports")
        .join(import_id);
    let resume_state_path = audit_root.join("resume-state.json");
    let write_plan_path = audit_root.join("write-plan.json");
    let manifest_path = audit_root.join("manifest.json");

    let resume_state = read_json_file(&resume_state_path)?;
    let _ = read_json_file(&manifest_path)?;
    let write_plan = read_json_file(&write_plan_path)?;

    let source_path = json_string(&resume_state, "source_path")
        .or_else(|| json_string(&write_plan, "config_path"))
        .ok_or_else(|| {
            format!(
                "Resume state '{}' is missing source_path/config_path.",
                resume_state_path.display()
            )
        })?;
    let selected_agents = json_string_list(&resume_state, "selected_agents")
        .or_else(|| json_string_list(&write_plan, "selected_agent_ids"));

    let requested_flags = write_plan
        .get("requested_flags")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut effective = args.clone();
    effective.root_path = Some(source_path);
    effective.agent_ids = selected_agents.unwrap_or_default();
    effective.all_agents = false;
    effective.fallback_provider = effective
        .fallback_provider
        .clone()
        .or_else(|| json_string_map(&requested_flags, "fallback_provider"));
    merge_missing_strings(
        &mut effective.workspace_root_rewrite,
        json_string_list_map(&requested_flags, "workspace_root_rewrite"),
    );
    effective.write_org = effective.write_org || json_bool_map(&requested_flags, "write_org");
    effective.write_bot_settings =
        effective.write_bot_settings || json_bool_map(&requested_flags, "write_bot_settings");
    effective.write_db = effective.write_db || json_bool_map(&requested_flags, "write_db");
    effective.overwrite = effective.overwrite || json_bool_map(&requested_flags, "overwrite");
    effective.with_channel_bindings =
        effective.with_channel_bindings || json_bool_map(&requested_flags, "with_channel_bindings");
    effective.with_sessions =
        effective.with_sessions || json_bool_map(&requested_flags, "with_sessions");
    effective.snapshot_source =
        effective.snapshot_source || json_bool_map(&requested_flags, "snapshot_source");
    effective.no_workspace =
        effective.no_workspace || json_bool_map(&requested_flags, "no_workspace");
    effective.no_memory = effective.no_memory || json_bool_map(&requested_flags, "no_memory");
    effective.no_prompts = effective.no_prompts || json_bool_map(&requested_flags, "no_prompts");
    if effective.tool_policy_mode == "report" {
        if let Some(stored) = json_string_map(&requested_flags, "tool_policy_mode") {
            effective.tool_policy_mode = stored;
        }
    }
    if effective.discord_token_mode == "report" {
        if let Some(stored) = json_string_map(&requested_flags, "discord_token_mode") {
            effective.discord_token_mode = stored;
        }
    }

    Ok(effective)
}

fn read_json_file(path: &Path) -> Result<serde_json::Value, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse '{}': {e}", path.display()))
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(ToOwned::to_owned)
}

fn json_string_list(value: &serde_json::Value, key: &str) -> Option<Vec<String>> {
    let values = value.get(key)?.as_array()?;
    Some(
        values
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect(),
    )
}

fn json_bool_map(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
    map.get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn json_string_map(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    map.get(key)?.as_str().map(ToOwned::to_owned)
}

fn json_string_list_map(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<String> {
    map.get(key)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn merge_missing_strings(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}
