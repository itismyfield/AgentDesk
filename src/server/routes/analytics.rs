use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use std::process::Command;

use super::AppState;

/// GET /api/streaks
pub async fn streaks(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
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
        "SELECT id, COALESCE(name, id), COALESCE(name_ko, name, id), xp, sprite_number FROM agents WHERE xp > 0",
    );
    let mut bind_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(ref agent_id) = params.agent_id {
        sql.push_str(&format!(" AND id = ?{}", bind_params.len() + 1));
        bind_params.push(Box::new(agent_id.clone()));
    }

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        bind_params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    let agents: Vec<(String, String, String, i64, i64)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            ))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    let sprite_emojis = ["🤖", "🦾", "🧠", "⚡", "🎯", "🔥", "💎", "🌟", "🏆", "👾"];

    let mut achievements = Vec::new();
    for (agent_id, name, name_ko, xp, sprite) in &agents {
        for (threshold, achievement_type, description) in milestones {
            if xp >= threshold {
                let emoji = sprite_emojis.get(*sprite as usize % sprite_emojis.len()).unwrap_or(&"🤖");
                achievements.push(json!({
                    "id": format!("{agent_id}:{achievement_type}"),
                    "agent_id": agent_id,
                    "type": achievement_type,
                    "name": format!("{description} ({threshold} XP)"),
                    "description": description,
                    "earned_at": 0,
                    "agent_name": name,
                    "agent_name_ko": name_ko,
                    "avatar_emoji": emoji,
                }));
            }
        }
    }

    (StatusCode::OK, Json(json!({ "achievements": achievements })))
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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let date = params.date.unwrap_or_else(|| {
        // 오늘 날짜 (UTC)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let days = now / 86400;
        let y = 1970 + days / 365;
        let rem = days % 365;
        let m = 1 + rem / 30;
        let d = 1 + rem % 30;
        format!("{y:04}-{m:02}-{d:02}")
    });

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
            if let Ok(rows) = stmt.query_map(rusqlite::params![&date, hour as i64], |row| {
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
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let limit = params.limit.unwrap_or(20);

    // audit_logs 테이블에서 읽기 (schema.rs에서 CREATE TABLE IF NOT EXISTS로 생성)
    let mut conditions = Vec::new();
    let mut bind_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
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

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        bind_values.iter().map(|v| v.as_ref()).collect();

    let rows = stmt
        .query_map(params_ref.as_slice(), |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "entityType": row.get::<_, Option<String>>(1)?,
                "entityId": row.get::<_, Option<String>>(2)?,
                "action": row.get::<_, Option<String>>(3)?,
                "timestamp": row.get::<_, Option<String>>(4)?,
                "actor": row.get::<_, Option<String>>(5)?,
            }))
        })
        .ok();

    let logs = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect::<Vec<_>>(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({ "logs": logs })))
}

/// GET /api/machine-status
pub async fn machine_status() -> (StatusCode, Json<serde_json::Value>) {
    let result = tokio::task::spawn_blocking(|| {
        let machines = vec![
            ("mac-mini", "mac-mini.local"),
            ("mac-book", "mac-book.local"),
        ];
        let mut results = Vec::new();
        for (name, host) in machines {
            let online = Command::new("ping")
                .args(["-c1", "-W2", host])
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
pub async fn rate_limits(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT provider, data, fetched_at FROM rate_limit_cache ORDER BY provider",
    ) {
        Ok(s) => s,
        Err(_) => return (StatusCode::OK, Json(json!({"providers": []}))),
    };

    let now = chrono::Utc::now().timestamp();
    let providers: Vec<serde_json::Value> = stmt
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
                    let buckets = parsed.get("buckets")?.as_array()?.clone();
                    let stale = (now - fetched_at) > 600; // >10min = stale
                    Some(json!({
                        "provider": provider,
                        "buckets": buckets,
                        "fetched_at": fetched_at,
                        "stale": stale,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    (StatusCode::OK, Json(json!({"providers": providers})))
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

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT DATE(used_at) as day, COUNT(*) as count
         FROM skill_usage
         WHERE used_at >= datetime('now', '-' || ?1 || ' days')
         GROUP BY DATE(used_at)
         ORDER BY day",
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
        .query_map([days], |row| {
            Ok(json!({
                "day": row.get::<_, String>(0)?,
                "count": row.get::<_, i64>(1)?,
            }))
        })
        .ok();

    let trend = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect::<Vec<_>>(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"trend": trend})))
}
