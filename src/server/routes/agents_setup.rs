use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::AppState;
use crate::config::Config;
use crate::services::discord::agentdesk_config::{
    AgentSetupConfigInput, AgentSetupConfigMutation, ensure_agent_setup_config,
    load_agent_setup_config,
};
use crate::services::provider::ProviderKind;

#[derive(Debug, Deserialize)]
pub(super) struct AgentSetupBody {
    pub(super) agent_id: String,
    pub(super) channel_id: String,
    pub(super) provider: String,
    pub(super) prompt_template_path: String,
    #[serde(default)]
    pub(super) skills: Vec<String>,
    #[serde(default)]
    pub(super) dry_run: bool,
}

#[derive(Clone, Debug, Serialize)]
struct MutationRecord {
    step: String,
    idempotency_key: String,
    target: String,
    action: String,
    status: String,
    rollback_available: bool,
    validation: String,
}

#[derive(Clone, Debug, Serialize)]
struct SetupError {
    step: String,
    message: String,
    kind: String,
}

#[derive(Clone, Debug)]
struct SetupContext {
    agent_id: String,
    channel_id: String,
    provider: String,
    prompt_template_path: PathBuf,
    prompt_dest_path: PathBuf,
    workspace_path: PathBuf,
    runtime_root: PathBuf,
    config_path: PathBuf,
    config_existed: bool,
    original_config_bytes: Option<Vec<u8>>,
    config: Config,
    skills: Vec<String>,
    dry_run: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PlannedStatus {
    Planned,
    Skipped,
    Conflict(String),
}

#[derive(Clone, Debug)]
struct SetupPlan {
    records: Vec<MutationRecord>,
    errors: Vec<SetupError>,
}

#[derive(Clone, Debug)]
enum RollbackAction {
    RestoreConfig {
        path: PathBuf,
        original: Option<Vec<u8>>,
        records: Vec<MutationRecord>,
    },
    RemovePath {
        path: PathBuf,
        record: MutationRecord,
    },
    RemoveDbAgent {
        agent_id: String,
        record: MutationRecord,
    },
    RestoreSkillManifest {
        path: PathBuf,
        original: Option<Vec<u8>>,
        records: Vec<MutationRecord>,
    },
}

#[derive(Clone, Debug)]
struct ExecutionReport {
    created: Vec<MutationRecord>,
    skipped: Vec<MutationRecord>,
    rolled_back: Vec<MutationRecord>,
    errors: Vec<SetupError>,
    planned: Vec<MutationRecord>,
    audit_log: Option<String>,
}

/// POST /api/agents/setup
pub(super) async fn setup_agent(
    State(state): State<AppState>,
    Json(body): Json<AgentSetupBody>,
) -> (StatusCode, Json<Value>) {
    let ctx = match SetupContext::from_body(body) {
        Ok(ctx) => ctx,
        Err(errors) => {
            return setup_json_response(
                StatusCode::BAD_REQUEST,
                false,
                false,
                ExecutionReport::from_errors(errors),
            );
        }
    };

    let plan = build_setup_plan(&state, &ctx).await;
    if ctx.dry_run {
        return setup_json_response(
            StatusCode::OK,
            plan.errors.is_empty(),
            true,
            ExecutionReport {
                created: Vec::new(),
                skipped: Vec::new(),
                rolled_back: Vec::new(),
                errors: plan.errors,
                planned: plan.records,
                audit_log: None,
            },
        );
    }

    if !plan.errors.is_empty() {
        return setup_json_response(
            StatusCode::CONFLICT,
            false,
            false,
            ExecutionReport {
                created: Vec::new(),
                skipped: Vec::new(),
                rolled_back: Vec::new(),
                errors: plan.errors,
                planned: plan.records,
                audit_log: None,
            },
        );
    }

    let report = execute_setup(&state, ctx, plan.records).await;
    let ok = report.errors.is_empty();
    let status = if ok && !report.created.is_empty() {
        StatusCode::CREATED
    } else if ok {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    setup_json_response(status, ok, false, report)
}

impl ExecutionReport {
    fn from_errors(errors: Vec<SetupError>) -> Self {
        Self {
            created: Vec::new(),
            skipped: Vec::new(),
            rolled_back: Vec::new(),
            errors,
            planned: Vec::new(),
            audit_log: None,
        }
    }
}

impl SetupContext {
    fn from_body(body: AgentSetupBody) -> Result<Self, Vec<SetupError>> {
        let mut errors = Vec::new();
        let agent_id = body.agent_id.trim().to_string();
        let channel_id = body.channel_id.trim().to_string();
        let provider = body.provider.trim().to_ascii_lowercase();
        let prompt_template = body.prompt_template_path.trim().to_string();

        if !is_safe_segment(&agent_id) {
            errors.push(setup_error(
                "request",
                "agent_id must be a non-empty path-safe identifier",
                "validation",
            ));
        }
        if channel_id.parse::<u64>().is_err() {
            errors.push(setup_error(
                "request",
                "channel_id must be a Discord snowflake string",
                "validation",
            ));
        }
        if ProviderKind::from_str(&provider).is_none() {
            errors.push(setup_error(
                "request",
                format!("unsupported provider '{provider}'"),
                "validation",
            ));
        }
        if prompt_template.is_empty() {
            errors.push(setup_error(
                "request",
                "prompt_template_path is required",
                "validation",
            ));
        }

        let skills = normalize_skill_list(body.skills, &mut errors);

        if !errors.is_empty() {
            return Err(errors);
        }

        let runtime_root = match crate::config::runtime_root() {
            Some(root) => root,
            None => {
                return Err(vec![setup_error(
                    "runtime_root",
                    "AGENTDESK runtime root could not be resolved",
                    "environment",
                )]);
            }
        };

        let prompt_template_path = resolve_setup_path(&runtime_root, &prompt_template);
        let prompt_dest_path = crate::runtime_layout::managed_agents_root(&runtime_root)
            .join(&agent_id)
            .join("IDENTITY.md");
        let workspace_path = runtime_root.join("workspaces").join(&agent_id);

        let (config, config_path, config_existed) = match load_agent_setup_config(&runtime_root) {
            Ok(loaded) => loaded,
            Err(error) => {
                return Err(vec![setup_error("agentdesk_yaml", error, "config")]);
            }
        };
        let original_config_bytes = if config_existed {
            std::fs::read(&config_path).ok()
        } else {
            None
        };

        Ok(Self {
            agent_id,
            channel_id,
            provider,
            prompt_template_path,
            prompt_dest_path,
            workspace_path,
            runtime_root,
            config_path,
            config_existed,
            original_config_bytes,
            config,
            skills,
            dry_run: body.dry_run,
        })
    }

    fn config_input(&self) -> AgentSetupConfigInput {
        AgentSetupConfigInput {
            agent_id: self.agent_id.clone(),
            provider: self.provider.clone(),
            channel_id: self.channel_id.clone(),
            prompt_file: self.prompt_dest_path.display().to_string(),
            workspace: self.workspace_path.display().to_string(),
        }
    }
}

async fn build_setup_plan(state: &AppState, ctx: &SetupContext) -> SetupPlan {
    let mut records = Vec::new();
    let mut errors = Vec::new();

    let config_status = config_planned_status(ctx);
    push_planned_pair(
        ctx,
        &mut records,
        &mut errors,
        "agentdesk_yaml",
        "upsert_agent_config",
        ctx.config_path.display().to_string(),
        &config_status,
        true,
    );
    push_planned_pair(
        ctx,
        &mut records,
        &mut errors,
        "discord_binding",
        "register_existing_channel_binding",
        &format!("discord_channel:{}", ctx.channel_id),
        &config_status,
        true,
    );

    let prompt_status = prompt_planned_status(ctx);
    push_planned_pair(
        ctx,
        &mut records,
        &mut errors,
        "prompt_file",
        "copy_prompt_template",
        ctx.prompt_dest_path.display().to_string(),
        &prompt_status,
        true,
    );

    let workspace_status = workspace_planned_status(ctx);
    push_planned_pair(
        ctx,
        &mut records,
        &mut errors,
        "workspace_seed",
        "create_agent_workspace",
        ctx.workspace_path.display().to_string(),
        &workspace_status,
        true,
    );

    let db_status = db_planned_status(state, ctx).await;
    push_planned_pair(
        ctx,
        &mut records,
        &mut errors,
        "db_seed",
        "seed_agent_row",
        &format!("agents:{}", ctx.agent_id),
        &db_status,
        true,
    );

    for (skill, status) in skill_planned_statuses(ctx) {
        push_planned_pair(
            ctx,
            &mut records,
            &mut errors,
            "skill_mapping",
            "map_skill_to_agent_workspace",
            &format!("skills:{skill}"),
            &status,
            true,
        );
    }

    SetupPlan { records, errors }
}

fn config_planned_status(ctx: &SetupContext) -> PlannedStatus {
    let mut config = ctx.config.clone();
    match ensure_agent_setup_config(&mut config, &ctx.config_input()) {
        AgentSetupConfigMutation::Created => PlannedStatus::Planned,
        AgentSetupConfigMutation::Unchanged => PlannedStatus::Skipped,
        AgentSetupConfigMutation::Conflict(error) => PlannedStatus::Conflict(error),
    }
}

fn prompt_planned_status(ctx: &SetupContext) -> PlannedStatus {
    if !ctx.prompt_template_path.is_file() {
        return PlannedStatus::Conflict(format!(
            "prompt template '{}' does not exist",
            ctx.prompt_template_path.display()
        ));
    }
    if !parent_can_be_created(&ctx.prompt_dest_path) {
        return PlannedStatus::Conflict(format!(
            "prompt destination parent for '{}' is not a directory",
            ctx.prompt_dest_path.display()
        ));
    }
    if ctx.prompt_dest_path.is_dir() {
        return PlannedStatus::Conflict(format!(
            "prompt destination '{}' is a directory",
            ctx.prompt_dest_path.display()
        ));
    }
    if !ctx.prompt_dest_path.exists() {
        return PlannedStatus::Planned;
    }

    if files_have_same_bytes(&ctx.prompt_template_path, &ctx.prompt_dest_path) {
        PlannedStatus::Skipped
    } else {
        PlannedStatus::Conflict(format!(
            "prompt destination '{}' already exists with different content",
            ctx.prompt_dest_path.display()
        ))
    }
}

fn workspace_planned_status(ctx: &SetupContext) -> PlannedStatus {
    if ctx.workspace_path.is_dir() {
        return PlannedStatus::Skipped;
    }
    if ctx.workspace_path.exists() {
        return PlannedStatus::Conflict(format!(
            "workspace target '{}' exists and is not a directory",
            ctx.workspace_path.display()
        ));
    }
    if !parent_can_be_created(&ctx.workspace_path) {
        return PlannedStatus::Conflict(format!(
            "workspace parent for '{}' is not a directory",
            ctx.workspace_path.display()
        ));
    }
    PlannedStatus::Planned
}

async fn db_planned_status(state: &AppState, ctx: &SetupContext) -> PlannedStatus {
    match db_agent_matches(state, ctx).await {
        Ok(DbAgentStatus::Missing) => PlannedStatus::Planned,
        Ok(DbAgentStatus::Matches) => PlannedStatus::Skipped,
        Ok(DbAgentStatus::Conflicts(reason)) => PlannedStatus::Conflict(reason),
        Err(error) => PlannedStatus::Conflict(error),
    }
}

fn skill_planned_statuses(ctx: &SetupContext) -> Vec<(String, PlannedStatus)> {
    ctx.skills
        .iter()
        .map(|skill| {
            let skill_dir =
                crate::runtime_layout::managed_skills_root(&ctx.runtime_root).join(skill);
            let status = if skill_dir.is_dir() {
                match skill_manifest_contains(ctx, skill) {
                    Ok(true) => PlannedStatus::Skipped,
                    Ok(false) => PlannedStatus::Planned,
                    Err(error) => PlannedStatus::Conflict(error),
                }
            } else {
                PlannedStatus::Conflict(format!(
                    "skill '{}' does not exist under '{}'",
                    skill,
                    crate::runtime_layout::managed_skills_root(&ctx.runtime_root).display()
                ))
            };
            (skill.clone(), status)
        })
        .collect()
}

async fn execute_setup(
    state: &AppState,
    ctx: SetupContext,
    planned: Vec<MutationRecord>,
) -> ExecutionReport {
    let mut report = ExecutionReport {
        created: Vec::new(),
        skipped: Vec::new(),
        rolled_back: Vec::new(),
        errors: Vec::new(),
        planned,
        audit_log: None,
    };
    let mut rollback = Vec::<RollbackAction>::new();

    if let Err(error) = apply_config_step(&ctx, &mut report, &mut rollback) {
        finalize_failed_setup(state, ctx, report, rollback, error).await
    } else if let Err(error) = apply_prompt_step(&ctx, &mut report, &mut rollback) {
        finalize_failed_setup(state, ctx, report, rollback, error).await
    } else if let Err(error) = apply_workspace_step(&ctx, &mut report, &mut rollback) {
        finalize_failed_setup(state, ctx, report, rollback, error).await
    } else if let Err(error) = apply_db_step(state, &ctx, &mut report, &mut rollback).await {
        finalize_failed_setup(state, ctx, report, rollback, error).await
    } else if let Err(error) = apply_skill_mapping_step(&ctx, &mut report, &mut rollback) {
        finalize_failed_setup(state, ctx, report, rollback, error).await
    } else {
        match write_audit_log(&ctx, true, &report) {
            Ok(path) => report.audit_log = Some(path.display().to_string()),
            Err(error) => report.errors.push(setup_error("audit_log", error, "audit")),
        }
        report
    }
}

fn apply_config_step(
    ctx: &SetupContext,
    report: &mut ExecutionReport,
    rollback: &mut Vec<RollbackAction>,
) -> Result<(), SetupError> {
    let mut config = ctx.config.clone();
    match ensure_agent_setup_config(&mut config, &ctx.config_input()) {
        AgentSetupConfigMutation::Unchanged => {
            report.skipped.push(record(
                ctx,
                "agentdesk_yaml",
                "upsert_agent_config",
                ctx.config_path.display().to_string(),
                "skipped",
                true,
                "already present",
            ));
            report.skipped.push(record(
                ctx,
                "discord_binding",
                "register_existing_channel_binding",
                format!("discord_channel:{}", ctx.channel_id),
                "skipped",
                true,
                "already present through agentdesk.yaml",
            ));
            Ok(())
        }
        AgentSetupConfigMutation::Conflict(error) => {
            Err(setup_error("agentdesk_yaml", error, "conflict"))
        }
        AgentSetupConfigMutation::Created => {
            if let Some(parent) = ctx.config_path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    setup_error(
                        "agentdesk_yaml",
                        format!("create config dir '{}': {error}", parent.display()),
                        "io",
                    )
                })?;
            }
            crate::config::save_to_path(&ctx.config_path, &config).map_err(|error| {
                setup_error(
                    "agentdesk_yaml",
                    format!("write '{}': {error}", ctx.config_path.display()),
                    "io",
                )
            })?;

            let config_record = record(
                ctx,
                "agentdesk_yaml",
                "upsert_agent_config",
                ctx.config_path.display().to_string(),
                "created",
                true,
                "agent added to agentdesk.yaml",
            );
            let binding_record = record(
                ctx,
                "discord_binding",
                "register_existing_channel_binding",
                format!("discord_channel:{}", ctx.channel_id),
                "created",
                true,
                "channel binding added through agentdesk.yaml",
            );
            report.created.push(config_record.clone());
            report.created.push(binding_record.clone());
            rollback.push(RollbackAction::RestoreConfig {
                path: ctx.config_path.clone(),
                original: ctx
                    .config_existed
                    .then(|| ctx.original_config_bytes.clone())
                    .flatten(),
                records: vec![binding_record, config_record],
            });
            maybe_forced_failure("agentdesk_yaml")?;
            Ok(())
        }
    }
}

