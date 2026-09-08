//! Agent read.

use super::*;

pub(super) async fn list_agents_pg(
    pool: &sqlx::PgPool,
    office_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = crate::db::agent_read::list_agent_rows(pool, office_id).await?;

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
    let rows = crate::db::agent_read::load_agent_row(pool, id).await?;

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
