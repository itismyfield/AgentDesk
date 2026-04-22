use crate::db::Db;
use crate::services::discord::settings::RoleBinding;

pub(super) fn extract_skill_id_from_tool_use(name: &str, input: &str) -> Option<String> {
    if name != "Skill" {
        return None;
    }

    serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|value| {
            value
                .get("skill")
                .and_then(|skill| skill.as_str())
                .map(str::trim)
                .filter(|skill| !skill.is_empty())
                .map(ToString::to_string)
        })
}

fn resolve_skill_usage_agent_id(
    sqlite: &Db,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Option<String> {
    session_key
        .and_then(|key| {
            let conn = sqlite.read_conn().ok()?;
            conn.query_row(
                "SELECT agent_id FROM sessions WHERE session_key = ?1",
                [key],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        })
        .or_else(|| {
            role_binding
                .map(|binding| binding.role_id.trim().to_string())
                .filter(|role_id| !role_id.is_empty())
        })
}

async fn resolve_skill_usage_agent_id_pg(
    pg_pool: &sqlx::PgPool,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Option<String> {
    if let Some(key) = session_key
        && let Ok(Some(agent_id)) = sqlx::query_scalar::<_, String>(
            "SELECT agent_id
             FROM sessions
             WHERE session_key = $1
               AND agent_id IS NOT NULL
             LIMIT 1",
        )
        .bind(key)
        .fetch_optional(pg_pool)
        .await
    {
        return Some(agent_id);
    }

    role_binding
        .map(|binding| binding.role_id.trim().to_string())
        .filter(|role_id| !role_id.is_empty())
}

fn record_skill_usage_sqlite(
    sqlite: &Db,
    skill_id: &str,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Result<(), String> {
    let conn = sqlite.lock().map_err(|e| format!("db lock failed: {e}"))?;
    let agent_id = resolve_skill_usage_agent_id(sqlite, session_key, role_binding);
    match (agent_id.as_deref(), session_key) {
        (Some(agent_id), Some(session_key)) => conn
            .execute(
                "INSERT INTO skill_usage (skill_id, agent_id, session_key) VALUES (?1, ?2, ?3)",
                [skill_id, agent_id, session_key],
            )
            .map_err(|e| format!("insert skill_usage failed: {e}"))?,
        (Some(agent_id), None) => conn
            .execute(
                "INSERT INTO skill_usage (skill_id, agent_id, session_key) VALUES (?1, ?2, NULL)",
                [skill_id, agent_id],
            )
            .map_err(|e| format!("insert skill_usage failed: {e}"))?,
        (None, Some(session_key)) => conn
            .execute(
                "INSERT INTO skill_usage (skill_id, agent_id, session_key) VALUES (?1, NULL, ?2)",
                [skill_id, session_key],
            )
            .map_err(|e| format!("insert skill_usage failed: {e}"))?,
        (None, None) => conn
            .execute(
                "INSERT INTO skill_usage (skill_id, agent_id, session_key) VALUES (?1, NULL, NULL)",
                [skill_id],
            )
            .map_err(|e| format!("insert skill_usage failed: {e}"))?,
    };
    Ok(())
}

async fn record_skill_usage_pg(
    pg_pool: &sqlx::PgPool,
    skill_id: &str,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Result<(), String> {
    let agent_id = resolve_skill_usage_agent_id_pg(pg_pool, session_key, role_binding).await;
    sqlx::query(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key)
         VALUES ($1, $2, $3)",
    )
    .bind(skill_id)
    .bind(agent_id.as_deref())
    .bind(session_key)
    .execute(pg_pool)
    .await
    .map_err(|e| format!("insert skill_usage failed: {e}"))?;
    Ok(())
}

pub(super) async fn record_skill_usage_from_tool_use(
    db: Option<&Db>,
    pg_pool: Option<&sqlx::PgPool>,
    name: &str,
    input: &str,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Result<Option<String>, String> {
    let Some(skill_id) = extract_skill_id_from_tool_use(name, input) else {
        return Ok(None);
    };
    if let Some(pg_pool) = pg_pool {
        record_skill_usage_pg(pg_pool, &skill_id, session_key, role_binding).await?;
    } else if let Some(db) = db {
        record_skill_usage_sqlite(db, &skill_id, session_key, role_binding)?;
    } else {
        return Err("no runtime database handle available for skill usage".to_string());
    }
    Ok(Some(skill_id))
}