fn apply_prompt_step(
    ctx: &SetupContext,
    report: &mut ExecutionReport,
    rollback: &mut Vec<RollbackAction>,
) -> Result<(), SetupError> {
    match prompt_planned_status(ctx) {
        PlannedStatus::Skipped => {
            report.skipped.push(record(
                ctx,
                "prompt_file",
                "copy_prompt_template",
                ctx.prompt_dest_path.display().to_string(),
                "skipped",
                true,
                "already present",
            ));
            Ok(())
        }
        PlannedStatus::Conflict(error) => Err(setup_error("prompt_file", error, "conflict")),
        PlannedStatus::Planned => {
            if let Some(parent) = ctx.prompt_dest_path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    setup_error(
                        "prompt_file",
                        format!("create prompt dir '{}': {error}", parent.display()),
                        "io",
                    )
                })?;
            }
            std::fs::copy(&ctx.prompt_template_path, &ctx.prompt_dest_path).map_err(|error| {
                setup_error(
                    "prompt_file",
                    format!(
                        "copy '{}' to '{}': {error}",
                        ctx.prompt_template_path.display(),
                        ctx.prompt_dest_path.display()
                    ),
                    "io",
                )
            })?;
            let record = record(
                ctx,
                "prompt_file",
                "copy_prompt_template",
                ctx.prompt_dest_path.display().to_string(),
                "created",
                true,
                "prompt template copied",
            );
            report.created.push(record.clone());
            rollback.push(RollbackAction::RemovePath {
                path: ctx.prompt_dest_path.clone(),
                record,
            });
            maybe_forced_failure("prompt_file")?;
            Ok(())
        }
    }
}

