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

#[tokio::test]
async fn dispatched_row_blocks_new_open_route_for_the_channel_pg() {
    let database = TestPostgresDb::create().await;
    let pool = database.connect_and_migrate().await;
    let _id = seed(
        &pool,
        "blocked-channel",
        IntakeOutboxStatus::Dispatched,
        Some(Utc::now()),
    )
    .await;

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
    let id = seed(
        &pool,
        "sweep-channel",
        IntakeOutboxStatus::Dispatched,
        Some(Utc::now()),
    )
    .await;
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
