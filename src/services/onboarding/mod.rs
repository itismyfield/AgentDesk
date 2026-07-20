//! Onboarding service.
//!
//! Historically a single 5000+ LOC file, this module is in the process of
//! being split into focused sub-modules. Today the bulk of the logic
//! (`status`, `draft_*`, `complete`, conflict detection, channel mapping,
//! persistence, tests) still lives here, with self-contained handlers
//! extracted to siblings.
//!
//! All public exports are preserved at the `crate::services::onboarding`
//! module path so existing callers (e.g. `src/server/routes/onboarding.rs`)
//! keep working without import changes.

mod channel;
mod provider;

pub use channel::{
    ChannelsBody, ChannelsQuery, ValidateTokenBody, channels, channels_post, validate_token,
};
pub use provider::{CheckProviderBody, GeneratePromptBody, check_provider, generate_prompt};

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::app_state::AppState;
use crate::error::{AppError, AppResult};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const ONBOARDING_DRAFT_VERSION: u8 = 1;
const MAX_ONBOARDING_DRAFT_BYTES: usize = 128 * 1024;
const MAX_ONBOARDING_DRAFT_COMMAND_BOTS: usize = 4;
const MAX_ONBOARDING_DRAFT_AGENTS: usize = 64;
const MAX_ONBOARDING_DRAFT_CHANNEL_ASSIGNMENTS: usize = 64;
const MAX_ONBOARDING_DRAFT_PROVIDER_STATUSES: usize = 8;
const MAX_ONBOARDING_DRAFT_FUTURE_SKEW_MS: i64 = 5 * 60 * 1000;
const ONBOARDING_NEW_AGENT_STATUS: &str = "idle";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
struct OnboardingDraftBotInfo {
    valid: bool,
    bot_id: Option<String>,
    bot_name: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct OnboardingDraftCommandBot {
    provider: String,
    token: String,
    bot_info: Option<OnboardingDraftBotInfo>,
}

impl Default for OnboardingDraftCommandBot {
    fn default() -> Self {
        Self {
            provider: "claude".to_string(),
            token: String::new(),
            bot_info: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
struct OnboardingDraftProviderStatus {
    installed: bool,
    logged_in: bool,
    version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
struct OnboardingDraftAgent {
    id: String,
    name: String,
    name_en: Option<String>,
    description: String,
    description_en: Option<String>,
    prompt: String,
    custom: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
struct OnboardingDraftChannelAssignment {
    agent_id: String,
    agent_name: String,
    recommended_name: String,
    channel_id: String,
    channel_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct OnboardingDraft {
    version: u8,
    updated_at_ms: i64,
    step: u8,
    command_bots: Vec<OnboardingDraftCommandBot>,
    announce_token: String,
    notify_token: String,
    announce_bot_info: Option<OnboardingDraftBotInfo>,
    notify_bot_info: Option<OnboardingDraftBotInfo>,
    provider_statuses: BTreeMap<String, OnboardingDraftProviderStatus>,
    selected_template: Option<String>,
    agents: Vec<OnboardingDraftAgent>,
    custom_name: String,
    custom_desc: String,
    custom_name_en: String,
    custom_desc_en: String,
    expanded_agent: Option<String>,
    selected_guild: String,
    channel_assignments: Vec<OnboardingDraftChannelAssignment>,
    owner_id: String,
    has_existing_setup: bool,
    confirm_rerun_overwrite: bool,
}

impl OnboardingDraft {
    fn normalize(mut self) -> Result<Self, String> {
        if self.version != ONBOARDING_DRAFT_VERSION {
            return Err(format!(
                "unsupported onboarding draft version '{}'",
                self.version
            ));
        }
        self.step = self.step.clamp(1, 5);
        let now = now_unix_ms();
        self.updated_at_ms = if self.updated_at_ms > 0 {
            self.updated_at_ms
                .min(now.saturating_add(MAX_ONBOARDING_DRAFT_FUTURE_SKEW_MS))
        } else {
            now
        };
        if self.command_bots.is_empty() {
            self.command_bots.push(OnboardingDraftCommandBot::default());
        }
        if self.command_bots.len() > MAX_ONBOARDING_DRAFT_COMMAND_BOTS {
            return Err(format!(
                "onboarding draft exceeds max command bot entries ({MAX_ONBOARDING_DRAFT_COMMAND_BOTS})"
            ));
        }
        if self.agents.len() > MAX_ONBOARDING_DRAFT_AGENTS {
            return Err(format!(
                "onboarding draft exceeds max agents ({MAX_ONBOARDING_DRAFT_AGENTS})"
            ));
        }
        if self.channel_assignments.len() > MAX_ONBOARDING_DRAFT_CHANNEL_ASSIGNMENTS {
            return Err(format!(
                "onboarding draft exceeds max channel assignments ({MAX_ONBOARDING_DRAFT_CHANNEL_ASSIGNMENTS})"
            ));
        }
        if self.provider_statuses.len() > MAX_ONBOARDING_DRAFT_PROVIDER_STATUSES {
            return Err(format!(
                "onboarding draft exceeds max provider statuses ({MAX_ONBOARDING_DRAFT_PROVIDER_STATUSES})"
            ));
        }
        self.owner_id = self.owner_id.trim().to_string();
        parse_owner_id(Some(self.owner_id.as_str()))?;
        let payload_size = serde_json::to_vec(&self)
            .map_err(|error| {
                format!("failed to serialize onboarding draft for validation: {error}")
            })?
            .len();
        if payload_size > MAX_ONBOARDING_DRAFT_BYTES {
            return Err(format!(
                "onboarding draft exceeds max payload size ({} bytes)",
                MAX_ONBOARDING_DRAFT_BYTES
            ));
        }
        Ok(self)
    }

    fn redact_secrets(mut self) -> Self {
        for bot in &mut self.command_bots {
            bot.token.clear();
        }
        self.announce_token.clear();
        self.notify_token.clear();
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OnboardingSetupMode {
    Fresh,
    Rerun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OnboardingResumeState {
    None,
    DraftAvailable,
    PartialApply,
}

fn onboarding_setup_mode(completed: bool) -> OnboardingSetupMode {
    if completed {
        OnboardingSetupMode::Rerun
    } else {
        OnboardingSetupMode::Fresh
    }
}

fn onboarding_resume_state(
    draft_available: bool,
    completion_state: Option<&OnboardingCompletionState>,
) -> OnboardingResumeState {
    if completion_state
        .map(|state| state.partial_apply)
        .unwrap_or(false)
    {
        OnboardingResumeState::PartialApply
    } else if draft_available {
        OnboardingResumeState::DraftAvailable
    } else {
        OnboardingResumeState::None
    }
}

fn sanitize_legacy_owner_id(owner_id: Option<String>) -> Option<String> {
    let value = owner_id?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    parse_owner_id(Some(trimmed)).ok().flatten()?;
    Some(trimmed.to_string())
}

fn sanitize_draft_owner_id(owner_id: &str) -> String {
    let trimmed = owner_id.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if parse_owner_id(Some(trimmed)).ok().flatten().is_some() {
        trimmed.to_string()
    } else {
        String::new()
    }
}

fn onboarding_draft_secret_policy_value() -> serde_json::Value {
    json!({
        "stores_raw_tokens": false,
        "returns_raw_tokens_in_draft": false,
        "masked_in_status_after_completion": true,
        "cleared_on_complete": true,
        "cleared_on_delete": true,
    })
}

/// GET /api/onboarding/status
/// Returns whether onboarding is complete + existing config values.
pub async fn status(state: &AppState) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Some(pool) = state.pg_pool_ref() {
        return status_pg(pool)
            .await
            .map(|value| (StatusCode::OK, Json(value)))
            .map_err(AppError::internal);
    }

    status_config()
        .map(|value| (StatusCode::OK, Json(value)))
        .map_err(AppError::internal)
}

pub(super) async fn pg_kv_value(pool: &sqlx::PgPool, key: &str) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres kv_meta {key}: {error}"))
}

async fn onboarding_has_agents_pg(pool: &sqlx::PgPool) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>("SELECT COUNT(*) > 0 FROM agents")
        .fetch_one(pool)
        .await
        .map_err(|error| format!("check postgres onboarding agents: {error}"))
}

async fn status_pg(pool: &sqlx::PgPool) -> Result<serde_json::Value, String> {
    let has_bots = onboarding_has_agents_pg(pool).await?;
    let bot_token = pg_kv_value(pool, "onboarding_bot_token").await?;
    let guild_id = pg_kv_value(pool, "onboarding_guild_id").await?;
    let owner_id = sanitize_legacy_owner_id(pg_kv_value(pool, "onboarding_owner_id").await?);
    let agent_count: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM agents")
        .fetch_one(pool)
        .await
        .map_err(|error| format!("count postgres onboarding agents: {error}"))?;
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT id, name, discord_channel_id FROM agents ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres onboarding agents: {error}"))?;
    let agents = rows
        .into_iter()
        .map(|(agent_id, name, channel_id)| {
            json!({
                "agent_id": agent_id,
                "name": name,
                "channel_id": channel_id,
            })
        })
        .collect::<Vec<_>>();

    let announce_token = pg_kv_value(pool, "onboarding_announce_token").await?;
    let notify_token = pg_kv_value(pool, "onboarding_notify_token").await?;
    let command_token_2 = pg_kv_value(pool, "onboarding_command_token_2").await?;
    let primary_provider = pg_kv_value(pool, "onboarding_provider").await?;
    let command_provider_2 = pg_kv_value(pool, "onboarding_command_provider_2").await?;

    let completed = has_bots && agent_count > 0;
    let runtime_root = crate::cli::agentdesk_runtime_root();
    let completion_state = runtime_root
        .as_ref()
        .and_then(|root| load_onboarding_completion_state(root).ok().flatten());
    let draft_available = runtime_root
        .as_ref()
        .map(|root| onboarding_draft_path(root).is_file())
        .unwrap_or(false);
    let setup_mode = onboarding_setup_mode(completed);
    let resume_state = onboarding_resume_state(draft_available, completion_state.as_ref());
    let redact = |_t: Option<String>| -> Option<String> { None };

    Ok(json!({
        "completed": completed,
        "agent_count": agent_count,
        "bot_tokens": {
            "command": redact(bot_token),
            "announce": redact(announce_token),
            "notify": redact(notify_token),
            "command2": redact(command_token_2),
        },
        "bot_providers": {
            "command": primary_provider,
            "command2": command_provider_2,
        },
        "guild_id": guild_id,
        "owner_id": owner_id,
        "agents": agents,
        "draft_available": draft_available,
        "setup_mode": setup_mode,
        "resume_state": resume_state,
        "completion_state": onboarding_completion_state_value(completion_state.as_ref()),
        "partial_apply": completion_state
            .as_ref()
            .map(|state| state.partial_apply)
            .unwrap_or(false),
        "retry_recommended": completion_state
            .as_ref()
            .map(|state| state.retry_recommended)
            .unwrap_or(false),
        "rerun_policy": onboarding_rerun_policy_value(
            OnboardingRerunPolicy::ReuseExisting,
            false,
        ),
    }))
}

fn status_config() -> Result<serde_json::Value, String> {
    let runtime_root = crate::cli::agentdesk_runtime_root();
    let config = match runtime_root.as_ref() {
        Some(root) => load_onboarding_config(root)?,
        None => crate::config::Config::default(),
    };
    let agent_count = config.agents.len() as i64;
    let agents = config
        .agents
        .iter()
        .map(|agent| {
            let channel_id = agent.channels.iter().into_iter().find_map(|(_, channel)| {
                channel.and_then(|channel| {
                    channel
                        .channel_id()
                        .or_else(|| channel.channel_name())
                        .or_else(|| channel.target())
                })
            });
            json!({
                "agent_id": agent.id,
                "name": agent.name,
                "channel_id": channel_id,
            })
        })
        .collect::<Vec<_>>();
    let completion_state = runtime_root
        .as_ref()
        .and_then(|root| load_onboarding_completion_state(root).ok().flatten());
    let draft_available = runtime_root
        .as_ref()
        .map(|root| onboarding_draft_path(root).is_file())
        .unwrap_or(false);
    let completed = config.discord.guild_id.is_some() && agent_count > 0;
    let setup_mode = onboarding_setup_mode(completed);
    let resume_state = onboarding_resume_state(draft_available, completion_state.as_ref());

    Ok(json!({
        "completed": completed,
        "agent_count": agent_count,
        "bot_tokens": {
            "command": Option::<String>::None,
            "announce": Option::<String>::None,
            "notify": Option::<String>::None,
            "command2": Option::<String>::None,
        },
        "bot_providers": {
            "command": config.discord.bots.get("command").and_then(|bot| bot.provider.clone()),
            "command2": config.discord.bots.get("command_2").and_then(|bot| bot.provider.clone()),
        },
        "guild_id": config.discord.guild_id,
        "owner_id": config.discord.owner_id.map(|id| id.to_string()),
        "agents": agents,
        "draft_available": draft_available,
        "setup_mode": setup_mode,
        "resume_state": resume_state,
        "completion_state": onboarding_completion_state_value(completion_state.as_ref()),
        "partial_apply": completion_state
            .as_ref()
            .map(|state| state.partial_apply)
            .unwrap_or(false),
        "retry_recommended": completion_state
            .as_ref()
            .map(|state| state.retry_recommended)
            .unwrap_or(false),
        "rerun_policy": onboarding_rerun_policy_value(
            OnboardingRerunPolicy::ReuseExisting,
            false,
        ),
    }))
}

/// GET /api/onboarding/draft
/// Returns the in-progress onboarding draft, distinct from completed setup summary.
pub async fn draft_get(state: &AppState) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let completed = if let Some(pool) = state.pg_pool_ref() {
        match onboarding_has_agents_pg(pool).await {
            Ok(completed) => completed,
            Err(error) => {
                return Err(AppError::internal(error));
            }
        }
    } else {
        {
            crate::cli::agentdesk_runtime_root()
                .as_ref()
                .and_then(|root| load_onboarding_config(root).ok())
                .map(|config| config.discord.guild_id.is_some() && !config.agents.is_empty())
                .unwrap_or(false)
        }
    };

    let root = crate::cli::agentdesk_runtime_root()
        .ok_or_else(|| AppError::internal("cannot determine runtime root"))?;

    let draft = load_onboarding_draft(&root)
        .map_err(AppError::internal)?
        .map(OnboardingDraft::redact_secrets);
    let completion_state = load_onboarding_completion_state(&root).map_err(AppError::internal)?;
    let available = draft.is_some();

    Ok((
        StatusCode::OK,
        Json(json!({
            "available": available,
            "completed": completed,
            "draft": draft,
            "setup_mode": onboarding_setup_mode(completed),
            "resume_state": onboarding_resume_state(available, completion_state.as_ref()),
            "completion_state": onboarding_completion_state_value(completion_state.as_ref()),
            "secret_policy": onboarding_draft_secret_policy_value(),
        })),
    ))
}

/// PUT /api/onboarding/draft
/// Persists the in-progress onboarding draft required to resume across browsers.
pub async fn draft_put(body: OnboardingDraft) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let root = crate::cli::agentdesk_runtime_root()
        .ok_or_else(|| AppError::internal("cannot determine runtime root"))?;

    crate::runtime_layout::ensure_runtime_layout(&root).map_err(|error| {
        AppError::internal(format!("failed to prepare runtime layout: {error}"))
    })?;

    let draft = body.normalize().map_err(AppError::bad_request)?;
    let draft = draft.redact_secrets();

    save_onboarding_draft(&root, &draft).map_err(AppError::internal)?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "available": true,
            "draft": draft,
            "secret_policy": onboarding_draft_secret_policy_value(),
        })),
    ))
}