fn apply_workspace_step(
    ctx: &SetupContext,
    report: &mut ExecutionReport,
    rollback: &mut Vec<RollbackAction>,
) -> Result<(), SetupError> {
    match workspace_planned_status(ctx) {
        PlannedStatus::Skipped => {
            report.skipped.push(record(
                ctx,
                "workspace_seed",
                "create_agent_workspace",
                ctx.workspace_path.display().to_string(),
                "skipped",
                true,
                "already present",
            ));
            Ok(())
        }
        PlannedStatus::Conflict(error) => Err(setup_error("workspace_seed", error, "conflict")),
        PlannedStatus::Planned => {
            std::fs::create_dir_all(&ctx.workspace_path).map_err(|error| {
                setup_error(
                    "workspace_seed",
                    format!(
                        "create workspace '{}': {error}",
                        ctx.workspace_path.display()
                    ),
                    "io",
                )
            })?;
            let record = record(
                ctx,
                "workspace_seed",
                "create_agent_workspace",
                ctx.workspace_path.display().to_string(),
                "created",
                true,
                "workspace created",
            );
            report.created.push(record.clone());
            rollback.push(RollbackAction::RemovePath {
                path: ctx.workspace_path.clone(),
                record,
            });
            maybe_forced_failure("workspace_seed")?;
            Ok(())
        }
    }
}

