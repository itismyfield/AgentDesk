use std::str::FromStr;

use sqlx::Connection;

use super::gateway_lease_recovery::{
    GATEWAY_LEASE_APPLICATION_PREFIX, GatewayLeaseHolder, PromotionHandoffOutcome,
    STANDBY_PROMOTION_IN_PROGRESS, gateway_holder_is_reapable, gateway_lease_application_name_for,
    reap_orphaned_gateway_lease_for_instance_with_min_age, recover_cancelled_promotion,
    wait_for_promotion_handoff,
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
