use sqlx::{PgPool, Row};

use crate::db::session_status::{ABORTED, AWAITING_BG, DISCONNECTED, IDLE, TURN_ACTIVE};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundChildSpawn {
    pub parent_session_key: String,
    pub provider: Option<String>,
    pub tool_name: String,
    pub tool_input: String,
    /// Launching tool call id; keys the child row so a later turn's completion
    /// notification can close exactly this child.
    pub tool_use_id: Option<String>,
}

pub async fn mark_session_tool_use_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<bool, sqlx::Error> {
    let session_key = session_key.trim();
    if session_key.is_empty() {
        return Ok(false);
    }

    let result = sqlx::query(
        "UPDATE sessions
            SET last_tool_at = NOW(),
                status = $2
          WHERE session_key = $1
            AND status NOT IN ($3, $4)",
    )
    .bind(session_key)
    .bind(TURN_ACTIVE)
    .bind(DISCONNECTED)
    .bind(ABORTED)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn insert_background_child_pg(
    pool: &PgPool,
    spawn: &BackgroundChildSpawn,
) -> Result<Option<i64>, sqlx::Error> {
    let parent_session_key = spawn.parent_session_key.trim();
    if parent_session_key.is_empty() {
        return Ok(None);
    }

    let mut tx = pool.begin().await?;
    let parent = sqlx::query(
        "SELECT id, agent_id, cwd, thread_channel_id
           FROM sessions
          WHERE session_key = $1
          FOR UPDATE",
    )
    .bind(parent_session_key)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(parent) = parent else {
        tx.commit().await?;
        return Ok(None);
    };

    let parent_id: i64 = parent.get("id");
    let agent_id: Option<String> = parent.try_get("agent_id").ok();
    let cwd: Option<String> = parent.try_get("cwd").ok();
    let thread_channel_id: Option<String> = parent.try_get("thread_channel_id").ok();
    let child_session_key = match spawn
        .tool_use_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(tool_use_id) => background_child_session_key(parent_session_key, tool_use_id),
        None => background_child_session_key(
            parent_session_key,
            &uuid::Uuid::new_v4().simple().to_string(),
        ),
    };
    let purpose = background_child_purpose(&spawn.tool_name, &spawn.tool_input);
    let provider = spawn
        .provider
        .as_deref()
        .filter(|value| !value.trim().is_empty());

    // A replayed ToolUse finds its row already present; it must not count twice.
    let child_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO sessions (
            session_key,
            agent_id,
            provider,
            status,
            cwd,
            thread_channel_id,
            parent_session_id,
            spawned_at,
            purpose,
            created_at
         ) VALUES ($1, $2, COALESCE($3, 'claude'), $8, $4, $5, $6, NOW(), $7, NOW())
         ON CONFLICT (session_key) DO NOTHING
         RETURNING id",
    )
    .bind(child_session_key)
    .bind(agent_id)
    .bind(provider)
    .bind(cwd)
    .bind(thread_channel_id)
    .bind(parent_id)
    .bind(purpose)
    .bind(TURN_ACTIVE)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(child_id) = child_id else {
        tx.commit().await?;
        return Ok(None);
    };

    sqlx::query("UPDATE sessions SET active_children = active_children + 1 WHERE id = $1")
        .bind(parent_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(Some(child_id))
}

