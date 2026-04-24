use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use std::{
    collections::{BTreeMap, HashSet},
    process::Command,
};

use super::{
    AppState, skill_usage_analytics::collect_skill_usage, skills_api::sync_skills_from_disk,
};

const UNSUPPORTED_RATE_LIMIT_PROVIDERS: &[(&str, &str)] = &[(
    "qwen",
    "No Qwen rate-limit telemetry source is implemented yet.",
)];
const UNSUPPORTED_RATE_LIMIT_USAGE_LOOKBACK_SECONDS: i64 = 30 * 24 * 60 * 60;

fn sqlite_datetime_to_millis(value: &str) -> Option<i64> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(value) {
        return Some(ts.timestamp_millis());
    }
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|ts| DateTime::<Utc>::from_naive_utc_and_offset(ts, Utc).timestamp_millis())
}

#[derive(Debug, Default, Deserialize)]
pub struct AnalyticsQuery {
    pub provider: Option<String>,
    #[serde(rename = "channelId")]
    pub channel_id: Option<String>,
    #[serde(rename = "eventType")]
    pub event_type: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct QualityEventsQuery {
    pub agent_id: Option<String>,
    pub days: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct InvariantsQuery {
    pub provider: Option<String>,
    #[serde(rename = "channelId")]
    pub channel_id: Option<String>,
    pub invariant: Option<String>,
    pub limit: Option<usize>,
}

/// GET /api/analytics
pub async fn analytics(
    State(state): State<AppState>,
    Query(params): Query<AnalyticsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let filters = crate::services::observability::AnalyticsFilters {
        provider: params.provider,
        channel_id: params.channel_id,
        event_type: params.event_type,
        event_limit: params.limit.unwrap_or(100),
        counter_limit: 200,
    };

    match crate::services::observability::query_analytics(
        state.sqlite_db(),
        state.pg_pool_ref(),
        &filters,
    )
    .await
    {
        Ok(response) => match serde_json::to_value(response) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("serialize analytics response: {error}")})),
            ),
        },
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query analytics: {error}")})),
        ),
    }
}

/// GET /api/quality/events
pub async fn quality_events(
    State(state): State<AppState>,
    Query(params): Query<QualityEventsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let filters = crate::services::observability::AgentQualityFilters {
        agent_id: params.agent_id,
        days: params.days.unwrap_or(7),
        limit: params.limit.unwrap_or(200),
    };

    match crate::services::observability::query_agent_quality_events(
        state.sqlite_db(),
        state.pg_pool_ref(),
        &filters,
    )
    .await
    {
        Ok(events) => (
            StatusCode::OK,
            Json(json!({
                "events": events,
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query agent quality events: {error}")})),
        ),
    }
}

/// GET /api/analytics/invariants
pub async fn invariants(
    State(state): State<AppState>,
    Query(params): Query<InvariantsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let filters = crate::services::observability::InvariantAnalyticsFilters {
        provider: params.provider,
        channel_id: params.channel_id,
        invariant: params.invariant,
        limit: params.limit.unwrap_or(50),
    };

    match crate::services::observability::query_invariant_analytics(
        state.sqlite_db(),
        state.pg_pool_ref(),
        &filters,
    )
    .await
    {
        Ok(response) => match serde_json::to_value(response) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("serialize invariant analytics response: {error}")})),
            ),
        },
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query invariant analytics: {error}")})),
        ),
    }
}

/// GET /api/streaks
pub async fn streaks(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.sqlite_db().lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // 에이전트별 연속 작업일 계산
    // 간단 버전: 에이전트별 완료 dispatch 날짜를 역순으로 가져와 연속일 계산
    let mut stmt = match conn.prepare(
        "SELECT a.id, a.name, a.avatar_emoji,
                GROUP_CONCAT(DISTINCT date(td.updated_at)) AS active_dates,
                MAX(td.updated_at) AS last_active
         FROM agents a
         INNER JOIN task_dispatches td ON td.to_agent_id = a.id
         WHERE td.status = 'completed'
         GROUP BY a.id
         ORDER BY last_active DESC",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("query prepare failed: {e}")})),
            );
        }
    };

    let rows = stmt
        .query_map([], |row| {
            let agent_id: String = row.get(0)?;
            let name: Option<String> = row.get(1)?;
            let avatar_emoji: Option<String> = row.get(2)?;
            let active_dates_str: Option<String> = row.get(3)?;
            let last_active: Option<String> = row.get(4)?;

            // 연속일 계산: 날짜 문자열을 파싱하여 오늘부터 역순으로 연속인 일수
            let streak = if let Some(ref dates_str) = active_dates_str {
                let mut dates: Vec<&str> = dates_str.split(',').collect();
                dates.sort();
                dates.reverse();
                compute_streak(&dates)
            } else {
                0
            };

            Ok(json!({
                "agent_id": agent_id,
                "name": name,
                "avatar_emoji": avatar_emoji,
                "streak": streak,
                "last_active": last_active,
            }))
        })
        .ok();

    let streaks = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect::<Vec<_>>(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({ "streaks": streaks })))
}

