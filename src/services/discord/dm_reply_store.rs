use crate::db::Db;

pub(crate) fn register_pending_dm_reply(
    db: &Db,
    source_agent: &str,
    user_id: &str,
    channel_id: Option<&str>,
    context_json: &str,
    ttl_seconds: i64,
) -> Result<i64, String> {
    let source_agent = source_agent.trim();
    let user_id = user_id.trim();
    if source_agent.is_empty() || user_id.is_empty() {
        return Err("source_agent and user_id are required".to_string());
    }

    let conn = db
        .separate_conn()
        .map_err(|e| format!("db connection: {e}"))?;
    let expires_at = if ttl_seconds > 0 {
        format!("datetime('now', '+{ttl_seconds} seconds')")
    } else {
        "NULL".to_string()
    };
    let channel_id = channel_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let sql = format!(
        "INSERT INTO pending_dm_replies (source_agent, user_id, channel_id, context, expires_at) \
         VALUES (?1, ?2, ?3, ?4, {expires_at})"
    );
    conn.execute(
        &sql,
        rusqlite::params![source_agent, user_id, channel_id, context_json],
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
        rusqlite::params![reply_id],
    )
    .map_err(|e| format!("delete failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
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
                rusqlite::params![reply_id],
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
                rusqlite::params![reply_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}
