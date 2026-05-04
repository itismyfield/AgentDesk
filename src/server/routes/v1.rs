use axum::{
    Json, Router, body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, patch},
};
use chrono::{DateTime, Duration, Local, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::{collections::HashMap, convert::Infallible, time::Duration as StdDuration};

use super::{
    ApiRouter, AppState, health_api, protected_api_domain,
    session_activity::SessionActivityResolver, settings,
};
use crate::db::session_status::is_active_status;
use crate::utils::api::clamp_api_limit;

const CACHE_OVERVIEW: &str = "max-age=30";
const CACHE_AGENTS: &str = "max-age=10";
const CACHE_TOKENS: &str = "max-age=60";
const CACHE_KANBAN: &str = "max-age=5";
const CACHE_OPS: &str = "max-age=5";
const CACHE_ACTIVITY: &str = "max-age=5";
const CACHE_ACHIEVEMENTS: &str = "max-age=300";
const CACHE_SETTINGS: &str = "no-store";

const ACHIEVEMENT_MILESTONES: &[(i64, &str, &str, &str)] = &[
    (10, "first_task", "첫 번째 작업 완료", "common"),
    (50, "getting_started", "본격적인 시작", "uncommon"),
    (100, "centurion", "100 XP 달성", "rare"),
    (250, "veteran", "베테랑", "epic"),
    (500, "expert", "전문가", "legendary"),
    (1000, "master", "마스터", "mythic"),
];

#[derive(Debug, Deserialize, Default)]
struct AgentsQuery {
    #[serde(rename = "officeId")]
    office_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokensQuery {
    range: Option<String>,
    period: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ActivityQuery {
    limit: Option<usize>,
    before: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AchievementsQuery {
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PatchSettingBody {
    value: Value,
}

#[derive(Debug, Clone)]
struct AgentActivitySnapshot {
    id: String,
    status: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionSnapshot {
    session_key: Option<String>,
    agent_id: String,
    status: Option<String>,
    active_dispatch_id: Option<String>,
    last_heartbeat: Option<String>,
}

#[derive(Debug, Clone)]
struct ActivityItem {
    timestamp_ms: i64,
    cursor_id: String,
    body: Value,
}

#[derive(Debug, Clone)]
struct AchievementBundle {
    achievements: Vec<Value>,
    events: Vec<ActivityItem>,
    daily_missions: Vec<Value>,
}

#[derive(Debug, Clone)]
struct CursorMarker {
    timestamp_ms: i64,
    cursor_id: String,
}

#[derive(Debug, Clone)]
struct StreamEnvelope {
    id: String,
    event: String,
    data: Value,
}

pub(crate) fn router(state: AppState) -> ApiRouter {
    protected_api_domain(
        Router::new()
            .route("/v1/overview", get(overview))
            .route("/v1/agents", get(list_agents))
            .route("/v1/tokens", get(tokens))
            .route("/v1/kanban", get(kanban))
            .route("/v1/ops/health", get(ops_health))
            .route("/v1/stream", get(stream))
            .route("/v1/activity", get(activity))
            .route("/v1/achievements", get(achievements))
            .route("/v1/settings", get(get_settings))
            .route("/v1/settings/{key}", patch(patch_setting)),
        state,
    )
}

async fn overview(State(state): State<AppState>) -> Response {
    let health_payload = match load_health_payload(state.clone()).await {
        Ok((_, payload)) => payload,
        Err(response) => return response,
    };
    let token_payload = load_token_payload("7d");
    let spark_14d = match load_overview_spark(state.clone()).await {
        Ok(points) => points,
        Err(response) => return response,
    };
    let session_count = match load_session_count(state.clone()).await {
        Ok(count) => count,
        Err(response) => return response,
    };
    let agent_counts = match load_agent_counts(state.clone()).await {
        Ok(counts) => counts,
        Err(response) => return response,
    };
    let kanban_summary = match load_kanban_summary(state.clone()).await {
        Ok(summary) => summary,
        Err(response) => return response,
    };
    let providers = health_payload["providers"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let body = json!({
        "generated_at": utc_now_iso(),
        "providers": providers,
        "session_count": session_count,
        "metrics": {
            "agents": agent_counts,
            "kanban": {
                "open_total": kanban_summary["open_total"],
                "review_queue": kanban_summary["review_queue"],
                "blocked": kanban_summary["blocked"],
                "failed": kanban_summary["failed"],
                "waiting_acceptance": kanban_summary["waiting_acceptance"],
                "stale_in_progress": kanban_summary["stale_in_progress"],
            },
            "dispatch": {
                "auto_queue": kanban_summary["auto_queue"],
                "wip_limit": kanban_summary["wip_limit"],
            },
            "tokens_7d": token_payload["summary"],
        },
        "spark_14d": spark_14d,
    });

    json_response(StatusCode::OK, CACHE_OVERVIEW, body)
}

async fn list_agents(State(state): State<AppState>, Query(query): Query<AgentsQuery>) -> Response {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable("list_agents");
    };
    let agents = match load_agents_pg(pool, query.office_id.as_deref()).await {
        Ok(agents) => agents,
        Err(error) => return internal_error("list_agents_pg", &error),
    };

    json_response(
        StatusCode::OK,
        CACHE_AGENTS,
        json!({
            "agents": agents,
            "generated_at": utc_now_iso(),
        }),
    )
}

async fn tokens(_state: State<AppState>, Query(query): Query<TokensQuery>) -> Response {
    let range = query
        .range
        .or(query.period)
        .unwrap_or_else(|| "30d".to_string());
    let payload = load_token_payload(&range);
    json_response(StatusCode::OK, CACHE_TOKENS, payload)
}

async fn kanban(State(state): State<AppState>) -> Response {
    let payload = match load_kanban_summary(state).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    json_response(StatusCode::OK, CACHE_KANBAN, payload)
}

async fn ops_health(State(state): State<AppState>) -> Response {
    let (status, mut payload) = match load_health_payload(state).await {
        Ok(data) => data,
        Err(response) => return response,
    };
    payload["bottlenecks"] = Value::Array(build_bottlenecks(&payload));
    json_response(status, CACHE_OPS, payload)
}

async fn stream(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let replay = last_event_id
        .as_deref()
        .map(|last_id| state.broadcast_tx.replay_since(last_id))
        .unwrap_or_default();
    let snapshot = if last_event_id.is_some() && !replay.is_empty() {
        Vec::new()
    } else {
        match build_stream_snapshot(state.clone()).await {
            Ok(snapshot) => snapshot,
            Err(response) => return response,
        }
    };
    let live_stream = stream::unfold(state.broadcast_tx.subscribe(), |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Some(mapped) = map_live_stream_event(event) {
                        break Some((Ok::<_, Infallible>(to_sse_event(mapped)), rx));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "sse stream lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break None,
            }
        }
    });
    let initial_stream = stream::iter(
        snapshot
            .into_iter()
            .chain(
                replay
                    .into_iter()
                    .filter_map(map_live_stream_event)
                    .collect::<Vec<_>>(),
            )
            .map(|event| Ok::<_, Infallible>(to_sse_event(event))),
    );
    let sse = Sse::new(initial_stream.chain(live_stream)).keep_alive(
        KeepAlive::new()
            .interval(StdDuration::from_secs(15))
            .text("keepalive"),
    );

    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        sse,
    )
        .into_response()
}

async fn activity(State(state): State<AppState>, Query(query): Query<ActivityQuery>) -> Response {
    let limit = clamp_api_limit(Some(query.limit.unwrap_or(20)));
    let before = query.before.as_deref().and_then(parse_cursor);
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable("activity");
    };
    let mut items = match load_activity_items_pg(pool).await {
        Ok(items) => items,
        Err(error) => return internal_error("activity_pg", &error),
    };

    items.sort_by(|left, right| {
        right
            .timestamp_ms
            .cmp(&left.timestamp_ms)
            .then_with(|| right.cursor_id.cmp(&left.cursor_id))
    });

    if let Some(before) = before.as_ref() {
        items.retain(|item| {
            item.timestamp_ms < before.timestamp_ms
                || (item.timestamp_ms == before.timestamp_ms && item.cursor_id < before.cursor_id)
        });
    }

    let page: Vec<ActivityItem> = items.into_iter().take(limit).collect();
    let next_cursor = page
        .last()
        .map(|item| format!("{}|{}", item.timestamp_ms, item.cursor_id));

    json_response(
        StatusCode::OK,
        CACHE_ACTIVITY,
        json!({
            "items": page.into_iter().map(|item| item.body).collect::<Vec<_>>(),
            "next_cursor": next_cursor,
        }),
    )
}

async fn achievements(
    State(state): State<AppState>,
    Query(query): Query<AchievementsQuery>,
) -> Response {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable("achievements");
    };
    let bundle = match build_achievements_pg(pool, query.agent_id.as_deref()).await {
        Ok(bundle) => bundle,
        Err(error) => return internal_error("achievements_pg", &error),
    };

    json_response(
        StatusCode::OK,
        CACHE_ACHIEVEMENTS,
        json!({
            "achievements": bundle.achievements,
            "daily_missions": bundle.daily_missions,
        }),
    )
}

async fn get_settings(State(state): State<AppState>) -> Response {
    let (status, Json(body)) = settings::get_config_entries(State(state)).await;
    if status != StatusCode::OK {
        return map_legacy_error(status, &body, "settings_fetch");
    }

    let entries = body["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(enrich_setting_entry)
        .collect::<Vec<_>>();

    json_response(
        StatusCode::OK,
        CACHE_SETTINGS,
        json!({
            "entries": entries,
            "generated_at": utc_now_iso(),
        }),
    )
}

async fn patch_setting(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<PatchSettingBody>,
) -> Response {
    let patch = json!({ key.clone(): body.value });
    let (patch_status, Json(patch_body)) =
        settings::patch_config_entries(State(state.clone()), Json(patch)).await;
    if patch_status != StatusCode::OK {
        return map_legacy_error(patch_status, &patch_body, "settings_patch");
    }
    let updated = patch_body["updated"].as_i64().unwrap_or(0);
    let rejected = patch_body["rejected"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if updated != 1 {
        let message = if rejected
            .iter()
            .any(|value| value.as_str() == Some(key.as_str()))
        {
            format!("setting key `{key}` is not editable")
        } else {
            format!("setting key `{key}` was not updated")
        };
        return v1_error(
            StatusCode::BAD_REQUEST,
            "settings_patch_rejected",
            message,
            false,
        );
    }

    let (status, Json(entries_body)) = settings::get_config_entries(State(state)).await;
    if status != StatusCode::OK {
        return map_legacy_error(status, &entries_body, "settings_refetch");
    }

    let Some(entry) = entries_body["entries"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["key"].as_str() == Some(key.as_str()))
                .cloned()
        })
        .map(enrich_setting_entry)
    else {
        return v1_error(
            StatusCode::NOT_FOUND,
            "setting_not_found",
            format!("setting key `{key}` not found"),
            false,
        );
    };

    json_response(StatusCode::OK, CACHE_SETTINGS, entry)
}

fn to_sse_event(event: StreamEnvelope) -> Event {
    Event::default()
        .id(event.id)
        .event(event.event)
        .data(event.data.to_string())
}

fn snapshot_envelope(
    id: impl Into<String>,
    event: impl Into<String>,
    data: Value,
) -> StreamEnvelope {
    StreamEnvelope {
        id: id.into(),
        event: event.into(),
        data,
    }
}

async fn build_stream_snapshot(state: AppState) -> Result<Vec<StreamEnvelope>, Response> {
    let mut events = Vec::new();
    let generated_at = utc_now_iso();

    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable("stream_snapshot"));
    };
    let agents = load_agents_pg(pool, None)
        .await
        .map_err(|error| internal_error("stream_agents_pg", &error))?;
    for agent in &agents {
        if let Some(agent_id) = agent["id"].as_str() {
            events.push(snapshot_envelope(
                format!("snapshot:agent-status:{agent_id}"),
                "agent.status",
                json!({
                    "agent_id": agent_id,
                    "status": agent["status"].as_str().unwrap_or("idle"),
                    "task": compact_agent_task(agent),
                    "snapshot": true,
                    "generated_at": generated_at,
                }),
            ));
            events.push(snapshot_envelope(
                format!("snapshot:token:{agent_id}"),
                "token.tick",
                json!({
                    "agent_id": agent_id,
                    "delta_tokens": agent["stats_tokens"].as_i64().unwrap_or(0),
                    "delta_cost_usd": "0",
                    "snapshot": true,
                    "generated_at": generated_at,
                }),
            ));
        }
    }

    let (health_status, health_payload) = load_health_payload(state.clone()).await?;
    events.push(snapshot_envelope(
        "snapshot:ops-health",
        "ops.health",
        compact_ops_health_event(health_status, &health_payload, &generated_at),
    ));

    let achievements = build_achievements_pg(pool, None)
        .await
        .map_err(|error| internal_error("stream_achievements_pg", &error))?;
    for achievement in achievements.achievements.iter().take(8) {
        if let Some(achievement_id) = achievement["id"].as_str() {
            events.push(snapshot_envelope(
                format!("snapshot:achievement:{achievement_id}"),
                "achievement.unlocked",
                json!({
                    "achievement_id": achievement_id,
                    "xp": achievement["progress"]["current"].as_i64().unwrap_or(0),
                    "snapshot": true,
                    "generated_at": generated_at,
                }),
            ));
        }
    }

    let activity_items = load_activity_items_pg(pool)
        .await
        .map_err(|error| internal_error("stream_activity_pg", &error))?;
    for item in activity_items
        .into_iter()
        .filter(|item| item.body["kind"] == "kanban_transition")
        .take(8)
    {
        if let Some(event) = compact_kanban_transition_snapshot(&item.body, &generated_at) {
            events.push(event);
        }
    }

    Ok(events)
}

