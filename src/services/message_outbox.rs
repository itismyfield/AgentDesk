use sqlx::PgPool;

use crate::db::Db;
use crate::services::provider::{CancelToken, cancel_requested};

pub(crate) const LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS: i64 = 5 * 60;
pub(crate) const LIFECYCLE_NOTIFIER_SOURCE: &str = "lifecycle_notifier";

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

fn normalized_reason_code(reason_code: Option<&str>) -> Option<&str> {
    reason_code.map(str::trim).filter(|value| !value.is_empty())
}

fn warn_outbox_enqueue_failure(
    backend: &'static str,
    message: OutboxMessage<'_>,
    error: impl std::fmt::Display,
) {
    let reason_code = normalized_reason_code(message.reason_code);
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

    if let Some(db) = db {
        return match enqueue_lifecycle_notification_sqlite(
            db,
            target,
            session_key,
            reason_code,
            content,
        ) {
            Ok(enqueued) => enqueued,
            Err(error) => {
                warn_lifecycle_enqueue_failure("sqlite", target, session_key, reason_code, &error);
                false
            }
        };
    }

    false
}

fn enqueue_lifecycle_notification_sqlite(
    db: &Db,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    content: &str,
) -> Result<bool, String> {
    let reason_code = normalized_reason_code(Some(reason_code));
    let Some(session_key) = normalized_session_key(target, session_key) else {
        return Ok(false);
    };
    let ttl_secs = LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS.to_string();

    let conn = db
        .lock()
        .map_err(|error| format!("db lock failed: {error}"))?;
    let duplicate_id = if let Some(reason_code) = reason_code {
        conn.query_row(
            "SELECT id
             FROM message_outbox
             WHERE target = ?1
               AND reason_code = ?2
               AND session_key = ?3
               AND status != 'failed'
               AND created_at >= datetime('now', '-' || ?4 || ' seconds')
             ORDER BY id DESC
             LIMIT 1",
            [target, reason_code, session_key.as_str(), ttl_secs.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .ok()
    } else {
        conn.query_row(
            "SELECT id
             FROM message_outbox
             WHERE target = ?1
               AND reason_code IS NULL
               AND content = ?2
               AND session_key = ?3
               AND status != 'failed'
               AND created_at >= datetime('now', '-' || ?4 || ' seconds')
             ORDER BY id DESC
             LIMIT 1",
            [target, content, session_key.as_str(), ttl_secs.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .ok()
    };

    if duplicate_id.is_some() {
        return Ok(false);
    }

    if let Some(reason_code) = reason_code {
        conn.execute(
            "INSERT INTO message_outbox
             (target, content, bot, source, reason_code, session_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            [
                target,
                content,
                "notify",
                LIFECYCLE_NOTIFIER_SOURCE,
                reason_code,
                session_key.as_str(),
            ],
        )
        .map_err(|error| format!("insert lifecycle notification sqlite: {error}"))?;
    } else {
        conn.execute(
            "INSERT INTO message_outbox
             (target, content, bot, source, reason_code, session_key)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            [
                target,
                content,
                "notify",
                LIFECYCLE_NOTIFIER_SOURCE,
                session_key.as_str(),
            ],
        )
        .map_err(|error| format!("insert lifecycle notification sqlite: {error}"))?;
    }

    Ok(true)
}

async fn find_duplicate_outbox_message_pg(
    pool: &PgPool,
    target: &str,
    content: &str,
    reason_code: Option<&str>,
    session_key: Option<&str>,
    dedupe_ttl_secs: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let Some(session_key) = session_key else {
        return Ok(None);
    };

    if let Some(reason_code) = reason_code {
        return sqlx::query_scalar::<_, i64>(
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
        .bind(dedupe_ttl_secs)
        .fetch_optional(pool)
        .await;
    }

    sqlx::query_scalar::<_, i64>(
        "SELECT id
         FROM message_outbox
         WHERE target = $1
           AND reason_code IS NULL
           AND content = $2
           AND session_key = $3
           AND status != 'failed'
           AND created_at >= NOW() - ($4::BIGINT * INTERVAL '1 second')
         ORDER BY id DESC
         LIMIT 1",
    )
    .bind(target)
    .bind(content)
    .bind(session_key)
    .bind(dedupe_ttl_secs)
    .fetch_optional(pool)
    .await
}

pub(crate) async fn enqueue_outbox_pg_returning_id(
    pool: &PgPool,
    message: OutboxMessage<'_>,
) -> Result<Option<i64>, sqlx::Error> {
    enqueue_outbox_pg_returning_id_with_ttl(pool, message, LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS).await
}

pub(crate) async fn enqueue_outbox_pg_returning_id_with_cancel(
    pool: &PgPool,
    message: OutboxMessage<'_>,
    cancel_token: Option<&CancelToken>,
) -> Result<Option<i64>, sqlx::Error> {
    enqueue_outbox_pg_returning_id_with_ttl_and_cancel(
        pool,
        message,
        LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS,
        cancel_token,
    )
    .await
}

/// Variant of [`enqueue_outbox_pg_returning_id`] that lets the caller pick the
/// dedupe TTL (in seconds). Use when the default 5-minute window is too short
/// for the firing cadence (e.g. periodic GitHub sync alerts that fire every
/// 20 minutes and should not spam the channel every cycle).
pub(crate) async fn enqueue_outbox_pg_returning_id_with_ttl(
    pool: &PgPool,
    message: OutboxMessage<'_>,
    dedupe_ttl_secs: i64,
) -> Result<Option<i64>, sqlx::Error> {
    enqueue_outbox_pg_returning_id_with_ttl_and_cancel(pool, message, dedupe_ttl_secs, None).await
}

pub(crate) async fn enqueue_outbox_pg_returning_id_with_ttl_and_cancel(
    pool: &PgPool,
    message: OutboxMessage<'_>,
    dedupe_ttl_secs: i64,
    cancel_token: Option<&CancelToken>,
) -> Result<Option<i64>, sqlx::Error> {
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);

    let duplicate_id = find_duplicate_outbox_message_pg(
        pool,
        message.target,
        message.content,
        reason_code,
        session_key.as_deref(),
        dedupe_ttl_secs,
    )
    .await?;

    if let Some(existing_id) = duplicate_id {
        tracing::info!(
            target = message.target,
            reason_code,
            session_key = session_key.as_deref(),
            existing_id,
            dedupe_ttl_secs,
            "suppressed duplicate outbox message"
        );
        return Ok(None);
    }

    if cancel_requested(cancel_token) {
        tracing::info!(
            target = message.target,
            bot = message.bot,
            source = message.source,
            reason_code,
            session_key = session_key.as_deref(),
            "skipped outbox enqueue after turn cancellation"
        );
        return Ok(None);
    }

    let outbox_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .fetch_one(pool)
    .await?;

    Ok(Some(outbox_id))
}

/// Variant of [`enqueue_outbox_pg`] that lets the caller pick the dedupe TTL.
pub(crate) async fn enqueue_outbox_pg_with_ttl(
    pool: &PgPool,
    message: OutboxMessage<'_>,
    dedupe_ttl_secs: i64,
) -> Result<bool, sqlx::Error> {
    Ok(
        enqueue_outbox_pg_returning_id_with_ttl(pool, message, dedupe_ttl_secs)
            .await?
            .is_some(),
    )
}

pub(crate) async fn enqueue_outbox_pg(
    pool: &PgPool,
    message: OutboxMessage<'_>,
) -> Result<bool, sqlx::Error> {
    Ok(enqueue_outbox_pg_returning_id(pool, message)
        .await?
        .is_some())
}

// PG outbox rows are authoritative for the release runtime. Without a PG pool,
// callers should choose a visible direct-send fallback instead of staging an
// undrained legacy row.
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

    let _ = db;
    false
}

pub(crate) async fn enqueue_lifecycle_notification_pg(
    pool: &PgPool,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    content: &str,
) -> Result<bool, sqlx::Error> {
    let reason_code = normalized_reason_code(Some(reason_code));
    let session_key = normalized_session_key(target, session_key);

    let duplicate_id = find_duplicate_outbox_message_pg(
        pool,
        target,
        content,
        reason_code,
        session_key.as_deref(),
        LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS,
    )
    .await?;

    if let Some(existing_id) = duplicate_id {
        tracing::info!(
            target,
            reason_code,
            session_key = session_key.as_deref(),
            existing_id,
            "suppressed duplicate lifecycle notification"
        );
        return Ok(false);
    }

    sqlx::query(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(target)
    .bind(content)
    .bind("notify")
    .bind(LIFECYCLE_NOTIFIER_SOURCE)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .execute(pool)
    .await?;

    Ok(true)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        LIFECYCLE_NOTIFIER_SOURCE, OutboxMessage, enqueue_lifecycle_notification_pg,
        enqueue_lifecycle_notification_sqlite, enqueue_outbox_pg_returning_id,
        enqueue_outbox_pg_returning_id_with_cancel, warn_lifecycle_enqueue_failure,
        warn_outbox_enqueue_failure,
    };
    use crate::services::provider::CancelToken;
    use std::io::{self, Write};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    struct TestDatabase {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestDatabase {
        async fn create() -> Self {
            let admin_url = admin_database_url();
            let database_name =
                format!("agentdesk_message_outbox_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "message_outbox pg tests",
            )
            .await
            .expect("create postgres test db");

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "message_outbox pg tests",
            )
            .await
            .expect("migrate postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "message_outbox pg tests",
            )
            .await
            .expect("drop postgres test db");
        }
    }

    fn base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", base_database_url(), admin_db)
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

    #[test]
    fn lifecycle_sqlite_dedupes_empty_reason_by_content_and_target_session() {
        let db = crate::db::test_db();

        let first = enqueue_lifecycle_notification_sqlite(
            &db,
            "channel:sqlite-partial",
            None,
            " ",
            "same body",
        )
        .expect("enqueue first sqlite lifecycle message");
        let second = enqueue_lifecycle_notification_sqlite(
            &db,
            "channel:sqlite-partial",
            None,
            "",
            "same body",
        )
        .expect("enqueue duplicate sqlite lifecycle message");

        assert!(first);
        assert!(!second);

        let conn = db.read_conn().expect("sqlite read conn");
        let row: (i64, Option<String>, String, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(reason_code), MAX(session_key), MAX(source)
                 FROM message_outbox
                 WHERE target = 'channel:sqlite-partial'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read sqlite message_outbox rows");
        assert_eq!(row.0, 1);
        assert_eq!(row.1, None);
        assert_eq!(row.2, "channel:sqlite-partial");
        assert_eq!(row.3, LIFECYCLE_NOTIFIER_SOURCE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outbox_pg_dedupes_missing_reason_code_by_content_and_target_session() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        let first = enqueue_outbox_pg_returning_id(
            &pool,
            OutboxMessage {
                target: "channel:pg-partial",
                content: "same body",
                bot: "notify",
                source: "test",
                reason_code: None,
                session_key: None,
            },
        )
        .await
        .expect("enqueue first postgres outbox message");
        let second = enqueue_outbox_pg_returning_id(
            &pool,
            OutboxMessage {
                target: "channel:pg-partial",
                content: "same body",
                bot: "notify",
                source: "test",
                reason_code: None,
                session_key: None,
            },
        )
        .await
        .expect("enqueue duplicate postgres outbox message");

        assert!(first.is_some());
        assert_eq!(second, None);

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM message_outbox
             WHERE target = $1",
        )
        .bind("channel:pg-partial")
        .fetch_one(&pool)
        .await
        .expect("count postgres outbox rows");
        assert_eq!(count, 1);

        pool.close().await;
        test_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outbox_pg_cancelled_enqueue_returns_no_row_without_insert() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;
        let token = CancelToken::new();
        token.cancelled.store(true, Ordering::Relaxed);

        let result = enqueue_outbox_pg_returning_id_with_cancel(
            &pool,
            OutboxMessage {
                target: "channel:pg-cancelled",
                content: "cancelled body",
                bot: "notify",
                source: "test",
                reason_code: Some("test.cancelled"),
                session_key: Some("session-cancelled"),
            },
            Some(&token),
        )
        .await
        .expect("cancelled postgres outbox enqueue");

        assert_eq!(result, None);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM message_outbox
             WHERE target = $1",
        )
        .bind("channel:pg-cancelled")
        .fetch_one(&pool)
        .await
        .expect("count postgres outbox rows");
        assert_eq!(count, 0);

        pool.close().await;
        test_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_pg_dedupes_empty_reason_code_by_content_and_target_session() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        let first = enqueue_lifecycle_notification_pg(
            &pool,
            "channel:pg-lifecycle-partial",
            None,
            " ",
            "same lifecycle body",
        )
        .await
        .expect("enqueue first postgres lifecycle message");
        let second = enqueue_lifecycle_notification_pg(
            &pool,
            "channel:pg-lifecycle-partial",
            None,
            "",
            "same lifecycle body",
        )
        .await
        .expect("enqueue duplicate postgres lifecycle message");

        assert!(first);
        assert!(!second);

        let row: (i64, Option<String>, String, String) = sqlx::query_as(
            "SELECT COUNT(*), MAX(reason_code), MAX(session_key), MAX(source)
             FROM message_outbox
             WHERE target = $1",
        )
        .bind("channel:pg-lifecycle-partial")
        .fetch_one(&pool)
        .await
        .expect("read postgres lifecycle rows");
        assert_eq!(row.0, 1);
        assert_eq!(row.1, None);
        assert_eq!(row.2, "channel:pg-lifecycle-partial");
        assert_eq!(row.3, LIFECYCLE_NOTIFIER_SOURCE);

        pool.close().await;
        test_db.drop().await;
    }
}