async fn apply_db_step(
    state: &AppState,
    ctx: &SetupContext,
    report: &mut ExecutionReport,
    rollback: &mut Vec<RollbackAction>,
) -> Result<(), SetupError> {
    match db_agent_matches(state, ctx).await {
        Ok(DbAgentStatus::Matches) => {
            report.skipped.push(record(
                ctx,
                "db_seed",
                "seed_agent_row",
                format!("agents:{}", ctx.agent_id),
                "skipped",
                true,
                "already present",
            ));
            Ok(())
        }
        Ok(DbAgentStatus::Conflicts(reason)) => Err(setup_error("db_seed", reason, "conflict")),
        Err(error) => Err(setup_error("db_seed", error, "db")),
        Ok(DbAgentStatus::Missing) => {
            insert_db_agent(state, ctx).await?;
            let record = record(
                ctx,
                "db_seed",
                "seed_agent_row",
                format!("agents:{}", ctx.agent_id),
                "created",
                true,
                "agent row inserted",
            );
            report.created.push(record.clone());
            rollback.push(RollbackAction::RemoveDbAgent {
                agent_id: ctx.agent_id.clone(),
                record,
            });
            maybe_forced_failure("db_seed")?;
            Ok(())
        }
    }
}

fn apply_skill_mapping_step(
    ctx: &SetupContext,
    report: &mut ExecutionReport,
    rollback: &mut Vec<RollbackAction>,
) -> Result<(), SetupError> {
    if ctx.skills.is_empty() {
        return Ok(());
    }

    let manifest_path = crate::runtime_layout::managed_skills_manifest_path(&ctx.runtime_root);
    let original = std::fs::read(&manifest_path).ok();
    let mut manifest = load_skill_manifest_value(&manifest_path)?;
    let mut changed_records = Vec::new();

    for (skill, status) in skill_planned_statuses(ctx) {
        match status {
            PlannedStatus::Skipped => report.skipped.push(record(
                ctx,
                "skill_mapping",
                "map_skill_to_agent_workspace",
                format!("skills:{skill}"),
                "skipped",
                true,
                "already present",
            )),
            PlannedStatus::Conflict(error) => {
                return Err(setup_error("skill_mapping", error, "conflict"));
            }
            PlannedStatus::Planned => {
                upsert_skill_manifest_mapping(&mut manifest, &skill, ctx)?;
                let record = record(
                    ctx,
                    "skill_mapping",
                    "map_skill_to_agent_workspace",
                    format!("skills:{skill}"),
                    "created",
                    true,
                    "skill mapped to agent workspace",
                );
                report.created.push(record.clone());
                changed_records.push(record);
            }
        }
    }

    if !changed_records.is_empty() {
        if let Some(parent) = manifest_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                setup_error(
                    "skill_mapping",
                    format!("create skills manifest dir '{}': {error}", parent.display()),
                    "io",
                )
            })?;
        }
        let rendered = serde_json::to_vec_pretty(&manifest)
            .map_err(|error| setup_error("skill_mapping", error.to_string(), "serialize"))?;
        std::fs::write(&manifest_path, rendered).map_err(|error| {
            setup_error(
                "skill_mapping",
                format!("write '{}': {error}", manifest_path.display()),
                "io",
            )
        })?;
        rollback.push(RollbackAction::RestoreSkillManifest {
            path: manifest_path,
            original,
            records: changed_records,
        });
        maybe_forced_failure("skill_mapping")?;
    }

    Ok(())
}

