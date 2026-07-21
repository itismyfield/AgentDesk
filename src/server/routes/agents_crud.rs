//! Agent CRUD handlers + system listing endpoints.
//! Extracted from mod.rs for #102.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::postgres::PgRow;
use sqlx::{Postgres, QueryBuilder, Row};
use std::path::{Path as FsPath, PathBuf};

use super::{AppState, agents_setup};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::git::{GitCommand, GitCommandError};
use crate::services::observability::session_inventory::derive_visual_status;
use crate::services::pipeline_override::{PipelineOverrideError, PipelineOverrideService};

// ── Query / Body structs ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct ListAgentsQuery {
    #[serde(rename = "officeId")]
    office_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateAgentBody {
    id: String,
    name: String,
    name_ko: Option<String>,
    provider: Option<String>,
    department: Option<String>,
    avatar_emoji: Option<String>,
    discord_channel_id: Option<String>,
    discord_channel_alt: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
    office_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(super) struct UpdateAgentBody {
    name: Option<String>,
    name_ko: Option<String>,
    provider: Option<String>,
    department: Option<String>,
    department_id: Option<String>,
    avatar_emoji: Option<String>,
    discord_channel_id: Option<String>,
    discord_channel_alt: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
    alias: Option<String>,
    cli_provider: Option<String>,
    sprite_number: Option<i64>,
    status: Option<String>,
    description: Option<String>,
    personality: Option<String>,
    system_prompt: Option<String>,
    #[serde(
        default,
        alias = "promptContent",
        alias = "prompt_md",
        alias = "promptMd"
    )]
    prompt_content: Option<String>,
    #[serde(default, alias = "autoCommit")]
    auto_commit: bool,
    #[serde(default, alias = "commitMessage")]
    commit_message: Option<String>,
    pipeline_config: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct DuplicateAgentBody {
    #[serde(alias = "newRoleId", alias = "new_agent_id")]
    new_agent_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    name_ko: Option<String>,
    #[serde(default)]
    department: Option<String>,
    #[serde(default, alias = "departmentId")]
    department_id: Option<String>,
    #[serde(default)]
    avatar_emoji: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default, alias = "channelId")]
    channel_id: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Clone, Debug, Default)]
struct AgentManagementFields {
    prompt_path: Option<String>,
    prompt_content: Option<String>,
}

fn normalize_channel_field(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn merged_channel_values(
    discord_channel_id: Option<String>,
    discord_channel_alt: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    // New columns (_cc, _cdx) are authoritative; legacy (_id, _alt) are mirrors.
    // Resolve new columns first (fallback from legacy if absent), then mirror back.
    let discord_channel_cc = normalize_channel_field(discord_channel_cc)
        .or_else(|| normalize_channel_field(discord_channel_id));
    let discord_channel_cdx = normalize_channel_field(discord_channel_cdx)
        .or_else(|| normalize_channel_field(discord_channel_alt));
    let discord_channel_id = discord_channel_cc.clone();
    let discord_channel_alt = discord_channel_cdx.clone();
    (
        discord_channel_id,
        discord_channel_alt,
        discord_channel_cc,
        discord_channel_cdx,
    )
}

fn parse_pipeline_config_json(raw: Option<String>) -> Option<serde_json::Value> {
    raw.and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok())
}

fn pipeline_override_error(error: PipelineOverrideError) -> AppError {
    match error {
        PipelineOverrideError::BadRequest(error) => {
            AppError::bad_request(format!("invalid pipeline_config: {error}"))
        }
        PipelineOverrideError::NotFound(error) => AppError::not_found(error),
        PipelineOverrideError::Database(error) => {
            AppError::internal(error).with_code(ErrorCode::Database)
        }
    }
}

fn visual_status_fields(row: &PgRow, agent_status: Option<&str>) -> (String, String, String) {
    let session_status = row
        .try_get::<Option<String>, _>("current_session_status")
        .ok()
        .flatten();
    let last_tool_at = row
        .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("current_last_tool_at")
        .ok()
        .flatten();
    let active_children = row
        .try_get::<Option<i32>, _>("current_active_children")
        .ok()
        .flatten()
        .unwrap_or(0);
    let status = session_status.as_deref().or(agent_status);
    let visual = derive_visual_status(status, last_tool_at, active_children, chrono::Utc::now());
    (
        visual.display(),
        visual.emoji().to_string(),
        visual.code().to_string(),
    )
}

