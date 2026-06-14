use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path as FsPath;

use super::AppState;
use crate::server::dto::agents::{
    AgentDispatchedSessionsResponse, AgentOfficesResponse, AgentSkillsResponse,
    AgentTimelineResponse, AgentTranscriptsResponse,
};
use crate::services::agents::query::{
    AgentQueryLookupError, agent_exists_pg, block_active_card_for_agent_pg, find_diag_session_pg,
    list_agent_offices_pg_json, list_agent_skills_pg_json, load_agent_dispatched_sessions_pg_json,
    load_agent_timeline_pg_json, mark_session_disconnected_pg,
};
use crate::services::agents::turn::{
    AgentTurnLookupError, capture_recent_tmux_output, collect_turn_tool_events, extract_tmux_name,
    find_agent_turn_session_pg, inflight_recent_output, list_agent_turn_history_pg_json,
    load_agent_turn_status_pg, load_inflight_snapshot, loop_suspicion,
    parse_local_timestamp_to_unix,
};
use crate::services::observability::session_inventory::{
    derive_visual_status, load_child_inventory_by_parent_key_pg,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, stop_turn_preserving_queue};
use crate::utils::api::{bad_request, internal_error, not_found};

// ── Query types ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TimelineQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TranscriptQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct AgentQualityQuery {
    pub days: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct AgentQualityRankingQuery {
    pub limit: Option<usize>,
    /// Which metric to rank by. One of `turn_success_rate` (default) or
    /// `review_pass_rate`.
    pub metric: Option<String>,
    /// Which rolling window to use. One of `7d` (default) or `30d`.
    pub window: Option<String>,
    /// Override the minimum sample_size threshold. Defaults to 5
    /// (`QUALITY_SAMPLE_GUARD`).
    pub min_sample_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct StartAgentTurnBody {
    pub prompt: String,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub source: Option<String>,
    /// Optional provider override: "claude" or "codex".
    /// When set, the turn runs on that provider's channel binding instead
    /// of the agent's primary channel — lets external babysitters drive
    /// either side without going through the command bot.
    #[serde(default)]
    pub provider: Option<String>,
    /// Optional explicit channel override (Discord channel id or alias).
    /// Takes precedence over `provider` when both are set.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Optional Discord user id. When set, the turn is bound to the
    /// agent's primary bot DM with that user instead of a guild channel.
    #[serde(default)]
    pub dm_user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentMessageBody {
    pub from_agent_id: String,
    pub message: String,
    #[serde(default)]
    pub channel_kind: Option<String>,
    #[serde(default)]
    pub prefix: Option<bool>,
}

fn pg_required_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

/// GET /api/agents/{id}/quality
pub async fn agent_quality(
    Path(id): Path<String>,
    Query(query): Query<AgentQualityQuery>,
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match crate::services::observability::query_agent_quality_summary(
        state.pg_pool_ref(),
        &id,
        query.days.unwrap_or(30),
        query.limit.unwrap_or(60),
    )
    .await
    {
        Ok(summary) => (StatusCode::OK, Json(json!(summary))),
        Err(error) => internal_error(format!("query agent quality summary: {error}")),
    }
}

/// GET /api/agents/quality/ranking
pub async fn agents_quality_ranking(
    Query(query): Query<AgentQualityRankingQuery>,
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::services::observability::{QualityRankingMetric, QualityRankingWindow};
    let metric = QualityRankingMetric::parse(query.metric.as_deref());
    let window = QualityRankingWindow::parse(query.window.as_deref());
    let min_sample_size = query.min_sample_size.unwrap_or(5);
    match crate::services::observability::query_agent_quality_ranking_with(
        state.pg_pool_ref(),
        query.limit.unwrap_or(50),
        metric,
        window,
        min_sample_size,
    )
    .await
    {
        Ok(ranking) => (StatusCode::OK, Json(json!(ranking))),
        Err(error) => internal_error(format!("query agent quality ranking: {error}")),
    }
}

fn resolve_channel_identifier(value: &str) -> Option<u64> {
    super::dispatches::resolve_channel_alias_pub(value).or_else(|| value.trim().parse::<u64>().ok())
}

fn channel_identifier_matches(left: &str, right: &str) -> bool {
    let left_trimmed = left.trim();
    let right_trimmed = right.trim();
    if left_trimmed.eq_ignore_ascii_case(right_trimmed) {
        return true;
    }

    match (
        resolve_channel_identifier(left_trimmed),
        resolve_channel_identifier(right_trimmed),
    ) {
        (Some(left_id), Some(right_id)) => left_id == right_id,
        _ => false,
    }
}

fn channel_override_is_allowed(
    override_channel: &str,
    bindings: &crate::db::agents::AgentChannelBindings,
) -> bool {
    bindings
        .all_channels()
        .into_iter()
        .any(|channel| channel_identifier_matches(&channel, override_channel))
}

// ── Handlers ─────────────────────────────────────────────────

/// GET /api/agents/diag/:identifier
pub async fn agent_diag(
    State(state): State<AppState>,
    Path(identifier): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };

    let session = match find_diag_session_pg(pool, &identifier).await {
        Ok(Some(session)) => session,
        Ok(None) => {
            return not_found("agent/channel session not found");
        }
        Err(error) => {
            return internal_error(format!("query diag session: {error}"));
        }
    };

    let now = Utc::now();
    let last_tool_elapsed_secs = session
        .last_tool_at
        .map(|last| now.signed_duration_since(last).num_seconds().max(0));

    let tmux_name = extract_tmux_name(&session.session_key);
    let tui_prompt_readiness = tui_prompt_readiness_json(
        session.provider.as_deref(),
        tmux_name.as_deref(),
        session.cwd.as_deref(),
        session.provider_session_id.as_deref(),
    );
    let inflight = load_inflight_snapshot(session.provider.as_deref(), tmux_name.as_deref());
    let recent_output = tmux_name
        .as_deref()
        .and_then(capture_recent_tmux_output)
        .or_else(|| inflight.as_ref().and_then(inflight_recent_output));
    let events = collect_turn_tool_events(recent_output.as_deref(), inflight.as_ref());
    let last_tool = events.iter().rev().find(|event| event.kind == "tool");
    let child_inventory = load_child_inventory_by_parent_key_pg(pool, &session.session_key)
        .await
        .unwrap_or_default();
    let effective_active_children =
        child_inventory.effective_active_children(session.active_children);
    let visual = derive_visual_status(
        session.status.as_deref(),
        session.last_tool_at,
        effective_active_children,
        now,
    );
    let oldest_child_spawned_at = child_inventory
        .alive
        .iter()
        .filter_map(|child| child.spawned_at)
        .min()
        .map(|value| value.to_rfc3339());

    // #1671: surface `relay_stall_state`, `pending_queue_depth`,
    // `inflight_age_secs`, and `task_notification_kind` directly on the diag
    // payload. Operators previously had to call
    // `/api/channels/{id}/watcher-state` to see these signals; folding them
    // into `agentdesk diag` shortens the same-class incident playbook.
    //
    // codex P2 — scope the watcher-state snapshot to *this* session's
    // provider. The unscoped helper returns the FIRST registered provider
    // that knows the channel, so when multiple providers share a Discord
    // channel the diag would surface another runtime's state for the same
    // channel (silently misleading). When the session row has no provider
    // recorded, fall back to the unscoped lookup so we still report
    // something useful instead of forcing a hard `null`.
    let session_provider_kind = session.provider.as_deref().and_then(ProviderKind::from_str);
    let watcher_snapshot = match (
        state.health_registry.as_ref(),
        session
            .thread_channel_id
            .as_deref()
            .and_then(|raw| raw.trim().parse::<u64>().ok()),
    ) {
        (Some(registry), Some(channel_num)) => match session_provider_kind {
            Some(provider) => {
                registry
                    .snapshot_watcher_state_for_provider(&provider, channel_num)
                    .await
            }
            None => registry.snapshot_watcher_state(channel_num).await,
        },
        _ => None,
    };
    let watcher_snapshot_json = watcher_snapshot
        .as_ref()
        .and_then(|snapshot| serde_json::to_value(snapshot).ok());
    let relay_stall_state = watcher_snapshot_json
        .as_ref()
        .and_then(|value| value.get("relay_stall_state").cloned());
    let pending_queue_depth = watcher_snapshot_json
        .as_ref()
        .and_then(|value| value.get("relay_health"))
        .and_then(|value| value.get("queue_depth"))
        .and_then(serde_json::Value::as_u64);
    let inflight_age_secs = inflight
        .as_ref()
        .and_then(|state| state.updated_at.as_deref())
        .and_then(parse_local_timestamp_to_unix)
        .map(|unix| Utc::now().timestamp().saturating_sub(unix).max(0));
    let task_notification_kind = inflight
        .as_ref()
        .and_then(|state| state.task_notification_kind.clone());

    (
        StatusCode::OK,
        Json(json!({
            "target": identifier,
            "agent_id": session.agent_id,
            "agent_name": session.agent_name,
            "provider": session.provider,
            "session_key": session.session_key,
            "status": session.status,
            "visual_status": visual.display(),
            "visual_status_emoji": visual.emoji(),
            "visual_status_code": visual.code(),
            "thread_channel_id": session.thread_channel_id,
            "created_at": session.created_at.map(|value| value.to_rfc3339()),
            "last_tool_at": session.last_tool_at.map(|value| value.to_rfc3339()),
            "last_tool_elapsed_secs": last_tool_elapsed_secs,
            "active_children": effective_active_children,
            "recorded_active_children": session.active_children,
            "oldest_child_spawned_at": oldest_child_spawned_at,
            "children": child_inventory,
            "tui_prompt_readiness": tui_prompt_readiness,
            "last_tool": last_tool.map(|event| json!({
                "tool_name": event.tool_name,
                "summary": event.summary,
                "status": event.status,
                "line": event.line,
            })),
            "recent_loop_suspicion": loop_suspicion(&events),
            // #1671 — observability fields lifted from the watcher-state
            // endpoint. `null` when the registry/channel is unavailable.
            "relay_stall_state": relay_stall_state,
            "inflight_age_secs": inflight_age_secs,
            "pending_queue_depth": pending_queue_depth,
            "task_notification_kind": task_notification_kind,
        })),
    )
}

#[cfg(unix)]
fn tui_prompt_readiness_json(
    provider: Option<&str>,
    tmux_name: Option<&str>,
    cwd: Option<&str>,
    provider_session_id: Option<&str>,
) -> Option<Value> {
    let tmux_name = tmux_name.map(str::trim).filter(|value| !value.is_empty())?;
    match provider
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "claude" => {
            let snapshot = crate::services::claude_tui::input::prompt_readiness_snapshot(tmux_name);
            let pane_ready = snapshot.tmux_pane_alive
                && snapshot.prompt_marker_detected
                && !snapshot.prompt_draft_detected;
            let transcript_state = claude_transcript_turn_state_for_diag(cwd, provider_session_id);
            let transcript_ready = snapshot.tmux_pane_alive
                && !snapshot.prompt_draft_detected
                && transcript_state == Some(crate::services::tui_turn_state::TuiTurnState::Idle);
            Some(json!({
                "kind": "claude-tui",
                "ready_for_input": pane_ready || transcript_ready,
                "prompt_marker_detected": snapshot.prompt_marker_detected,
                "prompt_draft_detected": snapshot.prompt_draft_detected,
                "tmux_pane_alive": snapshot.tmux_pane_alive,
                "capture_available": snapshot.capture_available,
                "transcript_turn_state": transcript_state.map(|state| state.as_str()),
                "pane_tail": snapshot.pane_tail,
            }))
        }
        "codex" => {
            let snapshot = crate::services::codex_tui::input::prompt_readiness_snapshot(tmux_name);
            Some(json!({
                "kind": "codex-tui",
                "ready_for_input": snapshot.tmux_pane_alive
                    && snapshot.composer_marker_detected
                    && !snapshot.prompt_draft_detected,
                "prompt_marker_detected": snapshot.composer_marker_detected,
                "prompt_draft_detected": snapshot.prompt_draft_detected,
                "tmux_pane_alive": snapshot.tmux_pane_alive,
                "capture_available": snapshot.capture_available,
                "pane_tail": snapshot.pane_tail,
            }))
        }
        _ => None,
    }
}

