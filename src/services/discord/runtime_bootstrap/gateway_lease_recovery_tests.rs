use std::str::FromStr;

use sqlx::Connection;

use super::gateway_lease_recovery::{
    GATEWAY_LEASE_APPLICATION_PREFIX, GatewayLeaseHolder, PromotionHandoffOutcome,
    STANDBY_PROMOTION_IN_PROGRESS, TerminalProof, attempt_clean_standby_promotion,
    follow_promotion_handoff_chain, gateway_holder_is_reapable, gateway_lease_application_name_for,
    reap_orphaned_gateway_lease_for_instance_with_min_age, recover_cancelled_promotion,
    restart_artifact_proof, restart_file_nonce, restart_request_artifact_path, terminal_proof,
    try_create_restart_marker, wait_for_promotion_handoff,
};
use crate::services::discord::ProviderKind;

/// #5254 S1 fixture: age a restart artifact well past any plausible process
/// start so a reintroduced mtime "current lifetime" conjunction would reject it.
fn age_artifact(path: &std::path::Path) {
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1_700_000_000, 0))
        .expect("age restart artifact");
}

/// #5185: `STANDBY_PROMOTION_IN_PROGRESS` is one process-global `AtomicBool`,
/// and the tests below both publish to it and assert on it. libtest runs them
/// on parallel threads, so a sibling's opening `store(true)` lands between this
/// test's clearing call and its closing assertion, and the assertion observes
/// `true` for a reason that has nothing to do with the code under test. That is
/// the failure the widened sweep hit as a non-reproducing flake.
///
/// Serialise only the tests that touch the flag, against each other. The rest
/// of the library suite still runs in parallel, so this costs no wall-clock
/// time and does not hide anything: a real regression in promotion recovery
/// still fails, deterministically.
///
/// A `tokio::sync::Mutex` rather than `std::sync::Mutex` because the guard is
/// held across `.await`, which the crate-wide `clippy::await_holding_lock` deny
/// forbids for std guards. It also cannot be poisoned, so a panicking holder
/// leaves no cascade behind it.
static PROMOTION_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct PromotionFixture {
    _root_env: crate::config::TestEnvVarGuard,
    _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
    root: tempfile::TempDir,
    admin_url: String,
    database_name: String,
    pool: sqlx::PgPool,
    registry: std::sync::Arc<crate::services::discord::health::HealthRegistry>,
    runtimes: Vec<std::sync::Arc<crate::services::discord::SharedData>>,
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl PromotionFixture {
    async fn new(label: &str) -> Option<Self> {
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().expect("promotion runtime root");
        let root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let lifecycle = crate::db::postgres::lock_test_lifecycle();
        let Some(base) = crate::db::postgres::postgres_test_database_url_base() else {
            return None;
        };
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        let admin_url = format!("{base}/{admin_db}");
        let database_name = format!(
            "agentdesk_gateway_promotion_{}",
            uuid::Uuid::new_v4().simple()
        );
        if let Err(error) =
            crate::db::postgres::create_test_database(&admin_url, &database_name, label).await
        {
            eprintln!("skipping {label}: {error}");
            return None;
        }
        let database_url = format!("{base}/{database_name}");
        let pool = crate::db::postgres::connect_test_pool_and_migrate(&database_url, label)
            .await
            .expect("connect isolated promotion database");
        let registry = std::sync::Arc::new(crate::services::discord::health::HealthRegistry::new());
        let mut runtimes = Vec::new();
        for provider in [ProviderKind::Claude, ProviderKind::Codex] {
            let mut runtime = crate::services::discord::make_shared_data_for_tests();
            let runtime_mut = std::sync::Arc::get_mut(&mut runtime)
                .expect("fresh runtime is uniquely owned before registry install");
            runtime_mut.provider = provider.clone();
            runtime_mut.health_registry = std::sync::Arc::downgrade(&registry);
            registry
                .register(provider.as_str().to_string(), runtime.clone())
                .await;
            runtimes.push(runtime);
        }
        Some(Self {
            _root_env: root_env,
            _lifecycle: lifecycle,
            root,
            admin_url,
            database_name,
            pool,
            registry,
            runtimes,
            _env_lock: env_lock,
        })
    }

    fn shared(&self) -> std::sync::Arc<crate::services::discord::SharedData> {
        self.runtimes[0].clone()
    }

    async fn lease(&self, label: &str) -> crate::db::postgres::AdvisoryLockLease {
        crate::db::postgres::AdvisoryLockLease::try_acquire(&self.pool, 91_480_260, label)
            .await
            .expect("acquire promotion test lease")
            .expect("promotion test lease available")
    }

    fn assert_fenced(&self) {
        for runtime in &self.runtimes {
            assert!(
                runtime
                    .restart
                    .intake_worker_lifecycle
                    .admission_is_fenced(),
                "committed promotion keeps every runtime admission fence closed"
            );
            assert!(
                runtime
                    .restart
                    .restart_pending
                    .load(std::sync::atomic::Ordering::Acquire),
                "committed promotion keeps every runtime restart-pending flag set"
            );
        }
    }

    fn assert_recovered(&self) {
        for runtime in &self.runtimes {
            assert!(
                !runtime
                    .restart
                    .intake_worker_lifecycle
                    .admission_is_fenced(),
                "cancelled promotion reopens every runtime admission fence"
            );
            assert!(
                !runtime
                    .restart
                    .restart_pending
                    .load(std::sync::atomic::Ordering::Acquire),
                "cancelled promotion clears every runtime restart-pending flag"
            );
        }
    }

    async fn close(self, label: &str) {
        drop(self.registry);
        crate::db::postgres::close_test_pool(self.pool, label)
            .await
            .expect("close promotion test pool");
        crate::db::postgres::drop_test_database(&self.admin_url, &self.database_name, label)
            .await
            .expect("drop promotion test database");
    }
}

#[tokio::test]
async fn committed_existing_handoff_returns_true_and_preserves_every_runtime_fence_pg() {
    let _flag = PROMOTION_FLAG_LOCK.lock().await;
    STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    let Some(fixture) = PromotionFixture::new("committed existing promotion handoff pg").await
    else {
        return;
    };
    let existing_nonce = "deploy-existing";
    std::fs::write(
        fixture.root.path().join("restart_pending"),
        format!("nonce={existing_nonce}\nsource=deploy\n"),
    )
    .expect("existing restart owner marker");
    std::fs::write(
        fixture
            .root
            .path()
            .join(format!("restart_persisted.{existing_nonce}")),
        format!("nonce={existing_nonce}\nsource=deploy\n"),
    )
    .expect("identity-bound persisted acknowledgement");

    let promoted = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        attempt_clean_standby_promotion(
            &fixture.shared(),
            &ProviderKind::Claude,
            fixture
                .lease("committed existing promotion handoff pg")
                .await,
        ),
    )
    .await
    .expect("existing promotion handoff resolves");

    assert!(promoted, "identity-bound Committed handoff returns true");
    fixture.assert_fenced();
    assert_eq!(
        restart_file_nonce(fixture.root.path(), "restart_pending").as_deref(),
        Some(existing_nonce)
    );
    assert!(
        std::fs::read_dir(fixture.root.path())
            .expect("list promotion runtime root")
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with("restart_pending.")),
        "refused promotion removes its own identity marker"
    );
    assert!(!STANDBY_PROMOTION_IN_PROGRESS.load(std::sync::atomic::Ordering::Acquire));
    fixture
        .close("committed existing promotion handoff pg")
        .await;
}

