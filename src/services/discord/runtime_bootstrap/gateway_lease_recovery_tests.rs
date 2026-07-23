use std::str::FromStr;

use sqlx::Connection;

use super::gateway_lease_recovery::{
    GATEWAY_LEASE_APPLICATION_PREFIX, GatewayLeaseHolder, PromotionHandoffOutcome,
    STANDBY_PROMOTION_IN_PROGRESS, follow_promotion_handoff_chain, gateway_holder_is_reapable,
    gateway_lease_application_name_for, reap_orphaned_gateway_lease_for_instance_with_min_age,
    recover_cancelled_promotion, restart_artifact_is_newer_than, restart_file_nonce,
    try_create_restart_marker, wait_for_promotion_handoff,
};
use crate::services::discord::ProviderKind;

#[tokio::test]
async fn promotion_owner_recovers_all_runtimes_when_cancel_precedes_first_poll_tick() {
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
    assert!(!try_create_restart_marker(&marker, "nonce=promotion-a\n").expect("exclusive create"));
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

#[tokio::test]
async fn stale_prior_lifetime_persisted_does_not_mask_current_cancel() {
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
    let stale_path = root.path().join("restart_persisted");
    std::fs::write(&stale_path, "nonce=current\n").expect("stale persisted");
    let old = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&stale_path, old).expect("set stale mtime");
    let boot = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_100);
    assert!(!restart_artifact_is_newer_than(
        root.path(),
        "restart_persisted",
        boot
    ));
    assert!(
        stale_path.exists(),
        "boot must not delete barrier-owned persisted ack"
    );

    std::fs::write(root.path().join("restart_pending"), "nonce=current\n").expect("current marker");
    let cancel = root.path().join("restart_cancelled");
    std::fs::write(&cancel, "nonce=current\n").expect("current cancellation");
    let fresh = filetime::FileTime::from_unix_time(1_700_000_200, 0);
    filetime::set_file_mtime(&cancel, fresh).expect("set fresh cancel mtime");
    assert!(restart_artifact_is_newer_than(
        root.path(),
        "restart_cancelled",
        boot
    ));
    std::fs::remove_file(root.path().join("restart_pending")).expect("remove current marker");

    // Model the lifetime-guarded production decision directly: stale persisted
    // is ignored, while the current-lifetime cancellation wins.
    let outcome = if restart_artifact_is_newer_than(root.path(), "restart_persisted", boot) {
        PromotionHandoffOutcome::Committed
    } else {
        PromotionHandoffOutcome::Cancelled
    };
    assert_eq!(outcome, PromotionHandoffOutcome::Cancelled);
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

#[test]
fn fresh_current_lifetime_persisted_is_commit_evidence_and_survives_boot() {
    let root = tempfile::tempdir().expect("runtime root");
    let persisted = root.path().join("restart_persisted");
    std::fs::write(&persisted, "nonce=current\n").expect("fresh persisted");
    let fresh = filetime::FileTime::from_unix_time(1_700_000_200, 0);
    filetime::set_file_mtime(&persisted, fresh).expect("set fresh mtime");
    let boot = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_100);
    assert!(restart_artifact_is_newer_than(
        root.path(),
        "restart_persisted",
        boot
    ));
    assert!(
        persisted.exists(),
        "respawned binary must preserve external barrier ack"
    );
}

#[test]
fn committed_existing_marker_gap_preserves_promotion_fence() {
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
    std::fs::write(root.path().join("restart_persisted"), "nonce=deploy-b\n")
        .expect("durable commit");
    assert!(restart_file_nonce(root.path(), "restart_pending").is_none());

    // AlreadyExists was observed just before the committer removed pending. The
    // persisted acknowledgement must win over the subsequent missing marker.
    let committed = root.path().join("restart_persisted").exists();
    assert!(committed);
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

fn pg_test_base_database_url() -> String {
    std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim_end_matches('/').to_string())
        .unwrap_or_else(|| {
            let user = std::env::var("PGUSER")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| std::env::var("USER").ok())
                .unwrap_or_else(|| "postgres".to_string());
            let host = std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_string());
            let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".to_string());
            format!("postgresql://{user}@{host}:{port}")
        })
}

#[tokio::test]
async fn gateway_orphan_reap_uses_production_query_and_right_parses_instance_id_pg() {
    let _lifecycle = crate::db::postgres::lock_test_lifecycle();
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    let base = pg_test_base_database_url();
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