#[cfg(not(unix))]
fn tui_prompt_readiness_json(
    _provider: Option<&str>,
    _tmux_name: Option<&str>,
    _cwd: Option<&str>,
    _provider_session_id: Option<&str>,
) -> Option<Value> {
    None
}

#[cfg(unix)]
fn claude_transcript_turn_state_for_diag(
    cwd: Option<&str>,
    provider_session_id: Option<&str>,
) -> Option<crate::services::tui_turn_state::TuiTurnState> {
    let cwd = cwd.map(str::trim).filter(|value| !value.is_empty())?;
    let provider_session_id = provider_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        FsPath::new(cwd),
        provider_session_id,
        None,
    )
    .ok()?;
    Some(crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(&path))
}

/// GET /api/agents/:id/offices
pub async fn agent_offices(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
    match agent_exists_pg(pool, &id).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "agent not found"})),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query: {e}")})),
            );
        }
    }

    match list_agent_offices_pg_json(pool, &id).await {
        Ok(offices) => (
            StatusCode::OK,
            Json(json!(AgentOfficesResponse { offices })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query: {e}")})),
        ),
    }
}

/// GET /api/agents/:id/skills
pub async fn agent_skills(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
    match agent_exists_pg(pool, &id).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "agent not found"})),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query: {e}")})),
            );
        }
    }

    match list_agent_skills_pg_json(pool, &id).await {
        Ok(skills) => {
            let total_count = skills.len();
            (
                StatusCode::OK,
                Json(json!(AgentSkillsResponse {
                    skills,
                    shared_skills: Vec::new(),
                    total_count,
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query: {e}")})),
        ),
    }
}

/// GET /api/agents/:id/dispatched-sessions
pub async fn agent_dispatched_sessions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };

    let guild_id = state.config.discord.guild_id.as_deref();
    match load_agent_dispatched_sessions_pg_json(pool, &id, guild_id).await {
        Ok(sessions) => (
            StatusCode::OK,
            Json(json!(AgentDispatchedSessionsResponse { sessions })),
        ),
        Err(AgentQueryLookupError::AgentNotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        ),
        Err(AgentQueryLookupError::Query(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query: {e}")})),
        ),
    }
}

/// GET /api/agents/:id/turn
pub async fn agent_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
    match load_agent_turn_status_pg(pool, &id).await {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err(AgentTurnLookupError::AgentNotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        ),
        Err(AgentTurnLookupError::Query(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query: {error}")})),
        ),
    }
}

/// POST /api/agents/:id/turn/start
pub async fn start_agent_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StartAgentTurnBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "prompt is required"})),
        );
    }

    let provider_override = body
        .provider
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let channel_override = body
        .channel_id
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let dm_user_id = body
        .dm_user_id
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let dm_user_id_num = if let Some(dm_user_id) = dm_user_id.as_deref() {
        if channel_override.is_some() || provider_override.is_some() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": "dm_user_id cannot be combined with provider or channel_id overrides",
                })),
            );
        }
        match dm_user_id.parse::<u64>().ok().filter(|id| *id > 0) {
            Some(id) => Some(id),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "ok": false,
                        "error": "dm_user_id must be a Discord snowflake string",
                    })),
                );
            }
        }
    } else {
        None
    };

    // Security: this endpoint runs the turn as the path agent `:id`. Client
    // metadata must not be able to rebind the turn to another agent's identity
    // via the routine `agent_id` field (which `routine_metadata_role_binding`
    // would otherwise honor with precedence), so reject a mismatch. The
    // in-process routine executor does not go through this HTTP handler and
    // sets its own server-trusted metadata.
    if let Some(metadata_agent_id) = body
        .metadata
        .as_ref()
        .and_then(|value| value.get("agent_id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && metadata_agent_id != id.trim()
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": format!(
                    "metadata.agent_id '{metadata_agent_id}' does not match requested agent '{}'",
                    id.trim()
                ),
            })),
        );
    }

    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "postgres pool unavailable"})),
        );
    };
    let (provider, primary_channel) = {
        match agent_exists_pg(pool, &id).await {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"ok": false, "error": "agent not found"})),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok": false, "error": format!("query: {error}")})),
                );
            }
        }

        let Some(bindings) = crate::db::agents::load_agent_channel_bindings_pg(pool, &id)
            .await
            .map_err(|error| error.to_string())
            .ok()
            .flatten()
        else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"ok": false, "error": "agent channel binding not found"})),
            );
        };

        if let Some(channel_override) = channel_override.as_deref()
            && !channel_override_is_allowed(channel_override, &bindings)
        {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "ok": false,
                    "error": format!(
                        "channel override {} is not allowed for agent {}",
                        channel_override,
                        id
                    ),
                })),
            );
        }

        let provider = match provider_override.as_deref() {
            Some(raw) => match ProviderKind::from_str(raw) {
                Some(kind) => kind,
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "ok": false,
                            "error": format!("unsupported provider override: {raw}"),
                        })),
                    );
                }
            },
            None => {
                let Some(kind) = bindings.resolved_primary_provider_kind() else {
                    return (
                        StatusCode::CONFLICT,
                        Json(
                            json!({"ok": false, "error": "agent primary provider is not configured"}),
                        ),
                    );
                };
                kind
            }
        };

        let primary_channel = if let Some(chan) = channel_override.clone() {
            chan
        } else if provider_override.is_some() {
            let Some(chan) = bindings.channel_for_provider(provider_override.as_deref()) else {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "ok": false,
                        "error": format!(
                            "agent has no channel bound for provider {}",
                            provider_override.as_deref().unwrap_or("")
                        ),
                    })),
                );
            };
            chan
        } else {
            let Some(chan) = bindings.primary_channel() else {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"ok": false, "error": "agent primary channel is not configured"})),
                );
            };
            chan
        };

        (provider, primary_channel)
    };

    let Some(channel_id_num) = super::dispatches::resolve_channel_alias_pub(&primary_channel)
        .or_else(|| primary_channel.parse::<u64>().ok())
    else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "ok": false,
                "error": format!("agent primary channel is invalid: {}", primary_channel),
            })),
        );
    };

    let Some(registry) = state.health_registry.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "discord runtime health registry unavailable"})),
        );
    };

    let channel_name_hint = primary_channel
        .chars()
        .all(|ch| ch.is_ascii_digit())
        .then_some(None)
        .unwrap_or_else(|| Some(primary_channel.clone()));

    let start_result = if let Some(dm_user_id_num) = dm_user_id_num {
        let metadata = metadata_with_parent_channel_id(body.metadata, channel_id_num);
        crate::services::discord::health::start_headless_agent_turn_in_dm(
            registry,
            poise::serenity_prelude::ChannelId::new(channel_id_num),
            dm_user_id_num,
            provider,
            prompt.to_string(),
            body.source,
            metadata,
        )
        .await
    } else {
        crate::services::discord::health::start_headless_agent_turn(
            registry,
            poise::serenity_prelude::ChannelId::new(channel_id_num),
            provider,
            prompt.to_string(),
            body.source,
            body.metadata,
            channel_name_hint,
        )
        .await
    };

    match start_result {
        Ok(outcome) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "turn_id": outcome.turn_id,
                "status": outcome.status.as_str(),
            })),
        ),
        Err(crate::services::discord::HeadlessTurnStartError::Conflict(error)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": error,
                "status": "conflict",
            })),
        ),
        Err(crate::services::discord::HeadlessTurnStartError::Internal(error)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "ok": false,
                "error": error,
            })),
        ),
    }
}

