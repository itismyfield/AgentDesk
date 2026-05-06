use chrono::{DateTime, Utc};
use sqlx::{Row, postgres::PgRow};

use crate::server::dto::agents::{
    agent_office_json, agent_skill_json, build_channel_deeplinks, dedup_dispatched_sessions,
    dispatched_session_json, timeline_event_json,
};
use crate::server::routes::session_activity::SessionActivityResolver;

#[derive(Debug, Clone)]
pub struct AgentDiagSession {
    pub session_key: String,
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub provider: Option<String>,
    pub status: Option<String>,
    pub last_tool_at: Option<DateTime<Utc>>,
    pub active_children: i32,
    pub thread_channel_id: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub enum AgentQueryLookupError {
    AgentNotFound,
    Query(sqlx::Error),
}

impl From<sqlx::Error> for AgentQueryLookupError {
    fn from(error: sqlx::Error) -> Self {
        Self::Query(error)
    }
}

#[derive(Debug, Clone, Default)]
struct AgentDispatchedSessionRow {
    id: i64,
    session_key: Option<String>,
    agent_id: Option<String>,
    provider: Option<String>,
    raw_status: Option<String>,
    active_dispatch_id: Option<String>,
    model: Option<String>,
    tokens: i64,
    cwd: Option<String>,
    last_heartbeat: Option<String>,
    thread_channel_id: Option<String>,
    kanban_card_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct AgentTimelineEventRow {
    id: String,
    source: String,
    event_type: String,
    title: Option<String>,
    status: Option<String>,
    timestamp: Option<i64>,
    duration_ms: Option<i64>,
}

pub async fn agent_exists_pg(pool: &sqlx::PgPool, id: &str) -> Result<bool, sqlx::Error> {
    let row = sqlx::query("SELECT COUNT(*)::BIGINT AS count FROM agents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.try_get::<i64, _>("count").unwrap_or(0) > 0)
}

pub fn pg_timestamp_to_rfc3339(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(|value| value.to_rfc3339())
}

pub async fn find_diag_session_pg(
    pool: &sqlx::PgPool,
    identifier: &str,
) -> Result<Option<AgentDiagSession>, sqlx::Error> {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        return Ok(None);
    }

    let row = sqlx::query(
        "SELECT COALESCE(s.session_key, '') AS session_key,
                s.agent_id,
                a.name AS agent_name,
                s.provider,
                s.status,
                s.last_tool_at,
                COALESCE(s.active_children, 0) AS active_children,
                s.thread_channel_id::TEXT AS thread_channel_id,
                s.created_at
           FROM sessions s
           LEFT JOIN agents a ON a.id = s.agent_id
          WHERE s.agent_id = $1
             OR s.thread_channel_id::TEXT = $1
             OR a.discord_channel_id = $1
             OR a.discord_channel_alt = $1
             OR a.discord_channel_cc = $1
             OR a.discord_channel_cdx = $1
          ORDER BY CASE
                       WHEN s.status IN ('turn_active', 'working') THEN 0
                       WHEN s.status = 'awaiting_bg' THEN 1
                       ELSE 2
                   END,
                   s.last_heartbeat DESC NULLS LAST,
                   s.last_tool_at DESC NULLS LAST,
                   s.created_at DESC NULLS LAST,
                   s.id DESC
          LIMIT 1",
    )
    .bind(identifier)
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        Ok(AgentDiagSession {
            session_key: row.try_get("session_key")?,
            agent_id: row.try_get("agent_id").ok().flatten(),
            agent_name: row.try_get("agent_name").ok().flatten(),
            provider: row.try_get("provider").ok().flatten(),
            status: row.try_get("status").ok().flatten(),
            last_tool_at: row.try_get("last_tool_at").ok().flatten(),
            active_children: row.try_get("active_children").unwrap_or(0),
            thread_channel_id: row.try_get("thread_channel_id").ok().flatten(),
            created_at: row.try_get("created_at").ok().flatten(),
        })
    })
    .transpose()
}