/// 날짜 문자열 배열 (내림차순)에서 오늘부터 연속일 계산
fn compute_streak(sorted_dates_desc: &[&str]) -> i64 {
    if sorted_dates_desc.is_empty() {
        return 0;
    }

    // 간단 구현: 날짜를 일수 차이로 변환
    // SQLite date format: "YYYY-MM-DD"
    let today = chrono_today();
    let mut streak = 0i64;
    let mut expected_date = today;

    for date_str in sorted_dates_desc {
        if let Some(d) = parse_date(date_str) {
            if d == expected_date {
                streak += 1;
                expected_date = d - 1;
            } else if d < expected_date {
                // 건너뛴 날이 있으면 중단
                break;
            }
            // d > expected_date는 무시 (미래 날짜 등)
        }
    }

    streak
}

/// 간단한 날짜 파싱 (YYYY-MM-DD → 일수 단위 정수, 비교용)
fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    // 일수 환산 (비교 목적이므로 정확한 달력 계산 불필요, 대략적 환산)
    Some(y * 365 + m * 30 + d)
}

fn chrono_today() -> i64 {
    // 현재 UTC 날짜를 같은 방식으로 환산
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (now / 86400) as i64;
    // Unix epoch 1970-01-01부터의 일수를 YYYY-MM-DD 환산과 맞추기 위해
    // 같은 공식 사용: 1970 * 365 + 1 * 30 + 1 + days
    // 대신 간단히: 오늘 날짜를 문자열로 만들어 parse_date 호출
    let total_days = days;
    let approx_year = 1970 + total_days / 365;
    let remaining = total_days % 365;
    let approx_month = 1 + remaining / 30;
    let approx_day = 1 + remaining % 30;
    approx_year * 365 + approx_month * 30 + approx_day
}

/// GET /api/achievements
#[derive(Debug, Deserialize)]
pub struct AchievementsQuery {
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

pub async fn achievements(
    State(state): State<AppState>,
    Query(params): Query<AchievementsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.sqlite_db().lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // XP milestone thresholds
    let milestones: &[(i64, &str, &str)] = &[
        (10, "first_task", "첫 번째 작업 완료"),
        (50, "getting_started", "본격적인 시작"),
        (100, "centurion", "100 XP 달성"),
        (250, "veteran", "베테랑"),
        (500, "expert", "전문가"),
        (1000, "master", "마스터"),
    ];

    // Build agent filter
    let mut sql = String::from(
        "SELECT id, COALESCE(name, id), COALESCE(name_ko, name, id), xp, avatar_emoji FROM agents WHERE xp > 0",
    );
    let mut bind_params: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();
    if let Some(ref agent_id) = params.agent_id {
        sql.push_str(&format!(" AND id = ?{}", bind_params.len() + 1));
        bind_params.push(Box::new(agent_id.clone()));
    }

    let param_refs: Vec<&dyn libsql_rusqlite::types::ToSql> =
        bind_params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let agents: Vec<(String, String, String, i64, String)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?
                    .unwrap_or_else(|| "🤖".to_string()),
            ))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    // Pre-fetch completion timestamps per agent (nth completed dispatch as earned_at proxy)
    let mut agent_completed_times: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    for (agent_id, _, _, _, _) in &agents {
        let times: Vec<i64> = conn
            .prepare(
                "SELECT CAST(strftime('%s', updated_at) AS INTEGER) * 1000 \
                 FROM task_dispatches WHERE to_agent_id = ?1 AND status = 'completed' \
                 ORDER BY updated_at ASC",
            )
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map([agent_id], |row| row.get::<_, i64>(0))
                    .ok()
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();
        agent_completed_times.insert(agent_id.clone(), times);
    }

    let mut achievements = Vec::new();
    for (agent_id, name, name_ko, xp, avatar_emoji) in &agents {
        let completion_times = agent_completed_times.get(agent_id.as_str());
        for (threshold, achievement_type, description) in milestones {
            if xp >= threshold {
                // Estimate earned_at: use the Nth completed dispatch timestamp
                // where N approximates when this XP threshold was crossed
                // (assuming ~10 XP per completion on average)
                let approx_index = (*threshold as usize / 10).saturating_sub(1);
                let earned_at = completion_times
                    .and_then(|times| times.get(approx_index.min(times.len().saturating_sub(1))))
                    .copied()
                    .unwrap_or(0);

                let emoji = avatar_emoji.as_str();
                achievements.push(json!({
                    "id": format!("{agent_id}:{achievement_type}"),
                    "agent_id": agent_id,
                    "type": achievement_type,
                    "name": format!("{description} ({threshold} XP)"),
                    "description": description,
                    "earned_at": earned_at,
                    "agent_name": name,
                    "agent_name_ko": name_ko,
                    "avatar_emoji": emoji,
                }));
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "achievements": achievements })),
    )
}

