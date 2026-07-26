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
        sqlx::query(
            "UPDATE runtime_cleanup_targets SET channel_id = 'forged',
             operation_high_watermark = operation_high_watermark + 1
             WHERE target_id = $1",
        )
        .bind(cleanup_target.target_id)
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM runtime_cleanup_targets WHERE target_id = $1")
            .bind(cleanup_target.target_id)
            .execute(&pool)
            .await
            .is_err()
    );
    let duplicate = sqlx::query(
        "INSERT INTO runtime_cleanup_targets
         (target_id, identity_kind, provider, discord_token_hash, channel_id,
          operation_high_watermark)
         VALUES ($1, 'discord_channel', 'claude', 'discord_0123456789abcdef',
                 'target-tombstone', 0)",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await;
    assert!(duplicate.is_err());
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
        claim_owner: "worker",
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
        CapabilityUse::NeedsReconcile { request_id }
    );
    let arbitrary_secret = [0_u8; 32];
    assert_eq!(
        replay_capability_request(
            &pool,
            &arbitrary_secret,
            binding.clone(),
            request_id,
            fingerprint,
        )
        .await
        .unwrap(),
        CapabilityUse::BindingMismatch
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
    assert_eq!(
        record_receipt(
            &pool,
            &secret,
            binding.clone(),
            request_id,
            fingerprint,
            "worker",
            attempt,
            ReceiptState::Applied,
            Some([9_u8; 32]),
        )
        .await
        .unwrap(),
        ReceiptWrite::Recorded
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
    let expired: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE runtime_cleanup_capabilities
         SET expires_at = clock_timestamp() - INTERVAL '1 second'
         WHERE capability_id = $1 RETURNING expires_at",
    )
    .bind(binding.capability_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut expired_binding = binding;
    expired_binding.expires_at = expired;
    assert_eq!(
        begin_capability_request(&pool, &secret, expired_binding, fingerprint)
            .await
            .unwrap(),
        CapabilityUse::Expired
    );
    close(db, pool).await;
}

#[tokio::test]
async fn capability_admission_requires_live_exact_claim_pg() {
    let (db, pool) = pool().await;
    let (target, operation) = operation(&pool, "capability-claim").await;
    let claimed = claim_operation(
        &pool,
        operation.operation_id,
        "worker-a",
        Duration::milliseconds(250),
    )
    .await
    .unwrap();
    let (attempt, claim_expiry) = match claimed {
        ClaimResult::Claimed {
            attempt_epoch,
            expires_at,
        } => (attempt_epoch, expires_at),
        other => panic!("{other:?}"),
    };
    let (intent_id, idempotency_identity) = intent(&pool, operation.operation_id).await;
    let binding = CapabilityBinding {
        capability_id: Uuid::new_v4(),
        target_id: target.target_id,
        operation_id: operation.operation_id,
        intent_id,
        attempt_epoch: attempt,
        audience: "claim-bound",
        claim_owner: "worker-a",
        expires_at: claim_expiry - Duration::milliseconds(50),
        idempotency_identity,
    };
    let mut too_long = binding.clone();
    too_long.capability_id = Uuid::new_v4();
    too_long.expires_at = claim_expiry + Duration::milliseconds(1);
    assert!(issue_capability(&pool, too_long).await.is_err());
    let secret = issue_capability(&pool, binding.clone()).await.unwrap();
    let mut wrong_owner = binding.clone();
    wrong_owner.claim_owner = "worker-b";
    assert_eq!(
        begin_capability_request(&pool, &secret, wrong_owner, [1; 32])
            .await
            .unwrap(),
        CapabilityUse::BindingMismatch
    );
    tokio::time::sleep(std::time::Duration::from_millis(275)).await;
    assert_eq!(
        begin_capability_request(&pool, &secret, binding.clone(), [1; 32])
            .await
            .unwrap(),
        CapabilityUse::Expired
    );
    let takeover = claim_operation(
        &pool,
        operation.operation_id,
        "worker-b",
        Duration::seconds(1),
    )
    .await
    .unwrap();
    assert!(matches!(takeover, ClaimResult::Claimed { .. }));
    assert_eq!(
        begin_capability_request(&pool, &secret, binding, [1; 32])
            .await
            .unwrap(),
        CapabilityUse::LostOwnership
    );
    close(db, pool).await;
}

#[tokio::test]
async fn lost_response_replay_after_lease_expiry_needs_reconcile_pg() {
    let (db, pool) = pool().await;
    let (target, operation) = operation(&pool, "capability-reconcile").await;
    let claimed = claim_operation(
        &pool,
        operation.operation_id,
        "worker",
        Duration::milliseconds(250),
    )
    .await
    .unwrap();
    let (attempt, claim_expiry) = match claimed {
        ClaimResult::Claimed {
            attempt_epoch,
            expires_at,
        } => (attempt_epoch, expires_at),
        other => panic!("{other:?}"),
    };
    let (intent_id, idempotency_identity) = intent(&pool, operation.operation_id).await;
    let binding = CapabilityBinding {
        capability_id: Uuid::new_v4(),
        target_id: target.target_id,
        operation_id: operation.operation_id,
        intent_id,
        attempt_epoch: attempt,
        audience: "reconcile",
        claim_owner: "worker",
        expires_at: claim_expiry - Duration::milliseconds(25),
        idempotency_identity,
    };
    let secret = issue_capability(&pool, binding.clone()).await.unwrap();
    let request_id = match begin_capability_request(&pool, &secret, binding.clone(), [2; 32])
        .await
        .unwrap()
    {
        CapabilityUse::Accepted { request_id } => request_id,
        other => panic!("{other:?}"),
    };
    tokio::time::sleep(std::time::Duration::from_millis(275)).await;
    assert_eq!(
        replay_capability_request(&pool, &secret, binding.clone(), request_id, [2; 32])
            .await
            .unwrap(),
        CapabilityUse::NeedsReconcile { request_id }
    );
    assert_eq!(
        record_receipt(
            &pool,
            &secret,
            binding.clone(),
            request_id,
            [2; 32],
            "worker",
            attempt,
            ReceiptState::Applied,
            None,
        )
        .await
        .unwrap(),
        ReceiptWrite::Expired
    );
    let takeover = claim_operation(
        &pool,
        operation.operation_id,
        "worker-b",
        Duration::seconds(1),
    )
    .await
    .unwrap();
    let takeover_attempt = match takeover {
        ClaimResult::Claimed { attempt_epoch, .. } => attempt_epoch,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        record_receipt(
            &pool,
            &secret,
            binding.clone(),
            request_id,
            [2; 32],
            "worker-b",
            takeover_attempt,
            ReceiptState::Applied,
            None,
        )
        .await
        .unwrap(),
        ReceiptWrite::Recorded
    );
    assert_eq!(
        replay_capability_request(&pool, &secret, binding, request_id, [2; 32])
            .await
            .unwrap(),
        CapabilityUse::Replay {
            request_id,
            state: ReceiptState::Applied
        }
    );
    close(db, pool).await;
}

#[tokio::test]
async fn receipt_state_graph_requires_current_reconciliation_authority_pg() {
    let (db, pool) = pool().await;
    let (target, operation) = operation(&pool, "receipt-state").await;
    let first = claim_operation(
        &pool,
        operation.operation_id,
        "worker-a",
        Duration::milliseconds(250),
    )
    .await
    .unwrap();
    let (first_attempt, first_expiry) = match first {
        ClaimResult::Claimed {
            attempt_epoch,
            expires_at,
        } => (attempt_epoch, expires_at),
        other => panic!("{other:?}"),
    };
    let (intent_id, idempotency_identity) = intent(&pool, operation.operation_id).await;
    let binding = CapabilityBinding {
        capability_id: Uuid::new_v4(),
        target_id: target.target_id,
        operation_id: operation.operation_id,
        intent_id,
        attempt_epoch: first_attempt,
        audience: "receipt-state",
        claim_owner: "worker-a",
        expires_at: first_expiry - Duration::milliseconds(25),
        idempotency_identity,
    };
    let secret = issue_capability(&pool, binding.clone()).await.unwrap();
    let request_id = match begin_capability_request(&pool, &secret, binding.clone(), [6; 32])
        .await
        .unwrap()
    {
        CapabilityUse::Accepted { request_id } => request_id,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        record_receipt(
            &pool,
            &secret,
            binding.clone(),
            request_id,
            [6; 32],
            "worker-a",
            first_attempt,
            ReceiptState::Unknown,
            None,
        )
        .await
        .unwrap(),
        ReceiptWrite::Recorded
    );
    assert!(
        sqlx::query(
            "UPDATE runtime_cleanup_receipts SET receipt_state = 'applied'
             WHERE request_id = $1",
        )
        .bind(request_id)
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM runtime_cleanup_receipts WHERE request_id = $1")
            .bind(request_id)
            .execute(&pool)
            .await
            .is_err()
    );
    tokio::time::sleep(std::time::Duration::from_millis(275)).await;
    let takeover = claim_operation(
        &pool,
        operation.operation_id,
        "worker-b",
        Duration::seconds(1),
    )
    .await
    .unwrap();
    let takeover_attempt = match takeover {
        ClaimResult::Claimed { attempt_epoch, .. } => attempt_epoch,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        record_receipt(
            &pool,
            &secret,
            binding.clone(),
            request_id,
            [6; 32],
            "worker-a",
            first_attempt,
            ReceiptState::Applied,
            Some([7; 32]),
        )
        .await
        .unwrap(),
        ReceiptWrite::LostOwnership
    );
    assert_eq!(
        record_receipt(
            &pool,
            &secret,
            binding.clone(),
            request_id,
            [6; 32],
            "worker-b",
            takeover_attempt,
            ReceiptState::Applied,
            Some([7; 32]),
        )
        .await
        .unwrap(),
        ReceiptWrite::Reconciled
    );
    assert_eq!(
        record_receipt(
            &pool,
            &secret,
            binding,
            request_id,
            [6; 32],
            "worker-b",
            takeover_attempt,
            ReceiptState::NotApplied,
            Some([8; 32]),
        )
        .await
        .unwrap(),
        ReceiptWrite::Conflict
    );
    close(db, pool).await;
}

#[tokio::test]
async fn bounded_capability_gc_preserves_active_and_unresolved_pg() {
    let (db, pool) = pool().await;
    let (target, operation) = operation(&pool, "capability-gc").await;
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
    let intents: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT intent_id, idempotency_identity FROM runtime_cleanup_intents
         WHERE operation_id = $1 ORDER BY ordinal LIMIT 3",
    )
    .bind(operation.operation_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut bindings = Vec::new();
    let mut secrets = Vec::new();
    for (index, (intent_id, idempotency_identity)) in intents.into_iter().enumerate() {
        let binding = CapabilityBinding {
            capability_id: Uuid::new_v4(),
            target_id: target.target_id,
            operation_id: operation.operation_id,
            intent_id,
            attempt_epoch: attempt,
            audience: "gc",
            claim_owner: "worker",
            expires_at: now + Duration::seconds(index as i64 + 1),
            idempotency_identity,
        };
        secrets.push(issue_capability(&pool, binding.clone()).await.unwrap());
        bindings.push(binding);
    }
    let unresolved_request =
        match begin_capability_request(&pool, &secrets[1], bindings[1].clone(), [4; 32])
            .await
            .unwrap()
        {
            CapabilityUse::Accepted { request_id } => request_id,
            other => panic!("{other:?}"),
        };
    record_receipt(
        &pool,
        &secrets[1],
        bindings[1].clone(),
        unresolved_request,
        [4; 32],
        "worker",
        attempt,
        ReceiptState::Unknown,
        None,
    )
    .await
    .unwrap();
    let terminal_request =
        match begin_capability_request(&pool, &secrets[2], bindings[2].clone(), [5; 32])
            .await
            .unwrap()
        {
            CapabilityUse::Accepted { request_id } => request_id,
            other => panic!("{other:?}"),
        };
    record_receipt(
        &pool,
        &secrets[2],
        bindings[2].clone(),
        terminal_request,
        [5; 32],
        "worker",
        attempt,
        ReceiptState::Applied,
        None,
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE runtime_cleanup_capabilities
         SET expires_at = clock_timestamp() - INTERVAL '31 days'
         WHERE capability_id <> $1",
    )
    .bind(bindings[0].capability_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(gc_expired_capabilities(&pool, 1).await.unwrap(), 1);
    let remaining: Vec<Uuid> = sqlx::query_scalar(
        "SELECT capability_id FROM runtime_cleanup_capabilities ORDER BY capability_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(remaining.contains(&bindings[1].capability_id));
    assert_eq!(gc_expired_capabilities(&pool, 10).await.unwrap(), 0);
    assert!(gc_expired_capabilities(&pool, 0).await.is_err());
    assert!(gc_expired_capabilities(&pool, 1001).await.is_err());
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn semantic_idempotency_concurrent_request_ids_converge_pg() {
    let (db, pool) = pool().await;
    let (target, operation) = operation(&pool, "semantic-race").await;
    let claimed = claim_operation(
        &pool,
        operation.operation_id,
        "worker",
        Duration::seconds(3),
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
        audience: "semantic-race",
        claim_owner: "worker",
        expires_at: now + Duration::seconds(2),
        idempotency_identity,
    };
    let secret = issue_capability(&pool, binding.clone()).await.unwrap();
    let left = begin_capability_request(&pool, &secret, binding.clone(), [8; 32]);
    let right = begin_capability_request(&pool, &secret, binding.clone(), [8; 32]);
    let (left, right) = tokio::join!(left, right);
    let left = left.unwrap();
    let right = right.unwrap();
    let request_id = match (&left, &right) {
        (
            CapabilityUse::Accepted { request_id },
            CapabilityUse::Replay {
                request_id: replay, ..
            },
        )
        | (
            CapabilityUse::Replay {
                request_id: replay, ..
            },
            CapabilityUse::Accepted { request_id },
        ) => {
            assert_eq!(request_id, replay);
            *request_id
        }
        other => panic!("{other:?}"),
    };
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_cleanup_request_identities
         WHERE operation_id = $1 AND intent_id = $2 AND idempotency_identity = $3",
    )
    .bind(operation.operation_id)
    .bind(intent_id)
    .bind(idempotency_identity)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert_ne!(request_id, Uuid::nil());
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
        claim_owner: "worker",
        expires_at: now + Duration::seconds(1),
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
    record_receipt(
        &pool,
        &secret,
        binding.clone(),
        request_id,
        [1; 32],
        "worker",
        epoch,
        ReceiptState::Unknown,
        None,
    )
    .await
    .unwrap();
    sqlx::query(
        "ALTER TABLE runtime_cleanup_receipts
         DROP CONSTRAINT runtime_cleanup_receipts_check",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DROP TRIGGER trg_runtime_cleanup_receipt_guard ON runtime_cleanup_receipts")
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
