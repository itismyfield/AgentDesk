use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;
use sqlx::PgPool;

async fn pool() -> (DispatchPostgresTestDb, PgPool) {
    let db = DispatchPostgresTestDb::create("adk_cleanup_substrate", "cleanup substrate pg").await;
    let pool = db.connect_and_migrate_with_max_connections(12).await;
    (db, pool)
}

async fn close(db: DispatchPostgresTestDb, pool: PgPool) {
    pool.close().await;
    db.drop().await;
}

fn identity<'a>(channel: &'a str) -> CanonicalCleanupIdentity<'a> {
    CanonicalCleanupIdentity {
        kind: CleanupIdentityKind::DiscordChannel,
        provider: "claude",
        discord_token_hash: "discord_0123456789abcdef",
        channel_id: channel,
    }
}

async fn target(pool: &PgPool, channel: &str) -> CleanupTarget {
    converge_target(pool, identity(channel), Uuid::new_v4())
        .await
        .unwrap()
}

async fn operation(pool: &PgPool, channel: &str) -> (CleanupTarget, CreatedOperation) {
    let target = target(pool, channel).await;
    let operation = create_operation(pool, target.target_id, Uuid::new_v4())
        .await
        .unwrap();
    (target, operation)
}

async fn intent(pool: &PgPool, operation_id: Uuid) -> (Uuid, Uuid) {
    sqlx::query_as(
        "SELECT intent_id, idempotency_identity FROM runtime_cleanup_intents
         WHERE operation_id = $1 ORDER BY ordinal LIMIT 1",
    )
    .bind(operation_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canonical_concurrent_target_converges_pg() {
    let (db, pool) = pool().await;
    let (left, right) = tokio::join!(
        converge_target(&pool, identity("canonical-race"), Uuid::new_v4()),
        converge_target(&pool, identity("canonical-race"), Uuid::new_v4())
    );
    assert_eq!(left.unwrap().target_id, right.unwrap().target_id);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_cleanup_targets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locator_retirement_never_reuses_generation_pg() {
    let (db, pool) = pool().await;
    let first = target(&pool, "locator-a").await;
    let second = target(&pool, "locator-b").await;
    let initial = reserve_locator(&pool, "host:session", first.target_id)
        .await
        .unwrap();
    assert_eq!(initial.generation, 1);
    assert!(retire_locator(&pool, "host:session", 1).await.unwrap());
    assert!(
        resolve_locator(&pool, "host:session")
            .await
            .unwrap()
            .is_none()
    );
    let replacement = reserve_locator(&pool, "host:session", second.target_id)
        .await
        .unwrap();
    assert_eq!(replacement.generation, 2);
    assert_eq!(replacement.target_id, second.target_id);
    let history: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT generation, active FROM runtime_cleanup_locator_claims
         WHERE locator = 'host:session' ORDER BY generation",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(history, vec![(1, false), (2, true)]);
    assert!(
        sqlx::query(
            "DELETE FROM runtime_cleanup_locator_claims
             WHERE locator = 'host:session' AND generation = 1",
        )
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE runtime_cleanup_locator_claims SET target_id = $1
             WHERE locator = 'host:session' AND generation = 2",
        )
        .bind(first.target_id)
        .execute(&pool)
        .await
        .is_err()
    );
    close(db, pool).await;
}

#[tokio::test]
async fn target_retirement_is_a_permanent_admission_tombstone_pg() {
    let (db, pool) = pool().await;
    let cleanup_target = target(&pool, "target-tombstone").await;
    assert!(
        retire_target(&pool, cleanup_target.target_id)
            .await
            .unwrap()
    );
    assert!(
        !retire_target(&pool, cleanup_target.target_id)
            .await
            .unwrap()
    );
    assert!(
        reserve_locator(&pool, "retired-target-locator", cleanup_target.target_id)
            .await
            .is_err()
    );
    assert!(
        create_operation(&pool, cleanup_target.target_id, Uuid::new_v4())
            .await
            .is_err()
    );
    let converged = target(&pool, "target-tombstone").await;
    assert_eq!(converged.target_id, cleanup_target.target_id);
    assert!(converged.retired);
    close(db, pool).await;
}

#[tokio::test]
async fn deleting_session_binding_never_deletes_target_pg() {
    let (db, pool) = pool().await;
    let target = target(&pool, "session-independent").await;
    let session_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status)
         VALUES ('cleanup-session', 'claude', 'idle') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    bind_session(&pool, target.target_id, session_id)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
    let target_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM runtime_cleanup_targets WHERE target_id = $1")
            .bind(target.target_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let binding_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_cleanup_target_session_bindings WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((target_count, binding_count), (1, 0));
    close(db, pool).await;
}

#[tokio::test]
async fn operation_and_attempt_epochs_are_monotonic_and_stale_tokens_fail_pg() {
    let (db, pool) = pool().await;
    let (target, first) = operation(&pool, "epochs").await;
    let claim1 = claim_operation(
        &pool,
        first.operation_id,
        "worker-a",
        Duration::milliseconds(30),
    )
    .await
    .unwrap();
    let epoch1 = match claim1 {
        ClaimResult::Claimed { attempt_epoch, .. } => attempt_epoch,
        other => panic!("{other:?}"),
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let claim2 = claim_operation(&pool, first.operation_id, "worker-b", Duration::seconds(1))
        .await
        .unwrap();
    let epoch2 = match claim2 {
        ClaimResult::Claimed { attempt_epoch, .. } => attempt_epoch,
        other => panic!("{other:?}"),
    };
    assert!(epoch2 > epoch1);
    assert!(
        !transition_operation(
            &pool,
            first.operation_id,
            "worker-a",
            epoch1,
            OperationState::Committed,
        )
        .await
        .unwrap()
    );
    assert!(
        transition_operation(
            &pool,
            first.operation_id,
            "worker-b",
            epoch2,
            OperationState::Committed,
        )
        .await
        .unwrap()
    );
    assert!(
        transition_operation(
            &pool,
            first.operation_id,
            "worker-b",
            epoch2,
            OperationState::Completed,
        )
        .await
        .unwrap()
    );
    let second = create_operation(&pool, target.target_id, Uuid::new_v4())
        .await
        .unwrap();
    assert!(second.operation_epoch > first.operation_epoch);
    close(db, pool).await;
}

#[tokio::test]
async fn database_clock_bounds_and_renew_are_enforced_pg() {
    let (db, pool) = pool().await;
    let (_, operation) = operation(&pool, "db-clock").await;
    assert!(
        claim_operation(
            &pool,
            operation.operation_id,
            "worker",
            Duration::seconds(301)
        )
        .await
        .is_err()
    );
    let claimed = claim_operation(
        &pool,
        operation.operation_id,
        "worker",
        Duration::seconds(2),
    )
    .await
    .unwrap();
    let (epoch, expires) = match claimed {
        ClaimResult::Claimed {
            attempt_epoch,
            expires_at,
        } => (attempt_epoch, expires_at),
        other => panic!("{other:?}"),
    };
    let renewed = renew_operation(
        &pool,
        operation.operation_id,
        "worker",
        epoch,
        Duration::seconds(4),
    )
    .await
    .unwrap();
    let renewed_expires = match renewed {
        ClaimResult::Renewed { expires_at, .. } => expires_at,
        other => panic!("{other:?}"),
    };
    assert!(renewed_expires > expires);
    assert_eq!(
        renew_operation(
            &pool,
            operation.operation_id,
            "worker",
            epoch + 1,
            Duration::seconds(1)
        )
        .await
        .unwrap(),
        ClaimResult::Stale
    );
    close(db, pool).await;
}

#[tokio::test]
async fn capability_binding_expiry_replay_conflict_and_unknown_are_typed_pg() {
    let (db, pool) = pool().await;
    let (target, operation) = operation(&pool, "capability").await;
    let claimed = claim_operation(
        &pool,
        operation.operation_id,
        "worker",
        Duration::seconds(5),
    )
    .await
    .unwrap();
    let attempt = match claimed {
        ClaimResult::Claimed { attempt_epoch, .. } => attempt_epoch,
        other => panic!("{other:?}"),
    };
    let (intent_id, idempotency_identity) = intent(&pool, operation.operation_id).await;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let binding = CapabilityBinding {
        capability_id: Uuid::new_v4(),
        target_id: target.target_id,
        operation_id: operation.operation_id,
        intent_id,
        attempt_epoch: attempt,
        audience: "cleanup-worker",
        expires_at: now + Duration::seconds(3),
        idempotency_identity,
    };
    let secret = issue_capability(&pool, binding.clone()).await.unwrap();
    let fingerprint = [7_u8; 32];
    let request_id = match begin_capability_request(&pool, &secret, binding.clone(), fingerprint)
        .await
        .unwrap()
    {
        CapabilityUse::Accepted { request_id } => request_id,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        replay_capability_request(&pool, &secret, binding.clone(), request_id, fingerprint)
            .await
            .unwrap(),
        CapabilityUse::Replay {
            request_id,
            state: ReceiptState::Unknown
        }
    );
    assert_eq!(
        begin_capability_request(&pool, &secret, binding.clone(), fingerprint)
            .await
            .unwrap(),
        CapabilityUse::Replay {
            request_id,
            state: ReceiptState::Unknown
        }
    );
    assert_eq!(
        replay_capability_request(&pool, &secret, binding.clone(), request_id, [8_u8; 32])
            .await
            .unwrap(),
        CapabilityUse::FingerprintConflict
    );
    let mut wrong = binding.clone();
    wrong.audience = "other-worker";
    assert_eq!(
        begin_capability_request(&pool, &secret, wrong, fingerprint)
            .await
            .unwrap(),
        CapabilityUse::BindingMismatch
    );
    assert!(
        record_receipt(&pool, request_id, ReceiptState::Applied, Some([9_u8; 32]))
            .await
            .unwrap()
    );
    assert_eq!(
        replay_capability_request(&pool, &secret, binding.clone(), request_id, fingerprint)
            .await
            .unwrap(),
        CapabilityUse::Replay {
            request_id,
            state: ReceiptState::Applied
        }
    );
    sqlx::query("UPDATE runtime_cleanup_capabilities SET expires_at = clock_timestamp() - INTERVAL '1 second' WHERE capability_id = $1")
        .bind(binding.capability_id).execute(&pool).await.unwrap();
    assert_eq!(
        begin_capability_request(&pool, &secret, binding, fingerprint)
            .await
            .unwrap(),
        CapabilityUse::Expired
    );
    close(db, pool).await;
}

#[tokio::test]
async fn canonical_plan_and_intent_target_are_database_enforced_pg() {
    let (db, pool) = pool().await;
    let (cleanup_target, operation) = operation(&pool, "canonical-plan").await;
    let rows: Vec<(i16, String)> = sqlx::query_as(
        "SELECT ordinal, intent_kind FROM runtime_cleanup_intents
         WHERE operation_id = $1 ORDER BY ordinal",
    )
    .bind(operation.operation_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        PLAN.into_iter()
            .map(|(kind, ordinal)| (ordinal, kind.to_owned()))
            .collect::<Vec<_>>()
    );
    assert!(
        sqlx::query(
            "INSERT INTO runtime_cleanup_intents
             (operation_id, intent_id, ordinal, intent_kind, target_id, idempotency_identity)
             VALUES ($1, $2, 2, 'block_runtime_admission', $3, $4)",
        )
        .bind(operation.operation_id)
        .bind(Uuid::new_v4())
        .bind(cleanup_target.target_id)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .is_err()
    );
    let other = target(&pool, "canonical-plan-other").await;
    assert!(
        sqlx::query(
            "INSERT INTO runtime_cleanup_intents
             (operation_id, intent_id, ordinal, intent_kind, target_id, idempotency_identity)
             VALUES ($1, $2, 7, 'release_runtime_slot', $3, $4)",
        )
        .bind(operation.operation_id)
        .bind(Uuid::new_v4())
        .bind(other.target_id)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .is_err()
    );
    close(db, pool).await;
}

#[tokio::test]
async fn legal_state_graph_is_database_enforced_pg() {
    let (db, pool) = pool().await;
    let (_, operation) = operation(&pool, "state-graph").await;
    let epoch = match claim_operation(
        &pool,
        operation.operation_id,
        "worker",
        Duration::seconds(2),
    )
    .await
    .unwrap()
    {
        ClaimResult::Claimed { attempt_epoch, .. } => attempt_epoch,
        other => panic!("{other:?}"),
    };
    let illegal = sqlx::query(
        "UPDATE runtime_cleanup_operations SET state = 'completed', committed_at = clock_timestamp(),
         completed_at = clock_timestamp() WHERE operation_id = $1",
    ).bind(operation.operation_id).execute(&pool).await;
    assert!(illegal.is_err());
    assert!(
        transition_operation(
            &pool,
            operation.operation_id,
            "worker",
            epoch,
            OperationState::Aborted
        )
        .await
        .unwrap()
    );
    let resurrection = sqlx::query(
        "UPDATE runtime_cleanup_operations SET state = 'open', aborted_at = NULL WHERE operation_id = $1",
    ).bind(operation.operation_id).execute(&pool).await;
    assert!(resurrection.is_err());
    close(db, pool).await;
}

#[tokio::test]
async fn receipt_gc_preserves_request_target_and_locator_authority_pg() {
    let (db, pool) = pool().await;
    let (target, operation) = operation(&pool, "gc").await;
    reserve_locator(&pool, "gc-locator", target.target_id)
        .await
        .unwrap();
    retire_locator(&pool, "gc-locator", 1).await.unwrap();
    let epoch = match claim_operation(
        &pool,
        operation.operation_id,
        "worker",
        Duration::seconds(2),
    )
    .await
    .unwrap()
    {
        ClaimResult::Claimed { attempt_epoch, .. } => attempt_epoch,
        other => panic!("{other:?}"),
    };
    let (intent_id, idem) = intent(&pool, operation.operation_id).await;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let binding = CapabilityBinding {
        capability_id: Uuid::new_v4(),
        target_id: target.target_id,
        operation_id: operation.operation_id,
        intent_id,
        attempt_epoch: epoch,
        audience: "gc",
        expires_at: now + Duration::seconds(2),
        idempotency_identity: idem,
    };
    let secret = issue_capability(&pool, binding.clone()).await.unwrap();
    let request_id = match begin_capability_request(&pool, &secret, binding.clone(), [1; 32])
        .await
        .unwrap()
    {
        CapabilityUse::Accepted { request_id } => request_id,
        other => panic!("{other:?}"),
    };
    record_receipt(&pool, request_id, ReceiptState::Unknown, None)
        .await
        .unwrap();
    sqlx::query(
        "ALTER TABLE runtime_cleanup_receipts
         DROP CONSTRAINT runtime_cleanup_receipts_check",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE runtime_cleanup_receipts
         SET retain_until = clock_timestamp() - INTERVAL '1 second'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(gc_terminal_receipts(&pool, 1).await.unwrap(), 1);
    let authority: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT operation_high_watermark FROM runtime_cleanup_targets WHERE target_id = $1),
                (SELECT COUNT(*) FROM runtime_cleanup_locator_claims WHERE locator = 'gc-locator'),
                (SELECT COUNT(*) FROM runtime_cleanup_request_identities WHERE request_id = $2)",
    ).bind(target.target_id).bind(request_id).fetch_one(&pool).await.unwrap();
    assert_eq!(authority, (operation.operation_epoch, 1, 1));
    assert_eq!(
        replay_capability_request(&pool, &secret, binding, request_id, [2; 32])
            .await
            .unwrap(),
        CapabilityUse::FingerprintConflict
    );
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_gc_and_lock_order_complete_without_deadlock_pg() {
    let (db, pool) = pool().await;
    let first = target(&pool, "deadlock-a").await;
    let second = target(&pool, "deadlock-b").await;
    let left = reserve_locator(&pool, "same-locator", first.target_id);
    let right = reserve_locator(&pool, "same-locator", second.target_id);
    let (left, right) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(left, right)
    })
    .await
    .expect("locator contenders must not deadlock");
    let winner = left.unwrap();
    assert_eq!(right.unwrap(), winner);
    assert!(gc_terminal_receipts(&pool, 0).await.is_err());
    assert!(gc_terminal_receipts(&pool, 1001).await.is_err());
    close(db, pool).await;
}