async fn finalize_failed_setup(
    state: &AppState,
    ctx: SetupContext,
    mut report: ExecutionReport,
    rollback: Vec<RollbackAction>,
    error: SetupError,
) -> ExecutionReport {
    report.errors.push(error);
    report.rolled_back = rollback_setup(state, rollback).await;
    match write_audit_log(&ctx, false, &report) {
        Ok(path) => report.audit_log = Some(path.display().to_string()),
        Err(error) => report.errors.push(setup_error("audit_log", error, "audit")),
    }
    report
}

async fn rollback_setup(
    state: &AppState,
    mut rollback: Vec<RollbackAction>,
) -> Vec<MutationRecord> {
    let mut rolled_back = Vec::new();
    while let Some(action) = rollback.pop() {
        match action {
            RollbackAction::RestoreConfig {
                path,
                original,
                records,
            } => {
                let restored = match original {
                    Some(bytes) => std::fs::write(&path, bytes).is_ok(),
                    None => remove_file_if_exists(&path),
                };
                if restored {
                    rolled_back.extend(mark_rolled_back(records));
                }
            }
            RollbackAction::RemovePath { path, record } => {
                if remove_path_if_exists(&path) {
                    rolled_back.push(mark_one_rolled_back(record));
                }
            }
            RollbackAction::RemoveDbAgent { agent_id, record } => {
                if delete_db_agent(state, &agent_id).await {
                    rolled_back.push(mark_one_rolled_back(record));
                }
            }
            RollbackAction::RestoreSkillManifest {
                path,
                original,
                records,
            } => {
                let restored = match original {
                    Some(bytes) => std::fs::write(&path, bytes).is_ok(),
                    None => remove_file_if_exists(&path),
                };
                if restored {
                    rolled_back.extend(mark_rolled_back(records));
                }
            }
        }
    }
    rolled_back
}

