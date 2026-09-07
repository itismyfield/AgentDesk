//! Agent read.

use super::*;

pub(super) async fn load_agents_pg(
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
            let name = row.try_get::<String, _>("name").unwrap_or_default();
            let cli_provider = row.try_get::<Option<String>, _>("provider").ok().flatten();
            let identity = crate::services::discord::org_schema::api_agent_identity(
                &agent_id,
                cli_provider.as_deref(),
                Some(name.as_str()),
            );
            json!({
                "id": agent_id.clone(),
                "name": name,
                "name_ko": row.try_get::<Option<String>, _>("name_ko").ok().flatten(),
                "cli_provider": cli_provider,
                "identity": identity,
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
