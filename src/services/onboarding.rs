use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::server::routes::AppState;
use crate::services::provider::ProviderKind;
use crate::services::provider_exec;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn legacy_db(state: &AppState) -> &crate::db::Db {
    state
        .engine
        .legacy_db()
        .or_else(|| state.legacy_db())
        .expect("legacy SQLite DB is only available in no-PG tests")
}

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const ONBOARDING_DRAFT_VERSION: u8 = 1;
const MAX_ONBOARDING_DRAFT_BYTES: usize = 128 * 1024;
const MAX_ONBOARDING_DRAFT_COMMAND_BOTS: usize = 4;
const MAX_ONBOARDING_DRAFT_AGENTS: usize = 64;
const MAX_ONBOARDING_DRAFT_CHANNEL_ASSIGNMENTS: usize = 64;
const MAX_ONBOARDING_DRAFT_PROVIDER_STATUSES: usize = 8;
const MAX_ONBOARDING_DRAFT_FUTURE_SKEW_MS: i64 = 5 * 60 * 1000;

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
pub async fn status(state: &AppState) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match status_pg(pool).await {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
    {
        return match status_config() {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        let conn = match legacy_db(state).lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };

        // Check whether onboarding created any agents yet.
        let has_bots: bool = conn
            .query_row("SELECT COUNT(*) > 0 FROM agents", [], |row| row.get(0))
            .unwrap_or(false);

        // Get existing config
        let bot_token: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_bot_token'",
                [],
                |row| row.get(0),
            )
            .ok();

        let guild_id: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_guild_id'",
                [],
                |row| row.get(0),
            )
            .ok();

        let owner_id = sanitize_legacy_owner_id(
            conn.query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_owner_id'",
                [],
                |row| row.get(0),
            )
            .ok(),
        );

        let agent_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM agents", [], |row| row.get(0))
            .unwrap_or(0);

        // Get channel mappings from agents table
        let mut stmt = conn
            .prepare("SELECT id, name, discord_channel_id FROM agents ORDER BY id")
            .unwrap();
        let agents: Vec<serde_json::Value> = stmt
            .query_map([], |row| {
                Ok(json!({
                    "agent_id": row.get::<_, String>(0)?,
                    "name": row.get::<_, Option<String>>(1)?,
                    "channel_id": row.get::<_, Option<String>>(2)?,
                }))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();

        // Load all bot tokens for pre-fill
        let announce_token: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_announce_token'",
                [],
                |row| row.get(0),
            )
            .ok();
        let notify_token: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_notify_token'",
                [],
                |row| row.get(0),
            )
            .ok();
        let command_token_2: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_command_token_2'",
                [],
                |row| row.get(0),
            )
            .ok();
        let primary_provider: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_provider'",
                [],
                |row| row.get(0),
            )
            .ok();
        let command_provider_2: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'onboarding_command_provider_2'",
                [],
                |row| row.get(0),
            )
            .ok();

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

        // Never return raw onboarding tokens from status.
        // This endpoint can be reachable without auth, so redact all token values.
        let redact = |_t: Option<String>| -> Option<String> { None };

        (
            StatusCode::OK,
            Json(json!({
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
            })),
        )
    }
}

async fn pg_kv_value(pool: &sqlx::PgPool, key: &str) -> Result<Option<String>, String> {
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

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
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
pub async fn draft_get(state: &AppState) -> (StatusCode, Json<serde_json::Value>) {
    let completed = if let Some(pool) = state.pg_pool_ref() {
        match onboarding_has_agents_pg(pool).await {
            Ok(completed) => completed,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    } else {
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        {
            crate::cli::agentdesk_runtime_root()
                .as_ref()
                .and_then(|root| load_onboarding_config(root).ok())
                .map(|config| config.discord.guild_id.is_some() && !config.agents.is_empty())
                .unwrap_or(false)
        }
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            match legacy_db(state).lock() {
                Ok(conn) => conn
                    .query_row("SELECT COUNT(*) > 0 FROM agents", [], |row| row.get(0))
                    .unwrap_or(false),
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }
            }
        }
    };

    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "cannot determine runtime root"})),
        );
    };

    let draft = match load_onboarding_draft(&root) {
        Ok(draft) => draft,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    }
    .map(OnboardingDraft::redact_secrets);
    let completion_state = match load_onboarding_completion_state(&root) {
        Ok(state) => state,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };
    let available = draft.is_some();

    (
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
    )
}

/// PUT /api/onboarding/draft
/// Persists the in-progress onboarding draft required to resume across browsers.
pub async fn draft_put(body: OnboardingDraft) -> (StatusCode, Json<serde_json::Value>) {
    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "cannot determine runtime root"})),
        );
    };

    if let Err(error) = crate::runtime_layout::ensure_runtime_layout(&root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to prepare runtime layout: {error}")})),
        );
    }

    let draft = match body.normalize() {
        Ok(draft) => draft,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    let draft = draft.redact_secrets();

    if let Err(error) = save_onboarding_draft(&root, &draft) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "available": true,
            "draft": draft,
            "secret_policy": onboarding_draft_secret_policy_value(),
        })),
    )
}

/// DELETE /api/onboarding/draft
/// Explicitly removes the in-progress onboarding draft.
pub async fn draft_delete() -> (StatusCode, Json<serde_json::Value>) {
    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "cannot determine runtime root"})),
        );
    };

    if let Err(error) = clear_onboarding_draft(&root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "available": false,
            "secret_policy": onboarding_draft_secret_policy_value(),
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct ValidateTokenBody {
    pub token: String,
}

/// POST /api/onboarding/validate-token
/// Validates a Discord bot token and returns bot info.
pub async fn validate_token(body: ValidateTokenBody) -> (StatusCode, Json<serde_json::Value>) {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://discord.com/api/v10/users/@me")
        .header("Authorization", format!("Bot {}", body.token))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let user: serde_json::Value = r.json().await.unwrap_or(json!({}));
            (
                StatusCode::OK,
                Json(json!({
                    "valid": true,
                    "bot_id": user.get("id").and_then(|v| v.as_str()),
                    "bot_name": user.get("username").and_then(|v| v.as_str()),
                    "avatar": user.get("avatar").and_then(|v| v.as_str()),
                })),
            )
        }
        Ok(r) => {
            let status = r.status();
            (
                StatusCode::OK,
                Json(json!({
                    "valid": false,
                    "error": format!("Discord API error: {status}"),
                })),
            )
        }
        Err(e) => (
            StatusCode::OK,
            Json(json!({
                "valid": false,
                "error": format!("Request failed: {e}"),
            })),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct ChannelsQuery {
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelsBody {
    pub token: Option<String>,
}

async fn load_channels(
    state: &AppState,
    token: Option<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Use provided token or saved token
    let token = match token {
        Some(token) => Some(token),
        None if state.pg_pool_ref().is_some() => {
            match pg_kv_value(
                state.pg_pool_ref().expect("checked pg_pool_ref"),
                "onboarding_bot_token",
            )
            .await
            {
                Ok(token) => token,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": error})),
                    );
                }
            }
        }
        None => saved_onboarding_bot_token_without_pg(state),
    };

    let Some(token) = token else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "No token provided"})),
        );
    };

    let client = reqwest::Client::new();

    // Fetch guilds
    let guilds: Vec<serde_json::Value> = match client
        .get("https://discord.com/api/v10/users/@me/guilds")
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => {
            return (
                StatusCode::OK,
                Json(json!({"guilds": [], "error": "Failed to fetch guilds"})),
            );
        }
    };

    let mut result_guilds = Vec::new();
    for guild in &guilds {
        let guild_id = guild.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let guild_name = guild.get("name").and_then(|v| v.as_str()).unwrap_or("");

        // Fetch channels for this guild
        let channels: Vec<serde_json::Value> = match client
            .get(format!(
                "https://discord.com/api/v10/guilds/{guild_id}/channels"
            ))
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => Vec::new(),
        };

        // Filter text channels (type 0)
        let text_channels: Vec<serde_json::Value> = channels
            .into_iter()
            .filter(|c| c.get("type").and_then(|v| v.as_i64()) == Some(0))
            .map(|c| {
                let parent = c
                    .get("parent_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                json!({
                    "id": c.get("id").and_then(|v| v.as_str()),
                    "name": c.get("name").and_then(|v| v.as_str()),
                    "category_id": parent,
                })
            })
            .collect();

        result_guilds.push(json!({
            "id": guild_id,
            "name": guild_name,
            "channels": text_channels,
        }));
    }

    (StatusCode::OK, Json(json!({"guilds": result_guilds})))
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
fn saved_onboarding_bot_token_without_pg(_state: &AppState) -> Option<String> {
    crate::cli::agentdesk_runtime_root()
        .as_ref()
        .and_then(|root| load_onboarding_config(root).ok())
        .and_then(|config| {
            config
                .discord
                .bots
                .get("command")
                .and_then(|bot| bot.token.clone())
        })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn saved_onboarding_bot_token_without_pg(state: &AppState) -> Option<String> {
    legacy_db(state).lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT value FROM kv_meta WHERE key = 'onboarding_bot_token'",
            [],
            |row| row.get(0),
        )
        .ok()
    })
}