fn metadata_with_parent_channel_id(
    metadata: Option<serde_json::Value>,
    parent_channel_id: u64,
) -> Option<serde_json::Value> {
    let parent_channel_id = parent_channel_id.to_string();
    match metadata {
        Some(serde_json::Value::Object(mut object)) => {
            object
                .entry("parent_channel_id")
                .or_insert_with(|| serde_json::Value::String(parent_channel_id));
            Some(serde_json::Value::Object(object))
        }
        Some(value) => Some(json!({
            "trigger_metadata": value,
            "parent_channel_id": parent_channel_id,
        })),
        None => Some(json!({
            "parent_channel_id": parent_channel_id,
        })),
    }
}

/// POST /api/agents/:id/turn/stop
pub async fn stop_agent_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
    let session = {
        match agent_exists_pg(pool, &id).await {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "agent not found"})),
                );
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }

        match find_agent_turn_session_pg(pool, &id).await {
            Ok(session) => session,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query: {e}")})),
                );
            }
        }
    };

    let Some(session) = session.filter(|candidate| candidate.is_working) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "no active turn found for agent",
                "agent_id": id,
                "status": "idle",
            })),
        );
    };

    if session.session_key.trim().is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "active session is missing session_key"})),
        );
    }

    let session_key = session.session_key.clone();
    let tmux_name = session_key.split(':').next_back().unwrap_or(&session_key);
    let lifecycle = stop_turn_preserving_queue(
        state.health_registry.as_deref(),
        &TurnLifecycleTarget {
            provider: session.provider.as_deref().and_then(ProviderKind::from_str),
            channel_id: session
                .runtime_channel_id
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
                .map(poise::serenity_prelude::ChannelId::new),
            tmux_name: tmux_name.to_string(),
        },
        &format!("사용자가 {id} 에이전트 턴 수동 중단 (POST /api/agents/{id}/turn/stop)"),
    )
    .await;

    mark_session_disconnected_pg(pool, &session_key).await;

    let status = StatusCode::OK;
    let Json(mut body) = Json(json!({
        "ok": true,
        "session_key": session_key,
        "tmux_session": tmux_name,
        "tmux_killed": lifecycle.tmux_killed,
        "lifecycle_path": lifecycle.lifecycle_path,
        "queued_remaining": lifecycle.queue_depth,
        "queue_preserved": lifecycle.queue_preserved,
    }));
    body["agent_id"] = json!(id);
    body["session_key"] = json!(session_key);
    body["status"] = json!(if status == StatusCode::OK {
        "stopped"
    } else {
        "error"
    });
    (status, Json(body))
}