#[derive(Debug, PartialEq, Eq)]
enum DbAgentStatus {
    Missing,
    Matches,
    Conflicts(String),
}

async fn db_agent_matches(state: &AppState, ctx: &SetupContext) -> Result<DbAgentStatus, String> {
    if let Some(pool) = state.pg_pool_ref() {
        let row = sqlx::query(
            "SELECT provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
             FROM agents
             WHERE id = $1",
        )
        .bind(&ctx.agent_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("query agent '{}': {error}", ctx.agent_id))?;

        let Some(row) = row else {
            return Ok(DbAgentStatus::Missing);
        };
        let bindings = crate::db::agents::AgentChannelBindings {
            provider: row.try_get("provider").ok().flatten(),
            discord_channel_id: row.try_get("discord_channel_id").ok().flatten(),
            discord_channel_alt: row.try_get("discord_channel_alt").ok().flatten(),
            discord_channel_cc: row.try_get("discord_channel_cc").ok().flatten(),
            discord_channel_cdx: row.try_get("discord_channel_cdx").ok().flatten(),
        };
        return Ok(db_bindings_match(ctx, &bindings));
    }

    Err("postgres pool unavailable".to_string())
}

fn db_bindings_match(
    ctx: &SetupContext,
    bindings: &crate::db::agents::AgentChannelBindings,
) -> DbAgentStatus {
    let provider_matches = bindings.provider.as_deref().unwrap_or("claude") == ctx.provider;
    let channel_matches = bindings
        .channel_for_provider(Some(&ctx.provider))
        .as_deref()
        == Some(ctx.channel_id.as_str());
    if provider_matches && channel_matches {
        DbAgentStatus::Matches
    } else {
        DbAgentStatus::Conflicts(format!(
            "agent '{}' already exists in DB with different setup data",
            ctx.agent_id
        ))
    }
}

async fn insert_db_agent(state: &AppState, ctx: &SetupContext) -> Result<(), SetupError> {
    let (discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx) =
        db_channel_columns(ctx);
    if let Some(pool) = state.pg_pool_ref() {
        sqlx::query(
            "INSERT INTO agents (
                id, name, provider,
                discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
             ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&ctx.agent_id)
        .bind(&ctx.agent_id)
        .bind(&ctx.provider)
        .bind(&discord_channel_id)
        .bind(&discord_channel_alt)
        .bind(&discord_channel_cc)
        .bind(&discord_channel_cdx)
        .execute(pool)
        .await
        .map_err(|error| setup_error("db_seed", format!("insert agent: {error}"), "db"))?;
        return Ok(());
    }

    Err(setup_error("db_seed", "postgres pool unavailable", "db"))
}

async fn delete_db_agent(state: &AppState, agent_id: &str) -> bool {
    if let Some(pool) = state.pg_pool_ref() {
        return sqlx::query("DELETE FROM agents WHERE id = $1")
            .bind(agent_id)
            .execute(pool)
            .await
            .map(|result| result.rows_affected() > 0)
            .unwrap_or(false);
    }

    false
}

fn db_channel_columns(
    ctx: &SetupContext,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    match ctx.provider.as_str() {
        "claude" => (
            Some(ctx.channel_id.clone()),
            None,
            Some(ctx.channel_id.clone()),
            None,
        ),
        "codex" => (
            None,
            Some(ctx.channel_id.clone()),
            None,
            Some(ctx.channel_id.clone()),
        ),
        _ => (Some(ctx.channel_id.clone()), None, None, None),
    }
}

fn setup_json_response(
    status: StatusCode,
    ok: bool,
    dry_run: bool,
    report: ExecutionReport,
) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({
            "ok": ok,
            "dry_run": dry_run,
            "created": report.created,
            "skipped": report.skipped,
            "rolled_back": report.rolled_back,
            "errors": report.errors,
            "planned": report.planned,
            "transaction": {
                "audit_log": report.audit_log,
            },
        })),
    )
}

