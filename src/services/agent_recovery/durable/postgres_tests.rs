use std::time::Duration;

use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;
use crate::services::agent_recovery::tests::{CHANNEL, enabled_runtime};

fn runtime(pool: &PgPool) -> Arc<Coordinator> {
    Arc::new(Coordinator {
        runtime: Mutex::new(enabled_runtime()),
        pool: Mutex::new(Some(pool.clone())),
        ..Coordinator::default()
    })
}

async fn fixture() -> (DispatchPostgresTestDb, PgPool, Arc<Coordinator>) {
    let db = DispatchPostgresTestDb::create("agent_recovery_review", "agent recovery").await;
    let pool = db.connect_and_migrate_with_max_connections(5).await;
    let coordinator = runtime(&pool);
    (db, pool, coordinator)
}

fn payload(progress: &str) -> CheckpointPayload {
    CheckpointPayload::compact("owner", "recover", progress, "", Vec::new(), "continue", "")
}

fn takeover(
    runtime: &mut RecoveryRuntime,
    turn: &str,
) -> Result<Option<ObserveOutcome>, RecoveryStoreError> {
    let outcome = runtime.observe(ObserveInput {
        channel_id: CHANNEL.to_string(),
        primary_turn_id: turn.to_string(),
        signal: DetectorSignal::StreamIdleTimeout,
    });
    Ok(outcome.spawn.is_some().then_some(outcome))
}

async fn seed_owner(coordinator: &Coordinator) {
    coordinator
        .transition(CHANNEL, |runtime| {
            runtime
                .note_owner_progress(CHANNEL, payload("owner progress"))
                .map(|event| event.map(|_| ()))
                .map_err(|error| conflict(error.message()))
        })
        .await
        .unwrap();
}

fn cached_state(coordinator: &Coordinator) -> ChannelState {
    lock(&coordinator.runtime)
        .states
        .get(CHANNEL)
        .unwrap()
        .clone()
}