fn compact_agent_task(agent: &Value) -> Value {
    let Some(current_task) = agent.get("current_task") else {
        return Value::Null;
    };
    if current_task.is_null() {
        return Value::Null;
    }
    json!({
        "dispatch_id": current_task["dispatch_id"],
        "card_id": current_task["card_id"],
        "card_title": current_task["card_title"],
    })
}

fn compact_ops_health_event(status: StatusCode, payload: &Value, generated_at: &str) -> Value {
    let providers = payload["providers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|provider| {
            Some(json!({
                "name": provider.get("name")?.as_str()?,
                "healthy": provider.get("healthy").and_then(Value::as_bool).unwrap_or(false),
            }))
        })
        .take(6)
        .collect::<Vec<_>>();
    let reasons = payload["reasons"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|reason| reason.as_str().map(str::to_string))
        .take(6)
        .collect::<Vec<_>>();
    json!({
        "status": if status.is_success() { "ok" } else { "degraded" },
        "providers": providers,
        "reasons": reasons,
        "deferred_hooks": payload["deferred_hooks"].as_i64().unwrap_or(0),
        "snapshot": true,
        "generated_at": generated_at,
    })
}

fn compact_kanban_transition_snapshot(body: &Value, generated_at: &str) -> Option<StreamEnvelope> {
    let meta = body.get("meta")?;
    let card_id = meta.get("card_id").and_then(Value::as_str)?;
    Some(snapshot_envelope(
        format!("snapshot:kanban:{card_id}"),
        "kanban.transition",
        json!({
            "card_id": card_id,
            "from": meta.get("from_status").and_then(Value::as_str).unwrap_or("unknown"),
            "to": meta.get("to_status").and_then(Value::as_str).unwrap_or("unknown"),
            "at": body["created_at"].as_str().unwrap_or(generated_at),
            "snapshot": true,
            "generated_at": generated_at,
        }),
    ))
}