/// DELETE /api/onboarding/draft
/// Explicitly removes the in-progress onboarding draft.
pub async fn draft_delete() -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let root = crate::cli::agentdesk_runtime_root()
        .ok_or_else(|| AppError::internal("cannot determine runtime root"))?;

    clear_onboarding_draft(&root).map_err(AppError::internal)?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "available": false,
            "secret_policy": onboarding_draft_secret_policy_value(),
        })),
    ))
}

// Discord token / channel discovery handlers moved to `channel` submodule.

#[derive(Debug, Deserialize, Clone)]
pub struct CompleteBody {
    pub token: String,
    pub announce_token: Option<String>,
    pub notify_token: Option<String>,
    pub command_token_2: Option<String>,
    pub command_provider_2: Option<String>,
    pub guild_id: String,
    pub owner_id: Option<String>,
    pub provider: Option<String>,
    pub channels: Vec<ChannelMapping>,
    pub template: Option<String>,
    pub rerun_policy: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ChannelMapping {
    pub channel_id: String,
    pub channel_name: String,
    pub role_id: String,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChannelResolutionKind {
    ProvidedId,
    ExistingChannel,
    CreatedChannel,
    Checkpoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedChannelMapping {
    channel_id: String,
    channel_name: String,
    requested_channel_name: String,
    role_id: String,
    description: Option<String>,
    system_prompt: Option<String>,
    created: bool,
    resolution: ChannelResolutionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OnboardingRerunPolicy {
    ReuseExisting,
    ReplaceExisting,
}

impl OnboardingRerunPolicy {
    fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("reuse_existing") => Ok(Self::ReuseExisting),
            Some("replace_existing") => Ok(Self::ReplaceExisting),
            Some(other) => Err(format!("unsupported rerun_policy '{other}'")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ReuseExisting => "reuse_existing",
            Self::ReplaceExisting => "replace_existing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OnboardingCompletionStage {
    ChannelsResolved,
    ArtifactsPersisted,
    Completed,
}

impl OnboardingCompletionStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::ChannelsResolved => "channels_resolved",
            Self::ArtifactsPersisted => "artifacts_persisted",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OnboardingCompletionChannelState {
    role_id: String,
    requested_channel_name: String,
    channel_id: String,
    channel_name: String,
    created: bool,
    resolution: ChannelResolutionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OnboardingCompletionState {
    request_fingerprint: String,
    guild_id: String,
    provider: String,
    rerun_policy: String,
    stage: OnboardingCompletionStage,
    partial_apply: bool,
    retry_recommended: bool,
    updated_at_ms: i64,
    last_error: Option<String>,
    channels: Vec<OnboardingCompletionChannelState>,
}

#[derive(Debug, Clone)]
struct CompleteExecutionOptions {
    discord_api_base: String,
    fail_after_stage: Option<OnboardingCompletionStage>,
}

impl Default for CompleteExecutionOptions {
    fn default() -> Self {
        Self {
            discord_api_base: DISCORD_API_BASE.to_string(),
            fail_after_stage: None,
        }
    }
}

fn is_discord_channel_id(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit())
}

fn normalized_channel_name(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_start_matches('#').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn desired_channel_name(mapping: &ChannelMapping) -> Result<String, String> {
    normalized_channel_name(&mapping.channel_name)
        .or_else(|| normalized_channel_name(&mapping.channel_id))
        .ok_or_else(|| format!("agent '{}' is missing a channel name", mapping.role_id))
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn onboarding_draft_path(runtime_root: &Path) -> PathBuf {
    crate::runtime_layout::config_dir(runtime_root).join("onboarding_draft.json")
}

fn load_onboarding_draft(runtime_root: &Path) -> Result<Option<OnboardingDraft>, String> {
    let path = onboarding_draft_path(runtime_root);
    if !path.is_file() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read onboarding draft {}: {e}", path.display()))?;
    let draft = match serde_json::from_str::<OnboardingDraft>(&content) {
        Ok(draft) => draft,
        Err(error) => {
            let corrupt_path = path.with_file_name(format!(
                "{}.corrupt-{}",
                path.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("draft"),
                now_unix_ms()
            ));
            match std::fs::rename(&path, &corrupt_path) {
                Ok(()) => tracing::warn!(
                    "ignored corrupt onboarding draft {}; moved to {}: {}",
                    path.display(),
                    corrupt_path.display(),
                    error
                ),
                Err(rename_error) => tracing::warn!(
                    "ignored corrupt onboarding draft {}; failed to move aside: {}; parse error: {}",
                    path.display(),
                    rename_error,
                    error
                ),
            }
            return Ok(None);
        }
    };
    let mut draft = draft;
    draft.owner_id = sanitize_draft_owner_id(&draft.owner_id);
    Ok(Some(draft))
}

fn save_onboarding_draft(runtime_root: &Path, draft: &OnboardingDraft) -> Result<(), String> {
    let path = onboarding_draft_path(runtime_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create draft dir {}: {e}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(draft)
        .map_err(|e| format!("failed to serialize onboarding draft: {e}"))?;
    crate::services::discord::runtime_store::atomic_write(&path, &content)
        .map_err(|e| format!("failed to write onboarding draft {}: {e}", path.display()))
}

fn clear_onboarding_draft(runtime_root: &Path) -> Result<(), String> {
    let path = onboarding_draft_path(runtime_root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to remove onboarding draft {}: {error}",
            path.display()
        )),
    }
}

fn onboarding_completion_state_path(runtime_root: &Path) -> PathBuf {
    crate::runtime_layout::config_dir(runtime_root).join("onboarding_completion_state.json")
}

fn load_onboarding_completion_state(
    runtime_root: &Path,
) -> Result<Option<OnboardingCompletionState>, String> {
    let path = onboarding_completion_state_path(runtime_root);
    if !path.is_file() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "failed to read onboarding completion state {}: {e}",
            path.display()
        )
    })?;
    let state = match serde_json::from_str::<OnboardingCompletionState>(&content) {
        Ok(state) => state,
        Err(error) => {
            let corrupt_path = path.with_file_name(format!(
                "{}.corrupt-{}",
                path.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("state"),
                now_unix_ms()
            ));
            match std::fs::rename(&path, &corrupt_path) {
                Ok(()) => tracing::warn!(
                    "ignored corrupt onboarding completion state {}; moved to {}: {}",
                    path.display(),
                    corrupt_path.display(),
                    error
                ),
                Err(rename_error) => tracing::warn!(
                    "ignored corrupt onboarding completion state {}; failed to move aside: {}; parse error: {}",
                    path.display(),
                    rename_error,
                    error
                ),
            }
            return Ok(None);
        }
    };
    Ok(Some(state))
}

fn save_onboarding_completion_state(
    runtime_root: &Path,
    state: &OnboardingCompletionState,
) -> Result<(), String> {
    let path = onboarding_completion_state_path(runtime_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create completion state dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let content = serde_json::to_string_pretty(state)
        .map_err(|e| format!("failed to serialize onboarding completion state: {e}"))?;
    crate::services::discord::runtime_store::atomic_write(&path, &content).map_err(|e| {
        format!(
            "failed to write onboarding completion state {}: {e}",
            path.display()
        )
    })
}

fn build_onboarding_completion_state(
    request_fingerprint: &str,
    guild_id: &str,
    provider: &str,
    rerun_policy: OnboardingRerunPolicy,
    stage: OnboardingCompletionStage,
    partial_apply: bool,
    retry_recommended: bool,
    last_error: Option<String>,
    resolved_channels: &[ResolvedChannelMapping],
) -> OnboardingCompletionState {
    OnboardingCompletionState {
        request_fingerprint: request_fingerprint.to_string(),
        guild_id: guild_id.trim().to_string(),
        provider: provider.trim().to_string(),
        rerun_policy: rerun_policy.as_str().to_string(),
        stage,
        partial_apply,
        retry_recommended,
        updated_at_ms: now_unix_ms(),
        last_error,
        channels: resolved_channels
            .iter()
            .map(|mapping| OnboardingCompletionChannelState {
                role_id: mapping.role_id.clone(),
                requested_channel_name: mapping.requested_channel_name.clone(),
                channel_id: mapping.channel_id.clone(),
                channel_name: mapping.channel_name.clone(),
                created: mapping.created,
                resolution: mapping.resolution,
            })
            .collect(),
    }
}

fn onboarding_completion_state_value(
    completion_state: Option<&OnboardingCompletionState>,
) -> serde_json::Value {
    completion_state
        .and_then(|state| serde_json::to_value(state).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn onboarding_rerun_policy_value(
    rerun_policy: OnboardingRerunPolicy,
    explicit: bool,
) -> serde_json::Value {
    json!({
        "applied": rerun_policy.as_str(),
        "explicit": explicit,
        "supported": ["reuse_existing", "replace_existing"],
    })
}

fn completion_response(
    status: StatusCode,
    ok: bool,
    provider: &str,
    rerun_policy: OnboardingRerunPolicy,
    explicit_rerun_policy: bool,
    completion_state: Option<&OnboardingCompletionState>,
    error: Option<String>,
    conflicts: Vec<String>,
    mut extra: serde_json::Map<String, serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    extra.insert("ok".to_string(), json!(ok));
    extra.insert("provider".to_string(), json!(provider));
    extra.insert(
        "partial_apply".to_string(),
        json!(
            completion_state
                .map(|state| state.partial_apply)
                .unwrap_or(false)
        ),
    );
    extra.insert(
        "retry_recommended".to_string(),
        json!(
            completion_state
                .map(|state| state.retry_recommended)
                .unwrap_or(false)
        ),
    );
    extra.insert(
        "completion_state".to_string(),
        onboarding_completion_state_value(completion_state),
    );
    extra.insert(
        "rerun_policy".to_string(),
        onboarding_rerun_policy_value(rerun_policy, explicit_rerun_policy),
    );
    if let Some(error) = error {
        extra.insert("error".to_string(), json!(error));
    }
    if !conflicts.is_empty() {
        extra.insert("conflicts".to_string(), json!(conflicts));
    }
    (status, serde_json::Value::Object(extra))
}

fn normalized_optional_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

fn requested_channel_fingerprint(body: &CompleteBody, provider: &str) -> Result<String, String> {
    let mut channels = body
        .channels
        .iter()
        .map(|mapping| {
            Ok(json!({
                "role_id": mapping.role_id.trim(),
                "channel_id": normalized_channel_name(&mapping.channel_id)
                    .unwrap_or_else(|| mapping.channel_id.trim().to_string()),
                "channel_name": desired_channel_name(mapping)?,
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;

    channels.sort_by(|left, right| left.to_string().cmp(&right.to_string()));

    let payload = json!({
        "guild_id": body.guild_id.trim(),
        "provider": provider.trim(),
        "channels": channels,
    });
    let mut hasher = Sha256::new();
    hasher.update(payload.to_string().as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

fn role_map_entry_role_id(value: &serde_json::Value) -> Option<&str> {
    value.get("roleId").and_then(|value| value.as_str())
}

fn role_map_entry_channel_id(value: &serde_json::Value) -> Option<&str> {
    value.get("channelId").and_then(|value| value.as_str())
}

async fn discord_list_guild_channels(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    guild_id: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let url = format!(
        "{}/guilds/{}/channels",
        api_base.trim_end_matches('/'),
        guild_id
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|e| format!("failed to fetch guild channels: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Discord API {status} while listing channels: {body}"
        ));
    }

    resp.json::<Vec<serde_json::Value>>()
        .await
        .map_err(|e| format!("failed to parse guild channels: {e}"))
}

async fn discord_create_text_channel(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    guild_id: &str,
    channel_name: &str,
    topic: Option<&str>,
) -> Result<serde_json::Value, String> {
    let url = format!(
        "{}/guilds/{}/channels",
        api_base.trim_end_matches('/'),
        guild_id
    );

    let mut payload = json!({
        "name": channel_name,
        "type": 0,
    });

    if let Some(topic) = topic.map(str::trim).filter(|value| !value.is_empty()) {
        let truncated: String = topic.chars().take(1024).collect();
        payload["topic"] = json!(truncated);
    }

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bot {}", token))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("failed to create channel '{channel_name}': {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Discord API {status} while creating channel '{channel_name}': {body}"
        ));
    }

    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("failed to parse created channel '{channel_name}': {e}"))
}

async fn resolve_channel_mapping(
    client: &reqwest::Client,
    token: &str,
    api_base: &str,
    guild_id: &str,
    mapping: &ChannelMapping,
    checkpoint: Option<&OnboardingCompletionChannelState>,
) -> Result<ResolvedChannelMapping, String> {
    let requested_name = desired_channel_name(mapping)?;

    if let Some(checkpoint) = checkpoint {
        return Ok(ResolvedChannelMapping {
            channel_id: checkpoint.channel_id.clone(),
            channel_name: checkpoint.channel_name.clone(),
            requested_channel_name: requested_name.clone(),
            role_id: mapping.role_id.clone(),
            description: mapping.description.clone(),
            system_prompt: mapping.system_prompt.clone(),
            created: checkpoint.created,
            resolution: ChannelResolutionKind::Checkpoint,
        });
    }

    if is_discord_channel_id(&mapping.channel_id) {
        return Ok(ResolvedChannelMapping {
            channel_id: mapping.channel_id.trim().to_string(),
            channel_name: requested_name.clone(),
            requested_channel_name: requested_name.clone(),
            role_id: mapping.role_id.clone(),
            description: mapping.description.clone(),
            system_prompt: mapping.system_prompt.clone(),
            created: false,
            resolution: ChannelResolutionKind::ProvidedId,
        });
    }

    let guild_id = guild_id.trim();
    if guild_id.is_empty() {
        return Err(format!(
            "cannot create channel '{}' without selecting a Discord server",
            requested_name
        ));
    }

    let existing = discord_list_guild_channels(client, token, api_base, guild_id)
        .await?
        .into_iter()
        .find(|channel| {
            channel.get("type").and_then(|value| value.as_i64()) == Some(0)
                && channel
                    .get("name")
                    .and_then(|value| value.as_str())
                    .map(|name| name == requested_name)
                    .unwrap_or(false)
        });

    if let Some(channel) = existing {
        let channel_id = channel
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("existing channel '{}' is missing an id", requested_name))?;
        let channel_name = channel
            .get("name")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&requested_name)
            .to_string();

        return Ok(ResolvedChannelMapping {
            channel_id: channel_id.to_string(),
            channel_name,
            requested_channel_name: requested_name.clone(),
            role_id: mapping.role_id.clone(),
            description: mapping.description.clone(),
            system_prompt: mapping.system_prompt.clone(),
            created: false,
            resolution: ChannelResolutionKind::ExistingChannel,
        });
    }

    let created = discord_create_text_channel(
        client,
        token,
        api_base,
        guild_id,
        &requested_name,
        mapping.description.as_deref(),
    )
    .await?;

    let channel_id = created
        .get("id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("created channel '{}' is missing an id", requested_name))?;
    let channel_name = created
        .get("name")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&requested_name)
        .to_string();

    Ok(ResolvedChannelMapping {
        channel_id: channel_id.to_string(),
        channel_name,
        requested_channel_name: requested_name,
        role_id: mapping.role_id.clone(),
        description: mapping.description.clone(),
        system_prompt: mapping.system_prompt.clone(),
        created: true,
        resolution: ChannelResolutionKind::CreatedChannel,
    })
}

fn write_credential_token(
    runtime_root: &Path,
    bot_name: &str,
    token: Option<&str>,
) -> Result<(), String> {
    crate::runtime_layout::ensure_credential_layout(runtime_root)?;
    let credential_dir = crate::runtime_layout::credential_dir(runtime_root);
    std::fs::create_dir_all(&credential_dir).map_err(|e| e.to_string())?;
    let path = crate::runtime_layout::credential_token_path(runtime_root, bot_name);

    match token.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => crate::utils::secret_file::write_secret_file(&path, format!("{value}\n"))
            .map_err(|e| e.to_string()),
        None => {
            if path.exists() {
                std::fs::remove_file(path).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod credential_token_permission_tests {
    use super::*;

    #[test]
    fn write_credential_token_creates_owner_only_secret_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(crate::runtime_layout::config_dir(root)).unwrap();

        write_credential_token(root, "announce", Some("announce-token")).unwrap();

        let token_path = crate::runtime_layout::credential_token_path(root, "announce");
        assert_eq!(
            std::fs::read_to_string(&token_path).unwrap(),
            "announce-token\n"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let credential_dir_mode =
                std::fs::metadata(crate::runtime_layout::credential_dir(root))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
            let token_mode = std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;

            assert_eq!(credential_dir_mode, 0o700);
            assert_eq!(token_mode, 0o600);
        }
    }
}

fn onboarding_config_path(runtime_root: &Path) -> PathBuf {
    let canonical = crate::runtime_layout::config_file_path(runtime_root);
    let legacy = crate::runtime_layout::legacy_config_file_path(runtime_root);
    if canonical.is_file() || !legacy.is_file() {
        canonical
    } else {
        legacy
    }
}

fn default_secondary_command_provider(primary_provider: &str) -> &'static str {
    match primary_provider {
        "codex" => "claude",
        "gemini" => "codex",
        "opencode" => "claude",
        _ => "codex",
    }
}

fn parse_owner_id(owner_id: Option<&str>) -> Result<Option<u64>, String> {
    let Some(value) = owner_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    if !(17..=20).contains(&value.len()) || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("owner_id must be a Discord user id with 17-20 digits".to_string());
    }

    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| "owner_id must be a valid Discord user id".to_string())
}

fn upsert_command_bot(
    config: &mut crate::config::Config,
    bot_name: &str,
    token: &str,
    provider: &str,
) {
    let mut bot = config
        .discord
        .bots
        .get(bot_name)
        .cloned()
        .unwrap_or_default();
    bot.token = Some(token.trim().to_string());
    bot.provider = Some(provider.trim().to_string());
    config.discord.bots.insert(bot_name.to_string(), bot);
}

fn write_agentdesk_discord_config(
    runtime_root: &Path,
    guild_id: &str,
    primary_token: &str,
    primary_provider: &str,
    secondary_token: Option<&str>,
    secondary_provider: Option<&str>,
    owner_id: Option<&str>,
) -> Result<(), String> {
    let config_path = onboarding_config_path(runtime_root);
    let mut config = if config_path.is_file() {
        crate::config::load_from_path(&config_path)
            .map_err(|e| format!("Failed to load config {}: {e}", config_path.display()))?
    } else {
        crate::config::Config::default()
    };

    config.discord.guild_id = Some(guild_id.trim().to_string());
    config.discord.owner_id = parse_owner_id(owner_id)?;

    upsert_command_bot(&mut config, "command", primary_token, primary_provider);

    match secondary_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(token) => {
            let provider = secondary_provider
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(default_secondary_command_provider(primary_provider));
            upsert_command_bot(&mut config, "command_2", token, provider);
        }
        None => {
            config.discord.bots.remove("command_2");
        }
    }

    crate::config::save_to_path(&config_path, &config)
        .map_err(|e| format!("Failed to write config {}: {e}", config_path.display()))
}

fn tilde_display_path(path: &Path) -> String {
    dirs::home_dir()
        .and_then(|home| {
            path.strip_prefix(&home)
                .ok()
                .map(|relative| format!("~/{}", relative.display()))
        })
        .unwrap_or_else(|| path.display().to_string())
}

fn agent_channel_slot_mut<'a>(
    channels: &'a mut crate::config::AgentChannels,
    provider: &str,
) -> Option<&'a mut Option<crate::config::AgentChannel>> {
    match provider {
        "claude" => Some(&mut channels.claude),
        "codex" => Some(&mut channels.codex),
        "gemini" => Some(&mut channels.gemini),
        "opencode" => Some(&mut channels.opencode),
        "qwen" => Some(&mut channels.qwen),
        _ => None,
    }
}

fn channel_config_from_existing(
    current: Option<crate::config::AgentChannel>,
) -> crate::config::AgentChannelConfig {
    match current {
        Some(crate::config::AgentChannel::Detailed(config)) => config,
        Some(crate::config::AgentChannel::Legacy(raw)) => {
            let mut config = crate::config::AgentChannelConfig::default();
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                if trimmed.parse::<u64>().is_ok() {
                    config.id = Some(trimmed.to_string());
                } else {
                    config.name = Some(trimmed.to_string());
                }
            }
            config
        }
        None => crate::config::AgentChannelConfig::default(),
    }
}

fn push_channel_alias(config: &mut crate::config::AgentChannelConfig, alias: String) {
    let trimmed = alias.trim();
    if trimmed.is_empty() || config.name.as_deref() == Some(trimmed) {
        return;
    }
    if !config.aliases.iter().any(|existing| existing == trimmed) {
        config.aliases.push(trimmed.to_string());
        config.aliases.sort();
        config.aliases.dedup();
    }
}

pub(super) fn load_onboarding_config(runtime_root: &Path) -> Result<crate::config::Config, String> {
    let config_path = onboarding_config_path(runtime_root);
    if config_path.is_file() {
        crate::config::load_from_path(&config_path)
            .map_err(|e| format!("Failed to load config {}: {e}", config_path.display()))
    } else {
        Ok(crate::config::Config::default())
    }
}

fn load_onboarding_role_map(runtime_root: &Path) -> Result<serde_json::Value, String> {
    let path = crate::runtime_layout::role_map_path(runtime_root);
    if !path.is_file() {
        return Ok(json!({
            "version": 1,
            "byChannelId": {},
            "byChannelName": {},
            "fallbackByChannelName": { "enabled": true },
        }));
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read role map {}: {e}", path.display()))?;
    serde_json::from_str(&content)
        .map_err(|e| format!("failed to parse role map {}: {e}", path.display()))
}

fn validate_unique_resolved_channels(
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<(), String> {
    let mut seen_roles = std::collections::BTreeSet::new();
    let mut seen_channel_ids = std::collections::BTreeMap::new();

    for mapping in resolved_channels {
        if !seen_roles.insert(mapping.role_id.clone()) {
            return Err(format!(
                "duplicate onboarding agent id '{}' in completion payload",
                mapping.role_id
            ));
        }

        if let Some(previous_role) =
            seen_channel_ids.insert(mapping.channel_id.clone(), mapping.role_id.clone())
        {
            return Err(format!(
                "channel '{}' is assigned to both '{}' and '{}'",
                mapping.channel_id, previous_role, mapping.role_id
            ));
        }
    }

    Ok(())
}

async fn collect_onboarding_conflicts_pg(
    pool: &sqlx::PgPool,
    runtime_root: &Path,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
    rerun_policy: OnboardingRerunPolicy,
) -> Result<Vec<String>, String> {
    validate_unique_resolved_channels(resolved_channels)?;

    let config = load_onboarding_config(runtime_root)?;
    let role_map = load_onboarding_role_map(runtime_root)?;
    let by_channel_id = role_map
        .get("byChannelId")
        .and_then(|value| value.as_object());
    let by_channel_name = role_map
        .get("byChannelName")
        .and_then(|value| value.as_object());

    let mut conflicts = Vec::new();

    for mapping in resolved_channels {
        let existing_agent = sqlx::query_as::<
            _,
            (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        >(
            "SELECT provider, discord_channel_id, description, system_prompt \
             FROM agents WHERE id = $1",
        )
        .bind(&mapping.role_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("failed to query postgres agent {}: {e}", mapping.role_id))?;

        if let Some((
            existing_provider,
            existing_channel_id,
            existing_description,
            existing_prompt,
        )) = existing_agent
        {
            if rerun_policy == OnboardingRerunPolicy::ReuseExisting {
                if let Some(existing_channel_id) =
                    normalized_optional_text(existing_channel_id.as_deref())
                {
                    if existing_channel_id != mapping.channel_id {
                        conflicts.push(format!(
                            "agent '{}' already uses Discord channel '{}' in DB; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_channel_id, mapping.channel_id
                        ));
                    }
                }

                if let Some(existing_provider) =
                    normalized_optional_text(existing_provider.as_deref())
                {
                    if existing_provider != provider {
                        conflicts.push(format!(
                            "agent '{}' already uses provider '{}' in config DB state; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_provider, provider
                        ));
                    }
                }

                if let (Some(existing), Some(requested)) = (
                    normalized_optional_text(existing_description.as_deref()),
                    normalized_optional_text(mapping.description.as_deref()),
                ) {
                    if existing != requested {
                        conflicts.push(format!(
                            "agent '{}' already has a different description in DB; rerun_policy=reuse_existing refuses to overwrite it",
                            mapping.role_id
                        ));
                    }
                }

                if let (Some(existing), Some(requested)) = (
                    normalized_optional_text(existing_prompt.as_deref()),
                    normalized_optional_text(mapping.system_prompt.as_deref()),
                ) {
                    if existing != requested {
                        conflicts.push(format!(
                            "agent '{}' already has a different system prompt in DB; rerun_policy=reuse_existing refuses to overwrite it",
                            mapping.role_id
                        ));
                    }
                }
            }
        }

        let conflicting_db_channel_owner = sqlx::query_scalar::<_, String>(
            "SELECT id FROM agents WHERE discord_channel_id = $1 AND id != $2 LIMIT 1",
        )
        .bind(&mapping.channel_id)
        .bind(&mapping.role_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            format!(
                "failed to check existing postgres DB channel owner {}: {e}",
                mapping.channel_id
            )
        })?;
        if let Some(other_agent_id) = conflicting_db_channel_owner {
            conflicts.push(format!(
                "Discord channel '{}' is already assigned to agent '{}' in DB",
                mapping.channel_id, other_agent_id
            ));
        }

        if let Some(agent) = config
            .agents
            .iter()
            .find(|agent| agent.id == mapping.role_id)
        {
            if rerun_policy == OnboardingRerunPolicy::ReuseExisting && agent.provider != provider {
                conflicts.push(format!(
                    "agent '{}' already uses provider '{}' in agentdesk.yaml; rerun_policy=reuse_existing refuses to replace it with '{}'",
                    mapping.role_id, agent.provider, provider
                ));
            }

            if rerun_policy == OnboardingRerunPolicy::ReuseExisting {
                if let Some(slot) = agent_channel_slot_ref(&agent.channels, provider) {
                    let channel = channel_config_from_existing(slot.clone());
                    let existing_channel_id = channel.channel_id();
                    let existing_names = channel.all_names();
                    let same_channel_id =
                        existing_channel_id.as_deref() == Some(mapping.channel_id.as_str());
                    let same_channel_name = existing_names.iter().any(|name| {
                        name == &mapping.channel_name || name == &mapping.requested_channel_name
                    });
                    let conflicts_with_existing = if existing_channel_id.is_some() {
                        !same_channel_id
                    } else {
                        !existing_names.is_empty() && !same_channel_name
                    };
                    if conflicts_with_existing {
                        conflicts.push(format!(
                            "agent '{}' already maps to a different channel in agentdesk.yaml; rerun_policy=reuse_existing refuses to replace it",
                            mapping.role_id
                        ));
                    }
                }
            }
        }

        for agent in &config.agents {
            if agent.id == mapping.role_id {
                continue;
            }
            let Some(slot) = agent_channel_slot_ref(&agent.channels, provider) else {
                continue;
            };
            let channel = channel_config_from_existing(slot.clone());
            let uses_same_target = channel.channel_id().as_deref()
                == Some(mapping.channel_id.as_str())
                || channel.all_names().iter().any(|name| {
                    name == &mapping.channel_name || name == &mapping.requested_channel_name
                });
            if uses_same_target {
                conflicts.push(format!(
                    "agent '{}' already owns channel '{}' in agentdesk.yaml",
                    agent.id, mapping.channel_id
                ));
            }
        }

        if let Some(entry) = by_channel_id.and_then(|entries| entries.get(&mapping.channel_id))
            && let Some(role_id) = role_map_entry_role_id(entry)
            && role_id != mapping.role_id
        {
            conflicts.push(format!(
                "role_map.json already binds channel '{}' to agent '{}'",
                mapping.channel_id, role_id
            ));
        }

        if let Some(entry) = by_channel_name.and_then(|entries| entries.get(&mapping.channel_name))
            && let Some(role_id) = role_map_entry_role_id(entry)
            && role_id != mapping.role_id
        {
            conflicts.push(format!(
                "role_map.json already binds channel name '{}' to agent '{}'",
                mapping.channel_name, role_id
            ));
        }

        if rerun_policy == OnboardingRerunPolicy::ReuseExisting {
            if let Some(entries) = by_channel_id {
                for (existing_channel_id, entry) in entries {
                    if role_map_entry_role_id(entry) == Some(mapping.role_id.as_str())
                        && existing_channel_id != &mapping.channel_id
                    {
                        conflicts.push(format!(
                            "role_map.json already binds agent '{}' to Discord channel '{}'; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_channel_id, mapping.channel_id
                        ));
                    }
                }
            }

            if let Some(entries) = by_channel_name {
                for (existing_name, entry) in entries {
                    if role_map_entry_role_id(entry) != Some(mapping.role_id.as_str()) {
                        continue;
                    }

                    let same_name = existing_name == &mapping.channel_name
                        || existing_name == &mapping.requested_channel_name;
                    if !same_name {
                        conflicts.push(format!(
                            "role_map.json already binds agent '{}' to channel name '{}'; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            mapping.role_id, existing_name, mapping.channel_name
                        ));
                        continue;
                    }

                    if let Some(existing_channel_id) = role_map_entry_channel_id(entry)
                        && existing_channel_id != mapping.channel_id
                    {
                        conflicts.push(format!(
                            "role_map.json already binds channel name '{}' for agent '{}' to Discord channel '{}'; rerun_policy=reuse_existing refuses to replace it with '{}'",
                            existing_name, mapping.role_id, existing_channel_id, mapping.channel_id
                        ));
                    }
                }
            }
        }
    }

    Ok(conflicts)
}

fn write_onboarding_role_map(
    runtime_root: &Path,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<(), String> {
    let mut role_map = load_onboarding_role_map(runtime_root)?;
    let root = role_map
        .as_object_mut()
        .ok_or_else(|| "role map root must be a JSON object".to_string())?;

    root.insert("version".to_string(), json!(1));
    root.entry("fallbackByChannelName".to_string())
        .or_insert_with(|| json!({ "enabled": true }));
    root.entry("byChannelId".to_string())
        .or_insert_with(|| json!({}));
    root.entry("byChannelName".to_string())
        .or_insert_with(|| json!({}));

    let resolved_role_ids = resolved_channels
        .iter()
        .map(|mapping| mapping.role_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    root.get_mut("byChannelId")
        .and_then(|value| value.as_object_mut())
        .ok_or_else(|| "role map byChannelId must be a JSON object".to_string())?
        .retain(|_, entry| {
            role_map_entry_role_id(entry)
                .map(|role_id| !resolved_role_ids.contains(role_id))
                .unwrap_or(true)
        });
    root.get_mut("byChannelName")
        .and_then(|value| value.as_object_mut())
        .ok_or_else(|| "role map byChannelName must be a JSON object".to_string())?
        .retain(|_, entry| {
            role_map_entry_role_id(entry)
                .map(|role_id| !resolved_role_ids.contains(role_id))
                .unwrap_or(true)
        });

    for mapping in resolved_channels {
        let workspace_tilde =
            tilde_display_path(&runtime_root.join("workspaces").join(&mapping.role_id));
        root.get_mut("byChannelId")
            .and_then(|value| value.as_object_mut())
            .ok_or_else(|| "role map byChannelId must be a JSON object".to_string())?
            .insert(
                mapping.channel_id.clone(),
                json!({
                    "roleId": mapping.role_id,
                    "provider": provider,
                    "workspace": workspace_tilde.clone(),
                }),
            );
        root.get_mut("byChannelName")
            .and_then(|value| value.as_object_mut())
            .ok_or_else(|| "role map byChannelName must be a JSON object".to_string())?
            .insert(
                mapping.channel_name.clone(),
                json!({
                    "roleId": mapping.role_id,
                    "channelId": mapping.channel_id,
                    "workspace": workspace_tilde,
                }),
            );
    }

    let path = crate::runtime_layout::role_map_path(runtime_root);
    let content = serde_json::to_string_pretty(&role_map)
        .map_err(|e| format!("failed to serialize role map: {e}"))?;
    std::fs::write(&path, content)
        .map_err(|e| format!("failed to write role map {}: {e}", path.display()))
}

fn write_agentdesk_channel_bindings(
    runtime_root: &Path,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<(), String> {
    let config_path = onboarding_config_path(runtime_root);
    let mut config = load_onboarding_config(runtime_root)?;

    for mapping in resolved_channels {
        let workspace = tilde_display_path(&runtime_root.join("workspaces").join(&mapping.role_id));
        let agent_index = if let Some(index) = config
            .agents
            .iter()
            .position(|agent| agent.id == mapping.role_id)
        {
            index
        } else {
            config.agents.push(crate::config::AgentDef {
                id: mapping.role_id.clone(),
                name: mapping.role_id.clone(),
                name_ko: None,
                aliases: Vec::new(),
                wake_word: None,
                voice_enabled: true,
                sensitivity_mode: None,
                voice: crate::config::AgentVoiceConfig::default(),
                provider: provider.to_string(),
                channels: crate::config::AgentChannels::default(),
                keywords: Vec::new(),
                department: None,
                avatar_emoji: None,
                preferred_intake_node_labels: None,
            });
            config.agents.len() - 1
        };

        let agent = &mut config.agents[agent_index];
        agent.provider = provider.to_string();

        let Some(slot) = agent_channel_slot_mut(&mut agent.channels, provider) else {
            return Err(format!(
                "unsupported provider for onboarding yaml sync: {provider}"
            ));
        };

        let mut channel = channel_config_from_existing(slot.clone());
        if let Some(existing_name) = channel
            .name
            .clone()
            .filter(|existing| existing != &mapping.channel_name)
        {
            push_channel_alias(&mut channel, existing_name);
        }
        channel.id = Some(mapping.channel_id.clone());
        channel.name = Some(mapping.channel_name.clone());
        channel.workspace = Some(workspace);
        channel.provider = Some(provider.to_string());
        *slot = Some(crate::config::AgentChannel::Detailed(channel));
    }

    crate::config::save_to_path(&config_path, &config)
        .map_err(|e| format!("Failed to write config {}: {e}", config_path.display()))
}

fn agent_channel_slot_ref<'a>(
    channels: &'a crate::config::AgentChannels,
    provider: &str,
) -> Option<&'a Option<crate::config::AgentChannel>> {
    match provider {
        "claude" => Some(&channels.claude),
        "codex" => Some(&channels.codex),
        "gemini" => Some(&channels.gemini),
        "opencode" => Some(&channels.opencode),
        "qwen" => Some(&channels.qwen),
        _ => None,
    }
}

fn verify_onboarding_settings_artifacts(
    runtime_root: &Path,
    primary_token: &str,
    primary_provider: &str,
    secondary_token: Option<&str>,
    secondary_provider: Option<&str>,
    guild_id: &str,
    owner_id: Option<&str>,
    announce_token: Option<&str>,
    notify_token: Option<&str>,
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<serde_json::Value, String> {
    let config_path = onboarding_config_path(runtime_root);
    if !config_path.is_file() {
        return Err(format!(
            "onboarding config was not written at {}",
            config_path.display()
        ));
    }
    let config = crate::config::load_from_path(&config_path).map_err(|e| {
        format!(
            "failed to reload onboarding config {}: {e}",
            config_path.display()
        )
    })?;

    if config.discord.guild_id.as_deref() != Some(guild_id.trim()) {
        return Err(format!(
            "discord guild mismatch after onboarding: expected '{}' got {:?}",
            guild_id.trim(),
            config.discord.guild_id
        ));
    }
    let expected_owner_id = parse_owner_id(owner_id)?;
    if config.discord.owner_id != expected_owner_id {
        return Err(format!(
            "discord owner mismatch after onboarding: expected {:?} got {:?}",
            expected_owner_id, config.discord.owner_id
        ));
    }

    let command_bot = config
        .discord
        .bots
        .get("command")
        .ok_or_else(|| "missing command bot config after onboarding".to_string())?;
    if command_bot.token.as_deref() != Some(primary_token.trim()) {
        return Err("primary command token was not persisted".to_string());
    }
    if command_bot.provider.as_deref() != Some(primary_provider.trim()) {
        return Err("primary command provider was not persisted".to_string());
    }

    match secondary_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(expected_token) => {
            let command2 = config
                .discord
                .bots
                .get("command_2")
                .ok_or_else(|| "missing command_2 bot config after onboarding".to_string())?;
            let expected_provider = secondary_provider
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(default_secondary_command_provider(primary_provider));
            if command2.token.as_deref() != Some(expected_token) {
                return Err("secondary command token was not persisted".to_string());
            }
            if command2.provider.as_deref() != Some(expected_provider) {
                return Err("secondary command provider was not persisted".to_string());
            }
        }
        None => {
            if config.discord.bots.contains_key("command_2") {
                return Err("unexpected command_2 bot config remained after onboarding".to_string());
            }
        }
    }

    for mapping in resolved_channels {
        let agent = config
            .agents
            .iter()
            .find(|agent| agent.id == mapping.role_id)
            .ok_or_else(|| format!("agent '{}' missing from onboarding config", mapping.role_id))?;
        if agent.provider != primary_provider {
            return Err(format!(
                "agent '{}' provider mismatch after onboarding: expected '{}' got '{}'",
                mapping.role_id, primary_provider, agent.provider
            ));
        }
        let slot = agent_channel_slot_ref(&agent.channels, primary_provider).ok_or_else(|| {
            format!(
                "unsupported provider '{}' in onboarding verification",
                primary_provider
            )
        })?;
        let channel = channel_config_from_existing(slot.clone());
        if channel.id.as_deref() != Some(mapping.channel_id.as_str()) {
            return Err(format!(
                "agent '{}' channel id mismatch after onboarding",
                mapping.role_id
            ));
        }
        if channel.name.as_deref() != Some(mapping.channel_name.as_str()) {
            return Err(format!(
                "agent '{}' channel name mismatch after onboarding",
                mapping.role_id
            ));
        }
    }

    let role_map_path = crate::runtime_layout::role_map_path(runtime_root);
    if !role_map_path.is_file() {
        return Err(format!(
            "role map was not written at {}",
            role_map_path.display()
        ));
    }

    let workspace_root = runtime_root.join("workspaces");
    for mapping in resolved_channels {
        let workspace = workspace_root.join(&mapping.role_id);
        if !workspace.is_dir() {
            return Err(format!(
                "workspace for agent '{}' missing at {}",
                mapping.role_id,
                workspace.display()
            ));
        }
    }

    let announce_path = crate::runtime_layout::credential_token_path(runtime_root, "announce");
    match announce_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(_) if !announce_path.is_file() => {
            return Err(format!(
                "announce credential missing at {}",
                announce_path.display()
            ));
        }
        None if announce_path.exists() => {
            return Err(format!(
                "announce credential should have been removed at {}",
                announce_path.display()
            ));
        }
        _ => {}
    }

    let notify_path = crate::runtime_layout::credential_token_path(runtime_root, "notify");
    match notify_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(_) if !notify_path.is_file() => {
            return Err(format!(
                "notify credential missing at {}",
                notify_path.display()
            ));
        }
        None if notify_path.exists() => {
            return Err(format!(
                "notify credential should have been removed at {}",
                notify_path.display()
            ));
        }
        _ => {}
    }

    Ok(json!({
        "config_path": config_path.display().to_string(),
        "role_map_path": role_map_path.display().to_string(),
        "workspace_root": workspace_root.display().to_string(),
        "workspace_count": resolved_channels.len(),
        "announce_credential_path": announce_path.display().to_string(),
        "notify_credential_path": notify_path.display().to_string(),
    }))
}

fn verify_onboarding_pipeline_artifact(runtime_root: &Path) -> Result<serde_json::Value, String> {
    let config_path = onboarding_config_path(runtime_root);
    let config = if config_path.is_file() {
        crate::config::load_from_path(&config_path).map_err(|e| {
            format!(
                "failed to reload onboarding config {}: {e}",
                config_path.display()
            )
        })?
    } else {
        crate::config::Config::default()
    };

    let mut candidates = Vec::new();
    candidates.push(config.policies.dir.join("default-pipeline.yaml"));
    if !config.policies.dir.is_absolute() {
        candidates.push(
            runtime_root
                .join(&config.policies.dir)
                .join("default-pipeline.yaml"),
        );
    }

    let pipeline_path = candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "default pipeline not found for onboarding under '{}' or runtime root '{}'",
                config.policies.dir.display(),
                runtime_root.display()
            )
        })?;

