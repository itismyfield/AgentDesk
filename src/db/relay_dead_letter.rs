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
/// Terminal frame body that ended with no delivery owner (sink did not deliver,
/// soft-terminal authority denied). Sole writer is `#[cfg(unix)]`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const KIND_TERMINAL_NO_DELIVERY_OWNER: &str = "terminal_no_delivery_owner";

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
    record_detached_reporting(pool, record, |_| {})
}

/// [`record_detached`] that also reports whether the row landed: `on_recorded(false)`
/// when there is no pool or the INSERT fails, `on_recorded(true)` after a successful write.
pub(crate) fn record_detached_reporting(
    pool: Option<&PgPool>,
    record: RelayDeadLetterRecord,
    on_recorded: impl FnOnce(bool) + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some(pool) = pool.cloned() else {
        on_recorded(false);
        return None;
    };
    Some(tokio::spawn(async move {
        match insert(&pool, &record).await {
            Ok(_) => {
                on_recorded(true);
                // Self-maintenance piggybacks on write traffic: no writes ⇒ no
                // growth ⇒ nothing to prune.
                if let Err(error) = prune_expired(&pool).await {
                    tracing::warn!(
                        "[dlq] failed to prune expired relay dead-letter rows (best-effort): {error}"
                    );
                }
            }
            Err(error) => {
                on_recorded(false);
                tracing::warn!(
                    kind = %record.kind,
                    channel_id = %record.channel_id,
                    "[dlq] failed to record relay dead-letter (best-effort): {error}"
                );
            }
        }
    }))
}

/// Settled `redelivery_state` values (migration 0120). A row starts `'pending'`;
/// [`claim_pending_redeliveries`] moves it to `'claimed'` and only a settle leaves
/// that state. Settling back to `'pending'` makes the row claimable again, so a
/// claim is once per attempt, not once per row. A claim lost to a crash stays
/// `'claimed'` until an operator recovers it.
///
/// The body was posted to the channel.
pub(crate) const REDELIVERY_DELIVERED: &str = "delivered";
/// Every byte of this row was already covered by a sibling row in the same plan.
pub(crate) const REDELIVERY_SUPERSEDED: &str = "superseded";
/// A witness answered that the body is already in the channel. Final.
pub(crate) const REDELIVERY_DECLINED: &str = "declined";
/// Back to the start: no witness could be READ, or the POST errored. Neither is
/// a verdict about the body, so a later claim picks the row up again instead of
/// retiring it unread.
pub(crate) const REDELIVERY_PENDING: &str = "pending";

/// One row claimed for redelivery. `id` settles it; the rest reconstruct the body.
#[derive(Clone, Debug)]
pub(crate) struct ClaimedDeadLetter {
    pub id: i64,
    pub channel_id: String,
    pub message_id: Option<String>,
    pub content: String,
    pub reason: String,
}

/// Atomically claim up to `limit` pending rows of `kind` whose age is inside
/// `[min_age_secs, max_age_secs]`. `FOR UPDATE SKIP LOCKED` makes the claim
/// exactly-once across concurrent sweeps and cluster nodes; the returned rows
/// are already `CLAIMED`, so a second call cannot hand them out again.
///
/// Fewest attempts first: rows sent back to `pending` keep their low id,
/// so id order alone lets them fill every batch until newer rows age out.
///
/// `min_age_secs` leaves the normal delivery path time to settle the turn;
/// `max_age_secs` bounds the window to one in which the consumer's
/// already-delivered witnesses can still answer.
pub(crate) async fn claim_pending_redeliveries(
    pool: &PgPool,
    kind: &str,
    min_age_secs: i64,
    max_age_secs: i64,
    limit: i64,
) -> Result<Vec<ClaimedDeadLetter>, sqlx::Error> {
    let rows: Vec<(i64, String, Option<String>, String, String)> = sqlx::query_as(
        "UPDATE relay_dead_letter
            SET redelivery_state = 'claimed',
                redelivery_attempts = redelivery_attempts + 1
          WHERE id IN (
                SELECT id
                  FROM relay_dead_letter
                 WHERE kind = $1
                   AND redelivery_state = 'pending'
                   AND created_at <= NOW() - ($2::BIGINT * INTERVAL '1 second')
                   AND created_at >= NOW() - ($3::BIGINT * INTERVAL '1 second')
                 ORDER BY redelivery_attempts, id
                 LIMIT $4
                 FOR UPDATE SKIP LOCKED
          )
        RETURNING id, channel_id, message_id, content, reason",
    )
    .bind(kind)
    .bind(min_age_secs)
    .bind(max_age_secs)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, channel_id, message_id, content, reason)| ClaimedDeadLetter {
                id,
                channel_id,
                message_id,
                content,
                reason,
            },
        )
        .collect())
}