/// GET /api/agents/:id/timeline?limit=30
pub async fn agent_timeline(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<TimelineQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };

    let limit = params.limit.unwrap_or(30);
    match load_agent_timeline_pg_json(pool, &id, limit).await {
        Ok(events) => (
            StatusCode::OK,
            Json(json!(AgentTimelineResponse { events })),
        ),
        Err(AgentQueryLookupError::AgentNotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "agent not found"})),
        ),
        Err(AgentQueryLookupError::Query(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query: {e}")})),
        ),
    }
}

/// GET /api/agents/:id/transcripts?limit=10
pub async fn agent_transcripts(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<TranscriptQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
    match agent_exists_pg(pool, &id).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "agent not found"})),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query: {e}")})),
            );
        }
    }

    match list_agent_turn_history_pg_json(pool, &id, params.limit.unwrap_or(8)).await {
        Ok(transcripts) => (
            StatusCode::OK,
            Json(json!(AgentTranscriptsResponse {
                agent_id: id,
                transcripts,
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("transcripts: {e}")})),
        ),
    }
}

/// POST /api/agents/:id/signal
/// Agent sends an operational signal (e.g., "blocked" with reason).
pub async fn agent_signal(
    State(state): State<super::AppState>,
    axum::extract::Path(agent_id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let signal = body.get("signal").and_then(|v| v.as_str()).unwrap_or("");
    let reason = body.get("reason").and_then(|v| v.as_str()).unwrap_or("");

    if signal != "blocked" {
        return bad_request(format!("unknown signal: {signal}. supported: blocked"));
    }

    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };

    let card_id = match block_active_card_for_agent_pg(pool, &agent_id, reason).await {
        Ok(card_id) => card_id,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query: {error}")})),
            );
        }
    };

    let Some(card_id) = card_id else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no active card for agent"})),
        );
    };

    (
        StatusCode::OK,
        Json(json!({"ok": true, "card_id": card_id, "signal": signal})),
    )
}

/// POST /api/agents/:id/message
/// Send a trigger-capable agent-to-agent handoff through the announce bot.
pub async fn agent_message(
    State(state): State<super::AppState>,
    axum::extract::Path(to_agent_id): axum::extract::Path<String>,
    axum::Json(body): axum::Json<AgentMessageBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_required_response();
    };
    let Some(registry) = state.health_registry.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "Discord not available (standalone mode)"})),
        );
    };

    let channel_kind = match crate::services::discord::agent_handoff::AgentHandoffChannelKind::parse(
        body.channel_kind.as_deref(),
    ) {
        Ok(channel_kind) => channel_kind,
        Err(error) => return (error.status(), Json(error.body())),
    };

    match crate::services::discord::agent_handoff::send_agent_handoff(
        registry,
        pool,
        &body.from_agent_id,
        &to_agent_id,
        &body.message,
        channel_kind,
        body.prefix.unwrap_or(true),
    )
    .await
    {
        Ok(response) => (StatusCode::OK, Json(response.to_value())),
        Err(error) => (error.status(), Json(error.body())),
    }
}