    let content = std::fs::read_to_string(&pipeline_path)
        .map_err(|e| format!("failed to read pipeline {}: {e}", pipeline_path.display()))?;
    let pipeline: crate::pipeline::PipelineConfig = serde_yaml::from_str(&content)
        .map_err(|e| format!("failed to parse pipeline {}: {e}", pipeline_path.display()))?;
    if pipeline.states.is_empty() || pipeline.transitions.is_empty() {
        return Err(format!(
            "pipeline {} is missing states or transitions",
            pipeline_path.display()
        ));
    }

    Ok(json!({
        "path": pipeline_path.display().to_string(),
        "states": pipeline.states.len(),
        "transitions": pipeline.transitions.len(),
    }))
}

async fn persist_onboarding_pg(
    pool: &sqlx::PgPool,
    body: &CompleteBody,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
) -> Result<(), String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("failed to start postgres onboarding transaction: {error}"))?;

    for (key, value) in [
        ("onboarding_bot_token", Some(body.token.trim())),
        ("onboarding_guild_id", Some(body.guild_id.trim())),
        ("onboarding_provider", Some(provider)),
        (
            "onboarding_owner_id",
            body.owner_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        (
            "onboarding_announce_token",
            body.announce_token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        (
            "onboarding_notify_token",
            body.notify_token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        (
            "onboarding_command_token_2",
            body.command_token_2
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        (
            "onboarding_command_provider_2",
            body.command_provider_2
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ),
        ("onboarding_complete", Some("true")),
    ] {
        match value {
            Some(value) => {
                sqlx::query(
                    "INSERT INTO kv_meta (key, value)
                     VALUES ($1, $2)
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, expires_at = NULL",
                )
                .bind(key)
                .bind(value)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("failed to persist postgres kv_meta {key}: {error}"))?;
            }
            None => {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| format!("failed to clear postgres kv_meta {key}: {error}"))?;
            }
        }
    }

    for mapping in resolved_channels {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id, description, system_prompt, status, xp)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 0)
             ON CONFLICT (id) DO UPDATE SET
               name = COALESCE(EXCLUDED.name, agents.name),
               provider = COALESCE(EXCLUDED.provider, agents.provider),
               discord_channel_id = EXCLUDED.discord_channel_id,
               description = COALESCE(EXCLUDED.description, agents.description),
               system_prompt = COALESCE(EXCLUDED.system_prompt, agents.system_prompt),
               updated_at = NOW()",
        )
        .bind(&mapping.role_id)
        .bind(&mapping.role_id)
        .bind(provider)
        .bind(&mapping.channel_id)
        .bind(&mapping.description)
        .bind(&mapping.system_prompt)
        .bind(ONBOARDING_NEW_AGENT_STATUS)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("failed to upsert postgres agent {}: {error}", mapping.role_id))?;
    }

    if !resolved_channels.is_empty() {
        let (template_name, template_name_ko, template_icon, template_color) =
            match body.template.as_deref() {
                Some("delivery") => ("Delivery Squad", "전달 스쿼드", "🚀", "#8b5cf6"),
                Some("operations") => ("Operations Cell", "운영 셀", "🛠️", "#10b981"),
                Some("insight") => ("Insight Desk", "인사이트 데스크", "📚", "#3b82f6"),
                _ => ("General", "일반", "📁", "#6b7280"),
            };

        let office_id = "hq";
        sqlx::query(
            "INSERT INTO offices (id, name, name_ko, icon)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(office_id)
        .bind("Headquarters")
        .bind("본사")
        .bind("🏛️")
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("failed to upsert postgres default office: {error}"))?;

        let dept_id = body.template.as_deref().unwrap_or("general").to_string();
        sqlx::query(
            "INSERT INTO departments (id, name, name_ko, icon, color, office_id, sort_order)
             VALUES ($1, $2, $3, $4, $5, $6, 0)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&dept_id)
        .bind(template_name)
        .bind(template_name_ko)
        .bind(template_icon)
        .bind(template_color)
        .bind(office_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("failed to upsert postgres onboarding department: {error}"))?;

        for mapping in resolved_channels {
            sqlx::query(
                "INSERT INTO office_agents (office_id, agent_id, department_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (office_id, agent_id)
                 DO UPDATE SET department_id = EXCLUDED.department_id",
            )
            .bind(office_id)
            .bind(&mapping.role_id)
            .bind(&dept_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!(
                    "failed to assign postgres office agent {}: {error}",
                    mapping.role_id
                )
            })?;

            sqlx::query("UPDATE agents SET department = $1, updated_at = NOW() WHERE id = $2")
                .bind(&dept_id)
                .bind(&mapping.role_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    format!(
                        "failed to set postgres agent department {}: {error}",
                        mapping.role_id
                    )
                })?;
        }
    }

    tx.commit()
        .await
        .map_err(|error| format!("failed to commit postgres onboarding transaction: {error}"))
}

