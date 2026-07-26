use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;

async fn create_pool() -> (DispatchPostgresTestDb, PgPool) {
    let db = DispatchPostgresTestDb::create(
        "agentdesk_runtime_cleanup",
        "runtime cleanup coordinator postgres tests",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(8).await;
    (db, pool)
}

async fn close(db: DispatchPostgresTestDb, pool: PgPool) {
    pool.close().await;
    db.drop().await;
}

async fn seed_session(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status)
         VALUES ($1, 'claude', 'idle') RETURNING id",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn command<'a>(
    operation_id: Uuid,
    request_key: &'a str,
    session_id: i64,
) -> CreateRuntimeCleanupOperation<'a> {
    CreateRuntimeCleanupOperation {
        operation_id,
        request_key,
        target: RuntimeCleanupTarget { session_id },
        action: RuntimeCleanupAction::ClearForResume,
    }
}

async fn create_and_fence(pool: &PgPool, request_key: &str, session_id: i64) -> (Uuid, i64) {
    let operation_id = Uuid::new_v4();
    assert_eq!(
        create_runtime_cleanup_operation(pool, &command(operation_id, request_key, session_id))
            .await
            .unwrap(),
        CreateRuntimeCleanupResult::Created { operation_id }
    );
    let fence = match fence_runtime_cleanup_operation(pool, operation_id)
        .await
        .unwrap()
    {
        FenceRuntimeCleanupResult::Advanced { fence } => fence,
        other => panic!("unexpected fence result: {other:?}"),
    };
    (operation_id, fence)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canonical_session_id_coalesces_primary_and_alias_target_pg() {
    let (db, pool) = create_pool().await;
    let session_id = seed_session(&pool, "primary-locator").await;
    sqlx::query(
        "INSERT INTO session_key_aliases (session_key, session_id) VALUES ('alias-locator', $1)",
    )
    .bind(session_id)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        resolve_runtime_cleanup_target(&pool, "primary-locator")
            .await
            .unwrap(),
        Some(RuntimeCleanupTarget { session_id })
    );
    assert_eq!(
        resolve_runtime_cleanup_target(&pool, "alias-locator")
            .await
            .unwrap(),
        Some(RuntimeCleanupTarget { session_id })
    );

    let first_id = Uuid::new_v4();
    assert_eq!(
        create_runtime_cleanup_operation(
            &pool,
            &command(first_id, "canonical-primary", session_id)
        )
        .await
        .unwrap(),
        CreateRuntimeCleanupResult::Created {
            operation_id: first_id
        }
    );
    assert_eq!(
        create_runtime_cleanup_operation(
            &pool,
            &command(Uuid::new_v4(), "canonical-alias", session_id)
        )
        .await
        .unwrap(),
        CreateRuntimeCleanupResult::TargetBusy {
            operation_id: first_id
        }
    );
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_request_different_target_returns_typed_conflict_pg() {
    let (db, pool) = create_pool().await;
    let first_session = seed_session(&pool, "request-race-a").await;
    let second_session = seed_session(&pool, "request-race-b").await;
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();
    let first = command(first_id, "shared-request-key", first_session);
    let second = command(second_id, "shared-request-key", second_session);
    let (left, right) = tokio::join!(
        create_runtime_cleanup_operation(&pool, &first),
        create_runtime_cleanup_operation(&pool, &second),
    );
    let results = [left.unwrap(), right.unwrap()];
    let created_id = results
        .iter()
        .find_map(|result| match result {
            CreateRuntimeCleanupResult::Created { operation_id } => Some(*operation_id),
            _ => None,
        })
        .expect("one creator");
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, CreateRuntimeCleanupResult::Created { .. }))
            .count(),
        1
    );
    assert_eq!(results.iter().filter(|result| matches!(result, CreateRuntimeCleanupResult::RequestConflict { operation_id } if *operation_id == created_id)).count(), 1);
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_plan_is_atomic_closed_and_database_enforced_pg() {
    let (db, pool) = create_pool().await;
    let session_id = seed_session(&pool, "canonical-plan").await;
    let operation_id = Uuid::new_v4();
    assert_eq!(
        create_runtime_cleanup_operation(
            &pool,
            &command(operation_id, "canonical-plan", session_id)
        )
        .await
        .unwrap(),
        CreateRuntimeCleanupResult::Created { operation_id }
    );
    let plan = sqlx::query_as::<_, (String, i16)>(
        "SELECT intent_kind, ordinal FROM runtime_cleanup_intents WHERE operation_id = $1 ORDER BY ordinal",
    )
    .bind(operation_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(plan.len(), CLEAR_FOR_RESUME_PLAN.len());
    assert!(plan.iter().zip(CLEAR_FOR_RESUME_PLAN).all(
        |((kind, ordinal), (expected_kind, expected_ordinal))| {
            kind == expected_kind && *ordinal == expected_ordinal
        }
    ));

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET CONSTRAINTS trg_runtime_cleanup_intent_plan DEFERRED")
        .execute(&mut *tx)
        .await
        .unwrap();
    let mutation =
        sqlx::query("DELETE FROM runtime_cleanup_intents WHERE operation_id = $1 AND ordinal = 6")
            .bind(operation_id)
            .execute(&mut *tx)
            .await;
    assert!(mutation.is_err(), "ordinary intent deletion is forbidden");
    tx.rollback().await.unwrap();
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attempt_token_distinguishes_replay_competitor_and_recovery_takeover_pg() {
    let (db, pool) = create_pool().await;
    let session_id = seed_session(&pool, "attempt-owner").await;
    let (operation_id, fence) = create_and_fence(&pool, "attempt-owner", session_id).await;
    let now = Utc::now();
    let first_token = Uuid::new_v4();
    let first = BeginRuntimeCleanupAttempt {
        operation_id,
        expected_fence: fence,
        claim_owner: "worker-a",
        attempt_token: first_token,
        lease_expires_at: now + chrono::Duration::milliseconds(500),
    };
    assert_eq!(
        begin_runtime_cleanup_attempt(&pool, first).await.unwrap(),
        BeginRuntimeCleanupResult::Acquired { attempt_no: 1 }
    );
    assert_eq!(
        begin_runtime_cleanup_attempt(&pool, first).await.unwrap(),
        BeginRuntimeCleanupResult::Replayed { attempt_no: 1 }
    );
    assert_eq!(
        begin_runtime_cleanup_attempt(
            &pool,
            BeginRuntimeCleanupAttempt {
                claim_owner: "worker-b",
                ..first
            },
        )
        .await
        .unwrap(),
        BeginRuntimeCleanupResult::LostOwnership { attempt_no: 1 }
    );

    let competing = BeginRuntimeCleanupAttempt {
        claim_owner: "worker-b",
        attempt_token: Uuid::new_v4(),
        ..first
    };
    assert_eq!(
        begin_runtime_cleanup_attempt(&pool, competing)
            .await
            .unwrap(),
        BeginRuntimeCleanupResult::LeaseHeld {
            owner: "worker-a".into(),
            attempt_no: 1
        }
    );

    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let takeover_token = Uuid::new_v4();
    let takeover = BeginRuntimeCleanupAttempt {
        claim_owner: "worker-b",
        attempt_token: takeover_token,
        lease_expires_at: now + chrono::Duration::minutes(2),
        ..first
    };
    assert_eq!(
        begin_runtime_cleanup_attempt(&pool, takeover)
            .await
            .unwrap(),
        BeginRuntimeCleanupResult::Acquired { attempt_no: 2 }
    );
    assert_eq!(
        complete_runtime_cleanup_attempt(
            &pool,
            CompleteRuntimeCleanupAttempt {
                operation_id,
                expected_fence: fence,
                attempt_token: first_token
            }
        )
        .await
        .unwrap(),
        CompleteRuntimeCleanupResult::LostOwnership
    );
    assert_eq!(
        complete_runtime_cleanup_attempt(
            &pool,
            CompleteRuntimeCleanupAttempt {
                operation_id,
                expected_fence: fence,
                attempt_token: takeover_token
            }
        )
        .await
        .unwrap(),
        CompleteRuntimeCleanupResult::Completed
    );
    assert_eq!(
        complete_runtime_cleanup_attempt(
            &pool,
            CompleteRuntimeCleanupAttempt {
                operation_id,
                expected_fence: fence,
                attempt_token: takeover_token
            }
        )
        .await
        .unwrap(),
        CompleteRuntimeCleanupResult::Replayed
    );
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applying_is_commit_decided_and_abort_requires_roll_forward_pg() {
    let (db, pool) = create_pool().await;
    let session_id = seed_session(&pool, "roll-forward").await;
    let (operation_id, fence) = create_and_fence(&pool, "roll-forward", session_id).await;
    let token = Uuid::new_v4();
    let now = Utc::now();
    begin_runtime_cleanup_attempt(
        &pool,
        BeginRuntimeCleanupAttempt {
            operation_id,
            expected_fence: fence,
            claim_owner: "worker-a",
            attempt_token: token,
            lease_expires_at: now + chrono::Duration::minutes(1),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        abort_runtime_cleanup_operation(&pool, operation_id)
            .await
            .unwrap(),
        AbortRuntimeCleanupResult::RollForwardRequired
    );
    let row = load_runtime_cleanup_operation(&pool, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, RuntimeCleanupState::Applying);
    assert!(row.commit_decided_at.is_some());
    let illegal_abort = sqlx::query(
        "UPDATE runtime_cleanup_operations
         SET state = 'aborted', claim_owner = NULL, attempt_token = NULL,
             attempt_no = 0, lease_expires_at = NULL, attempt_started_at = NULL,
             commit_decided_at = NULL,
             aborted_from_state = 'fenced', aborted_at = NOW()
         WHERE operation_id = $1",
    )
    .bind(operation_id)
    .execute(&pool)
    .await;
    assert!(illegal_abort.is_err(), "database rejects applying rollback");
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_replay_requires_exact_transition_identity_pg() {
    let (db, pool) = create_pool().await;
    let session_id = seed_session(&pool, "exact-transition").await;
    let operation_id = Uuid::new_v4();
    create_runtime_cleanup_operation(
        &pool,
        &command(operation_id, "exact-transition", session_id),
    )
    .await
    .unwrap();
    assert_eq!(
        abort_runtime_cleanup_operation(&pool, operation_id)
            .await
            .unwrap(),
        AbortRuntimeCleanupResult::Aborted
    );
    assert!(matches!(
        fence_runtime_cleanup_operation(&pool, operation_id)
            .await
            .unwrap(),
        FenceRuntimeCleanupResult::Stale {
            state: RuntimeCleanupState::Aborted
        }
    ));
    let fake_attempt = BeginRuntimeCleanupAttempt {
        operation_id,
        expected_fence: 1,
        claim_owner: "worker",
        attempt_token: Uuid::new_v4(),
        lease_expires_at: Utc::now() + chrono::Duration::minutes(1),
    };
    assert!(matches!(
        begin_runtime_cleanup_attempt(&pool, fake_attempt)
            .await
            .unwrap(),
        BeginRuntimeCleanupResult::Stale {
            state: RuntimeCleanupState::Aborted
        }
    ));
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_retention_archives_plan_and_preserves_latest_fence_pg() {
    let (db, pool) = create_pool().await;
    let session_id = seed_session(&pool, "retention").await;
    let (operation_id, fence) = create_and_fence(&pool, "retention", session_id).await;
    let token = Uuid::new_v4();
    let now = Utc::now();
    begin_runtime_cleanup_attempt(
        &pool,
        BeginRuntimeCleanupAttempt {
            operation_id,
            expected_fence: fence,
            claim_owner: "retention-worker",
            attempt_token: token,
            lease_expires_at: now + chrono::Duration::minutes(1),
        },
    )
    .await
    .unwrap();
    complete_runtime_cleanup_attempt(
        &pool,
        CompleteRuntimeCleanupAttempt {
            operation_id,
            expected_fence: fence,
            attempt_token: token,
        },
    )
    .await
    .unwrap();

    assert!(
        retire_terminal_runtime_cleanup_operation(
            &pool,
            operation_id,
            Utc::now() + chrono::Duration::seconds(1)
        )
        .await
        .unwrap()
    );
    assert!(
        load_runtime_cleanup_operation(&pool, operation_id)
            .await
            .unwrap()
            .is_none()
    );
    let archived_intents = sqlx::query_scalar::<_, i32>("SELECT JSONB_ARRAY_LENGTH(intents) FROM runtime_cleanup_operation_archive WHERE operation_id = $1")
        .bind(operation_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(archived_intents, 6);
    let authority = sqlx::query_as::<_, (i64, Uuid)>(
        "SELECT last_fence, operation_id FROM runtime_cleanup_fences WHERE target_session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(authority, (fence, operation_id));
    assert_eq!(
        create_runtime_cleanup_operation(&pool, &command(Uuid::new_v4(), "retention", session_id))
            .await
            .unwrap(),
        CreateRuntimeCleanupResult::Replayed { operation_id }
    );
    let different_session = seed_session(&pool, "retention-conflict").await;
    assert_eq!(
        create_runtime_cleanup_operation(
            &pool,
            &command(Uuid::new_v4(), "retention", different_session)
        )
        .await
        .unwrap(),
        CreateRuntimeCleanupResult::RequestConflict { operation_id }
    );

    let second_id = Uuid::new_v4();
    create_runtime_cleanup_operation(&pool, &command(second_id, "retention-next", session_id))
        .await
        .unwrap();
    assert_eq!(
        fence_runtime_cleanup_operation(&pool, second_id)
            .await
            .unwrap(),
        FenceRuntimeCleanupResult::Advanced { fence: fence + 1 }
    );
    assert!(
        !retire_terminal_runtime_cleanup_operation(
            &pool,
            second_id,
            Utc::now() + chrono::Duration::days(1)
        )
        .await
        .unwrap(),
        "active operation cannot be retired"
    );
    close(db, pool).await;
}