pub async fn list_agent_offices_pg_json(
    pool: &sqlx::PgPool,
    agent_id: &str,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT o.id, o.name, o.layout, oa.department_id, oa.joined_at
         FROM office_agents oa
         INNER JOIN offices o ON o.id = oa.office_id
         WHERE oa.agent_id = $1
         ORDER BY o.id",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            agent_office_json(
                row.try_get::<String, _>("id").unwrap_or_default(),
                row.try_get::<Option<String>, _>("name").ok().flatten(),
                row.try_get::<Option<String>, _>("layout").ok().flatten(),
                row.try_get::<Option<String>, _>("department_id")
                    .ok()
                    .flatten(),
                pg_timestamp_to_rfc3339(
                    row.try_get::<Option<DateTime<Utc>>, _>("joined_at")
                        .ok()
                        .flatten(),
                ),
            )
        })
        .collect())
}

pub async fn list_agent_skills_pg_json(
    pool: &sqlx::PgPool,
    agent_id: &str,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT DISTINCT s.id, s.name, s.description, s.source_path, s.trigger_patterns, s.updated_at
         FROM skills s
         INNER JOIN skill_usage su ON su.skill_id = s.id
         WHERE su.agent_id = $1
         ORDER BY s.id",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            agent_skill_json(
                row.try_get::<String, _>("id").unwrap_or_default(),
                row.try_get::<Option<String>, _>("name").ok().flatten(),
                row.try_get::<Option<String>, _>("description")
                    .ok()
                    .flatten(),
                row.try_get::<Option<String>, _>("source_path")
                    .ok()
                    .flatten(),
                row.try_get::<Option<String>, _>("trigger_patterns")
                    .ok()
                    .flatten(),
                pg_timestamp_to_rfc3339(
                    row.try_get::<Option<DateTime<Utc>>, _>("updated_at")
                        .ok()
                        .flatten(),
                ),
            )
        })
        .collect())
}

pub async fn list_agent_dispatched_sessions_pg_json(
    pool: &sqlx::PgPool,
    agent_id: &str,
    guild_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    // SQL only orders by recency. Dedupe + activity-aware ranking are done in
    // application code with SessionActivityResolver because raw status can lag.
    let rows = sqlx::query(
        "SELECT s.id, s.session_key, s.agent_id, s.provider, s.status, s.active_dispatch_id,
                s.model, s.tokens, s.cwd, s.last_heartbeat, s.thread_channel_id,
                td.kanban_card_id AS kanban_card_id
         FROM sessions s
         LEFT JOIN task_dispatches td ON td.id = s.active_dispatch_id
         WHERE s.agent_id = $1
         ORDER BY COALESCE(s.last_heartbeat, s.created_at) DESC NULLS LAST, s.id DESC",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await?;

    let rows = rows
        .iter()
        .map(agent_dispatched_session_row_from_pg)
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok(build_agent_dispatched_sessions_json(rows, guild_id))
}

pub async fn load_agent_dispatched_sessions_pg_json(
    pool: &sqlx::PgPool,
    agent_id: &str,
    guild_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, AgentQueryLookupError> {
    if !agent_exists_pg(pool, agent_id).await? {
        return Err(AgentQueryLookupError::AgentNotFound);
    }

    Ok(list_agent_dispatched_sessions_pg_json(pool, agent_id, guild_id).await?)
}

fn agent_dispatched_session_row_from_pg(
    row: &PgRow,
) -> Result<AgentDispatchedSessionRow, sqlx::Error> {
    Ok(AgentDispatchedSessionRow {
        id: row.try_get::<i64, _>("id").unwrap_or(0),
        session_key: row
            .try_get::<Option<String>, _>("session_key")
            .ok()
            .flatten(),
        agent_id: row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
        provider: row.try_get::<Option<String>, _>("provider").ok().flatten(),
        raw_status: row.try_get::<Option<String>, _>("status").ok().flatten(),
        active_dispatch_id: row
            .try_get::<Option<String>, _>("active_dispatch_id")
            .ok()
            .flatten(),
        model: row.try_get::<Option<String>, _>("model").ok().flatten(),
        tokens: row.try_get::<i64, _>("tokens").unwrap_or(0),
        cwd: row.try_get::<Option<String>, _>("cwd").ok().flatten(),
        last_heartbeat: pg_timestamp_to_rfc3339(
            row.try_get::<Option<DateTime<Utc>>, _>("last_heartbeat")
                .ok()
                .flatten(),
        ),
        thread_channel_id: row
            .try_get::<Option<String>, _>("thread_channel_id")
            .ok()
            .flatten(),
        kanban_card_id: row
            .try_get::<Option<String>, _>("kanban_card_id")
            .ok()
            .flatten(),
    })
}

fn build_agent_dispatched_sessions_json(
    rows: Vec<AgentDispatchedSessionRow>,
    guild_id: Option<&str>,
) -> Vec<serde_json::Value> {
    let guild_id = guild_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut resolver = SessionActivityResolver::new();
    let resolved: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            let effective = resolver.resolve(
                row.session_key.as_deref(),
                row.raw_status.as_deref(),
                row.active_dispatch_id.as_deref(),
                row.last_heartbeat.as_deref(),
            );

            let (channel_web_url, channel_deeplink_url) =
                build_channel_deeplinks(row.thread_channel_id.as_deref(), guild_id.as_deref());

            dispatched_session_json(
                row.id,
                row.session_key,
                row.agent_id,
                row.provider,
                effective.status,
                effective.active_dispatch_id,
                row.model,
                row.tokens,
                row.cwd,
                row.last_heartbeat,
                row.thread_channel_id,
                guild_id.clone(),
                channel_web_url,
                channel_deeplink_url,
                row.kanban_card_id,
            )
        })
        .collect();

    dedup_dispatched_sessions(resolved)
}