#[tokio::test]
async fn postgres_takeover_is_invisible_until_commit() {
    let (db, pool, coordinator) = fixture().await;
    seed_owner(&coordinator).await;
    let before = cached_state(&coordinator);
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT channel_id FROM agent_recovery_channel_state WHERE channel_id = $1 FOR UPDATE",
    )
    .bind(CHANNEL)
    .fetch_one(&mut *blocker)
    .await
    .unwrap();
    let worker = Arc::clone(&coordinator);
    let (staged_tx, staged_rx) = tokio::sync::oneshot::channel();
    let transition = tokio::spawn(async move {
        worker
            .transition(CHANNEL, |runtime| {
                let outcome = takeover(runtime, "turn-1");
                staged_tx.send(()).unwrap();
                outcome
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), staged_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached_state(&coordinator), before);
    assert!(lock(&coordinator.runtime).allows_cli_turn(CHANNEL, "claude"));
    blocker.rollback().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), transition)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(outcome.spawn.is_some());
    assert_eq!(
        cached_state(&coordinator).status,
        ChannelRecoveryStatus::TakeoverPending
    );
    assert_eq!(
        load_channel_state(&pool, CHANNEL).await.unwrap().unwrap(),
        cached_state(&coordinator)
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_failed_wal_append_never_publishes_the_staged_takeover() {
    let (db, pool, coordinator) = fixture().await;
    seed_owner(&coordinator).await;
    let before = cached_state(&coordinator);
    sqlx::raw_sql(
        "CREATE FUNCTION reject_recovery_stall() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected WAL failure'; END $$;
         CREATE TRIGGER reject_recovery_stall BEFORE INSERT ON agent_recovery_checkpoint_events
         FOR EACH ROW WHEN (NEW.kind = 'stall') EXECUTE FUNCTION reject_recovery_stall();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let result = coordinator
        .transition(CHANNEL, |runtime| takeover(runtime, "turn-1"))
        .await;
    assert!(result.is_err());
    assert_eq!(cached_state(&coordinator), before);
    assert_eq!(
        load_channel_state(&pool, CHANNEL).await.unwrap().unwrap(),
        before
    );
    assert_eq!(
        load_checkpoint_events(&pool, CHANNEL, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_competing_coordinators_yield_only_one_spawn_plan() {
    let (db, pool, first) = fixture().await;
    let second = runtime(&pool);
    let (left, right) = tokio::join!(
        first.transition(CHANNEL, |runtime| takeover(runtime, "left")),
        second.transition(CHANNEL, |runtime| takeover(runtime, "right")),
    );
    let winners = [&left, &right]
        .iter()
        .filter(|result| matches!(result, Ok(Some(_))))
        .count();
    assert_eq!(winners, 1, "two independent runtimes must not both spawn");
    assert_eq!(
        load_channel_state(&pool, CHANNEL)
            .await
            .unwrap()
            .unwrap()
            .generation,
        1
    );
    assert_eq!(
        load_checkpoint_events(&pool, CHANNEL, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_stale_completion_cannot_claim_a_restarted_takeover() {
    let (db, pool, first) = fixture().await;
    first
        .transition(CHANNEL, |runtime| takeover(runtime, "turn-1"))
        .await
        .unwrap()
        .unwrap();
    let original_lease = RecoveryLease::from_state(&cached_state(&first));
    first
        .transition(CHANNEL, |runtime| {
            apply_completion(runtime, &original_lease, payload("done"))
        })
        .await
        .unwrap()
        .unwrap();
    first
        .transition(CHANNEL, |runtime| {
            Ok(runtime.try_restore_owner(CHANNEL, &ProviderKind::Grok, true, false))
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached_state(&first).generation, 2);
    let restore_lease = RecoveryLease::from_state(&cached_state(&first));
    first
        .transition(CHANNEL, |runtime| runtime.acknowledge_start(&restore_lease))
        .await
        .unwrap();
    // Start with an empty cache, simulating a restart after a non-locked state.
    let restarted = runtime(&pool);
    restarted
        .transition(CHANNEL, |runtime| takeover(runtime, "turn-2"))
        .await
        .unwrap()
        .unwrap();
    let current = cached_state(&restarted);
    assert_eq!(current.generation, 3);
    assert!(
        restarted
            .transition(CHANNEL, |runtime| {
                apply_completion(runtime, &original_lease, payload("late old completion"))
            })
            .await
            .is_err()
    );
    assert_eq!(cached_state(&restarted), current);
    assert_eq!(
        load_channel_state(&pool, CHANNEL).await.unwrap().unwrap(),
        current
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_wal_frontier_rejects_same_generation_stale_progress() {
    let (db, pool, coordinator) = fixture().await;
    seed_owner(&coordinator).await;
    let mut stale = coordinator.snapshot(&pool, CHANNEL).await.unwrap();
    let before = stale.states.get(CHANNEL).unwrap().clone();
    let event = stale
        .note_owner_progress(CHANNEL, payload("stale"))
        .unwrap()
        .unwrap();
    seed_owner(&coordinator).await;
    let state = stale.states.get(CHANNEL).unwrap();
    let result = commit_recovery_transition(
        &pool,
        state,
        &[event],
        RecoveryTransition {
            expected_generation: before.generation,
            expected_next_seq: before.next_seq,
            expected_writer_agent_id: Some(&before.active_writer_agent_id),
            allowed_statuses: &[ChannelRecoveryStatus::Owner],
        },
    )
    .await;
    assert!(matches!(result, Err(RecoveryStoreError::Conflict(_))));
    assert_eq!(
        load_channel_state(&pool, CHANNEL)
            .await
            .unwrap()
            .unwrap()
            .next_seq,
        2
    );
    assert_eq!(
        load_checkpoint_events(&pool, CHANNEL, 10)
            .await
            .unwrap()
            .len(),
        2
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_mailbox_admission_serializes_takeover_before_a_state_row_exists() {
    use crate::services::agent_recovery::admission::admit_on;
    let (db, pool, first) = fixture().await;
    let guard = admit_on(&first, CHANNEL, &ProviderKind::Grok, None, None)
        .await
        .unwrap();
    let second = runtime(&pool);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move {
        second
            .transition(CHANNEL, |runtime| {
                ready_tx.send(()).unwrap();
                takeover(runtime, "racing-turn")
            })
            .await
    });
    ready_rx.await.unwrap();
    assert!(load_channel_state(&pool, CHANNEL).await.unwrap().is_none());
    assert!(!worker.is_finished());
    drop(guard);
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        admit_on(&first, CHANNEL, &ProviderKind::Grok, None, None)
            .await
            .is_err()
    );
    assert!(
        admit_on(&first, CHANNEL, &ProviderKind::Codex, None, None)
            .await
            .is_err()
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_pending_launch_and_restore_require_the_exact_internal_lease() {
    use crate::services::agent_recovery::admission::admit_on;
    let (db, pool, coordinator) = fixture().await;
    coordinator
        .transition(CHANNEL, |runtime| takeover(runtime, "turn"))
        .await
        .unwrap()
        .unwrap();
    let fallback = RecoveryLease::from_state(&cached_state(&coordinator));
    assert!(
        admit_on(
            &coordinator,
            CHANNEL,
            &ProviderKind::Grok,
            None,
            Some(&fallback)
        )
        .await
        .is_err()
    );
    assert!(
        admit_on(
            &coordinator,
            CHANNEL,
            &ProviderKind::Codex,
            Some("another-agent"),
            Some(&fallback)
        )
        .await
        .is_err()
    );
    drop(
        admit_on(
            &coordinator,
            CHANNEL,
            &ProviderKind::Codex,
            Some("monitoring"),
            Some(&fallback),
        )
        .await
        .unwrap(),
    );
    coordinator
        .transition(CHANNEL, |runtime| runtime.acknowledge_start(&fallback))
        .await
        .unwrap();
    drop(
        admit_on(
            &coordinator,
            CHANNEL,
            &ProviderKind::Codex,
            Some("monitoring"),
            None,
        )
        .await
        .unwrap(),
    );
    coordinator
        .transition(CHANNEL, |runtime| {
            apply_completion(runtime, &fallback, payload("fallback done"))
        })
        .await
        .unwrap();
    assert!(
        admit_on(
            &coordinator,
            CHANNEL,
            &ProviderKind::Codex,
            None,
            Some(&fallback)
        )
        .await
        .is_err()
    );
    coordinator
        .transition(CHANNEL, |runtime| {
            Ok(runtime.try_restore_owner(CHANNEL, &ProviderKind::Grok, true, false))
        })
        .await
        .unwrap()
        .unwrap();
    let owner = RecoveryLease::from_state(&cached_state(&coordinator));
    assert_eq!(
        cached_state(&coordinator).status,
        ChannelRecoveryStatus::RestorePending
    );
    assert!(
        admit_on(&coordinator, CHANNEL, &ProviderKind::Grok, None, None)
            .await
            .is_err()
    );
    assert!(
        admit_on(
            &coordinator,
            CHANNEL,
            &ProviderKind::Codex,
            None,
            Some(&fallback)
        )
        .await
        .is_err()
    );
    drop(
        admit_on(
            &coordinator,
            CHANNEL,
            &ProviderKind::Grok,
            Some("claude"),
            Some(&owner),
        )
        .await
        .unwrap(),
    );
    coordinator
        .transition(CHANNEL, |runtime| runtime.acknowledge_start(&owner))
        .await
        .unwrap();
    assert_eq!(
        cached_state(&coordinator).status,
        ChannelRecoveryStatus::Restored
    );
    drop(
        admit_on(&coordinator, CHANNEL, &ProviderKind::Grok, None, None)
            .await
            .unwrap(),
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_restart_and_catalog_removal_preserve_the_frozen_recovery_fence() {
    use crate::services::agent_recovery::admission::admit_on;
    let (db, pool, first) = fixture().await;
    first
        .transition(CHANNEL, |runtime| takeover(runtime, "turn"))
        .await
        .unwrap()
        .unwrap();
    let old = RecoveryLease::from_state(&cached_state(&first));
    first
        .transition(CHANNEL, |runtime| runtime.acknowledge_start(&old))
        .await
        .unwrap();
    let restarted = runtime(&pool);
    lock(&restarted.runtime).clear_catalog();
    restarted
        .transition(CHANNEL, |runtime| runtime.retry_interrupted_launch(CHANNEL))
        .await
        .unwrap()
        .unwrap();
    let state = cached_state(&restarted);
    assert_eq!(state.status, ChannelRecoveryStatus::TakeoverPending);
    assert!(state.generation > old.generation);
    assert_eq!(
        lock(&restarted.runtime)
            .inherit_workspace(CHANNEL)
            .as_deref(),
        Some("/primary-workspace")
    );
    assert_eq!(
        lock(&restarted.runtime).fallback_provider(CHANNEL),
        Some(ProviderKind::Codex)
    );
    assert!(
        admit_on(&restarted, CHANNEL, &ProviderKind::Grok, None, None)
            .await
            .is_err()
    );
    assert!(
        admit_on(&restarted, CHANNEL, &ProviderKind::Codex, None, Some(&old))
            .await
            .is_err()
    );
    assert!(
        restarted
            .transition(CHANNEL, |runtime| apply_completion(
                runtime,
                &old,
                payload("late")
            ))
            .await
            .is_err()
    );
    assert_eq!(
        load_channel_state(&pool, CHANNEL).await.unwrap().unwrap(),
        state
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_failed_launch_ack_keeps_a_retryable_pending_intent() {
    let (db, pool, coordinator) = fixture().await;
    coordinator
        .transition(CHANNEL, |runtime| takeover(runtime, "turn"))
        .await
        .unwrap()
        .unwrap();
    let before = cached_state(&coordinator);
    let lease = RecoveryLease::from_state(&before);
    sqlx::raw_sql(
        "CREATE FUNCTION reject_launch_ack() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected acknowledgement failure'; END $$;
         CREATE TRIGGER reject_launch_ack BEFORE INSERT ON agent_recovery_checkpoint_events
         FOR EACH ROW WHEN (NEW.payload->>'progress' = 'runtime start acknowledged')
         EXECUTE FUNCTION reject_launch_ack();",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        coordinator
            .transition(CHANNEL, |runtime| runtime.acknowledge_start(&lease))
            .await
            .is_err()
    );
    assert_eq!(cached_state(&coordinator), before);
    assert_eq!(
        load_channel_state(&pool, CHANNEL).await.unwrap().unwrap(),
        before
    );
    coordinator
        .transition(CHANNEL, |runtime| runtime.retry_interrupted_launch(CHANNEL))
        .await
        .unwrap()
        .unwrap();
    assert!(cached_state(&coordinator).generation > lease.generation);
    assert_eq!(
        cached_state(&coordinator).status,
        ChannelRecoveryStatus::TakeoverPending
    );
    pool.close().await;
    db.drop().await;
}
