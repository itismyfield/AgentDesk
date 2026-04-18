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

pub(crate) fn register_pending_dm_reply(
    db: &Db,
    source_agent: &str,
    user_id: &str,
    channel_id: Option<&str>,
    context_json: &str,
    ttl_seconds: i64,
) -> Result<i64, String> {
    let (source_agent, user_id, channel_id) =
        normalize_register_args(source_agent, user_id, channel_id)?;

    let conn = db
        .separate_conn()
        .map_err(|e| format!("db connection: {e}"))?;
    let expires_at = if ttl_seconds > 0 {
        format!("datetime('now', '+{ttl_seconds} seconds')")
    } else {
        "NULL".to_string()
    };
    let sql = format!(
        "INSERT INTO pending_dm_replies (source_agent, user_id, channel_id, context, expires_at) \
         VALUES (?1, ?2, ?3, ?4, {expires_at})"
    );
    conn.execute(
        &sql,
        libsql_rusqlite::params![source_agent, user_id, channel_id, context_json],
    )
    .map_err(|e| format!("insert failed: {e}"))?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn delete_pending_dm_reply(db: &Db, reply_id: i64) -> Result<(), String> {
    let conn = db
        .separate_conn()
        .map_err(|e| format!("db connection: {e}"))?;
    conn.execute(
        "DELETE FROM pending_dm_replies WHERE id = ?1",
        libsql_rusqlite::params![reply_id],
    )
    .map_err(|e| format!("delete failed: {e}"))?;
    Ok(())
}

pub(crate) async fn register_pending_dm_reply_db(
    db: &Db,
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
        return register_pending_dm_reply(
            db,
            &source_agent,
            &user_id,
            channel_id.as_deref(),
            context_json,
            ttl_seconds,
        );
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

pub(crate) async fn delete_pending_dm_reply_db(
    db: &Db,
    pg_pool: Option<&PgPool>,
    reply_id: i64,
) -> Result<(), String> {
    let Some(pool) = pg_pool else {
        return delete_pending_dm_reply(db, reply_id);
    };

    sqlx::query("DELETE FROM pending_dm_replies WHERE id = $1")
        .bind(reply_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete failed: {error}"))?;
    Ok(())
}

pub(crate) async fn load_oldest_pending_dm_reply_db(
    db: &Db,
    pg_pool: Option<&PgPool>,
    user_id: &str,
) -> Result<Option<PendingDmReplyRecord>, String> {
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
    db: &Db,
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

    let conn = db
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
    db: &Db,
    pg_pool: Option<&PgPool>,
    reply_id: i64,
    updated_context_json: &str,
) -> Result<bool, String> {
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
    db: &Db,
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
    db: &Db,
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
    db: &Db,
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

    #[test]
    fn register_pending_dm_reply_inserts_expected_row() {
        let db = test_db();
        let reply_id = register_pending_dm_reply(
            &db,
            "family-counsel",
            "12345",
            Some("1473922824350601297"),
            r#"{"topicKey":"obujang.health_checkup","question":"건강검진 요즘 했어?"}"#,
            86_400,
        )
        .expect("insert should succeed");

        let conn = db.separate_conn().unwrap();
        let row: (String, String, Option<String>, String) = conn
            .query_row(
                "SELECT source_agent, user_id, channel_id, context FROM pending_dm_replies WHERE id = ?1",
                libsql_rusqlite::params![reply_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row.0, "family-counsel");
        assert_eq!(row.1, "12345");
        assert_eq!(row.2.as_deref(), Some("1473922824350601297"));
        assert!(row.3.contains("health_checkup"));
    }

    #[test]
    fn delete_pending_dm_reply_removes_row() {
        let db = test_db();
        let reply_id = register_pending_dm_reply(&db, "family-counsel", "12345", None, "{}", 3_600)
            .expect("insert should succeed");

        delete_pending_dm_reply(&db, reply_id).expect("delete should succeed");

        let conn = db.separate_conn().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_dm_replies WHERE id = ?1",
                libsql_rusqlite::params![reply_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}
