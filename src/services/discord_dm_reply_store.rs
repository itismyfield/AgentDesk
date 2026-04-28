use crate::db::Db;
use sqlx::PgPool;
use sqlx::Row;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingDmReplyRecord {
    pub(crate) id: i64,
    pub(crate) source_agent: String,
    pub(crate) context_json: String,
    pub(crate) channel_id: Option<String>,
}

fn normalize_register_args(
    source_agent: &str,
    user_id: &str,
    channel_id: Option<&str>,
) -> Result<(String, String, Option<String>), String> {
    let source_agent = source_agent.trim();
    let user_id = user_id.trim();
    if source_agent.is_empty() || user_id.is_empty() {
        return Err("source_agent and user_id are required".to_string());
    }

    let channel_id = channel_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok((source_agent.to_string(), user_id.to_string(), channel_id))
}

pub(crate) async fn register_pending_dm_reply_db(
    pg_pool: Option<&PgPool>,
    source_agent: &str,
    user_id: &str,
    channel_id: Option<&str>,
    context_json: &str,
    ttl_seconds: i64,
) -> Result<i64, String> {
    let (source_agent, user_id, channel_id) =
        normalize_register_args(source_agent, user_id, channel_id)?;

    let Some(pool) = pg_pool else {
        return Err("postgres pool unavailable for dm_reply.register".to_string());
    };

    let id = if ttl_seconds > 0 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO pending_dm_replies (
                source_agent, user_id, channel_id, context, expires_at
             )
             VALUES ($1, $2, $3, CAST($4 AS jsonb), NOW() + ($5 * INTERVAL '1 second'))
             RETURNING id",
        )
        .bind(source_agent)
        .bind(user_id)
        .bind(channel_id)
        .bind(context_json)
        .bind(ttl_seconds)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("insert failed: {error}"))?
    } else {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO pending_dm_replies (
                source_agent, user_id, channel_id, context, expires_at
             )
             VALUES ($1, $2, $3, CAST($4 AS jsonb), NULL)
             RETURNING id",
        )
        .bind(source_agent)
        .bind(user_id)
        .bind(channel_id)
        .bind(context_json)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("insert failed: {error}"))?
    };

    Ok(id)
}

pub(crate) async fn load_oldest_pending_dm_reply_db(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    user_id: &str,
) -> Result<Option<PendingDmReplyRecord>, String> {
    // Mixed mode reads from PG first because pending reply ownership has
    // already moved there for runtime DM routing.
    if let Some(pool) = pg_pool {
        let row = sqlx::query(
            "SELECT id, source_agent, context::text AS context_json, channel_id
             FROM pending_dm_replies
             WHERE user_id = $1
               AND status = 'pending'
               AND (expires_at IS NULL OR expires_at > NOW())
             ORDER BY created_at ASC
             LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("query failed: {error}"))?;
        return Ok(row.map(|row| PendingDmReplyRecord {
            id: row.get("id"),
            source_agent: row.get("source_agent"),
            context_json: row.get("context_json"),
            channel_id: row.get("channel_id"),
        }));
    }

    let conn = db
        .ok_or("sqlite db unavailable for pending_dm_replies lookup")?
        .separate_conn()
        .map_err(|error| format!("db connection: {error}"))?;
    match conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'pending' \
         AND (expires_at IS NULL OR expires_at > datetime('now')) \
         ORDER BY created_at ASC LIMIT 1",
        libsql_rusqlite::params![user_id],
        |row| {
            Ok(PendingDmReplyRecord {
                id: row.get(0)?,
                source_agent: row.get(1)?,
                context_json: row.get(2)?,
                channel_id: row.get(3)?,
            })
        },
    ) {
        Ok(record) => Ok(Some(record)),
        Err(libsql_rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(format!("query failed: {error}")),
    }
}

pub(crate) async fn load_most_recent_consumed_dm_reply_db(
    sqlite: &Db,
    pg_pool: Option<&PgPool>,
    user_id: &str,
) -> Result<Option<PendingDmReplyRecord>, String> {
    if let Some(pool) = pg_pool {
        let row = sqlx::query(
            "SELECT id, source_agent, context::text AS context_json, channel_id
             FROM pending_dm_replies
             WHERE user_id = $1
               AND status = 'consumed'
             ORDER BY consumed_at DESC NULLS LAST, id DESC
             LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("query failed: {error}"))?;
        return Ok(row.map(|row| PendingDmReplyRecord {
            id: row.get("id"),
            source_agent: row.get("source_agent"),
            context_json: row.get("context_json"),
            channel_id: row.get("channel_id"),
        }));
    }

    let conn = sqlite
        .separate_conn()
        .map_err(|error| format!("db connection: {error}"))?;
    match conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'consumed' \
         ORDER BY consumed_at DESC, id DESC LIMIT 1",
        libsql_rusqlite::params![user_id],
        |row| {
            Ok(PendingDmReplyRecord {
                id: row.get(0)?,
                source_agent: row.get(1)?,
                context_json: row.get(2)?,
                channel_id: row.get(3)?,
            })
        },
    ) {
        Ok(record) => Ok(Some(record)),
        Err(libsql_rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(format!("query failed: {error}")),
    }
}

