use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;

fn intents() -> Vec<RuntimeCleanupIntent> {
    vec![
        RuntimeCleanupIntent {
            kind: RuntimeCleanupIntentKind::BlockRuntimeAdmission,
            ordinal: 1,
        },
        RuntimeCleanupIntent {
            kind: RuntimeCleanupIntentKind::ClearQueuedInput,
            ordinal: 2,
        },
        RuntimeCleanupIntent {
            kind: RuntimeCleanupIntentKind::ExpireRuntimeLease,
            ordinal: 3,
        },
        RuntimeCleanupIntent {
            kind: RuntimeCleanupIntentKind::CancelActiveRuntime,
            ordinal: 4,
        },
        RuntimeCleanupIntent {
            kind: RuntimeCleanupIntentKind::ClearPersistedSession,
            ordinal: 5,
        },
        RuntimeCleanupIntent {
            kind: RuntimeCleanupIntentKind::ReleaseRuntimeSlot,
            ordinal: 6,
        },
    ]
}

fn command<'a>(
    operation_id: Uuid,
    request_key: &'a str,
    target_key: &'a str,
    intents: &'a [RuntimeCleanupIntent],
) -> CreateRuntimeCleanupOperation<'a> {
    CreateRuntimeCleanupOperation {
        operation_id,
        request_key,
        target_kind: RuntimeCleanupTargetKind::DiscordSession,
        target_key,
        action: RuntimeCleanupAction::ClearForResume,
        intents,
    }
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_is_atomic_and_exact_replay_is_idempotent_pg() {
    let (db, pool) = create_pool().await;
    let operation_id = Uuid::new_v4();
    let expected_intents = intents();
    let create = command(
        operation_id,
        "request-atomic",
        "session-atomic",
        &expected_intents,
    );

    assert_eq!(
        create_runtime_cleanup_operation(&pool, &create)
            .await
            .unwrap(),
        CreateRuntimeCleanupResult::Created { operation_id }
    );
    assert_eq!(
        create_runtime_cleanup_operation(&pool, &create)
            .await
            .unwrap(),
        CreateRuntimeCleanupResult::Replayed { operation_id }
    );

    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM runtime_cleanup_intents WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, expected_intents.len() as i64);

    let conflicting_intents = &expected_intents[..5];
    let conflict = command(
        Uuid::new_v4(),
        "request-atomic",
        "session-atomic",
        conflicting_intents,
    );
    assert_eq!(
        create_runtime_cleanup_operation(&pool, &conflict)
            .await
            .unwrap(),
        CreateRuntimeCleanupResult::RequestConflict { operation_id }
    );
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_target_has_one_deterministic_owner_pg() {
    let (db, pool) = create_pool().await;
    let first_id = Uuid::from_u128(1);
    let second_id = Uuid::from_u128(2);
    let expected_intents = intents();
    let first = command(
        first_id,
        "request-writer-a",
        "session-shared",
        &expected_intents,
    );
    let second = command(
        second_id,
        "request-writer-b",
        "session-shared",
        &expected_intents,
    );
    let (first_result, second_result) = tokio::join!(
        create_runtime_cleanup_operation(&pool, &first),
        create_runtime_cleanup_operation(&pool, &second),
    );
    let first_result = first_result.unwrap();
    let second_result = second_result.unwrap();

    let owner = match (&first_result, &second_result) {
        (
            CreateRuntimeCleanupResult::Created { operation_id },
            CreateRuntimeCleanupResult::TargetBusy {
                operation_id: busy_id,
            },
        )
        | (
            CreateRuntimeCleanupResult::TargetBusy {
                operation_id: busy_id,
            },
            CreateRuntimeCleanupResult::Created { operation_id },
        ) => {
            assert_eq!(operation_id, busy_id);
            *operation_id
        }
        other => panic!("expected one owner and one busy result, got {other:?}"),
    };

    let open = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM runtime_cleanup_operations
         WHERE target_kind = 'discord_session' AND target_key = 'session-shared'
           AND state IN ('pending', 'fenced', 'applying')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open, 1);
    assert!(owner == first_id || owner == second_id);
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transitions_are_monotonic_fenced_and_idempotent_pg() {
    let (db, pool) = create_pool().await;
    let operation_id = Uuid::new_v4();
    let expected_intents = intents();
    create_runtime_cleanup_operation(
        &pool,
        &command(
            operation_id,
            "request-transition",
            "session-transition",
            &expected_intents,
        ),
    )
    .await
    .unwrap();

    let fenced = transition_runtime_cleanup_operation(
        &pool,
        operation_id,
        RuntimeCleanupState::Pending,
        None,
        RuntimeCleanupState::Fenced,
    )
    .await
    .unwrap();
    let fence = match fenced {
        TransitionRuntimeCleanupResult::Advanced {
            state: RuntimeCleanupState::Fenced,
            fence: Some(fence),
            ..
        } => fence,
        other => panic!("unexpected fence result: {other:?}"),
    };
    assert_eq!(fence, 1);

    assert_eq!(
        transition_runtime_cleanup_operation(
            &pool,
            operation_id,
            RuntimeCleanupState::Pending,
            Some(fence),
            RuntimeCleanupState::Fenced,
        )
        .await
        .unwrap(),
        TransitionRuntimeCleanupResult::Replayed {
            operation_id,
            state: RuntimeCleanupState::Fenced,
            fence: Some(fence),
        }
    );
    assert!(matches!(
        transition_runtime_cleanup_operation(
            &pool,
            operation_id,
            RuntimeCleanupState::Fenced,
            Some(fence),
            RuntimeCleanupState::Completed,
        )
        .await
        .unwrap(),
        TransitionRuntimeCleanupResult::Illegal {
            current_state: RuntimeCleanupState::Fenced,
            ..
        }
    ));
    assert!(matches!(
        transition_runtime_cleanup_operation(
            &pool,
            operation_id,
            RuntimeCleanupState::Fenced,
            Some(fence + 1),
            RuntimeCleanupState::Applying,
        )
        .await
        .unwrap(),
        TransitionRuntimeCleanupResult::Stale {
            current_fence: Some(current),
            ..
        } if current == fence
    ));

    assert!(matches!(
        transition_runtime_cleanup_operation(
            &pool,
            operation_id,
            RuntimeCleanupState::Fenced,
            Some(fence),
            RuntimeCleanupState::Applying,
        )
        .await
        .unwrap(),
        TransitionRuntimeCleanupResult::Advanced {
            state: RuntimeCleanupState::Applying,
            ..
        }
    ));
    assert!(matches!(
        transition_runtime_cleanup_operation(
            &pool,
            operation_id,
            RuntimeCleanupState::Applying,
            Some(fence),
            RuntimeCleanupState::Completed,
        )
        .await
        .unwrap(),
        TransitionRuntimeCleanupResult::Advanced {
            state: RuntimeCleanupState::Completed,
            ..
        }
    ));
    assert!(matches!(
        transition_runtime_cleanup_operation(
            &pool,
            operation_id,
            RuntimeCleanupState::Completed,
            Some(fence),
            RuntimeCleanupState::Applying,
        )
        .await
        .unwrap(),
        TransitionRuntimeCleanupResult::Illegal {
            current_state: RuntimeCleanupState::Completed,
            ..
        }
    ));
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_fence_writers_serialize_and_next_operation_advances_epoch_pg() {
    let (db, pool) = create_pool().await;
    let expected_intents = intents();
    let first_id = Uuid::new_v4();
    create_runtime_cleanup_operation(
        &pool,
        &command(
            first_id,
            "request-fence-1",
            "session-fence",
            &expected_intents,
        ),
    )
    .await
    .unwrap();

    let (left, right) = tokio::join!(
        transition_runtime_cleanup_operation(
            &pool,
            first_id,
            RuntimeCleanupState::Pending,
            None,
            RuntimeCleanupState::Fenced,
        ),
        transition_runtime_cleanup_operation(
            &pool,
            first_id,
            RuntimeCleanupState::Pending,
            None,
            RuntimeCleanupState::Fenced,
        ),
    );
    let results = [left.unwrap(), right.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, TransitionRuntimeCleanupResult::Advanced { .. }))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, TransitionRuntimeCleanupResult::Replayed { .. }))
            .count(),
        1
    );
    assert!(results.iter().all(|result| match result {
        TransitionRuntimeCleanupResult::Advanced { fence, .. }
        | TransitionRuntimeCleanupResult::Replayed { fence, .. } => *fence == Some(1),
        _ => false,
    }));

    transition_runtime_cleanup_operation(
        &pool,
        first_id,
        RuntimeCleanupState::Fenced,
        Some(1),
        RuntimeCleanupState::Aborted,
    )
    .await
    .unwrap();
    let second_id = Uuid::new_v4();
    assert_eq!(
        create_runtime_cleanup_operation(
            &pool,
            &command(
                second_id,
                "request-fence-2",
                "session-fence",
                &expected_intents,
            ),
        )
        .await
        .unwrap(),
        CreateRuntimeCleanupResult::Created {
            operation_id: second_id
        }
    );
    assert!(matches!(
        transition_runtime_cleanup_operation(
            &pool,
            second_id,
            RuntimeCleanupState::Pending,
            None,
            RuntimeCleanupState::Fenced,
        )
        .await
        .unwrap(),
        TransitionRuntimeCleanupResult::Advanced { fence: Some(2), .. }
    ));
    close(db, pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn database_constraints_reject_mutable_intents_and_invalid_state_shape_pg() {
    let (db, pool) = create_pool().await;
    let operation_id = Uuid::new_v4();
    let expected_intents = intents();
    create_runtime_cleanup_operation(
        &pool,
        &command(
            operation_id,
            "request-constraints",
            "session-constraints",
            &expected_intents,
        ),
    )
    .await
    .unwrap();

    let immutable = sqlx::query(
        "UPDATE runtime_cleanup_intents SET ordinal = 7
         WHERE operation_id = $1 AND intent_kind = 'block_runtime_admission'",
    )
    .bind(operation_id)
    .execute(&pool)
    .await;
    assert!(immutable.is_err());

    let invalid_shape = sqlx::query(
        "UPDATE runtime_cleanup_operations SET state = 'completed' WHERE operation_id = $1",
    )
    .bind(operation_id)
    .execute(&pool)
    .await;
    assert!(invalid_shape.is_err());
    close(db, pool).await;
}
