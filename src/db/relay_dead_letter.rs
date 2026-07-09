//! `relay_dead_letter` table primitives — #4260 durable dead-letter sink for
//! the silent message-loss vectors (catch-up "too old" drop + intervention-queue
//! capacity-overflow evict). Preserves the lost original content so an operator,
//! or the user prompted by the aggregate notice, can recover it.
//!
//! All recording is FIRE-AND-FORGET: a dead-letter write must never block or
//! fail the origin path (the message was already lost — failing to record it
//! must not compound the loss). Hot-path callers use [`record_detached`], which
//! runs the INSERT on a detached `tokio::spawn` so even a PG pool at its
//! acquire-timeout (3s, `db::postgres`) cannot stall the catch-up loop /
//! queue-exit feedback / outbox drain (#4260 dual-review r1, codex#1).
//!
//! Retention (#4260 dual-review r1, opus#2): the table is self-maintaining —
//! after each successful detached insert, the same spawned task best-effort
//! DELETEs rows older than [`RETENTION_DAYS`], so no separate maintenance job
//! is needed and the table cannot grow unboundedly.
//!
//! Outbox terminal failures (loss vector 3) are NOT recorded here: the
//! `message_outbox` row already flips to `status='failed'` and serves as its
//! own natural dead-letter (migration 0001). That vector only gains a
//! notification (see `server::outbox_delivery_alert`).

use sqlx::PgPool;

/// Loss-vector discriminators for the `kind` column. Constants so the producer
/// sites and tests share one spelling.
pub(crate) const KIND_CATCH_UP_TOO_OLD: &str = "catch_up_too_old";
pub(crate) const KIND_QUEUE_OVERFLOW: &str = "queue_overflow";
/// #4380: a crash restart re-adopted a still-live real-user bridge turn, but the
/// `readopted_from_inflight` relay-resume marker did not durably persist, so the
/// recovered watcher will yield to the dead bridge and silently drop the turn's
/// remaining output. Recording the undelivered body here ends the 30-minute silent
/// wedge (the recurring `.stuck-manual-*` hand-recovery) with an observable,
/// recoverable row. The root fix (watcher-yield escape hatch) resumes relay on the
/// normal path; this KIND only fires on the marker-write-failure residual.
///
/// Declared unconditionally like the sibling `KIND_*` discriminators (a DB `kind`
/// string is platform-independent), but its sole consumer
/// (`crash_resume_guard::record_readopt_relay_black_hole_dead_letter`) is
/// `#[cfg(unix)]`, so a Windows build sees it unused — allow that on non-unix rather
/// than making a schema value platform-specific (which would break a future non-unix
/// writer).
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const KIND_READOPT_RELAY_STUCK: &str = "readopt_relay_stuck";

/// Self-maintenance horizon: rows older than this are pruned opportunistically
/// after each successful insert.
pub(crate) const RETENTION_DAYS: i64 = 30;

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

/// INSERT one dead-letter row, returning its id. Prefer [`record_detached`] on
/// the hot path; this variant surfaces the error for tests and callers that
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

/// Best-effort retention sweep: DELETE rows older than [`RETENTION_DAYS`].
/// Returns the number of pruned rows.
pub(crate) async fn prune_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM relay_dead_letter
          WHERE created_at < NOW() - ($1::BIGINT * INTERVAL '1 day')",
    )
    .bind(RETENTION_DAYS)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Fire-and-forget dead-letter recording: the INSERT (and the opportunistic
/// retention sweep after a successful insert) run on a detached `tokio::spawn`,
/// so the origin path never awaits a PG pool acquire. Failures only warn-log —
/// a broken DLQ write cannot compound the original loss. A `None` pool (no PG
/// configured) is a silent no-op. Logs the channel under the relay's standard
/// `channel_id` field (#4218 drift gate). Returns the join handle so tests can
/// await completion deterministically; production callers drop it.
pub(crate) fn record_detached(
    pool: Option<&PgPool>,
    record: RelayDeadLetterRecord,
) -> Option<tokio::task::JoinHandle<()>> {
    let pool = pool.cloned()?;
    Some(tokio::spawn(async move {
        match insert(&pool, &record).await {
            Ok(_) => {
                // Self-maintenance piggybacks on write traffic: no writes ⇒ no
                // growth ⇒ nothing to prune.
                if let Err(error) = prune_expired(&pool).await {
                    tracing::warn!(
                        "[dlq] failed to prune expired relay dead-letter rows (best-effort): {error}"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    kind = %record.kind,
                    channel_id = %record.channel_id,
                    "[dlq] failed to record relay dead-letter (best-effort): {error}"
                );
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_read_back_detached_and_retention_roundtrip_pg() {
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

        // Detached path: spawns and completes; None pool is a no-spawn no-op.
        let handle =
            record_detached(Some(&pool), too_old.clone()).expect("live pool must spawn a task");
        handle.await.expect("detached DLQ task must not panic");
        assert!(
            record_detached(None, too_old.clone()).is_none(),
            "no pool ⇒ no spawned task"
        );

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM relay_dead_letter")
            .fetch_one(&pool)
            .await
            .expect("count rows");
        assert_eq!(count, 3, "two explicit inserts + one detached insert");

        // Retention (#4260 dual r1 opus#2): age one row past the horizon; the
        // next detached record must sweep it within the same spawned task.
        sqlx::query(
            "UPDATE relay_dead_letter
                SET created_at = NOW() - INTERVAL '31 days'
              WHERE id = $1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .expect("age a row past retention");
        let handle = record_detached(Some(&pool), overflow.clone()).expect("spawn retention pass");
        handle
            .await
            .expect("detached retention task must not panic");
        let expired_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM relay_dead_letter WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("count expired row");
        assert_eq!(expired_left, 0, "31-day-old row must be pruned");
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM relay_dead_letter")
            .fetch_one(&pool)
            .await
            .expect("count remaining rows");
        assert_eq!(remaining, 3, "fresh rows survive the sweep (+1 new insert)");

        pool.close().await;
        pg_db.drop().await;
    }
}
