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

/// #5254 D11-2: the identity name is what the commit publishes, and the index
/// is a hard link derived from it. Same inode is the whole point — a reader that
/// finds our nonce under the fixed name with no identity beside it is looking at
/// a pre-I2c publisher, and that inference only holds while this publisher never
/// writes the index by itself (ERRATUM §E5.2).
#[test]
fn commit_publishes_the_identity_and_derives_the_index_from_it() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let root = tempfile::tempdir().expect("runtime root");
    let nonce = "identity-publish";
    std::fs::write(
        root.path().join("restart_pending"),
        format!("nonce={nonce}\n"),
    )
    .expect("restart request");
    let mut guard =
        DeferredRestartCancellationGuard::new(shared, root.path().to_path_buf(), nonce.to_owned());

    assert!(
        commit_deferred_restart_sentinel(root.path(), &ProviderKind::Codex, nonce, &guard)
            .expect("durable commit")
    );
    guard.disarm();

    let identity = root.path().join(format!("restart_persisted.{nonce}"));
    let index = root.path().join("restart_persisted");
    let published = std::fs::read_to_string(&identity).expect("identity artifact");
    assert!(published.contains(&format!("nonce={nonce}\n")));
    assert_eq!(
        std::fs::read_to_string(&index).expect("fixed-name index"),
        published,
        "the index must carry the identity's body, not a body of its own"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&identity).expect("identity meta").ino(),
            std::fs::metadata(&index).expect("index meta").ino(),
            "the index must be a hard link of the identity inode"
        );
    }
}

/// ERRATUM §E5.2 corollary: a nonce that cannot spell an identity name fails the
/// commit. Publishing the index alone would be the one shape that breaks the
/// inference above, so this path publishes nothing at all.
#[test]
fn an_unsafe_nonce_publishes_nothing_and_refuses_the_commit() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let root = tempfile::tempdir().expect("runtime root");
    let nonce = "escape/../smuggled";
    std::fs::write(
        root.path().join("restart_pending"),
        format!("nonce={nonce}\n"),
    )
    .expect("restart request");
    let mut guard =
        DeferredRestartCancellationGuard::new(shared, root.path().to_path_buf(), nonce.to_owned());

    let error = commit_deferred_restart_sentinel(root.path(), &ProviderKind::Codex, nonce, &guard)
        .expect_err("an unsafe nonce must fail closed");
    guard.disarm();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        !root.path().join("restart_persisted").exists(),
        "the fixed-name index must never be published on its own"
    );
    assert_eq!(
        std::fs::read_dir(root.path())
            .expect("runtime root listing")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "restart_pending")
            .count(),
        0,
        "a refused commit leaves no artifact behind"
    );
}

/// #5254 D11-4 step (2) + [r3]-3: the identity name is the authority, so
/// disposal takes both of this request's names and repeating it is a no-op.
#[test]
fn disposal_removes_both_of_its_own_names_and_repeats_harmlessly() {
    let root = tempfile::tempdir().expect("runtime root");
    let nonce = "dispose-own";
    let identity = root.path().join(format!("restart_pending.{nonce}"));
    std::fs::write(&identity, format!("nonce={nonce}\n")).expect("identity marker");
    std::fs::hard_link(&identity, root.path().join("restart_pending")).expect("canonical lease");

    identity_safe_dispose_restart_marker(root.path(), nonce);
    assert!(!root.path().join("restart_pending").exists());
    assert!(!identity.exists());

    // A newer request now owns the canonical name. The second disposal call must
    // not touch it: no name it is allowed to unlink exists any more.
    std::fs::write(root.path().join("restart_pending"), "nonce=next-request\n")
        .expect("newer request");
    identity_safe_dispose_restart_marker(root.path(), nonce);
    assert_eq!(
        std::fs::read_to_string(root.path().join("restart_pending")).expect("newer request"),
        "nonce=next-request\n"
    );
}

/// #5254 D4②: a stale disposer that finds someone else's lease restores it.
#[test]
fn disposal_restores_a_foreign_lease_and_leaves_its_identity_alone() {
    let root = tempfile::tempdir().expect("runtime root");
    let newer = root.path().join("restart_pending.newer-request");
    std::fs::write(&newer, "nonce=newer-request\n").expect("newer identity");
    std::fs::hard_link(&newer, root.path().join("restart_pending")).expect("newer lease");

    identity_safe_dispose_restart_marker(root.path(), "stale-request");

    assert_eq!(
        std::fs::read_to_string(root.path().join("restart_pending")).expect("restored lease"),
        "nonce=newer-request\n"
    );
    assert!(
        newer.exists(),
        "a foreign identity name is never ours to take"
    );
}

/// #5254 D4② lower bound / §7 [r3]-2: the third actor's window. Between the CAS
/// rename and the restore, a newer request re-created the canonical name, so
/// `hard_link` fails with `EEXIST`. Deleting the disposed inode here is what
/// loses the middle request outright; keeping it leaves that request reachable
/// through its identity name and nameable as `restart-lease-lost`.
#[test]
fn a_failed_restore_keeps_the_disposed_marker_as_residue() {
    let root = tempfile::tempdir().expect("runtime root");
    let disposed = ".restart_pending.dispose.third-actor-window";
    std::fs::write(root.path().join(disposed), "nonce=middle-request\n")
        .expect("marker taken by the CAS rename");
    std::fs::write(
        root.path().join("restart_pending"),
        "nonce=newest-request\n",
    )
    .expect("a third request re-created the canonical name");

    restore_foreign_disposed_marker(root.path(), disposed, "stale-request");

    assert_eq!(
        std::fs::read_to_string(root.path().join(disposed)).expect("preserved residue"),
        "nonce=middle-request\n",
        "an EEXIST restore must not destroy the inode it holds"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("restart_pending")).expect("newest request"),
        "nonce=newest-request\n"
    );
}

