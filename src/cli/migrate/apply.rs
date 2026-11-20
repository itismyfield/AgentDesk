use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::db;
use crate::runtime_layout;
use crate::services::discord::org_writer;
use crate::services::discord::runtime_store::org_schema_path_for_root;
use crate::services::provider::ProviderKind;

use super::OpenClawMigrateArgs;
use super::plan::{ImportAgentPlan, ImportPlan};
use super::source::ResolvedSourceRoot;

mod agent_assets;
mod discord_auth;
mod fingerprint;
mod fs_write;
mod org_updates;
mod phase_state;
mod secrets;
mod sessions;

use agent_assets::{import_memory_files, merge_imported_agents, render_imported_prompt};
use discord_auth::{
    build_bot_settings_entry_plans, build_discord_auth_report, render_bot_settings_json,
};
use fingerprint::source_fingerprint;
use fs_write::{
    backup_existing_path, copy_tree_filtered, write_json_file, write_text_file, write_yaml_file,
};
use org_updates::{build_org_agent_updates, build_org_channel_updates, org_task_outputs};
use phase_state::{
    build_initial_apply_agents, build_initial_phase_status, collect_existing_tree_files,
    load_restored_audit_state, manifest_agent_lookup, mark_first_running_phase_failed,
    mark_phase_completed, mark_phase_running, merge_strings, normalize_resumable_state,
    phase_needs_apply, task_is_finished, update_task_state, upsert_manifest_agent,
};
use sessions::{import_sessions, planned_session_map};

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
    provider: String,
    role_id: Option<String>,
    token: String,
    allowed_channel_ids: Option<Vec<u64>>,
    allowed_tools: Option<Vec<String>>,
}

/// Partition the planned agents into importable (eligible for v1) and skipped
/// (source ids), erroring early when nothing remains importable. Behavior is
/// identical to the inline prelude it replaces in `apply_import_plan`.
fn apply_import_partition_agents(
    plan: &ImportPlan,
) -> Result<(Vec<&ImportAgentPlan>, Vec<String>), String> {
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

    Ok((importable_agents, skipped_agents))
}

/// Persist an in-progress audit snapshot with the canonical `running`/`running`
/// status pair and the live session map. This is the exact `persist_audit_state`
/// invocation repeated between each apply phase; extracting it removes the
/// duplicated 16-argument call without altering any value passed through.
#[allow(clippy::too_many_arguments)]
fn persist_running_audit_state(
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
    session_map: &[SessionMapEntry],
) -> Result<(), String> {
    persist_audit_state(
        audit_root,
        import_id,
        source,
        plan,
        discord_auth_report,
        agent_map,
        manifest_agents,
        written_paths,
        warnings,
        apply_agents,
        phase_status,
        source_fingerprint,
        "running",
        "running",
        Some(session_map),
    )
}

/// Apply the per-agent file outputs (prompt, memory, workspace) for a single
/// importable agent, update its manifest/task state, and persist a running audit
/// snapshot — exactly the body that previously ran inline inside the
/// `apply_files` phase loop. The mutable accumulators are threaded through by
/// reference so the ordering, resume short-circuits, and side effects are
/// byte-for-byte identical to the original loop iteration.
#[allow(clippy::too_many_arguments)]
fn apply_import_agent_files(
    agent: &ImportAgentPlan,
    args: &OpenClawMigrateArgs,
    runtime_root: &Path,
    backups_root: &Path,
    prompts_root: &Path,
    role_context_root: &Path,
    workspaces_root: &Path,
    audit_root: &Path,
    import_id: &str,
    source: &ResolvedSourceRoot,
    plan: &ImportPlan,
    discord_auth_report: &DiscordAuthReportData,
    agent_map: &[AgentMapEntry],
    source_fingerprint: &str,
    session_map: &[SessionMapEntry],
    manifest_agents: &mut Vec<ManifestAgent>,
    written_paths: &mut Vec<String>,
    warnings: &mut Vec<String>,
    apply_agents: &mut BTreeMap<String, ApplyAgentState>,
    phase_status: &BTreeMap<String, ApplyPhaseState>,
) -> Result<(), String> {
    let existing_manifest = manifest_agent_lookup(manifest_agents, &agent.final_role_id).cloned();
    let prompt_already_done = task_is_finished(apply_agents, &agent.final_role_id, "prompt_write");
    let memory_already_done = task_is_finished(apply_agents, &agent.final_role_id, "memory_import");
    let workspace_already_done =
        task_is_finished(apply_agents, &agent.final_role_id, "workspace_copy");
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
            backups_root,
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
            backups_root,
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
                backups_root,
                args.overwrite || args.resume.is_some(),
                warnings,
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
        manifest_agents,
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

    persist_running_audit_state(
        audit_root,
        import_id,
        source,
        plan,
        discord_auth_report,
        agent_map,
        manifest_agents,
        written_paths,
        warnings,
        apply_agents,
        phase_status,
        source_fingerprint,
        session_map,
    )
}

