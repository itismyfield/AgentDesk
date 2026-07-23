//! Tests for `spawns`.
//!
//! Split out of `spawns.rs` (#4735): the restart persistence barrier and
//! cancellation Drop-guard coverage pushed the file past the 700-LoC
//! namespace cap. Production logic is unchanged; only these tests moved.

use super::*;

#[tokio::test]
async fn standby_marker_fences_intake_exposes_ack_and_counts_shutdown_once() {
    let registry = health::HealthRegistry::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.restart.shutdown_remaining.store(1, Ordering::SeqCst);
    registry
        .register_standby("codex".to_string(), shared.clone())
        .await;

    let execute_started = Arc::new(tokio::sync::Notify::new());
    let execute_release = Arc::new(tokio::sync::Notify::new());
    let shared_for_worker = shared.clone();
    let started_for_worker = execute_started.clone();
    let release_for_worker = execute_release.clone();
    let worker = tokio::spawn(async move {
        let _active_tick = shared_for_worker
            .restart
            .intake_worker_lifecycle
            .try_begin_tick()
            .expect("tick admitted before restart fence");
        started_for_worker.notify_one();
        release_for_worker.notified().await;
    });
    execute_started.notified().await;

    let shared_for_prepare = shared.clone();
    let prepare = tokio::spawn(async move {
        prepare_deferred_restart(
            &shared_for_prepare,
            std::path::Path::new("/nonexistent"),
            "test-nonce".to_owned(),
        )
        .await
        .map(|(permit, mut guard)| {
            guard.disarm();
            permit
        })
    });
    while !shared.restart.shutting_down.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }

    assert!(
        !shared.restart.restart_pending.load(Ordering::Acquire),
        "health must not acknowledge while the accepted execute future is active"
    );
    assert_eq!(
        shared.restart.shutdown_remaining.load(Ordering::Acquire),
        1,
        "the shutdown token must remain unconsumed while execute is active"
    );
    assert!(begin_deferred_restart(&shared).is_none());

    execute_release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(1), worker)
        .await
        .expect("accepted execute drain")
        .expect("worker join");
    let permit = tokio::time::timeout(std::time::Duration::from_secs(1), prepare)
        .await
        .expect("marker acknowledgement after execute drain")
        .expect("prepare join")
        .expect("first marker acknowledgement");

    let snapshot = serde_json::to_value(health::build_health_snapshot(&registry).await)
        .expect("serialize acknowledged standby health");
    assert_eq!(snapshot["providers"][0]["restart_pending"], true);
    assert!(shared.restart.shutting_down.load(Ordering::Acquire));

    assert!(finish_deferred_restart(&shared, permit));
    assert_eq!(
        shared.restart.shutdown_remaining.load(Ordering::Acquire),
        0,
        "the standby provider consumes its barrier slot exactly once"
    );
}

#[test]
fn cancellation_guard_rolls_back_consumed_slot_when_cancel_arrives_after_finish() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.restart.shutdown_remaining.store(2, Ordering::SeqCst);
    let root = tempfile::tempdir().expect("runtime root");
    let nonce = "timeout-during-await";
    std::fs::write(
        root.path().join("restart_pending"),
        format!("nonce={nonce}\n"),
    )
    .expect("restart request");

    let permit = begin_deferred_restart(&shared).expect("restart permit");
    assert!(!finish_deferred_restart(&shared, permit));
    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 1);

    let guard = DeferredRestartCancellationGuard::new(
        shared.clone(),
        root.path().to_path_buf(),
        nonce.to_owned(),
    );
    // Cancellation publication precedes request removal, so Drop can
    // always distinguish the cancellation handoff from a new request.
    std::fs::write(
        root.path().join("restart_cancelled"),
        format!("nonce={nonce}\n"),
    )
    .expect("publish cancellation during persistence await");
    std::fs::remove_file(root.path().join("restart_pending")).expect("remove request");
    drop(guard);

    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 2);
    assert!(!shared.restart.intake_worker_lifecycle.admission_is_fenced());
    assert!(!shared.restart.shutting_down.load(Ordering::Acquire));
    assert!(!shared.restart.restart_pending.load(Ordering::Acquire));
    assert!(!shared.restart.shutdown_counted.load(Ordering::Acquire));
}