/// Settle a claimed row into its next `redelivery_state`. Guarded on the claim,
/// so a repeated settle writes nothing and returns 0. `redelivered_at` is stamped
/// only when the row leaves the sweep; a row back in `pending` was not redelivered.
pub(crate) async fn settle_redelivery(
    pool: &PgPool,
    id: i64,
    state: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE relay_dead_letter
            SET redelivery_state = $2,
                redelivered_at = CASE WHEN $2 = $3 THEN NULL ELSE NOW() END
          WHERE id = $1 AND redelivery_state = 'claimed'",
    )
    .bind(id)
    .bind(state)
    .bind(REDELIVERY_PENDING)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    fn terminal_row(content: &str) -> RelayDeadLetterRecord {
        RelayDeadLetterRecord {
            kind: KIND_TERMINAL_NO_DELIVERY_OWNER.to_string(),
            channel_id: "5551".to_string(),
            author_id: None,
            message_id: Some("7001".to_string()),
            content: content.to_string(),
            reason: "r".to_string(),
        }
    }

    /// Claimed ids, sorted: `UPDATE ... RETURNING` does not keep the claim order.
    async fn claim_ids(pool: &PgPool, limit: i64) -> Vec<i64> {
        let mut ids: Vec<i64> =
            claim_pending_redeliveries(pool, KIND_TERMINAL_NO_DELIVERY_OWNER, 0, 3600, limit)
                .await
                .expect("claim")
                .into_iter()
                .map(|row| row.id)
                .collect();
        ids.sort_unstable();
        ids
    }

    /// A batch full of rows that keep settling back to `pending` must not
    /// shut a never-tried row out of the claim until its age window closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deferred_rows_do_not_starve_a_never_tried_row_pg() {
        let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_relay_dlq_starvation",
            "relay dead letter redelivery starvation",
        )
        .await;
        let pool = pg_db.connect_and_migrate().await;
        const BATCH: i64 = 2;

        let mut deferred = Vec::new();
        for n in 0..BATCH {
            deferred.push(
                insert(&pool, &terminal_row(&format!("stuck {n}")))
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(claim_ids(&pool, BATCH).await, deferred);
        for id in &deferred {
            assert_eq!(
                settle_redelivery(&pool, *id, REDELIVERY_PENDING)
                    .await
                    .unwrap(),
                1
            );
        }
        let fresh = insert(&pool, &terminal_row("never tried")).await.unwrap();
        // Premise: the fresh row loses on id, which is the order being replaced.
        assert!(deferred.iter().all(|id| *id < fresh));

        let claimed = claim_ids(&pool, BATCH).await;
        assert_eq!(
            claimed,
            vec![deferred[0], fresh],
            "a never-tried row takes a slot ahead of rows already tried; the rest fill in id order"
        );
        for id in &claimed {
            settle_redelivery(&pool, *id, REDELIVERY_PENDING)
                .await
                .unwrap();
        }
        assert_eq!(
            claim_ids(&pool, BATCH).await,
            vec![deferred[1], fresh],
            "least-tried rows go next, so retried rows rotate instead of pinning the head"
        );

        let stamped: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT redelivered_at FROM relay_dead_letter WHERE id = $1")
                .bind(fresh)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stamped, None,
            "a row sent back to pending was not redelivered"
        );

        assert_eq!(
            settle_redelivery(&pool, fresh, REDELIVERY_DELIVERED)
                .await
                .unwrap(),
            1
        );
        let stamped: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT redelivered_at FROM relay_dead_letter WHERE id = $1")
                .bind(fresh)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            stamped.is_some(),
            "a row leaving the sweep is stamped redelivered"
        );
        assert_eq!(
            settle_redelivery(&pool, fresh, REDELIVERY_DELIVERED)
                .await
                .unwrap(),
            0,
            "a repeated settle is guarded on the claim"
        );

        pool.close().await;
        pg_db.drop().await;
    }

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