pub async fn list_agent_timeline_pg_json(
    pool: &sqlx::PgPool,
    agent_id: &str,
    limit: i64,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query(
        "
        SELECT id, source, type, title, status, timestamp, duration_ms FROM (
            SELECT
                id,
                'dispatch' AS source,
                COALESCE(dispatch_type, 'task') AS type,
                title,
                status,
                (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS timestamp,
                CASE
                    WHEN updated_at IS NOT NULL AND created_at IS NOT NULL
                    THEN ((EXTRACT(EPOCH FROM updated_at) - EXTRACT(EPOCH FROM created_at)) * 1000)::BIGINT
                    ELSE NULL
                END AS duration_ms
            FROM task_dispatches
            WHERE to_agent_id = $1 OR from_agent_id = $1

            UNION ALL

            SELECT
                id::TEXT,
                'session' AS source,
                'session' AS type,
                COALESCE(session_key, 'session') AS title,
                status,
                (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS timestamp,
                CASE
                    WHEN last_heartbeat IS NOT NULL AND created_at IS NOT NULL
                    THEN ((EXTRACT(EPOCH FROM last_heartbeat) - EXTRACT(EPOCH FROM created_at)) * 1000)::BIGINT
                    ELSE NULL
                END AS duration_ms
            FROM sessions
            WHERE agent_id = $1

            UNION ALL

            SELECT
                id,
                'kanban' AS source,
                'card' AS type,
                title,
                status,
                (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS timestamp,
                CASE
                    WHEN updated_at IS NOT NULL AND created_at IS NOT NULL
                    THEN ((EXTRACT(EPOCH FROM updated_at) - EXTRACT(EPOCH FROM created_at)) * 1000)::BIGINT
                    ELSE NULL
                END AS duration_ms
            FROM kanban_cards
            WHERE assigned_agent_id = $1
        )
        ORDER BY timestamp DESC
        LIMIT $2
    ",
    )
    .bind(agent_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let rows = rows
        .iter()
        .map(agent_timeline_event_row_from_pg)
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok(build_agent_timeline_json(rows))
}

pub async fn load_agent_timeline_pg_json(
    pool: &sqlx::PgPool,
    agent_id: &str,
    limit: i64,
) -> Result<Vec<serde_json::Value>, AgentQueryLookupError> {
    if !agent_exists_pg(pool, agent_id).await? {
        return Err(AgentQueryLookupError::AgentNotFound);
    }

    Ok(list_agent_timeline_pg_json(pool, agent_id, limit).await?)
}

fn agent_timeline_event_row_from_pg(row: &PgRow) -> Result<AgentTimelineEventRow, sqlx::Error> {
    Ok(AgentTimelineEventRow {
        id: row.try_get::<String, _>("id").unwrap_or_default(),
        source: row.try_get::<String, _>("source").unwrap_or_default(),
        event_type: row.try_get::<String, _>("type").unwrap_or_default(),
        title: row.try_get::<Option<String>, _>("title").ok().flatten(),
        status: row.try_get::<Option<String>, _>("status").ok().flatten(),
        timestamp: row.try_get::<Option<i64>, _>("timestamp").ok().flatten(),
        duration_ms: row.try_get::<Option<i64>, _>("duration_ms").ok().flatten(),
    })
}

fn build_agent_timeline_json(rows: Vec<AgentTimelineEventRow>) -> Vec<serde_json::Value> {
    rows.into_iter()
        .map(|row| {
            timeline_event_json(
                row.id,
                row.source,
                row.event_type,
                row.title,
                row.status,
                row.timestamp,
                row.duration_ms,
            )
        })
        .collect()
}

pub async fn mark_session_disconnected_pg(pool: &sqlx::PgPool, session_key: &str) {
    sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected',
             active_dispatch_id = NULL,
             claude_session_id = NULL,
             raw_provider_session_id = NULL
         WHERE session_key = $1",
    )
    .bind(session_key)
    .execute(pool)
    .await
    .ok();
}

pub async fn block_active_card_for_agent_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
    reason: &str,
) -> Result<Option<String>, sqlx::Error> {
    let card_id: Option<String> = sqlx::query_scalar(
        "SELECT id
         FROM kanban_cards
         WHERE assigned_agent_id = $1 AND status = 'in_progress'
         ORDER BY updated_at DESC
         LIMIT 1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await?;

    if let Some(card_id) = card_id.as_deref() {
        sqlx::query(
            "UPDATE kanban_cards SET blocked_reason = $1, updated_at = NOW() WHERE id = $2",
        )
        .bind(reason)
        .bind(card_id)
        .execute(pool)
        .await
        .ok();
    }

    Ok(card_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatched_session_service_maps_happy_row() {
        let sessions = build_agent_dispatched_sessions_json(
            vec![AgentDispatchedSessionRow {
                id: 7,
                session_key: Some("remote:agentdesk-1".to_string()),
                agent_id: Some("project-agentdesk".to_string()),
                provider: Some("codex".to_string()),
                raw_status: Some("awaiting_bg".to_string()),
                active_dispatch_id: Some("dispatch-1".to_string()),
                model: Some("gpt-5.3-codex".to_string()),
                tokens: 42,
                cwd: Some("/work/repo".to_string()),
                last_heartbeat: Some("2026-05-06T01:00:00+00:00".to_string()),
                thread_channel_id: Some("1501429790727606332".to_string()),
                kanban_card_id: Some("card-1".to_string()),
            }],
            Some("1490141479707086938"),
        );

        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session["id"], 7);
        assert_eq!(session["session_key"], "remote:agentdesk-1");
        assert_eq!(session["agent_id"], "project-agentdesk");
        assert_eq!(session["provider"], "codex");
        assert_eq!(session["status"], "awaiting_bg");
        assert_eq!(session["model"], "gpt-5.3-codex");
        assert_eq!(session["tokens"], 42);
        assert_eq!(session["cwd"], "/work/repo");
        assert_eq!(session["last_heartbeat"], "2026-05-06T01:00:00+00:00");
        assert_eq!(session["thread_channel_id"], "1501429790727606332");
        assert_eq!(session["kanban_card_id"], "card-1");
        assert_eq!(
            session["channel_web_url"],
            "https://discord.com/channels/1490141479707086938/1501429790727606332"
        );
        assert_eq!(
            session["channel_deeplink_url"],
            "discord://discord.com/channels/1490141479707086938/1501429790727606332"
        );
    }

    #[test]
    fn dispatched_session_service_maps_empty_rows() {
        let sessions = build_agent_dispatched_sessions_json(Vec::new(), Some("guild-1"));

        assert!(sessions.is_empty());
    }

    #[test]
    fn timeline_service_maps_happy_row() {
        let events = build_agent_timeline_json(vec![AgentTimelineEventRow {
            id: "dispatch-1".to_string(),
            source: "dispatch".to_string(),
            event_type: "implementation".to_string(),
            title: Some("Implement service split".to_string()),
            status: Some("completed".to_string()),
            timestamp: Some(1_777_777_000),
            duration_ms: Some(4200),
        }]);

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event["id"], "dispatch-1");
        assert_eq!(event["source"], "dispatch");
        assert_eq!(event["type"], "implementation");
        assert_eq!(event["title"], "Implement service split");
        assert_eq!(event["status"], "completed");
        assert_eq!(event["timestamp"], 1_777_777_000);
        assert_eq!(event["duration_ms"], 4200);
    }

    #[test]
    fn timeline_service_maps_empty_rows() {
        let events = build_agent_timeline_json(Vec::new());

        assert!(events.is_empty());
    }
}