fn map_live_stream_event(event: crate::server::ws::BroadcastEvent) -> Option<StreamEnvelope> {
    let data = match event.event.as_str() {
        "agent.status"
        | "ops.health"
        | "achievement.unlocked"
        | "token.tick"
        | "kanban.transition" => event.data,
        "agent_status" => {
            let agent_id = event.data["id"].as_str()?;
            json!({
                "agent_id": agent_id,
                "status": event.data["status"].as_str().unwrap_or("idle"),
                "task": {
                    "dispatch_id": event.data["current_task_id"],
                },
            })
        }
        "dispatched_session_new" | "dispatched_session_update" => {
            let agent_id = event.data["linked_agent_id"].as_str()?;
            json!({
                "agent_id": agent_id,
                "status": event.data["status"].as_str().unwrap_or("idle"),
                "task": {
                    "dispatch_id": event.data["active_dispatch_id"],
                    "session_key": event.data["session_key"],
                },
            })
        }
        _ => event.data,
    };
    let event_name = match event.event.as_str() {
        "agent_status" | "dispatched_session_new" | "dispatched_session_update" => {
            "agent.status".to_string()
        }
        other => other.to_string(),
    };
    Some(StreamEnvelope {
        id: event.id,
        event: event_name,
        data,
    })
}

fn json_response(status: StatusCode, cache_control: &str, body: Value) -> Response {
    let cache_value = HeaderValue::from_str(cache_control)
        .unwrap_or_else(|_| HeaderValue::from_static("no-store"));
    (status, [(header::CACHE_CONTROL, cache_value)], Json(body)).into_response()
}

fn v1_error(
    status: StatusCode,
    code: impl Into<String>,
    message: impl Into<String>,
    retryable: bool,
) -> Response {
    let body = json!({
        "error": {
            "code": code.into(),
            "message": message.into(),
            "retryable": retryable,
        }
    });
    (status, Json(body)).into_response()
}

fn internal_error(operation: &str, message: &str) -> Response {
    v1_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        format!("{operation}: {message}"),
        true,
    )
}

fn pg_unavailable(operation: &str) -> Response {
    v1_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "postgres_unavailable",
        format!("{operation}: postgres pool unavailable"),
        true,
    )
}

fn map_legacy_error(status: StatusCode, body: &Value, fallback_code: &str) -> Response {
    let message = body["error"]
        .as_str()
        .or_else(|| body["message"].as_str())
        .unwrap_or("request failed");
    let code = body["code"].as_str().unwrap_or(fallback_code);
    v1_error(status, code, message, status.is_server_error())
}

async fn load_health_payload(state: AppState) -> Result<(StatusCode, Value), Response> {
    let response = health_api::health_handler(State(state)).await;
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|error| internal_error("health_body", &error.to_string()))?;
    let value = serde_json::from_slice::<Value>(&bytes)
        .map_err(|error| internal_error("health_json", &error.to_string()))?;
    Ok((status, value))
}

fn load_token_payload(range: &str) -> Value {
    let now = Utc::now();
    let local_now = now.with_timezone(&Local);
    let (days, label, period) = match range {
        "7d" => (7_i64, "Last 7 Days", "7d"),
        "90d" => (90_i64, "Last 90 Days", "90d"),
        _ => (30_i64, "Last 30 Days", "30d"),
    };
    let start_date = local_now.date_naive() - Duration::days(days.saturating_sub(1));
    let start = Local
        .from_local_datetime(&start_date.and_hms_opt(0, 0, 0).expect("valid midnight"))
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| now - Duration::days(days));
    let data = crate::receipt::collect_token_analytics(start, now, label, period);

    json!({
        "range": data.period,
        "period_label": data.period_label,
        "generated_at": normalize_datetime_to_iso(&data.generated_at).unwrap_or_else(utc_now_iso),
        "summary": {
            "total_tokens": data.summary.total_tokens,
            "total_cost": decimal_string(data.summary.total_cost),
            "cache_discount": decimal_string(data.summary.cache_discount),
            "total_messages": data.summary.total_messages,
            "total_sessions": data.summary.total_sessions,
            "active_days": data.summary.active_days,
            "average_daily_tokens": data.summary.average_daily_tokens,
            "peak_day": data.summary.peak_day.as_ref().map(|day| json!({
                "date": day.date,
                "total_tokens": day.total_tokens,
                "cost": decimal_string(day.cost),
            })),
        },
        "daily": data.daily.iter().map(|day| {
            json!({
                "date": day.date,
                "input_tokens": day.input_tokens,
                "output_tokens": day.output_tokens,
                "cache_read_tokens": day.cache_read_tokens,
                "cache_creation_tokens": day.cache_creation_tokens,
                "total_tokens": day.total_tokens,
                "cost": decimal_string(day.cost),
            })
        }).collect::<Vec<_>>(),
        "models": data.receipt.models.iter().map(|model| {
            json!({
                "model": model.model,
                "display_name": model.display_name,
                "provider": model.provider,
                "input_tokens": model.input_tokens,
                "output_tokens": model.output_tokens,
                "cache_read_tokens": model.cache_read_tokens,
                "cache_creation_tokens": model.cache_creation_tokens,
                "total_tokens": model.total_tokens,
                "cost": decimal_string(model.cost),
                "cost_without_cache": decimal_string(model.cost_without_cache),
            })
        }).collect::<Vec<_>>(),
        "per_agent": data.receipt.agents.iter().map(|agent| {
            json!({
                "agent": agent.agent,
                "tokens": agent.tokens,
                "cost": decimal_string(agent.cost),
                "cost_without_cache": decimal_string(agent.cost_without_cache),
                "input_tokens": agent.input_tokens,
                "cache_read_tokens": agent.cache_read_tokens,
                "cache_creation_tokens": agent.cache_creation_tokens,
                "percentage": decimal_string(agent.percentage),
            })
        }).collect::<Vec<_>>(),
    })
}

