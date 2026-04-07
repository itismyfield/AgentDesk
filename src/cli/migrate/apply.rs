use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::Utc;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::config::{self, AgentDef as RuntimeAgentDef};
use crate::db;
use crate::db::agents::sync_agents_from_config;
use crate::runtime_layout;
use crate::services::agent_protocol::DEFAULT_ALLOWED_TOOLS;
use crate::services::discord::org_writer::{self, OrgAgentUpdate, OrgChannelBindingUpdate};
use crate::services::discord::runtime_store::org_schema_path_for_root;
use crate::services::discord::settings::discord_token_hash;
use crate::services::provider::ProviderKind;
use crate::ui::ai_screen::{HistoryItem, HistoryType, SessionData};

use super::DiscordTokenMode;
use super::OpenClawMigrateArgs;
use super::ToolPolicyMode;
use super::plan::{ImportAgentPlan, ImportPlan, ImportSessionPlan};
use super::source::{
    OpenClawBindingConfig, OpenClawDiscordGuildConfig, OpenClawSecretProviderConfig,
    OpenClawSecretsConfig, ResolvedSourceRoot,
};

const BOOTSTRAP_PROMPT_SECTIONS: &[(&str, &str)] = &[
    ("IDENTITY.md", "Imported OpenClaw Identity"),
    ("AGENTS.md", "Imported OpenClaw Agent Rules"),
    ("SOUL.md", "Imported OpenClaw Persona"),
    ("USER.md", "Imported OpenClaw User Context"),
    ("TOOLS.md", "Imported OpenClaw Tool Notes"),
    ("BOOT.md", "Imported OpenClaw Boot Intent"),
    ("BOOTSTRAP.md", "Imported OpenClaw Bootstrap"),
    ("HEARTBEAT.md", "Imported OpenClaw Heartbeat"),
];

