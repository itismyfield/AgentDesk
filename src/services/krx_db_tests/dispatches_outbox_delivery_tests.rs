//! Moved from src/db/dispatches/outbox/delivery.rs: seeds its dispatch through the KE test fixture.

use crate::db::dispatches::outbox::delivery::*;
use chrono::{DateTime, Utc};
use sqlx::Row;

#[tokio::test]
async fn old_owner_completion_after_stale_reclaim_is_noop_pg() {
    let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        "agentdesk_dispatch_outbox_lease",
        "dispatch outbox lease fencing tests",
    )
    .await
    else {
        return;
    };
    let pool = pg_db.connect_and_migrate().await;

    crate::dispatch::test_support::seed_pg_dispatch(&pool, "dispatch-lease-fence", "Lease fence")
        .await;
    let row = sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, status, retry_count, claimed_at, claim_owner
         ) VALUES (
            'dispatch-lease-fence', 'notify', 'processing', 0,
            NOW() - INTERVAL '10 minutes', 'old-owner'
         )
         RETURNING id, claimed_at",
    )
    .fetch_one(&pool)
    .await
    .expect("seed claimed dispatch outbox row");
    let outbox_id: i64 = row.try_get("id").unwrap();
    let old_claimed_at: DateTime<Utc> = row.try_get("claimed_at").unwrap();

    sqlx::query(
        "UPDATE dispatch_outbox
            SET claim_owner = 'new-owner',
                claimed_at = NOW()
          WHERE id = $1",
    )
    .bind(outbox_id)
    .execute(&pool)
    .await
    .expect("reclaim dispatch outbox row");

    let result =
        mark_outbox_done_pg(&pool, outbox_id, "sent", "{}", "old-owner", old_claimed_at).await;
    assert!(matches!(
        result,
        Err(DispatchOutboxLeaseUpdateError::StaleLeaseLost { .. })
    ));

    let state = sqlx::query(
        "SELECT status, claim_owner, delivery_status
           FROM dispatch_outbox
          WHERE id = $1",
    )
    .bind(outbox_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state.try_get::<String, _>("status").unwrap(), "processing");
    assert_eq!(
        state.try_get::<String, _>("claim_owner").unwrap(),
        "new-owner"
    );
    assert!(
        state
            .try_get::<Option<String>, _>("delivery_status")
            .unwrap()
            .is_none()
    );

    pool.close().await;
    pg_db.drop().await;
}