/// GET /api/activity-heatmap?date=2026-03-19
#[derive(Debug, Deserialize)]
pub struct HeatmapQuery {
    date: Option<String>,
}

pub async fn activity_heatmap(
    State(state): State<AppState>,
    Query(params): Query<HeatmapQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.sqlite_db().lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let date = params
        .date
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());

    // 시간대별 에이전트 활동 집계 (task_dispatches 기반)
    let mut hours: Vec<serde_json::Value> = Vec::with_capacity(24);
    for hour in 0..24 {
        let sql = format!(
            "SELECT td.to_agent_id, COUNT(*) AS cnt
             FROM task_dispatches td
             WHERE date(td.created_at) = ?1
               AND CAST(strftime('%H', td.created_at) AS INTEGER) = ?2
               AND td.to_agent_id IS NOT NULL
             GROUP BY td.to_agent_id"
        );

        let agents_map = {
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => {
                    hours.push(json!({ "hour": hour, "agents": {} }));
                    continue;
                }
            };

            let mut map = serde_json::Map::new();
            if let Ok(rows) = stmt.query_map(libsql_rusqlite::params![&date, hour as i64], |row| {
                let agent_id: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((agent_id, count))
            }) {
                for row in rows.flatten() {
                    map.insert(row.0, json!(row.1));
                }
            }
            serde_json::Value::Object(map)
        };

        hours.push(json!({
            "hour": hour,
            "agents": agents_map,
        }));
    }

    (
        StatusCode::OK,
        Json(json!({
            "hours": hours,
            "date": date,
        })),
    )
}

/// GET /api/audit-logs?limit=20&entityType=...&entityId=...
#[derive(Debug, Deserialize)]
pub struct AuditLogsQuery {
    limit: Option<i64>,
    #[serde(rename = "entityType")]
    entity_type: Option<String>,
    #[serde(rename = "entityId")]
    entity_id: Option<String>,
}