const COPY_PRUNE_DIR_NAMES: &[&str] =
    &[".git", "node_modules", "target", "dist", ".venv", ".cache"];

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AgentMapEntry {
    source_id: String,
    role_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestAgent {
    source_id: String,
    role_id: String,
    provider: String,
    prompt_path: Option<String>,
    memory_dir: Option<String>,
    workspace_source: String,
    workspace_target: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Manifest {
    import_id: String,
    source_root: String,
    config_path: String,
    selected_agent_ids: Vec<String>,
    selected_discord_account_ids: Vec<String>,
    written_paths: Vec<String>,
    warnings: Vec<String>,
    agents: Vec<ManifestAgent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApplyTaskState {
    status: String,
    outputs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApplyAgentState {
    tasks: BTreeMap<String, ApplyTaskState>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApplyPhaseState {
    status: String,
    started_at: String,
    ended_at: String,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApplyResult {
    status: String,
    import_id: String,
    source_root: String,
    phases: BTreeMap<String, ApplyPhaseState>,
    agents: BTreeMap<String, ApplyAgentState>,
    warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ResumeAgentState {
    tasks: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ResumeState {
    status: String,
    source_path: String,
    source_fingerprint: String,
    import_id: String,
    selected_agents: Vec<String>,
    completed_phases: Vec<String>,
    pending_phases: Vec<String>,
    phases: BTreeMap<String, String>,
    agents: BTreeMap<String, ResumeAgentState>,
    next_recommended_step: String,
}

#[derive(Clone, Debug, Serialize)]
struct DiscordAuthReportData {
    default_token_configured: bool,
    has_named_accounts: bool,
    requested_token_mode: String,
    write_bot_settings_enabled: bool,
    accounts: Vec<DiscordAccountReportEntry>,
    account_to_bot_mappings: Vec<DiscordAccountToBotMapping>,
}

#[derive(Clone, Debug, Serialize)]
struct DiscordAccountReportEntry {
    account_id: String,
    source: &'static str,
    enabled: bool,
    has_token: bool,
    token_kind: String,
    token_status: String,
    importable: bool,
    guild_count: usize,
    channel_override_count: usize,
    user_allowlist_count: usize,
    role_allowlist_count: usize,
    binding_roles_present: bool,
    allow_bots_enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
struct DiscordAccountToBotMapping {
    account_id: String,
    role_ids: Vec<String>,
    providers: Vec<String>,
    live_channel_ids: Vec<String>,
    live_binding_agents: Vec<String>,
    preview_only_binding_agents: Vec<String>,
    mode: &'static str,
}

#[derive(Debug, Serialize)]
struct DiscordAuthReport<'a> {
    default_token_configured: bool,
    has_named_accounts: bool,
    requested_token_mode: &'a str,
    write_bot_settings_enabled: bool,
    accounts: &'a [DiscordAccountReportEntry],
    account_to_bot_mappings: &'a [DiscordAccountToBotMapping],
    bindings: &'a [super::source::DiscordBindingImportPlan],
    selected_account_ids: &'a [String],
    warnings: &'a [String],
}

#[derive(Debug, Serialize)]
struct ChannelBindingPreview<'a> {
    bindings: &'a [super::source::DiscordBindingImportPlan],
    warnings: &'a [String],
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SessionMapEntry {
    source_agent_id: String,
    role_id: String,
    session_key: String,
    session_id: String,
    transcript_path: Option<String>,
    ai_session_path: String,
    db_session_key: String,
}

#[derive(Default)]
struct RestoredAuditState {
    manifest_agents: Vec<ManifestAgent>,
    written_paths: Vec<String>,
    warnings: Vec<String>,
    apply_agents: BTreeMap<String, ApplyAgentState>,
    phase_status: BTreeMap<String, ApplyPhaseState>,
    session_map: Vec<SessionMapEntry>,
}

#[derive(Clone, Debug)]
struct BotSettingsEntryPlan {
    account_id: String,
    provider: String,
    role_id: Option<String>,
    token: String,
    allowed_channel_ids: Option<Vec<u64>>,
    allowed_tools: Option<Vec<String>>,
}

pub(super) fn apply_import_plan(
    plan: &ImportPlan,
    source: &ResolvedSourceRoot,
    args: &OpenClawMigrateArgs,
    runtime_root: &Path,
) -> Result<(), String> {
    let importable_agents = plan
        .agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .collect::<Vec<_>>();
    if importable_agents.is_empty() {
        let blocked_agents = plan
            .agents
            .iter()
            .map(|agent| agent.source_id.clone())
            .collect::<Vec<_>>();
        return Err(format!(
            "OpenClaw migrate apply has no importable agents after provider/workspace validation. Blocked: {}.",
            blocked_agents.join(", ")
        ));
    }

    let skipped_agents = plan
        .agents
        .iter()
        .filter(|agent| !agent.eligible_for_v1)
        .map(|agent| agent.source_id.clone())
        .collect::<Vec<_>>();

    let audit_root = plan
        .audit_root
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| "OpenClaw migrate apply requires a resolved audit root.".to_string())?;
    let import_id = audit_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("openclaw-import")
        .to_string();
    let backups_root = audit_root.join("backups");
    fs::create_dir_all(&audit_root)
        .map_err(|e| format!("Failed to create '{}': {e}", audit_root.display()))?;
    fs::create_dir_all(&backups_root)
        .map_err(|e| format!("Failed to create '{}': {e}", backups_root.display()))?;

    runtime_layout::ensure_runtime_layout(runtime_root)?;

    let yaml_path = runtime_layout::config_file_path(runtime_root);
    let org_yaml_path = org_schema_path_for_root(runtime_root);
    let bot_settings_path = runtime_root.join("config").join("bot_settings.json");
    let ai_sessions_root = runtime_root.join("ai_sessions");
    let prompts_root = runtime_layout::managed_agents_root(runtime_root);
    let role_context_root = runtime_layout::long_term_memory_root(runtime_root);
    let workspaces_root = runtime_root.join("openclaw").join("workspaces");
    if !args.no_prompts {
        fs::create_dir_all(&prompts_root)
            .map_err(|e| format!("Failed to create '{}': {e}", prompts_root.display()))?;
    }
    if !args.no_memory {
        fs::create_dir_all(&role_context_root)
            .map_err(|e| format!("Failed to create '{}': {e}", role_context_root.display()))?;
    }
    if !args.no_workspace {
        fs::create_dir_all(&workspaces_root)
            .map_err(|e| format!("Failed to create '{}': {e}", workspaces_root.display()))?;
    }
    if args.with_sessions {
        fs::create_dir_all(&ai_sessions_root)
            .map_err(|e| format!("Failed to create '{}': {e}", ai_sessions_root.display()))?;
    }

    let mut warnings = plan.warnings.clone();
    let mut written_paths = Vec::new();
    let mut manifest_agents = Vec::new();
    let mut session_map = planned_session_map(plan, runtime_root);
    let agent_map = plan
        .agents
        .iter()
        .map(|agent| AgentMapEntry {
            source_id: agent.source_id.clone(),
            role_id: agent.final_role_id.clone(),
        })
        .collect::<Vec<_>>();
    let mut apply_agents = build_initial_apply_agents(plan, args);
    let mut phase_status = build_initial_phase_status(plan, args);
    let source_fingerprint = source_fingerprint(source, plan, args)?;
    let discord_auth_report = build_discord_auth_report(plan, source, args)?;
    if let Some(restored) = load_restored_audit_state(&audit_root, &source_fingerprint, args)? {
        warnings = merge_strings(plan.warnings.clone(), restored.warnings);
        written_paths = restored.written_paths;
        manifest_agents = restored.manifest_agents;
        if !restored.apply_agents.is_empty() {
            apply_agents = restored.apply_agents;
        }
        if !restored.phase_status.is_empty() {
            phase_status = restored.phase_status;
        }
        if !restored.session_map.is_empty() {
            session_map = restored.session_map;
        }
    }
    normalize_resumable_state(&mut apply_agents, &mut phase_status);

    if !skipped_agents.is_empty() {
        warnings.push(format!(
            "Skipped non-importable OpenClaw agents during live apply: {}.",
            skipped_agents.join(", ")
        ));
        warnings.sort();
        warnings.dedup();
    }

    persist_audit_state(
        &audit_root,
        &import_id,
        source,
        plan,
        &discord_auth_report,
        &agent_map,
        &manifest_agents,
        &written_paths,
        &warnings,
        &apply_agents,
        &phase_status,
        &source_fingerprint,
        "running",
        "running",
        Some(&session_map),
    )?;

    let result = (|| -> Result<(), String> {
        let mut config = if yaml_path.exists() {
            config::load_from_path(&yaml_path)
                .map_err(|e| format!("Failed to load '{}': {e}", yaml_path.display()))?
        } else {
            let mut config = config::Config::default();
            config.data.dir = runtime_root.join("data");
            config.policies.dir = runtime_root.join("policies");
            config
        };
        merge_imported_agents(&mut config, plan, args.overwrite)?;
        if !args.write_db && !importable_agents.is_empty() {
            warnings.push(
                "Imported agents updated agentdesk.yaml only; runtime DB visibility may wait for the next standard sync or restart."
                    .to_string(),
            );
        }

        if args.with_sessions && phase_needs_apply(&phase_status, "sessions") {
            mark_phase_running(&mut phase_status, "sessions");
            persist_audit_state(
                &audit_root,
                &import_id,
                source,
                plan,
                &discord_auth_report,
                &agent_map,
                &manifest_agents,
                &written_paths,
                &warnings,
                &apply_agents,
                &phase_status,
                &source_fingerprint,
                "running",
                "running",
                Some(&session_map),
            )?;

            let session_outputs = import_sessions(
                plan,
                runtime_root,
                &ai_sessions_root,
                &backups_root,
                args.overwrite || args.resume.is_some(),
                args.no_workspace,
            )?;
            for (role_id, outputs) in &session_outputs.agent_outputs {
                if let Some(state) = apply_agents.get_mut(role_id) {
                    update_task_state(state, "session_import", "completed", outputs.clone());
                }
            }
            written_paths.extend(session_outputs.written_paths);
            write_json_file(&audit_root.join("session-map.json"), &session_map)?;
            mark_phase_completed(&mut phase_status, "sessions");
        }

        if phase_needs_apply(&phase_status, "apply_files") {
            mark_phase_running(&mut phase_status, "apply_files");
            persist_audit_state(
                &audit_root,
                &import_id,
                source,
                plan,
                &discord_auth_report,
                &agent_map,
                &manifest_agents,
                &written_paths,
                &warnings,
                &apply_agents,
                &phase_status,
                &source_fingerprint,
                "running",
                "running",
                Some(&session_map),
            )?;

            if yaml_path.exists() {
                backup_existing_path(&yaml_path, runtime_root, &backups_root)?;
            }
            config::save_to_path(&yaml_path, &config)
                .map_err(|e| format!("Failed to write '{}': {e}", yaml_path.display()))?;
            written_paths.push(yaml_path.display().to_string());

            for agent in importable_agents.iter().copied() {
                let existing_manifest =
                    manifest_agent_lookup(&manifest_agents, &agent.final_role_id).cloned();
                let prompt_already_done =
                    task_is_finished(&apply_agents, &agent.final_role_id, "prompt_write");
                let memory_already_done =
                    task_is_finished(&apply_agents, &agent.final_role_id, "memory_import");
                let workspace_already_done =
                    task_is_finished(&apply_agents, &agent.final_role_id, "workspace_copy");
                let prompt_path = prompts_root.join(&agent.final_role_id).join("IDENTITY.md");
                let written_prompt_path = if args.no_prompts || prompt_already_done {
                    existing_manifest
                        .as_ref()
                        .and_then(|entry| entry.prompt_path.clone())
                } else {
                    let prompt_content = render_imported_prompt(agent, runtime_root, args);
                    write_text_file(
                        &prompt_path,
                        &prompt_content,
                        runtime_root,
                        &backups_root,
                        args.overwrite || args.resume.is_some(),
                    )?;
                    Some(prompt_path.display().to_string())
                };

                let memory_dir = role_context_root.join(&agent.final_role_id);
                let memory_outputs = if args.no_memory || memory_already_done {
                    existing_manifest
                        .as_ref()
                        .and_then(|entry| entry.memory_dir.as_ref().map(PathBuf::from))
                        .map(|root| collect_existing_tree_files(&root))
                        .transpose()?
                        .unwrap_or_default()
                } else {
                    import_memory_files(
                        agent,
                        &memory_dir,
                        runtime_root,
                        &backups_root,
                        args.overwrite || args.resume.is_some(),
                    )?
                };
                let written_memory_dir = if args.no_memory {
                    None
                } else if memory_already_done {
                    existing_manifest
                        .as_ref()
                        .and_then(|entry| entry.memory_dir.clone())
                } else {
                    Some(memory_dir.display().to_string())
                };

                let workspace_target = if args.no_workspace {
                    None
                } else {
                    let destination = workspaces_root.join(&agent.final_role_id);
                    if !workspace_already_done {
                        copy_workspace_snapshot(
                            Path::new(&agent.workspace_source),
                            &destination,
                            runtime_root,
                            &backups_root,
                            args.overwrite || args.resume.is_some(),
                            &mut warnings,
                        )?;
                    }
                    Some(destination)
                };

                if let Some(prompt_path) = &written_prompt_path {
                    written_paths.push(prompt_path.clone());
                }
                written_paths.extend(memory_outputs.iter().map(|path| path.display().to_string()));
                if let Some(workspace_target) = &workspace_target {
                    written_paths.push(workspace_target.display().to_string());
                }

                upsert_manifest_agent(
                    &mut manifest_agents,
                    ManifestAgent {
                        source_id: agent.source_id.clone(),
                        role_id: agent.final_role_id.clone(),
                        provider: agent
                            .mapped_provider
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string()),
                        prompt_path: written_prompt_path.clone(),
                        memory_dir: written_memory_dir,
                        workspace_source: agent.workspace_source.clone(),
                        workspace_target: workspace_target
                            .as_ref()
                            .map(|path| path.display().to_string()),
                    },
                );

                if let Some(state) = apply_agents.get_mut(&agent.final_role_id) {
                    update_task_state(
                        state,
                        "workspace_copy",
                        if args.no_workspace {
                            "skipped"
                        } else {
                            "completed"
                        },
                        workspace_target
                            .as_ref()
                            .map(|path| vec![path.display().to_string()])
                            .unwrap_or_default(),
                    );
                    update_task_state(
                        state,
                        "prompt_write",
                        if args.no_prompts {
                            "skipped"
                        } else {
                            "completed"
                        },
                        written_prompt_path.clone().into_iter().collect(),
                    );
                    update_task_state(
                        state,
                        "memory_import",
                        if args.no_memory {
                            "skipped"
                        } else {
                            "completed"
                        },
                        memory_outputs
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect(),
                    );
                }

                persist_audit_state(
                    &audit_root,
                    &import_id,
                    source,
                    plan,
                    &discord_auth_report,
                    &agent_map,
                    &manifest_agents,
                    &written_paths,
                    &warnings,
                    &apply_agents,
                    &phase_status,
                    &source_fingerprint,
                    "running",
                    "running",
                    Some(&session_map),
                )?;
            }

            mark_phase_completed(&mut phase_status, "apply_files");
        }
        if args.write_org && phase_needs_apply(&phase_status, "apply_org") {
            mark_phase_running(&mut phase_status, "apply_org");
            persist_audit_state(
                &audit_root,
                &import_id,
                source,
                plan,
                &discord_auth_report,
                &agent_map,
                &manifest_agents,
                &written_paths,
                &warnings,
                &apply_agents,
                &phase_status,
                &source_fingerprint,
                "running",
                "running",
                Some(&session_map),
            )?;

            let rendered_org = org_writer::merge_org_updates(
                runtime_root,
                &build_org_agent_updates(plan, runtime_root, args),
                &build_org_channel_updates(plan, runtime_root, args),
                args.overwrite || args.resume.is_some(),
            )?;
            write_text_file(
                &org_yaml_path,
                &rendered_org,
                runtime_root,
                &backups_root,
                true,
            )?;
            written_paths.push(org_yaml_path.display().to_string());

            for state in apply_agents.values_mut() {
                update_task_state(
                    state,
                    "org_agent_write",
                    "completed",
                    org_task_outputs(&org_yaml_path, args.with_channel_bindings),
                );
            }
            mark_phase_completed(&mut phase_status, "apply_org");
        }

        if args.write_bot_settings && phase_needs_apply(&phase_status, "apply_bot_settings") {
            mark_phase_running(&mut phase_status, "apply_bot_settings");
            persist_audit_state(
                &audit_root,
                &import_id,
                source,
                plan,
                &discord_auth_report,
                &agent_map,
                &manifest_agents,
                &written_paths,
                &warnings,
                &apply_agents,
                &phase_status,
                &source_fingerprint,
                "running",
                "running",
                Some(&session_map),
            )?;

            let bot_entries = build_bot_settings_entry_plans(plan, source, args, &mut warnings)?;
            if !bot_entries.is_empty() {
                let rendered_bot_settings = render_bot_settings_json(
                    &bot_settings_path,
                    &bot_entries,
                    args.overwrite || args.resume.is_some(),
                )?;
                write_text_file(
                    &bot_settings_path,
                    &rendered_bot_settings,
                    runtime_root,
                    &backups_root,
                    true,
                )?;
                written_paths.push(bot_settings_path.display().to_string());
            }
            mark_phase_completed(&mut phase_status, "apply_bot_settings");
        }

        if args.write_db && phase_needs_apply(&phase_status, "apply_db") {
            mark_phase_running(&mut phase_status, "apply_db");
            persist_audit_state(
                &audit_root,
                &import_id,
                source,
                plan,
                &discord_auth_report,
                &agent_map,
                &manifest_agents,
                &written_paths,
                &warnings,
                &apply_agents,
                &phase_status,
                &source_fingerprint,
                "running",
                "running",
                Some(&session_map),
            )?;

            let db_path = apply_db_import(
                &config,
                plan,
                &session_map,
                args,
                runtime_root,
                &mut warnings,
            )?;
            written_paths.push(db_path.display().to_string());
            for state in apply_agents.values_mut() {
                update_task_state(
                    state,
                    "db_upsert",
                    "completed",
                    vec![db_path.display().to_string()],
                );
            }
            mark_phase_completed(&mut phase_status, "apply_db");
        }

        if phase_needs_apply(&phase_status, "finalize") {
            mark_phase_running(&mut phase_status, "finalize");
            if args.snapshot_source {
                let snapshot_root = audit_root.join("snapshot");
                copy_source_snapshot(
                    &source.root,
                    &snapshot_root,
                    args.overwrite || args.resume.is_some(),
                    &mut warnings,
                )?;
                written_paths.push(snapshot_root.display().to_string());
            }
            warnings.sort();
            warnings.dedup();
            written_paths.sort();
            written_paths.dedup();
            persist_audit_state(
                &audit_root,
                &import_id,
                source,
                plan,
                &discord_auth_report,
                &agent_map,
                &manifest_agents,
                &written_paths,
                &warnings,
                &apply_agents,
                &phase_status,
                &source_fingerprint,
                "running",
                "running",
                Some(&session_map),
            )?;
            mark_phase_completed(&mut phase_status, "finalize");
        }
        persist_audit_state(
            &audit_root,
            &import_id,
            source,
            plan,
            &discord_auth_report,
            &agent_map,
            &manifest_agents,
            &written_paths,
            &warnings,
            &apply_agents,
            &phase_status,
            &source_fingerprint,
            "completed",
            "finalized",
            Some(&session_map),
        )?;
        Ok(())
    })();

    if let Err(err) = result {
        mark_first_running_phase_failed(&mut phase_status, &err);
        warnings.sort();
        warnings.dedup();
        written_paths.sort();
        written_paths.dedup();
        let _ = persist_audit_state(
            &audit_root,
            &import_id,
            source,
            plan,
            &discord_auth_report,
            &agent_map,
            &manifest_agents,
            &written_paths,
            &warnings,
            &apply_agents,
            &phase_status,
            &source_fingerprint,
            "failed",
            "failed",
            Some(&session_map),
        );
        return Err(err);
    }

    Ok(())
}

fn build_org_agent_updates(
    plan: &ImportPlan,
    runtime_root: &Path,
    args: &OpenClawMigrateArgs,
) -> Vec<OrgAgentUpdate> {
    plan.agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .map(|agent| OrgAgentUpdate {
            role_id: agent.final_role_id.clone(),
            display_name: agent.display_name.clone(),
            prompt_file: if args.no_prompts {
                None
            } else {
                Some(
                    runtime_layout::managed_agents_root(runtime_root)
                        .join(&agent.final_role_id)
                        .join("IDENTITY.md")
                        .display()
                        .to_string(),
                )
            },
            provider: agent.mapped_provider.clone(),
            model: agent.model_hint.clone(),
            workspace: if args.no_workspace {
                None
            } else {
                Some(
                    runtime_root
                        .join("openclaw")
                        .join("workspaces")
                        .join(&agent.final_role_id)
                        .display()
                        .to_string(),
                )
            },
        })
        .collect()
}

fn build_org_channel_updates(
    plan: &ImportPlan,
    runtime_root: &Path,
    args: &OpenClawMigrateArgs,
) -> Vec<OrgChannelBindingUpdate> {
    if !args.with_channel_bindings {
        return Vec::new();
    }

    let agent_lookup = plan
        .agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .map(|agent| (agent.source_id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();
    let mut updates = Vec::new();

    for binding in plan
        .discord
        .bindings
        .iter()
        .filter(|binding| binding.mode == "live_applicable")
    {
        let Some(agent) = agent_lookup.get(binding.agent_id.as_str()) else {
            continue;
        };
        for channel_id in &binding.channel_ids {
            updates.push(OrgChannelBindingUpdate {
                channel_id: channel_id.clone(),
                agent: agent.final_role_id.clone(),
                workspace: if args.no_workspace {
                    None
                } else {
                    Some(
                        runtime_root
                            .join("openclaw")
                            .join("workspaces")
                            .join(&agent.final_role_id)
                            .display()
                            .to_string(),
                    )
                },
                provider: agent.mapped_provider.clone(),
                model: agent.model_hint.clone(),
            });
        }
    }

    updates
}

fn org_task_outputs(org_yaml_path: &Path, with_channel_bindings: bool) -> Vec<String> {
    let mut outputs = vec![org_yaml_path.display().to_string()];
    if with_channel_bindings {
        outputs.push(format!("{}#channels.by_id", org_yaml_path.display()));
    }
    outputs
}

fn build_initial_apply_agents(
    plan: &ImportPlan,
    args: &OpenClawMigrateArgs,
) -> BTreeMap<String, ApplyAgentState> {
    plan.agents
        .iter()
        .map(|agent| {
            let mut tasks = BTreeMap::new();
            let live_enabled = agent.eligible_for_v1;
            tasks.insert(
                "workspace_copy".to_string(),
                ApplyTaskState {
                    status: if !live_enabled || args.no_workspace {
                        "skipped".to_string()
                    } else {
                        "pending".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "prompt_write".to_string(),
                ApplyTaskState {
                    status: if !live_enabled || args.no_prompts {
                        "skipped".to_string()
                    } else {
                        "pending".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "memory_import".to_string(),
                ApplyTaskState {
                    status: if !live_enabled || args.no_memory {
                        "skipped".to_string()
                    } else {
                        "pending".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "session_import".to_string(),
                ApplyTaskState {
                    status: if live_enabled && args.with_sessions {
                        "pending".to_string()
                    } else {
                        "skipped".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "org_agent_write".to_string(),
                ApplyTaskState {
                    status: if live_enabled && args.write_org {
                        "pending".to_string()
                    } else {
                        "skipped".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "db_upsert".to_string(),
                ApplyTaskState {
                    status: if live_enabled && args.write_db {
                        "pending".to_string()
                    } else {
                        "skipped".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            (agent.final_role_id.clone(), ApplyAgentState { tasks })
        })
        .collect()
}

fn build_initial_phase_status(
    plan: &ImportPlan,
    args: &OpenClawMigrateArgs,
) -> BTreeMap<String, ApplyPhaseState> {
    let now = Utc::now().to_rfc3339();
    plan.phases
        .iter()
        .map(|phase| {
            let status = match phase.phase {
                "scan" | "map" | "prompt" | "memory" | "policy_discord" => "completed",
                "sessions" if args.with_sessions => "pending",
                "apply_org" if args.write_org => "pending",
                "apply_bot_settings" if args.write_bot_settings => "pending",
                "apply_db" if args.write_db => "pending",
                "apply_org" | "apply_bot_settings" | "apply_db" | "sessions" => "skipped",
                _ => "pending",
            };
            (
                phase.phase.to_string(),
                ApplyPhaseState {
                    status: status.to_string(),
                    started_at: if status == "completed" {
                        now.clone()
                    } else {
                        String::new()
                    },
                    ended_at: if status == "completed" {
                        now.clone()
                    } else {
                        String::new()
                    },
                    error: None,
                },
            )
        })
        .collect()
}

fn mark_phase_running(phases: &mut BTreeMap<String, ApplyPhaseState>, phase: &str) {
    if let Some(entry) = phases.get_mut(phase) {
        entry.status = "running".to_string();
        entry.started_at = Utc::now().to_rfc3339();
        entry.ended_at.clear();
        entry.error = None;
    }
}

fn mark_phase_completed(phases: &mut BTreeMap<String, ApplyPhaseState>, phase: &str) {
    if let Some(entry) = phases.get_mut(phase) {
        if entry.started_at.is_empty() {
            entry.started_at = Utc::now().to_rfc3339();
        }
        entry.status = "completed".to_string();
        entry.ended_at = Utc::now().to_rfc3339();
        entry.error = None;
    }
}

fn mark_first_running_phase_failed(phases: &mut BTreeMap<String, ApplyPhaseState>, error: &str) {
    if let Some((_, entry)) = phases
        .iter_mut()
        .find(|(_, phase)| phase.status == "running")
    {
        if entry.started_at.is_empty() {
            entry.started_at = Utc::now().to_rfc3339();
        }
        entry.status = "failed".to_string();
        entry.ended_at = Utc::now().to_rfc3339();
        entry.error = Some(error.to_string());
        return;
    }
    if let Some(entry) = phases.get_mut("finalize") {
        entry.status = "failed".to_string();
        entry.started_at = Utc::now().to_rfc3339();
        entry.ended_at = Utc::now().to_rfc3339();
        entry.error = Some(error.to_string());
    }
}

fn update_task_state(
    state: &mut ApplyAgentState,
    key: &str,
    status: &'static str,
    outputs: Vec<String>,
) {
    state.tasks.insert(
        key.to_string(),
        ApplyTaskState {
            status: status.to_string(),
            outputs,
        },
    );
}

fn load_restored_audit_state(
    audit_root: &Path,
    source_fingerprint: &str,
    args: &OpenClawMigrateArgs,
) -> Result<Option<RestoredAuditState>, String> {
    if args.resume.is_none() {
        return Ok(None);
    }

    let resume_state: ResumeState =
        match read_json_if_exists(&audit_root.join("resume-state.json"))? {
            Some(value) => value,
            None => return Ok(None),
        };
    if resume_state.source_fingerprint != source_fingerprint {
        return Err(format!(
            "Resume fingerprint mismatch for '{}'. Expected '{}', found '{}'.",
            audit_root.display(),
            source_fingerprint,
            resume_state.source_fingerprint
        ));
    }

    let manifest: Option<Manifest> = read_json_if_exists(&audit_root.join("manifest.json"))?;
    let apply_result: Option<ApplyResult> =
        read_json_if_exists(&audit_root.join("apply-result.json"))?;
    let session_map: Option<Vec<SessionMapEntry>> =
        read_json_if_exists(&audit_root.join("session-map.json"))?;

    Ok(Some(RestoredAuditState {
        manifest_agents: manifest
            .as_ref()
            .map(|value| value.agents.clone())
            .unwrap_or_default(),
        written_paths: manifest
            .as_ref()
            .map(|value| value.written_paths.clone())
            .unwrap_or_default(),
        warnings: merge_strings(
            manifest
                .as_ref()
                .map(|value| value.warnings.clone())
                .unwrap_or_default(),
            apply_result
                .as_ref()
                .map(|value| value.warnings.clone())
                .unwrap_or_default(),
        ),
        apply_agents: apply_result
            .as_ref()
            .map(|value| value.agents.clone())
            .unwrap_or_default(),
        phase_status: apply_result
            .as_ref()
            .map(|value| value.phases.clone())
            .unwrap_or_default(),
        session_map: session_map.unwrap_or_default(),
    }))
}

fn read_json_if_exists<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|e| format!("Failed to parse '{}': {e}", path.display()))
}

fn merge_strings(mut left: Vec<String>, right: Vec<String>) -> Vec<String> {
    left.extend(right);
    left.sort();
    left.dedup();
    left
}

fn normalize_resumable_state(
    apply_agents: &mut BTreeMap<String, ApplyAgentState>,
    phase_status: &mut BTreeMap<String, ApplyPhaseState>,
) {
    for phase in phase_status.values_mut() {
        if phase.status == "running" || phase.status == "failed" {
            phase.status = "pending".to_string();
            phase.error = None;
            phase.started_at.clear();
            phase.ended_at.clear();
        }
    }

    for agent in apply_agents.values_mut() {
        for task in agent.tasks.values_mut() {
            if task.status == "running" || task.status == "failed" {
                task.status = "pending".to_string();
            }
        }
    }
}

fn phase_needs_apply(phases: &BTreeMap<String, ApplyPhaseState>, phase: &str) -> bool {
    phases
        .get(phase)
        .map(|state| state.status != "completed" && state.status != "skipped")
        .unwrap_or(true)
}

fn task_is_finished(
    apply_agents: &BTreeMap<String, ApplyAgentState>,
    role_id: &str,
    task_key: &str,
) -> bool {
    apply_agents
        .get(role_id)
        .and_then(|state| state.tasks.get(task_key))
        .map(|task| task.status == "completed" || task.status == "skipped")
        .unwrap_or(false)
}

fn manifest_agent_lookup<'a>(
    manifest_agents: &'a [ManifestAgent],
    role_id: &str,
) -> Option<&'a ManifestAgent> {
    manifest_agents
        .iter()
        .find(|entry| entry.role_id == role_id)
}

fn upsert_manifest_agent(manifest_agents: &mut Vec<ManifestAgent>, next: ManifestAgent) {
    if let Some(existing) = manifest_agents
        .iter_mut()
        .find(|entry| entry.role_id == next.role_id)
    {
        *existing = next;
    } else {
        manifest_agents.push(next);
    }
}

fn collect_existing_tree_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if root.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }

    let mut outputs = Vec::new();
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
                outputs.push(path);
            }
        }
    }
    outputs.sort();
    Ok(outputs)
}

#[allow(clippy::too_many_arguments)]
fn persist_audit_state(
    audit_root: &Path,
    import_id: &str,
    source: &ResolvedSourceRoot,
    plan: &ImportPlan,
    discord_auth_report: &DiscordAuthReportData,
    agent_map: &[AgentMapEntry],
    manifest_agents: &[ManifestAgent],
    written_paths: &[String],
    warnings: &[String],
    apply_agents: &BTreeMap<String, ApplyAgentState>,
    phase_status: &BTreeMap<String, ApplyPhaseState>,
    source_fingerprint: &str,
    apply_status: &'static str,
    resume_status: &'static str,
    session_map: Option<&[SessionMapEntry]>,
) -> Result<(), String> {
    let manifest = Manifest {
        import_id: import_id.to_string(),
        source_root: source.root.display().to_string(),
        config_path: source.config_path.display().to_string(),
        selected_agent_ids: plan.selected_agent_ids.clone(),
        selected_discord_account_ids: plan.selected_discord_account_ids.clone(),
        written_paths: written_paths.to_vec(),
        warnings: warnings.to_vec(),
        agents: manifest_agents.to_vec(),
    };
    write_json_file(&audit_root.join("manifest.json"), &manifest)?;
    write_json_file(&audit_root.join("agent-map.json"), &agent_map)?;
    write_json_file(&audit_root.join("write-plan.json"), plan)?;
    write_json_file(
        &audit_root.join("tool-policy-report.json"),
        &plan.tool_policy,
    )?;
    write_json_file(
        &audit_root.join("discord-auth-report.json"),
        &DiscordAuthReport {
            default_token_configured: discord_auth_report.default_token_configured,
            has_named_accounts: discord_auth_report.has_named_accounts,
            requested_token_mode: &discord_auth_report.requested_token_mode,
            write_bot_settings_enabled: discord_auth_report.write_bot_settings_enabled,
            accounts: &discord_auth_report.accounts,
            account_to_bot_mappings: &discord_auth_report.account_to_bot_mappings,
            bindings: &plan.discord.bindings,
            selected_account_ids: &plan.selected_discord_account_ids,
            warnings,
        },
    )?;
    write_yaml_file(
        &audit_root.join("channel-binding-preview.yaml"),
        &ChannelBindingPreview {
            bindings: &plan.discord.bindings,
            warnings,
        },
    )?;
    if let Some(session_map) = session_map {
        write_json_file(&audit_root.join("session-map.json"), &session_map)?;
    }

    let apply_result = ApplyResult {
        status: apply_status.to_string(),
        import_id: import_id.to_string(),
        source_root: source.root.display().to_string(),
        phases: phase_status.clone(),
        agents: apply_agents.clone(),
        warnings: warnings.to_vec(),
    };
    write_json_file(&audit_root.join("apply-result.json"), &apply_result)?;

    let completed_phases = phase_status
        .iter()
        .filter_map(|(phase, state)| (state.status == "completed").then_some(phase.clone()))
        .collect::<Vec<_>>();
    let pending_phases = phase_status
        .iter()
        .filter_map(|(phase, state)| {
            (state.status == "pending" || state.status == "running").then_some(phase.clone())
        })
        .collect::<Vec<_>>();
    let agents = apply_agents
        .iter()
        .map(|(role_id, state)| {
            (
                role_id.clone(),
                ResumeAgentState {
                    tasks: state
                        .tasks
                        .iter()
                        .map(|(key, value)| (key.clone(), value.status.clone()))
                        .collect(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let next_recommended_step = if resume_status == "finalized" {
        "finalized"
    } else if phase_status.values().any(|phase| phase.status == "failed") {
        "manual_intervention_required"
    } else if !pending_phases.is_empty() {
        "resume_phase"
    } else {
        "resume_agent_tasks"
    };
    let resume_state = ResumeState {
        status: resume_status.to_string(),
        source_path: source.config_path.display().to_string(),
        source_fingerprint: source_fingerprint.to_string(),
        import_id: import_id.to_string(),
        selected_agents: plan.selected_agent_ids.clone(),
        completed_phases,
        pending_phases,
        phases: phase_status
            .iter()
            .map(|(phase, state)| (phase.clone(), state.status.clone()))
            .collect(),
        agents,
        next_recommended_step: next_recommended_step.to_string(),
    };
    write_json_file(&audit_root.join("resume-state.json"), &resume_state)?;
    fs::write(audit_root.join("warnings.txt"), warnings.join("\n"))
        .map_err(|e| format!("Failed to write warnings.txt: {e}"))?;
    Ok(())
}

fn planned_session_map(plan: &ImportPlan, runtime_root: &Path) -> Vec<SessionMapEntry> {
    let ai_sessions_root = runtime_root.join("ai_sessions");
    plan.sessions
        .iter()
        .map(|session| SessionMapEntry {
            source_agent_id: session.source_agent_id.clone(),
            role_id: session.final_role_id.clone(),
            session_key: session.session_key.clone(),
            session_id: session.session_id.clone(),
            transcript_path: session.transcript_path.clone(),
            ai_session_path: ai_sessions_root
                .join(format!("{}.json", session_output_stem(session)))
                .display()
                .to_string(),
            db_session_key: build_db_session_key(session),
        })
        .collect()
}

struct SessionImportOutputs {
    written_paths: Vec<String>,
    agent_outputs: BTreeMap<String, Vec<String>>,
}

fn import_sessions(
    plan: &ImportPlan,
    runtime_root: &Path,
    ai_sessions_root: &Path,
    backups_root: &Path,
    overwrite: bool,
    no_workspace: bool,
) -> Result<SessionImportOutputs, String> {
    let mut written_paths = Vec::new();
    let mut agent_outputs = BTreeMap::<String, Vec<String>>::new();
    let agent_lookup = plan
        .agents
        .iter()
        .map(|agent| (agent.final_role_id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();

    for session in &plan.sessions {
        let output_path = ai_sessions_root.join(format!("{}.json", session_output_stem(session)));
        let Some(agent) = agent_lookup.get(session.final_role_id.as_str()) else {
            continue;
        };
        let history = match session.transcript_path.as_deref() {
            Some(transcript_path) => parse_openclaw_transcript(Path::new(transcript_path))?,
            None => Vec::new(),
        };
        let current_path = if no_workspace {
            session
                .cwd
                .clone()
                .unwrap_or_else(|| agent.workspace_source.clone())
        } else {
            runtime_root
                .join("openclaw")
                .join("workspaces")
                .join(&agent.final_role_id)
                .display()
                .to_string()
        };
        let session_data = SessionData {
            session_id: session.session_id.clone(),
            history,
            current_path,
            created_at: timestamp_to_rfc3339(session.updated_at),
            discord_channel_id: session.channel.as_deref().and_then(parse_u64_str),
            discord_channel_name: session.channel.clone(),
            discord_category_name: None,
            remote_profile_name: None,
            born_generation: 0,
        };
        let rendered = serde_json::to_string_pretty(&session_data)
            .map_err(|e| format!("Failed to serialize '{}': {e}", output_path.display()))?;
        write_text_file(
            &output_path,
            &rendered,
            runtime_root,
            backups_root,
            overwrite,
        )?;
        let rendered_path = output_path.display().to_string();
        written_paths.push(rendered_path.clone());
        agent_outputs
            .entry(session.final_role_id.clone())
            .or_default()
            .push(rendered_path);
    }

    Ok(SessionImportOutputs {
        written_paths,
        agent_outputs,
    })
}

fn parse_openclaw_transcript(path: &Path) -> Result<Vec<HistoryItem>, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut history = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("session") {
            continue;
        }

        let message = value.get("message").unwrap_or(&value);
        let role = message
            .get("role")
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("role").and_then(serde_json::Value::as_str));
        let Some(role) = role else {
            continue;
        };
        let items =
            extract_transcript_history_items(message.get("content").unwrap_or(message), role);
        if items.is_empty() {
            if let Some(content) =
                extract_transcript_text(&value).filter(|text| !text.trim().is_empty())
            {
                history.push(HistoryItem {
                    item_type: map_transcript_role(role),
                    content,
                });
            }
            continue;
        }
        history.extend(items);
    }

    Ok(history)
}

fn extract_transcript_history_items(
    value: &serde_json::Value,
    fallback_role: &str,
) -> Vec<HistoryItem> {
    let Some(items) = value.as_array() else {
        return extract_transcript_text(value)
            .filter(|text| !text.trim().is_empty())
            .map(|content| {
                vec![HistoryItem {
                    item_type: map_transcript_role(fallback_role),
                    content,
                }]
            })
            .unwrap_or_default();
    };

    let mut history = Vec::new();
    for item in items {
        let item_type = item
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|value| value.trim().to_ascii_lowercase());
        match item_type.as_deref() {
            Some("tool_use") | Some("toolcall") | Some("tool_call") => {
                let tool_name = item
                    .get("name")
                    .or_else(|| item.get("tool"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("tool");
                let payload = item
                    .get("input")
                    .or_else(|| item.get("arguments"))
                    .map(render_transcript_block)
                    .unwrap_or_default();
                let content = if payload.is_empty() {
                    tool_name.to_string()
                } else {
                    format!("{tool_name}\n{payload}")
                };
                history.push(HistoryItem {
                    item_type: HistoryType::ToolUse,
                    content,
                });
            }
            Some("tool_result") | Some("tool_result_error") => {
                if let Some(content) = item
                    .get("result")
                    .or_else(|| item.get("output"))
                    .or_else(|| item.get("text"))
                    .map(render_transcript_block)
                    .filter(|text| !text.trim().is_empty())
                {
                    history.push(HistoryItem {
                        item_type: HistoryType::ToolResult,
                        content,
                    });
                }
            }
            _ => {
                if let Some(content) =
                    extract_transcript_text(item).filter(|text| !text.trim().is_empty())
                {
                    history.push(HistoryItem {
                        item_type: map_transcript_role(fallback_role),
                        content,
                    });
                }
            }
        }
    }

    history
}

fn extract_transcript_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(object) = value.as_object() {
        if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
            return Some(text.to_string());
        }
        if let Some(content) = object.get("content") {
            return extract_transcript_text(content);
        }
    }
    let Some(items) = value.as_array() else {
        return None;
    };
    let parts = items
        .iter()
        .filter_map(|item| {
            item.get("text")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| item.as_str().map(ToOwned::to_owned))
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn render_transcript_block(value: &serde_json::Value) -> String {
    if let Some(text) = extract_transcript_text(value) {
        return text;
    }
    if value.is_null() {
        return String::new();
    }
    serde_json::to_string(value).unwrap_or_default()
}

fn map_transcript_role(role: &str) -> HistoryType {
    match role.trim().to_ascii_lowercase().as_str() {
        "user" => HistoryType::User,
        "assistant" => HistoryType::Assistant,
        "system" => HistoryType::System,
        "tool" => HistoryType::ToolResult,
        _ => HistoryType::Assistant,
    }
}

fn build_db_session_key(session: &ImportSessionPlan) -> String {
    format!("openclaw:{}:{}", session.final_role_id, session.session_key)
}

fn session_output_stem(session: &ImportSessionPlan) -> String {
    let base = format!("openclaw-{}-{}", session.final_role_id, session.session_id);
    let sanitized = sanitize_file_component(&base);
    let mut hasher = Sha256::new();
    hasher.update(session.session_key.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{}-{}", sanitized, &digest[..8])
}

fn sanitize_file_component(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

fn timestamp_to_rfc3339(updated_at_ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(updated_at_ms)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

fn parse_u64_str(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

fn apply_db_import(
    config: &config::Config,
    plan: &ImportPlan,
    session_map: &[SessionMapEntry],
    args: &OpenClawMigrateArgs,
    runtime_root: &Path,
    warnings: &mut Vec<String>,
) -> Result<PathBuf, String> {
    fs::create_dir_all(&config.data.dir).map_err(|e| {
        format!(
            "Failed to create data directory '{}': {e}",
            config.data.dir.display()
        )
    })?;
    let db = db::init(config).map_err(|e| format!("Failed to initialize DB: {e}"))?;
    sync_agents_from_config(&db, &config.agents)
        .map_err(|e| format!("Failed to sync imported agents into DB: {e}"))?;

    if args.with_sessions {
        upsert_imported_sessions(&db, plan, session_map, args, runtime_root)?;
    }

    warnings.push(format!(
        "Imported agents were synced into '{}'.",
        config.data.dir.join(&config.data.db_name).display()
    ));
    Ok(config.data.dir.join(&config.data.db_name))
}

fn upsert_imported_sessions(
    db: &db::Db,
    plan: &ImportPlan,
    session_map: &[SessionMapEntry],
    args: &OpenClawMigrateArgs,
    runtime_root: &Path,
) -> Result<(), String> {
    let mut session_map_lookup = HashMap::<(&str, &str), &SessionMapEntry>::new();
    for entry in session_map {
        session_map_lookup.insert((entry.role_id.as_str(), entry.session_key.as_str()), entry);
    }
    let agent_lookup = plan
        .agents
        .iter()
        .map(|agent| (agent.final_role_id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();

    let conn = db
        .lock()
        .map_err(|e| format!("DB lock error during session import: {e}"))?;
    for session in &plan.sessions {
        let Some(agent) = agent_lookup.get(session.final_role_id.as_str()) else {
            continue;
        };
        let Some(session_map) = session_map_lookup
            .get(&(session.final_role_id.as_str(), session.session_key.as_str()))
            .copied()
        else {
            continue;
        };
        let provider = session
            .provider_hint
            .as_deref()
            .and_then(ProviderKind::from_str)
            .map(|provider| provider.as_str().to_string())
            .or_else(|| agent.mapped_provider.clone())
            .unwrap_or_else(|| "claude".to_string());
        let model = session.model.clone().or_else(|| agent.model_hint.clone());
        let cwd = if args.no_workspace {
            session
                .cwd
                .clone()
                .unwrap_or_else(|| agent.workspace_source.clone())
        } else {
            runtime_root
                .join("openclaw")
                .join("workspaces")
                .join(&agent.final_role_id)
                .display()
                .to_string()
        };
        let status = "idle";
        let session_info = serde_json::json!({
            "imported_from": "openclaw",
            "source_agent_id": session.source_agent_id,
            "source_session_key": session.session_key,
            "source_session_id": session.session_id,
            "source_status": session.status,
            "source_updated_at": session.updated_at,
            "source_cwd": session.cwd,
            "ai_session_path": session_map.ai_session_path,
        })
        .to_string();
        let thread_channel_id = session.thread_id.clone();
        let last_heartbeat = None::<String>;

        conn.execute(
            "INSERT INTO sessions (session_key, agent_id, provider, status, session_info, model, tokens, cwd, active_dispatch_id, thread_channel_id, claude_session_id, last_heartbeat)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, NULL, ?10)
             ON CONFLICT(session_key) DO UPDATE SET
               status = excluded.status,
               provider = excluded.provider,
               session_info = excluded.session_info,
               model = COALESCE(excluded.model, sessions.model),
               tokens = excluded.tokens,
               cwd = COALESCE(excluded.cwd, sessions.cwd),
               agent_id = COALESCE(excluded.agent_id, sessions.agent_id),
               thread_channel_id = COALESCE(excluded.thread_channel_id, sessions.thread_channel_id),
               claude_session_id = excluded.claude_session_id,
               last_heartbeat = excluded.last_heartbeat",
            rusqlite::params![
                session_map.db_session_key,
                session.final_role_id,
                provider,
                status,
                session_info,
                model,
                0,
                cwd,
                thread_channel_id,
                last_heartbeat,
            ],
        )
        .map_err(|e| format!("Failed to upsert imported session '{}': {e}", session.session_key))?;
    }

    Ok(())
}

fn planned_bot_account_ids(plan: &ImportPlan) -> BTreeSet<String> {
    let importable_agents = plan
        .agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .map(|agent| agent.source_id.as_str())
        .collect::<BTreeSet<_>>();
    plan.discord
        .bindings
        .iter()
        .filter(|binding| importable_agents.contains(binding.agent_id.as_str()))
        .filter_map(|binding| binding.selected_account_id.clone())
        .collect()
}

#[derive(Default)]
struct DiscordGuildStats {
    guild_count: usize,
    channel_override_count: usize,
    user_allowlist_count: usize,
    role_allowlist_count: usize,
}

fn collect_discord_guild_stats(
    guilds: &BTreeMap<String, OpenClawDiscordGuildConfig>,
) -> DiscordGuildStats {
    let mut stats = DiscordGuildStats::default();
    for guild in guilds.values() {
        stats.guild_count += 1;
        stats.channel_override_count += guild.channels.len();
        stats.user_allowlist_count += guild.users.len();
        stats.role_allowlist_count += guild.roles.len();
        for channel in guild.channels.values() {
            stats.user_allowlist_count += channel.users.len();
            stats.role_allowlist_count += channel.roles.len();
        }
    }
    stats
}

fn allow_bots_enabled(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::Number(value)) => value.as_u64().unwrap_or_default() > 0,
        Some(serde_json::Value::String(value)) => {
            let normalized = value.trim().to_ascii_lowercase();
            !normalized.is_empty()
                && normalized != "false"
                && normalized != "off"
                && normalized != "disabled"
                && normalized != "no"
        }
        _ => false,
    }
}

fn binding_targets_account(
    binding: &OpenClawBindingConfig,
    account_id: &str,
    plan: &ImportPlan,
) -> bool {
    if !binding.r#match.channel.eq_ignore_ascii_case("discord") {
        return false;
    }
    if binding.r#match.account_id.as_deref() == Some(account_id) {
        return true;
    }
    if binding.r#match.account_id.is_some() {
        return false;
    }
    plan.discord.bindings.iter().any(|planned| {
        planned.agent_id == binding.agent_id
            && planned.requested_account_id.is_none()
            && planned.selected_account_id.as_deref() == Some(account_id)
    })
}

fn build_discord_account_mappings(plan: &ImportPlan) -> Vec<DiscordAccountToBotMapping> {
    #[derive(Default)]
    struct MappingAccumulator {
        role_ids: BTreeSet<String>,
        providers: BTreeSet<String>,
        live_channel_ids: BTreeSet<String>,
        live_binding_agents: BTreeSet<String>,
        preview_only_binding_agents: BTreeSet<String>,
    }

    let agent_lookup = plan
        .agents
        .iter()
        .filter(|agent| agent.eligible_for_v1)
        .map(|agent| (agent.source_id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();
    let mut mappings = BTreeMap::<String, MappingAccumulator>::new();

    for binding in &plan.discord.bindings {
        let Some(account_id) = binding.selected_account_id.as_deref() else {
            continue;
        };
        let Some(agent) = agent_lookup.get(binding.agent_id.as_str()) else {
            continue;
        };
        let entry = mappings.entry(account_id.to_string()).or_default();
        entry.role_ids.insert(agent.final_role_id.clone());
        if let Some(provider) = agent.mapped_provider.as_ref() {
            entry.providers.insert(provider.clone());
        }
        if binding.mode == "live_applicable" {
            entry.live_binding_agents.insert(agent.source_id.clone());
            entry
                .live_channel_ids
                .extend(binding.channel_ids.iter().cloned());
        } else {
            entry
                .preview_only_binding_agents
                .insert(agent.source_id.clone());
        }
    }

    mappings
        .into_iter()
        .map(|(account_id, entry)| {
            let mode = if entry.live_channel_ids.is_empty() {
                "preview_only"
            } else {
                "live_applicable"
            };
            DiscordAccountToBotMapping {
                account_id,
                role_ids: entry.role_ids.into_iter().collect(),
                providers: entry.providers.into_iter().collect(),
                live_channel_ids: entry.live_channel_ids.into_iter().collect(),
                live_binding_agents: entry.live_binding_agents.into_iter().collect(),
                preview_only_binding_agents: entry
                    .preview_only_binding_agents
                    .into_iter()
                    .collect(),
                mode,
            }
        })
        .collect()
}

fn classify_discord_token_status(
    source: &ResolvedSourceRoot,
    account_id: &str,
    has_token: bool,
    token_kind_name: &str,
    token_mode: DiscordTokenMode,
    write_bot_settings: bool,
    planned_bot_accounts: &BTreeSet<String>,
) -> String {
    if !has_token {
        return "missing".to_string();
    }
    if !write_bot_settings || !planned_bot_accounts.contains(account_id) {
        return "skipped".to_string();
    }
    if token_mode == DiscordTokenMode::Report {
        return "skipped".to_string();
    }
    if token_mode == DiscordTokenMode::PlaintextOnly && token_kind_name != "plaintext" {
        return "skipped".to_string();
    }
    if token_mode == DiscordTokenMode::ResolveEnvFile && token_kind_name == "exec" {
        return "skipped".to_string();
    }
    match resolve_account_token(source, account_id, token_mode) {
        Ok(Some(_)) => "imported".to_string(),
        Ok(None) => "missing".to_string(),
        Err(_) => "could_not_be_resolved".to_string(),
    }
}

fn build_discord_auth_report(
    plan: &ImportPlan,
    source: &ResolvedSourceRoot,
    args: &OpenClawMigrateArgs,
) -> Result<DiscordAuthReportData, String> {
    let token_mode = DiscordTokenMode::parse(&args.discord_token_mode)?;
    let planned_bot_accounts = planned_bot_account_ids(plan);
    let mut accounts = Vec::new();

    if let Some(discord) = source.config.channels.discord.as_ref() {
        for account in &plan.discord.accounts {
            let (guilds, allow_bots) = if account.account_id == "default" {
                (&discord.guilds, discord.allow_bots.as_ref())
            } else {
                let account_config = discord.accounts.get(&account.account_id);
                (
                    account_config
                        .map(|value| &value.guilds)
                        .unwrap_or(&discord.guilds),
                    account_config.and_then(|value| value.allow_bots.as_ref()),
                )
            };
            let stats = collect_discord_guild_stats(guilds);
            let binding_roles_present = source.config.bindings.iter().any(|binding| {
                !binding.r#match.roles.is_empty()
                    && binding_targets_account(binding, &account.account_id, plan)
            });
            accounts.push(DiscordAccountReportEntry {
                account_id: account.account_id.clone(),
                source: account.source,
                enabled: account.enabled,
                has_token: account.has_token,
                token_kind: account.token_kind.clone(),
                token_status: classify_discord_token_status(
                    source,
                    &account.account_id,
                    account.has_token,
                    &account.token_kind,
                    token_mode,
                    args.write_bot_settings,
                    &planned_bot_accounts,
                ),
                importable: account.importable,
                guild_count: stats.guild_count,
                channel_override_count: stats.channel_override_count,
                user_allowlist_count: stats.user_allowlist_count,
                role_allowlist_count: stats.role_allowlist_count,
                binding_roles_present,
                allow_bots_enabled: allow_bots_enabled(allow_bots),
            });
        }
    }

    Ok(DiscordAuthReportData {
        default_token_configured: source
            .config
            .channels
            .discord
            .as_ref()
            .and_then(|discord| discord.token.as_ref())
            .is_some(),
        has_named_accounts: source
            .config
            .channels
            .discord
            .as_ref()
            .map(|discord| !discord.accounts.is_empty())
            .unwrap_or(false),
        requested_token_mode: args.discord_token_mode.clone(),
        write_bot_settings_enabled: args.write_bot_settings,
        accounts,
        account_to_bot_mappings: build_discord_account_mappings(plan),
    })
}

fn build_bot_settings_entry_plans(
    plan: &ImportPlan,
    source: &ResolvedSourceRoot,
    args: &OpenClawMigrateArgs,
    warnings: &mut Vec<String>,
) -> Result<Vec<BotSettingsEntryPlan>, String> {
    let token_mode = DiscordTokenMode::parse(&args.discord_token_mode)?;
    let tool_policy_mode = ToolPolicyMode::parse(&args.tool_policy_mode)?;

    if token_mode == DiscordTokenMode::Report {
        warnings.push(
            "Discord token mode is report-only, so bot_settings.json live import is skipped."
                .to_string(),
        );
        return Ok(Vec::new());
    }

    let agent_lookup = plan
        .agents
        .iter()
        .map(|agent| (agent.source_id.as_str(), agent))
        .collect::<BTreeMap<_, _>>();

    #[derive(Default)]
    struct BotAccountAccumulator {
        role_ids: BTreeSet<String>,
        providers: BTreeSet<String>,
        channel_ids: BTreeSet<u64>,
        source_agent_ids: BTreeSet<String>,
    }

    let mut by_account = BTreeMap::<String, BotAccountAccumulator>::new();
    for binding in &plan.discord.bindings {
        if binding.mode != "live_applicable" {
            continue;
        }
        let Some(account_id) = binding.selected_account_id.as_deref() else {
            continue;
        };
        let Some(agent) = agent_lookup.get(binding.agent_id.as_str()) else {
            continue;
        };
        if !agent.eligible_for_v1 {
            continue;
        }
        let entry = by_account.entry(account_id.to_string()).or_default();
        entry.role_ids.insert(agent.final_role_id.clone());
        entry.source_agent_ids.insert(agent.source_id.clone());
        if let Some(provider) = agent.mapped_provider.as_ref() {
            entry.providers.insert(provider.clone());
        }
        if args.with_channel_bindings && args.write_org && binding.mode == "live_applicable" {
            for channel_id in &binding.channel_ids {
                if let Some(parsed) = parse_u64_str(channel_id) {
                    entry.channel_ids.insert(parsed);
                }
            }
        }
    }

    let mut entries = Vec::new();
    for (account_id, accumulator) in by_account {
        let Some(token) = resolve_account_token(source, &account_id, token_mode)? else {
            warnings.push(format!(
                "Skipping bot_settings import for Discord account '{}': token could not be resolved under mode '{}'.",
                account_id,
                token_mode.as_str()
            ));
            continue;
        };
        if accumulator.providers.len() > 1 {
            warnings.push(format!(
                "Skipping bot_settings import for Discord account '{}': multiple providers map to one token ({:?}).",
                account_id,
                accumulator.providers
            ));
            continue;
        }
        let provider = accumulator
            .providers
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "claude".to_string());
        if tool_policy_mode == ToolPolicyMode::BotIntersection
            && plan
                .tool_policy
                .agents
                .iter()
                .filter(|scan| accumulator.source_agent_ids.contains(&scan.agent_id))
                .any(|scan| scan.has_sender_scoped_policy)
        {
            warnings.push(format!(
                "Skipping bot_settings tool auto-apply for Discord account '{}': bot-intersection cannot preserve sender-scoped OpenClaw tool policy.",
                account_id
            ));
            continue;
        }
        let role_id = if accumulator.role_ids.len() == 1 {
            accumulator.role_ids.iter().next().cloned()
        } else {
            None
        };
        let allowed_tools = build_bot_allowed_tools(
            plan,
            &accumulator.source_agent_ids,
            &provider,
            tool_policy_mode,
            warnings,
        );
        let allowed_channel_ids = if args.with_channel_bindings
            && args.write_org
            && !accumulator.channel_ids.is_empty()
        {
            Some(accumulator.channel_ids.into_iter().collect())
        } else {
            None
        };
        if accumulator.role_ids.len() > 1 && allowed_channel_ids.is_none() {
            warnings.push(format!(
                "Skipping bot_settings import for Discord account '{}': multiple imported agents share the same token, but no live channel allowlist could be written to keep routing scoped.",
                account_id
            ));
            continue;
        }
        entries.push(BotSettingsEntryPlan {
            account_id,
            provider,
            role_id,
            token,
            allowed_channel_ids,
            allowed_tools,
        });
    }

    Ok(entries)
}

fn build_bot_allowed_tools(
    plan: &ImportPlan,
    source_agent_ids: &BTreeSet<String>,
    provider: &str,
    mode: ToolPolicyMode,
    warnings: &mut Vec<String>,
) -> Option<Vec<String>> {
    if mode == ToolPolicyMode::Report {
        return None;
    }

    let mut sets = Vec::new();
    for agent in plan
        .tool_policy
        .agents
        .iter()
        .filter(|agent| source_agent_ids.contains(&agent.agent_id))
    {
        let tools = agent.normalized_candidate_tools.clone();
        if !tools.is_empty() {
            sets.push(tools);
        }
    }

    if sets.is_empty() {
        warnings.push(format!(
            "No explicit OpenClaw allowlist was recognized for provider '{}'; using AgentDesk default allowed tools.",
            provider
        ));
        return Some(
            DEFAULT_ALLOWED_TOOLS
                .iter()
                .map(|tool| (*tool).to_string())
                .collect(),
        );
    }

    let combined = match mode {
        ToolPolicyMode::BotUnion => {
            let mut union = BTreeSet::new();
            for set in sets {
                union.extend(set);
            }
            union.into_iter().collect::<Vec<_>>()
        }
        ToolPolicyMode::BotIntersection => {
            let mut iter = sets.into_iter();
            let Some(first) = iter.next() else {
                return Some(
                    DEFAULT_ALLOWED_TOOLS
                        .iter()
                        .map(|tool| (*tool).to_string())
                        .collect(),
                );
            };
            let mut intersection = first.into_iter().collect::<BTreeSet<_>>();
            for set in iter {
                let current = set.into_iter().collect::<BTreeSet<_>>();
                intersection = intersection.intersection(&current).cloned().collect();
            }
            if intersection.is_empty() {
                warnings.push(
                    "Tool-policy intersection resolved to an empty allowlist; using AgentDesk default allowed tools."
                        .to_string(),
                );
                DEFAULT_ALLOWED_TOOLS
                    .iter()
                    .map(|tool| (*tool).to_string())
                    .collect()
            } else {
                intersection.into_iter().collect()
            }
        }
        ToolPolicyMode::Report => unreachable!(),
    };

    Some(combined)
}

fn render_bot_settings_json(
    path: &Path,
    entries: &[BotSettingsEntryPlan],
    _overwrite: bool,
) -> Result<String, String> {
    let mut root = if path.exists() {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
        serde_json::from_str::<serde_json::Value>(&content)
            .map_err(|e| format!("Failed to parse '{}': {e}", path.display()))?
    } else {
        serde_json::json!({})
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| format!("'{}' root must be a JSON object.", path.display()))?;
    for entry in entries {
        let key = discord_token_hash(&entry.token);
        let mut entry_json = object
            .get(&key)
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        entry_json.insert("token".to_string(), serde_json::json!(entry.token));
        entry_json.insert("provider".to_string(), serde_json::json!(entry.provider));
        entry_json.insert("agent".to_string(), serde_json::json!(entry.role_id));
        if let Some(allowed_channel_ids) = entry.allowed_channel_ids.as_ref() {
            entry_json.insert(
                "allowed_channel_ids".to_string(),
                serde_json::json!(allowed_channel_ids),
            );
        }
        if let Some(allowed_tools) = entry.allowed_tools.as_ref() {
            entry_json.insert(
                "allowed_tools".to_string(),
                serde_json::json!(allowed_tools),
            );
        }
        object.insert(key, serde_json::Value::Object(entry_json));
    }
    serde_json::to_string_pretty(&root)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))
}

fn resolve_account_token(
    source: &ResolvedSourceRoot,
    account_id: &str,
    mode: DiscordTokenMode,
) -> Result<Option<String>, String> {
    let Some(discord) = source.config.channels.discord.as_ref() else {
        return Ok(None);
    };
    let token_value = if account_id == "default" {
        discord.token.as_ref()
    } else {
        discord
            .accounts
            .get(account_id)
            .and_then(|account| account.token.as_ref())
    };
    let Some(token_value) = token_value else {
        return Ok(None);
    };
    resolve_token_value(
        token_value,
        source.config.secrets.as_ref(),
        &source.root,
        mode,
    )
    .map(Some)
}

#[derive(Clone, Debug)]
struct SecretRefSpec {
    source: String,
    provider: String,
    id: String,
    implicit_provider: bool,
}

fn resolve_token_value(
    token_value: &serde_json::Value,
    secrets: Option<&OpenClawSecretsConfig>,
    source_root: &Path,
    mode: DiscordTokenMode,
) -> Result<String, String> {
    if let Some(value) = token_value.as_str() {
        let trimmed = value.trim();
        if !(trimmed.starts_with("${") && trimmed.ends_with('}')) {
            return Ok(trimmed.to_string());
        }
    }

    let secret_ref = parse_secret_ref(token_value, secrets)?;
    let Some(secret_ref) = secret_ref else {
        return Err("unsupported Discord token shape".to_string());
    };

    match (mode, secret_ref.source.as_str()) {
        (DiscordTokenMode::PlaintextOnly, _) => {
            Err("token mode plaintext-only does not allow SecretRef tokens".to_string())
        }
        (DiscordTokenMode::ResolveEnvFile, "exec") => {
            Err("token mode resolve-env-file does not allow exec SecretRefs".to_string())
        }
        (DiscordTokenMode::Report, _) => {
            Err("token mode report does not resolve tokens".to_string())
        }
        _ => resolve_secret_ref_value(&secret_ref, secrets, source_root),
    }
}

fn parse_secret_ref(
    token_value: &serde_json::Value,
    secrets: Option<&OpenClawSecretsConfig>,
) -> Result<Option<SecretRefSpec>, String> {
    if let Some(raw) = token_value.as_str() {
        let trimmed = raw.trim();
        if let Some(name) = trimmed
            .strip_prefix("${")
            .and_then(|value| value.strip_suffix('}'))
        {
            let provider = secrets
                .and_then(|secrets| secrets.defaults.env.clone())
                .unwrap_or_else(|| "default".to_string());
            return Ok(Some(SecretRefSpec {
                source: "env".to_string(),
                provider,
                id: name.to_string(),
                implicit_provider: secrets
                    .and_then(|value| value.defaults.env.as_ref())
                    .is_none(),
            }));
        }
        return Ok(None);
    }
    let Some(object) = token_value.as_object() else {
        return Ok(None);
    };
    let Some(source) = object
        .get("source")
        .and_then(serde_json::Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
    else {
        return Ok(None);
    };
    let Some(id) = object
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Err("SecretRef token is missing id".to_string());
    };
    let implicit_provider = object.get("provider").is_none()
        && match source.as_str() {
            "env" => secrets
                .and_then(|secrets| secrets.defaults.env.as_ref())
                .is_none(),
            _ => false,
        };
    let provider = object
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| match source.as_str() {
            "env" => secrets.and_then(|secrets| secrets.defaults.env.clone()),
            "file" => secrets.and_then(|secrets| secrets.defaults.file.clone()),
            "exec" => secrets.and_then(|secrets| secrets.defaults.exec.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "default".to_string());
    Ok(Some(SecretRefSpec {
        source,
        provider,
        id: id.to_string(),
        implicit_provider,
    }))
}

fn resolve_secret_ref_value(
    secret_ref: &SecretRefSpec,
    secrets: Option<&OpenClawSecretsConfig>,
    source_root: &Path,
) -> Result<String, String> {
    if secret_ref.source == "env" && secret_ref.implicit_provider {
        return std::env::var(&secret_ref.id).map_err(|_| {
            format!(
                "Env SecretRef '{}' is not set in the current environment.",
                secret_ref.id
            )
        });
    }

    let provider = secrets
        .and_then(|secrets| secrets.providers.get(&secret_ref.provider))
        .ok_or_else(|| {
            format!(
                "Missing OpenClaw secrets provider '{}' for {} SecretRef.",
                secret_ref.provider, secret_ref.source
            )
        })?;

    match provider {
        OpenClawSecretProviderConfig::Env { allowlist } => {
            if !allowlist.is_empty() && !allowlist.iter().any(|item| item == &secret_ref.id) {
                return Err(format!(
                    "Env SecretRef '{}' is not in allowlist for provider '{}'.",
                    secret_ref.id, secret_ref.provider
                ));
            }
            std::env::var(&secret_ref.id).map_err(|_| {
                format!(
                    "Env SecretRef '{}' is not set in the current environment.",
                    secret_ref.id
                )
            })
        }
        OpenClawSecretProviderConfig::File {
            path,
            mode,
            max_bytes,
            ..
        } => {
            let resolved_path = {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    source_root.join(path)
                }
            };
            let bytes = fs::read(&resolved_path).map_err(|e| {
                format!(
                    "Failed to read SecretRef file provider '{}' at '{}': {e}",
                    secret_ref.provider,
                    resolved_path.display()
                )
            })?;
            if let Some(limit) = max_bytes {
                if bytes.len() as u64 > *limit {
                    return Err(format!(
                        "SecretRef file provider '{}' exceeded maxBytes.",
                        secret_ref.provider
                    ));
                }
            }
            let raw = String::from_utf8(bytes).map_err(|e| {
                format!(
                    "SecretRef file '{}' is not UTF-8: {e}",
                    resolved_path.display()
                )
            })?;
            if mode.as_deref() == Some("json") {
                let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
                    format!(
                        "Failed to parse JSON SecretRef file '{}': {e}",
                        resolved_path.display()
                    )
                })?;
                extract_secret_json_value(&value, &secret_ref.id)
            } else {
                Ok(raw.trim().to_string())
            }
        }
        OpenClawSecretProviderConfig::Exec {
            command,
            args,
            json_only,
            env,
            pass_env,
            ..
        } => {
            let mut cmd = Command::new(command);
            cmd.args(args);
            cmd.stdin(Stdio::null());
            cmd.stderr(Stdio::piped());
            cmd.stdout(Stdio::piped());
            for (key, value) in env {
                cmd.env(key, value);
            }
            for key in pass_env {
                if let Ok(value) = std::env::var(key) {
                    cmd.env(key, value);
                }
            }
            let output = cmd.output().map_err(|e| {
                format!(
                    "Failed to execute SecretRef provider '{}': {e}",
                    secret_ref.provider
                )
            })?;
            if !output.status.success() {
                return Err(format!(
                    "SecretRef exec provider '{}' failed: {}",
                    secret_ref.provider,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            let stdout = String::from_utf8(output.stdout).map_err(|e| {
                format!(
                    "SecretRef exec provider '{}' returned non UTF-8 output: {e}",
                    secret_ref.provider
                )
            })?;
            let trimmed = stdout.trim();
            if json_only == &Some(true) || trimmed.starts_with('{') || trimmed.starts_with('[') {
                let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
                    format!(
                        "SecretRef exec provider '{}' returned invalid JSON: {e}",
                        secret_ref.provider
                    )
                })?;
                extract_secret_json_value(&value, &secret_ref.id)
            } else {
                Ok(trimmed.to_string())
            }
        }
    }
}

fn extract_secret_json_value(value: &serde_json::Value, id: &str) -> Result<String, String> {
    if let Some(pointer_value) = value.pointer(id).and_then(serde_json::Value::as_str) {
        return Ok(pointer_value.to_string());
    }
    if let Some(object_value) = value.get(id).and_then(serde_json::Value::as_str) {
        return Ok(object_value.to_string());
    }
    if let Some(text) = value.as_str() {
        return Ok(text.to_string());
    }
    Err(format!(
        "SecretRef JSON payload did not contain string id '{}'.",
        id
    ))
}

fn copy_source_snapshot(
    source_root: &Path,
    snapshot_root: &Path,
    overwrite: bool,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    if snapshot_root.exists() {
        if !overwrite {
            return Err(format!(
                "Source snapshot '{}' already exists. Re-run with --overwrite to replace it.",
                snapshot_root.display()
            ));
        }
        fs::remove_dir_all(snapshot_root).map_err(|e| {
            format!(
                "Failed to replace existing source snapshot '{}': {e}",
                snapshot_root.display()
            )
        })?;
    }
    copy_tree_filtered(source_root, snapshot_root, warnings)
}

fn merge_imported_agents(
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
            channels: std::collections::HashMap::new(),
            department: None,
            avatar_emoji: agent.avatar_emoji.clone(),
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

fn render_imported_prompt(
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

fn import_memory_files(
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

fn copy_workspace_snapshot(
    source_root: &Path,
    destination_root: &Path,
    runtime_root: &Path,
    backups_root: &Path,
    overwrite: bool,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    if destination_root.exists() {
        if !overwrite {
            return Err(format!(
                "Workspace snapshot '{}' already exists. Re-run with --overwrite to replace it.",
                destination_root.display()
            ));
        }
        backup_existing_path(destination_root, runtime_root, backups_root)?;
        fs::remove_dir_all(destination_root).map_err(|e| {
            format!(
                "Failed to replace existing workspace '{}': {e}",
                destination_root.display()
            )
        })?;
    }

    copy_tree_filtered(source_root, destination_root, warnings)
}

fn copy_tree_filtered(
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

fn write_text_file(
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

fn backup_existing_path(
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

fn write_json_file(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    let rendered = serde_json::to_string_pretty(value)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    Ok(())
}

fn write_yaml_file(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    let rendered = serde_yaml::to_string(value)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    Ok(())
}

fn source_fingerprint(
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
