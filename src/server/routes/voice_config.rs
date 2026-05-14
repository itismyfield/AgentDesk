use std::path::PathBuf;

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::AppState;
use crate::config::{AgentDef, Config};
use crate::voice::barge_in::BargeInSensitivity;
use crate::voice::config::DEFAULT_ACTIVE_AGENT_TTL_SECS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct VoiceGlobalConfigDto {
    lobby_channel_id: Option<String>,
    active_agent_ttl_seconds: u64,
    default_sensitivity_mode: BargeInSensitivity,
}

impl Default for VoiceGlobalConfigDto {
    fn default() -> Self {
        Self {
            lobby_channel_id: None,
            active_agent_ttl_seconds: DEFAULT_ACTIVE_AGENT_TTL_SECS,
            default_sensitivity_mode: BargeInSensitivity::Normal,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct VoiceAgentConfigDto {
    id: String,
    name: String,
    name_ko: Option<String>,
    voice_enabled: bool,
    wake_word: String,
    aliases: Vec<String>,
    sensitivity_mode: BargeInSensitivity,
}

impl Default for VoiceAgentConfigDto {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            name_ko: None,
            voice_enabled: true,
            wake_word: String::new(),
            aliases: Vec::new(),
            sensitivity_mode: BargeInSensitivity::Normal,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct VoiceConfigResponse {
    global: VoiceGlobalConfigDto,
    agents: Vec<VoiceAgentConfigDto>,
    version: String,
    source_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct PutVoiceConfigBody {
    version: Option<String>,
    actor: Option<String>,
    global: VoiceGlobalConfigDto,
    agents: Vec<VoiceAgentConfigDto>,
}

impl Default for PutVoiceConfigBody {
    fn default() -> Self {
        Self {
            version: None,
            actor: None,
            global: VoiceGlobalConfigDto::default(),
            agents: Vec::new(),
        }
    }
}

#[derive(Debug)]
enum VoiceConfigError {
    BadRequest(String),
    Conflict(Value),
    Internal(String),
}

impl VoiceConfigError {
    fn into_response(self) -> (StatusCode, Json<Value>) {
        match self {
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "bad_request", "message": message})),
            ),
            Self::Conflict(body) => (StatusCode::CONFLICT, Json(body)),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error", "message": message})),
            ),
        }
    }
}

/// GET /api/voice/config
pub async fn get_voice_config(State(_state): State<AppState>) -> (StatusCode, Json<Value>) {
    match load_voice_config_response() {
        Ok(response) => (StatusCode::OK, Json(json!(response))),
        Err(error) => error.into_response(),
    }
}

/// PUT /api/voice/config
pub async fn put_voice_config(
    State(state): State<AppState>,
    Json(body): Json<PutVoiceConfigBody>,
) -> (StatusCode, Json<Value>) {
    match put_voice_config_inner(&state, body).await {
        Ok(response) => (StatusCode::OK, Json(json!(response))),
        Err(error) => error.into_response(),
    }
}

async fn put_voice_config_inner(
    state: &AppState,
    body: PutVoiceConfigBody,
) -> Result<VoiceConfigResponse, VoiceConfigError> {
    let (mut config, path, _) = load_editable_config()?;
    let current_version = voice_config_version(&config);
    if let Some(version) = body.version.as_deref()
        && version != current_version
    {
        return Err(VoiceConfigError::Conflict(json!({
            "error": "version_conflict",
            "message": "voice config was changed by another writer",
            "current_version": current_version,
        })));
    }

    let before = config.clone();
    apply_voice_config_body(&mut config, &body)?;
    crate::voice::commands::validate_agent_alias_collisions(&config.agents)
        .map_err(|collision| alias_collision_response(&config, collision))?;

    crate::config::save_to_path(&path, &config).map_err(|error| {
        VoiceConfigError::Internal(format!("write config '{}': {error}", path.display()))
    })?;
    write_voice_config_audit_logs(
        state.pg_pool_ref(),
        body.actor.as_deref().unwrap_or("dashboard"),
        &before,
        &config,
    )
    .await;

    Ok(response_from_config(config, Some(path)))
}

fn load_voice_config_response() -> Result<VoiceConfigResponse, VoiceConfigError> {
    let (config, path, _) = load_editable_config()?;
    Ok(response_from_config(config, Some(path)))
}