/// GET /api/onboarding/channels
/// Fetches Discord guilds + text channels for the given bot token.
pub async fn channels(
    state: &AppState,
    query: ChannelsQuery,
) -> (StatusCode, Json<serde_json::Value>) {
    load_channels(state, query.token).await
}

/// POST /api/onboarding/channels
/// Fetches Discord guilds + text channels for the given bot token from request body.
pub async fn channels_post(
    state: &AppState,
    body: ChannelsBody,
) -> (StatusCode, Json<serde_json::Value>) {
    load_channels(state, body.token).await
}

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
        Some(value) => std::fs::write(path, format!("{value}\n")).map_err(|e| e.to_string()),
        None => {
            if path.exists() {
                std::fs::remove_file(path).map_err(|e| e.to_string())?;
            }
            Ok(())
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

fn load_onboarding_config(runtime_root: &Path) -> Result<crate::config::Config, String> {
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn collect_onboarding_conflicts(
    conn: &sqlite_test::Connection,
    runtime_root: &Path,
    provider: &str,
    resolved_channels: &[ResolvedChannelMapping],
    rerun_policy: OnboardingRerunPolicy,
) -> Result<Vec<String>, String> {
    use sqlite_test::OptionalExtension;

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
        let existing_agent = conn
            .query_row(
                "SELECT provider, discord_channel_id, description, system_prompt \
                 FROM agents WHERE id = ?1",
                [mapping.role_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| format!("failed to query agent {}: {e}", mapping.role_id))?;

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

        let conflicting_db_channel_owner = conn
            .query_row(
                "SELECT id FROM agents WHERE discord_channel_id = ?1 AND id != ?2 LIMIT 1",
                sqlite_test::params![mapping.channel_id, mapping.role_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| {
                format!(
                    "failed to check existing DB channel owner {}: {e}",
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
                provider: provider.to_string(),
                channels: crate::config::AgentChannels::default(),
                keywords: Vec::new(),
                department: None,
                avatar_emoji: None,
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
             VALUES ($1, $2, $3, $4, $5, $6, 'active', 0)
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

async fn complete_with_options(
    state: &AppState,
    body: &CompleteBody,
    options: &CompleteExecutionOptions,
) -> (StatusCode, serde_json::Value) {
    let provider = body.provider.as_deref().unwrap_or("claude");
    if body.guild_id.trim().is_empty() {
        return completion_response(
            StatusCode::BAD_REQUEST,
            false,
            provider,
            OnboardingRerunPolicy::ReuseExisting,
            false,
            None,
            Some("guild_id is required for onboarding completion".to_string()),
            Vec::new(),
            serde_json::Map::new(),
        );
    }
    if let Err(error) = parse_owner_id(body.owner_id.as_deref()) {
        return completion_response(
            StatusCode::BAD_REQUEST,
            false,
            provider,
            OnboardingRerunPolicy::ReuseExisting,
            false,
            None,
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
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
            return completion_response(
                StatusCode::BAD_REQUEST,
                false,
                provider,
                OnboardingRerunPolicy::ReuseExisting,
                explicit_rerun_policy,
                None,
                Some(error),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };
    let request_fingerprint = match requested_channel_fingerprint(body, provider) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            return completion_response(
                StatusCode::BAD_REQUEST,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                None,
                Some(error),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };
    let discord_token = body
        .announce_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(body.token.as_str());

    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            None,
            Some("cannot determine runtime root".to_string()),
            Vec::new(),
            serde_json::Map::new(),
        );
    };

    if let Err(error) = crate::runtime_layout::ensure_runtime_layout(&root) {
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            None,
            Some(format!("failed to prepare runtime layout: {error}")),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let existing_completion_state = match load_onboarding_completion_state(&root) {
        Ok(state) => state,
        Err(error) => {
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                None,
                Some(error),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };

    if let Some(existing_state) = existing_completion_state
        .as_ref()
        .filter(|state| state.partial_apply && state.request_fingerprint != request_fingerprint)
    {
        return completion_response(
            StatusCode::CONFLICT,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(existing_state),
            Some(
                "an incomplete onboarding attempt exists for a different channel plan; retry the same payload or reset the previous partial apply before changing channel mappings".to_string(),
            ),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let checkpoint_state = existing_completion_state
        .as_ref()
        .filter(|state| state.request_fingerprint == request_fingerprint);

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
                return completion_response(
                    StatusCode::BAD_REQUEST,
                    false,
                    provider,
                    rerun_policy,
                    explicit_rerun_policy,
                    existing_completion_state.as_ref(),
                    Some(format!(
                        "failed to resolve channel for agent '{}': {}",
                        mapping.role_id, error
                    )),
                    Vec::new(),
                    serde_json::Map::new(),
                );
            }
        };
        resolved_channels.push(resolved);
    }

    if let Err(error) = validate_unique_resolved_channels(&resolved_channels) {
        return completion_response(
            StatusCode::BAD_REQUEST,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            existing_completion_state.as_ref(),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

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
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
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
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                Some(format!(
                    "{error}; additionally failed to persist completion state: {save_error}"
                )),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let conflicts = if let Some(pool) = state.pg_pool_ref() {
        collect_onboarding_conflicts_pg(pool, &root, provider, &resolved_channels, rerun_policy)
            .await
    } else {
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        {
            Err("Postgres pool is required to check onboarding database conflicts".to_string())
        }
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            let conn = match legacy_db(state).lock() {
                Ok(conn) => conn,
                Err(error) => {
                    completion_state.last_error = Some(format!("{error}"));
                    let _ = save_onboarding_completion_state(&root, &completion_state);
                    return completion_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        false,
                        provider,
                        rerun_policy,
                        explicit_rerun_policy,
                        Some(&completion_state),
                        Some(format!("{error}")),
                        Vec::new(),
                        serde_json::Map::new(),
                    );
                }
            };
            collect_onboarding_conflicts(&conn, &root, provider, &resolved_channels, rerun_policy)
        }
    };

    let conflicts = match conflicts {
        Ok(conflicts) => conflicts,
        Err(error) => {
            completion_state.last_error = Some(error.clone());
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                Some(error),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };
    if !conflicts.is_empty() {
        let error = "onboarding rerun would overwrite existing agent/channel bindings; re-run with rerun_policy=replace_existing only if you intend to replace them".to_string();
        completion_state.last_error = Some(error.clone());
        completion_state.retry_recommended = false;
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::CONFLICT,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            conflicts,
            serde_json::Map::new(),
        );
    }

    let config_dir = crate::runtime_layout::config_dir(&root);
    if let Err(error) = std::fs::create_dir_all(&config_dir) {
        completion_state.last_error = Some(format!(
            "failed to create config dir {}: {error}",
            config_dir.display()
        ));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let workspaces_dir = root.join("workspaces");
    if let Err(error) = std::fs::create_dir_all(&workspaces_dir) {
        completion_state.last_error = Some(format!(
            "failed to create workspaces dir {}: {error}",
            workspaces_dir.display()
        ));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }
    for mapping in &resolved_channels {
        let ws_dir = workspaces_dir.join(&mapping.role_id);
        if let Err(error) = std::fs::create_dir_all(&ws_dir) {
            completion_state.last_error = Some(format!(
                "failed to create workspace {}: {error}",
                ws_dir.display()
            ));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    }

    if let Err(error) = write_onboarding_role_map(&root, provider, &resolved_channels) {
        completion_state.last_error = Some(error);
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Err(error) = write_agentdesk_channel_bindings(&root, provider, &resolved_channels) {
        completion_state.last_error = Some(format!("failed to write agentdesk.yaml: {error}"));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Err(error) = write_agentdesk_discord_config(
        &root,
        &body.guild_id,
        &body.token,
        provider,
        body.command_token_2.as_deref(),
        body.command_provider_2.as_deref(),
        body.owner_id.as_deref(),
    ) {
        completion_state.last_error = Some(format!(
            "failed to write agentdesk.yaml discord config: {error}"
        ));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Err(error) = write_credential_token(&root, "announce", body.announce_token.as_deref()) {
        completion_state.last_error = Some(format!("failed to write announce credential: {error}"));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Err(error) = write_credential_token(&root, "notify", body.notify_token.as_deref()) {
        completion_state.last_error = Some(format!("failed to write notify credential: {error}"));
        let _ = save_onboarding_completion_state(&root, &completion_state);
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            completion_state.last_error.clone(),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    let settings_report = match verify_onboarding_settings_artifacts(
        &root,
        &body.token,
        provider,
        body.command_token_2.as_deref(),
        body.command_provider_2.as_deref(),
        &body.guild_id,
        body.owner_id.as_deref(),
        body.announce_token.as_deref(),
        body.notify_token.as_deref(),
        &resolved_channels,
    ) {
        Ok(report) => report,
        Err(error) => {
            completion_state.last_error =
                Some(format!("onboarding settings verification failed: {error}"));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    };

    let pipeline_report = match verify_onboarding_pipeline_artifact(&root) {
        Ok(report) => report,
        Err(error) => {
            completion_state.last_error =
                Some(format!("onboarding pipeline verification failed: {error}"));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
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
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
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
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                Some(format!(
                    "{error}; additionally failed to persist completion state: {save_error}"
                )),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
        );
    }

    if let Some(pool) = state.pg_pool_ref() {
        if let Err(error) = persist_onboarding_pg(pool, body, provider, &resolved_channels).await {
            completion_state.last_error = Some(error);
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
    } else {
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        {
            completion_state.last_error =
                Some("Postgres pool is required to persist onboarding state".to_string());
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            let mut conn = match legacy_db(state).lock() {
                Ok(conn) => conn,
                Err(error) => {
                    completion_state.last_error = Some(format!("{error}"));
                    let _ = save_onboarding_completion_state(&root, &completion_state);
                    return completion_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        false,
                        provider,
                        rerun_policy,
                        explicit_rerun_policy,
                        Some(&completion_state),
                        Some(format!("{error}")),
                        Vec::new(),
                        serde_json::Map::new(),
                    );
                }
            };

            let tx = match conn.transaction() {
                Ok(tx) => tx,
                Err(error) => {
                    completion_state.last_error =
                        Some(format!("failed to start onboarding transaction: {error}"));
                    let _ = save_onboarding_completion_state(&root, &completion_state);
                    return completion_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        false,
                        provider,
                        rerun_policy,
                        explicit_rerun_policy,
                        Some(&completion_state),
                        completion_state.last_error.clone(),
                        Vec::new(),
                        serde_json::Map::new(),
                    );
                }
            };

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
                        if let Err(error) = tx.execute(
                            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                            sqlite_test::params![key, value],
                        ) {
                            completion_state.last_error =
                                Some(format!("failed to persist kv_meta {}: {error}", key));
                            let _ = save_onboarding_completion_state(&root, &completion_state);
                            return completion_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                false,
                                provider,
                                rerun_policy,
                                explicit_rerun_policy,
                                Some(&completion_state),
                                completion_state.last_error.clone(),
                                Vec::new(),
                                serde_json::Map::new(),
                            );
                        }
                    }
                    None => {
                        if let Err(error) = tx.execute("DELETE FROM kv_meta WHERE key = ?1", [key])
                        {
                            completion_state.last_error =
                                Some(format!("failed to clear kv_meta {}: {error}", key));
                            let _ = save_onboarding_completion_state(&root, &completion_state);
                            return completion_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                false,
                                provider,
                                rerun_policy,
                                explicit_rerun_policy,
                                Some(&completion_state),
                                completion_state.last_error.clone(),
                                Vec::new(),
                                serde_json::Map::new(),
                            );
                        }
                    }
                }
            }

            for mapping in &resolved_channels {
                if let Err(error) = tx.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, description, system_prompt, status, xp) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 0) \
             ON CONFLICT(id) DO UPDATE SET \
               name = COALESCE(excluded.name, agents.name), \
               provider = COALESCE(excluded.provider, agents.provider), \
               discord_channel_id = excluded.discord_channel_id, \
               description = COALESCE(excluded.description, agents.description), \
               system_prompt = COALESCE(excluded.system_prompt, agents.system_prompt)",
            sqlite_test::params![
                mapping.role_id,
                mapping.role_id,
                provider,
                mapping.channel_id,
                mapping.description,
                mapping.system_prompt
            ],
        ) {
            completion_state.last_error =
                Some(format!("failed to upsert agent {}: {error}", mapping.role_id));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }
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
                if let Err(error) = tx.execute(
                "INSERT OR IGNORE INTO offices (id, name, name_ko, icon) VALUES (?1, ?2, ?3, ?4)",
                sqlite_test::params![office_id, "Headquarters", "본사", "🏛️"],
            ) {
                completion_state.last_error =
                    Some(format!("failed to upsert default office: {error}"));
                let _ = save_onboarding_completion_state(&root, &completion_state);
                return completion_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    false,
                    provider,
                    rerun_policy,
                    explicit_rerun_policy,
                    Some(&completion_state),
                    completion_state.last_error.clone(),
                    Vec::new(),
                    serde_json::Map::new(),
                );
            }

                let dept_id = body.template.as_deref().unwrap_or("general").to_string();
                if let Err(error) = tx.execute(
            "INSERT OR IGNORE INTO departments (id, name, name_ko, icon, color, office_id, sort_order) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            sqlite_test::params![
                dept_id,
                template_name,
                template_name_ko,
                template_icon,
                template_color,
                office_id,
            ],
        ) {
            completion_state.last_error =
                Some(format!("failed to upsert onboarding department: {error}"));
            let _ = save_onboarding_completion_state(&root, &completion_state);
            return completion_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
                provider,
                rerun_policy,
                explicit_rerun_policy,
                Some(&completion_state),
                completion_state.last_error.clone(),
                Vec::new(),
                serde_json::Map::new(),
            );
        }

                for mapping in &resolved_channels {
                    if let Err(error) = tx.execute(
                        "INSERT OR REPLACE INTO office_agents (office_id, agent_id, department_id) \
                 VALUES (?1, ?2, ?3)",
                        sqlite_test::params![office_id, mapping.role_id, dept_id],
                    ) {
                        completion_state.last_error = Some(format!(
                            "failed to assign office agent {}: {error}",
                            mapping.role_id
                        ));
                        let _ = save_onboarding_completion_state(&root, &completion_state);
                        return completion_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            false,
                            provider,
                            rerun_policy,
                            explicit_rerun_policy,
                            Some(&completion_state),
                            completion_state.last_error.clone(),
                            Vec::new(),
                            serde_json::Map::new(),
                        );
                    }
                    if let Err(error) = tx.execute(
                        "UPDATE agents SET department = ?1 WHERE id = ?2",
                        sqlite_test::params![dept_id, mapping.role_id],
                    ) {
                        completion_state.last_error = Some(format!(
                            "failed to set agent department {}: {error}",
                            mapping.role_id
                        ));
                        let _ = save_onboarding_completion_state(&root, &completion_state);
                        return completion_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            false,
                            provider,
                            rerun_policy,
                            explicit_rerun_policy,
                            Some(&completion_state),
                            completion_state.last_error.clone(),
                            Vec::new(),
                            serde_json::Map::new(),
                        );
                    }
                }
            }

            if let Err(error) = tx.commit() {
                completion_state.last_error =
                    Some(format!("failed to commit onboarding transaction: {error}"));
                let _ = save_onboarding_completion_state(&root, &completion_state);
                return completion_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    false,
                    provider,
                    rerun_policy,
                    explicit_rerun_policy,
                    Some(&completion_state),
                    completion_state.last_error.clone(),
                    Vec::new(),
                    serde_json::Map::new(),
                );
            }
        }
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
        return completion_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            false,
            provider,
            rerun_policy,
            explicit_rerun_policy,
            Some(&completion_state),
            Some(error),
            Vec::new(),
            serde_json::Map::new(),
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{
        Arc, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, body::Body, extract::Path as AxumPath, http::Request, routing::get};
    use tower::ServiceExt;

    fn env_guard() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    struct RuntimeRootGuard {
        _lock: MutexGuard<'static, ()>,
        previous: Option<std::path::PathBuf>,
    }

    impl RuntimeRootGuard {
        fn new(path: &Path) -> Self {
            let lock = env_guard();
            let previous = crate::config::current_test_runtime_root_override();
            crate::config::set_test_runtime_root_override(Some(path.to_path_buf()));
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            crate::config::set_test_runtime_root_override(self.previous.take());
        }
    }

    fn test_db() -> crate::db::Db {
        crate::db::test_db()
    }

    fn test_engine(db: &crate::db::Db) -> crate::engine::PolicyEngine {
        crate::engine::PolicyEngine::new_with_legacy_db(
            &crate::config::Config::default(),
            db.clone(),
        )
        .unwrap()
    }

    const VALID_OWNER_ID: &str = "123456789012345678";
    const VALID_OWNER_ID_U64: u64 = 123_456_789_012_345_678;

    fn sample_complete_body(
        channel_id: &str,
        channel_name: &str,
        rerun_policy: Option<&str>,
    ) -> CompleteBody {
        CompleteBody {
            token: "command-token".to_string(),
            announce_token: None,
            notify_token: None,
            command_token_2: None,
            command_provider_2: None,
            guild_id: "123".to_string(),
            owner_id: Some(VALID_OWNER_ID.to_string()),
            provider: Some("codex".to_string()),
            channels: vec![ChannelMapping {
                channel_id: channel_id.to_string(),
                channel_name: channel_name.to_string(),
                role_id: "adk-cdx".to_string(),
                description: Some("dispatch desk".to_string()),
                system_prompt: Some("be precise".to_string()),
            }],
            template: Some("operations".to_string()),
            rerun_policy: rerun_policy.map(str::to_string),
        }
    }

    fn sample_draft() -> OnboardingDraft {
        OnboardingDraft {
            version: ONBOARDING_DRAFT_VERSION,
            updated_at_ms: 1,
            step: 4,
            command_bots: vec![
                OnboardingDraftCommandBot {
                    provider: "codex".to_string(),
                    token: "command-token".to_string(),
                    bot_info: Some(OnboardingDraftBotInfo {
                        valid: true,
                        bot_id: Some("100".to_string()),
                        bot_name: Some("command".to_string()),
                        error: None,
                    }),
                },
                OnboardingDraftCommandBot {
                    provider: "claude".to_string(),
                    token: "command-token-2".to_string(),
                    bot_info: Some(OnboardingDraftBotInfo {
                        valid: true,
                        bot_id: Some("101".to_string()),
                        bot_name: Some("command-2".to_string()),
                        error: None,
                    }),
                },
            ],
            announce_token: "announce-token".to_string(),
            notify_token: "notify-token".to_string(),
            announce_bot_info: Some(OnboardingDraftBotInfo {
                valid: true,
                bot_id: Some("200".to_string()),
                bot_name: Some("announce".to_string()),
                error: None,
            }),
            notify_bot_info: Some(OnboardingDraftBotInfo {
                valid: true,
                bot_id: Some("300".to_string()),
                bot_name: Some("notify".to_string()),
                error: None,
            }),
            provider_statuses: BTreeMap::from([
                (
                    "codex".to_string(),
                    OnboardingDraftProviderStatus {
                        installed: true,
                        logged_in: true,
                        version: Some("1.2.3".to_string()),
                    },
                ),
                (
                    "claude".to_string(),
                    OnboardingDraftProviderStatus {
                        installed: true,
                        logged_in: false,
                        version: Some("9.9.9".to_string()),
                    },
                ),
            ]),
            selected_template: Some("operations".to_string()),
            agents: vec![OnboardingDraftAgent {
                id: "adk-cdx".to_string(),
                name: "Dispatch Desk".to_string(),
                name_en: Some("Dispatch Desk".to_string()),
                description: "dispatch desk".to_string(),
                description_en: Some("dispatch desk".to_string()),
                prompt: "be precise".to_string(),
                custom: true,
            }],
            custom_name: "Dispatch Desk".to_string(),
            custom_desc: "dispatch desk".to_string(),
            custom_name_en: "Dispatch Desk".to_string(),
            custom_desc_en: "dispatch desk".to_string(),
            expanded_agent: Some("adk-cdx".to_string()),
            selected_guild: "guild-123".to_string(),
            channel_assignments: vec![OnboardingDraftChannelAssignment {
                agent_id: "adk-cdx".to_string(),
                agent_name: "Dispatch Desk".to_string(),
                recommended_name: "adk-cdx-cdx".to_string(),
                channel_id: "1234".to_string(),
                channel_name: "dispatch-room".to_string(),
            }],
            owner_id: VALID_OWNER_ID.to_string(),
            has_existing_setup: false,
            confirm_rerun_overwrite: false,
        }
    }

    async fn spawn_mock_discord_server() -> (String, Arc<AtomicUsize>) {
        let post_count = Arc::new(AtomicUsize::new(0));
        let channels = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let channels_for_get = channels.clone();
        let channels_for_post = channels.clone();
        let post_count_for_route = post_count.clone();
        let app = Router::new().route(
            "/guilds/{guild_id}/channels",
            get(move |AxumPath(_guild_id): AxumPath<String>| {
                let channels = channels_for_get.clone();
                async move { Json(channels.lock().unwrap().clone()) }
            })
            .post(
                move |AxumPath(_guild_id): AxumPath<String>,
                      Json(body): Json<serde_json::Value>| {
                    let channels = channels_for_post.clone();
                    let post_count = post_count_for_route.clone();
                    async move {
                        let count = post_count.fetch_add(1, Ordering::SeqCst) + 1;
                        let created = json!({
                            "id": (1000 + count).to_string(),
                            "name": body.get("name").and_then(|value| value.as_str()).unwrap_or("created"),
                            "type": 0
                        });
                        channels.lock().unwrap().push(created.clone());
                        Json(created)
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{}", addr), post_count)
    }

    #[tokio::test]
    async fn draft_api_round_trip_redacts_tokens_and_exposes_resume_state() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::get(crate::server::routes::onboarding::draft_get)
                    .put(crate::server::routes::onboarding::draft_put)
                    .delete(draft_delete),
            )
            .route(
                "/status",
                axum::routing::get(crate::server::routes::onboarding::status),
            )
            .with_state(state);

        let put_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/draft")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&sample_draft()).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put_response.status(), StatusCode::OK);
        let put_body = axum::body::to_bytes(put_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let put_json: serde_json::Value = serde_json::from_slice(&put_body).unwrap();
        assert_eq!(put_json["ok"], json!(true));
        assert_eq!(put_json["draft"]["command_bots"][0]["token"], json!(""));
        assert_eq!(put_json["draft"]["announce_token"], json!(""));
        assert_eq!(put_json["draft"]["notify_token"], json!(""));
        assert_eq!(put_json["draft"]["updated_at_ms"], json!(1));
        assert_eq!(
            put_json["secret_policy"]["cleared_on_complete"],
            json!(true)
        );

        let status_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status_response.status(), StatusCode::OK);
        let status_body = axum::body::to_bytes(status_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
        assert_eq!(status_json["setup_mode"], json!("fresh"));
        assert_eq!(status_json["draft_available"], json!(true));
        assert_eq!(status_json["resume_state"], json!("draft_available"));

        let get_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/draft")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_response.status(), StatusCode::OK);
        let get_body = axum::body::to_bytes(get_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
        assert_eq!(get_json["available"], json!(true));
        assert_eq!(get_json["draft"]["command_bots"][0]["token"], json!(""));
        assert_eq!(get_json["draft"]["announce_token"], json!(""));
        assert_eq!(get_json["draft"]["notify_token"], json!(""));
        assert_eq!(get_json["draft"]["selected_template"], json!("operations"));
        assert_eq!(get_json["secret_policy"]["stores_raw_tokens"], json!(false));
        assert_eq!(
            get_json["secret_policy"]["returns_raw_tokens_in_draft"],
            json!(false)
        );

        let delete_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/draft")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete_response.status(), StatusCode::OK);

        let status_after_delete = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status_after_delete_body =
            axum::body::to_bytes(status_after_delete.into_body(), usize::MAX)
                .await
                .unwrap();
        let status_after_delete_json: serde_json::Value =
            serde_json::from_slice(&status_after_delete_body).unwrap();
        assert_eq!(status_after_delete_json["draft_available"], json!(false));
        assert_eq!(status_after_delete_json["resume_state"], json!("none"));
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn write_agentdesk_discord_config_prefers_config_dir_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(
            root.join("config").join("agentdesk.yaml"),
            "server:\n  port: 8791\n",
        )
        .unwrap();

        write_agentdesk_discord_config(
            root,
            "guild-123",
            "primary-token",
            "claude",
            None,
            None,
            Some(VALID_OWNER_ID),
        )
        .unwrap();

        assert!(!root.join("agentdesk.yaml").exists());
        let config =
            crate::config::load_from_path(&root.join("config").join("agentdesk.yaml")).unwrap();
        assert_eq!(config.server.port, 8791);
        assert_eq!(config.discord.guild_id.as_deref(), Some("guild-123"));
        assert_eq!(config.discord.owner_id, Some(VALID_OWNER_ID_U64));
        assert_eq!(
            config.discord.bots["command"].provider.as_deref(),
            Some("claude")
        );
        assert_eq!(
            config.discord.bots["command"].token.as_deref(),
            Some("primary-token")
        );
    }

    #[test]
    fn write_discord_and_credential_artifacts_use_runtime_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        write_agentdesk_discord_config(
            root,
            "guild-123",
            "primary-token",
            "claude",
            Some("secondary-token"),
            Some("codex"),
            Some(VALID_OWNER_ID),
        )
        .unwrap();
        write_credential_token(root, "announce", Some("announce-token")).unwrap();
        write_credential_token(root, "notify", Some("notify-token")).unwrap();

        let config =
            crate::config::load_from_path(&root.join("config").join("agentdesk.yaml")).unwrap();
        assert_eq!(config.discord.guild_id.as_deref(), Some("guild-123"));
        assert_eq!(config.discord.owner_id, Some(VALID_OWNER_ID_U64));
        assert_eq!(config.discord.bots.len(), 2);
        assert_eq!(
            config.discord.bots["command"].provider.as_deref(),
            Some("claude")
        );
        assert_eq!(
            config.discord.bots["command_2"].provider.as_deref(),
            Some("codex")
        );

        assert_eq!(
            std::fs::read_to_string(crate::runtime_layout::credential_token_path(
                root, "announce"
            ))
            .unwrap(),
            "announce-token\n"
        );
        assert_eq!(
            std::fs::read_to_string(crate::runtime_layout::credential_token_path(root, "notify"))
                .unwrap(),
            "notify-token\n"
        );
        assert!(
            std::fs::symlink_metadata(crate::runtime_layout::legacy_credential_dir(root))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(
                crate::runtime_layout::legacy_credential_dir(root).join("announce_bot_token"),
            )
            .unwrap(),
            "announce-token\n"
        );
    }

    #[test]
    fn write_agentdesk_discord_config_rejects_short_owner_id() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        let error = write_agentdesk_discord_config(
            root,
            "guild-123",
            "primary-token",
            "claude",
            None,
            None,
            Some("7"),
        )
        .unwrap_err();

        assert!(error.contains("owner_id must be a Discord user id"));
    }

    #[test]
    fn desired_channel_name_strips_leading_hash() {
        let mapping = ChannelMapping {
            channel_id: String::new(),
            channel_name: "#agentdesk-cdx".to_string(),
            role_id: "adk-cdx".to_string(),
            description: None,
            system_prompt: None,
        };

        assert_eq!(desired_channel_name(&mapping).unwrap(), "agentdesk-cdx");
    }

    #[tokio::test]
    async fn resolve_channel_mapping_reuses_existing_channel() {
        let post_count = Arc::new(AtomicUsize::new(0));
        let post_count_for_route = post_count.clone();
        let app = Router::new().route(
            "/guilds/{guild_id}/channels",
            get(|AxumPath(_guild_id): AxumPath<String>| async move {
                Json(json!([
                    {"id": "42", "name": "agentdesk-cdx", "type": 0}
                ]))
            })
            .post(
                move |AxumPath(_guild_id): AxumPath<String>,
                      Json(body): Json<serde_json::Value>| {
                    let post_count = post_count_for_route.clone();
                    async move {
                        post_count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "id": "77",
                            "name": body.get("name").and_then(|value| value.as_str()).unwrap_or("created"),
                            "type": 0
                        }))
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mapping = ChannelMapping {
            channel_id: "agentdesk-cdx".to_string(),
            channel_name: "agentdesk-cdx".to_string(),
            role_id: "adk-cdx".to_string(),
            description: Some("desc".to_string()),
            system_prompt: Some("prompt".to_string()),
        };

        let resolved = resolve_channel_mapping(
            &reqwest::Client::new(),
            "token",
            &format!("http://{}", addr),
            "123",
            &mapping,
            None,
        )
        .await
        .unwrap();

        assert_eq!(resolved.channel_id, "42");
        assert_eq!(resolved.channel_name, "agentdesk-cdx");
        assert!(!resolved.created);
        assert_eq!(post_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolve_channel_mapping_creates_missing_channel() {
        let post_count = Arc::new(AtomicUsize::new(0));
        let post_count_for_route = post_count.clone();
        let app = Router::new().route(
            "/guilds/{guild_id}/channels",
            get(|AxumPath(_guild_id): AxumPath<String>| async move { Json(json!([])) }).post(
                move |AxumPath(_guild_id): AxumPath<String>,
                      Json(body): Json<serde_json::Value>| {
                    let post_count = post_count_for_route.clone();
                    async move {
                        post_count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "id": "77",
                            "name": body.get("name").and_then(|value| value.as_str()).unwrap_or("created"),
                            "type": 0
                        }))
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mapping = ChannelMapping {
            channel_id: "agentdesk-cdx".to_string(),
            channel_name: "agentdesk-cdx".to_string(),
            role_id: "adk-cdx".to_string(),
            description: Some("desc".to_string()),
            system_prompt: Some("prompt".to_string()),
        };

        let resolved = resolve_channel_mapping(
            &reqwest::Client::new(),
            "token",
            &format!("http://{}", addr),
            "123",
            &mapping,
            None,
        )
        .await
        .unwrap();

        assert_eq!(resolved.channel_id, "77");
        assert_eq!(resolved.channel_name, "agentdesk-cdx");
        assert!(resolved.created);
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn complete_retries_from_partial_state_without_duplicate_channel_creation() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let (discord_api_base, post_count) = spawn_mock_discord_server().await;
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let body = sample_complete_body("agentdesk-cdx", "agentdesk-cdx", Some("reuse_existing"));

        let failure_options = CompleteExecutionOptions {
            discord_api_base: discord_api_base.clone(),
            fail_after_stage: Some(OnboardingCompletionStage::ArtifactsPersisted),
        };
        let (failed_status, failed_body) =
            complete_with_options(&state, &body, &failure_options).await;
        assert_eq!(failed_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failed_body["partial_apply"], json!(true));
        assert_eq!(
            failed_body["completion_state"]["stage"],
            json!("artifacts_persisted")
        );
        assert_eq!(post_count.load(Ordering::SeqCst), 1);

        let status_state = AppState::test_state(db.clone(), test_engine(&db));
        let (status_code, Json(status_body)) = status(&status_state).await;
        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(status_body["partial_apply"], json!(true));
        assert_eq!(
            status_body["completion_state"]["channels"][0]["channel_id"],
            json!("1001")
        );

        let success_options = CompleteExecutionOptions {
            discord_api_base,
            fail_after_stage: None,
        };
        let (success_status, success_body) =
            complete_with_options(&state, &body, &success_options).await;
        assert_eq!(success_status, StatusCode::OK);
        assert_eq!(success_body["ok"], json!(true));
        assert_eq!(success_body["partial_apply"], json!(false));
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            success_body["artifacts"]["channel_mappings"][0]["resolution"],
            json!("checkpoint")
        );

        let conn = db.read_conn().unwrap();
        let stored_channel: String = conn
            .query_row(
                "SELECT discord_channel_id FROM agents WHERE id = 'adk-cdx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_channel, "1001");
    }

    #[tokio::test]
    async fn status_omits_invalid_legacy_owner_id_from_rerun_payload() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                sqlite_test::params!["onboarding_owner_id", "42"],
            )
            .unwrap();
        }

        let state = AppState::test_state(db.clone(), test_engine(&db));
        let (status_code, Json(status_body)) = status(&state).await;

        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(status_body["owner_id"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn draft_get_sanitizes_invalid_legacy_owner_id_from_saved_draft() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::get(crate::server::routes::onboarding::draft_get),
            )
            .with_state(state);

        let mut legacy_draft = sample_draft();
        legacy_draft.owner_id = "42".to_string();
        save_onboarding_draft(temp.path(), &legacy_draft).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/draft")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["draft"]["owner_id"], json!(""));
    }

    #[tokio::test]
    async fn complete_retries_from_channels_resolved_partial_state() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let (discord_api_base, post_count) = spawn_mock_discord_server().await;
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let body = sample_complete_body("agentdesk-cdx", "agentdesk-cdx", Some("reuse_existing"));

        let failure_options = CompleteExecutionOptions {
            discord_api_base: discord_api_base.clone(),
            fail_after_stage: Some(OnboardingCompletionStage::ChannelsResolved),
        };
        let (failed_status, failed_body) =
            complete_with_options(&state, &body, &failure_options).await;
        assert_eq!(failed_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failed_body["partial_apply"], json!(true));
        assert_eq!(
            failed_body["completion_state"]["stage"],
            json!("channels_resolved")
        );
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
        assert!(!onboarding_config_path(temp.path()).is_file());

        let success_options = CompleteExecutionOptions {
            discord_api_base,
            fail_after_stage: None,
        };
        let (success_status, success_body) =
            complete_with_options(&state, &body, &success_options).await;
        assert_eq!(success_status, StatusCode::OK);
        assert_eq!(success_body["ok"], json!(true));
        assert_eq!(success_body["partial_apply"], json!(false));
        assert_eq!(post_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            success_body["artifacts"]["channel_mappings"][0]["resolution"],
            json!("checkpoint")
        );
        assert!(onboarding_config_path(temp.path()).is_file());
    }

    #[tokio::test]
    async fn complete_keeps_existing_draft_on_failure_and_clears_it_on_success() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let (discord_api_base, _post_count) = spawn_mock_discord_server().await;
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::get(crate::server::routes::onboarding::draft_get),
            )
            .with_state(state.clone());
        let body = sample_complete_body("agentdesk-cdx", "agentdesk-cdx", Some("reuse_existing"));

        save_onboarding_draft(temp.path(), &sample_draft().normalize().unwrap()).unwrap();
        assert!(onboarding_draft_path(temp.path()).is_file());

        let failure_options = CompleteExecutionOptions {
            discord_api_base: discord_api_base.clone(),
            fail_after_stage: Some(OnboardingCompletionStage::ArtifactsPersisted),
        };
        let (failed_status, failed_body) =
            complete_with_options(&state, &body, &failure_options).await;
        assert_eq!(failed_status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failed_body["partial_apply"], json!(true));
        let retained_draft = load_onboarding_draft(temp.path()).unwrap().unwrap();
        assert_eq!(retained_draft.command_bots[0].token, "command-token");

        let success_options = CompleteExecutionOptions {
            discord_api_base,
            fail_after_stage: None,
        };
        let (success_status, success_body) =
            complete_with_options(&state, &body, &success_options).await;
        assert_eq!(success_status, StatusCode::OK);
        assert_eq!(success_body["ok"], json!(true));
        assert!(load_onboarding_draft(temp.path()).unwrap().is_none());
        let draft_get_response = app
            .oneshot(
                Request::builder()
                    .uri("/draft")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(draft_get_response.status(), StatusCode::OK);
        let draft_get_body = axum::body::to_bytes(draft_get_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let draft_get_json: serde_json::Value = serde_json::from_slice(&draft_get_body).unwrap();
        assert_eq!(draft_get_json["available"], json!(false));

        let status_state = AppState::test_state(db.clone(), test_engine(&db));
        let (status_code, Json(status_body)) = status(&status_state).await;
        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(status_body["setup_mode"], json!("rerun"));
        assert_eq!(status_body["draft_available"], json!(false));
        assert_eq!(status_body["resume_state"], json!("none"));
    }

    #[tokio::test]
    async fn draft_put_rejects_oversized_payload() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::put(crate::server::routes::onboarding::draft_put),
            )
            .with_state(state);

        let mut oversized = sample_draft();
        oversized.agents = (0..80)
            .map(|index| OnboardingDraftAgent {
                id: format!("agent-{index}"),
                name: format!("Agent {index}"),
                name_en: Some(format!("Agent {index}")),
                description: "desc".to_string(),
                description_en: Some("desc".to_string()),
                prompt: "prompt".repeat(32),
                custom: true,
            })
            .collect();

        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/draft")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&oversized).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("max agents")
        );
    }

    #[tokio::test]
    async fn draft_put_rejects_invalid_owner_id() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let app = Router::new()
            .route(
                "/draft",
                axum::routing::put(crate::server::routes::onboarding::draft_put),
            )
            .with_state(state);

        let mut draft = sample_draft();
        draft.owner_id = "42".to_string();

        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/draft")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&draft).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("owner_id must be a Discord user id")
        );
        assert!(load_onboarding_draft(temp.path()).unwrap().is_none());
    }

    #[test]
    fn requested_channel_fingerprint_includes_provider() {
        let body = sample_complete_body("agentdesk-cdx", "agentdesk-cdx", Some("reuse_existing"));
        let claude = requested_channel_fingerprint(&body, "claude").unwrap();
        let codex = requested_channel_fingerprint(&body, "codex").unwrap();
        assert_ne!(claude, codex);
    }

    #[test]
    fn load_onboarding_completion_state_ignores_corrupt_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = onboarding_completion_state_path(temp.path());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, "{not-json").unwrap();

        let state = load_onboarding_completion_state(temp.path()).unwrap();
        assert!(state.is_none());
        assert!(!path.exists());
        let archived = path
            .parent()
            .unwrap()
            .read_dir()
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("onboarding_completion_state.json.corrupt-")
            });
        assert!(archived);
    }

    #[tokio::test]
    async fn complete_rejects_empty_guild_id_even_with_numeric_channels() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));
        let mut body = sample_complete_body("9001", "agentdesk-cdx", Some("reuse_existing"));
        body.guild_id = "   ".to_string();

        let (status, response) =
            complete_with_options(&state, &body, &CompleteExecutionOptions::default()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            response["error"],
            json!("guild_id is required for onboarding completion")
        );
        assert!(
            load_onboarding_completion_state(temp.path())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn complete_requires_explicit_replace_policy_before_overwriting_agent() {
        let temp = tempfile::tempdir().unwrap();
        let _runtime = RuntimeRootGuard::new(temp.path());
        let db = test_db();
        let config_path = onboarding_config_path(temp.path());
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut config = crate::config::Config::default();
        config.agents.push(crate::config::AgentDef {
            id: "adk-cdx".to_string(),
            name: "adk-cdx".to_string(),
            name_ko: None,
            provider: "claude".to_string(),
            channels: crate::config::AgentChannels {
                codex: Some(crate::config::AgentChannel::Detailed(
                    crate::config::AgentChannelConfig {
                        id: Some("5555".to_string()),
                        name: Some("legacy-cdx".to_string()),
                        aliases: Vec::new(),
                        prompt_file: None,
                        workspace: Some("~/legacy".to_string()),
                        provider: Some("codex".to_string()),
                        model: None,
                        reasoning_effort: None,
                        peer_agents: None,
                        quality_feedback_injection: None,
                        dispatch_profile: None,
                        cache_ttl_minutes: None,
                    },
                )),
                ..crate::config::AgentChannels::default()
            },
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        });
        crate::config::save_to_path(&config_path, &config).unwrap();
        let role_map_path = crate::runtime_layout::role_map_path(temp.path());
        if let Some(parent) = role_map_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            &role_map_path,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "byChannelId": {
                    "5555": {
                        "roleId": "adk-cdx",
                        "provider": "codex",
                        "workspace": "~/legacy"
                    }
                },
                "byChannelName": {
                    "legacy-cdx": {
                        "roleId": "adk-cdx",
                        "channelId": "5555",
                        "workspace": "~/legacy"
                    }
                },
                "fallbackByChannelName": { "enabled": true }
            }))
            .unwrap(),
        )
        .unwrap();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, discord_channel_id, description, system_prompt, status, xp) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 0)",
                sqlite_test::params![
                    "adk-cdx",
                    "adk-cdx",
                    "claude",
                    "5555",
                    "existing desc",
                    "existing prompt"
                ],
            )
            .unwrap();
        }

        let state = AppState::test_state(db.clone(), test_engine(&db));
        let reuse_body = sample_complete_body("9001", "agentdesk-cdx", Some("reuse_existing"));
        let (conflict_status, conflict_body) =
            complete_with_options(&state, &reuse_body, &CompleteExecutionOptions::default()).await;
        assert_eq!(conflict_status, StatusCode::CONFLICT);
        assert_eq!(conflict_body["partial_apply"], json!(false));
        assert_eq!(conflict_body["retry_recommended"], json!(false));
        assert_eq!(
            conflict_body["conflicts"][0]
                .as_str()
                .unwrap()
                .contains("rerun_policy=reuse_existing"),
            true
        );

        let replace_body = sample_complete_body("9001", "agentdesk-cdx", Some("replace_existing"));
        let (replace_status, replace_body_json) =
            complete_with_options(&state, &replace_body, &CompleteExecutionOptions::default())
                .await;
        assert_eq!(replace_status, StatusCode::OK);
        assert_eq!(replace_body_json["ok"], json!(true));
        assert_eq!(
            replace_body_json["rerun_policy"]["applied"],
            json!("replace_existing")
        );

        let conn = db.read_conn().unwrap();
        let (provider, channel_id, description, prompt): (
            String,
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT provider, discord_channel_id, description, system_prompt \
                 FROM agents WHERE id = 'adk-cdx'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(provider, "codex");
        assert_eq!(channel_id, "9001");
        assert_eq!(description.as_deref(), Some("dispatch desk"));
        assert_eq!(prompt.as_deref(), Some("be precise"));

        let saved_config = crate::config::load_from_path(&config_path).unwrap();
        let saved_agent = saved_config
            .agents
            .iter()
            .find(|agent| agent.id == "adk-cdx")
            .unwrap();
        let saved_channel = saved_agent
            .channels
            .codex
            .as_ref()
            .and_then(crate::config::AgentChannel::target)
            .unwrap();
        assert_eq!(saved_channel, "9001");

        let saved_role_map: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&role_map_path).unwrap()).unwrap();
        assert!(saved_role_map["byChannelId"].get("5555").is_none());
        assert!(saved_role_map["byChannelName"].get("legacy-cdx").is_none());
        assert_eq!(
            saved_role_map["byChannelId"]["9001"]["roleId"],
            json!("adk-cdx")
        );
        assert_eq!(
            saved_role_map["byChannelName"]["agentdesk-cdx"]["channelId"],
            json!("9001")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_provider_uses_resolver_exec_path_under_minimal_path() {
        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("provider-helper");
        let provider = temp.path().join("claude");
        let original_path = std::env::var_os("PATH");
        let original_home = std::env::var_os("HOME");

        write_executable(&helper, "#!/bin/sh\nprintf 'claude-test 9.9.9\\n'\n");
        write_executable(
            &provider,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  provider-helper\nelse\n  exit 64\nfi\n",
        );

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("AGENTDESK_CLAUDE_PATH", &provider);
        }

        let (status, Json(body)) = check_provider(CheckProviderBody {
            provider: "claude".to_string(),
        })
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(true));
        assert_eq!(body["logged_in"], json!(false));
        assert_eq!(body["version"], json!("claude-test 9.9.9"));
        assert_eq!(body["source"], json!("env_override"));
        assert_eq!(body["path"], json!(provider.to_string_lossy().to_string()));

        unsafe {
            std::env::remove_var("AGENTDESK_CLAUDE_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_provider_reports_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("claude");
        let original_path = std::env::var_os("PATH");
        let original_home = std::env::var_os("HOME");

        std::fs::write(&provider, "#!/bin/sh\nprintf 'claude-test 9.9.9\\n'\n").unwrap();
        let mut perms = std::fs::metadata(&provider).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&provider, perms).unwrap();

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("AGENTDESK_CLAUDE_PATH", &provider);
        }

        let (status, Json(body)) = check_provider(CheckProviderBody {
            provider: "claude".to_string(),
        })
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(false));
        assert_eq!(body["version"], json!(null));
        assert_eq!(body["failure_kind"], json!("permission_denied"));
        assert_eq!(body["path"], json!(null));

        unsafe {
            std::env::remove_var("AGENTDESK_CLAUDE_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_provider_reports_opencode_permission_denied_with_attempts() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("opencode");
        let original_path = std::env::var_os("PATH");
        let original_home = std::env::var_os("HOME");

        std::fs::write(&provider, "#!/bin/sh\nprintf 'opencode 9.9.9\\n'\n").unwrap();
        let mut perms = std::fs::metadata(&provider).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&provider, perms).unwrap();

        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("AGENTDESK_OPENCODE_PATH", &provider);
        }

        let (status, Json(body)) = check_provider(CheckProviderBody {
            provider: "opencode".to_string(),
        })
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["installed"], json!(false));
        assert_eq!(body["logged_in"], json!(false));
        assert_eq!(body["version"], json!(null));
        assert_eq!(body["failure_kind"], json!("permission_denied"));
        assert_eq!(body["path"], json!(null));
        assert!(
            body["attempts"]
                .as_array()
                .is_some_and(|attempts| !attempts.is_empty())
        );

        unsafe {
            std::env::remove_var("AGENTDESK_OPENCODE_PATH");
            match original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match original_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

// ── Provider Check ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CheckProviderBody {
    pub provider: String,
}