fn push_planned_pair(
    ctx: &SetupContext,
    records: &mut Vec<MutationRecord>,
    errors: &mut Vec<SetupError>,
    step: &str,
    action: &str,
    target: impl ToString,
    status: &PlannedStatus,
    rollback_available: bool,
) {
    match status {
        PlannedStatus::Planned => records.push(record(
            ctx,
            step,
            action,
            target,
            "planned",
            rollback_available,
            "validated",
        )),
        PlannedStatus::Skipped => records.push(record(
            ctx,
            step,
            action,
            target,
            "skipped",
            rollback_available,
            "idempotent match",
        )),
        PlannedStatus::Conflict(error) => {
            records.push(record(
                ctx,
                step,
                action,
                target,
                "conflict",
                rollback_available,
                error,
            ));
            errors.push(setup_error(step, error.clone(), "conflict"));
        }
    }
}

fn record(
    ctx: &SetupContext,
    step: &str,
    action: &str,
    target: impl ToString,
    status: &str,
    rollback_available: bool,
    validation: impl ToString,
) -> MutationRecord {
    MutationRecord {
        step: step.to_string(),
        idempotency_key: idempotency_key(ctx, step, &target.to_string()),
        target: target.to_string(),
        action: action.to_string(),
        status: status.to_string(),
        rollback_available,
        validation: validation.to_string(),
    }
}

fn setup_error(step: &str, message: impl ToString, kind: &str) -> SetupError {
    SetupError {
        step: step.to_string(),
        message: message.to_string(),
        kind: kind.to_string(),
    }
}

fn idempotency_key(ctx: &SetupContext, step: &str, target: &str) -> String {
    format!(
        "agent-setup:{}:{}:{}:{}",
        ctx.agent_id,
        ctx.provider,
        step,
        sanitize_key(target)
    )
}

fn sanitize_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn normalize_skill_list(raw: Vec<String>, errors: &mut Vec<SetupError>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut skills = Vec::new();
    for value in raw {
        let skill = value.trim();
        if skill.is_empty() {
            continue;
        }
        if !is_safe_skill_name(skill) {
            errors.push(setup_error(
                "request",
                format!("skill '{skill}' must be a path-safe skill id"),
                "validation",
            ));
            continue;
        }
        if seen.insert(skill.to_string()) {
            skills.push(skill.to_string());
        }
    }
    skills
}

fn is_safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

fn is_safe_skill_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
}

fn resolve_setup_path(root: &Path, raw: &str) -> PathBuf {
    let expanded = expand_tilde(raw);
    let candidate = PathBuf::from(&expanded);
    if candidate.is_absolute() {
        return candidate;
    }
    let root_candidate = root.join(&candidate);
    if root_candidate.exists() {
        return root_candidate;
    }
    crate::runtime_layout::config_dir(root).join(candidate)
}

fn expand_tilde(raw: &str) -> String {
    if raw == "~" {
        return dirs::home_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| raw.to_string());
    }
    if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped).display().to_string();
        }
    }
    raw.to_string()
}

fn parent_can_be_created(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return true;
    };
    if parent.exists() {
        return parent.is_dir();
    }
    parent_can_be_created(parent)
}

