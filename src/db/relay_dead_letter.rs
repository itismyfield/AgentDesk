//! `relay_dead_letter` table primitives — #4260 durable dead-letter sink for
//! the silent message-loss vectors (catch-up "too old" drop + intervention-queue
//! overflow evict). Preserves the lost original content so an operator, or the
//! user prompted by the aggregate notice, can recover it.
//!
//! All inserts are BEST-EFFORT: a dead-letter write must never block or fail the
//! origin path (the message was already lost — failing to record it must not
//! compound the loss). Hot-path callers use [`insert_best_effort`], which
//! swallows and logs any error.
//!
//! Outbox terminal failures (loss vector 3) are NOT recorded here: the
//! `message_outbox` row already flips to `status='failed'` and serves as its
//! own natural dead-letter (migration 0001). That vector only gains a
//! notification (see `server::note_terminal_outbox_delivery_failure`).

use sqlx::PgPool;

/// Loss-vector discriminators for the `kind` column. Constants so the producer
/// sites and tests share one spelling.
pub(crate) const KIND_CATCH_UP_TOO_OLD: &str = "catch_up_too_old";
pub(crate) const KIND_QUEUE_OVERFLOW: &str = "queue_overflow";

/// Owned payload for one dead-letter row. `content`/`reason` are required;
/// `author_id`/`message_id` are optional because a queue-overflow evict may
/// carry a merged intervention with no single resolvable source message.
#[derive(Clone, Debug)]
pub(crate) struct RelayDeadLetterRecord {
    pub kind: String,
    pub channel_id: String,
    pub author_id: Option<String>,
    pub message_id: Option<String>,
    pub content: String,
    pub reason: String,
}

/// INSERT one dead-letter row, returning its id. Prefer [`insert_best_effort`]
/// on the hot path; this variant surfaces the error for tests and callers that
/// want the id.
pub(crate) async fn insert(
    pool: &PgPool,
    record: &RelayDeadLetterRecord,
) -> Result<i64, sqlx::Error> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO relay_dead_letter
            (kind, channel_id, author_id, message_id, content, reason)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(&record.kind)
    .bind(&record.channel_id)
    .bind(record.author_id.as_deref())
    .bind(record.message_id.as_deref())
    .bind(&record.content)
    .bind(&record.reason)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Best-effort dead-letter insert: never propagates, logs a warn on failure so
/// a broken DLQ write cannot compound the original loss by breaking the origin
/// path. A `None` pool (no PG configured) is a silent no-op. Logs the channel
/// under the relay's standard `channel_id` field (#4218 drift gate).
pub(crate) async fn insert_best_effort(pool: Option<&PgPool>, record: &RelayDeadLetterRecord) {
    let Some(pool) = pool else {
        return;
    };
    if let Err(error) = insert(pool, record).await {
        tracing::warn!(
            kind = %record.kind,
            channel_id = %record.channel_id,
            "[dlq] failed to record relay dead-letter (best-effort): {error}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_and_read_back_roundtrip_pg() {
        let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_relay_dead_letter",
            "relay dead letter roundtrip",
        )
        .await;
        let pool = pg_db.connect_and_migrate().await;

        // Vector 1: catch-up too-old drop with a full author + message id.
        let too_old = RelayDeadLetterRecord {
            kind: KIND_CATCH_UP_TOO_OLD.to_string(),
            channel_id: "123".to_string(),
            author_id: Some("456".to_string()),
            message_id: Some("789".to_string()),
            content: "lost message body".to_string(),
            reason: "age_secs=420 > max_age_secs=300".to_string(),
        };
        let id = insert(&pool, &too_old).await.expect("insert too-old row");
        assert!(id > 0);

        let row = sqlx::query(
            "SELECT kind, channel_id, author_id, message_id, content, reason
               FROM relay_dead_letter WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("read too-old row");
        assert_eq!(
            row.try_get::<String, _>("kind").unwrap(),
            KIND_CATCH_UP_TOO_OLD
        );
        assert_eq!(row.try_get::<String, _>("channel_id").unwrap(), "123");
        assert_eq!(
            row.try_get::<Option<String>, _>("author_id").unwrap(),
            Some("456".to_string())
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("message_id").unwrap(),
            Some("789".to_string())
        );
        assert_eq!(
            row.try_get::<String, _>("content").unwrap(),
            "lost message body"
        );

        // Vector 2: queue-overflow evict with NULL author/message id must persist.
        let overflow = RelayDeadLetterRecord {
            kind: KIND_QUEUE_OVERFLOW.to_string(),
            channel_id: "999".to_string(),
            author_id: None,
            message_id: None,
            content: "overflowed intervention text".to_string(),
            reason: "intervention queue overflow (drop-oldest)".to_string(),
        };
        let overflow_id = insert(&pool, &overflow).await.expect("insert overflow row");
        let author: Option<String> =
            sqlx::query_scalar("SELECT author_id FROM relay_dead_letter WHERE id = $1")
                .bind(overflow_id)
                .fetch_one(&pool)
                .await
                .expect("read overflow author");
        assert_eq!(author, None);

        // Best-effort path must not panic with a live pool, and must no-op on None.
        insert_best_effort(Some(&pool), &too_old).await;
        insert_best_effort(None, &too_old).await;

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM relay_dead_letter")
            .fetch_one(&pool)
            .await
            .expect("count rows");
        assert_eq!(count, 3, "two explicit inserts + one best-effort insert");

        pool.close().await;
        pg_db.drop().await;
    }
}