#[tokio::test]
async fn committed_owned_handoff_returns_true_and_preserves_every_runtime_fence_pg() {
    let _flag = PROMOTION_FLAG_LOCK.lock().await;
    STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    let Some(fixture) = PromotionFixture::new("committed owned promotion handoff pg").await else {
        return;
    };
    let root = fixture.root.path().to_path_buf();
    let acknowledge = tokio::spawn(async move {
        let nonce = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(nonce) = restart_file_nonce(&root, "restart_pending") {
                    let request = std::fs::read_to_string(root.join("restart_pending"))
                        .expect("read owned promotion request");
                    assert!(
                        request
                            .lines()
                            .any(|line| line == "reason=gateway_standby_promotion")
                    );
                    assert!(request.lines().any(|line| line == "provider=claude"));
                    break nonce;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("promotion publishes its restart marker");
        std::fs::write(
            root.join(format!("restart_persisted.{nonce}")),
            format!("nonce={nonce}\nsource=test-deploy\n"),
        )
        .expect("identity-bound persisted acknowledgement");
        nonce
    });

    let promoted = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        attempt_clean_standby_promotion(
            &fixture.shared(),
            &ProviderKind::Claude,
            fixture.lease("committed owned promotion handoff pg").await,
        ),
    )
    .await
    .expect("owned promotion handoff resolves");
    let nonce = acknowledge.await.expect("acknowledgement task joins");

    assert!(
        promoted,
        "owned identity-bound Committed handoff returns true"
    );
    assert_eq!(
        restart_file_nonce(fixture.root.path(), "restart_pending").as_deref(),
        Some(nonce.as_str())
    );
    fixture.assert_fenced();
    // The owned Committed arm intentionally keeps this pre-S1 process owner latched.
    assert!(STANDBY_PROMOTION_IN_PROGRESS.load(std::sync::atomic::Ordering::Acquire));
    STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
    fixture.close("committed owned promotion handoff pg").await;
}

