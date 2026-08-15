use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox_delivery_proof::{list_stale_dispatched, settle_dispatched_unknown};
use chrono::{DateTime, Duration, Utc};

fn truncate_to_micros(value: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(value.timestamp_micros()).expect("valid timestamp")
}

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

    let prior_clock = truncate_to_micros(Utc::now() - Duration::minutes(1));
    for (key, status, clock) in [
        ("accepted", IntakeOutboxStatus::Accepted, None),
        (
            "dispatched",
            IntakeOutboxStatus::Dispatched,
            Some(prior_clock),
        ),
        ("done", IntakeOutboxStatus::Done, None),
        ("unknown", IntakeOutboxStatus::Unknown, None),
    ] {
        let id = seed(&pool, key, status, clock).await;
        assert!(!mark_dispatched(&pool, id).await.expect("non-spawned CAS"));
        assert_eq!(audit(&pool, id).await, (status, clock));
        assert_eq!(observe_status(&pool, id).await.unwrap(), Some(status));
    }
    assert!(
        !mark_dispatched(&pool, i64::MAX)
            .await
            .expect("missing-row CAS")
    );
    assert_eq!(observe_status(&pool, i64::MAX).await.unwrap(), None);

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

#[tokio::test]
async fn dispatched_row_blocks_new_open_route_for_the_channel_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let id = seed(&pool, "blocked-channel", IntakeOutboxStatus::Spawned, None).await;
    assert!(mark_dispatched(&pool, id).await.unwrap());

    let error = sqlx::query(
        "INSERT INTO public.intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id,
            user_msg_id, request_owner_id, user_text, turn_kind, agent_id, status
         ) VALUES (
            'worker', 'leader', 'blocked-channel', 'next-message', 'user',
            'next', 'standard', 'agent', 'pending'
         )",
    )
    .execute(&pool)
    .await
    .expect_err("dispatched row must retain the open-route fence");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("intake_outbox_one_open_route_per_channel")
    );

    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn dispatched_row_is_reclaimed_by_sweep_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let id = seed(&pool, "sweep-channel", IntakeOutboxStatus::Spawned, None).await;
    assert!(mark_dispatched(&pool, id).await.unwrap());
    let now = truncate_to_micros(Utc::now());
    let stale_at = now - Duration::hours(1);
    sqlx::query("UPDATE public.intake_outbox SET dispatched_at = $2 WHERE id = $1")
        .bind(id)
        .bind(stale_at)
        .execute(&pool)
        .await
        .unwrap();
    let cutoff = now - Duration::minutes(30);
    assert_eq!(
        list_stale_dispatched(&pool, cutoff, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![id]
    );
    let mut transaction = pool.begin().await.unwrap();
    assert!(
        settle_dispatched_unknown(&mut transaction, id, cutoff, now - Duration::seconds(30))
            .await
            .unwrap()
    );
    transaction.commit().await.unwrap();
    assert_eq!(
        observe_status(&pool, id).await.unwrap(),
        Some(IntakeOutboxStatus::Unknown)
    );

    sqlx::query(
        "INSERT INTO public.intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id,
            user_msg_id, request_owner_id, user_text, turn_kind, agent_id, status
         ) VALUES (
            'worker', 'leader', 'sweep-channel', 'next-message', 'user',
            'next', 'standard', 'agent', 'pending'
         )",
    )
    .execute(&pool)
    .await
    .expect("terminal sweep result releases the open-route fence");

    pool.close().await;
    database.drop().await;
}