pub async fn audit_logs(
    State(state): State<AppState>,
    Query(params): Query<AuditLogsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.sqlite_db().lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let limit = params.limit.unwrap_or(20);
    let audit_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM audit_logs", [], |row| row.get(0))
        .unwrap_or(0);

    let logs = if audit_count > 0 {
        let mut conditions = Vec::new();
        let mut bind_values: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(ref et) = params.entity_type {
            conditions.push(format!("entity_type = ?{idx}"));
            bind_values.push(Box::new(et.clone()));
            idx += 1;
        }
        if let Some(ref eid) = params.entity_id {
            conditions.push(format!("entity_id = ?{idx}"));
            bind_values.push(Box::new(eid.clone()));
            idx += 1;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let sql = format!(
            "SELECT id, entity_type, entity_id, action, timestamp, actor
             FROM audit_logs
             {where_clause}
             ORDER BY timestamp DESC
             LIMIT ?{idx}"
        );
        bind_values.push(Box::new(limit));

        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("query prepare failed: {e}")})),
                );
            }
        };

        let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> =
            bind_values.iter().map(|v| v.as_ref()).collect();

        stmt.query_map(params_ref.as_slice(), |row| {
            let entity_type = row
                .get::<_, Option<String>>(1)?
                .unwrap_or_else(|| "system".to_string());
            let entity_id = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let action = row
                .get::<_, Option<String>>(3)?
                .unwrap_or_else(|| "updated".to_string());
            let created_raw = row.get::<_, Option<String>>(4)?.unwrap_or_default();
            let actor = row.get::<_, Option<String>>(5)?.unwrap_or_default();
            let created_at = sqlite_datetime_to_millis(&created_raw).unwrap_or(0);
            let summary = if entity_id.is_empty() {
                format!("{entity_type} {action}")
            } else {
                format!("{entity_type}:{entity_id} {action}")
            };
            Ok(json!({
                "id": row.get::<_, i64>(0)?.to_string(),
                "actor": actor,
                "action": action,
                "entity_type": entity_type,
                "entity_id": entity_id,
                "summary": summary,
                "created_at": created_at,
            }))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
    } else {
        if let Some(ref entity_type) = params.entity_type {
            if entity_type != "kanban_card" {
                return (StatusCode::OK, Json(json!({ "logs": [] })));
            }
        }

        let mut conditions = Vec::new();
        let mut bind_values: Vec<Box<dyn libsql_rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(ref card_id) = params.entity_id {
            conditions.push(format!("card_id = ?{idx}"));
            bind_values.push(Box::new(card_id.clone()));
            idx += 1;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let sql = format!(
            "SELECT id, card_id, from_status, to_status, source, created_at
             FROM kanban_audit_logs
             {where_clause}
             ORDER BY created_at DESC
             LIMIT ?{idx}"
        );
        bind_values.push(Box::new(limit));

        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return (StatusCode::OK, Json(json!({ "logs": [] }))),
        };

        let params_ref: Vec<&dyn libsql_rusqlite::types::ToSql> =
            bind_values.iter().map(|v| v.as_ref()).collect();

        stmt.query_map(params_ref.as_slice(), |row| {
            let card_id = row.get::<_, String>(1)?;
            let from_status = row
                .get::<_, Option<String>>(2)?
                .unwrap_or_else(|| "unknown".to_string());
            let to_status = row
                .get::<_, Option<String>>(3)?
                .unwrap_or_else(|| "unknown".to_string());
            let actor = row
                .get::<_, Option<String>>(4)?
                .unwrap_or_else(|| "hook".to_string());
            let created_raw = row.get::<_, Option<String>>(5)?.unwrap_or_default();
            let created_at = sqlite_datetime_to_millis(&created_raw).unwrap_or(0);
            Ok(json!({
                "id": format!("kanban-{}", row.get::<_, i64>(0)?),
                "actor": actor.clone(),
                "action": format!("{from_status}->{to_status}"),
                "entity_type": "kanban_card",
                "entity_id": card_id,
                "summary": format!("{from_status} -> {to_status}"),
                "metadata": {
                    "from_status": from_status,
                    "to_status": to_status,
                    "source": actor,
                },
                "created_at": created_at,
            }))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
    };

    (StatusCode::OK, Json(json!({ "logs": logs })))
}

fn parse_machine_config(value: &str) -> Option<Vec<(String, String)>> {
    serde_json::from_str::<Vec<serde_json::Value>>(value)
        .ok()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m.get("name")?.as_str()?.to_string();
                    let host = m.get("host").and_then(|h| h.as_str()).unwrap_or_else(|| {
                        m.get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("localhost")
                    });
                    Some((name, format!("{}.local", host)))
                })
                .collect()
        })
        .filter(|machines: &Vec<(String, String)>| !machines.is_empty())
}

fn default_machine_config() -> Vec<(String, String)> {
    let hostname = crate::services::platform::hostname_short();
    vec![(hostname.clone(), hostname)]
}

async fn load_machine_config_pg(pool: &sqlx::PgPool) -> Option<Vec<(String, String)>> {
    sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
        .bind("machines")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|value| parse_machine_config(&value))
}

fn load_machine_config_sqlite(db: &crate::db::Db) -> Option<Vec<(String, String)>> {
    db.lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT value FROM kv_meta WHERE key = 'machines'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .and_then(|value| parse_machine_config(&value))
}

async fn load_machine_config(
    db: &crate::db::Db,
    pg_pool: Option<&sqlx::PgPool>,
) -> Vec<(String, String)> {
    if let Some(pool) = pg_pool {
        return load_machine_config_pg(pool)
            .await
            .unwrap_or_else(default_machine_config);
    }

    load_machine_config_sqlite(db).unwrap_or_else(default_machine_config)
}