#[tokio::test]
async fn missing_existing_persisted_artifact_returns_false_and_reopens_every_runtime_fence_pg() {
    let _flag = PROMOTION_FLAG_LOCK.lock().await;
    STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    let Some(fixture) = PromotionFixture::new("missing existing persisted artifact pg").await
    else {
        return;
    };
    let existing_nonce = "deploy-cancelled";
    std::fs::write(
        fixture.root.path().join("restart_pending"),
        format!("nonce={existing_nonce}\nsource=deploy\n"),
    )
    .expect("existing restart owner marker");
    std::fs::write(
        fixture
            .root
            .path()
            .join(format!("restart_cancelled.{existing_nonce}")),
        format!("nonce={existing_nonce}\nsource=deploy\n"),
    )
    .expect("identity-bound cancellation acknowledgement");

    let promoted = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        attempt_clean_standby_promotion(
            &fixture.shared(),
            &ProviderKind::Claude,
            fixture
                .lease("missing existing persisted artifact pg")
                .await,
        ),
    )
    .await
    .expect("cancelled promotion handoff resolves");

    assert!(
        !promoted,
        "missing persisted artifact keeps the cancelled handoff on the retry path"
    );
    assert_eq!(
        terminal_proof(fixture.root.path(), existing_nonce),
        TerminalProof::Absent
    );
    fixture.assert_recovered();
    assert!(!STANDBY_PROMOTION_IN_PROGRESS.load(std::sync::atomic::Ordering::Acquire));
    fixture
        .close("missing existing persisted artifact pg")
        .await;
}