#[tokio::test]
async fn cancellation_during_prepare_drain_drops_guard_and_restores_admission() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let root = tempfile::tempdir().expect("runtime root");
    let nonce = "prepare-await-race";
    std::fs::write(
        root.path().join("restart_pending"),
        format!("nonce={nonce}\n"),
    )
    .expect("restart request");
    let tick = shared
        .restart
        .intake_worker_lifecycle
        .try_begin_tick()
        .expect("admitted tick");
    let shared_for_prepare = shared.clone();
    let root_for_prepare = root.path().to_path_buf();
    let prepare = tokio::spawn(async move {
        prepare_deferred_restart(&shared_for_prepare, &root_for_prepare, nonce.to_owned()).await
    });
    while !shared.restart.shutting_down.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }

    // Mirror the timeout helper's safe handoff while prepare is awaiting
    // its active tick drain: cancellation publishes before marker removal.
    std::fs::write(
        root.path().join("restart_cancelled"),
        format!("nonce={nonce}\n"),
    )
    .expect("publish cancellation");
    std::fs::remove_file(root.path().join("restart_pending")).expect("remove request");
    drop(tick);
    assert!(prepare.await.expect("prepare join").is_none());
    assert!(!shared.restart.intake_worker_lifecycle.admission_is_fenced());
    assert!(!shared.restart.shutting_down.load(Ordering::Acquire));
    assert!(!shared.restart.restart_pending.load(Ordering::Acquire));
    assert!(!shared.restart.shutdown_counted.load(Ordering::Acquire));
}

#[test]
fn cancellation_after_sentinel_write_before_rename_rolls_back() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.restart.shutdown_remaining.store(1, Ordering::SeqCst);
    let root = tempfile::tempdir().expect("runtime root");
    let nonce = "rename-boundary";
    std::fs::write(
        root.path().join("restart_pending"),
        format!("nonce={nonce}\n"),
    )
    .expect("restart request");
    let permit = begin_deferred_restart(&shared).expect("restart permit");
    assert!(finish_deferred_restart(&shared, permit));
    let guard = DeferredRestartCancellationGuard::new(
        shared.clone(),
        root.path().to_path_buf(),
        nonce.to_owned(),
    );
    // This models timeout publication after sentinel staging but before
    // rename. The production helper must observe it after writing its tmp
    // sentinel and leave no committed acknowledgement behind.
    std::fs::write(
        root.path().join("restart_cancelled"),
        format!("nonce={nonce}\n"),
    )
    .expect("cancel before rename");
    assert!(
        !commit_deferred_restart_sentinel(root.path(), &ProviderKind::Codex, nonce, &guard,)
            .expect("sentinel staging")
    );
    assert!(!root.path().join("restart_persisted").exists());
    drop(guard);
    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 1);
    assert!(!shared.restart.intake_worker_lifecycle.admission_is_fenced());
}

#[test]
fn cancellation_before_durable_commit_rolls_back_but_after_commit_stays_committed() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.restart.shutdown_remaining.store(1, Ordering::SeqCst);
    let root = tempfile::tempdir().expect("runtime root");
    let nonce = "commit-boundary";
    std::fs::write(
        root.path().join("restart_pending"),
        format!("nonce={nonce}\n"),
    )
    .expect("restart request");

    let permit = begin_deferred_restart(&shared).expect("restart permit");
    let guard = DeferredRestartCancellationGuard::new(
        shared.clone(),
        root.path().to_path_buf(),
        nonce.to_owned(),
    );
    std::fs::write(
        root.path().join("restart_cancelled"),
        format!("nonce={nonce}\n"),
    )
    .expect("cancel before commit");
    assert!(
        guard.cancelled(),
        "commit boundary rejects pre-commit cancellation"
    );
    drop(guard);
    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 1);
    assert!(!shared.restart.intake_worker_lifecycle.admission_is_fenced());
    drop(permit);

    std::fs::remove_file(root.path().join("restart_cancelled")).expect("clear cancellation");
    let permit = begin_deferred_restart(&shared).expect("new restart permit");
    let mut guard = DeferredRestartCancellationGuard::new(
        shared.clone(),
        root.path().to_path_buf(),
        nonce.to_owned(),
    );
    assert!(finish_deferred_restart(&shared, permit));
    std::fs::write(
        root.path().join("restart_persisted"),
        format!("nonce={nonce}\n"),
    )
    .expect("durable sentinel");
    guard.disarm();
    std::fs::write(
        root.path().join("restart_cancelled"),
        format!("nonce={nonce}\n"),
    )
    .expect("late cancellation");
    drop(guard);
    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 0);
    assert!(shared.restart.intake_worker_lifecycle.admission_is_fenced());
    assert!(shared.restart.shutting_down.load(Ordering::Acquire));
}

