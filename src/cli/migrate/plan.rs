use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runtime_layout;
use crate::services::provider::ProviderKind;

use super::runtime_root_path;
use super::source::{
    DiscordChannelImportResult, OpenClawAgentConfig, OpenClawAgentDefaultsConfig, OpenClawConfig,
    ResolvedSourceRoot, ToolPolicyScanResult, collect_representable_discord_channel_imports,
    scan_tool_policy,
};
use super::{DiscordTokenMode, OpenClawMigrateArgs, ToolPolicyMode};

const BOOTSTRAP_FILE_NAMES: &[&str] = &[
    "IDENTITY.md",
    "AGENTS.md",
    "SOUL.md",
    "USER.md",
    "TOOLS.md",
    "BOOT.md",
    "BOOTSTRAP.md",
    "HEARTBEAT.md",
];

const REQUIRED_AUDIT_OUTPUTS: &[&str] = &[
    "manifest.json",
    "agent-map.json",
    "write-plan.json",
    "apply-result.json",
    "resume-state.json",
    "warnings.txt",
    "tool-policy-report.json",
    "discord-auth-report.json",
    "channel-binding-preview.yaml",
];

#[derive(Clone, Debug)]
enum AgentSelectionMode {
    Explicit,
    AllAgents,
    DefaultAgent,
    SingleAgent,
}

