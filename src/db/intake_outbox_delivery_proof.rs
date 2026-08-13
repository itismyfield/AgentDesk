//! Dormant delivery-proof settlement primitives for `intake_outbox`.
//!
//! Future actor and reconciler callers must start with `pool.begin()` and pass
//! the same active `&mut *tx` through judgment, proof locking, strict cutoff
//! recheck, and terminal CAS. Autocommit is forbidden because PostgreSQL would
//! release the proof lock too early.

use super::intake_outbox_status::IntakeOutboxStatus;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

fn normalize_limit(limit: i64) -> i64 {
    limit.clamp(1, 500)
}

#[derive(sqlx::FromRow)]
pub(crate) struct StaleDispatchedRow {
    pub(crate) id: i64,
}

#[allow(dead_code)]
pub(crate) async fn list_stale_dispatched(
    pool: &PgPool,
    cutoff: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<StaleDispatchedRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id FROM public.intake_outbox
         WHERE status = $1 AND dispatched_at < $2
         ORDER BY dispatched_at ASC, id ASC LIMIT $3",
    )
    .bind(IntakeOutboxStatus::Dispatched)
    .bind(cutoff)
    .bind(normalize_limit(limit))
    .fetch_all(pool)
    .await
}

/// Tries to retain the dispatched row for a delivery-proof decision.
///
/// `conn` must be the same connection inside a caller-owned active transaction
/// that performed the judgment and will perform its terminal CAS. Autocommit is
/// forbidden. `false` conflates absence, another state, and lock contention.
#[allow(dead_code)]
pub(crate) async fn try_lock_dispatched_for_proof(
    conn: &mut PgConnection,
    outbox_id: i64,
) -> Result<bool, sqlx::Error> {
    let locked: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM public.intake_outbox
         WHERE id = $1 AND status = $2 FOR UPDATE SKIP LOCKED",
    )
    .bind(outbox_id)
    .bind(IntakeOutboxStatus::Dispatched)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(locked.is_some())
}

/// Applies a successful delivery-proof decision with a dispatched-state CAS.
///
/// The caller must already have judged delivery and locked this row inside an
/// active caller-owned transaction, and must pass that same connection here.
/// Autocommit use is forbidden. Dispatch audit fields are intentionally kept.
#[allow(dead_code)]
pub(crate) async fn mark_done_from_delivery_proof(
    conn: &mut PgConnection,
    outbox_id: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE public.intake_outbox SET status = $2, completed_at = NOW()
         WHERE id = $1 AND status = $3",
    )
    .bind(outbox_id)
    .bind(IntakeOutboxStatus::Done)
    .bind(IntakeOutboxStatus::Dispatched)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Locks and settles a strictly stale dispatched row as official `Unknown`.