/// POST /api/onboarding/complete
/// Saves onboarding configuration and sets up agents.
pub async fn complete(
    state: &AppState,
    body: CompleteBody,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, response) =
        complete_with_options(state, &body, &CompleteExecutionOptions::default()).await;
    (status, Json(response))
}

/// Shared error context threaded into every phase helper so that an early
/// return can rebuild the exact same `completion_response` the monolithic
/// implementation produced. Holds the request-derived metadata that is
/// constant once the request has been validated.
struct CompleteErrorContext<'a> {
    provider: &'a str,
    rerun_policy: OnboardingRerunPolicy,
    explicit_rerun_policy: bool,
}

impl<'a> CompleteErrorContext<'a> {
    fn error(
        &self,
        status: StatusCode,
        completion_state: Option<&OnboardingCompletionState>,
        error: Option<String>,
        conflicts: Vec<String>,
    ) -> (StatusCode, serde_json::Value) {
        completion_response(
            status,
            false,
            self.provider,
            self.rerun_policy,
            self.explicit_rerun_policy,
            completion_state,
            error,
            conflicts,
            serde_json::Map::new(),
        )
    }
}

/// Records `error` on the completion state, best-effort persists it, and returns
/// the matching INTERNAL_SERVER_ERROR response. Mirrors the repeated
/// "set last_error + save + respond" tail used throughout the persistence phase.
fn fail_and_persist_completion_state(
    ctx: &CompleteErrorContext<'_>,
    root: &Path,
    completion_state: &mut OnboardingCompletionState,
    error: String,
) -> (StatusCode, serde_json::Value) {
    completion_state.last_error = Some(error);
    let _ = save_onboarding_completion_state(root, completion_state);
    ctx.error(
        StatusCode::INTERNAL_SERVER_ERROR,
        Some(completion_state),
        completion_state.last_error.clone(),
        Vec::new(),
    )
}

