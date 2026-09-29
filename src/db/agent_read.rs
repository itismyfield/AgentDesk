//! Database read models for the agent management views.
//! HTTP response shaping and runtime identity resolution belong to callers.

pub(crate) async fn list_agent_rows(
    pool: &sqlx::PgPool,
) -> Result<Vec<sqlx::postgres::PgRow>, String> {
    let sql = "
        SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
               a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
               a.status, a.xp, a.sprite_number, d.name AS department_name, d.name_ko AS department_name_ko,
               d.color AS department_color, a.created_at::text AS created_at,
               (SELECT COUNT(DISTINCT kc.id)::BIGINT FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
               (SELECT COALESCE(SUM(s.tokens), 0)::BIGINT FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
               (SELECT td2.id
                  FROM task_dispatches td2
                  JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                 WHERE td2.to_agent_id = a.id
                   AND kc.status = 'in_progress'
                 ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                 LIMIT 1) AS current_task,
               (SELECT s.thread_channel_id
                  FROM sessions s
                 WHERE s.agent_id = a.id
                   AND s.status IN ('turn_active', 'awaiting_bg', 'working')
                 ORDER BY s.last_heartbeat DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_thread_channel_id,
               (SELECT s.status
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_session_status,
               (SELECT s.last_tool_at
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_last_tool_at,
               (SELECT COALESCE(s.active_children, 0)
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_active_children,
               a.pipeline_config::text AS pipeline_config
          FROM agents a
          LEFT JOIN departments d ON d.id = a.department
         ORDER BY a.id";

    sqlx::query(sql)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("query agents: {error}"))
}

pub(crate) async fn load_agent_row(
    pool: &sqlx::PgPool,
    id: &str,
) -> Result<Option<sqlx::postgres::PgRow>, String> {
    let rows = sqlx::query(
        "
        SELECT a.id, a.name, a.name_ko, a.provider, a.department, a.avatar_emoji,
               a.discord_channel_id, a.discord_channel_alt, a.discord_channel_cc, a.discord_channel_cdx,
               a.status, a.xp, a.sprite_number, d.name AS department_name, d.name_ko AS department_name_ko,
               d.color AS department_color, a.created_at::text AS created_at,
               (SELECT COUNT(DISTINCT kc.id)::BIGINT FROM kanban_cards kc WHERE kc.assigned_agent_id = a.id AND kc.status = 'done') AS tasks_done,
               (SELECT COALESCE(SUM(s.tokens), 0)::BIGINT FROM sessions s WHERE s.agent_id = a.id) AS total_tokens,
               (SELECT td2.id
                  FROM task_dispatches td2
                  JOIN kanban_cards kc ON kc.latest_dispatch_id = td2.id
                 WHERE td2.to_agent_id = a.id
                   AND kc.status = 'in_progress'
                 ORDER BY td2.created_at DESC NULLS LAST, td2.id DESC
                 LIMIT 1) AS current_task,
               (SELECT s.thread_channel_id
                  FROM sessions s
                 WHERE s.agent_id = a.id
                   AND s.status IN ('turn_active', 'awaiting_bg', 'working')
                 ORDER BY s.last_heartbeat DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_thread_channel_id,
               (SELECT s.status
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_session_status,
               (SELECT s.last_tool_at
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_last_tool_at,
               (SELECT COALESCE(s.active_children, 0)
                  FROM sessions s
                 WHERE s.agent_id = a.id
                 ORDER BY CASE WHEN s.status IN ('turn_active', 'awaiting_bg', 'working') THEN 0 ELSE 1 END,
                          s.last_heartbeat DESC NULLS LAST, s.created_at DESC NULLS LAST, s.id DESC
                 LIMIT 1) AS current_active_children,
               a.pipeline_config::text AS pipeline_config
          FROM agents a
          LEFT JOIN departments d ON d.id = a.department
         WHERE a.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load agent {id}: {error}"))?;

    Ok(rows)
}