/// POST /api/onboarding/check-provider
/// Checks if a CLI provider (claude/codex/gemini/opencode/qwen) is installed and authenticated.
pub async fn check_provider(body: CheckProviderBody) -> (StatusCode, Json<serde_json::Value>) {
    let cmd = match body.provider.as_str() {
        "claude" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        "opencode" => "opencode",
        "qwen" => "qwen",
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "provider must be 'claude', 'codex', 'gemini', 'opencode', or 'qwen'"}),
                ),
            );
        }
    };

    // Resolve binary using the exact same provider-specific resolver as the runtime,
    // including known-path fallbacks (~/bin, /opt/homebrew/bin, etc.).
    // This ensures onboarding and actual launch always agree on availability.
    let resolution = {
        let provider = cmd.to_string();
        tokio::task::spawn_blocking(move || {
            crate::services::platform::resolve_provider_binary(&provider)
        })
        .await
        .ok()
    }
    .unwrap_or_else(|| crate::services::platform::resolve_provider_binary(cmd));
    let mut failure_kind = resolution.failure_kind.clone();

    let Some(bin_path) = resolution.resolved_path.clone() else {
        return (
            StatusCode::OK,
            Json(json!({
                "installed": false,
                "logged_in": false,
                "version": null,
                "path": null,
                "canonical_path": null,
                "source": null,
                "failure_kind": resolution.failure_kind,
                "attempts": resolution.attempts,
            })),
        );
    };

    // Get version using the resolved binary path (not bare command name)
    // so it works even when PATH doesn't contain the provider.
    let (version, probe_failure_kind) = {
        let resolution = resolution.clone();
        let bin_path = bin_path.clone();
        tokio::task::spawn_blocking(move || {
            crate::services::platform::probe_resolved_binary_version(&bin_path, &resolution)
        })
        .await
        .ok()
        .unwrap_or((None, Some("version_probe_spawn_failed".to_string())))
    };
    if failure_kind.is_none() {
        failure_kind = probe_failure_kind.clone();
    }

    // Check login (heuristic: config directory exists with content)
    let logged_in = if cmd == "opencode" {
        true
    } else {
        dirs::home_dir()
            .map(|home| {
                let config_dir = if cmd == "claude" {
                    home.join(".claude")
                } else if cmd == "codex" {
                    home.join(".codex")
                } else if cmd == "qwen" {
                    home.join(".qwen")
                } else {
                    home.join(".gemini")
                };
                config_dir.is_dir()
            })
            .unwrap_or(false)
    };

    (
        StatusCode::OK,
        Json(json!({
            "installed": true,
            "logged_in": logged_in,
            "version": version,
            "path": resolution.resolved_path,
            "canonical_path": resolution.canonical_path,
            "source": resolution.source,
            "failure_kind": failure_kind,
            "attempts": resolution.attempts,
        })),
    )
}