#[tokio::test]
async fn promotion_owner_recovers_all_runtimes_when_cancel_precedes_first_poll_tick() {
    let _flag = PROMOTION_FLAG_LOCK.lock().await;
    STANDBY_PROMOTION_IN_PROGRESS.store(true, std::sync::atomic::Ordering::SeqCst);
    let runtime_a = crate::services::discord::make_shared_data_for_tests();
    let runtime_b = crate::services::discord::make_shared_data_for_tests();
    let runtimes = vec![runtime_a.clone(), runtime_b.clone()];
    for runtime in &runtimes {
        runtime.restart.intake_worker_lifecycle.fence_admission();
        runtime
            .restart
            .restart_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    let root = tempfile::tempdir().expect("runtime root");
    let nonce = "promotion-missed-by-pollers";
    std::fs::write(
        root.path().join("restart_pending"),
        format!("nonce={nonce}\nreason=gateway_standby_promotion\n"),
    )
    .expect("promotion marker");
    // clear_restart_drain_mode publishes cancellation then removes the marker;
    // model that entire handoff before a provider poller gets its first tick.
    std::fs::write(
        root.path().join("restart_cancelled"),
        format!("nonce={nonce}\n"),
    )
    .expect("promotion cancellation");
    std::fs::remove_file(root.path().join("restart_pending")).expect("remove marker");

    assert_eq!(
        wait_for_promotion_handoff(root.path(), nonce).await,
        PromotionHandoffOutcome::Cancelled
    );
    recover_cancelled_promotion(&runtimes);

    for runtime in runtimes {
        assert!(
            !runtime
                .restart
                .intake_worker_lifecycle
                .admission_is_fenced()
        );
        assert!(
            !runtime
                .restart
                .restart_pending
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }
    assert!(!STANDBY_PROMOTION_IN_PROGRESS.load(std::sync::atomic::Ordering::Acquire));
}

#[tokio::test]
async fn superseded_promotion_preserves_new_owner_fence_and_flags() {
    let _flag = PROMOTION_FLAG_LOCK.lock().await;
    STANDBY_PROMOTION_IN_PROGRESS.store(true, std::sync::atomic::Ordering::SeqCst);
    let runtime_a = crate::services::discord::make_shared_data_for_tests();
    let runtime_b = crate::services::discord::make_shared_data_for_tests();
    let runtimes = vec![runtime_a.clone(), runtime_b.clone()];
    for runtime in &runtimes {
        runtime.restart.intake_worker_lifecycle.fence_admission();
        runtime
            .restart
            .restart_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let root = tempfile::tempdir().expect("runtime root");
    std::fs::write(
        root.path().join("restart_pending"),
        "nonce=deploy-b\nsource=deploy\n",
    )
    .expect("new owner marker");

    assert_eq!(
        wait_for_promotion_handoff(root.path(), "promotion-a").await,
        PromotionHandoffOutcome::Superseded
    );
    STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);

    for runtime in runtimes {
        assert!(
            runtime
                .restart
                .intake_worker_lifecycle
                .admission_is_fenced()
        );
        assert!(
            runtime
                .restart
                .restart_pending
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }
    assert!(root.path().join("restart_pending").exists());
}

#[tokio::test]
async fn supersession_chain_keeps_owner_until_final_cancel_and_recovers_all_runtimes() {
    let _flag = PROMOTION_FLAG_LOCK.lock().await;
    STANDBY_PROMOTION_IN_PROGRESS.store(true, std::sync::atomic::Ordering::SeqCst);
    let runtime_a = crate::services::discord::make_shared_data_for_tests();
    let runtime_b = crate::services::discord::make_shared_data_for_tests();
    let runtimes = vec![runtime_a.clone(), runtime_b.clone()];
    for runtime in &runtimes {
        runtime.restart.intake_worker_lifecycle.fence_admission();
        runtime
            .restart
            .restart_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let root = tempfile::tempdir().expect("runtime root");
    std::fs::write(root.path().join("restart_pending"), "nonce=a\n").expect("A marker");
    let root_for_owner = root.path().to_path_buf();
    let owner = tokio::spawn(async move {
        follow_promotion_handoff_chain(&root_for_owner, "a".to_string()).await
    });
    tokio::task::yield_now().await;
    std::fs::write(root.path().join("restart_pending"), "nonce=b\n").expect("B supersedes A");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        !owner.is_finished(),
        "process-wide owner must follow B rather than terminate on supersession"
    );
    std::fs::write(root.path().join("restart_pending"), "nonce=c\n").expect("C supersedes B");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!owner.is_finished(), "owner must follow the whole chain");
    std::fs::write(root.path().join("restart_cancelled"), "nonce=c\n").expect("cancel C");
    std::fs::remove_file(root.path().join("restart_pending")).expect("remove C marker");
    assert_eq!(
        owner.await.expect("owner join"),
        PromotionHandoffOutcome::Cancelled
    );
    recover_cancelled_promotion(&runtimes);
    for runtime in runtimes {
        assert!(
            !runtime
                .restart
                .intake_worker_lifecycle
                .admission_is_fenced()
        );
        assert!(
            !runtime
                .restart
                .restart_pending
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }
    assert!(!STANDBY_PROMOTION_IN_PROGRESS.load(std::sync::atomic::Ordering::Acquire));
}

#[tokio::test]
async fn existing_marker_cancel_restores_promotion_fence_for_retry() {
    let _flag = PROMOTION_FLAG_LOCK.lock().await;
    STANDBY_PROMOTION_IN_PROGRESS.store(true, std::sync::atomic::Ordering::SeqCst);
    let runtime_a = crate::services::discord::make_shared_data_for_tests();
    let runtime_b = crate::services::discord::make_shared_data_for_tests();
    let runtimes = vec![runtime_a.clone(), runtime_b.clone()];
    for runtime in &runtimes {
        runtime.restart.intake_worker_lifecycle.fence_admission();
        runtime
            .restart
            .restart_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let root = tempfile::tempdir().expect("runtime root");
    let marker = root.path().join("restart_pending");
    std::fs::write(&marker, "nonce=deploy-b\nsource=deploy\n").expect("existing marker");
    assert!(
        !try_create_restart_marker(root.path(), "promotion-a", "nonce=promotion-a\n")
            .expect("exclusive create")
    );
    assert!(
        !root.path().join("restart_pending.promotion-a").exists(),
        "a refused lease must not leave our identity marker behind"
    );
    let existing_nonce =
        restart_file_nonce(root.path(), "restart_pending").expect("existing nonce");
    let root_for_owner = root.path().to_path_buf();
    let owner =
        tokio::spawn(
            async move { wait_for_promotion_handoff(&root_for_owner, &existing_nonce).await },
        );
    std::fs::write(root.path().join("restart_cancelled"), "nonce=deploy-b\n")
        .expect("cancel existing owner");
    std::fs::remove_file(&marker).expect("remove existing marker");
    assert_eq!(
        owner.await.expect("owner join"),
        PromotionHandoffOutcome::Cancelled
    );
    recover_cancelled_promotion(&runtimes);

    for runtime in runtimes {
        assert!(
            !runtime
                .restart
                .intake_worker_lifecycle
                .admission_is_fenced()
        );
        assert!(
            !runtime
                .restart
                .restart_pending
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }
    assert!(!STANDBY_PROMOTION_IN_PROGRESS.load(std::sync::atomic::Ordering::Acquire));
}

/// #5254 [r2]-4 / M12: a terminal artifact carrying somebody else's nonce is
/// not evidence about our request, however fresh its mtime is — and the retired
/// lifetime gate answered "current" for exactly these files.
#[tokio::test]
async fn foreign_nonce_terminal_artifact_does_not_mask_our_cancellation() {
    let root = tempfile::tempdir().expect("runtime root");
    for name in ["restart_persisted", "restart_persisted.someone-else"] {
        std::fs::write(root.path().join(name), "nonce=someone-else\n").expect("foreign commit");
    }
    assert_eq!(
        terminal_proof(root.path(), "current"),
        TerminalProof::Absent
    );

    std::fs::write(root.path().join("restart_cancelled"), "nonce=current\n")
        .expect("our cancellation");
    assert_eq!(
        wait_for_promotion_handoff(root.path(), "current").await,
        PromotionHandoffOutcome::Cancelled
    );
}

/// #5254 D8 / M14: nonce equality does not consult the wall clock, so an
/// identity proof whose mtime predates this process is still the point of no
/// return. Restoring the lifetime conjunction turns this commit into a cancel.
#[tokio::test]
async fn identity_terminal_proof_commits_handoff_despite_clock_regression() {
    let root = tempfile::tempdir().expect("runtime root");
    let identity = root.path().join("restart_persisted.current");
    std::fs::write(&identity, "nonce=current\nprovider=claude\n").expect("identity proof");
    age_artifact(&identity);

    assert_eq!(
        terminal_proof(root.path(), "current"),
        TerminalProof::Proven
    );
    assert_eq!(
        wait_for_promotion_handoff(root.path(), "current").await,
        PromotionHandoffOutcome::Committed
    );
    assert!(
        identity.exists(),
        "respawned binary must preserve external barrier ack"
    );
}

/// #5254 D8 / M12: the `existing_nonce == None` arm fails closed. A marker we
/// lost the race to that carries no nonce cannot attribute any terminal
/// artifact to itself, so the promotion restores its preflight fence (already
/// covered by the recovery tests above) rather than claiming success.
#[test]
fn nonce_free_existing_marker_fails_closed_instead_of_claiming_promotion() {
    let root = tempfile::tempdir().expect("runtime root");
    std::fs::write(root.path().join("restart_pending"), "source=deploy\n")
        .expect("nonce-free marker");
    std::fs::write(root.path().join("restart_persisted"), "nonce=deploy-b\n")
        .expect("unrelated durable commit");
    assert!(
        !try_create_restart_marker(root.path(), "promotion-a", "nonce=promotion-a\n")
            .expect("exclusive create")
    );
    assert!(restart_file_nonce(root.path(), "restart_pending").is_none());
    assert_eq!(
        terminal_proof(root.path(), "promotion-a"),
        TerminalProof::Absent,
        "a nonce-free marker plus somebody else's commit proves nothing about us"
    );

    // The arm production takes on that `None` is the fail-closed one.
    let arm = include_str!("gateway_lease_recovery.rs")
        .split_once("let Some(existing_nonce) = restart_file_nonce(&root, \"restart_pending\")")
        .expect("fail-closed arm")
        .1
        .split_once("};")
        .expect("arm body")
        .0;
    assert!(arm.contains("recover_cancelled_promotion(&runtimes);"));
    assert!(
        !arm.contains("return true"),
        "the nonce-free arm must not claim promotion succeeded"
    );
}

/// #5254 D8 / M12: the supersession arm has no nonce-free shortcut left. A
/// marker without a `nonce=` line supersedes us and yields no next nonce, and a
/// fresh foreign terminal artifact must not turn that into a commit.
#[tokio::test]
async fn nonce_free_supersession_folds_to_cancelled_despite_fresh_terminal_artifact() {
    let root = tempfile::tempdir().expect("runtime root");
    std::fs::write(
        root.path().join("restart_persisted"),
        "nonce=someone-else\n",
    )
    .expect("fresh foreign commit");
    std::fs::write(root.path().join("restart_pending"), "source=deploy\n")
        .expect("nonce-free superseding marker");

    assert_eq!(
        follow_promotion_handoff_chain(root.path(), "promotion-a".to_string()).await,
        PromotionHandoffOutcome::Cancelled
    );
}

/// #5254 D1 + D11-1 / M13: both reachable names are hard links to a body that
/// was complete before either existed, and the dot-prefixed stage is gone.
#[test]
fn marker_creation_links_a_complete_body_under_both_names() {
    let root = tempfile::tempdir().expect("runtime root");
    let body = "nonce=promotion-a\nreason=gateway_standby_promotion\n";
    assert!(try_create_restart_marker(root.path(), "promotion-a", body).expect("acquire lease"));

    let identity = root.path().join("restart_pending.promotion-a");
    let canonical = root.path().join("restart_pending");
    for path in [&identity, &canonical] {
        assert_eq!(
            std::fs::read_to_string(path).expect("marker body"),
            body,
            "both names must resolve to the complete staged body"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&identity).expect("identity meta").ino(),
            std::fs::metadata(&canonical).expect("canonical meta").ino(),
            "the canonical lease is a hard link of the identity name"
        );
    }
    assert!(
        !std::fs::read_dir(root.path())
            .expect("scan root")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".restart_")),
        "no staging residue may survive a successful claim"
    );
}

/// #5254 D1 / M-N9: EEXIST on the identity name is nonce reuse, which is
/// refused. Crucially the canonical lease is never published without its
/// identity name — an index-only marker is a shape this runtime cannot make.
#[test]
fn nonce_reuse_is_refused_without_publishing_an_index_only_marker() {
    let root = tempfile::tempdir().expect("runtime root");
    let orphan = root.path().join("restart_pending.promotion-a");
    std::fs::write(&orphan, "nonce=promotion-a\n").expect("identity orphan");

    let error = try_create_restart_marker(root.path(), "promotion-a", "nonce=promotion-a\n")
        .expect_err("nonce reuse is fail-closed");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(
        !root.path().join("restart_pending").exists(),
        "a refused claim must not publish the canonical lease"
    );
    assert!(
        !std::fs::read_dir(root.path())
            .expect("scan root")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".restart_"))
    );
}

