use libsql_rusqlite::{Connection, OptionalExtension};
use sqlx::PgPool;

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

fn warn_outbox_enqueue_failure(
    backend: &'static str,
    message: OutboxMessage<'_>,
    error: impl std::fmt::Display,
) {
    let reason_code = message
        .reason_code
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let session_key = normalized_session_key(message.target, message.session_key);
    tracing::warn!(
        backend,
        target = message.target,
        bot = message.bot,
        source = message.source,
        reason_code,
        session_key = session_key.as_deref(),
        "failed to enqueue outbox message: {error}"
    );
}

fn warn_lifecycle_enqueue_failure(
    backend: &'static str,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    error: impl std::fmt::Display,
) {
    let session_key = normalized_session_key(target, session_key);
    tracing::warn!(
        backend,
        target,
        reason_code,
        session_key = session_key.as_deref(),
        "failed to enqueue lifecycle notification: {error}"
    );
}

pub(crate) fn enqueue(
    conn: &Connection,
    message: OutboxMessage<'_>,
) -> libsql_rusqlite::Result<bool> {
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
                libsql_rusqlite::params![message.target, reason_code, session_key, lookback],
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
        libsql_rusqlite::params![
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

pub(crate) fn enqueue_lifecycle_notification_best_effort(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    content: &str,
) -> bool {
    // PG outbox rows are authoritative whenever a pool is configured. The
    // release worker drains PG only in that mode, so writing a "fallback"
    // lifecycle row to SQLite would create an undeliverable ghost message.
    if let Some(pool) = pg_pool {
        let target_owned = target.to_string();
        let session_key_owned = session_key.map(str::to_string);
        let reason_code_owned = reason_code.to_string();
        let content_owned = content.to_string();
        match crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                enqueue_lifecycle_notification_pg(
                    &pool,
                    &target_owned,
                    session_key_owned.as_deref(),
                    &reason_code_owned,
                    &content_owned,
                )
                .await
                .map_err(|error| format!("enqueue lifecycle notification via postgres: {error}"))
            },
            |message| message,
        ) {
            Ok(enqueued) => return enqueued,
            Err(error) => {
                warn_lifecycle_enqueue_failure(
                    "postgres",
                    target,
                    session_key,
                    reason_code,
                    &error,
                );
                return false;
            }
        }
    }

    db.is_some_and(|db| {
        enqueue_lifecycle_notification(db, target, session_key, reason_code, content)
    })
}

pub(crate) async fn enqueue_outbox_pg(
    pool: &PgPool,
    message: OutboxMessage<'_>,
) -> Result<bool, sqlx::Error> {
    let reason_code = message
        .reason_code
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let session_key = normalized_session_key(message.target, message.session_key);

    if let (Some(reason_code), Some(session_key)) = (reason_code, session_key.as_deref()) {
        let duplicate_id = sqlx::query_scalar::<_, i64>(
            "SELECT id
             FROM message_outbox
             WHERE target = $1
               AND reason_code = $2
               AND session_key = $3
               AND status != 'failed'
               AND created_at >= NOW() - ($4::BIGINT * INTERVAL '1 second')
             ORDER BY id DESC
             LIMIT 1",
        )
        .bind(message.target)
        .bind(reason_code)
        .bind(session_key)
        .bind(LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS)
        .fetch_optional(pool)
        .await?;

        if let Some(existing_id) = duplicate_id {
            tracing::info!(
                target = message.target,
                reason_code,
                session_key,
                existing_id,
                "suppressed duplicate outbox message"
            );
            return Ok(false);
        }
    }

    sqlx::query(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .execute(pool)
    .await?;

    Ok(true)
}

// When PG is configured, its outbox is authoritative; falling back to SQLite
// would write rows no release worker is polling. In that mode, insert failures
// must surface to the caller so it can choose a visible fallback.
pub(crate) async fn enqueue_outbox_best_effort(
    pg_pool: Option<&PgPool>,
    db: Option<&Db>,
    message: OutboxMessage<'_>,
) -> bool {
    if let Some(pool) = pg_pool {
        return match enqueue_outbox_pg(pool, message).await {
            Ok(enqueued) => enqueued,
            Err(error) => {
                warn_outbox_enqueue_failure("postgres", message, &error);
                false
            }
        };
    }

    let Some(db) = db else {
        return false;
    };
    match db.separate_conn() {
        Ok(conn) => match enqueue(&conn, message) {
            Ok(enqueued) => enqueued,
            Err(error) => {
                warn_outbox_enqueue_failure("sqlite", message, &error);
                false
            }
        },
        Err(error) => {
            warn_outbox_enqueue_failure("sqlite", message, format!("open connection: {error}"));
            false
        }
    }
}

pub(crate) async fn enqueue_lifecycle_notification_pg(
    pool: &PgPool,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    content: &str,
) -> Result<bool, sqlx::Error> {
    let reason_code = reason_code.trim();
    let session_key = normalized_session_key(target, session_key);

    if let Some(session_key) = session_key.as_deref() {
        let duplicate_id = sqlx::query_scalar::<_, i64>(
            "SELECT id
             FROM message_outbox
             WHERE target = $1
               AND reason_code = $2
               AND session_key = $3
               AND status != 'failed'
               AND created_at >= NOW() - ($4::BIGINT * INTERVAL '1 second')
             ORDER BY id DESC
             LIMIT 1",
        )
        .bind(target)
        .bind(reason_code)
        .bind(session_key)
        .bind(LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS)
        .fetch_optional(pool)
        .await?;

        if let Some(existing_id) = duplicate_id {
            tracing::info!(
                target,
                reason_code,
                session_key,
                existing_id,
                "suppressed duplicate lifecycle notification"
            );
            return Ok(false);
        }
    }

    sqlx::query(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(target)
    .bind(content)
    .bind("notify")
    .bind("system")
    .bind(reason_code)
    .bind(session_key.as_deref())
    .execute(pool)
    .await?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{
        OutboxMessage, enqueue, warn_lifecycle_enqueue_failure, warn_outbox_enqueue_failure,
    };
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    fn test_conn() -> libsql_rusqlite::Connection {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        conn
    }

    #[derive(Clone)]
    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let log_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(move || TestLogWriter {
                buffer: log_buffer.clone(),
            })
            .finish();

        let result = tracing::subscriber::with_default(subscriber, run);
        let captured = buffer.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&captured).to_string())
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

    #[test]
    fn warn_outbox_enqueue_failure_logs_reason_code_and_session_key() {
        let (_, logs) = capture_logs(|| {
            warn_outbox_enqueue_failure(
                "postgres",
                OutboxMessage {
                    target: "channel:123",
                    content: "msg",
                    bot: "notify",
                    source: "system",
                    reason_code: Some("lifecycle.force_kill"),
                    session_key: Some("session-a"),
                },
                "boom",
            )
        });

        assert!(logs.contains("failed to enqueue outbox message"));
        assert!(logs.contains("postgres"));
        assert!(logs.contains("reason_code=\"lifecycle.force_kill\""));
        assert!(logs.contains("session_key=\"session-a\""));
    }

    #[test]
    fn warn_lifecycle_enqueue_failure_logs_normalized_session_key() {
        let (_, logs) = capture_logs(|| {
            warn_lifecycle_enqueue_failure(
                "postgres",
                "channel:123",
                None,
                "lifecycle.force_kill",
                "boom",
            )
        });

        assert!(logs.contains("failed to enqueue lifecycle notification"));
        assert!(logs.contains("postgres"));
        assert!(logs.contains("session_key=\"channel:123\""));
    }
}