fn files_have_same_bytes(left: &Path, right: &Path) -> bool {
    match (std::fs::read(left), std::fs::read(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn remove_path_if_exists(path: &Path) -> bool {
    if path.is_dir() {
        std::fs::remove_dir_all(path).is_ok()
    } else {
        remove_file_if_exists(path)
    }
}

fn remove_file_if_exists(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn mark_rolled_back(records: Vec<MutationRecord>) -> Vec<MutationRecord> {
    records.into_iter().map(mark_one_rolled_back).collect()
}

fn mark_one_rolled_back(mut record: MutationRecord) -> MutationRecord {
    record.status = "rolled_back".to_string();
    record.validation = "rollback applied".to_string();
    record
}

fn load_skill_manifest_value(path: &Path) -> Result<Value, SetupError> {
    if !path.exists() {
        return Ok(json!({
            "version": 1,
            "global_core_skills": [],
            "skills": {}
        }));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        setup_error(
            "skill_mapping",
            format!("read '{}': {error}", path.display()),
            "io",
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        setup_error(
            "skill_mapping",
            format!("parse '{}': {error}", path.display()),
            "parse",
        )
    })
}

fn skill_manifest_contains(ctx: &SetupContext, skill: &str) -> Result<bool, String> {
    let path = crate::runtime_layout::managed_skills_manifest_path(&ctx.runtime_root);
    let manifest = load_skill_manifest_value(&path).map_err(|error| error.message)?;
    let entry = manifest
        .get("skills")
        .and_then(Value::as_object)
        .and_then(|skills| skills.get(skill));
    let Some(entry) = entry else {
        return Ok(false);
    };
    let has_provider = entry
        .get("providers")
        .and_then(Value::as_array)
        .map(|providers| {
            providers
                .iter()
                .any(|value| value.as_str() == Some(ctx.provider.as_str()))
        })
        .unwrap_or(false);
    let has_workspace = entry
        .get("workspaces")
        .and_then(Value::as_array)
        .map(|workspaces| {
            workspaces
                .iter()
                .any(|value| value.as_str() == Some(ctx.agent_id.as_str()))
        })
        .unwrap_or(false);
    Ok(has_provider && has_workspace)
}

fn upsert_skill_manifest_mapping(
    manifest: &mut Value,
    skill: &str,
    ctx: &SetupContext,
) -> Result<(), SetupError> {
    if !manifest.is_object() {
        *manifest = json!({
            "version": 1,
            "global_core_skills": [],
            "skills": {}
        });
    }
    let Some(root) = manifest.as_object_mut() else {
        return Err(setup_error(
            "skill_mapping",
            "skills manifest root is not an object",
            "parse",
        ));
    };
    root.entry("version").or_insert_with(|| json!(1));
    root.entry("global_core_skills")
        .or_insert_with(|| json!([]));
    root.entry("skills").or_insert_with(|| json!({}));
    let Some(skills) = root.get_mut("skills").and_then(Value::as_object_mut) else {
        return Err(setup_error(
            "skill_mapping",
            "skills manifest 'skills' field is not an object",
            "parse",
        ));
    };
    let entry = skills.entry(skill.to_string()).or_insert_with(|| {
        json!({
            "providers": [],
            "workspaces": [],
            "global": false
        })
    });
    if !entry.is_object() {
        *entry = json!({
            "providers": [],
            "workspaces": [],
            "global": false
        });
    }
    let Some(entry_obj) = entry.as_object_mut() else {
        return Err(setup_error(
            "skill_mapping",
            format!("skill manifest entry for '{skill}' is not an object"),
            "parse",
        ));
    };
    entry_obj.entry("global").or_insert_with(|| json!(false));
    push_json_array_string(entry_obj, "providers", &ctx.provider);
    push_json_array_string(entry_obj, "workspaces", &ctx.agent_id);
    Ok(())
}

fn push_json_array_string(object: &mut serde_json::Map<String, Value>, key: &str, value: &str) {
    if !object.get(key).is_some_and(Value::is_array) {
        object.insert(key.to_string(), json!([]));
    }
    if let Some(array) = object.get_mut(key).and_then(Value::as_array_mut)
        && !array.iter().any(|item| item.as_str() == Some(value))
    {
        array.push(json!(value));
    }
}

fn write_audit_log(
    ctx: &SetupContext,
    ok: bool,
    report: &ExecutionReport,
) -> Result<PathBuf, String> {
    let audit_dir = crate::runtime_layout::config_dir(&ctx.runtime_root).join(".audit");
    std::fs::create_dir_all(&audit_dir)
        .map_err(|error| format!("create audit dir '{}': {error}", audit_dir.display()))?;
    let audit_path = audit_path(ctx);
    let payload = json!({
        "kind": "agent_setup",
        "ok": ok,
        "agent_id": ctx.agent_id,
        "provider": ctx.provider,
        "channel_id": ctx.channel_id,
        "created": report.created,
        "skipped": report.skipped,
        "rolled_back": report.rolled_back,
        "errors": report.errors,
        "planned": report.planned,
    });
    let rendered = serde_json::to_vec_pretty(&payload)
        .map_err(|error| format!("serialize audit log '{}': {error}", audit_path.display()))?;
    std::fs::write(&audit_path, rendered)
        .map_err(|error| format!("write audit log '{}': {error}", audit_path.display()))?;
    Ok(audit_path)
}

fn audit_path(ctx: &SetupContext) -> PathBuf {
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%S%9fZ");
    crate::runtime_layout::config_dir(&ctx.runtime_root)
        .join(".audit")
        .join(format!("agent-setup-{}-{now}.json", ctx.agent_id))
}

#[cfg(test)]
fn maybe_forced_failure(step: &str) -> Result<(), SetupError> {
    match std::env::var("AGENTDESK_TEST_AGENT_SETUP_FAIL_AFTER") {
        Ok(value) if value == step => Err(setup_error(
            step,
            format!("forced test failure after {step}"),
            "forced_test_failure",
        )),
        _ => Ok(()),
    }
}

#[cfg(not(test))]
fn maybe_forced_failure(_step: &str) -> Result<(), SetupError> {
    Ok(())
}
