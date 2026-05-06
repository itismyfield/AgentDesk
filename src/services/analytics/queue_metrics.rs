use super::dto::{
    AnalyticsResponse, AuditLogsParams, AuditLogsResponse, InvariantsResponse,
    ObservabilityResponse, PolicyHooksParams, PolicyHooksResponse, QualityEventsResponse,
    SkillsTrendResponse,
};
use serde_json::{Value, json};
use sqlx::{PgPool, QueryBuilder, Row};
use std::collections::BTreeMap;

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
