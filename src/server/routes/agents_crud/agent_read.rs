//! Agent read.

use super::*;

pub(super) async fn list_agents_pg(
    pool: &sqlx::PgPool,
    office_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let sql_with_office = "
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
          INNER JOIN office_agents oa ON oa.agent_id = a.id
          LEFT JOIN departments d ON d.id = a.department
         WHERE oa.office_id = $1
         ORDER BY a.id";
    let sql_all = "
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

    let rows = match office_id {
        Some(office_id) => {
            sqlx::query(sql_with_office)
                .bind(office_id)
                .fetch_all(pool)
                .await
        }
        None => sqlx::query(sql_all).fetch_all(pool).await,
    }
    .map_err(|error| format!("query agents: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let provider = row.try_get::<Option<String>, _>("provider").ok().flatten();
            let status = row.try_get::<Option<String>, _>("status").ok().flatten();
            let (visual_status, visual_status_emoji, visual_status_code) =
                visual_status_fields(&row, status.as_deref());
            let discord_channel_alt = row
                .try_get::<Option<String>, _>("discord_channel_alt")
                .ok()
                .flatten();
            let discord_channel_cdx = row
                .try_get::<Option<String>, _>("discord_channel_cdx")
                .ok()
                .flatten();
            let agent_id = row.try_get::<String, _>("id").unwrap_or_default();
            let name = row.try_get::<String, _>("name").unwrap_or_default();
            json!({
                "id": agent_id.clone(),
                "name": name.clone(),
                "name_ko": row.try_get::<Option<String>, _>("name_ko").ok().flatten(),
                "provider": provider.clone(),
                "cli_provider": provider.clone(),
                "identity": crate::services::discord::org_schema::api_agent_identity(
                    &agent_id,
                    provider.as_deref(),
                    Some(name.as_str()),
                ),
                "department": row.try_get::<Option<String>, _>("department").ok().flatten(),
                "department_id": row.try_get::<Option<String>, _>("department").ok().flatten(),
                "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").ok().flatten(),
                "discord_channel_id": row.try_get::<Option<String>, _>("discord_channel_id").ok().flatten(),
                "discord_channel_alt": discord_channel_alt,
                "discord_channel_cc": row.try_get::<Option<String>, _>("discord_channel_cc").ok().flatten(),
                "discord_channel_cdx": discord_channel_cdx.clone(),
                "discord_channel_id_codex": discord_channel_cdx,
                "status": status,
                "visual_status": visual_status,
                "visual_status_emoji": visual_status_emoji,
                "visual_status_code": visual_status_code,
                "xp": row.try_get::<Option<i64>, _>("xp").ok().flatten().unwrap_or(0),
                "stats_xp": row.try_get::<Option<i64>, _>("xp").ok().flatten().unwrap_or(0),
                "stats_tasks_done": row.try_get::<Option<i64>, _>("tasks_done").ok().flatten().unwrap_or(0),
                "stats_tokens": row.try_get::<Option<i64>, _>("total_tokens").ok().flatten().unwrap_or(0),
                "sprite_number": row.try_get::<Option<i64>, _>("sprite_number").ok().flatten(),
                "department_name": row.try_get::<Option<String>, _>("department_name").ok().flatten(),
                "department_name_ko": row.try_get::<Option<String>, _>("department_name_ko").ok().flatten(),
                "department_color": row.try_get::<Option<String>, _>("department_color").ok().flatten(),
                "created_at": row.try_get::<Option<String>, _>("created_at").ok().flatten(),
                "alias": serde_json::Value::Null,
                "role_id": row.try_get::<Option<String>, _>("id").ok().flatten(),
                "personality": serde_json::Value::Null,
                "current_task_id": row.try_get::<Option<String>, _>("current_task").ok().flatten(),
                "current_thread_channel_id": row.try_get::<Option<String>, _>("current_thread_channel_id").ok().flatten(),
                "pipeline_config": parse_pipeline_config_json(
                    row.try_get::<Option<String>, _>("pipeline_config").ok().flatten()
                ),
            })
        })
        .collect())
}

