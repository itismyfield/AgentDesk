use super::*;

#[tokio::test]
async fn concurrent_source_authority_serializes_with_claim_and_duplicate_start_pg() {
    let (fixture, pool) = setup().await;
    let first = receipt_on(
        &pool,
        "race-start",
        "claude",
        &["first", "shared"],
        "registered_not_started",
    )
    .await
    .unwrap();
    let second = receipt_on(
        &pool,
        "race-start",
        "claude",
        &["second", "shared"],
        "registered_not_started",
    )
    .await
    .unwrap();
    let mut owner = pool.begin().await.unwrap();
    sqlx::query("UPDATE intake_outbox SET replay_disposition='started_unclassified' WHERE id=$1")
        .bind(first)
        .execute(&mut *owner)
        .await
        .unwrap();
    let mut contender = pool.begin().await.unwrap();
    let contender_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *contender)
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        let result = sqlx::query(
            "UPDATE intake_outbox SET replay_disposition='started_unclassified' WHERE id=$1",
        )
        .bind(second)
        .execute(&mut *contender)
        .await;
        contender.rollback().await.unwrap();
        result
    });
    wait_for_lock(&pool, contender_pid).await;
    owner.commit().await.unwrap();
    assert_fenced(
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap(),
        "two nodes cannot start overlapping absorbed-source families",
    );

    let authority = receipt_on(
        &pool,
        "race-claim",
        "claude",
        &["pending"],
        "registered_not_started",
    )
    .await
    .unwrap();
    let pending = legacy_on(&pool, "race-claim", "claude", "pending", "pending")
        .await
        .unwrap();
    let mut owner = pool.begin().await.unwrap();
    sqlx::query("UPDATE intake_outbox SET replay_disposition='started_unclassified' WHERE id=$1")
        .bind(authority)
        .execute(&mut *owner)
        .await
        .unwrap();
    let mut contender = pool.begin().await.unwrap();
    let contender_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *contender)
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        let result = sqlx::query(
            "UPDATE intake_outbox SET status='claimed',claim_owner='other-node' WHERE id=$1",
        )
        .bind(pending)
        .execute(&mut *contender)
        .await;
        contender.rollback().await.unwrap();
        result
    });
    wait_for_lock(&pool, contender_pid).await;
    owner.commit().await.unwrap();
    assert_fenced(
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap(),
        "old-writer claim waits for authority and rechecks after its commit",
    );
    assert!(
        intake_outbox::claim_pending_for_target(&pool, "worker-a", "claude", "replacement-node")
            .await
            .unwrap()
            .is_none(),
        "new worker candidate query consumes canonical source authority"
    );
    assert!(
        !intake_outbox::return_claimed_to_pending(&pool, pending, "other-node")
            .await
            .unwrap()
    );
    let claimed = legacy_on(&pool, "claimed-hold", "claude", "claimed-source", "claimed")
        .await
        .unwrap();
    sqlx::query("UPDATE intake_outbox SET claim_owner='old-node',claimed_at=NOW()-INTERVAL '2 hours' WHERE id=$1")
        .bind(claimed).execute(&pool).await.unwrap();
    sqlx::query("UPDATE intake_outbox SET replay_disposition='started_unclassified',replay_source_message_ids=ARRAY[user_msg_id] WHERE id=$1")
        .bind(claimed).execute(&pool).await.unwrap();
    assert!(
        !intake_outbox::return_claimed_to_pending(&pool, claimed, "old-node")
            .await
            .unwrap()
    );
    assert!(
        !intake_outbox::mark_accepted(&pool, claimed, "old-node")
            .await
            .unwrap()
    );
    assert_eq!(
        intake_outbox::sweep_stale_pre_accept_claims(&pool, 60)
            .await
            .unwrap(),
        0
    );
    let held_claim: (String, Option<String>, i64) = sqlx::query_as(
        "SELECT status,claim_owner,retry_count::BIGINT FROM intake_outbox WHERE id=$1",
    )
    .bind(claimed)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(held_claim, ("claimed".into(), Some("old-node".into()), 0));
    finish(fixture, pool).await;
}

async fn wait_for_lock(pool: &PgPool, pid: i32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND NOT granted)",
            )
            .bind(pid)
            .fetch_one(pool)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("contending statement must actually wait on a PostgreSQL lock");
}