/// #5254 §2-3 / [r3]-4 / M-N3: the charset gate is not line-anchored. Both
/// `x\n../escape` and the charset-clean-per-line `x\nescape` must be refused; a
/// `grep -Eqx`-shaped gate passes both of them, because it succeeds on any one
/// matching line and the leading `x` line matches on its own.
#[test]
fn nonce_charset_gate_refuses_smuggled_newlines_and_builds_no_path() {
    let root = tempfile::tempdir().expect("runtime root");
    for unsafe_nonce in [
        "",
        ".",
        "..",
        "../escape",
        "x\n../escape",
        "x\nescape",
        "x\0y",
        "a b",
        "n".repeat(129).as_str(),
    ] {
        assert!(
            restart_request_artifact_path(root.path(), "restart_pending", unsafe_nonce).is_none(),
            "gate must refuse {unsafe_nonce:?}"
        );
        let error = try_create_restart_marker(root.path(), unsafe_nonce, "nonce=x\n")
            .expect_err("unsafe nonce is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    assert_eq!(
        std::fs::read_dir(root.path())
            .expect("scan root")
            .flatten()
            .count(),
        0,
        "a refused nonce must not create anything at all"
    );
    for safe_nonce in [
        uuid::Uuid::new_v4().to_string().as_str(),
        "a.b_c-d",
        "n".repeat(128).as_str(),
    ] {
        assert_eq!(
            restart_request_artifact_path(root.path(), "restart_pending", safe_nonce),
            Some(root.path().join(format!("restart_pending.{safe_nonce}"))),
        );
    }
}

/// ERRATUM §E5.4 / M-N9: an unsafe nonce yields `Absent`. There is no fallback
/// read to promote, so a nonce that cannot name an identity file cannot be
/// proven by the index either.
#[test]
fn unsafe_nonce_terminal_read_is_absent_and_never_promotes_the_index() {
    let root = tempfile::tempdir().expect("runtime root");
    std::fs::write(root.path().join("restart_persisted"), "nonce=../escape\n")
        .expect("index body matching an unsafe nonce");

    assert_eq!(
        restart_file_nonce(root.path(), "restart_persisted").as_deref(),
        Some("../escape"),
        "the index body really does carry the unsafe nonce"
    );
    assert_eq!(
        terminal_proof(root.path(), "../escape"),
        TerminalProof::Absent
    );

    // §E5.4's second sentence: this arm also leaves the label. §E5.8 4-R makes
    // diagnostic confidence the only residual risk, so the label is load-bearing.
    let arm = include_str!("gateway_lease_recovery.rs")
        .split_once("let Some(identity) = restart_request_artifact_path(root, name, nonce)")
        .expect("unsafe-nonce read arm")
        .1
        .split_once("};")
        .expect("arm body")
        .0;
    assert!(
        arm.contains("tracing::warn!") && arm.contains("restart-nonce-unsafe"),
        "the read path must name its unsafe-nonce refusal with the fixed label"
    );
}

/// ERRATUM §E5.2: the terminal read is three-valued and only the identity name
/// is a durability authority. R2's disposition for a demoted index observation
/// is unchanged from main — it commits the handoff, which keeps the shared fence
/// closed and stops the standby lease retry loop — and R2 asserts no durability
/// of its own. The shell deploy gate does still promote a fixed-name index
/// observation to a green verdict in this tree; S4/S5 close that, not this arm.
#[tokio::test]
async fn terminal_proof_is_three_valued_and_only_identity_is_green() {
    let root = tempfile::tempdir().expect("runtime root");
    assert_eq!(terminal_proof(root.path(), "n1"), TerminalProof::Absent);

    let index = root.path().join("restart_persisted");
    std::fs::write(&index, "nonce=n1\n").expect("legacy fixed-name publish");
    assert_eq!(
        terminal_proof(root.path(), "n1"),
        TerminalProof::LegacyIndexOnly,
        "a fixed-name-only publisher is identified, never proven"
    );
    assert_eq!(
        wait_for_promotion_handoff(root.path(), "n1").await,
        PromotionHandoffOutcome::Committed
    );

    let identity = root.path().join("restart_persisted.n2");
    std::fs::write(&identity, "nonce=n2\n").expect("identity publish");
    assert_eq!(terminal_proof(root.path(), "n2"), TerminalProof::Proven);

    // Name/body disagreement is ignored rather than trusted (D11-1).
    std::fs::write(root.path().join("restart_persisted.n3"), "nonce=n2\n")
        .expect("hand-made identity file");
    assert_eq!(terminal_proof(root.path(), "n3"), TerminalProof::Absent);
}

/// ERRATUM §E5.2 priority order: a legacy index observation suppresses a
/// `cancelled` verdict, because the rename that carried our nonce necessarily
/// happened after the cancellation record.
#[tokio::test]
async fn legacy_index_persisted_suppresses_a_matching_cancellation() {
    let root = tempfile::tempdir().expect("runtime root");
    std::fs::write(root.path().join("restart_cancelled.n1"), "nonce=n1\n")
        .expect("identity cancellation");
    assert_eq!(
        restart_artifact_proof(root.path(), "restart_cancelled", "n1"),
        TerminalProof::Proven
    );
    assert_eq!(
        wait_for_promotion_handoff(root.path(), "n1").await,
        PromotionHandoffOutcome::Cancelled
    );

    std::fs::write(root.path().join("restart_persisted"), "nonce=n1\n").expect("legacy publish");
    assert_eq!(
        wait_for_promotion_handoff(root.path(), "n1").await,
        PromotionHandoffOutcome::Committed
    );
}

/// #5254 PC-5 for the pending family: per-request identities accumulate, and an
/// earlier request's leftover identity marker does not block the next claim.
#[test]
fn sequential_requests_keep_independent_identities() {
    let root = tempfile::tempdir().expect("runtime root");
    assert!(try_create_restart_marker(root.path(), "a", "nonce=a\n").expect("A claims"));
    // The committer drops the lease; the identity name is swept later (D5).
    std::fs::remove_file(root.path().join("restart_pending")).expect("release lease");
    assert!(try_create_restart_marker(root.path(), "b", "nonce=b\n").expect("B claims"));

    assert!(root.path().join("restart_pending.a").exists());
    assert!(root.path().join("restart_pending.b").exists());
    assert_eq!(
        restart_file_nonce(root.path(), "restart_pending").as_deref(),
        Some("b")
    );
}

/// #5254 §9-0 auxiliary source assertion: the mtime lifetime gate retired with
/// five symbols and one call site. This is a regression hint, not the authority
/// — the behavioural authority is the clock-regression and fail-closed tests
/// above — but it fails loudly if a refactor resurrects the deleted branch.
#[test]
fn retired_mtime_lifetime_gate_has_no_remaining_source_references() {
    let recovery = include_str!("gateway_lease_recovery.rs");
    let bootstrap = include_str!("../runtime_bootstrap.rs");
    for symbol in [
        "RESTART_ARTIFACT_BOOT_INSTANT",
        "restart_artifact_boot_instant",
        "record_restart_artifact_boot_instant",
        "restart_artifact_is_current_lifetime",
        "restart_artifact_is_newer_than",
    ] {
        assert!(
            !recovery.contains(symbol),
            "{symbol} must stay retired in gateway_lease_recovery.rs"
        );
        assert!(
            !bootstrap.contains(symbol),
            "{symbol} must stay retired in runtime_bootstrap.rs"
        );
    }
}

#[test]
fn orphan_reap_requires_named_stale_matching_worker() {
    let safe = GatewayLeaseHolder {
        pid: 42,
        application_name: gateway_lease_application_name_for("node:a", 42, "claude"),
        instance_id: Some("node:a".to_string()),
        node_status: Some("offline".to_string()),
        heartbeat_recent: Some(false),
        process_matches: Some(true),
        dcserver_pid: Some(42),
    };
    assert!(gateway_holder_is_reapable(&safe));

    for unsafe_holder in [
        GatewayLeaseHolder {
            application_name: "other-service".to_string(),
            ..safe.clone()
        },
        GatewayLeaseHolder {
            node_status: Some("online".to_string()),
            ..safe.clone()
        },
        GatewayLeaseHolder {
            heartbeat_recent: Some(true),
            ..safe.clone()
        },
        GatewayLeaseHolder {
            process_matches: Some(false),
            ..safe.clone()
        },
        GatewayLeaseHolder {
            instance_id: None,
            ..safe.clone()
        },
    ] {
        assert!(!gateway_holder_is_reapable(&unsafe_holder));
    }
}

#[tokio::test]
async fn gateway_orphan_reap_uses_production_query_and_right_parses_instance_id_pg() {
    let _lifecycle = crate::db::postgres::lock_test_lifecycle();
    let Some(base) = crate::db::postgres::postgres_test_database_url_base() else {
        return;
    };
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    let admin_url = format!("{base}/{admin_db}");
    let database_name = format!("agentdesk_gateway_reap_{}", uuid::Uuid::new_v4().simple());
    if let Err(error) = crate::db::postgres::create_test_database(
        &admin_url,
        &database_name,
        "gateway orphan holder pg",
    )
    .await
    {
        eprintln!("skipping gateway orphan holder pg test: {error}");
        return;
    }
    let database_url = format!("{base}/{database_name}");
    let pool = crate::db::postgres::connect_test_pool_and_migrate(
        &database_url,
        "gateway orphan holder pg",
    )
    .await
    .expect("connect isolated gateway reap database");

    let instance_id = &format!("node:east:{}", "x".repeat(120));
    let dcserver_pid = std::process::id() as i32;
    sqlx::query(
        "INSERT INTO worker_nodes (
             instance_id, process_id, role, effective_role, status, last_heartbeat_at
         ) VALUES ($1, $2, 'auto', 'worker', 'offline', NOW() - INTERVAL '1 minute')",
    )
    .bind(instance_id)
    .bind(dcserver_pid)
    .execute(&pool)
    .await
    .expect("seed stale worker node");

    let holder_name =
        gateway_lease_application_name_for(instance_id, dcserver_pid as u32, "claude");
    assert!(holder_name.len() <= 63);
    let options = sqlx::postgres::PgConnectOptions::from_str(&database_url)
        .expect("parse isolated database url")
        .application_name(&holder_name);
    let mut holder = sqlx::PgConnection::connect_with(&options)
        .await
        .expect("connect named holder backend");
    let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut holder)
        .await
        .expect("read holder backend pid");
    assert_ne!(
        dcserver_pid, backend_pid,
        "PID domains must differ in this test"
    );

    let lock_id = 91_480_100_i64;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(lock_id)
        .fetch_one(&mut holder)
        .await
        .expect("hold gateway advisory lock");
    assert!(acquired);
    sqlx::query("SELECT 1")
        .execute(&mut holder)
        .await
        .expect("leave holder idle");

    let stored_name: String =
        sqlx::query_scalar("SELECT application_name FROM pg_stat_activity WHERE pid = $1")
            .bind(backend_pid)
            .fetch_one(&pool)
            .await
            .expect("read stored application name");
    assert_eq!(
        stored_name, holder_name,
        "bounded identity must survive PostgreSQL storage"
    );

    let reaped = reap_orphaned_gateway_lease_for_instance_with_min_age(
        &pool,
        lock_id,
        &ProviderKind::Claude,
        0,
        instance_id,
    )
    .await
    .expect("run production orphan reap query");
    assert!(
        reaped,
        "production query must reap delimiter-bearing stale instance"
    );
    let still_alive: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)")
            .bind(backend_pid)
            .fetch_one(&pool)
            .await
            .expect("check holder termination");
    assert!(!still_alive);

    drop(holder);
    crate::db::postgres::close_test_pool(pool, "gateway orphan holder pg")
        .await
        .expect("close gateway reap pool");
    crate::db::postgres::drop_test_database(&admin_url, &database_name, "gateway orphan holder pg")
        .await
        .expect("drop gateway reap database");
}
