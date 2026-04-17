use rusqlite::{Connection, OptionalExtension};

use crate::db::Db;

pub(crate) const LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS: i64 = 45;

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutboxMessage<'a> {
    pub target: &'a str,
    pub content: &'a str,
    pub bot: &'a str,
    pub source: &'a str,
    pub reason_code: Option<&'a str>,
    pub session_key: Option<&'a str>,
}

fn normalized_session_key(target: &str, session_key: Option<&str>) -> Option<String> {
    session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let target = target.trim();
            (!target.is_empty()).then(|| target.to_string())
        })
}

pub(crate) fn enqueue(conn: &Connection, message: OutboxMessage<'_>) -> rusqlite::Result<bool> {
    let reason_code = message
        .reason_code
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let session_key = normalized_session_key(message.target, message.session_key);

    if let (Some(reason_code), Some(session_key)) = (reason_code, session_key.as_deref()) {
        let lookback = format!("-{} seconds", LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS);
        let duplicate_id: Option<i64> = conn
            .query_row(
                "SELECT id
                 FROM message_outbox
                 WHERE target = ?1
                   AND reason_code = ?2
                   AND session_key = ?3
                   AND status != 'failed'
                   AND created_at >= datetime('now', ?4)
                 ORDER BY id DESC
                 LIMIT 1",
                rusqlite::params![message.target, reason_code, session_key, lookback],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing_id) = duplicate_id {
            tracing::info!(
                target = message.target,
                reason_code,
                session_key,
                existing_id,
                "suppressed duplicate lifecycle notification"
            );
            return Ok(false);
        }
    }

    conn.execute(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            message.target,
            message.content,
            message.bot,
            message.source,
            reason_code,
            session_key,
        ],
    )?;

    Ok(true)
}

pub(crate) fn enqueue_with_db(db: &Db, message: OutboxMessage<'_>) -> bool {
    match db.separate_conn() {
        Ok(conn) => enqueue(&conn, message).unwrap_or(false),
        Err(error) => {
            tracing::warn!("failed to open outbox connection: {error}");
            false
        }
    }
}

pub(crate) fn enqueue_lifecycle_notification(
    db: &Db,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    content: &str,
) -> bool {
    enqueue_with_db(
        db,
        OutboxMessage {
            target,
            content,
            bot: "notify",
            source: "system",
            reason_code: Some(reason_code),
            session_key,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{OutboxMessage, enqueue};

    fn test_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn lifecycle_notifications_dedupe_same_target_reason_and_session() {
        let conn = test_conn();
        let message = OutboxMessage {
            target: "channel:123",
            content: "🔴 세션 종료",
            bot: "notify",
            source: "system",
            reason_code: Some("lifecycle.force_kill"),
            session_key: Some("session-a"),
        };

        assert!(enqueue(&conn, message).unwrap());
        assert!(!enqueue(&conn, message).unwrap());

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn lifecycle_notifications_keep_distinct_reason_or_session() {
        let conn = test_conn();

        assert!(
            enqueue(
                &conn,
                OutboxMessage {
                    target: "channel:123",
                    content: "A",
                    bot: "notify",
                    source: "system",
                    reason_code: Some("lifecycle.force_kill"),
                    session_key: Some("session-a"),
                },
            )
            .unwrap()
        );
        assert!(
            enqueue(
                &conn,
                OutboxMessage {
                    target: "channel:123",
                    content: "B",
                    bot: "notify",
                    source: "system",
                    reason_code: Some("lifecycle.auto_cleanup"),
                    session_key: Some("session-a"),
                },
            )
            .unwrap()
        );
        assert!(
            enqueue(
                &conn,
                OutboxMessage {
                    target: "channel:123",
                    content: "C",
                    bot: "notify",
                    source: "system",
                    reason_code: Some("lifecycle.force_kill"),
                    session_key: Some("session-b"),
                },
            )
            .unwrap()
        );

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }
}