/// Validates the request payload and parses the request-derived metadata.
/// Returns the rerun policy, whether it was explicitly supplied, and the
/// channel fingerprint, or a ready-to-return BAD_REQUEST response. Pure: no
/// filesystem or network side effects.
fn validate_complete_request(
    body: &CompleteBody,
    provider: &str,
) -> Result<(OnboardingRerunPolicy, bool, String), (StatusCode, serde_json::Value)> {
    let bad_request = |rerun_policy, explicit, error: String| {
        completion_response(
            StatusCode::BAD_REQUEST,
            false,
            provider,
            rerun_policy,
            explicit,
            None,
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        )
    };

    if body.guild_id.trim().is_empty() {
        return Err(bad_request(
            OnboardingRerunPolicy::ReuseExisting,
            false,
            "guild_id is required for onboarding completion".to_string(),
        ));
    }
    if let Err(error) = parse_owner_id(body.owner_id.as_deref()) {
        return Err(bad_request(
            OnboardingRerunPolicy::ReuseExisting,
            false,
            error,
        ));
    }
    let explicit_rerun_policy = body
        .rerun_policy
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    let rerun_policy = match OnboardingRerunPolicy::parse(body.rerun_policy.as_deref()) {
        Ok(policy) => policy,
        Err(error) => {
            return Err(bad_request(
                OnboardingRerunPolicy::ReuseExisting,
                explicit_rerun_policy,
                error,
            ));
        }
    };
    let request_fingerprint = match requested_channel_fingerprint(body, provider) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            return Err(bad_request(rerun_policy, explicit_rerun_policy, error));
        }
    };
    Ok((rerun_policy, explicit_rerun_policy, request_fingerprint))
}