impl AgentSelectionMode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::AllAgents => "all_agents",
            Self::DefaultAgent => "default_agent",
            Self::SingleAgent => "single_agent",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ImportPlan {
    pub(super) status: &'static str,
    pub(super) mode: &'static str,
    pub(super) selection_mode: &'static str,
    pub(super) source_root: String,
    pub(super) config_path: String,
    pub(super) runtime_root: Option<String>,
    pub(super) audit_root: Option<String>,
    pub(super) discovered_candidate_roots: Vec<String>,
    pub(super) selected_agent_ids: Vec<String>,
    pub(super) importable_agent_ids: Vec<String>,
    pub(super) selected_discord_account_ids: Vec<String>,
    pub(super) existing_role_ids: Vec<String>,
    pub(super) requested_flags: RequestedFlagsPlan,
    pub(super) effective_modes: EffectiveModesPlan,
    pub(super) phases: Vec<PhasePlan>,
    pub(super) audit_outputs: Vec<AuditOutputPlan>,
    pub(super) warnings: Vec<String>,
    pub(super) discord: DiscordChannelImportResult,
    pub(super) tool_policy: ToolPolicyScanResult,
    pub(super) sessions: Vec<ImportSessionPlan>,
    pub(super) agents: Vec<ImportAgentPlan>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct RequestedFlagsPlan {
    pub(super) dry_run: bool,
    pub(super) agentdesk_root: Option<String>,
    pub(super) resume: Option<String>,
    pub(super) fallback_provider: Option<String>,
    pub(super) workspace_root_rewrite: Vec<String>,
    pub(super) write_org: bool,
    pub(super) write_bot_settings: bool,
    pub(super) write_db: bool,
    pub(super) overwrite: bool,
    pub(super) with_channel_bindings: bool,
    pub(super) with_sessions: bool,
    pub(super) snapshot_source: bool,
    pub(super) no_workspace: bool,
    pub(super) no_memory: bool,
    pub(super) no_prompts: bool,
    pub(super) tool_policy_mode: String,
    pub(super) discord_token_mode: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct EffectiveModesPlan {
    pub(super) prompt_generation: &'static str,
    pub(super) memory_import: &'static str,
    pub(super) workspace_copy: &'static str,
    pub(super) tool_policy: &'static str,
    pub(super) bot_tokens: &'static str,
    pub(super) channel_bindings: &'static str,
    pub(super) sessions: &'static str,
    pub(super) apply_org: &'static str,
    pub(super) apply_bot_settings: &'static str,
    pub(super) apply_db: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct PhasePlan {
    pub(super) phase: &'static str,
    pub(super) mode: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct AuditOutputPlan {
    pub(super) path: &'static str,
    pub(super) mode: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ImportAgentPlan {
    pub(super) source_id: String,
    pub(super) display_name: String,
    pub(super) final_role_id: String,
    pub(super) avatar_emoji: Option<String>,
    pub(super) provider_hint: Option<String>,
    pub(super) mapped_provider: Option<String>,
    pub(super) model_hint: Option<String>,
    pub(super) workspace_source: String,
    pub(super) workspace_exists: bool,
    pub(super) bootstrap_files: Vec<String>,
    pub(super) has_memory_md: bool,
    pub(super) daily_memory_files: usize,
    pub(super) eligible_for_v1: bool,
    pub(super) tasks: Vec<AgentTaskPlan>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ImportSessionPlan {
    pub(super) source_agent_id: String,
    pub(super) final_role_id: String,
    pub(super) session_key: String,
    pub(super) session_id: String,
    pub(super) session_store_path: String,
    pub(super) transcript_path: Option<String>,
    pub(super) updated_at: i64,
    pub(super) model: Option<String>,
    pub(super) provider_hint: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) channel: Option<String>,
    pub(super) account_id: Option<String>,
    pub(super) thread_id: Option<String>,
    pub(super) status: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct AgentTaskPlan {
    pub(super) key: &'static str,
    pub(super) mode: &'static str,
}

#[derive(Clone, Debug, Deserialize)]
struct StoredAgentMapEntry {
    source_id: String,
    role_id: String,
}

pub(super) fn build_import_plan(
    source: &ResolvedSourceRoot,
    args: &OpenClawMigrateArgs,
    runtime_root: Option<&Path>,
) -> Result<ImportPlan, String> {
    let fallback_provider = parse_fallback_provider(args.fallback_provider.as_deref())?;
    let _tool_policy_mode = parse_tool_policy_mode(&args.tool_policy_mode)?;
    let _discord_token_mode = parse_discord_token_mode(&args.discord_token_mode)?;
    let workspace_rewrite_rules = parse_workspace_rewrite_rules(&args.workspace_root_rewrite)?;
    let default_agent_id = source.config.agents.default_agent_id()?;
    let (selected_agents, selection_mode) = select_agents(&source.config, args)?;
    let resume_role_ids =
        load_resume_role_ids(runtime_root, args.resume.as_deref(), &selected_agents)?;
    let existing_role_ids = load_existing_role_ids(runtime_root);
    let selected_agent_ids = selected_agents
        .iter()
        .map(|agent| agent.id.clone())
        .collect::<BTreeSet<_>>();
    let mut used_role_ids = existing_role_ids.clone();
    let mut warnings = Vec::new();
    let mut agents = Vec::new();
    collect_preview_flag_warnings(args, &mut warnings);

    for agent in selected_agents {
        let is_default_agent = default_agent_id
            .map(|selected_default_agent_id| selected_default_agent_id == agent.id)
            .unwrap_or(false);
        let final_role_id = if let Some(stored_role_id) = resume_role_ids.get(&agent.id) {
            used_role_ids.insert(stored_role_id.clone());
            stored_role_id.clone()
        } else {
            assign_role_id(&agent.id, &mut used_role_ids)
        };
        let provider_hint = direct_provider_hint(agent).map(|value| value.to_string());
        let model_hint =
            model_hint(agent, &source.config.agents.defaults).map(|value| value.to_string());
        let mapped_provider = map_provider(
            agent,
            &source.config.agents.defaults,
            fallback_provider.as_ref(),
            &mut warnings,
        );
        let workspace_source = resolve_workspace_path(
            &source.root,
            is_default_agent,
            agent,
            source.config.agents.defaults.workspace.as_deref(),
            &workspace_rewrite_rules,
            &mut warnings,
        );
        let workspace_exists = workspace_source.exists();
        let bootstrap_files = existing_bootstrap_files(&workspace_source);
        let has_memory_md = workspace_source.join("MEMORY.md").is_file();
        let daily_memory_files = count_markdown_files(&workspace_source.join("memory"));
        let eligible_for_v1 = workspace_exists && mapped_provider.is_some();
        let task_modes = build_agent_task_preview(args, workspace_exists, eligible_for_v1);

        if !workspace_exists {
            warnings.push(format!(
                "agent '{}' workspace path does not exist: {}",
                agent.id,
                workspace_source.display()
            ));
        }

        agents.push(ImportAgentPlan {
            source_id: agent.id.clone(),
            display_name: agent.name.clone().unwrap_or_else(|| agent.id.clone()),
            final_role_id,
            avatar_emoji: agent
                .identity
                .as_ref()
                .and_then(|identity| identity.emoji.clone()),
            provider_hint,
            mapped_provider,
            model_hint,
            workspace_source: workspace_source.display().to_string(),
            workspace_exists,
            bootstrap_files,
            has_memory_md,
            daily_memory_files,
            eligible_for_v1,
            tasks: task_modes,
        });
    }

    let discord =
        collect_representable_discord_channel_imports(&source.config, &selected_agent_ids);
    let tool_policy = scan_tool_policy(&source.config, &selected_agent_ids);
    let importable_agent_ids = agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .map(|agent| agent.source_id.clone())
        .collect::<BTreeSet<_>>();
    if importable_agent_ids.is_empty() && agents.iter().all(|agent| agent.mapped_provider.is_none())
    {
        return Err(
            "No importable OpenClaw agents remain after provider mapping. Supply --fallback-provider or select a directly supported agent."
                .to_string(),
        );
    }

    let mut discord = discord;
    restrict_discord_plan_to_importable_agents(&mut discord, &importable_agent_ids, &mut warnings);
    warnings.extend(discord.warnings.iter().cloned());
    warnings.extend(tool_policy.warnings.iter().cloned());
    let importable_agents = agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .cloned()
        .collect::<Vec<_>>();
    let sessions = if args.with_sessions {
        collect_session_plans(source, &importable_agents, &mut warnings)
    } else {
        Vec::new()
    };
    warnings.sort();
    warnings.dedup();

    let audit_root = runtime_root.map(|root| {
        if let Some(import_id) = args.resume.as_deref() {
            root.join("openclaw")
                .join("imports")
                .join(import_id)
                .display()
                .to_string()
        } else {
            root.join("openclaw")
                .join("imports")
                .join(build_import_id(source, &agents))
                .display()
                .to_string()
        }
    });

    Ok(ImportPlan {
        status: if args.dry_run { "preview" } else { "planned" },
        mode: if args.dry_run {
            "dry_run"
        } else {
            "live_apply"
        },
        selection_mode: selection_mode.as_str(),
        source_root: source.root.display().to_string(),
        config_path: source.config_path.display().to_string(),
        runtime_root: runtime_root_path(runtime_root),
        audit_root,
        discovered_candidate_roots: source
            .discovered_candidate_roots
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        selected_agent_ids: agents.iter().map(|agent| agent.source_id.clone()).collect(),
        importable_agent_ids: importable_agent_ids.into_iter().collect(),
        selected_discord_account_ids: discord.selected_account_ids.clone(),
        existing_role_ids: existing_role_ids.into_iter().collect(),
        requested_flags: RequestedFlagsPlan::from_args(args),
        effective_modes: build_effective_modes(args),
        phases: build_phase_preview(args),
        audit_outputs: build_audit_output_preview(args),
        warnings,
        discord,
        tool_policy,
        sessions,
        agents,
    })
}

impl RequestedFlagsPlan {
    fn from_args(args: &OpenClawMigrateArgs) -> Self {
        Self {
            dry_run: args.dry_run,
            agentdesk_root: args.agentdesk_root.clone(),
            resume: args.resume.clone(),
            fallback_provider: args.fallback_provider.clone(),
            workspace_root_rewrite: args.workspace_root_rewrite.clone(),
            write_org: args.write_org,
            write_bot_settings: args.write_bot_settings,
            write_db: args.write_db,
            overwrite: args.overwrite,
            with_channel_bindings: args.with_channel_bindings,
            with_sessions: args.with_sessions,
            snapshot_source: args.snapshot_source,
            no_workspace: args.no_workspace,
            no_memory: args.no_memory,
            no_prompts: args.no_prompts,
            tool_policy_mode: args.tool_policy_mode.clone(),
            discord_token_mode: args.discord_token_mode.clone(),
        }
    }
}

fn parse_fallback_provider(raw: Option<&str>) -> Result<Option<ProviderKind>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let parsed = ProviderKind::from_str(raw).ok_or_else(|| {
        format!(
            "Unsupported --fallback-provider '{}'. Expected one of: claude, codex, gemini, qwen.",
            raw
        )
    })?;
    Ok(Some(parsed))
}

fn build_effective_modes(args: &OpenClawMigrateArgs) -> EffectiveModesPlan {
    let tool_policy_mode = parse_tool_policy_mode(&args.tool_policy_mode).ok();
    let discord_token_mode = parse_discord_token_mode(&args.discord_token_mode).ok();
    EffectiveModesPlan {
        prompt_generation: if args.no_prompts {
            "disabled"
        } else {
            "preview_only"
        },
        memory_import: if args.no_memory {
            "disabled"
        } else {
            "preview_only"
        },
        workspace_copy: if args.no_workspace {
            "disabled"
        } else {
            "preview_only"
        },
        tool_policy: match tool_policy_mode.unwrap_or(ToolPolicyMode::Report) {
            ToolPolicyMode::Report => "report_only",
            ToolPolicyMode::BotIntersection | ToolPolicyMode::BotUnion => "preview_only",
        },
        bot_tokens: match discord_token_mode.unwrap_or(DiscordTokenMode::Report) {
            DiscordTokenMode::Report => "report_only",
            DiscordTokenMode::PlaintextOnly
            | DiscordTokenMode::ResolveEnvFile
            | DiscordTokenMode::ResolveAll => preview_mode(args.write_bot_settings),
        },
        channel_bindings: preview_mode(args.with_channel_bindings),
        sessions: preview_mode(args.with_sessions),
        apply_org: preview_mode(args.write_org),
        apply_bot_settings: preview_mode(args.write_bot_settings),
        apply_db: preview_mode(args.write_db),
    }
}

fn build_phase_preview(args: &OpenClawMigrateArgs) -> Vec<PhasePlan> {
    vec![
        PhasePlan {
            phase: "scan",
            mode: "preview_only",
        },
        PhasePlan {
            phase: "map",
            mode: "preview_only",
        },
        PhasePlan {
            phase: "prompt",
            mode: "preview_only",
        },
        PhasePlan {
            phase: "memory",
            mode: "preview_only",
        },
        PhasePlan {
            phase: "policy_discord",
            mode: "preview_only",
        },
        PhasePlan {
            phase: "sessions",
            mode: preview_mode(args.with_sessions),
        },
        PhasePlan {
            phase: "apply_files",
            mode: "preview_only",
        },
        PhasePlan {
            phase: "apply_org",
            mode: preview_mode(args.write_org),
        },
        PhasePlan {
            phase: "apply_bot_settings",
            mode: preview_mode(args.write_bot_settings),
        },
        PhasePlan {
            phase: "apply_db",
            mode: preview_mode(args.write_db),
        },
        PhasePlan {
            phase: "finalize",
            mode: "preview_only",
        },
    ]
}

fn build_audit_output_preview(args: &OpenClawMigrateArgs) -> Vec<AuditOutputPlan> {
    let mut outputs = REQUIRED_AUDIT_OUTPUTS
        .iter()
        .map(|path| AuditOutputPlan {
            path,
            mode: "preview_only",
        })
        .collect::<Vec<_>>();
    outputs.push(AuditOutputPlan {
        path: "session-map.json",
        mode: preview_mode(args.with_sessions),
    });
    outputs.push(AuditOutputPlan {
        path: "snapshot/",
        mode: preview_mode(args.snapshot_source),
    });
    outputs.push(AuditOutputPlan {
        path: "backups/",
        mode: preview_mode(args.overwrite),
    });
    outputs
}

fn build_agent_task_preview(
    args: &OpenClawMigrateArgs,
    workspace_exists: bool,
    eligible_for_v1: bool,
) -> Vec<AgentTaskPlan> {
    if !eligible_for_v1 {
        return vec![
            AgentTaskPlan {
                key: "workspace_copy",
                mode: "disabled",
            },
            AgentTaskPlan {
                key: "prompt_write",
                mode: "disabled",
            },
            AgentTaskPlan {
                key: "memory_import",
                mode: "disabled",
            },
            AgentTaskPlan {
                key: "session_import",
                mode: "disabled",
            },
            AgentTaskPlan {
                key: "org_agent_write",
                mode: "disabled",
            },
            AgentTaskPlan {
                key: "db_upsert",
                mode: "disabled",
            },
        ];
    }

    vec![
        AgentTaskPlan {
            key: "workspace_copy",
            mode: if !workspace_exists || args.no_workspace {
                "disabled"
            } else {
                "preview_only"
            },
        },
        AgentTaskPlan {
            key: "prompt_write",
            mode: if workspace_exists && !args.no_prompts {
                "preview_only"
            } else {
                "disabled"
            },
        },
        AgentTaskPlan {
            key: "memory_import",
            mode: if workspace_exists && !args.no_memory {
                "preview_only"
            } else {
                "disabled"
            },
        },
        AgentTaskPlan {
            key: "session_import",
            mode: preview_mode(args.with_sessions),
        },
        AgentTaskPlan {
            key: "org_agent_write",
            mode: if args.write_org && eligible_for_v1 {
                "preview_only"
            } else {
                "disabled"
            },
        },
        AgentTaskPlan {
            key: "db_upsert",
            mode: if args.write_db && eligible_for_v1 {
                "preview_only"
            } else {
                "disabled"
            },
        },
    ]
}

fn restrict_discord_plan_to_importable_agents(
    discord: &mut DiscordChannelImportResult,
    importable_agent_ids: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) {
    for binding in &mut discord.bindings {
        if importable_agent_ids.contains(&binding.agent_id) || binding.mode != "live_applicable" {
            continue;
        }
        let warning = format!(
            "Discord binding for agent '{}' stays preview-only because that agent is not importable.",
            binding.agent_id
        );
        warnings.push(warning.clone());
        binding.mode = "preview_only";
        binding.reason = Some(
            "Target agent is not importable because provider mapping or workspace resolution is incomplete."
                .to_string(),
        );
        binding.warnings.push(warning);
    }

    let mut selected_account_ids = BTreeSet::new();
    let mut agent_channels = BTreeMap::<String, BTreeSet<String>>::new();
    for binding in discord
        .bindings
        .iter()
        .filter(|binding| binding.mode == "live_applicable")
    {
        if let Some(account_id) = binding.selected_account_id.as_ref() {
            selected_account_ids.insert(account_id.clone());
        }
        agent_channels
            .entry(binding.agent_id.clone())
            .or_default()
            .extend(binding.channel_ids.iter().cloned());
    }

    discord.selected_account_ids = selected_account_ids.into_iter().collect();
    discord.agent_channels = agent_channels
        .into_iter()
        .map(|(agent_id, channel_ids)| (agent_id, channel_ids.into_iter().collect()))
        .collect();
}

fn collect_preview_flag_warnings(args: &OpenClawMigrateArgs, warnings: &mut Vec<String>) {
    let tool_policy_mode = parse_tool_policy_mode(&args.tool_policy_mode).ok();
    let discord_token_mode = parse_discord_token_mode(&args.discord_token_mode).ok();

    if args.with_channel_bindings && !args.write_org {
        warnings.push(
            "--with-channel-bindings without --write-org stays preview-only; no live org.yaml bindings can be applied."
                .to_string(),
        );
    }

    if args.write_bot_settings && !args.write_org && args.with_channel_bindings {
        warnings.push(
            "--write-bot-settings with channel bindings but without --write-org cannot produce live allowed_channel_ids safely."
                .to_string(),
        );
    }

    if args.with_sessions && !args.write_db {
        warnings.push(
            "--with-sessions without --write-db can only target file-based session artifacts; DB rows remain preview-only."
                .to_string(),
        );
    }

    if args.no_workspace {
        warnings.push(
            "--no-workspace disables copied workspace output; prompt and memory preview still reads the source workspace."
                .to_string(),
        );
    }

    if args.no_memory {
        warnings.push("--no-memory skips Markdown memory import.".to_string());
    }

    if args.no_prompts {
        warnings.push("--no-prompts skips merged prompt generation.".to_string());
    }

    if args.write_bot_settings
        && matches!(discord_token_mode, Some(DiscordTokenMode::Report) | None)
    {
        warnings.push(
            "--write-bot-settings with --discord-token-mode report stays audit-only for bot tokens."
                .to_string(),
        );
    }

    if !args.write_bot_settings
        && !matches!(discord_token_mode, Some(DiscordTokenMode::Report) | None)
    {
        warnings.push(
            "--discord-token-mode without --write-bot-settings stays report-only.".to_string(),
        );
    }

    if !args.write_bot_settings
        && matches!(
            tool_policy_mode,
            Some(ToolPolicyMode::BotIntersection | ToolPolicyMode::BotUnion)
        )
    {
        warnings
            .push("--tool-policy-mode without --write-bot-settings stays report-only.".to_string());
    }
}

fn preview_mode(enabled: bool) -> &'static str {
    if enabled { "preview_only" } else { "disabled" }
}

fn parse_tool_policy_mode(raw: &str) -> Result<ToolPolicyMode, String> {
    ToolPolicyMode::parse(raw)
}

fn parse_discord_token_mode(raw: &str) -> Result<DiscordTokenMode, String> {
    DiscordTokenMode::parse(raw)
}

fn load_existing_role_ids(runtime_root: Option<&Path>) -> BTreeSet<String> {
    let Some(runtime_root) = runtime_root else {
        return BTreeSet::new();
    };
    let mut ids = BTreeSet::new();

    let org_path = runtime_root.join("config").join("org.yaml");
    if let Ok(content) = fs::read_to_string(org_path) {
        if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
            if let Some(agents) = value.get("agents").and_then(serde_yaml::Value::as_mapping) {
                ids.extend(
                    agents
                        .keys()
                        .filter_map(serde_yaml::Value::as_str)
                        .map(ToOwned::to_owned),
                );
            }
        }
    }

    for agentdesk_path in [
        runtime_layout::config_file_path(runtime_root),
        runtime_layout::legacy_config_file_path(runtime_root),
    ] {
        if let Ok(content) = fs::read_to_string(agentdesk_path) {
            if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
                if let Some(agents) = value.get("agents").and_then(serde_yaml::Value::as_sequence) {
                    ids.extend(agents.iter().filter_map(|agent| {
                        agent
                            .get("id")
                            .and_then(serde_yaml::Value::as_str)
                            .map(ToOwned::to_owned)
                    }));
                }
            }
        }
    }

    ids
}

fn select_agents<'a>(
    config: &'a OpenClawConfig,
    args: &OpenClawMigrateArgs,
) -> Result<(Vec<&'a OpenClawAgentConfig>, AgentSelectionMode), String> {
    if !args.agent_ids.is_empty() {
        let by_id = config
            .agents
            .list
            .iter()
            .map(|agent| (agent.id.as_str(), agent))
            .collect::<BTreeMap<_, _>>();
        let mut selected = Vec::new();
        let mut seen = BTreeSet::new();
        for requested in &args.agent_ids {
            let normalized = requested.trim();
            let Some(agent) = by_id.get(normalized) else {
                return Err(format!(
                    "Unknown OpenClaw agent '{}'. Available agents: {}.",
                    requested,
                    available_agent_ids(config)
                ));
            };
            if seen.insert(normalized.to_string()) {
                selected.push(*agent);
            }
        }
        return Ok((selected, AgentSelectionMode::Explicit));
    }

    if args.all_agents {
        return Ok((
            config.agents.list.iter().collect(),
            AgentSelectionMode::AllAgents,
        ));
    }

    if let Some(default_agent) = config.agents.default_agent_id()? {
        let Some(agent) = config
            .agents
            .list
            .iter()
            .find(|agent| agent.id == default_agent)
        else {
            return Err(format!(
                "openclaw.json default agent '{}' is missing from agents.list.",
                default_agent
            ));
        };
        return Ok((vec![agent], AgentSelectionMode::DefaultAgent));
    }

    if config.agents.list.len() == 1 {
        return Ok((
            vec![&config.agents.list[0]],
            AgentSelectionMode::SingleAgent,
        ));
    }

    Err(format!(
        "Multiple OpenClaw agents found ({}). Pass --agent or --all-agents.",
        available_agent_ids(config)
    ))
}

fn available_agent_ids(config: &OpenClawConfig) -> String {
    config
        .agents
        .list
        .iter()
        .map(|agent| agent.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn assign_role_id(source_id: &str, used: &mut BTreeSet<String>) -> String {
    let base = sanitize_role_id(source_id);
    let prefixed = sanitize_role_id(&format!("openclaw-{source_id}"));

    // Stable reruns should keep reusing an existing imported role id instead
    // of drifting back to a bare source id once it becomes available again.
    if used.contains(&prefixed) {
        return prefixed;
    }

    if used.insert(base.clone()) {
        return base;
    }

    if used.insert(prefixed.clone()) {
        return prefixed;
    }

    for suffix in 2.. {
        let candidate = format!("{prefixed}-{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }

    unreachable!("role id generation exhausted");
}

fn load_resume_role_ids(
    runtime_root: Option<&Path>,
    import_id: Option<&str>,
    selected_agents: &[&OpenClawAgentConfig],
) -> Result<BTreeMap<String, String>, String> {
    let Some(import_id) = import_id else {
        return Ok(BTreeMap::new());
    };
    let runtime_root = runtime_root
        .ok_or_else(|| "OpenClaw migrate resume requires an AgentDesk runtime root.".to_string())?;
    let agent_map_path = runtime_root
        .join("openclaw")
        .join("imports")
        .join(import_id)
        .join("agent-map.json");
    let agent_map_raw = fs::read_to_string(&agent_map_path)
        .map_err(|e| format!("Failed to read '{}': {e}", agent_map_path.display()))?;
    let agent_map: Vec<StoredAgentMapEntry> = serde_json::from_str(&agent_map_raw)
        .map_err(|e| format!("Failed to parse '{}': {e}", agent_map_path.display()))?;

    let mut by_source = BTreeMap::new();
    for entry in agent_map {
        if entry.source_id.trim().is_empty() || entry.role_id.trim().is_empty() {
            return Err(format!(
                "Resume agent map '{}' contains an empty source_id or role_id entry.",
                agent_map_path.display()
            ));
        }
        if let Some(existing_role_id) =
            by_source.insert(entry.source_id.clone(), entry.role_id.clone())
        {
            if existing_role_id != entry.role_id {
                return Err(format!(
                    "Resume agent map '{}' contains conflicting role ids for source agent '{}'.",
                    agent_map_path.display(),
                    entry.source_id
                ));
            }
        }
    }

    let mut selected_map = BTreeMap::new();
    let mut seen_role_ids = BTreeSet::new();
    for agent in selected_agents {
        let role_id = by_source.get(&agent.id).cloned().ok_or_else(|| {
            format!(
                "Resume agent map '{}' is missing source agent '{}'.",
                agent_map_path.display(),
                agent.id
            )
        })?;
        if !seen_role_ids.insert(role_id.clone()) {
            return Err(format!(
                "Resume agent map '{}' assigns duplicate role id '{}' across selected agents.",
                agent_map_path.display(),
                role_id
            ));
        }
        selected_map.insert(agent.id.clone(), role_id);
    }

    Ok(selected_map)
}

fn sanitize_role_id(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;

    for ch in raw.trim().chars() {
        let mapped = match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => Some(ch),
            _ => Some('-'),
        };

        let Some(mapped) = mapped else {
            continue;
        };

        if mapped == '-' {
            if last_dash || out.is_empty() {
                continue;
            }
            last_dash = true;
            out.push(mapped);
            continue;
        }

        last_dash = false;
        out.push(mapped);
    }

    while out.ends_with('-') {
        out.pop();
    }

    if out.is_empty() {
        "openclaw-agent".to_string()
    } else {
        out
    }
}

fn direct_provider_hint(agent: &OpenClawAgentConfig) -> Option<&str> {
    agent
        .runtime
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            agent
                .runtime
                .get("agent")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            agent
                .runtime
                .get("acp")
                .and_then(|acp| acp.get("agent"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            agent
                .runtime
                .get("acp")
                .and_then(|acp| acp.get("backend"))
                .and_then(serde_json::Value::as_str)
        })
}

fn model_hint<'a>(
    agent: &'a OpenClawAgentConfig,
    defaults: &'a OpenClawAgentDefaultsConfig,
) -> Option<&'a str> {
    agent
        .model
        .as_ref()
        .and_then(|model| model.primary())
        .or_else(|| {
            agent
                .runtime
                .get("model")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| defaults.model.as_ref().and_then(|model| model.primary()))
}

fn map_known_provider(raw: &str) -> Option<ProviderKind> {
    if let Some(mapped) = ProviderKind::from_str(raw) {
        return Some(mapped);
    }
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "anthropic" => Some(ProviderKind::Claude),
        "openai" | "openai-codex" | "codex" => Some(ProviderKind::Codex),
        "google" | "gemini" => Some(ProviderKind::Gemini),
        _ => None,
    }
}

fn map_provider_from_model(model: &str) -> Option<ProviderKind> {
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.starts_with("anthropic/") {
        return Some(ProviderKind::Claude);
    }
    if normalized.starts_with("openai-codex/") || normalized.starts_with("openai/") {
        return Some(ProviderKind::Codex);
    }
    if normalized.starts_with("google/") || normalized.contains("gemini") {
        return Some(ProviderKind::Gemini);
    }
    None
}

fn map_provider(
    agent: &OpenClawAgentConfig,
    defaults: &OpenClawAgentDefaultsConfig,
    fallback_provider: Option<&ProviderKind>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    if let Some(raw_provider) = direct_provider_hint(agent) {
        if let Some(mapped) = map_known_provider(raw_provider) {
            return Some(mapped.as_str().to_string());
        }
        if let Some(model) = model_hint(agent, defaults).and_then(map_provider_from_model) {
            warnings.push(format!(
                "agent '{}' runtime/provider hint '{}' is unsupported; inferred provider '{}' from model '{}'.",
                agent.id,
                raw_provider,
                model.as_str(),
                model_hint(agent, defaults).unwrap_or("unknown")
            ));
            return Some(model.as_str().to_string());
        }
        if let Some(fallback_provider) = fallback_provider {
            warnings.push(format!(
                "agent '{}' provider hint '{}' is unsupported; dry-run mapped it to fallback '{}'.",
                agent.id,
                raw_provider,
                fallback_provider.as_str()
            ));
            return Some(fallback_provider.as_str().to_string());
        }
        warnings.push(format!(
            "agent '{}' provider hint '{}' is unsupported and no --fallback-provider was supplied.",
            agent.id, raw_provider
        ));
        return None;
    }

    if let Some(model_hint) = model_hint(agent, defaults) {
        if let Some(mapped) = map_provider_from_model(model_hint) {
            return Some(mapped.as_str().to_string());
        }
        if let Some(fallback_provider) = fallback_provider {
            warnings.push(format!(
                "agent '{}' model '{}' is unsupported; dry-run mapped it to fallback '{}'.",
                agent.id,
                model_hint,
                fallback_provider.as_str()
            ));
            return Some(fallback_provider.as_str().to_string());
        }
        warnings.push(format!(
            "agent '{}' model '{}' is unsupported and no --fallback-provider was supplied.",
            agent.id, model_hint
        ));
        return None;
    }

    if let Some(fallback_provider) = fallback_provider {
        warnings.push(format!(
            "agent '{}' has no provider or model hint; dry-run mapped it to fallback '{}'.",
            agent.id,
            fallback_provider.as_str()
        ));
        return Some(fallback_provider.as_str().to_string());
    }

    warnings.push(format!(
        "agent '{}' has no provider or model hint; live apply will require an explicit fallback later.",
        agent.id
    ));
    None
}

#[derive(Clone, Debug)]
struct WorkspaceRewriteRule {
    from: String,
    to: PathBuf,
}

fn parse_workspace_rewrite_rules(
    raw_rules: &[String],
) -> Result<Vec<WorkspaceRewriteRule>, String> {
    raw_rules
        .iter()
        .map(|raw| {
            let Some((from, to)) = raw.split_once('=') else {
                return Err(format!(
                    "Invalid --workspace-root-rewrite '{}'. Expected OLD=NEW.",
                    raw
                ));
            };
            let from = from.trim().trim_end_matches('/');
            let to = to.trim();
            if from.is_empty() || to.is_empty() {
                return Err(format!(
                    "Invalid --workspace-root-rewrite '{}'. OLD and NEW must be non-empty.",
                    raw
                ));
            }
            Ok(WorkspaceRewriteRule {
                from: from.to_string(),
                to: PathBuf::from(to),
            })
        })
        .collect()
}

fn resolve_workspace_path(
    source_root: &Path,
    is_default_agent: bool,
    agent: &OpenClawAgentConfig,
    defaults_workspace: Option<&str>,
    workspace_rewrite_rules: &[WorkspaceRewriteRule],
    warnings: &mut Vec<String>,
) -> PathBuf {
    if let Some(raw_workspace) = agent.workspace.as_deref() {
        return resolve_workspace_value_with_rewrites(
            source_root,
            raw_workspace,
            &agent.id,
            workspace_rewrite_rules,
            warnings,
        );
    }

    if let Some(defaults_workspace) = defaults_workspace {
        return resolve_workspace_value_with_rewrites(
            source_root,
            defaults_workspace,
            &agent.id,
            workspace_rewrite_rules,
            warnings,
        );
    }

    if is_default_agent {
        return source_root.join("workspace");
    }

    source_root.join(format!("workspace-{}", agent.id))
}

fn resolve_workspace_value_with_rewrites(
    source_root: &Path,
    raw_workspace: &str,
    agent_id: &str,
    workspace_rewrite_rules: &[WorkspaceRewriteRule],
    warnings: &mut Vec<String>,
) -> PathBuf {
    let trimmed_workspace = raw_workspace.trim();
    let path = PathBuf::from(trimmed_workspace);
    let normalized = trimmed_workspace.trim_end_matches(|ch| ch == '/' || ch == '\\');
    for rule in workspace_rewrite_rules {
        let from = rule
            .from
            .trim()
            .trim_end_matches(|ch| ch == '/' || ch == '\\');
        let matches_rule = normalized == from
            || normalized
                .strip_prefix(from)
                .map(|suffix| suffix.starts_with('/') || suffix.starts_with('\\'))
                .unwrap_or(false);
        if matches_rule {
            let suffix = normalized
                .strip_prefix(from)
                .unwrap_or_default()
                .trim_start_matches(|ch| ch == '/' || ch == '\\');
            let target_root = if rule.to.is_absolute() {
                rule.to.clone()
            } else {
                source_root.join(&rule.to)
            };
            let rewritten = if suffix.is_empty() {
                target_root
            } else {
                target_root.join(suffix)
            };
            warnings.push(format!(
                "agent '{}' workspace '{}' was remapped via --workspace-root-rewrite to '{}'.",
                agent_id,
                raw_workspace,
                rewritten.display()
            ));
            return rewritten;
        }
    }

    let looks_absolute = workspace_path_is_absolute(trimmed_workspace);
    if !looks_absolute {
        return source_root.join(path);
    }

    if !path.exists() {
        if let Some(file_name) = path.file_name() {
            let relocated = source_root.join(file_name);
            if relocated.exists() {
                warnings.push(format!(
                    "agent '{}' workspace '{}' was auto-relocated to '{}' under the discovered OpenClaw root.",
                    agent_id,
                    raw_workspace,
                    relocated.display()
                ));
                return relocated;
            }
        }
    }

    path
}

fn workspace_path_is_absolute(raw: &str) -> bool {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    Path::new(trimmed).is_absolute()
        || trimmed.starts_with('/')
        || trimmed.starts_with('\\')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/'))
}

fn existing_bootstrap_files(workspace_path: &Path) -> Vec<String> {
    BOOTSTRAP_FILE_NAMES
        .iter()
        .filter_map(|name| {
            let path = workspace_path.join(name);
            path.is_file().then(|| (*name).to_string())
        })
        .collect()
}

fn count_markdown_files(root: &Path) -> usize {
    if !root.exists() {
        return 0;
    }

    let mut count = 0;
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
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
            if file_type.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("md"))
                    .unwrap_or(false)
            {
                count += 1;
            }
        }
    }

    count
}

