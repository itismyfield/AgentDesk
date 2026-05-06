use chrono::{DateTime, Utc};
use sqlx::Row;

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

    let guild_id = guild_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut resolver = SessionActivityResolver::new();
    let resolved: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let session_key = row
                .try_get::<Option<String>, _>("session_key")
                .ok()
                .flatten();
            let status = row.try_get::<Option<String>, _>("status").ok().flatten();
            let active_dispatch_id = row
                .try_get::<Option<String>, _>("active_dispatch_id")
                .ok()
                .flatten();
            let last_heartbeat = pg_timestamp_to_rfc3339(
                row.try_get::<Option<DateTime<Utc>>, _>("last_heartbeat")
                    .ok()
                    .flatten(),
            );
            let provider = row.try_get::<Option<String>, _>("provider").ok().flatten();
            let thread_channel_id = row
                .try_get::<Option<String>, _>("thread_channel_id")
                .ok()
                .flatten();

            let effective = resolver.resolve(
                session_key.as_deref(),
                status.as_deref(),
                active_dispatch_id.as_deref(),
                last_heartbeat.as_deref(),
            );

            let (channel_web_url, channel_deeplink_url) =
                build_channel_deeplinks(thread_channel_id.as_deref(), guild_id.as_deref());
            let kanban_card_id = row
                .try_get::<Option<String>, _>("kanban_card_id")
                .ok()
                .flatten();

            dispatched_session_json(
                row.try_get::<i64, _>("id").unwrap_or(0),
                session_key,
                row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
                provider,
                effective.status,
                effective.active_dispatch_id,
                row.try_get::<Option<String>, _>("model").ok().flatten(),
                row.try_get::<i64, _>("tokens").unwrap_or(0),
                row.try_get::<Option<String>, _>("cwd").ok().flatten(),
                last_heartbeat,
                thread_channel_id,
                guild_id.clone(),
                channel_web_url,
                channel_deeplink_url,
                kanban_card_id,
            )
        })
        .collect();

    Ok(dedup_dispatched_sessions(resolved))
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

    Ok(rows
        .iter()
        .map(|row| {
            timeline_event_json(
                row.try_get::<String, _>("id").unwrap_or_default(),
                row.try_get::<String, _>("source").unwrap_or_default(),
                row.try_get::<String, _>("type").unwrap_or_default(),
                row.try_get::<Option<String>, _>("title").ok().flatten(),
                row.try_get::<Option<String>, _>("status").ok().flatten(),
                row.try_get::<Option<i64>, _>("timestamp").ok().flatten(),
                row.try_get::<Option<i64>, _>("duration_ms").ok().flatten(),
            )
        })
        .collect())
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
