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
    conn: &rusqlite::Connection,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Option<String> {
    session_key
        .and_then(|key| {
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

fn record_skill_usage(
    db: &Db,
    skill_id: &str,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Result<(), String> {
    let conn = db.lock().map_err(|e| format!("db lock failed: {e}"))?;
    let agent_id = resolve_skill_usage_agent_id(&conn, session_key, role_binding);
    conn.execute(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key) VALUES (?1, ?2, ?3)",
        rusqlite::params![skill_id, agent_id, session_key],
    )
    .map_err(|e| format!("insert skill_usage failed: {e}"))?;
    Ok(())
}

pub(super) fn record_skill_usage_from_tool_use(
    db: &Db,
    name: &str,
    input: &str,
    session_key: Option<&str>,
    role_binding: Option<&RoleBinding>,
) -> Result<Option<String>, String> {
    let Some(skill_id) = extract_skill_id_from_tool_use(name, input) else {
        return Ok(None);
    };
    record_skill_usage(db, &skill_id, session_key, role_binding)?;
    Ok(Some(skill_id))
}