const COMMIT_LATCH_CHILD_ENV: &str = "AGENTDESK_RESTART_COMMIT_LATCH_CHILD";

/// #5254 D4④ (§9 S2 required gate): run the real commit path in a process of its
/// own, so the latch is exercised end-to-end with nothing else in the binary
/// sharing it. What this cannot show is the production *storage*: a test binary
/// compiles the thread-local twin of the cell (`runtime_bootstrap.rs`), and the
/// source assertion below is what pins the `cfg(not(test))` branch's shape.
#[test]
fn a_committed_restart_survives_late_cancellation_in_a_dedicated_process() {
    let test_name = concat!(
        "services::discord::runtime_bootstrap::spawns::tests::",
        "commit_latch_child"
    );
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", test_name, "--nocapture"])
        .env(COMMIT_LATCH_CHILD_ENV, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn isolated commit latch test");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().expect("poll commit latch child") {
            assert!(status.success(), "isolated commit latch child failed");
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().expect("kill stuck commit latch child");
            let _ = child.wait();
            panic!("the commit latch child must reach its assertions");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn commit_latch_child() {
    if std::env::var(COMMIT_LATCH_CHILD_ENV).is_err() {
        return;
    }
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.restart.shutdown_remaining.store(1, Ordering::SeqCst);
    let root = tempfile::tempdir().expect("runtime root");
    let nonce = "latched-commit";
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

    assert!(
        commit_deferred_restart_sentinel(root.path(), &ProviderKind::Codex, nonce, &guard)
            .expect("durable commit")
    );
    assert_eq!(committed_nonce().as_deref(), Some(nonce));
    assert!(!latch_commit("a-later-nonce"), "the latch is one-shot");
    assert_eq!(committed_nonce().as_deref(), Some(nonce));

    // Cancellation published after the point of no return. The guard is still
    // armed, and only the latch stands between it and an unfenced runtime that
    // has already published a durable terminal artifact.
    std::fs::write(
        root.path().join("restart_cancelled"),
        format!("nonce={nonce}\n"),
    )
    .expect("late cancellation");
    drop(guard);

    assert!(shared.restart.intake_worker_lifecycle.admission_is_fenced());
    assert!(shared.restart.shutting_down.load(Ordering::Acquire));
    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::Acquire), 0);
}

/// #5254 §9-0 auxiliary assertion: line adjacency, which the design grades as a
/// regression hint rather than a proof — it is format-fragile and says nothing
/// about control flow. It does pin the two orderings the commit contract needs:
/// the latch takes the point of no return before any call that can fail, and the
/// durability call follows rather than swallows it.
#[test]
fn the_commit_latches_between_the_rename_and_the_directory_fsync() {
    const PUBLISHER: &str = include_str!("gateway_lease_recovery.rs");
    let lines: Vec<&str> = PUBLISHER
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .collect();
    let renamed = lines
        .iter()
        .position(|line| line.starts_with("runtime_store::atomic_write("))
        .expect("the terminal publish still goes through runtime_store::atomic_write");
    assert!(
        lines[renamed + 1].starts_with("latch_commit("),
        "#5254 D4④: the latch must be the first call after the point of no return, found {:?}",
        lines[renamed + 1]
    );
    assert!(
        lines[renamed + 2].contains("runtime_store::fsync_parent_dir("),
        "#5254 D10: the directory fsync must follow the latch, found {:?}",
        lines[renamed + 2]
    );

    const LATCH: &str = include_str!("../runtime_bootstrap.rs");
    assert!(
        LATCH.contains("#[cfg(not(test))]\nfn with_commit_latch")
            && LATCH.contains("static COMMIT_LATCH: std::sync::OnceLock<String>"),
        "#5254 D4④ P2-3: production must keep one process-global latch cell"
    );
}

/// #5254 D4③ / §9-0: the point of no return has exactly one exit. A string
/// assertion cannot prove that `exit(0)` dominates the path — the runtime tests
/// above do that work — but it does fail loudly if a refactor reintroduces a
/// retraction between the commit and the exit, which is how both [r2]-2 and
/// [r2]-3 were reachable states on main.
#[test]
fn nothing_between_the_point_of_no_return_and_the_exit_can_undo_it() {
    const POLLER: &str = include_str!("spawns.rs");
    let (_, after_commit) = POLLER
        .split_once("Ok(true) => {}")
        .expect("the poller still matches a committed sentinel");
    let (committed_path, _) = after_commit
        .split_once("std::process::exit(0);")
        .expect("the committed path still ends in an unconditional exit");
    for retraction in [
        "handoff_superseded_restart",
        "remove_file",
        "restart_request_matches",
        "continue",
    ] {
        assert!(
            !committed_path.contains(retraction),
            "#5254 D4③: `{retraction}` must not stand between the commit and the exit"
        );
    }

    let (_, commit_body) = POLLER
        .split_once("fn commit_deferred_restart_sentinel")
        .expect("the commit helper still exists");
    let (commit_body, _) = commit_body
        .split_once("\n}\n")
        .expect("the commit helper still ends");
    assert!(
        !commit_body.contains("remove_file"),
        "#5254 D4③: a published acknowledgement is never withdrawn by its publisher"
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