// ── AI Prompt Generation ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GeneratePromptBody {
    pub name: String,
    pub description: String,
    pub provider: Option<String>,
}

/// POST /api/onboarding/generate-prompt
/// Generates a system prompt for a custom agent using the local CLI.
pub async fn generate_prompt(body: GeneratePromptBody) -> (StatusCode, Json<serde_json::Value>) {
    let provider = body
        .provider
        .as_deref()
        .and_then(ProviderKind::from_str)
        .unwrap_or(ProviderKind::Claude);

    let instruction = format!(
        "다음 AI 에이전트의 시스템 프롬프트를 한국어로 작성해줘.\n\
         이름: {}\n설명: {}\n\n\
         에이전트의 역할, 핵심 능력, 소통 스타일을 포함해서 5-10줄로 작성해.\n\
         시스템 프롬프트 텍스트만 출력하고 다른 설명은 붙이지 마.",
        body.name, body.description
    );

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        provider_exec::execute_simple(provider, instruction),
    )
    .await;

    if let Ok(Ok(text)) = result {
        if !text.trim().is_empty() {
            return (
                StatusCode::OK,
                Json(json!({ "prompt": text.trim(), "source": "ai" })),
            );
        }
    }

    // Fallback to template
    let fallback = format!(
        "당신은 '{name}'입니다. {desc}\n\n\
         ## 역할\n\
         - 위 설명에 맞는 업무를 수행합니다\n\
         - 사용자의 요청에 정확하고 친절하게 응답합니다\n\n\
         ## 소통 원칙\n\
         - 한국어로 소통합니다\n\
         - 간결하고 명확하게 답변합니다\n\
         - 필요시 확인 질문을 합니다",
        name = body.name,
        desc = body.description,
    );

    (
        StatusCode::OK,
        Json(json!({ "prompt": fallback, "source": "template" })),
    )
}
