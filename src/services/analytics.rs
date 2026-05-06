use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, QueryBuilder, Row};
use std::{collections::BTreeMap, process::Command};

pub mod api_usage;
pub use api_usage::{RateLimitsResponse, build_rate_limit_provider_payloads_pg, rate_limits_pg};

#[derive(Debug, Serialize)]
pub struct AnalyticsResponse {
    pub generated_at: String,
    pub counters: Vec<Value>,
    pub events: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct QualityEventsResponse {
    pub events: Vec<Value>,
    pub generated_at_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct InvariantsResponse {
    pub generated_at: String,
    pub total_violations: i64,
    pub counts: Vec<Value>,
    pub recent: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct ObservabilityResponse {
    pub counters: Value,
    pub recent_events: Value,
    pub watcher_first_relay: Value,
    pub generated_at_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct PolicyHooksResponse {
    pub events: Vec<Value>,
    pub generated_at_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct StreaksResponse {
    pub streaks: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AchievementsResponse {
    pub achievements: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct ActivityHeatmapResponse {
    pub hours: Vec<Value>,
    pub date: String,
}

#[derive(Debug, Serialize)]
pub struct AuditLogsResponse {
    pub logs: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct MachineStatusResponse {
    pub machines: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct SkillsTrendResponse {
    pub trend: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct PolicyHooksParams {
    pub policy_name: Option<String>,
    pub hook_name: Option<String>,
    pub last_minutes: Option<i64>,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct AuditLogsParams<'a> {
    pub limit: i64,
    pub entity_type: Option<&'a str>,
    pub entity_id: Option<&'a str>,
    pub agent_id: Option<&'a str>,
}

pub async fn query_analytics_pg(
    pool: &PgPool,
    filters: &crate::services::observability::AnalyticsFilters,
) -> Result<AnalyticsResponse, sqlx::Error> {
    let limit = filters.event_limit.min(1000) as i64;
    let mut events_query = QueryBuilder::new(
        "SELECT id::TEXT AS id,
                provider,
                channel_id,
                event_type::TEXT AS event_type,
                payload::TEXT AS payload,
                created_at::TEXT AS created_at
           FROM agent_quality_event WHERE 1=1",
    );
    if let Some(provider) = filters.provider.as_deref() {
        events_query.push(" AND provider = ").push_bind(provider);
    }
    if let Some(channel_id) = filters.channel_id.as_deref() {
        events_query
            .push(" AND channel_id = ")
            .push_bind(channel_id);
    }
    if let Some(event_type) = filters.event_type.as_deref() {
        events_query
            .push(" AND event_type::TEXT = ")
            .push_bind(event_type);
    }
    events_query
        .push(" ORDER BY created_at DESC LIMIT ")
        .push_bind(limit);

    let events = events_query
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "provider": row.try_get::<Option<String>, _>("provider").ok().flatten(),
                "channel_id": row.try_get::<Option<String>, _>("channel_id").ok().flatten(),
                "event_type": row.try_get::<String, _>("event_type").unwrap_or_default(),
                "payload": row.try_get::<Option<String>, _>("payload").ok().flatten(),
                "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();

    Ok(AnalyticsResponse {
        generated_at: chrono::Utc::now().to_rfc3339(),
        counters: Vec::new(),
        events,
    })
}

pub async fn query_agent_quality_events_pg(
    pool: &PgPool,
    filters: &crate::services::observability::AgentQualityFilters,
) -> Result<QualityEventsResponse, sqlx::Error> {
    let days = filters.days.clamp(1, 365);
    let limit = filters.limit.clamp(1, 1000) as i64;
    let mut query = QueryBuilder::new(
        "SELECT id::TEXT AS id,
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type::TEXT AS event_type,
                payload::TEXT AS payload,
                created_at::TEXT AS created_at
           FROM agent_quality_event
          WHERE created_at >= NOW() - (",
    );
    query.push_bind(days).push("::BIGINT * INTERVAL '1 day')");
    if let Some(agent_id) = filters.agent_id.as_deref() {
        query.push(" AND agent_id = ").push_bind(agent_id);
    }
    query
        .push(" ORDER BY created_at DESC LIMIT ")
        .push_bind(limit);

    let events = query
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "source_event_id": row.try_get::<Option<String>, _>("source_event_id").ok().flatten(),
                "correlation_id": row.try_get::<Option<String>, _>("correlation_id").ok().flatten(),
                "agent_id": row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
                "provider": row.try_get::<Option<String>, _>("provider").ok().flatten(),
                "channel_id": row.try_get::<Option<String>, _>("channel_id").ok().flatten(),
                "card_id": row.try_get::<Option<String>, _>("card_id").ok().flatten(),
                "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id").ok().flatten(),
                "event_type": row.try_get::<String, _>("event_type").unwrap_or_default(),
                "payload": row.try_get::<Option<String>, _>("payload").ok().flatten(),
                "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect();

    Ok(QualityEventsResponse {
        events,
        generated_at_ms: chrono::Utc::now().timestamp_millis(),
    })
}

pub async fn query_invariants_pg(
    pool: &PgPool,
    filters: &crate::services::observability::InvariantAnalyticsFilters,
) -> Result<InvariantsResponse, sqlx::Error> {
    let limit = filters.limit.min(1000) as i64;
    let mut query = QueryBuilder::new(
        "SELECT provider,
                channel_id,
                event_type::TEXT AS invariant,
                COUNT(*)::BIGINT AS count
           FROM agent_quality_event
          WHERE event_type::TEXT LIKE '%invariant%'",
    );
    if let Some(provider) = filters.provider.as_deref() {
        query.push(" AND provider = ").push_bind(provider);
    }
    if let Some(channel_id) = filters.channel_id.as_deref() {
        query.push(" AND channel_id = ").push_bind(channel_id);
    }
    if let Some(invariant) = filters.invariant.as_deref() {
        query.push(" AND event_type::TEXT = ").push_bind(invariant);
    }
    query.push(" GROUP BY provider, channel_id, event_type ORDER BY count DESC LIMIT ");
    query.push_bind(limit);

    let counts = query
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            json!({
                "provider": row.try_get::<Option<String>, _>("provider").ok().flatten(),
                "channel_id": row.try_get::<Option<String>, _>("channel_id").ok().flatten(),
                "invariant": row.try_get::<String, _>("invariant").unwrap_or_default(),
                "count": row.try_get::<i64, _>("count").unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();
    let total_violations = counts
        .iter()
        .filter_map(|row| row["count"].as_i64())
        .sum::<i64>();

    Ok(InvariantsResponse {
        generated_at: chrono::Utc::now().to_rfc3339(),
        total_violations,
        counts,
        recent: Vec::new(),
    })
}

pub fn observability_response(recent_limit: usize) -> ObservabilityResponse {
    let limit = recent_limit.min(1000);
    let counters = crate::services::observability::metrics::snapshot();
    let recent_events = crate::services::observability::events::recent(limit);
    let watcher_first_relay = crate::services::observability::watcher_latency::snapshot();
    ObservabilityResponse {
        counters: serde_json::to_value(counters).unwrap_or(Value::Null),
        recent_events: serde_json::to_value(recent_events).unwrap_or(Value::Null),
        watcher_first_relay: serde_json::to_value(watcher_first_relay).unwrap_or(Value::Null),
        generated_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

pub fn policy_hooks_response(params: PolicyHooksParams) -> PolicyHooksResponse {
    let pool = crate::services::observability::events::recent(
        crate::services::observability::events::MAX_EVENTS,
    );
    let now_ms = chrono::Utc::now().timestamp_millis();
    let window_ms = params.last_minutes.map(|m| m.saturating_mul(60_000));

    let mut matched: Vec<Value> = Vec::new();
    for ev in pool.into_iter().rev() {
        if ev.event_type != "policy_hook_executed" {
            continue;
        }
        if let Some(window) = window_ms {
            if now_ms.saturating_sub(ev.timestamp_ms) > window {
                continue;
            }
        }
        if let Some(ref needed) = params.policy_name {
            let ok = ev
                .payload
                .get("policy_name")
                .and_then(|v| v.as_str())
                .map(|s| s == needed.as_str())
                .unwrap_or(false);
            if !ok {
                continue;
            }
        }
        if let Some(ref needed) = params.hook_name {
            let ok = ev
                .payload
                .get("hook_name")
                .and_then(|v| v.as_str())
                .map(|s| s == needed.as_str())
                .unwrap_or(false);
            if !ok {
                continue;
            }
        }
        matched.push(json!({
            "timestamp_ms": ev.timestamp_ms,
            "policy_name": ev.payload.get("policy_name").cloned().unwrap_or(Value::Null),
            "hook_name": ev.payload.get("hook_name").cloned().unwrap_or(Value::Null),
            "policy_version": ev.payload.get("policy_version").cloned().unwrap_or(Value::Null),
            "duration_ms": ev.payload.get("duration_ms").cloned().unwrap_or(Value::Null),
            "result": ev.payload.get("result").cloned().unwrap_or(Value::Null),
            "effects_count": ev.payload.get("effects_count").cloned().unwrap_or(Value::Null),
        }));
        if matched.len() >= params.limit {
            break;
        }
    }

    PolicyHooksResponse {
        events: matched,
        generated_at_ms: now_ms,
    }
}

pub async fn streaks_pg(pool: &PgPool) -> Result<StreaksResponse, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT a.id, a.name, a.avatar_emoji,
                STRING_AGG(DISTINCT td.updated_at::date::text, ',') AS active_dates,
                MAX(td.updated_at)::text AS last_active
         FROM agents a
         INNER JOIN task_dispatches td ON td.to_agent_id = a.id
         WHERE td.status = 'completed'
         GROUP BY a.id
         ORDER BY last_active DESC",
    )
    .fetch_all(pool)
    .await?;

    let streaks = rows
        .into_iter()
        .map(|row| {
            let agent_id = row.try_get::<String, _>("id").unwrap_or_default();
            let name = row.try_get::<Option<String>, _>("name").ok().flatten();
            let avatar_emoji = row
                .try_get::<Option<String>, _>("avatar_emoji")
                .ok()
                .flatten();
            let active_dates_str = row
                .try_get::<Option<String>, _>("active_dates")
                .ok()
                .flatten();
            let last_active = row
                .try_get::<Option<String>, _>("last_active")
                .ok()
                .flatten();
            let streak = if let Some(ref dates_str) = active_dates_str {
                let mut dates: Vec<&str> = dates_str.split(',').collect();
                dates.sort();
                dates.reverse();
                compute_streak(&dates)
            } else {
                0
            };

            json!({
                "agent_id": agent_id,
                "name": name,
                "avatar_emoji": avatar_emoji,
                "streak": streak,
                "last_active": last_active,
            })
        })
        .collect::<Vec<_>>();

    Ok(StreaksResponse { streaks })
}

fn compute_streak(sorted_dates_desc: &[&str]) -> i64 {
    if sorted_dates_desc.is_empty() {
        return 0;
    }

    let today = chrono_today();
    let mut streak = 0i64;
    let mut expected_date = today;

    for date_str in sorted_dates_desc {
        if let Some(d) = parse_date(date_str) {
            if d == expected_date {
                streak += 1;
                expected_date = d - 1;
            } else if d < expected_date {
                break;
            }
        }
    }

    streak
}

fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    Some(y * 365 + m * 30 + d)
}

fn chrono_today() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (now / 86400) as i64;
    let approx_year = 1970 + days / 365;
    let remaining = days % 365;
    let approx_month = 1 + remaining / 30;
    let approx_day = 1 + remaining % 30;
    approx_year * 365 + approx_month * 30 + approx_day
}

pub async fn achievements_pg(
    pool: &PgPool,
    agent_id: Option<&str>,
) -> Result<AchievementsResponse, sqlx::Error> {
    let milestones: &[(i64, &str, &str)] = &[
        (10, "first_task", "첫 번째 작업 완료"),
        (50, "getting_started", "본격적인 시작"),
        (100, "centurion", "100 XP 달성"),
        (250, "veteran", "베테랑"),
        (500, "expert", "전문가"),
        (1000, "master", "마스터"),
    ];

    let mut query = QueryBuilder::new(
        "SELECT id, COALESCE(name, id), COALESCE(name_ko, name, id), xp, avatar_emoji FROM agents WHERE xp > 0",
    );
    if let Some(agent_id) = agent_id {
        query.push(" AND id = ").push_bind(agent_id);
    }

    let agents: Vec<(String, String, String, i64, String)> = query
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>(0).unwrap_or_default(),
                row.try_get::<String, _>(1).unwrap_or_default(),
                row.try_get::<String, _>(2).unwrap_or_default(),
                row.try_get::<i64, _>(3).unwrap_or(0),
                row.try_get::<Option<String>, _>(4)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "🤖".to_string()),
            )
        })
        .collect();

    let mut agent_completed_times: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    for (agent_id, _, _, _, _) in &agents {
        let times: Vec<i64> = sqlx::query_scalar(
            "SELECT (EXTRACT(EPOCH FROM updated_at)::BIGINT * 1000) AS completed_at_ms
             FROM task_dispatches WHERE to_agent_id = $1 AND status = 'completed'
             ORDER BY updated_at ASC",
        )
        .bind(agent_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        agent_completed_times.insert(agent_id.clone(), times);
    }

    let mut achievements = Vec::new();
    for (agent_id, name, name_ko, xp, avatar_emoji) in &agents {
        let completion_times = agent_completed_times.get(agent_id.as_str());
        for (threshold, achievement_type, description) in milestones {
            if xp >= threshold {
                let approx_index = (*threshold as usize / 10).saturating_sub(1);
                let earned_at = completion_times
                    .and_then(|times| times.get(approx_index.min(times.len().saturating_sub(1))))
                    .copied()
                    .unwrap_or(0);

                achievements.push(json!({
                    "id": format!("{agent_id}:{achievement_type}"),
                    "agent_id": agent_id,
                    "type": achievement_type,
                    "name": format!("{description} ({threshold} XP)"),
                    "description": description,
                    "earned_at": earned_at,
                    "agent_name": name,
                    "agent_name_ko": name_ko,
                    "avatar_emoji": avatar_emoji.as_str(),
                }));
            }
        }
    }

    Ok(AchievementsResponse { achievements })
}

pub async fn activity_heatmap_pg(
    pool: &PgPool,
    date: String,
) -> Result<ActivityHeatmapResponse, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT EXTRACT(HOUR FROM td.created_at)::BIGINT AS hour,
                td.to_agent_id,
                COUNT(*)::BIGINT AS cnt
           FROM task_dispatches td
          WHERE td.created_at >= $1::date
            AND td.created_at < $1::date + INTERVAL '1 day'
            AND td.to_agent_id IS NOT NULL
          GROUP BY hour, td.to_agent_id",
    )
    .bind(&date)
    .fetch_all(pool)
    .await?;

    let mut buckets: Vec<serde_json::Map<String, Value>> =
        (0..24).map(|_| serde_json::Map::new()).collect();
    for row in rows {
        let hour = row.try_get::<i64, _>("hour").unwrap_or(-1);
        if !(0..24).contains(&hour) {
            continue;
        }
        let agent_id = match row.try_get::<String, _>("to_agent_id") {
            Ok(value) => value,
            Err(_) => continue,
        };
        let count = row.try_get::<i64, _>("cnt").unwrap_or(0);
        buckets[hour as usize].insert(agent_id, json!(count));
    }
    let hours = buckets
        .into_iter()
        .enumerate()
        .map(|(hour, agents)| json!({ "hour": hour, "agents": agents }))
        .collect();

    Ok(ActivityHeatmapResponse { hours, date })
}

pub async fn audit_logs_pg(pool: &PgPool, params: AuditLogsParams<'_>) -> AuditLogsResponse {
    let audit_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM audit_logs")
        .fetch_one(pool)
        .await
        .unwrap_or(0);

    let logs = if audit_count > 0 {
        let mut query = QueryBuilder::new(
            "SELECT a.id, a.entity_type, a.entity_id, a.action, a.timestamp, a.actor,
                    c.title AS card_title,
                    c.github_issue_number AS card_issue_number,
                    c.github_issue_url AS card_issue_url,
                    c.assigned_agent_id AS card_assigned_agent_id
             FROM audit_logs a
             LEFT JOIN kanban_cards c
               ON a.entity_type = 'kanban_card' AND a.entity_id = c.id
             WHERE 1=1",
        );
        if let Some(entity_type) = params.entity_type {
            query.push(" AND a.entity_type = ").push_bind(entity_type);
        }
        if let Some(entity_id) = params.entity_id {
            query.push(" AND a.entity_id = ").push_bind(entity_id);
        }
        if let Some(agent_id) = params.agent_id {
            query
                .push(" AND a.entity_type = 'kanban_card' AND c.assigned_agent_id = ")
                .push_bind(agent_id);
        }
        query
            .push(" ORDER BY a.timestamp DESC LIMIT ")
            .push_bind(params.limit);

        query
            .build()
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| {
                let entity_type = row
                    .try_get::<Option<String>, _>("entity_type")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "system".to_string());
                let entity_id = row
                    .try_get::<Option<String>, _>("entity_id")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let action = row
                    .try_get::<Option<String>, _>("action")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "updated".to_string());
                let created_at = row
                    .try_get::<chrono::DateTime<chrono::Utc>, _>("timestamp")
                    .map(|ts| ts.timestamp_millis())
                    .unwrap_or(0);
                let actor = row
                    .try_get::<Option<String>, _>("actor")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let card_title = row
                    .try_get::<Option<String>, _>("card_title")
                    .ok()
                    .flatten();
                let card_issue_number = row
                    .try_get::<Option<i32>, _>("card_issue_number")
                    .ok()
                    .flatten();
                let card_issue_url = row
                    .try_get::<Option<String>, _>("card_issue_url")
                    .ok()
                    .flatten();
                let card_assigned_agent_id = row
                    .try_get::<Option<String>, _>("card_assigned_agent_id")
                    .ok()
                    .flatten();
                let summary = build_audit_summary(
                    &entity_type,
                    &entity_id,
                    &action,
                    card_title.as_deref(),
                    card_issue_number,
                );
                json!({
                    "id": row.try_get::<i64, _>("id").unwrap_or(0).to_string(),
                    "actor": actor,
                    "action": action,
                    "entity_type": entity_type,
                    "entity_id": entity_id,
                    "summary": summary,
                    "created_at": created_at,
                    "card_title": card_title,
                    "card_issue_number": card_issue_number,
                    "card_issue_url": card_issue_url,
                    "card_assigned_agent_id": card_assigned_agent_id,
                })
            })
            .collect::<Vec<_>>()
    } else {
        if let Some(entity_type) = params.entity_type {
            if entity_type != "kanban_card" {
                return AuditLogsResponse { logs: Vec::new() };
            }
        }

        let mut query = QueryBuilder::new(
            "SELECT k.id, k.card_id, k.from_status, k.to_status, k.source, k.created_at,
                    c.title AS card_title,
                    c.github_issue_number AS card_issue_number,
                    c.github_issue_url AS card_issue_url,
                    c.assigned_agent_id AS card_assigned_agent_id
             FROM kanban_audit_logs k
             LEFT JOIN kanban_cards c ON k.card_id = c.id
             WHERE 1=1",
        );
        if let Some(card_id) = params.entity_id {
            query.push(" AND k.card_id = ").push_bind(card_id);
        }
        if let Some(agent_id) = params.agent_id {
            query
                .push(" AND c.assigned_agent_id = ")
                .push_bind(agent_id);
        }
        query
            .push(" ORDER BY k.created_at DESC LIMIT ")
            .push_bind(params.limit);

        query
            .build()
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| {
                let card_id = row.try_get::<String, _>("card_id").unwrap_or_default();
                let from_status = row
                    .try_get::<Option<String>, _>("from_status")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "unknown".to_string());
                let to_status = row
                    .try_get::<Option<String>, _>("to_status")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "unknown".to_string());
                let actor = row
                    .try_get::<Option<String>, _>("source")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "hook".to_string());
                let created_at = row
                    .try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                    .map(|ts| ts.timestamp_millis())
                    .unwrap_or(0);
                let card_title = row
                    .try_get::<Option<String>, _>("card_title")
                    .ok()
                    .flatten();
                let card_issue_number = row
                    .try_get::<Option<i32>, _>("card_issue_number")
                    .ok()
                    .flatten();
                let card_issue_url = row
                    .try_get::<Option<String>, _>("card_issue_url")
                    .ok()
                    .flatten();
                let card_assigned_agent_id = row
                    .try_get::<Option<String>, _>("card_assigned_agent_id")
                    .ok()
                    .flatten();
                let action = format!("{from_status}->{to_status}");
                let summary = build_audit_summary(
                    "kanban_card",
                    &card_id,
                    &action,
                    card_title.as_deref(),
                    card_issue_number,
                );
                json!({
                    "id": format!("kanban-{}", row.try_get::<i64, _>("id").unwrap_or(0)),
                    "actor": actor.clone(),
                    "action": action,
                    "entity_type": "kanban_card",
                    "entity_id": card_id,
                    "summary": summary,
                    "metadata": {
                        "from_status": from_status,
                        "to_status": to_status,
                        "source": actor,
                    },
                    "created_at": created_at,
                    "card_title": card_title,
                    "card_issue_number": card_issue_number,
                    "card_issue_url": card_issue_url,
                    "card_assigned_agent_id": card_assigned_agent_id,
                })
            })
            .collect::<Vec<_>>()
    };

    AuditLogsResponse { logs }
}