/// Resolves the runtime root and ensures the runtime layout exists. Returns the
/// root path or a ready-to-return INTERNAL_SERVER_ERROR response.
fn prepare_complete_runtime_root(
    ctx: &CompleteErrorContext<'_>,
) -> Result<PathBuf, (StatusCode, serde_json::Value)> {
    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return Err(ctx.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
            Some("cannot determine runtime root".to_string()),
            Vec::new(),
        ));
    };
    if let Err(error) = crate::runtime_layout::ensure_runtime_layout(&root) {
        return Err(ctx.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
            Some(format!("failed to prepare runtime layout: {error}")),
            Vec::new(),
        ));
    }
    Ok(root)
}

/// Loads any existing completion state and guards against a partial apply that
/// targets a different channel plan. Returns the loaded state or a
/// ready-to-return error response.
fn load_existing_completion_for_request(
    ctx: &CompleteErrorContext<'_>,
    root: &Path,
    request_fingerprint: &str,
) -> Result<Option<OnboardingCompletionState>, (StatusCode, serde_json::Value)> {
    let existing_completion_state = match load_onboarding_completion_state(root) {
        Ok(state) => state,
        Err(error) => {
            return Err(ctx.error(
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                Some(error),
                Vec::new(),
            ));
        }
    };

    if let Some(existing_state) = existing_completion_state
        .as_ref()
        .filter(|state| state.partial_apply && state.request_fingerprint != request_fingerprint)
    {
        return Err(ctx.error(
            StatusCode::CONFLICT,
            Some(existing_state),
            Some(
                "an incomplete onboarding attempt exists for a different channel plan; retry the same payload or reset the previous partial apply before changing channel mappings".to_string(),
            ),
            Vec::new(),
        ));
    }

    Ok(existing_completion_state)
}