#[test]
fn superseded_owner_releases_slot_but_preserves_fence_for_next_nonce() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.restart.shutdown_remaining.store(2, Ordering::SeqCst);
    let permit = begin_deferred_restart(&shared).expect("A owns provider bookkeeping");
    assert!(!finish_deferred_restart(&shared, permit));
    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 1);

    handoff_superseded_restart(&shared);

    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 2);
    assert!(!shared.restart.shutdown_counted.load(Ordering::Acquire));
    assert!(
        !shared
            .restart
            .shutdown_slot_consumed
            .load(Ordering::Acquire)
    );
    assert!(shared.restart.intake_worker_lifecycle.admission_is_fenced());
    assert!(shared.restart.shutting_down.load(Ordering::Acquire));
    shared
        .restart
        .restart_pending
        .store(true, Ordering::Release);
    assert!(shared.restart.restart_pending.load(Ordering::Acquire));
    assert!(
        begin_deferred_restart(&shared).is_some(),
        "B must be able to acquire provider bookkeeping after A supersedes"
    );
}

#[test]
fn stale_nonce_cannot_commit_persistence_or_remove_newer_marker() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let root = tempfile::tempdir().expect("runtime root");
    std::fs::write(
        root.path().join("restart_pending"),
        "nonce=deploy-b\nsource=deploy\n",
    )
    .expect("new owner marker");
    let guard = DeferredRestartCancellationGuard::new(
        shared,
        root.path().to_path_buf(),
        "promotion-a".to_string(),
    );

    assert!(
        !commit_deferred_restart_sentinel(
            root.path(),
            &ProviderKind::Codex,
            "promotion-a",
            &guard,
        )
        .expect("stale commit check")
    );
    assert!(!root.path().join("restart_persisted").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("restart_pending")).expect("new marker survives"),
        "nonce=deploy-b\nsource=deploy\n"
    );
}

#[tokio::test]
async fn cancellation_restores_admission_health_and_consumed_barrier_slot() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.restart.shutdown_remaining.store(2, Ordering::SeqCst);

    let permit = prepare_deferred_restart(
        &shared,
        std::path::Path::new("/nonexistent"),
        "test-nonce".to_owned(),
    )
    .await
    .map(|(permit, mut guard)| {
        guard.disarm();
        permit
    })
    .expect("first restart permit");
    assert!(!finish_deferred_restart(&shared, permit));
    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 1);
    assert!(shared.restart.intake_worker_lifecycle.admission_is_fenced());
    assert!(shared.restart.shutting_down.load(Ordering::Acquire));
    assert!(shared.restart.restart_pending.load(Ordering::Acquire));

    rollback_deferred_restart(&shared);

    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 2);
    assert!(!shared.restart.intake_worker_lifecycle.admission_is_fenced());
    assert!(!shared.restart.shutting_down.load(Ordering::Acquire));
    assert!(!shared.restart.restart_pending.load(Ordering::Acquire));
    assert!(!shared.restart.shutdown_counted.load(Ordering::Acquire));

    let second_permit = prepare_deferred_restart(
        &shared,
        std::path::Path::new("/nonexistent"),
        "test-nonce".to_owned(),
    )
    .await
    .map(|(permit, mut guard)| {
        guard.disarm();
        permit
    })
    .expect("restart permit after cancellation");
    assert!(!finish_deferred_restart(&shared, second_permit));
    assert_eq!(
        shared.restart.shutdown_remaining.load(Ordering::Acquire),
        1,
        "a new request consumes exactly one restored barrier slot"
    );
}