pub async fn close_background_child_pg(
    pool: &PgPool,
    child_session_id: i64,
    status: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT parent_session_id, closed_at
           FROM sessions
          WHERE id = $1
          FOR UPDATE",
    )
    .bind(child_session_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = row else {
        tx.commit().await?;
        return Ok(false);
    };

    let parent_session_id: Option<i64> = row.try_get("parent_session_id").ok();
    let closed_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("closed_at").ok();
    if closed_at.is_some() {
        tx.commit().await?;
        return Ok(false);
    }

    let status = normalized_close_status(status);
    sqlx::query("UPDATE sessions SET closed_at = NOW(), status = $2 WHERE id = $1")
        .bind(child_session_id)
        .bind(status)
        .execute(&mut *tx)
        .await?;

    // A parent whose turn ended waiting only on background work is idle once
    // its last open child closes.
    if let Some(parent_session_id) = parent_session_id {
        // Lock first so the open-child check runs on a snapshot taken after any
        // sibling close that held the parent commits.
        sqlx::query("SELECT 1 FROM sessions WHERE id = $1 FOR NO KEY UPDATE")
            .bind(parent_session_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE sessions
                SET active_children = GREATEST(active_children - 1, 0),
                    status = CASE
                        WHEN GREATEST(active_children - 1, 0) = 0
                         AND status = $2
                         AND NOT EXISTS (
                             SELECT 1 FROM sessions child
                              WHERE child.parent_session_id = sessions.id
                                AND child.closed_at IS NULL
                         )
                        THEN $3
                        ELSE status
                    END
              WHERE id = $1",
        )
        .bind(parent_session_id)
        .bind(AWAITING_BG)
        .bind(IDLE)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(true)
}

fn background_child_session_key(parent_session_key: &str, child_id: &str) -> String {
    format!("{parent_session_key}:child:{child_id}")
}

/// Whether the parent still has an open background child of any key format.
pub async fn has_open_background_children_pg(
    pool: &PgPool,
    parent_session_key: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM sessions parent
               JOIN sessions child ON child.parent_session_id = parent.id
              WHERE parent.session_key = $1
                AND child.closed_at IS NULL
         )",
    )
    .bind(parent_session_key)
    .fetch_one(pool)
    .await
}

/// Closes `parent_session_key`'s open child launched by `tool_use_id`, whichever
/// turn observes its completion. Without both identities nothing closes.
pub async fn close_background_child_for_tool_use_pg(
    pool: &PgPool,
    parent_session_key: &str,
    tool_use_id: &str,
    status: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let parent_session_key = parent_session_key.trim();
    let tool_use_id = tool_use_id.trim();
    if parent_session_key.is_empty() || tool_use_id.is_empty() {
        return Ok(None);
    }
    let child_session_id: Option<i64> = sqlx::query_scalar(
        "SELECT child.id
           FROM sessions parent
           JOIN sessions child ON child.parent_session_id = parent.id
          WHERE parent.session_key = $1
            AND child.session_key = $1 || ':child:' || $2
            AND child.closed_at IS NULL",
    )
    .bind(parent_session_key)
    .bind(tool_use_id)
    .fetch_optional(pool)
    .await?;
    let Some(child_session_id) = child_session_id else {
        return Ok(None);
    };
    Ok(close_background_child_pg(pool, child_session_id, status)
        .await?
        .then_some(child_session_id))
}

pub fn background_child_purpose(tool_name: &str, tool_input: &str) -> String {
    let tool_name = tool_name.trim();
    let tool_label = if tool_name.is_empty() {
        "Tool"
    } else {
        tool_name
    };
    let input = serde_json::from_str::<serde_json::Value>(tool_input).ok();
    let detail = match tool_label.to_ascii_lowercase().as_str() {
        "bash" => input
            .as_ref()
            .and_then(|value| value.get("command"))
            .and_then(|value| value.as_str())
            .unwrap_or(tool_input),
        "agent" | "task" => input
            .as_ref()
            .and_then(|value| value.get("description"))
            .and_then(|value| value.as_str())
            .unwrap_or(tool_input),
        _ => tool_input,
    };
    let detail = truncate_utf8_bytes(detail.trim(), 80);
    if detail.is_empty() {
        tool_label.to_string()
    } else {
        format!("{tool_label}: {detail}")
    }
}

fn normalized_close_status(status: &str) -> &'static str {
    match status.trim().to_ascii_lowercase().as_str() {
        "aborted" | "abort" | "cancelled" | "canceled" | "failed" | "error" => ABORTED,
        _ => IDLE,
    }
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
#[path = "session_observability/tests_pg.rs"]
mod tests_pg;