/// Resolves every requested channel mapping (reusing checkpoints where possible)
/// and validates that the resolved channels are unique. Returns the resolved
/// channels or a ready-to-return BAD_REQUEST response.
async fn resolve_complete_channels(
    ctx: &CompleteErrorContext<'_>,
    body: &CompleteBody,
    options: &CompleteExecutionOptions,
    discord_token: &str,
    existing_completion_state: Option<&OnboardingCompletionState>,
    checkpoint_state: Option<&OnboardingCompletionState>,
) -> Result<Vec<ResolvedChannelMapping>, (StatusCode, serde_json::Value)> {
    let client = reqwest::Client::new();
    let mut resolved_channels = Vec::with_capacity(body.channels.len());
    for mapping in &body.channels {
        let checkpoint = checkpoint_state.and_then(|state| {
            let requested_name = desired_channel_name(mapping).ok()?;
            state.channels.iter().find(|channel| {
                channel.role_id == mapping.role_id
                    && channel.requested_channel_name == requested_name
            })
        });
        let resolved = match resolve_channel_mapping(
            &client,
            discord_token,
            &options.discord_api_base,
            &body.guild_id,
            mapping,
            checkpoint,
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                return Err(ctx.error(
                    StatusCode::BAD_REQUEST,
                    existing_completion_state,
                    Some(format!(
                        "failed to resolve channel for agent '{}': {}",
                        mapping.role_id, error
                    )),
                    Vec::new(),
                ));
            }
        };
        resolved_channels.push(resolved);
    }

    if let Err(error) = validate_unique_resolved_channels(&resolved_channels) {
        return Err(ctx.error(
            StatusCode::BAD_REQUEST,
            existing_completion_state,
            Some(error),
            Vec::new(),
        ));
    }

    Ok(resolved_channels)
}

/// Persists the filesystem artifacts (config/workspace dirs, role map, channel
/// bindings, discord config, credentials) and verifies the settings and
/// pipeline artifacts. On any failure the error is recorded on
/// `completion_state`, best-effort persisted, and a ready-to-return response is
/// produced. On success returns the settings and pipeline verification reports.
fn persist_complete_filesystem_artifacts(
    ctx: &CompleteErrorContext<'_>,
    root: &Path,
    body: &CompleteBody,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
    completion_state: &mut OnboardingCompletionState,
) -> Result<(serde_json::Value, serde_json::Value), (StatusCode, serde_json::Value)> {
    let config_dir = crate::runtime_layout::config_dir(root);
    if let Err(error) = std::fs::create_dir_all(&config_dir) {
        return Err(fail_and_persist_completion_state(
            ctx,
            root,
            completion_state,
            format!(
                "failed to create config dir {}: {error}",
                config_dir.display()
            ),
        ));
    }

    let workspaces_dir = root.join("workspaces");
    if let Err(error) = std::fs::create_dir_all(&workspaces_dir) {
        return Err(fail_and_persist_completion_state(
            ctx,
            root,
            completion_state,
            format!(
                "failed to create workspaces dir {}: {error}",
                workspaces_dir.display()
            ),
        ));
    }
    for mapping in resolved_channels {
        let ws_dir = workspaces_dir.join(&mapping.role_id);
        if let Err(error) = std::fs::create_dir_all(&ws_dir) {
            return Err(fail_and_persist_completion_state(
                ctx,
                root,
                completion_state,
                format!("failed to create workspace {}: {error}", ws_dir.display()),
            ));
        }
    }

    if let Err(error) = write_onboarding_role_map(root, provider, resolved_channels) {
        return Err(fail_and_persist_completion_state(
            ctx,
            root,
            completion_state,
            error,
        ));
    }

    if let Err(error) = write_agentdesk_channel_bindings(root, provider, resolved_channels) {
        return Err(fail_and_persist_completion_state(
            ctx,
            root,
            completion_state,
            format!("failed to write agentdesk.yaml: {error}"),
        ));
    }

    if let Err(error) = write_agentdesk_discord_config(
        root,
        &body.guild_id,
        &body.token,
        provider,
        body.command_token_2.as_deref(),
        body.command_provider_2.as_deref(),
        body.owner_id.as_deref(),
    ) {
        return Err(fail_and_persist_completion_state(
            ctx,
            root,
            completion_state,
            format!("failed to write agentdesk.yaml discord config: {error}"),
        ));
    }

    if let Err(error) = write_credential_token(root, "announce", body.announce_token.as_deref()) {
        return Err(fail_and_persist_completion_state(
            ctx,
            root,
            completion_state,
            format!("failed to write announce credential: {error}"),
        ));
    }

    if let Err(error) = write_credential_token(root, "notify", body.notify_token.as_deref()) {
        return Err(fail_and_persist_completion_state(
            ctx,
            root,
            completion_state,
            format!("failed to write notify credential: {error}"),
        ));
    }

    let settings_report = match verify_onboarding_settings_artifacts(
        root,
        &body.token,
        provider,
        body.command_token_2.as_deref(),
        body.command_provider_2.as_deref(),
        &body.guild_id,
        body.owner_id.as_deref(),
        body.announce_token.as_deref(),
        body.notify_token.as_deref(),
        resolved_channels,
    ) {
        Ok(report) => report,
        Err(error) => {
            return Err(fail_and_persist_completion_state(
                ctx,
                root,
                completion_state,
                format!("onboarding settings verification failed: {error}"),
            ));
        }
    };

    let pipeline_report = match verify_onboarding_pipeline_artifact(root) {
        Ok(report) => report,
        Err(error) => {
            return Err(fail_and_persist_completion_state(
                ctx,
                root,
                completion_state,
                format!("onboarding pipeline verification failed: {error}"),
            ));
        }
    };

    Ok((settings_report, pipeline_report))
}