async fn load_overview_spark(state: AppState) -> Result<Vec<Value>, Response> {
    let tokens_14d = load_token_payload("30d");
    let token_daily = tokens_14d["daily"].as_array().cloned().unwrap_or_default();
    let mut token_map = HashMap::new();
    for day in token_daily.into_iter().rev().take(14) {
        if let Some(date) = day["date"].as_str() {
            token_map.insert(date.to_string(), day["total_tokens"].as_i64().unwrap_or(0));
        }
    }

    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable("overview_spark"));
    };
    let completed_map = load_completed_dispatch_counts_pg(pool, 14)
        .await
        .map_err(|error| internal_error("overview_spark_pg", &error))?;

    let mut dates = token_map.keys().cloned().collect::<Vec<_>>();
    for date in completed_map.keys() {
        if !dates.iter().any(|existing| existing == date) {
            dates.push(date.clone());
        }
    }
    dates.sort();
    if dates.len() > 14 {
        let start = dates.len().saturating_sub(14);
        dates = dates.split_off(start);
    }

    Ok(dates
        .into_iter()
        .map(|date| {
            json!({
                "date": date,
                "completed_dispatches": completed_map.get(&date).copied().unwrap_or(0),
                "token_total": token_map.get(&date).copied().unwrap_or(0),
            })
        })
        .collect())
}

async fn load_session_count(state: AppState) -> Result<i64, Response> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable("session_count"));
    };
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM sessions WHERE status IS DISTINCT FROM 'disconnected'",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| internal_error("session_count_pg", &error.to_string()))
}

async fn load_agent_counts(state: AppState) -> Result<Value, Response> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable("agent_counts"));
    };
    let agents = sqlx::query("SELECT id, status FROM agents ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(|error| internal_error("agent_counts_agents_pg", &error.to_string()))?
        .into_iter()
        .map(|row| AgentActivitySnapshot {
            id: row.try_get::<String, _>("id").unwrap_or_default(),
            status: row.try_get::<Option<String>, _>("status").ok().flatten(),
        })
        .collect::<Vec<_>>();
    let sessions = sqlx::query(
        "SELECT session_key, agent_id, status, active_dispatch_id, last_heartbeat
         FROM sessions
         WHERE agent_id IS NOT NULL
           AND status IS DISTINCT FROM 'disconnected'",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| internal_error("agent_counts_sessions_pg", &error.to_string()))?
    .into_iter()
    .map(|row| SessionSnapshot {
        session_key: row
            .try_get::<Option<String>, _>("session_key")
            .ok()
            .flatten(),
        agent_id: row.try_get::<String, _>("agent_id").unwrap_or_default(),
        status: row.try_get::<Option<String>, _>("status").ok().flatten(),
        active_dispatch_id: row
            .try_get::<Option<String>, _>("active_dispatch_id")
            .ok()
            .flatten(),
        last_heartbeat: row
            .try_get::<Option<String>, _>("last_heartbeat")
            .ok()
            .flatten(),
    })
    .collect::<Vec<_>>();

    let mut resolver = SessionActivityResolver::new();
    let mut working_agents = std::collections::HashSet::new();
    for session in sessions {
        let effective = resolver.resolve(
            session.session_key.as_deref(),
            session.status.as_deref(),
            session.active_dispatch_id.as_deref(),
            session.last_heartbeat.as_deref(),
        );
        if effective.is_working {
            working_agents.insert(session.agent_id);
        }
    }

    let mut working = 0_i64;
    let mut idle = 0_i64;
    let mut on_break = 0_i64;
    let mut offline = 0_i64;
    for agent in &agents {
        let effective_working = working_agents.contains(&agent.id)
            || agent.status.as_deref().is_some_and(is_active_status);
        if effective_working {
            working += 1;
            continue;
        }
        match agent.status.as_deref() {
            Some("break") => on_break += 1,
            Some("offline") => offline += 1,
            _ => idle += 1,
        }
    }

    Ok(json!({
        "total": agents.len() as i64,
        "working": working,
        "idle": idle,
        "break": on_break,
        "offline": offline,
    }))
}

async fn load_kanban_summary(state: AppState) -> Result<Value, Response> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(pg_unavailable("kanban_summary"));
    };
    let summary = load_kanban_summary_pg(pool)
        .await
        .map_err(|error| internal_error("kanban_summary_pg", &error))?;
    Ok(summary)
}

async fn load_kanban_summary_pg(pool: &sqlx::PgPool) -> Result<Value, String> {
    let statuses = [
        "backlog",
        "ready",
        "requested",
        "in_progress",
        "review",
        "failed",
        "done",
        "cancelled",
    ];
    let rows = sqlx::query(
        "SELECT status, COUNT(*)::BIGINT AS count
         FROM kanban_cards
         GROUP BY status",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load status counts: {error}"))?;
    let mut by_status = serde_json::Map::new();
    for status in statuses {
        let count = rows
            .iter()
            .find(|row| {
                row.try_get::<Option<String>, _>("status")
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(status)
            })
            .and_then(|row| row.try_get::<i64, _>("count").ok())
            .unwrap_or(0);
        by_status.insert(status.to_string(), json!(count));
    }
    let review_rows = sqlx::query("SELECT review_status, blocked_reason FROM kanban_cards")
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load review rows: {error}"))?;
    let blocked = review_rows
        .iter()
        .filter(|row| {
            crate::manual_intervention::requires_manual_intervention(
                row.try_get::<Option<String>, _>("review_status")
                    .ok()
                    .flatten()
                    .as_deref(),
                row.try_get::<Option<String>, _>("blocked_reason")
                    .ok()
                    .flatten()
                    .as_deref(),
            )
        })
        .count() as i64;
    let top_repos = sqlx::query(
        "SELECT repo_id, COUNT(*)::BIGINT AS count
         FROM kanban_cards
         WHERE repo_id IS NOT NULL
           AND status NOT IN ('done', 'cancelled')
         GROUP BY repo_id
         ORDER BY count DESC, repo_id ASC
         LIMIT 5",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load top repos: {error}"))?
    .into_iter()
    .map(|row| {
        let count = row.try_get::<i64, _>("count").unwrap_or(0);
        json!({
            "github_repo": row.try_get::<String, _>("repo_id").unwrap_or_default(),
            "open_count": count,
            "pressure_count": count,
        })
    })
    .collect::<Vec<_>>();
    Ok(build_kanban_payload(
        by_status,
        blocked,
        top_repos,
        load_auto_queue_summary_pg(pool).await?,
    ))
}