fn load_editable_config() -> Result<(Config, PathBuf, bool), VoiceConfigError> {
    let root = crate::config::runtime_root()
        .ok_or_else(|| VoiceConfigError::Internal("runtime root unavailable".to_string()))?;
    for path in [
        crate::runtime_layout::config_file_path(&root),
        crate::runtime_layout::legacy_config_file_path(&root),
    ] {
        if path.is_file() {
            let config = crate::config::load_from_path(&path).map_err(|error| {
                VoiceConfigError::Internal(format!("load '{}': {error}", path.display()))
            })?;
            return Ok((config, path, true));
        }
    }

    Ok((
        Config::default(),
        crate::runtime_layout::config_file_path(&root),
        false,
    ))
}

fn apply_voice_config_body(
    config: &mut Config,
    body: &PutVoiceConfigBody,
) -> Result<(), VoiceConfigError> {
    config.voice.lobby_channel_id = clean_optional_string(body.global.lobby_channel_id.clone());
    config.voice.active_agent_ttl_seconds = match body.global.active_agent_ttl_seconds {
        0 => DEFAULT_ACTIVE_AGENT_TTL_SECS,
        value => value,
    };
    config.voice.default_sensitivity_mode = body.global.default_sensitivity_mode;
    config.voice.barge_in.sensitivity = body.global.default_sensitivity_mode;

    for patch in &body.agents {
        let agent = config
            .agents
            .iter_mut()
            .find(|agent| agent.id == patch.id)
            .ok_or_else(|| VoiceConfigError::BadRequest(format!("unknown agent '{}'", patch.id)))?;
        agent.voice_enabled = patch.voice_enabled;
        agent.wake_word = clean_optional_string(Some(patch.wake_word.clone()));
        agent.aliases = clean_string_list(&patch.aliases);
        agent.sensitivity_mode = Some(patch.sensitivity_mode);
    }

    Ok(())
}

fn response_from_config(config: Config, source_path: Option<PathBuf>) -> VoiceConfigResponse {
    VoiceConfigResponse {
        global: VoiceGlobalConfigDto {
            lobby_channel_id: clean_optional_string(config.voice.lobby_channel_id.clone()),
            active_agent_ttl_seconds: config.voice.active_agent_ttl_seconds,
            default_sensitivity_mode: config.voice.default_sensitivity_mode,
        },
        agents: config.agents.iter().map(agent_to_dto).collect(),
        version: voice_config_version(&config),
        source_path: source_path.map(|path| path.display().to_string()),
    }
}

fn agent_to_dto(agent: &AgentDef) -> VoiceAgentConfigDto {
    VoiceAgentConfigDto {
        id: agent.id.clone(),
        name: agent.name.clone(),
        name_ko: agent.name_ko.clone(),
        voice_enabled: agent.voice_enabled,
        wake_word: agent
            .wake_word
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
        aliases: clean_string_list(&agent.aliases),
        sensitivity_mode: agent.sensitivity_mode.unwrap_or(BargeInSensitivity::Normal),
    }
}