fn clean_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn agent_channel_for_provider<'a>(
    agent: &'a crate::config::AgentDef,
    provider: Option<&str>,
) -> Option<&'a crate::config::AgentChannel> {
    let provider = provider
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(agent.provider.as_str());

    match provider {
        "claude" => agent.channels.claude.as_ref(),
        "codex" => agent.channels.codex.as_ref(),
        "gemini" => agent.channels.gemini.as_ref(),
        "opencode" => agent.channels.opencode.as_ref(),
        "qwen" => agent.channels.qwen.as_ref(),
        _ => None,
    }
    .or_else(|| agent.channels.claude.as_ref())
    .or_else(|| agent.channels.codex.as_ref())
    .or_else(|| agent.channels.gemini.as_ref())
    .or_else(|| agent.channels.opencode.as_ref())
    .or_else(|| agent.channels.qwen.as_ref())
}

fn resolve_configured_path(runtime_root: &FsPath, raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(expanded) = crate::runtime_layout::expand_user_path(trimmed) {
        return expanded;
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        path
    } else {
        crate::runtime_layout::config_dir(runtime_root).join(path)
    }
}

fn default_prompt_path(runtime_root: &FsPath, agent_id: &str) -> PathBuf {
    crate::runtime_layout::managed_agents_root(runtime_root)
        .join(agent_id)
        .join("IDENTITY.md")
}

fn git_command_error_detail(error: &GitCommandError) -> String {
    let stderr = error.stderr_text();
    if stderr.is_empty() {
        error.to_string()
    } else {
        stderr
    }
}

fn resolve_agent_prompt_path(agent_id: &str, provider: Option<&str>) -> Option<PathBuf> {
    let runtime_root = crate::config::runtime_root()?;
    let config = crate::services::discord::agentdesk_config::load_agent_setup_config(&runtime_root)
        .ok()
        .map(|(config, _, _)| config);

    if let Some(config) = config
        && let Some(agent) = config.agents.iter().find(|agent| agent.id == agent_id)
        && let Some(path) = agent_channel_for_provider(agent, provider)
            .and_then(crate::config::AgentChannel::prompt_file)
    {
        return Some(resolve_configured_path(&runtime_root, &path));
    }

    Some(default_prompt_path(&runtime_root, agent_id))
}

fn load_agent_management_fields(agent_id: &str, provider: Option<&str>) -> AgentManagementFields {
    let prompt_path = resolve_agent_prompt_path(agent_id, provider);
    let prompt_content = prompt_path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());

    AgentManagementFields {
        prompt_path: prompt_path.map(|path| path.display().to_string()),
        prompt_content,
    }
}

fn attach_management_fields(mut agent: Value, fields: AgentManagementFields) -> serde_json::Value {
    if let Some(object) = agent.as_object_mut() {
        object.insert(
            "prompt_path".to_string(),
            fields.prompt_path.map(Value::String).unwrap_or(Value::Null),
        );
        object.insert(
            "prompt_content".to_string(),
            fields
                .prompt_content
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
    }
    agent
}

#[allow(clippy::result_large_err)]
async fn run_prompt_auto_commit(
    prompt_path: &FsPath,
    message: Option<&str>,
) -> Result<Value, String> {
    let repo_dir = prompt_path
        .parent()
        .ok_or_else(|| format!("prompt path '{}' has no parent", prompt_path.display()))?;
    let message = message
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Update agent prompt from dashboard")
        .to_string();

    let add_repo = repo_dir.to_path_buf();
    let add_path = prompt_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        GitCommand::new()
            .repo(&add_repo)
            .arg("add")
            .arg(&add_path)
            .run_output()
    })
    .await
    .map_err(|error| format!("git add prompt join failed: {error}"))?
    .map_err(|error| {
        format!(
            "git add prompt failed: {}",
            git_command_error_detail(&error)
        )
    })?;

    let commit_repo = repo_dir.to_path_buf();
    let commit_message = message.clone();
    let commit = tokio::task::spawn_blocking(move || {
        GitCommand::new()
            .repo(&commit_repo)
            .arg("commit")
            .arg("-m")
            .arg(&commit_message)
            .run_output()
    })
    .await
    .map_err(|error| format!("git commit prompt join failed: {error}"))?
    .map_err(|error| {
        format!(
            "git commit prompt failed: {}",
            git_command_error_detail(&error)
        )
    })?;

    Ok(json!({
        "message": message,
        "stdout": String::from_utf8_lossy(&commit.stdout).trim(),
    }))
}

async fn write_prompt_if_changed(
    agent_id: &str,
    provider: Option<&str>,
    content: &str,
    auto_commit: bool,
    commit_message: Option<&str>,
) -> Result<Value, String> {
    let prompt_path = resolve_agent_prompt_path(agent_id, provider)
        .ok_or_else(|| "AGENTDESK runtime root could not be resolved".to_string())?;
    let existing = std::fs::read_to_string(&prompt_path).ok();
    if existing.as_deref() == Some(content) {
        return Ok(json!({
            "changed": false,
            "path": prompt_path.display().to_string(),
            "auto_commit": Value::Null,
        }));
    }
    if let Some(parent) = prompt_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create prompt dir '{}': {error}", parent.display()))?;
    }
    std::fs::write(&prompt_path, content)
        .map_err(|error| format!("write prompt '{}': {error}", prompt_path.display()))?;
    let auto_commit_result = if auto_commit {
        match run_prompt_auto_commit(&prompt_path, commit_message).await {
            Ok(value) => value,
            Err(error) => json!({ "error": error }),
        }
    } else {
        Value::Null
    };
    Ok(json!({
        "changed": true,
        "path": prompt_path.display().to_string(),
        "auto_commit": auto_commit_result,
    }))
}