///
/// `conn` must belong to the caller-owned active transaction that performed
/// reconciliation judgment. The same connection retains the proof lock through
/// the cutoff recheck and CAS; autocommit use is forbidden.
#[allow(dead_code)]
pub(crate) async fn settle_dispatched_unknown(
    conn: &mut PgConnection,
    outbox_id: i64,
    cutoff: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    if !try_lock_dispatched_for_proof(conn, outbox_id).await? {
        return Ok(false);
    }
    let result = sqlx::query(
        "UPDATE public.intake_outbox SET status = $2, completed_at = NOW()
         WHERE id = $1 AND status = $3 AND dispatched_at < $4",
    )
    .bind(outbox_id)
    .bind(IntakeOutboxStatus::Unknown)
    .bind(IntakeOutboxStatus::Dispatched)
    .bind(cutoff)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use chrono::Duration;
    use std::sync::Arc;
    use tokio::sync::Barrier;
    use tokio::time::{Duration as TokioDuration, timeout};

    type Audit = (
        IntakeOutboxStatus,
        Option<DateTime<Utc>>,
        Option<String>,
        Option<DateTime<Utc>>,
    );

    fn pg_time(value: DateTime<Utc>) -> DateTime<Utc> {
        DateTime::from_timestamp_micros(value.timestamp_micros()).expect("valid PG timestamp") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    async fn seed(pool: &PgPool, key: &str, status: IntakeOutboxStatus, at: DateTime<Utc>) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, channel_id,
                user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
                status, claim_owner, dispatched_at
             ) VALUES (
                'worker', 'leader', $1, $1, 'user', 'hello', 'standard', 'agent',
                $2, 'dispatch-worker', $3
             ) RETURNING id",
        )
        .bind(key)
        .bind(status)
        .bind(at)
        .fetch_one(pool)
        .await
        .expect("seed intake outbox row") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    async fn audit(pool: &PgPool, id: i64) -> Audit {
        sqlx::query_as(
            "SELECT status, completed_at, claim_owner, dispatched_at
             FROM intake_outbox WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("fetch intake outbox audit fields") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_stale_dispatched_is_strict_ordered_and_bounded_pg() {
        assert_eq!(
            [-1, 0, 500, i64::MAX].map(normalize_limit),
            [1, 1, 500, 500]
        );
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let cutoff = pg_time(Utc::now());
        let oldest_at = cutoff - Duration::minutes(2);
        let tied_at = cutoff - Duration::minutes(1);
        let earliest = seed(&pool, "a", IntakeOutboxStatus::Dispatched, oldest_at).await;
        let tied_first = seed(&pool, "b", IntakeOutboxStatus::Dispatched, tied_at).await;
        let tied_second = seed(&pool, "c", IntakeOutboxStatus::Dispatched, tied_at).await;
        let equal = seed(&pool, "list-equal", IntakeOutboxStatus::Dispatched, cutoff).await;
        let after_at = cutoff + Duration::seconds(1);
        let done_at = cutoff - Duration::minutes(3);
        seed(&pool, "after", IntakeOutboxStatus::Dispatched, after_at).await;
        seed(&pool, "done", IntakeOutboxStatus::Done, done_at).await;
        let ids: Vec<_> = list_stale_dispatched(&pool, cutoff, 3)
            .await
            .expect("list stale rows") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(ids, vec![earliest, tied_first, tied_second]);
        let all_ids: Vec<_> = list_stale_dispatched(&pool, cutoff, 10)
            .await
            .expect("list all eligible rows") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(all_ids, vec![earliest, tied_first, tied_second]);
        assert!(!all_ids.contains(&equal), "cutoff equality is not stale");
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_done_from_delivery_proof_is_idempotent_and_preserves_dispatch_audit_fields_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let dispatched_at = pg_time(Utc::now()) - Duration::minutes(2);
        let id = seed(&pool, "done", IntakeOutboxStatus::Dispatched, dispatched_at).await;
        let mut tx = pool.begin().await.expect("begin proof transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert!(
            try_lock_dispatched_for_proof(&mut *tx, id)
                .await
                .expect("lock row") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        );
        assert!(
            mark_done_from_delivery_proof(&mut *tx, id)
                .await
                .expect("mark done") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        );
        let inside: Audit = sqlx::query_as("SELECT status, completed_at, claim_owner, dispatched_at FROM intake_outbox WHERE id=$1")
            .bind(id).fetch_one(&mut *tx).await.expect("audit uncommitted done"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert_eq!(inside.0, IntakeOutboxStatus::Done);
        assert!(inside.1.is_some());
        tx.rollback().await.expect("rollback proof transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert_eq!(
            audit(&pool, id).await,
            (
                IntakeOutboxStatus::Dispatched,
                None,
                Some("dispatch-worker".into()),
                Some(dispatched_at)
            )
        );

        let mut tx = pool
            .begin()
            .await
            .expect("begin committed proof transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert!(
            try_lock_dispatched_for_proof(&mut *tx, id)
                .await
                .expect("lock row") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        );
        assert!(
            mark_done_from_delivery_proof(&mut *tx, id)
                .await
                .expect("mark done") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        );
        tx.commit().await.expect("commit done"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        let committed = audit(&pool, id).await;
        assert_eq!(
            (committed.0, committed.2, committed.3),
            (
                IntakeOutboxStatus::Done,
                Some("dispatch-worker".into()),
                Some(dispatched_at)
            )
        );
        assert!(committed.1.is_some());
        let mut repeat = pool.begin().await.expect("begin repeat transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert!(
            !mark_done_from_delivery_proof(&mut *repeat, id)
                .await
                .expect("repeat CAS") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        );
        repeat.rollback().await.expect("rollback repeat"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settle_dispatched_unknown_rechecks_cutoff_and_preserves_audit_fields_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let cutoff = pg_time(Utc::now());
        let old_at = cutoff - Duration::minutes(1);
        let old = seed(&pool, "unknown-old", IntakeOutboxStatus::Dispatched, old_at).await;
        let fresh_at = cutoff + Duration::seconds(1);
        let equal = seed(&pool, "equal", IntakeOutboxStatus::Dispatched, cutoff).await;
        let fresh = seed(&pool, "fresh", IntakeOutboxStatus::Dispatched, fresh_at).await;
        for (id, expected) in [(old, true), (equal, false), (fresh, false)] {
            let mut tx = pool.begin().await.expect("begin unknown transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
            assert_eq!(
                settle_dispatched_unknown(&mut *tx, id, cutoff)
                    .await
                    .expect("settle unknown"), // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
                expected
            );
            tx.commit().await.expect("commit unknown transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        }
        let settled = audit(&pool, old).await;
        assert_eq!(
            (settled.0, settled.2, settled.3),
            (
                IntakeOutboxStatus::Unknown,
                Some("dispatch-worker".into()),
                Some(old_at)
            )
        );
        assert!(settled.1.is_some());
        let mut repeat = pool.begin().await.expect("begin repeat"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert!(
            !settle_dispatched_unknown(&mut *repeat, old, cutoff)
                .await
                .expect("repeat unknown") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        );
        repeat.rollback().await.expect("rollback repeat"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert_eq!(audit(&pool, equal).await.0, IntakeOutboxStatus::Dispatched);
        assert_eq!(audit(&pool, fresh).await.0, IntakeOutboxStatus::Dispatched);
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proof_lock_skips_contention_and_terminal_cas_has_one_winner_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let cutoff = pg_time(Utc::now());
        let old_at = cutoff - Duration::minutes(1);
        let id = seed(&pool, "contended", IntakeOutboxStatus::Dispatched, old_at).await;
        let mut holder = pool.begin().await.expect("begin lock holder"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert!(
            try_lock_dispatched_for_proof(&mut *holder, id)
                .await
                .expect("holder lock") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        );
        let mut contender = pool.begin().await.expect("begin contender"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        let skipped = timeout(
            TokioDuration::from_secs(1),
            try_lock_dispatched_for_proof(&mut *contender, id),
        )
        .await
        .expect("SKIP LOCKED must not block") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        .expect("contender query"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert!(!skipped);
        contender.rollback().await.expect("rollback contender"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        holder.rollback().await.expect("release holder lock"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion

        let gate = Arc::new(Barrier::new(2));
        let done = async {
            let mut tx = pool.begin().await.expect("begin done actor"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
            gate.wait().await;
            let won = try_lock_dispatched_for_proof(&mut *tx, id)
                .await
                .expect("done lock") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
                && mark_done_from_delivery_proof(&mut *tx, id)
                    .await
                    .expect("done CAS"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
            tx.commit().await.expect("commit done actor"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
            won
        };
        let unknown_gate = Arc::clone(&gate);
        let unknown = async {
            let mut tx = pool.begin().await.expect("begin unknown actor"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
            unknown_gate.wait().await;
            let won = settle_dispatched_unknown(&mut *tx, id, cutoff)
                .await
                .expect("unknown CAS"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
            tx.commit().await.expect("commit unknown actor"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
            won
        };
        let (done_won, unknown_won) = tokio::join!(done, unknown);
        assert_ne!(done_won, unknown_won, "exactly one terminal CAS wins");
        let winner = audit(&pool, id).await;
        assert!(matches!(
            winner.0,
            IntakeOutboxStatus::Done | IntakeOutboxStatus::Unknown
        ));
        assert!(winner.1.is_some(), "winner stamps completed_at");
        pool.close().await;
        pg_db.drop().await;
    }
}