async fn complete_with_options(
    state: &AppState,
    body: &CompleteBody,
    options: &CompleteExecutionOptions,
) -> (StatusCode, serde_json::Value) {
    let provider = body.provider.as_deref().unwrap_or("claude");

    let (rerun_policy, explicit_rerun_policy, request_fingerprint) =
        match validate_complete_request(body, provider) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
    let ctx = CompleteErrorContext {
        provider,
        rerun_policy,
        explicit_rerun_policy,
    };

    let discord_token = body
        .announce_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(body.token.as_str());

    let root = match prepare_complete_runtime_root(&ctx) {
        Ok(root) => root,
        Err(response) => return response,
    };

    let existing_completion_state =
        match load_existing_completion_for_request(&ctx, &root, &request_fingerprint) {
            Ok(state) => state,
            Err(response) => return response,
        };

    let checkpoint_state = existing_completion_state
        .as_ref()
        .filter(|state| state.request_fingerprint == request_fingerprint);

    let resolved_channels = match resolve_complete_channels(
        &ctx,
        body,
        options,
        discord_token,
        existing_completion_state.as_ref(),
        checkpoint_state,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };

    let channels_created = resolved_channels
        .iter()
        .filter(|mapping| mapping.resolution == ChannelResolutionKind::CreatedChannel)
        .count();
    let checkpoint_reused = resolved_channels
        .iter()
        .filter(|mapping| mapping.resolution == ChannelResolutionKind::Checkpoint)
        .count();
    let has_partial_apply = channels_created > 0
        || checkpoint_state
            .map(|state| state.partial_apply)
            .unwrap_or(false);

    let mut completion_state = build_onboarding_completion_state(
        &request_fingerprint,
        &body.guild_id,
        provider,
        rerun_policy,
        OnboardingCompletionStage::ChannelsResolved,
        has_partial_apply,
        has_partial_apply,
        None,
        &resolved_channels,
    );
    if let Err(error) = save_onboarding_completion_state(&root, &completion_state) {
        return ctx.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some(&completion_state),
            Some(error),
            Vec::new(),
        );
    }

    if options.fail_after_stage == Some(OnboardingCompletionStage::ChannelsResolved) {
        let error = format!(
            "test failpoint triggered after stage {}",
            OnboardingCompletionStage::ChannelsResolved.as_str()
        );
        completion_state.last_error = Some(error.clone());
        completion_state.retry_recommended = true;
        if let Err(save_error) = save_onboarding_completion_state(&root, &completion_state) {
            return ctx.error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(&completion_state),
                Some(format!(
                    "{error}; additionally failed to persist completion state: {save_error}"
                )),
                Vec::new(),
            );
        }
        return ctx.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some(&completion_state),
            Some(error),
            Vec::new(),
        );
    }

    let conflicts = if let Some(pool) = state.pg_pool_ref() {
        collect_onboarding_conflicts_pg(pool, &root, provider, &resolved_channels, rerun_policy)
            .await
    } else {
        Err("Postgres pool is required to check onboarding database conflicts".to_string())
    };

    let conflicts = match conflicts {
        Ok(conflicts) => conflicts,
        Err(error) => {
            completion_state.last_error = Some(error.clone());
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return ctx.error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(&completion_state),
                Some(error),
                Vec::new(),
            );
        }
    };
    if !conflicts.is_empty() {
        let error = "onboarding rerun would overwrite existing agent/channel bindings; re-run with rerun_policy=replace_existing only if you intend to replace them".to_string();
        completion_state.last_error = Some(error.clone());
        completion_state.retry_recommended = false;
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return ctx.error(
            StatusCode::CONFLICT,
            Some(&completion_state),
            Some(error),
            conflicts,
        );
    }

    let (settings_report, pipeline_report) = match persist_complete_filesystem_artifacts(
        &ctx,
        &root,
        body,
        provider,
        &resolved_channels,
        &mut completion_state,
    ) {
        Ok(reports) => reports,
        Err(response) => return response,
    };

    completion_state = build_onboarding_completion_state(
        &request_fingerprint,
        &body.guild_id,
        provider,
        rerun_policy,
        OnboardingCompletionStage::ArtifactsPersisted,
        true,
        true,
        None,
        &resolved_channels,
    );
    if let Err(error) = save_onboarding_completion_state(&root, &completion_state) {
        return ctx.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some(&completion_state),
            Some(error),
            Vec::new(),
        );
    }

    if options.fail_after_stage == Some(OnboardingCompletionStage::ArtifactsPersisted) {
        let error = format!(
            "test failpoint triggered after stage {}",
            OnboardingCompletionStage::ArtifactsPersisted.as_str()
        );
        completion_state.last_error = Some(error.clone());
        completion_state.retry_recommended = true;
        if let Err(save_error) = save_onboarding_completion_state(&root, &completion_state) {
            return ctx.error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(&completion_state),
                Some(format!(
                    "{error}; additionally failed to persist completion state: {save_error}"
                )),
                Vec::new(),
            );
        }
        return ctx.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some(&completion_state),
            Some(error),
            Vec::new(),
        );
    }

    if let Some(pool) = state.pg_pool_ref() {
        if let Err(error) = persist_onboarding_pg(pool, body, provider, &resolved_channels).await {
            return fail_and_persist_completion_state(&ctx, &root, &mut completion_state, error);
        }
    } else {
        return fail_and_persist_completion_state(
            &ctx,
            &root,
            &mut completion_state,
            "Postgres pool is required to persist onboarding state".to_string(),
        );
    }

    completion_state = build_onboarding_completion_state(
        &request_fingerprint,
        &body.guild_id,
        provider,
        rerun_policy,
        OnboardingCompletionStage::Completed,
        false,
        false,
        None,
        &resolved_channels,
    );
    if let Err(error) = save_onboarding_completion_state(&root, &completion_state) {
        return ctx.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some(&completion_state),
            Some(error),
            Vec::new(),
        );
    }
    if let Err(error) = clear_onboarding_draft(&root) {
        tracing::warn!("failed to clear onboarding draft after completion: {error}");
    }

    let checklist = vec![
        json!({
            "key": "channels",
            "ok": true,
            "label": "Discord channels ready",
            "detail": format!(
                "{} channel mappings resolved ({} created, {} reused, {} checkpointed)",
                resolved_channels.len(),
                channels_created,
                resolved_channels.len().saturating_sub(channels_created + checkpoint_reused),
                checkpoint_reused,
            ),
        }),
        json!({
            "key": "settings",
            "ok": true,
            "label": "Settings persisted",
            "detail": format!(
                "agentdesk config, credentials, role-map, and {} workspaces verified",
                resolved_channels.len()
            ),
        }),
        json!({
            "key": "pipeline",
            "ok": true,
            "label": "Pipeline ready",
            "detail": format!(
                "default pipeline verified at {}",
                pipeline_report["path"].as_str().unwrap_or("(unknown)")
            ),
        }),
    ];

    let mut extra = serde_json::Map::new();
    extra.insert("agents_created".to_string(), json!(resolved_channels.len()));
    extra.insert("channels_created".to_string(), json!(channels_created));
    extra.insert("checklist".to_string(), json!(checklist));
    extra.insert(
        "artifacts".to_string(),
        json!({
            "settings": settings_report,
            "pipeline": pipeline_report,
            "channel_mappings": resolved_channels
                .iter()
                .map(|mapping| {
                    json!({
                        "role_id": mapping.role_id,
                        "channel_id": mapping.channel_id,
                        "channel_name": mapping.channel_name,
                        "requested_channel_name": mapping.requested_channel_name,
                        "created": mapping.created,
                        "resolution": mapping.resolution,
                    })
                })
                .collect::<Vec<_>>(),
        }),
    );

    completion_response(
        StatusCode::OK,
        true,
        provider,
        rerun_policy,
        explicit_rerun_policy,
        Some(&completion_state),
        None,
        Vec::new(),
        extra,
    )
}

#[cfg(test)]
mod canonical_status_tests {
    use super::*;

    #[test]
    fn new_agents_start_with_core_canonical_idle_status() {
        let migration = include_str!("../../../migrations/postgres/0089_drop_agent_archive.sql");

        assert_eq!(ONBOARDING_NEW_AGENT_STATUS, "idle");
        assert!(
            migration.contains("status IN ('idle', 'working')"),
            "onboarding must use a status allowed by agents_status_known_check"
        );
    }
}

// Provider check + AI prompt generation handlers moved to `provider` submodule.