fn build_audit_summary(
    entity_type: &str,
    entity_id: &str,
    action: &str,
    card_title: Option<&str>,
    card_issue_number: Option<i32>,
) -> String {
    if entity_type == "kanban_card" {
        if let Some(title) = card_title {
            return match card_issue_number {
                Some(num) => format!("#{num} {title} · {action}"),
                None => format!("{title} · {action}"),
            };
        }
        if let Some(num) = card_issue_number {
            return format!("#{num} · {action}");
        }
    }
    if entity_id.is_empty() {
        format!("{entity_type} {action}")
    } else {
        format!("{entity_type}:{entity_id} {action}")
    }
}

fn parse_machine_config(value: &str) -> Option<Vec<(String, String)>> {
    serde_json::from_str::<Vec<Value>>(value)
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

async fn load_machine_config_pg(pool: &PgPool) -> Option<Vec<(String, String)>> {
    sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
        .bind("machines")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|value| parse_machine_config(&value))
}

pub async fn load_machine_config(pg_pool: Option<&PgPool>) -> Vec<(String, String)> {
    if let Some(pool) = pg_pool {
        return load_machine_config_pg(pool)
            .await
            .unwrap_or_else(default_machine_config);
    }

    default_machine_config()
}

pub async fn machine_status(pg_pool: Option<&PgPool>) -> MachineStatusResponse {
    let machines_config = load_machine_config(pg_pool).await;

    let machines = tokio::task::spawn_blocking(move || {
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
    .await
    .unwrap_or_default();

    MachineStatusResponse { machines }
}

pub fn skills_trend_from_days(days: impl IntoIterator<Item = String>) -> SkillsTrendResponse {
    let mut by_day = BTreeMap::<String, i64>::new();
    for day in days {
        *by_day.entry(day).or_default() += 1;
    }

    let trend = by_day
        .into_iter()
        .map(|(day, count)| json!({ "day": day, "count": count }))
        .collect();

    SkillsTrendResponse { trend }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_trend_from_days_counts_days_in_order() {
        let response = skills_trend_from_days([
            "2026-05-02".to_string(),
            "2026-05-01".to_string(),
            "2026-05-02".to_string(),
        ]);

        assert_eq!(
            response.trend,
            vec![
                json!({"day": "2026-05-01", "count": 1}),
                json!({"day": "2026-05-02", "count": 2}),
            ]
        );
    }
}