pub(crate) async fn mark_pending_dm_reply_consumed_db(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    reply_id: i64,
    updated_context_json: &str,
) -> Result<bool, String> {
    // Consumption updates follow the same authority split as reads/writes:
    // PG when configured, SQLite only for legacy no-PG runtime/tests.
    if let Some(pool) = pg_pool {
        let updated = sqlx::query(
            "UPDATE pending_dm_replies
             SET status = 'consumed',
                 consumed_at = NOW(),
                 context = CAST($1 AS jsonb)
             WHERE id = $2
               AND status = 'pending'",
        )
        .bind(updated_context_json)
        .bind(reply_id)
        .execute(pool)
        .await
        .map_err(|error| format!("update failed: {error}"))?;
        return Ok(updated.rows_affected() > 0);
    }

    let conn = db
        .ok_or("sqlite db unavailable for pending_dm_replies update")?
        .separate_conn()
        .map_err(|error| format!("db connection: {error}"))?;
    let updated = conn
        .execute(
            "UPDATE pending_dm_replies SET status = 'consumed', consumed_at = datetime('now'), \
             context = ?1 WHERE id = ?2 AND status = 'pending'",
            libsql_rusqlite::params![updated_context_json, reply_id],
        )
        .map_err(|error| format!("update failed: {error}"))?;
    Ok(updated > 0)
}

pub(crate) async fn load_failed_consumed_dm_replies_db(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
) -> Result<Vec<PendingDmReplyRecord>, String> {
    if let Some(pool) = pg_pool {
        let rows = sqlx::query(
            "SELECT id, source_agent, context::text AS context_json, channel_id
             FROM pending_dm_replies
             WHERE status = 'consumed'
               AND context ? '_notify_failed'
             ORDER BY consumed_at ASC NULLS LAST, id ASC
             LIMIT 10",
        )
        .fetch_all(pool)
        .await
        .map_err(|error| format!("query failed: {error}"))?;
        return Ok(rows
            .into_iter()
            .map(|row| PendingDmReplyRecord {
                id: row.get("id"),
                source_agent: row.get("source_agent"),
                context_json: row.get("context_json"),
                channel_id: row.get("channel_id"),
            })
            .collect());
    }

    let conn = db
        .ok_or("sqlite db unavailable for pending_dm_replies lookup")?
        .separate_conn()
        .map_err(|error| format!("db connection: {error}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
             WHERE status = 'consumed' AND json_extract(context, '$._notify_failed') IS NOT NULL \
             LIMIT 10",
        )
        .map_err(|error| format!("query failed: {error}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(PendingDmReplyRecord {
                id: row.get(0)?,
                source_agent: row.get(1)?,
                context_json: row.get(2)?,
                channel_id: row.get(3)?,
            })
        })
        .map_err(|error| format!("query failed: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("query failed: {error}"))?;
    Ok(rows)
}

pub(crate) async fn mark_pending_dm_reply_notify_failed_db(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    reply_id: i64,
    error_text: &str,
) -> Result<(), String> {
    if let Some(pool) = pg_pool {
        sqlx::query(
            "UPDATE pending_dm_replies
             SET context = jsonb_set(
                 jsonb_set(COALESCE(context, '{}'::jsonb), '{_notify_failed}', 'true'::jsonb, true),
                 '{_notify_error}',
                 to_jsonb(CAST($1 AS text)),
                 true
             )
             WHERE id = $2",
        )
        .bind(error_text)
        .bind(reply_id)
        .execute(pool)
        .await
        .map_err(|error| format!("mark notify failed: {error}"))?;
        return Ok(());
    }

    let conn = db
        .ok_or("sqlite db unavailable for pending_dm_replies update")?
        .separate_conn()
        .map_err(|error| format!("db connection: {error}"))?;
    conn.execute(
        "UPDATE pending_dm_replies SET context = \
         json_set(context, '$._notify_failed', json('true'), '$._notify_error', ?1) \
         WHERE id = ?2",
        libsql_rusqlite::params![error_text, reply_id],
    )
    .map_err(|error| format!("mark notify failed: {error}"))?;
    Ok(())
}

pub(crate) async fn clear_pending_dm_reply_notify_failure_db(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    reply_id: i64,
) -> Result<(), String> {
    if let Some(pool) = pg_pool {
        sqlx::query(
            "UPDATE pending_dm_replies
             SET context = COALESCE(context, '{}'::jsonb) - '_notify_failed' - '_notify_error'
             WHERE id = $1",
        )
        .bind(reply_id)
        .execute(pool)
        .await
        .map_err(|error| format!("clear notify failure: {error}"))?;
        return Ok(());
    }

    let conn = db
        .ok_or("sqlite db unavailable for pending_dm_replies update")?
        .separate_conn()
        .map_err(|error| format!("db connection: {error}"))?;
    conn.execute(
        "UPDATE pending_dm_replies SET context = \
         json_remove(context, '$._notify_failed', '$._notify_error') \
         WHERE id = ?1",
        libsql_rusqlite::params![reply_id],
    )
    .map_err(|error| format!("clear notify failure: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }
}