fn build_import_id(source: &ResolvedSourceRoot, agents: &[ImportAgentPlan]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.config_path.as_os_str().to_string_lossy().as_bytes());
    for agent in agents {
        hasher.update(agent.source_id.as_bytes());
        hasher.update(agent.final_role_id.as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    let root_name = source
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("openclaw");
    format!("openclaw-{}-{}", sanitize_role_id(root_name), &digest[..12])
}

fn collect_session_plans(
    source: &ResolvedSourceRoot,
    agents: &[ImportAgentPlan],
    warnings: &mut Vec<String>,
) -> Vec<ImportSessionPlan> {
    let mut sessions = Vec::new();

    for agent in agents {
        let sessions_dir = source
            .root
            .join("agents")
            .join(&agent.source_id)
            .join("sessions");
        let store_path = sessions_dir.join("sessions.json");
        if !store_path.is_file() {
            continue;
        }

        let raw = match fs::read_to_string(&store_path) {
            Ok(raw) => raw,
            Err(err) => {
                warnings.push(format!(
                    "Failed to read OpenClaw session store '{}': {err}",
                    store_path.display()
                ));
                continue;
            }
        };

        let parsed = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(parsed) => parsed,
            Err(err) => {
                warnings.push(format!(
                    "Failed to parse OpenClaw session store '{}': {err}",
                    store_path.display()
                ));
                continue;
            }
        };

        let Some(entries) = parsed.as_object() else {
            warnings.push(format!(
                "OpenClaw session store '{}' is not a JSON object.",
                store_path.display()
            ));
            continue;
        };

        for (session_key, entry) in entries {
            let Some(session_id) = entry
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
            else {
                warnings.push(format!(
                    "Skipping OpenClaw session '{}' in '{}': missing sessionId.",
                    session_key,
                    store_path.display()
                ));
                continue;
            };

            let transcript_path = resolve_session_transcript_path(
                &sessions_dir,
                &session_id,
                entry.get("sessionFile").and_then(serde_json::Value::as_str),
                warnings,
            )
            .map(|path| path.display().to_string());

            sessions.push(ImportSessionPlan {
                source_agent_id: agent.source_id.clone(),
                final_role_id: agent.final_role_id.clone(),
                session_key: session_key.clone(),
                session_id,
                session_store_path: store_path.display().to_string(),
                transcript_path,
                updated_at: json_i64(entry.get("updatedAt")).unwrap_or_default(),
                model: entry
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        entry
                            .get("modelOverride")
                            .and_then(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                    }),
                provider_hint: entry
                    .get("modelProvider")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        entry
                            .get("providerOverride")
                            .and_then(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                    })
                    .or_else(|| {
                        entry
                            .get("origin")
                            .and_then(|origin| origin.get("provider"))
                            .and_then(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                    }),
                cwd: entry
                    .get("spawnedWorkspaceDir")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        entry
                            .get("cwd")
                            .and_then(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                    })
                    .or_else(|| {
                        entry
                            .get("acp")
                            .and_then(|acp| acp.get("cwd"))
                            .and_then(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                    }),
                channel: entry
                    .get("lastChannel")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        entry
                            .get("channel")
                            .and_then(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                    }),
                account_id: entry
                    .get("lastAccountId")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        entry
                            .get("origin")
                            .and_then(|origin| origin.get("accountId"))
                            .and_then(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                    }),
                thread_id: entry
                    .get("lastThreadId")
                    .and_then(json_stringish)
                    .or_else(|| {
                        entry
                            .get("origin")
                            .and_then(|origin| origin.get("threadId"))
                            .and_then(json_stringish)
                    }),
                status: entry
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
            });
        }
    }

    sessions.sort_by(|left, right| {
        left.source_agent_id
            .cmp(&right.source_agent_id)
            .then_with(|| left.session_key.cmp(&right.session_key))
    });
    sessions
}

fn resolve_session_transcript_path(
    sessions_dir: &Path,
    session_id: &str,
    session_file: Option<&str>,
    warnings: &mut Vec<String>,
) -> Option<PathBuf> {
    let candidate = match session_file {
        Some(raw) if !raw.trim().is_empty() => {
            let path = PathBuf::from(raw.trim());
            if path.is_absolute() {
                path
            } else {
                sessions_dir.join(path)
            }
        }
        _ => sessions_dir.join(format!("{session_id}.jsonl")),
    };

    if candidate.is_file() {
        Some(candidate)
    } else {
        warnings.push(format!(
            "OpenClaw transcript file is missing for session '{}' at '{}'.",
            session_id,
            candidate.display()
        ));
        None
    }
}

fn json_i64(value: Option<&serde_json::Value>) -> Option<i64> {
    value
        .and_then(serde_json::Value::as_i64)
        .or_else(|| value.and_then(serde_json::Value::as_u64).map(|v| v as i64))
}

fn json_stringish(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| value.as_i64().map(|v| v.to_string()))
        .or_else(|| value.as_u64().map(|v| v.to_string()))
}