/// GET /api/machine-status
/// Machine list from kv_meta key 'machines' (JSON array of {name, host}).
/// Falls back to current hostname if not configured.
pub async fn machine_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let machines_config = load_machine_config(state.sqlite_db(), state.pg_pool_ref()).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut results = Vec::new();
        for (name, host) in machines_config {
            let online = Command::new("ping")
                .args(["-c1", "-W2", &host])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            results.push(json!({"name": name, "online": online}));
        }
        results
    })
    .await;

    let machines = result.unwrap_or_default();
    (StatusCode::OK, Json(json!({"machines": machines})))
}

/// GET /api/rate-limits
/// Returns cached rate limit data from rate_limit_cache table.
pub async fn rate_limits(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let now = chrono::Utc::now().timestamp();
    let providers = if let Some(pool) = state.pg_pool_ref() {
        build_rate_limit_provider_payloads_pg(pool, now).await
    } else {
        let conn = match state.sqlite_db().lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        build_rate_limit_provider_payloads(&conn, now)
    };

    (StatusCode::OK, Json(json!({"providers": providers})))
}

fn build_rate_limit_provider_payloads(
    conn: &libsql_rusqlite::Connection,
    now: i64,
) -> Vec<serde_json::Value> {
    let stale_sec: i64 = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'rateLimitStaleSec'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);

    let mut stmt = match conn
        .prepare("SELECT provider, data, fetched_at FROM rate_limit_cache ORDER BY provider")
    {
        Ok(s) => s,
        Err(_) => return build_unsupported_rate_limit_entries(conn, now),
    };

    let mut seen = HashSet::new();
    let mut providers: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            let provider: String = row.get(0)?;
            let data: String = row.get(1)?;
            let fetched_at: i64 = row.get(2)?;
            Ok((provider, data, fetched_at))
        })
        .ok()
        .map(|rows| {
            rows.filter_map(|r| r.ok())
                .filter_map(|(provider, data, fetched_at)| {
                    let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
                    let buckets = parsed
                        .get("buckets")
                        .and_then(|value| value.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let unsupported = parsed
                        .get("unsupported")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let reason = parsed
                        .get("reason")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    let stale = (now - fetched_at) > stale_sec;
                    seen.insert(provider.to_lowercase());
                    Some(json!({
                        "provider": provider,
                        "buckets": buckets,
                        "fetched_at": fetched_at,
                        "stale": stale,
                        "unsupported": unsupported,
                        "reason": reason,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    for (provider, reason) in UNSUPPORTED_RATE_LIMIT_PROVIDERS {
        if seen.contains(*provider) {
            continue;
        }
        if !provider_has_recent_session_usage(conn, provider, now) {
            continue;
        }
        providers.push(json!({
            "provider": provider,
            "buckets": [],
            "fetched_at": now,
            "stale": false,
            "unsupported": true,
            "reason": reason,
        }));
    }

    providers.sort_by_key(|entry| {
        match entry
            .get("provider")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "claude" => 0,
            "codex" => 1,
            "gemini" => 2,
            "qwen" => 3,
            _ => 9,
        }
    });
    providers
}

async fn build_rate_limit_provider_payloads_pg(
    pool: &sqlx::PgPool,
    now: i64,
) -> Vec<serde_json::Value> {
    let stale_sec =
        sqlx::query("SELECT value FROM kv_meta WHERE key = 'rateLimitStaleSec' LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.try_get::<String, _>("value").ok())
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(600);

    let rows = match sqlx::query(
        "SELECT provider, data, fetched_at
         FROM rate_limit_cache
         ORDER BY provider",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(_) => return build_unsupported_rate_limit_entries_pg(pool, now).await,
    };

    let mut seen = HashSet::new();
    let mut providers = Vec::new();

    for row in rows {
        let provider = match row.try_get::<String, _>("provider") {
            Ok(provider) => provider,
            Err(_) => continue,
        };
        let data = match row.try_get::<String, _>("data") {
            Ok(data) => data,
            Err(_) => continue,
        };
        let fetched_at = match row.try_get::<i64, _>("fetched_at") {
            Ok(fetched_at) => fetched_at,
            Err(_) => continue,
        };

        let parsed: serde_json::Value = match serde_json::from_str(&data) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        let buckets = parsed
            .get("buckets")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let unsupported = parsed
            .get("unsupported")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let reason = parsed
            .get("reason")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let stale = (now - fetched_at) > stale_sec;
        seen.insert(provider.to_lowercase());
        providers.push(json!({
            "provider": provider,
            "buckets": buckets,
            "fetched_at": fetched_at,
            "stale": stale,
            "unsupported": unsupported,
            "reason": reason,
        }));
    }

    for (provider, reason) in UNSUPPORTED_RATE_LIMIT_PROVIDERS {
        if seen.contains(*provider) {
            continue;
        }
        if !provider_has_recent_session_usage_pg(pool, provider, now).await {
            continue;
        }
        providers.push(json!({
            "provider": provider,
            "buckets": [],
            "fetched_at": now,
            "stale": false,
            "unsupported": true,
            "reason": reason,
        }));
    }

    providers.sort_by_key(|entry| {
        match entry
            .get("provider")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "claude" => 0,
            "codex" => 1,
            "gemini" => 2,
            "qwen" => 3,
            _ => 9,
        }
    });
    providers
}

fn provider_has_recent_session_usage(
    conn: &libsql_rusqlite::Connection,
    provider: &str,
    now: i64,
) -> bool {
    let threshold = now.saturating_sub(UNSUPPORTED_RATE_LIMIT_USAGE_LOOKBACK_SECONDS);
    conn.query_row(
        "SELECT 1
         FROM sessions
         WHERE lower(provider) = lower(?1)
           AND COALESCE(
                 CAST(strftime('%s', last_heartbeat) AS INTEGER),
                 CAST(strftime('%s', created_at) AS INTEGER),
                 0
               ) >= ?2
         LIMIT 1",
        libsql_rusqlite::params![provider, threshold],
        |_row| Ok(()),
    )
    .is_ok()
}

async fn provider_has_recent_session_usage_pg(
    pool: &sqlx::PgPool,
    provider: &str,
    now: i64,
) -> bool {
    let threshold = now.saturating_sub(UNSUPPORTED_RATE_LIMIT_USAGE_LOOKBACK_SECONDS);
    sqlx::query(
        "SELECT 1
         FROM sessions
         WHERE lower(provider) = lower($1)
           AND COALESCE(
                 EXTRACT(EPOCH FROM last_heartbeat)::BIGINT,
                 EXTRACT(EPOCH FROM created_at)::BIGINT,
                 0
               ) >= $2
         LIMIT 1",
    )
    .bind(provider)
    .bind(threshold)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .is_some()
}

fn build_unsupported_rate_limit_entries(
    conn: &libsql_rusqlite::Connection,
    now: i64,
) -> Vec<serde_json::Value> {
    UNSUPPORTED_RATE_LIMIT_PROVIDERS
        .iter()
        .filter(|(provider, _reason)| provider_has_recent_session_usage(conn, provider, now))
        .map(|(provider, reason)| {
            json!({
                "provider": provider,
                "buckets": [],
                "fetched_at": now,
                "stale": false,
                "unsupported": true,
                "reason": reason,
            })
        })
        .collect()
}

async fn build_unsupported_rate_limit_entries_pg(
    pool: &sqlx::PgPool,
    now: i64,
) -> Vec<serde_json::Value> {
    let mut providers = Vec::new();
    for (provider, reason) in UNSUPPORTED_RATE_LIMIT_PROVIDERS {
        if provider_has_recent_session_usage_pg(pool, provider, now).await {
            providers.push(json!({
                "provider": provider,
                "buckets": [],
                "fetched_at": now,
                "stale": false,
                "unsupported": true,
                "reason": reason,
            }));
        }
    }
    providers
}

/// GET /api/skills-trend?days=30
#[derive(Debug, Deserialize)]
pub struct SkillsTrendQuery {
    days: Option<i64>,
}

pub async fn skills_trend(
    State(state): State<AppState>,
    Query(params): Query<SkillsTrendQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let days = params.days.unwrap_or(30).min(90).max(1);

    let conn = match state.sqlite_db().lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    sync_skills_from_disk(&conn);
    let usage = match collect_skill_usage(&conn, Some(days)) {
        Ok(data) => data,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("usage query failed: {e}")})),
            );
        }
    };

    let mut by_day = BTreeMap::<String, i64>::new();
    for record in usage {
        *by_day.entry(record.day).or_default() += 1;
    }

    let trend = by_day
        .into_iter()
        .map(|(day, count)| json!({ "day": day, "count": count }))
        .collect::<Vec<_>>();

    (StatusCode::OK, Json(json!({"trend": trend})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> libsql_rusqlite::Connection {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        conn
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_analytics_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "analytics tests",
            )
            .await
            .expect("create postgres test db");

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "analytics tests",
            )
            .await
            .expect("apply postgres migration")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "analytics tests",
            )
            .await
            .expect("drop postgres test db");
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    fn test_engine(db: &crate::db::Db) -> crate::engine::PolicyEngine {
        let config = crate::config::Config::default();
        crate::engine::PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    #[tokio::test]
    async fn machine_status_machines_config_prefers_postgres_when_pool_exists() {
        let sqlite_db = crate::db::test_db();
        {
            let conn = sqlite_db.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                libsql_rusqlite::params![
                    "machines",
                    serde_json::json!([{ "name": "sqlite-machine", "host": "sqlite-host" }])
                        .to_string()
                ],
            )
            .unwrap();
        }

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind("machines")
        .bind(serde_json::json!([{ "name": "pg-machine", "host": "pg-host" }]).to_string())
        .execute(&pool)
        .await
        .unwrap();

        let machines = load_machine_config(&sqlite_db, Some(&pool)).await;

        assert_eq!(
            machines,
            vec![("pg-machine".to_string(), "pg-host.local".to_string())]
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn machine_status_machines_config_uses_hostname_when_postgres_is_unconfigured() {
        let sqlite_db = crate::db::test_db();
        {
            let conn = sqlite_db.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                libsql_rusqlite::params![
                    "machines",
                    serde_json::json!([{ "name": "stale-sqlite-machine", "host": "stale-host" }])
                        .to_string()
                ],
            )
            .unwrap();
        }

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let hostname = crate::services::platform::hostname_short();

        let machines = load_machine_config(&sqlite_db, Some(&pool)).await;

        assert_eq!(machines, vec![(hostname.clone(), hostname)]);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn machine_status_machines_config_uses_hostname_for_empty_postgres_config() {
        let sqlite_db = crate::db::test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind("machines")
        .bind("[]")
        .execute(&pool)
        .await
        .unwrap();
        let hostname = crate::services::platform::hostname_short();

        let machines = load_machine_config(&sqlite_db, Some(&pool)).await;

        assert_eq!(machines, vec![(hostname.clone(), hostname)]);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn machine_status_machines_config_uses_sqlite_without_pg_pool() {
        let sqlite_db = crate::db::test_db();
        {
            let conn = sqlite_db.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                libsql_rusqlite::params![
                    "machines",
                    serde_json::json!([{ "name": "sqlite-machine", "host": "sqlite-host" }])
                        .to_string()
                ],
            )
            .unwrap();
        }

        let machines = load_machine_config(&sqlite_db, None).await;

        assert_eq!(
            machines,
            vec![(
                "sqlite-machine".to_string(),
                "sqlite-host.local".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn machine_status_machines_config_uses_hostname_for_invalid_sqlite_entries() {
        let sqlite_db = crate::db::test_db();
        {
            let conn = sqlite_db.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                libsql_rusqlite::params![
                    "machines",
                    serde_json::json!([{ "host": "missing-name" }]).to_string()
                ],
            )
            .unwrap();
        }
        let hostname = crate::services::platform::hostname_short();

        let machines = load_machine_config(&sqlite_db, None).await;

        assert_eq!(machines, vec![(hostname.clone(), hostname)]);
    }

    #[tokio::test]
    async fn machine_status_machines_config_falls_back_to_hostname_when_unconfigured() {
        let sqlite_db = crate::db::test_db();
        let hostname = crate::services::platform::hostname_short();

        let machines = load_machine_config(&sqlite_db, None).await;

        assert_eq!(machines, vec![(hostname.clone(), hostname)]);
    }

    #[test]
    fn build_rate_limit_provider_payloads_hides_unused_unsupported_qwen() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO rate_limit_cache (provider, data, fetched_at) VALUES (?1, ?2, ?3)",
            libsql_rusqlite::params![
                "claude",
                serde_json::json!({
                    "buckets": [{
                        "name": "requests",
                        "limit": 100,
                        "used": 20,
                        "remaining": 80,
                        "reset": 1_700_000_000_i64
                    }]
                })
                .to_string(),
                1_700_000_000_i64
            ],
        )
        .unwrap();

        let providers = build_rate_limit_provider_payloads(&conn, 1_700_000_100);

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["provider"], json!("claude"));
    }

    #[test]
    fn build_rate_limit_provider_payloads_shows_recent_unsupported_qwen_only_when_used() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO sessions (session_key, provider, status, created_at, last_heartbeat)
             VALUES (?1, ?2, 'completed', ?3, ?4)",
            libsql_rusqlite::params![
                "qwen-session-1",
                "qwen",
                "2023-11-14 22:00:00",
                "2023-11-14 22:10:00"
            ],
        )
        .unwrap();

        let providers = build_rate_limit_provider_payloads(&conn, 1_700_000_100);

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["provider"], json!("qwen"));
        assert_eq!(providers[0]["unsupported"], json!(true));
        assert_eq!(providers[0]["buckets"], json!([]));
    }

    #[tokio::test]
    async fn analytics_route_returns_observability_payload() {
        let _guard = crate::services::observability::test_runtime_lock();
        crate::services::observability::reset_for_tests();
        let db = crate::db::test_db();
        crate::services::observability::init_observability(db.clone(), None);
        crate::services::observability::emit_turn_started(
            "codex",
            4242,
            Some("dispatch-analytics"),
            Some("session-analytics"),
            Some("turn-analytics"),
        );
        crate::services::observability::emit_turn_finished(
            "codex",
            4242,
            Some("dispatch-analytics"),
            Some("session-analytics"),
            Some("turn-analytics"),
            "completed",
            120,
            false,
        );
        crate::services::observability::flush_for_tests().await;

        let state = AppState::test_state(db.clone(), test_engine(&db));
        let (status, Json(body)) = analytics(
            State(state),
            Query(AnalyticsQuery {
                provider: Some("codex".to_string()),
                channel_id: Some("4242".to_string()),
                event_type: None,
                limit: Some(10),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["counters"][0]["turn_attempts"], json!(1));
        assert_eq!(body["counters"][0]["turn_successes"], json!(1));
        assert!(
            body["events"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .any(|event| event["event_type"] == json!("turn_finished"))
        );
    }

    #[tokio::test]
    async fn invariants_route_returns_violation_payload() {
        let _guard = crate::services::observability::test_runtime_lock();
        crate::services::observability::reset_for_tests();
        let db = crate::db::test_db();
        crate::services::observability::init_observability(db.clone(), None);
        crate::services::observability::record_invariant_check(
            false,
            crate::services::observability::InvariantViolation {
                provider: Some("codex"),
                channel_id: Some(5150),
                dispatch_id: Some("dispatch-route"),
                session_key: None,
                turn_id: Some("discord:5150:1"),
                invariant: "watcher_one_per_channel",
                code_location: "src/services/discord/tmux.rs:test",
                message: "route test violation",
                details: json!({ "source": "test" }),
            },
        );
        crate::services::observability::flush_for_tests().await;

        let state = AppState::test_state(db.clone(), test_engine(&db));
        let (status, Json(body)) = invariants(
            State(state),
            Query(InvariantsQuery {
                provider: Some("codex".to_string()),
                channel_id: Some("5150".to_string()),
                invariant: Some("watcher_one_per_channel".to_string()),
                limit: Some(10),
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total_violations"], json!(1));
        assert_eq!(
            body["counts"][0]["invariant"],
            json!("watcher_one_per_channel")
        );
        assert_eq!(body["recent"][0]["message"], json!("route test violation"));
    }

    #[tokio::test]
    async fn build_rate_limit_provider_payloads_pg_hides_unused_unsupported_qwen() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO rate_limit_cache (provider, data, fetched_at)
             VALUES ($1, $2, $3)",
        )
        .bind("claude")
        .bind(
            serde_json::json!({
                "buckets": [{
                    "name": "requests",
                    "limit": 100,
                    "used": 20,
                    "remaining": 80,
                    "reset": 1_700_000_000_i64
                }]
            })
            .to_string(),
        )
        .bind(1_700_000_000_i64)
        .execute(&pool)
        .await
        .unwrap();

        let providers = build_rate_limit_provider_payloads_pg(&pool, 1_700_000_100).await;

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["provider"], json!("claude"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn build_rate_limit_provider_payloads_pg_shows_recent_unsupported_qwen_only_when_used() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, created_at, last_heartbeat)
             VALUES ($1, $2, 'completed', TO_TIMESTAMP($3), TO_TIMESTAMP($4))",
        )
        .bind("qwen-session-1")
        .bind("qwen")
        .bind(1_700_000_000_i64)
        .bind(1_700_000_050_i64)
        .execute(&pool)
        .await
        .unwrap();

        let providers = build_rate_limit_provider_payloads_pg(&pool, 1_700_000_100).await;

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["provider"], json!("qwen"));
        assert_eq!(providers[0]["unsupported"], json!(true));
        assert_eq!(providers[0]["buckets"], json!([]));

        pool.close().await;
        pg_db.drop().await;
    }
}
