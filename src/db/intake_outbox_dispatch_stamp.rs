//! Monotonic intake-outbox handoff stamp for the Discord turn bridge.

use crate::db::intake_outbox_status::IntakeOutboxStatus;
use sqlx::PgPool;

/// Transitions `spawned -> dispatched` immediately before bridge handoff.
///
/// The bridge site does not own the worker's claim token, so the monotonic
/// status CAS is the authority boundary. Dispatch audit fields are retained.
pub(crate) async fn mark_dispatched(pool: &PgPool, outbox_id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE public.intake_outbox
         SET status = $2, dispatched_at = NOW()
         WHERE id = $1 AND status = $3",
    )
    .bind(outbox_id)
    .bind(IntakeOutboxStatus::Dispatched)
    .bind(IntakeOutboxStatus::Spawned)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use chrono::{DateTime, Utc};

    async fn seed(
        pool: &PgPool,
        key: &str,
        status: IntakeOutboxStatus,
        dispatched_at: Option<DateTime<Utc>>,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO public.intake_outbox (
                target_instance_id, forwarded_by_instance_id, channel_id,
                user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
                status, claim_owner, spawned_at, dispatched_at
             ) VALUES (
                'worker', 'leader', $1, $1, 'user', 'hello', 'standard', 'agent',
                $2, 'dispatch-worker', NOW(), $3
             ) RETURNING id",
        )
        .bind(key)
        .bind(status)
        .bind(dispatched_at)
        .fetch_one(pool)
        .await
        .expect("seed intake row")
    }

    async fn audit(pool: &PgPool, id: i64) -> (IntakeOutboxStatus, Option<DateTime<Utc>>) {
        sqlx::query_as("SELECT status, dispatched_at FROM public.intake_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("audit intake row")
    }

    #[tokio::test]
    async fn mark_dispatched_sets_clock_and_requires_spawned_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let spawned = seed(&pool, "spawned", IntakeOutboxStatus::Spawned, None).await;

        assert!(
            mark_dispatched(&pool, spawned)
                .await
                .expect("stamp spawned")
        );
        let stamped = audit(&pool, spawned).await;
        assert_eq!(stamped.0, IntakeOutboxStatus::Dispatched);
        assert!(stamped.1.is_some());
        assert!(!mark_dispatched(&pool, spawned).await.expect("repeat stamp"));
        assert_eq!(audit(&pool, spawned).await, stamped);

        for (key, status) in [
            ("accepted", IntakeOutboxStatus::Accepted),
            ("done", IntakeOutboxStatus::Done),
            ("unknown", IntakeOutboxStatus::Unknown),
        ] {
            let id = seed(&pool, key, status, None).await;
            assert!(!mark_dispatched(&pool, id).await.expect("non-spawned CAS"));
            assert_eq!(audit(&pool, id).await, (status, None));
        }
        assert!(
            !mark_dispatched(&pool, i64::MAX)
                .await
                .expect("missing-row CAS")
        );

        pool.close().await;
        database.drop().await;
    }

    #[tokio::test]
    async fn mark_dispatched_satisfies_dispatched_requires_clock_check_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let id = seed(&pool, "clock", IntakeOutboxStatus::Spawned, None).await;

        let missing_clock =
            sqlx::query("UPDATE public.intake_outbox SET status = 'dispatched' WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await
                .expect_err("dispatched without a clock must violate the schema check");
        assert_eq!(
            missing_clock
                .as_database_error()
                .and_then(|error| error.code()),
            Some(std::borrow::Cow::Borrowed("23514"))
        );
        assert!(mark_dispatched(&pool, id).await.expect("typed stamp"));
        assert!(audit(&pool, id).await.1.is_some());

        pool.close().await;
        database.drop().await;
    }
}