pub(super) fn apply_import_plan(
    plan: &ImportPlan,
    source: &ResolvedSourceRoot,
    args: &OpenClawMigrateArgs,
    runtime_root: &Path,
) -> Result<(), String> {
    let (importable_agents, skipped_agents) = apply_import_partition_agents(plan)?;

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

    persist_running_audit_state(
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
        &session_map,
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
            persist_running_audit_state(
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
                &session_map,
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
            persist_running_audit_state(
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
                &session_map,
            )?;

            if yaml_path.exists() {
                backup_existing_path(&yaml_path, runtime_root, &backups_root)?;
            }
            config::save_to_path(&yaml_path, &config)
                .map_err(|e| format!("Failed to write '{}': {e}", yaml_path.display()))?;
            written_paths.push(yaml_path.display().to_string());

            for agent in importable_agents.iter().copied() {
                apply_import_agent_files(
                    agent,
                    args,
                    runtime_root,
                    &backups_root,
                    &prompts_root,
                    &role_context_root,
                    &workspaces_root,
                    &audit_root,
                    &import_id,
                    source,
                    plan,
                    &discord_auth_report,
                    &agent_map,
                    &source_fingerprint,
                    &session_map,
                    &mut manifest_agents,
                    &mut written_paths,
                    &mut warnings,
                    &mut apply_agents,
                    &phase_status,
                )?;
            }

            mark_phase_completed(&mut phase_status, "apply_files");
        }
        if args.write_org && phase_needs_apply(&phase_status, "apply_org") {
            mark_phase_running(&mut phase_status, "apply_org");
            persist_running_audit_state(
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
                &session_map,
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
            persist_running_audit_state(
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
                &session_map,
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
            persist_running_audit_state(
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
                &session_map,
            )?;

            let db_target = apply_db_import(
                &config,
                plan,
                &session_map,
                args,
                runtime_root,
                &mut warnings,
            )?;
            written_paths.push(db_target.clone());
            for state in apply_agents.values_mut() {
                update_task_state(state, "db_upsert", "completed", vec![db_target.clone()]);
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
            persist_running_audit_state(
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
                &session_map,
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

fn apply_db_import(
    config: &config::Config,
    plan: &ImportPlan,
    session_map: &[SessionMapEntry],
    args: &OpenClawMigrateArgs,
    runtime_root: &Path,
    warnings: &mut Vec<String>,
) -> Result<String, String> {
    if !crate::services::settings::startup::shared_config_sync_enabled(config) {
        return Err(format!(
            "OpenClaw --write-db imports run the destructive config-to-DB agent roster sync; run this command only on a single-node deployment or the configured cluster hub (cluster.role={}).",
            config.cluster.role
        ));
    }
    fs::create_dir_all(&config.data.dir).map_err(|e| {
        format!(
            "Failed to create data directory '{}': {e}",
            config.data.dir.display()
        )
    })?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("Failed to create PostgreSQL import runtime: {e}"))?;
    runtime.block_on(async {
        let pool = db::postgres::connect_and_migrate(config)
            .await
            .map_err(|e| format!("Failed to initialize PostgreSQL: {e}"))?
            .ok_or_else(|| "PostgreSQL is required for OpenClaw DB import.".to_string())?;
        db::postgres::sync_agents_from_config_pg(&pool, &config.agents)
            .await
            .map_err(|e| format!("Failed to sync imported agents into PostgreSQL: {e}"))?;

        if args.with_sessions {
            upsert_imported_sessions_pg(&pool, plan, session_map, args, runtime_root).await?;
        }

        Ok::<(), String>(())
    })?;

    warnings.push("Imported agents were synced into PostgreSQL.".to_string());
    Ok("postgresql".to_string())
}

async fn upsert_imported_sessions_pg(
    pool: &sqlx::PgPool,
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

        sqlx::query(
            "INSERT INTO sessions (session_key, agent_id, provider, status, session_info, model, tokens, cwd, active_dispatch_id, thread_channel_id, claude_session_id, last_heartbeat)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, $9, NULL, CAST($10 AS timestamptz))
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
               last_heartbeat = excluded.last_heartbeat"
        )
        .bind(&session_map.db_session_key)
        .bind(&session.final_role_id)
        .bind(&provider)
        .bind(status)
        .bind(&session_info)
        .bind(&model)
        .bind(0_i64)
        .bind(&cwd)
        .bind(&thread_channel_id)
        .bind(&last_heartbeat)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to upsert imported session '{}': {e}", session.session_key))?;
    }

    Ok(())
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