fn build_kanban_payload(
    by_status: serde_json::Map<String, Value>,
    blocked: i64,
    top_repos: Vec<Value>,
    auto_queue: Value,
) -> Value {
    let open_total = by_status
        .iter()
        .filter(|(status, _)| status.as_str() != "done" && status.as_str() != "cancelled")
        .map(|(_, count)| count.as_i64().unwrap_or(0))
        .sum::<i64>();
    let review_queue = by_status.get("review").and_then(Value::as_i64).unwrap_or(0);
    let failed = by_status.get("failed").and_then(Value::as_i64).unwrap_or(0);
    let waiting_acceptance = by_status
        .get("requested")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let stale_in_progress = 0_i64;

    json!({
        "generated_at": utc_now_iso(),
        "open_total": open_total,
        "review_queue": review_queue,
        "blocked": blocked,
        "failed": failed,
        "waiting_acceptance": waiting_acceptance,
        "stale_in_progress": stale_in_progress,
        "by_status": Value::Object(by_status),
        "top_repos": top_repos,
        "auto_queue": auto_queue,
        "wip_limit": Value::Null,
    })
}

async fn load_auto_queue_summary_pg(pool: &sqlx::PgPool) -> Result<Value, String> {
    let run = sqlx::query(
        "SELECT id, status, repo, agent_id, created_at::text AS created_at
         FROM auto_queue_runs
         ORDER BY created_at DESC NULLS LAST, id DESC
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load auto queue run: {error}"))?;
    let Some(run) = run else {
        return Ok(json!({
            "run": Value::Null,
            "pending": 0,
            "dispatched": 0,
            "done": 0,
            "failed": 0,
        }));
    };
    let run_id = run.try_get::<String, _>("id").unwrap_or_default();
    let counts = sqlx::query(
        "SELECT status, COUNT(*)::BIGINT AS count
         FROM auto_queue_entries
         WHERE run_id = $1
         GROUP BY status",
    )
    .bind(&run_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load auto queue entry counts: {error}"))?;
    Ok(json!({
        "run": {
            "id": run_id,
            "status": run.try_get::<String, _>("status").unwrap_or_else(|_| "active".to_string()),
            "repo": run.try_get::<Option<String>, _>("repo").ok().flatten(),
            "agent_id": run.try_get::<Option<String>, _>("agent_id").ok().flatten(),
            "created_at": normalize_datetime_to_iso(
                &run.try_get::<String, _>("created_at").unwrap_or_default()
            ),
        },
        "pending": find_status_count(&counts, "pending"),
        "dispatched": find_status_count(&counts, "dispatched"),
        "done": find_status_count(&counts, "done"),
        "failed": find_status_count(&counts, "failed"),
    }))
}

fn find_status_count(rows: &[sqlx::postgres::PgRow], status: &str) -> i64 {
    rows.iter()
        .find(|row| {
            row.try_get::<Option<String>, _>("status")
                .ok()
                .flatten()
                .as_deref()
                == Some(status)
        })
        .and_then(|row| row.try_get::<i64, _>("count").ok())
        .unwrap_or(0)
}

async fn load_agents_pg(
    pool: &sqlx::PgPool,
    office_id: Option<&str>,
) -> Result<Vec<Value>, String> {
    let rows = match office_id {
        Some(office_id) => {
            sqlx::query(
                "SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
                        a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
                        a.status, COALESCE(a.xp, 0)::BIGINT AS xp, a.sprite_number,
                        d.name AS department_name, d.name_ko AS department_name_ko, d.color AS department_color,
                        a.created_at::text AS created_at,
                        (SELECT COUNT(DISTINCT kc.id)::BIGINT FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
                        (SELECT COALESCE(SUM(s.tokens), 0)::BIGINT FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
                        (SELECT td2.id
                           FROM task_dispatches td2
                           JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                          WHERE td2.to_agent_id = a.id
                            AND kc.status = 'in_progress'
                          ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                          LIMIT 1) AS current_task_id,
                        (SELECT kc.id
                           FROM task_dispatches td2
                           JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                          WHERE td2.to_agent_id = a.id
                            AND kc.status = 'in_progress'
                          ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                          LIMIT 1) AS current_card_id,
                        (SELECT kc.title
                           FROM task_dispatches td2
                           JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                          WHERE td2.to_agent_id = a.id
                            AND kc.status = 'in_progress'
                          ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                          LIMIT 1) AS current_card_title
                 FROM agents a
                 INNER JOIN office_agents oa ON oa.agent_id = a.id
                 LEFT JOIN departments d ON d.id = a.department
                 WHERE oa.office_id = $1
                 ORDER BY a.id",
            )
            .bind(office_id)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(
                "SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
                        a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
                        a.status, COALESCE(a.xp, 0)::BIGINT AS xp, a.sprite_number,
                        d.name AS department_name, d.name_ko AS department_name_ko, d.color AS department_color,
                        a.created_at::text AS created_at,
                        (SELECT COUNT(DISTINCT kc.id)::BIGINT FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
                        (SELECT COALESCE(SUM(s.tokens), 0)::BIGINT FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
                        (SELECT td2.id
                           FROM task_dispatches td2
                           JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                          WHERE td2.to_agent_id = a.id
                            AND kc.status = 'in_progress'
                          ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                          LIMIT 1) AS current_task_id,
                        (SELECT kc.id
                           FROM task_dispatches td2
                           JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                          WHERE td2.to_agent_id = a.id
                            AND kc.status = 'in_progress'
                          ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                          LIMIT 1) AS current_card_id,
                        (SELECT kc.title
                           FROM task_dispatches td2
                           JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                          WHERE td2.to_agent_id = a.id
                            AND kc.status = 'in_progress'
                          ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                          LIMIT 1) AS current_card_title
                 FROM agents a
                 LEFT JOIN departments d ON d.id = a.department
                 ORDER BY a.id",
            )
            .fetch_all(pool)
            .await
        }
    }
    .map_err(|error| format!("query agents: {error}"))?;

    let agent_ids = rows
        .iter()
        .filter_map(|row| row.try_get::<String, _>("id").ok())
        .collect::<Vec<_>>();
    let skills_7d = load_skills_7d_pg(pool, &agent_ids).await;

    Ok(rows
        .into_iter()
        .map(|row| {
            let agent_id = row.try_get::<String, _>("id").unwrap_or_default();
            json!({
                "id": agent_id.clone(),
                "name": row.try_get::<String, _>("name").unwrap_or_default(),
                "name_ko": row.try_get::<Option<String>, _>("name_ko").ok().flatten(),
                "cli_provider": row.try_get::<Option<String>, _>("provider").ok().flatten(),
                "department_id": row.try_get::<Option<String>, _>("department").ok().flatten(),
                "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").ok().flatten(),
                "discord_channel_id": row.try_get::<Option<String>, _>("discord_channel_id").ok().flatten(),
                "discord_channel_alt": row.try_get::<Option<String>, _>("discord_channel_alt").ok().flatten(),
                "discord_channel_cc": row.try_get::<Option<String>, _>("discord_channel_cc").ok().flatten(),
                "discord_channel_cdx": row.try_get::<Option<String>, _>("discord_channel_cdx").ok().flatten(),
                "status": row.try_get::<Option<String>, _>("status").ok().flatten(),
                "stats_xp": row.try_get::<i64, _>("xp").unwrap_or(0),
                "stats_tasks_done": row.try_get::<Option<i64>, _>("tasks_done").ok().flatten().unwrap_or(0),
                "stats_tokens": row.try_get::<Option<i64>, _>("total_tokens").ok().flatten().unwrap_or(0),
                "sprite_number": row.try_get::<Option<i64>, _>("sprite_number").ok().flatten(),
                "department_name": row.try_get::<Option<String>, _>("department_name").ok().flatten(),
                "department_name_ko": row.try_get::<Option<String>, _>("department_name_ko").ok().flatten(),
                "department_color": row.try_get::<Option<String>, _>("department_color").ok().flatten(),
                "created_at": row
                    .try_get::<Option<String>, _>("created_at")
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(normalize_datetime_to_iso),
                "current_task_id": row.try_get::<Option<String>, _>("current_task_id").ok().flatten(),
                "current_task": build_current_task(
                    row.try_get::<Option<String>, _>("current_task_id").ok().flatten(),
                    row.try_get::<Option<String>, _>("current_card_id").ok().flatten(),
                    row.try_get::<Option<String>, _>("current_card_title").ok().flatten(),
                ),
                "skills_7d": skills_7d.get(&agent_id).cloned().unwrap_or_default(),
            })
        })
        .collect())
}

async fn load_skills_7d_pg(
    pool: &sqlx::PgPool,
    agent_ids: &[String],
) -> HashMap<String, Vec<Value>> {
    if agent_ids.is_empty() {
        return HashMap::new();
    }
    let rows = match sqlx::query(
        "SELECT su.agent_id,
                su.skill_id,
                COALESCE(s.name, su.skill_id) AS skill_name,
                COUNT(*)::BIGINT AS uses
         FROM skill_usage su
         LEFT JOIN skills s ON s.id = su.skill_id
         WHERE su.agent_id = ANY($1::TEXT[])
           AND su.used_at >= NOW() - INTERVAL '7 days'
         GROUP BY su.agent_id, su.skill_id, skill_name
         ORDER BY su.agent_id ASC, uses DESC, su.skill_id ASC",
    )
    .bind(agent_ids.to_vec())
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(_) => return HashMap::new(),
    };
    group_skill_rows(rows.into_iter().map(|row| {
        (
            row.try_get::<String, _>("agent_id").unwrap_or_default(),
            row.try_get::<String, _>("skill_id").unwrap_or_default(),
            row.try_get::<String, _>("skill_name").unwrap_or_default(),
            row.try_get::<i64, _>("uses").unwrap_or(0),
        )
    }))
}

fn group_skill_rows<I>(rows: I) -> HashMap<String, Vec<Value>>
where
    I: IntoIterator<Item = (String, String, String, i64)>,
{
    let mut grouped = HashMap::<String, Vec<Value>>::new();
    for (agent_id, skill_id, skill_name, uses) in rows {
        let bucket = grouped.entry(agent_id).or_default();
        if bucket.len() >= 3 {
            continue;
        }
        bucket.push(json!({
            "id": skill_id,
            "name": skill_name,
            "count": uses,
        }));
    }
    grouped
}

fn build_current_task(
    dispatch_id: Option<String>,
    card_id: Option<String>,
    card_title: Option<String>,
) -> Value {
    match dispatch_id {
        Some(dispatch_id) => json!({
            "dispatch_id": dispatch_id,
            "card_id": card_id,
            "card_title": card_title,
        }),
        None => Value::Null,
    }
}

async fn load_activity_items_pg(pool: &sqlx::PgPool) -> Result<Vec<ActivityItem>, String> {
    let mut items = Vec::new();
    let kanban_rows = sqlx::query(
        "SELECT kal.id::text AS id,
                kal.card_id,
                kal.from_status,
                kal.to_status,
                kal.source,
                kal.result,
                kal.created_at::text AS created_at,
                kc.title,
                kc.github_issue_number::BIGINT AS issue_number
         FROM kanban_audit_logs kal
         LEFT JOIN kanban_cards kc ON kc.id = kal.card_id
         ORDER BY kal.created_at DESC NULLS LAST, kal.id DESC
         LIMIT 64",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("activity kanban rows: {error}"))?;
    for row in kanban_rows {
        let created_at = row.try_get::<String, _>("created_at").unwrap_or_default();
        let iso = normalize_datetime_to_iso(&created_at).unwrap_or_else(utc_now_iso);
        let ts = iso_to_millis(&iso);
        let id = format!(
            "kanban:{}",
            row.try_get::<String, _>("id").unwrap_or_default()
        );
        let card_title = row.try_get::<Option<String>, _>("title").ok().flatten();
        let issue_number = row.try_get::<Option<i64>, _>("issue_number").ok().flatten();
        items.push(ActivityItem {
            timestamp_ms: ts,
            cursor_id: id.clone(),
            body: json!({
                "id": id,
                "kind": "kanban_transition",
                "created_at": iso,
                "summary": format!(
                    "{} {} -> {}",
                    issue_number.map(|value| format!("#{value}")).unwrap_or_else(|| "card".to_string()),
                    row.try_get::<Option<String>, _>("from_status").ok().flatten().unwrap_or_else(|| "unknown".to_string()),
                    row.try_get::<Option<String>, _>("to_status").ok().flatten().unwrap_or_else(|| "unknown".to_string()),
                ),
                "meta": {
                    "card_id": row.try_get::<Option<String>, _>("card_id").ok().flatten(),
                    "card_title": card_title,
                    "from_status": row.try_get::<Option<String>, _>("from_status").ok().flatten(),
                    "to_status": row.try_get::<Option<String>, _>("to_status").ok().flatten(),
                    "source": row.try_get::<Option<String>, _>("source").ok().flatten(),
                    "result": row.try_get::<Option<String>, _>("result").ok().flatten(),
                }
            }),
        });
    }

    let dispatch_rows = sqlx::query(
        "SELECT id,
                kanban_card_id,
                to_agent_id,
                dispatch_type,
                status,
                title,
                created_at::text AS created_at
         FROM task_dispatches
         ORDER BY created_at DESC NULLS LAST, id DESC
         LIMIT 64",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("activity dispatch rows: {error}"))?;
    for row in dispatch_rows {
        let created_at = row.try_get::<String, _>("created_at").unwrap_or_default();
        let iso = normalize_datetime_to_iso(&created_at).unwrap_or_else(utc_now_iso);
        let ts = iso_to_millis(&iso);
        let id = format!(
            "dispatch:{}",
            row.try_get::<String, _>("id").unwrap_or_default()
        );
        items.push(ActivityItem {
            timestamp_ms: ts,
            cursor_id: id.clone(),
            body: json!({
                "id": id,
                "kind": "dispatch",
                "created_at": iso,
                "summary": row.try_get::<Option<String>, _>("title")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "dispatch created".to_string()),
                "meta": {
                    "dispatch_id": row.try_get::<Option<String>, _>("id").ok().flatten(),
                    "card_id": row.try_get::<Option<String>, _>("kanban_card_id").ok().flatten(),
                    "agent_id": row.try_get::<Option<String>, _>("to_agent_id").ok().flatten(),
                    "dispatch_type": row.try_get::<Option<String>, _>("dispatch_type").ok().flatten(),
                    "status": row.try_get::<Option<String>, _>("status").ok().flatten(),
                }
            }),
        });
    }

    let provider_rows = sqlx::query(
        "SELECT id::text AS id,
                entity_type,
                entity_id,
                action,
                actor,
                timestamp::text AS created_at
         FROM audit_logs
         WHERE entity_type ILIKE 'provider%'
            OR action ILIKE 'provider%'
         ORDER BY timestamp DESC NULLS LAST, id DESC
         LIMIT 32",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("activity provider rows: {error}"))?;
    for row in provider_rows {
        let created_at = row.try_get::<String, _>("created_at").unwrap_or_default();
        let iso = normalize_datetime_to_iso(&created_at).unwrap_or_else(utc_now_iso);
        let ts = iso_to_millis(&iso);
        let id = format!(
            "provider:{}",
            row.try_get::<String, _>("id").unwrap_or_default()
        );
        items.push(ActivityItem {
            timestamp_ms: ts,
            cursor_id: id.clone(),
            body: json!({
                "id": id,
                "kind": "provider_event",
                "created_at": iso,
                "summary": row.try_get::<Option<String>, _>("action")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "provider event".to_string()),
                "meta": {
                    "entity_type": row.try_get::<Option<String>, _>("entity_type").ok().flatten(),
                    "entity_id": row.try_get::<Option<String>, _>("entity_id").ok().flatten(),
                    "actor": row.try_get::<Option<String>, _>("actor").ok().flatten(),
                }
            }),
        });
    }

    let achievement_bundle = build_achievements_pg(pool, None).await?;
    items.extend(achievement_bundle.events);
    Ok(items)
}

async fn build_achievements_pg(
    pool: &sqlx::PgPool,
    agent_filter: Option<&str>,
) -> Result<AchievementBundle, String> {
    let agents = match agent_filter {
        Some(agent_id) => {
            sqlx::query(
                "SELECT id,
                        COALESCE(name, id) AS name,
                        COALESCE(name_ko, name, id) AS name_ko,
                        COALESCE(xp, 0)::BIGINT AS xp,
                        COALESCE(avatar_emoji, '🤖') AS avatar_emoji
                 FROM agents
                 WHERE id = $1
                   AND COALESCE(xp, 0) > 0",
            )
            .bind(agent_id)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query(
                "SELECT id,
                        COALESCE(name, id) AS name,
                        COALESCE(name_ko, name, id) AS name_ko,
                        COALESCE(xp, 0)::BIGINT AS xp,
                        COALESCE(avatar_emoji, '🤖') AS avatar_emoji
                 FROM agents
                 WHERE COALESCE(xp, 0) > 0",
            )
            .fetch_all(pool)
            .await
        }
    }
    .map_err(|error| format!("load achievement agents: {error}"))?;

    let mut achievements = Vec::new();
    let mut events = Vec::new();

    for row in agents {
        let agent_id = row.try_get::<String, _>("id").unwrap_or_default();
        let xp = row.try_get::<i64, _>("xp").unwrap_or(0);
        let completion_times = sqlx::query_scalar::<_, Option<String>>(
            "SELECT updated_at::text
             FROM task_dispatches
             WHERE to_agent_id = $1
               AND status = 'completed'
             ORDER BY updated_at ASC",
        )
        .bind(&agent_id)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("load achievement completions: {error}"))?
        .into_iter()
        .flatten()
        .filter_map(|value| normalize_datetime_to_iso(&value))
        .map(|value| iso_to_millis(&value))
        .collect::<Vec<_>>();

        for (index, (threshold, achievement_type, description, rarity)) in
            ACHIEVEMENT_MILESTONES.iter().enumerate()
        {
            if xp < *threshold {
                continue;
            }
            let next_threshold = ACHIEVEMENT_MILESTONES
                .get(index + 1)
                .map(|(next, _, _, _)| *next);
            let earned_at = completion_times
                .get((threshold / 10).saturating_sub(1) as usize)
                .copied()
                .unwrap_or(0);
            let event_iso = millis_to_iso(earned_at).unwrap_or_else(utc_now_iso);
            let achievement = json!({
                "id": format!("{agent_id}:{achievement_type}"),
                "agent_id": agent_id,
                "type": achievement_type,
                "name": format!("{description} ({threshold} XP)"),
                "description": description,
                "earned_at": event_iso,
                "agent_name": row.try_get::<String, _>("name").unwrap_or_default(),
                "agent_name_ko": row.try_get::<String, _>("name_ko").unwrap_or_default(),
                "avatar_emoji": row.try_get::<String, _>("avatar_emoji").unwrap_or_else(|_| "🤖".to_string()),
                "rarity": rarity,
                "progress": {
                    "current_xp": xp,
                    "threshold": threshold,
                    "next_threshold": next_threshold,
                    "percent": if let Some(next) = next_threshold {
                        ((xp as f64 / next as f64) * 100.0).round() as i64
                    } else {
                        100
                    },
                }
            });
            achievements.push(achievement.clone());
            events.push(ActivityItem {
                timestamp_ms: earned_at,
                cursor_id: format!("achievement:{agent_id}:{achievement_type}"),
                body: json!({
                    "id": format!("achievement:{agent_id}:{achievement_type}"),
                    "kind": "achievement",
                    "created_at": event_iso,
                    "summary": format!("{} achieved {}", row.try_get::<String, _>("name").unwrap_or_default(), achievement["name"].as_str().unwrap_or("milestone")),
                    "meta": achievement,
                }),
            });
        }
    }

    let daily_missions = build_daily_missions_pg(pool).await?;
    Ok(AchievementBundle {
        achievements,
        events,
        daily_missions,
    })
}

async fn build_daily_missions_pg(pool: &sqlx::PgPool) -> Result<Vec<Value>, String> {
    let completed_today = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM task_dispatches
         WHERE status = 'completed'
           AND updated_at >= date_trunc('day', NOW())",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("daily mission completed_today: {error}"))?;
    let active_agents_today = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT to_agent_id)::BIGINT
         FROM task_dispatches
         WHERE status = 'completed'
           AND updated_at >= date_trunc('day', NOW())
           AND to_agent_id IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("daily mission active_agents_today: {error}"))?;
    let review_queue = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM kanban_cards WHERE status = 'review'",
    )
    .fetch_one(pool)
    .await
    .map_err(|error| format!("daily mission review_queue: {error}"))?;
    Ok(build_daily_missions_payload(
        completed_today,
        active_agents_today,
        review_queue,
    ))
}

fn build_daily_missions_payload(
    completed_today: i64,
    active_agents_today: i64,
    review_queue: i64,
) -> Vec<Value> {
    vec![
        json!({
            "id": "dispatches_today",
            "label": "Complete 5 dispatches today",
            "current": completed_today,
            "target": 5,
            "completed": completed_today >= 5,
        }),
        json!({
            "id": "active_agents_today",
            "label": "Get 3 agents shipping today",
            "current": active_agents_today,
            "target": 3,
            "completed": active_agents_today >= 3,
        }),
        json!({
            "id": "review_queue_zero",
            "label": "Drain the review queue",
            "current": if review_queue == 0 { 1 } else { 0 },
            "target": 1,
            "completed": review_queue == 0,
        }),
    ]
}

async fn load_completed_dispatch_counts_pg(
    pool: &sqlx::PgPool,
    days: i64,
) -> Result<HashMap<String, i64>, String> {
    let rows = sqlx::query(
        "SELECT TO_CHAR(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS day,
                COUNT(*)::BIGINT AS count
         FROM task_dispatches
         WHERE status = 'completed'
           AND updated_at >= NOW() - ($1::BIGINT || ' days')::INTERVAL
         GROUP BY day",
    )
    .bind(days)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load completed dispatch counts: {error}"))?;
    let mut map = HashMap::new();
    for row in rows {
        map.insert(
            row.try_get::<String, _>("day").unwrap_or_default(),
            row.try_get::<i64, _>("count").unwrap_or(0),
        );
    }
    Ok(map)
}

fn build_bottlenecks(payload: &Value) -> Vec<Value> {
    let mut rows = Vec::new();
    let deferred_hooks = payload["deferred_hooks"].as_i64().unwrap_or(0);
    let outbox_age = payload["outbox_age"]
        .as_i64()
        .or_else(|| payload["dispatch_outbox"]["oldest_pending_age"].as_i64())
        .unwrap_or(0);
    let queue_depth = payload["queue_depth"].as_i64().unwrap_or(0);
    let watcher_count = payload["watcher_count"].as_i64().unwrap_or(0);
    let recovery_duration = payload["recovery_duration"].as_f64().unwrap_or(0.0) as i64;
    let providers = payload["providers"].as_array().cloned().unwrap_or_default();
    let disconnected_providers = providers
        .iter()
        .filter(|provider| provider["connected"].as_bool() == Some(false))
        .count() as i64;
    let provider_queue = providers
        .iter()
        .map(|provider| provider["queue_depth"].as_i64().unwrap_or(0))
        .sum::<i64>();

    push_bottleneck(
        &mut rows,
        "deferred_hooks",
        deferred_hooks,
        1,
        3,
        "deferred hook backlog",
    );
    push_bottleneck(
        &mut rows,
        "outbox_age",
        outbox_age,
        30,
        60,
        "oldest pending outbox age",
    );
    push_bottleneck(
        &mut rows,
        "pending_queue",
        queue_depth,
        1,
        3,
        "global pending queue depth",
    );
    push_bottleneck(
        &mut rows,
        "active_watchers",
        watcher_count,
        4,
        8,
        "watcher load",
    );
    push_bottleneck(
        &mut rows,
        "recovery_seconds",
        recovery_duration,
        180,
        600,
        "recovery duration",
    );
    if disconnected_providers > 0 {
        rows.push(json!({
            "kind": "provider_disconnects",
            "count": disconnected_providers,
            "severity": if disconnected_providers >= 2 { "danger" } else { "warning" },
            "detail": "providers disconnected",
        }));
    }
    if provider_queue > 0 {
        rows.push(json!({
            "kind": "provider_queue",
            "count": provider_queue,
            "severity": if provider_queue >= 3 { "danger" } else { "warning" },
            "detail": "aggregate provider queue depth",
        }));
    }
    if rows.is_empty() {
        for reason in payload["degraded_reasons"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if let Some(reason) = reason.as_str() {
                rows.push(json!({
                    "kind": reason,
                    "count": 1,
                    "severity": if payload["status"].as_str() == Some("unhealthy") { "danger" } else { "warning" },
                    "detail": reason,
                }));
            }
        }
    }
    rows.sort_by(|left, right| {
        severity_rank(right["severity"].as_str())
            .cmp(&severity_rank(left["severity"].as_str()))
            .then_with(|| {
                right["count"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&left["count"].as_i64().unwrap_or(0))
            })
    });
    rows
}

fn push_bottleneck(
    rows: &mut Vec<Value>,
    kind: &str,
    count: i64,
    warning: i64,
    danger: i64,
    detail: &str,
) {
    if count <= 0 {
        return;
    }
    let severity = if count >= danger {
        "danger"
    } else if count >= warning {
        "warning"
    } else {
        return;
    };
    rows.push(json!({
        "kind": kind,
        "count": count,
        "severity": severity,
        "detail": detail,
    }));
}

fn severity_rank(severity: Option<&str>) -> i64 {
    match severity {
        Some("danger") => 2,
        Some("warning") => 1,
        _ => 0,
    }
}

fn enrich_setting_entry(mut entry: Value) -> Value {
    let live_override = json!({
        "active": entry["override_active"],
        "value": entry["value"],
        "baseline": entry["baseline"],
        "restart_behavior": entry["restart_behavior"],
    });
    entry["live_override"] = live_override;
    entry
}

fn decimal_string(value: f64) -> String {
    let mut rendered = format!("{value:.6}");
    while rendered.contains('.') && rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.push('0');
    }
    rendered
}

fn utc_now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn normalize_datetime_to_iso(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true),
        );
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Some(
            DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true),
        );
    }
    if let Ok(ts) = value.parse::<i64>() {
        return millis_to_iso(ts);
    }
    None
}

fn millis_to_iso(value: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(value)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn iso_to_millis(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn parse_cursor(value: &str) -> Option<CursorMarker> {
    let (left, right) = value.split_once('|')?;
    let timestamp_ms = left.parse::<i64>().ok()?;
    Some(CursorMarker {
        timestamp_ms,
        cursor_id: right.to_string(),
    })
}