fn voice_config_version(config: &Config) -> String {
    let snapshot = json!({
        "global": {
            "lobby_channel_id": clean_optional_string(config.voice.lobby_channel_id.clone()),
            "active_agent_ttl_seconds": config.voice.active_agent_ttl_seconds,
            "default_sensitivity_mode": config.voice.default_sensitivity_mode,
        },
        "agents": config.agents.iter().map(agent_to_dto).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec(&snapshot).expect("voice config snapshot serializes");
    hex::encode(Sha256::digest(bytes))
}

fn clean_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn clean_string_list(values: &[String]) -> Vec<String> {
    let mut cleaned = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() || cleaned.iter().any(|existing| existing == value) {
            continue;
        }
        cleaned.push(value.to_string());
    }
    cleaned
}

fn alias_collision_response(
    config: &Config,
    collision: crate::voice::commands::VoiceAliasCollision,
) -> VoiceConfigError {
    let first_name = config
        .agents
        .iter()
        .find(|agent| agent.id == collision.first_agent_id)
        .map(|agent| agent.name.clone())
        .unwrap_or_else(|| collision.first_agent_id.clone());
    let second_name = config
        .agents
        .iter()
        .find(|agent| agent.id == collision.second_agent_id)
        .map(|agent| agent.name.clone())
        .unwrap_or_else(|| collision.second_agent_id.clone());
    VoiceConfigError::Conflict(json!({
        "error": "alias_conflict",
        "message": collision.to_string(),
        "conflict": {
            "normalized": collision.normalized,
            "first_agent_id": collision.first_agent_id,
            "first_agent_name": first_name,
            "first_alias": collision.first_alias,
            "second_agent_id": collision.second_agent_id,
            "second_agent_name": second_name,
            "second_alias": collision.second_alias,
        }
    }))
}

async fn write_voice_config_audit_logs(
    pool: Option<&sqlx::PgPool>,
    actor: &str,
    before: &Config,
    after: &Config,
) {
    let Some(pool) = pool else {
        return;
    };
    for action in voice_config_audit_actions(actor, before, after) {
        let _ = sqlx::query(
            "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&action.entity_type)
        .bind(&action.entity_id)
        .bind(&action.action)
        .bind(actor)
        .execute(pool)
        .await;
    }
}

struct AuditAction {
    entity_type: &'static str,
    entity_id: String,
    action: String,
}

fn voice_config_audit_actions(actor: &str, before: &Config, after: &Config) -> Vec<AuditAction> {
    let mut actions = Vec::new();
    if before.voice.lobby_channel_id != after.voice.lobby_channel_id
        || before.voice.active_agent_ttl_seconds != after.voice.active_agent_ttl_seconds
        || before.voice.default_sensitivity_mode != after.voice.default_sensitivity_mode
    {
        actions.push(AuditAction {
            entity_type: "voice_config",
            entity_id: "global".to_string(),
            action: format!("voice global config changed by {actor}"),
        });
    }

    for after_agent in &after.agents {
        let Some(before_agent) = before
            .agents
            .iter()
            .find(|agent| agent.id == after_agent.id)
        else {
            continue;
        };
        let before_dto = agent_to_dto(before_agent);
        let after_dto = agent_to_dto(after_agent);
        if before_dto == after_dto {
            continue;
        }
        let mut changed = Vec::new();
        if before_dto.voice_enabled != after_dto.voice_enabled {
            changed.push(format!(
                "voice_enabled:{}->{}",
                before_dto.voice_enabled, after_dto.voice_enabled
            ));
        }
        if before_dto.wake_word != after_dto.wake_word {
            changed.push("wake_word changed".to_string());
        }
        if before_dto.aliases != after_dto.aliases {
            changed.push(format!(
                "aliases:{:?}->{:?}",
                before_dto.aliases, after_dto.aliases
            ));
        }
        if before_dto.sensitivity_mode != after_dto.sensitivity_mode {
            changed.push(format!(
                "sensitivity:{:?}->{:?}",
                before_dto.sensitivity_mode, after_dto.sensitivity_mode
            ));
        }
        actions.push(AuditAction {
            entity_type: "agent",
            entity_id: after_agent.id.clone(),
            action: format!("voice config changed by {actor}: {}", changed.join(", ")),
        });
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentChannels, AgentDef};

    fn agent(id: &str, name: &str) -> AgentDef {
        AgentDef {
            id: id.to_string(),
            name: name.to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: crate::config::AgentVoiceConfig::default(),
            provider: "codex".to_string(),
            channels: AgentChannels::default(),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        }
    }

    #[test]
    fn put_body_rejects_duplicate_dashboard_aliases() {
        let mut config = Config {
            agents: vec![agent("ch-td", "TD"), agent("ch-pd", "PD")],
            ..Config::default()
        };
        let body = PutVoiceConfigBody {
            global: VoiceGlobalConfigDto::default(),
            agents: vec![
                VoiceAgentConfigDto {
                    id: "ch-td".to_string(),
                    aliases: vec!["테크 디렉터".to_string()],
                    ..VoiceAgentConfigDto::default()
                },
                VoiceAgentConfigDto {
                    id: "ch-pd".to_string(),
                    aliases: vec!["테크디렉터".to_string()],
                    ..VoiceAgentConfigDto::default()
                },
            ],
            ..PutVoiceConfigBody::default()
        };

        apply_voice_config_body(&mut config, &body).unwrap();
        let collision = crate::voice::commands::validate_agent_alias_collisions(&config.agents)
            .expect_err("duplicate normalized aliases should be rejected");
        let VoiceConfigError::Conflict(body) = alias_collision_response(&config, collision) else {
            panic!("expected conflict");
        };
        assert_eq!(body["error"], "alias_conflict");
        assert_eq!(body["conflict"]["first_agent_id"], "ch-td");
        assert_eq!(body["conflict"]["second_agent_id"], "ch-pd");
    }

    #[test]
    fn version_changes_when_voice_alias_changes() {
        let mut config = Config {
            agents: vec![agent("ch-td", "TD")],
            ..Config::default()
        };
        let before = voice_config_version(&config);
        config.agents[0].aliases.push("테크 디렉터".to_string());
        let after = voice_config_version(&config);
        assert_ne!(before, after);
    }
}