pub(super) async fn load_agent_pg(
    pool: &sqlx::PgPool,
    id: &str,
) -> Result<Option<serde_json::Value>, String> {
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

    let Some(row) = rows else {
        return Ok(None);
    };

    let provider = row.try_get::<Option<String>, _>("provider").ok().flatten();
    let status = row.try_get::<Option<String>, _>("status").ok().flatten();
    let (visual_status, visual_status_emoji, visual_status_code) =
        visual_status_fields(&row, status.as_deref());
    let discord_channel_alt = row
        .try_get::<Option<String>, _>("discord_channel_alt")
        .ok()
        .flatten();
    let discord_channel_cdx = row
        .try_get::<Option<String>, _>("discord_channel_cdx")
        .ok()
        .flatten();
    let fields = load_agent_management_fields(&id, provider.as_deref());

    Ok(Some(attach_management_fields(
        json!({
            "id": row.try_get::<String, _>("id").unwrap_or_default(),
            "name": row.try_get::<String, _>("name").unwrap_or_default(),
            "name_ko": row.try_get::<Option<String>, _>("name_ko").ok().flatten(),
            "provider": provider.clone(),
            "cli_provider": provider.clone(),
            "identity": crate::services::discord::org_schema::api_agent_identity(
                &id,
                provider.as_deref(),
                row.try_get::<String, _>("name").ok().as_deref(),
            ),
            "department": row.try_get::<Option<String>, _>("department").ok().flatten(),
            "department_id": row.try_get::<Option<String>, _>("department").ok().flatten(),
            "avatar_emoji": row.try_get::<Option<String>, _>("avatar_emoji").ok().flatten(),
            "discord_channel_id": row.try_get::<Option<String>, _>("discord_channel_id").ok().flatten(),
            "discord_channel_alt": discord_channel_alt,
            "discord_channel_cc": row.try_get::<Option<String>, _>("discord_channel_cc").ok().flatten(),
            "discord_channel_cdx": discord_channel_cdx.clone(),
            "discord_channel_id_codex": discord_channel_cdx,
            "status": status,
            "visual_status": visual_status,
            "visual_status_emoji": visual_status_emoji,
            "visual_status_code": visual_status_code,
            "xp": row.try_get::<Option<i64>, _>("xp").ok().flatten().unwrap_or(0),
            "stats_xp": row.try_get::<Option<i64>, _>("xp").ok().flatten().unwrap_or(0),
            "stats_tasks_done": row.try_get::<Option<i64>, _>("tasks_done").ok().flatten().unwrap_or(0),
            "stats_tokens": row.try_get::<Option<i64>, _>("total_tokens").ok().flatten().unwrap_or(0),
            "sprite_number": row.try_get::<Option<i64>, _>("sprite_number").ok().flatten(),
            "department_name": row.try_get::<Option<String>, _>("department_name").ok().flatten(),
            "department_name_ko": row.try_get::<Option<String>, _>("department_name_ko").ok().flatten(),
            "department_color": row.try_get::<Option<String>, _>("department_color").ok().flatten(),
            "created_at": row.try_get::<Option<String>, _>("created_at").ok().flatten(),
            "alias": serde_json::Value::Null,
            "role_id": row.try_get::<Option<String>, _>("id").ok().flatten(),
            "personality": serde_json::Value::Null,
            "current_task_id": row.try_get::<Option<String>, _>("current_task").ok().flatten(),
            "current_thread_channel_id": row.try_get::<Option<String>, _>("current_thread_channel_id").ok().flatten(),
            "pipeline_config": parse_pipeline_config_json(
                row.try_get::<Option<String>, _>("pipeline_config").ok().flatten()
            ),
        }),
        fields,
    )))
}