async fn list_agents_pg(
    pool: &sqlx::PgPool,
    office_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let sql_with_office = "
        SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
               a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
               a.status, a.xp, a.sprite_number, d.name AS department_name, d.name_ko AS department_name_ko,
               d.color AS department_color, a.created_at::text AS created_at,
               (SELECT COUNT(DISTINCT kc.id)::BIGINT FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
               (SELECT COALESCE(SUM(s.tokens), 0)::BIGINT FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
               (SELECT td2.id
                  FROM task_dispatches td2
                  JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                 WHERE td2.to_agent_id = a.id
                   AND kc.status = 'in_progress'
                 ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                 LIMIT 1) AS current_task,
               (SELECT s.thread_channel_id
                  FROM sessions s
                 WHERE s.agent_id = a.id
                   AND s.status IN ('turn_active', 'awaiting_bg', 'working')
                 ORDER BY s.last_heartbeat DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_thread_channel_id,
               (SELECT s.status
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_session_status,
               (SELECT s.last_tool_at
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_last_tool_at,
               (SELECT COALESCE(s.active_children, 0)
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_active_children,
               a.pipeline_config::text AS pipeline_config
          FROM agents a
          INNER JOIN office_agents oa ON oa.agent_id = a.id
          LEFT JOIN departments d ON d.id = a.department
         WHERE oa.office_id = $1
         ORDER BY a.id";
    let sql_all = "
        SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
               a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
               a.status, a.xp, a.sprite_number, d.name AS department_name, d.name_ko AS department_name_ko,
               d.color AS department_color, a.created_at::text AS created_at,
               (SELECT COUNT(DISTINCT kc.id)::BIGINT FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
               (SELECT COALESCE(SUM(s.tokens), 0)::BIGINT FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
               (SELECT td2.id
                  FROM task_dispatches td2
                  JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                 WHERE td2.to_agent_id = a.id
                   AND kc.status = 'in_progress'
                 ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                 LIMIT 1) AS current_task,
               (SELECT s.thread_channel_id
                  FROM sessions s
                 WHERE s.agent_id = a.id
                   AND s.status IN ('turn_active', 'awaiting_bg', 'working')
                 ORDER BY s.last_heartbeat DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_thread_channel_id,
               (SELECT s.status
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_session_status,
               (SELECT s.last_tool_at
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_last_tool_at,
               (SELECT COALESCE(s.active_children, 0)
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_active_children,
               a.pipeline_config::text AS pipeline_config
          FROM agents a
          LEFT JOIN departments d ON d.id = a.department
         ORDER BY a.id";

    let rows = match office_id {
        Some(office_id) => {
            sqlx::query(sql_with_office)
                .bind(office_id)
                .fetch_all(pool)
                .await
        }
        None => sqlx::query(sql_all).fetch_all(pool).await,
    }
    .map_err(|error| format!("query agents: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let provider = row.try_get::<Option<String>, _>("provider").ok().flatten();
            let status = row.try_get::<Option<String>, _>("status").ok().flatten();
            let (visual_status, visual_status_emoji, visual_status_code) =
                visual_status_fields(&row, status.as_deref());
            let discord_channel_alt = row
                .try_get::<Option<String>, _>("discord_channel_alt")
                .ok()
                .flatten();
            let discord_channel_cdx = row
                .try_get::<Option<String>, _>("discord_channel_cdx")
                .ok()
                .flatten();
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "name": row.try_get::<String, _>("name").unwrap_or_default(),
                "name_ko": row.try_get::<Option<String>, _>("name_ko").ok().flatten(),
                "provider": provider.clone(),
                "cli_provider": provider,
                "department": row.try_get::<Option<String>, _>("department").ok().flatten(),
                "department_id": row.try_get::<Option<String>, _>("department").ok().flatten(),
                "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").ok().flatten(),
                "discord_channel_id": row.try_get::<Option<String>, _>("discord_channel_id").ok().flatten(),
                "discord_channel_alt": discord_channel_alt,
                "discord_channel_cc": row.try_get::<Option<String>, _>("discord_channel_cc").ok().flatten(),
                "discord_channel_cdx": discord_channel_cdx.clone(),
                "discord_channel_id_codex": discord_channel_cdx,
                "status": status,
                "visual_status": visual_status,
                "visual_status_emoji": visual_status_emoji,
                "visual_status_code": visual_status_code,
                "xp": row.try_get::<Option<i64>, _>("xp").ok().flatten().unwrap_or(0),
                "stats_xp": row.try_get::<Option<i64>, _>("xp").ok().flatten().unwrap_or(0),
                "stats_tasks_done": row.try_get::<Option<i64>, _>("tasks_done").ok().flatten().unwrap_or(0),
                "stats_tokens": row.try_get::<Option<i64>, _>("total_tokens").ok().flatten().unwrap_or(0),
                "sprite_number": row.try_get::<Option<i64>, _>("sprite_number").ok().flatten(),
                "department_name": row.try_get::<Option<String>, _>("department_name").ok().flatten(),
                "department_name_ko": row.try_get::<Option<String>, _>("department_name_ko").ok().flatten(),
                "department_color": row.try_get::<Option<String>, _>("department_color").ok().flatten(),
                "created_at": row.try_get::<Option<String>, _>("created_at").ok().flatten(),
                "alias": serde_json::Value::Null,
                "role_id": row.try_get::<Option<String>, _>("id").ok().flatten(),
                "personality": serde_json::Value::Null,
                "current_task_id": row.try_get::<Option<String>, _>("current_task").ok().flatten(),
                "current_thread_channel_id": row.try_get::<Option<String>, _>("current_thread_channel_id").ok().flatten(),
                "pipeline_config": parse_pipeline_config_json(
                    row.try_get::<Option<String>, _>("pipeline_config").ok().flatten()
                ),
            })
        })
        .collect())
}

async fn load_agent_pg(pool: &sqlx::PgPool, id: &str) -> Result<Option<serde_json::Value>, String> {
    let rows = sqlx::query(
        "
        SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
               a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
               a.status, a.xp, a.sprite_number, d.name AS department_name, d.name_ko AS department_name_ko,
               d.color AS department_color, a.created_at::text AS created_at,
               (SELECT COUNT(DISTINCT kc.id)::BIGINT FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
               (SELECT COALESCE(SUM(s.tokens), 0)::BIGINT FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
               (SELECT td2.id
                  FROM task_dispatches td2
                  JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                 WHERE td2.to_agent_id = a.id
                   AND kc.status = 'in_progress'
                 ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                 LIMIT 1) AS current_task,
               (SELECT s.thread_channel_id
                  FROM sessions s
                 WHERE s.agent_id = a.id
                   AND s.status IN ('turn_active', 'awaiting_bg', 'working')
                 ORDER BY s.last_heartbeat DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_thread_channel_id,
               (SELECT s.status
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_session_status,
               (SELECT s.last_tool_at
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_last_tool_at,
               (SELECT COALESCE(s.active_children, 0)
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_active_children,
               a.pipeline_config::text AS pipeline_config
          FROM agents a
          LEFT JOIN departments d ON d.id = a.department
         WHERE a.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load agent {id}: {error}"))?;

    let Some(row) = rows else {
        return Ok(None);
    };

    let provider = row.try_get::<Option<String>, _>("provider").ok().flatten();
    let status = row.try_get::<Option<String>, _>("status").ok().flatten();
    let (visual_status, visual_status_emoji, visual_status_code) =
        visual_status_fields(&row, status.as_deref());
    let discord_channel_alt = row
        .try_get::<Option<String>, _>("discord_channel_alt")
        .ok()
        .flatten();
    let discord_channel_cdx = row
        .try_get::<Option<String>, _>("discord_channel_cdx")
        .ok()
        .flatten();
    let fields = load_agent_management_fields(&id, provider.as_deref());

    Ok(Some(attach_management_fields(
        json!({
            "id": row.try_get::<String, _>("id").unwrap_or_default(),
            "name": row.try_get::<String, _>("name").unwrap_or_default(),
            "name_ko": row.try_get::<Option<String>, _>("name_ko").ok().flatten(),
            "provider": provider.clone(),
            "cli_provider": provider,
            "department": row.try_get::<Option<String>, _>("department").ok().flatten(),
            "department_id": row.try_get::<Option<String>, _>("department").ok().flatten(),
            "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").ok().flatten(),
            "discord_channel_id": row.try_get::<Option<String>, _>("discord_channel_id").ok().flatten(),
            "discord_channel_alt": discord_channel_alt,
            "discord_channel_cc": row.try_get::<Option<String>, _>("discord_channel_cc").ok().flatten(),
            "discord_channel_cdx": discord_channel_cdx.clone(),
            "discord_channel_id_codex": discord_channel_cdx,
            "status": status,
            "visual_status": visual_status,
            "visual_status_emoji": visual_status_emoji,
            "visual_status_code": visual_status_code,
            "xp": row.try_get::<Option<i64>, _>("xp").ok().flatten().unwrap_or(0),
            "stats_xp": row.try_get::<Option<i64>, _>("xp").ok().flatten().unwrap_or(0),
            "stats_tasks_done": row.try_get::<Option<i64>, _>("tasks_done").ok().flatten().unwrap_or(0),
            "stats_tokens": row.try_get::<Option<i64>, _>("total_tokens").ok().flatten().unwrap_or(0),
            "sprite_number": row.try_get::<Option<i64>, _>("sprite_number").ok().flatten(),
            "department_name": row.try_get::<Option<String>, _>("department_name").ok().flatten(),
            "department_name_ko": row.try_get::<Option<String>, _>("department_name_ko").ok().flatten(),
            "department_color": row.try_get::<Option<String>, _>("department_color").ok().flatten(),
            "created_at": row.try_get::<Option<String>, _>("created_at").ok().flatten(),
            "alias": serde_json::Value::Null,
            "role_id": row.try_get::<Option<String>, _>("id").ok().flatten(),
            "personality": serde_json::Value::Null,
            "current_task_id": row.try_get::<Option<String>, _>("current_task").ok().flatten(),
            "current_thread_channel_id": row.try_get::<Option<String>, _>("current_thread_channel_id").ok().flatten(),
            "pipeline_config": parse_pipeline_config_json(
                row.try_get::<Option<String>, _>("pipeline_config").ok().flatten()
            ),
        }),
        fields,
    )))
}

// ── Handlers ─────────────────────────────────────────────────────

pub(super) async fn list_agents(
    State(state): State<AppState>,
    Query(params): Query<ListAgentsQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        let agents = list_agents_pg(pool, params.office_id.as_deref())
            .await
            .unwrap_or_default();
        return Ok((StatusCode::OK, Json(json!({ "agents": agents }))));
    }

    Err(AppError::new(
        StatusCode::OK,
        ErrorCode::Database,
        "postgres pool unavailable",
    ))
}

pub(super) async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        return match load_agent_pg(pool, &id).await {
            Ok(Some(agent)) => Ok((StatusCode::OK, Json(json!({ "agent": agent })))),
            Ok(None) => Err(AppError::new(
                StatusCode::OK,
                ErrorCode::NotFound,
                "agent not found",
            )),
            Err(error) => Err(AppError::new(StatusCode::OK, ErrorCode::Database, error)),
        };
    }

    Err(AppError::new(
        StatusCode::OK,
        ErrorCode::Database,
        "postgres pool unavailable",
    ))
}

pub(super) async fn create_agent(
    State(state): State<AppState>,
    Json(body): Json<CreateAgentBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        let (discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx) =
            merged_channel_values(
                body.discord_channel_id.clone(),
                body.discord_channel_alt.clone(),
                body.discord_channel_cc.clone(),
                body.discord_channel_cdx.clone(),
            );

        if let Err(error) = sqlx::query(
            "INSERT INTO agents (
                id, name, name_ko, provider, department, avatar_emoji,
                discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(&body.id)
        .bind(&body.name)
        .bind(&body.name_ko)
        .bind(&body.provider)
        .bind(&body.department)
        .bind(&body.avatar_emoji)
        .bind(&discord_channel_id)
        .bind(&discord_channel_alt)
        .bind(&discord_channel_cc)
        .bind(&discord_channel_cdx)
        .execute(pool)
        .await
        {
            return Err(AppError::internal(format!("{error}")).with_code(ErrorCode::Database));
        }

        if let Some(ref office_id) = body.office_id {
            if let Err(error) = sqlx::query(
                "INSERT INTO office_agents (office_id, agent_id)
                 VALUES ($1, $2)
                 ON CONFLICT (office_id, agent_id) DO NOTHING",
            )
            .bind(office_id)
            .bind(&body.id)
            .execute(pool)
            .await
            {
                return Err(AppError::internal(format!("{error}")).with_code(ErrorCode::Database));
            }
        }

        return match load_agent_pg(pool, &body.id).await {
            Ok(Some(agent)) => {
                // #2050 P1 finding 1 — broadcast agent_created so other dashboards
                // refresh their agent rosters without a manual reload.
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "agent_created",
                    json!({ "id": body.id, "agent": agent }),
                );
                Ok((StatusCode::CREATED, Json(json!({"agent": agent}))))
            }
            Ok(None) => Err(
                AppError::internal("agent insert succeeded but readback failed")
                    .with_code(ErrorCode::Database),
            ),
            Err(error) => Err(AppError::internal(error).with_code(ErrorCode::Database)),
        };
    }

    Err(AppError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Database,
        "postgres pool unavailable",
    ))
}

#[allow(clippy::type_complexity)]
pub(super) async fn update_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAgentBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        let mut updated_any = false;
        let channel_patch_requested = body.discord_channel_id.is_some()
            || body.discord_channel_alt.is_some()
            || body.discord_channel_cc.is_some()
            || body.discord_channel_cdx.is_some();

        let existing_channels: Option<(
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = if channel_patch_requested {
            match sqlx::query(
                "SELECT discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
                 FROM agents
                 WHERE id = $1",
            )
            .bind(&id)
            .fetch_optional(pool)
            .await
            {
                Ok(Some(row)) => Some((
                    row.try_get("discord_channel_id").ok().flatten(),
                    row.try_get("discord_channel_alt").ok().flatten(),
                    row.try_get("discord_channel_cc").ok().flatten(),
                    row.try_get("discord_channel_cdx").ok().flatten(),
                )),
                Ok(None) => return Err(AppError::not_found("agent not found")),
                Err(error) => {
                    return Err(AppError::internal(format!("{error}")).with_code(ErrorCode::Database));
                }
            }
        } else {
            None
        };

        let mut builder = QueryBuilder::<Postgres>::new("UPDATE agents SET ");
        let mut separated = builder.separated(", ");

        if let Some(ref name) = body.name {
            updated_any = true;
            separated.push("name = ").push_bind_unseparated(name);
        }
        if let Some(ref name_ko) = body.name_ko {
            updated_any = true;
            separated.push("name_ko = ").push_bind_unseparated(name_ko);
        }
        if let Some(ref provider) = body.provider {
            updated_any = true;
            separated
                .push("provider = ")
                .push_bind_unseparated(provider);
        }
        if body.provider.is_none()
            && let Some(ref provider) = body.cli_provider
        {
            updated_any = true;
            separated
                .push("provider = ")
                .push_bind_unseparated(provider);
        }
        let dept_value = body.department_id.as_ref().or(body.department.as_ref());
        if let Some(department) = dept_value {
            updated_any = true;
            separated
                .push("department = ")
                .push_bind_unseparated(department);
        }
        if let Some(ref avatar_emoji) = body.avatar_emoji {
            updated_any = true;
            separated
                .push("avatar_emoji = ")
                .push_bind_unseparated(avatar_emoji);
        }
        if let Some(sprite_number) = body.sprite_number {
            updated_any = true;
            separated
                .push("sprite_number = ")
                .push_bind_unseparated(sprite_number);
        }
        if let Some(ref status) = body.status {
            updated_any = true;
            separated.push("status = ").push_bind_unseparated(status);
        }
        if let Some(ref description) = body.description {
            updated_any = true;
            separated
                .push("description = ")
                .push_bind_unseparated(description);
        }
        let system_prompt = body.system_prompt.as_ref().or(body.personality.as_ref());
        if let Some(system_prompt) = system_prompt {
            updated_any = true;
            separated
                .push("system_prompt = ")
                .push_bind_unseparated(system_prompt);
        }
        if channel_patch_requested {
            let existing_channels = existing_channels.expect("existing channel lookup");
            let (discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx) =
                merged_channel_values(
                    body.discord_channel_id.clone().or(existing_channels.0),
                    body.discord_channel_alt.clone().or(existing_channels.1),
                    body.discord_channel_cc.clone().or(existing_channels.2),
                    body.discord_channel_cdx.clone().or(existing_channels.3),
                );
            updated_any = true;
            separated
                .push("discord_channel_id = ")
                .push_bind_unseparated(discord_channel_id);
            separated
                .push("discord_channel_alt = ")
                .push_bind_unseparated(discord_channel_alt);
            separated
                .push("discord_channel_cc = ")
                .push_bind_unseparated(discord_channel_cc);
            separated
                .push("discord_channel_cdx = ")
                .push_bind_unseparated(discord_channel_cdx);
        }
        if let Some(ref pipeline_config) = body.pipeline_config {
            updated_any = true;
            if pipeline_config.is_null() {
                let service = PipelineOverrideService::new(pool);
                if let Err(error) = service.validate_agent_pipeline_config(&id, None).await {
                    return Err(pipeline_override_error(error));
                }
                separated.push("pipeline_config = NULL");
            } else {
                let pipeline_text = pipeline_config.to_string();
                let service = PipelineOverrideService::new(pool);
                if let Err(error) = service
                    .validate_agent_pipeline_config(&id, Some(pipeline_config))
                    .await
                {
                    return Err(pipeline_override_error(error));
                }
                separated
                    .push("pipeline_config = ")
                    .push_bind_unseparated(pipeline_text)
                    .push_unseparated("::jsonb");
            }
        }

        let prompt_result = if let Some(ref prompt_content) = body.prompt_content {
            let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agents WHERE id = $1")
                .bind(&id)
                .fetch_one(pool)
                .await
                .map_err(|error| format!("{error}"));
            match exists {
                Ok(0) => return Err(AppError::not_found("agent not found")),
                Ok(_) => match write_prompt_if_changed(
                    &id,
                    body.provider.as_deref().or(body.cli_provider.as_deref()),
                    prompt_content,
                    body.auto_commit,
                    body.commit_message.as_deref(),
                )
                .await
                {
                    Ok(result) => Some(result),
                    Err(error) => {
                        return Err(AppError::internal(error).with_code(ErrorCode::Config));
                    }
                },
                Err(error) => {
                    return Err(AppError::internal(error).with_code(ErrorCode::Database));
                }
            }
        } else {
            None
        };

        if !updated_any && prompt_result.is_none() {
            return Err(AppError::bad_request("no fields to update"));
        }

        if updated_any {
            separated.push("updated_at = NOW()");
            builder.push(" WHERE id = ").push_bind(&id);

            match builder.build().execute(pool).await {
                Ok(result) if result.rows_affected() == 0 => {
                    return Err(AppError::not_found("agent not found"));
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(
                        AppError::internal(format!("{error}")).with_code(ErrorCode::Database)
                    );
                }
            }
        }

        return match load_agent_pg(pool, &id).await {
            Ok(Some(agent)) => Ok((
                StatusCode::OK,
                Json(json!({"agent": agent, "prompt": prompt_result})),
            )),
            Ok(None) => Err(
                AppError::internal("agent update succeeded but readback failed")
                    .with_code(ErrorCode::Database),
            ),
            Err(error) => Err(AppError::internal(error).with_code(ErrorCode::Database)),
        };
    }

    Err(AppError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Database,
        "postgres pool unavailable",
    ))
}

async fn load_duplicate_source_pg(
    pool: &sqlx::PgPool,
    source_id: &str,
) -> Result<Option<Value>, String> {
    load_agent_pg(pool, source_id).await
}

fn update_duplicate_config_metadata(
    new_agent_id: &str,
    name: Option<&str>,
    name_ko: Option<&str>,
    department: Option<&str>,
    avatar_emoji: Option<&str>,
) -> Result<(), String> {
    let Some(runtime_root) = crate::config::runtime_root() else {
        return Ok(());
    };
    let (mut config, path, _) =
        crate::services::discord::agentdesk_config::load_agent_setup_config(&runtime_root)?;
    let Some(agent) = config
        .agents
        .iter_mut()
        .find(|agent| agent.id == new_agent_id)
    else {
        return Ok(());
    };
    if let Some(name) = name {
        agent.name = name.to_string();
    }
    agent.name_ko = name_ko.map(str::to_string);
    agent.department = department.map(str::to_string);
    agent.avatar_emoji = avatar_emoji.map(str::to_string);
    crate::config::save_to_path(&path, &config)
        .map_err(|error| format!("write config '{}': {error}", path.display()))
}

pub(super) async fn duplicate_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DuplicateAgentBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let new_agent_id = body.new_agent_id.trim().to_string();
    let Some(channel_id) = clean_optional_text(body.channel_id.clone()) else {
        return Err(AppError::bad_request("channel_id is required"));
    };

    let source = if let Some(pool) = state.pg_pool_ref() {
        match load_duplicate_source_pg(pool, &id).await {
            Ok(Some(agent)) => agent,
            Ok(None) => return Err(AppError::not_found("agent not found")),
            Err(error) => return Err(AppError::internal(error).with_code(ErrorCode::Database)),
        }
    } else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };

    let provider = clean_optional_text(body.provider.clone())
        .or_else(|| {
            source
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "claude".to_string());
    let prompt_path = source
        .get("prompt_path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            resolve_agent_prompt_path(&id, Some(&provider)).map(|path| path.display().to_string())
        });
    let Some(prompt_template_path) = prompt_path else {
        return Err(
            AppError::internal("source prompt path could not be resolved")
                .with_code(ErrorCode::Config),
        );
    };

    let setup_body = agents_setup::AgentSetupBody {
        agent_id: new_agent_id.clone(),
        channel_id,
        provider: provider.clone(),
        prompt_template_path,
        skills: body.skills.clone(),
        dry_run: body.dry_run,
    };
    let (setup_status, Json(setup_json)) =
        agents_setup::setup_agent(State(state.clone()), Json(setup_body)).await;
    if body.dry_run || !setup_status.is_success() {
        return Ok((
            setup_status,
            Json(json!({
                "ok": setup_status.is_success(),
                "duplicate": true,
                "source_agent_id": id,
                "new_agent_id": new_agent_id,
                "setup": setup_json,
            })),
        ));
    }

    let name = clean_optional_text(body.name.clone())
        .or_else(|| {
            source
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| new_agent_id.clone());
    let name_ko = clean_optional_text(body.name_ko.clone()).or_else(|| {
        source
            .get("name_ko")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let department = clean_optional_text(body.department_id.clone())
        .or_else(|| clean_optional_text(body.department.clone()))
        .or_else(|| {
            source
                .get("department_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let avatar_emoji = clean_optional_text(body.avatar_emoji.clone()).or_else(|| {
        source
            .get("avatar_emoji")
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };
    if let Err(error) = sqlx::query(
        "UPDATE agents
            SET name = $2, name_ko = $3, department = $4, avatar_emoji = $5, updated_at = NOW()
          WHERE id = $1",
    )
    .bind(&new_agent_id)
    .bind(&name)
    .bind(&name_ko)
    .bind(&department)
    .bind(&avatar_emoji)
    .execute(pool)
    .await
    {
        return Err(
            AppError::internal(format!("update duplicate metadata: {error}"))
                .with_code(ErrorCode::Database),
        );
    }

    if let Err(error) = update_duplicate_config_metadata(
        &new_agent_id,
        Some(&name),
        name_ko.as_deref(),
        department.as_deref(),
        avatar_emoji.as_deref(),
    ) {
        return Err(AppError::internal(error).with_code(ErrorCode::Config));
    }

    let agent = load_agent_pg(pool, &new_agent_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({"id": new_agent_id}));

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "duplicate": true,
            "source_agent_id": id,
            "new_agent_id": new_agent_id,
            "setup": setup_json,
            "agent": agent,
        })),
    ))
}

pub(super) async fn delete_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query("DELETE FROM agents WHERE id = $1")
            .bind(&id)
            .execute(pool)
            .await
        {
            Ok(result) if result.rows_affected() == 0 => {
                return Err(AppError::not_found("agent not found"));
            }
            Ok(_) => {
                let _ = sqlx::query("DELETE FROM office_agents WHERE agent_id = $1")
                    .bind(&id)
                    .execute(pool)
                    .await;
                // #2050 P1 finding 1 — broadcast agent_deleted to other dashboards.
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "agent_deleted",
                    json!({ "id": id }),
                );
                return Ok((StatusCode::OK, Json(json!({"ok": true}))));
            }
            Err(error) => {
                return Err(AppError::internal(format!("{error}")).with_code(ErrorCode::Database));
            }
        }
    }

    Err(AppError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Database,
        "postgres pool unavailable",
    ))
}

pub(super) async fn list_sessions(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::OK,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };
    let rows = match sqlx::query(
        "SELECT id, session_key, instance_id, agent_id, provider, status, active_dispatch_id,
                model, tokens, cwd, to_char(last_heartbeat, 'YYYY-MM-DD HH24:MI:SS') AS last_heartbeat
         FROM sessions
         WHERE status IN ('connected', 'turn_active', 'awaiting_bg', 'awaiting_user', 'idle', 'working')
         ORDER BY id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return Err(AppError::new(
                StatusCode::OK,
                ErrorCode::Database,
                format!("query failed: {error}"),
            ));
        }
    };
    let worker_nodes = match crate::server::cluster::list_worker_nodes(
        pool,
        state.config.cluster.lease_ttl_secs.max(1),
    )
    .await
    {
        Ok(nodes) => nodes,
        Err(error) => {
            tracing::warn!("failed to list worker nodes for session owner routing: {error}");
            Vec::new()
        }
    };
    let local_instance_id = state.cluster_instance_id.as_deref();
    let mut sessions: Vec<_> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.try_get::<i64, _>("id").unwrap_or(0),
                "session_key": row.try_get::<Option<String>, _>("session_key").ok().flatten(),
                "instance_id": row.try_get::<Option<String>, _>("instance_id").ok().flatten(),
                "agent_id": row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
                "provider": row.try_get::<Option<String>, _>("provider").ok().flatten(),
                "status": row.try_get::<Option<String>, _>("status").ok().flatten(),
                "active_dispatch_id": row.try_get::<Option<String>, _>("active_dispatch_id").ok().flatten(),
                "model": row.try_get::<Option<String>, _>("model").ok().flatten(),
                "tokens": row.try_get::<i64, _>("tokens").unwrap_or(0),
                "cwd": row.try_get::<Option<String>, _>("cwd").ok().flatten(),
                "last_heartbeat": row.try_get::<Option<String>, _>("last_heartbeat").ok().flatten(),
            })
        })
        .collect();
    crate::server::cluster_session_routing::enrich_session_owner_routing(
        &mut sessions,
        local_instance_id,
        &worker_nodes,
    );

    Ok((StatusCode::OK, Json(json!({ "sessions": sessions }))))
}

pub(super) async fn list_policies(State(state): State<AppState>) -> Json<serde_json::Value> {
    let policies = state.engine.list_policies();
    let items: Vec<serde_json::Value> = policies
        .into_iter()
        .map(|p| {
            json!({
                "name": p.name,
                "file": p.file,
                "priority": p.priority,
                "hooks": p.hooks,
            })
        })
        .collect();
    Json(json!({ "policies": items }))
}
