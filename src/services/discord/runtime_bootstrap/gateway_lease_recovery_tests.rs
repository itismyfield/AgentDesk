use sqlx::Connection;

use super::gateway_lease_recovery::{
    GATEWAY_LEASE_APPLICATION_PREFIX, GATEWAY_ORPHAN_MIN_AGE_SECS, GatewayLeaseHolder,
    gateway_holder_is_reapable,
};

#[test]
fn orphan_reap_requires_named_stale_matching_worker() {
    let safe = GatewayLeaseHolder {
        pid: 42,
        application_name: "agentdesk:gateway:node-a:42:claude".to_string(),
        instance_id: Some("node-a".to_string()),
        node_status: Some("offline".to_string()),
        heartbeat_recent: Some(false),
        process_matches: Some(true),
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
async fn gateway_orphan_holder_sql_distinguishes_dcserver_and_backend_pids_pg() {
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

    let dcserver_pid = std::process::id() as i32;
    sqlx::query(
        "INSERT INTO worker_nodes (
             instance_id, process_id, role, effective_role, status, last_heartbeat_at
         ) VALUES ('node-a', $1, 'auto', 'worker', 'offline',
                   NOW() - ($2::BIGINT * INTERVAL '1 second'))",
    )
    .bind(dcserver_pid)
    .bind(GATEWAY_ORPHAN_MIN_AGE_SECS + 60)
    .execute(&pool)
    .await
    .expect("seed stale worker node");

    let holder_name = format!("{GATEWAY_LEASE_APPLICATION_PREFIX}node-a:{dcserver_pid}:claude");
    let options = sqlx::postgres::PgConnectOptions::new()
        .host("localhost")
        .username(
            std::env::var("PGUSER")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| std::env::var("USER").ok())
                .as_deref()
                .unwrap_or("postgres"),
        )
        .database(&database_name)
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
    let inspected = sqlx::query_as::<_, GatewayLeaseHolder>(
        r#"
        SELECT a.pid, a.application_name, n.instance_id, n.status AS node_status,
               (n.last_heartbeat_at >= NOW() - ($1::BIGINT * INTERVAL '1 second')) AS heartbeat_recent,
               (n.process_id = split_part(a.application_name, ':', 4)::INTEGER) AS process_matches
          FROM pg_stat_activity a
          LEFT JOIN worker_nodes n
            ON split_part(a.application_name, ':', 3) = n.instance_id
           AND split_part(a.application_name, ':', 4) ~ '^[0-9]+$'
           AND n.process_id = split_part(a.application_name, ':', 4)::INTEGER
           AND split_part(a.application_name, ':', 5) = 'claude'
         WHERE a.pid = $2 AND a.pid <> pg_backend_pid()
        "#,
    )
    .bind(GATEWAY_ORPHAN_MIN_AGE_SECS)
    .bind(backend_pid)
    .fetch_one(&pool)
    .await
    .expect("inspect holder through production PID parsing contract");
    assert_eq!(inspected.pid, backend_pid);
    assert_eq!(inspected.instance_id.as_deref(), Some("node-a"));
    assert_eq!(inspected.process_matches, Some(true));
    assert_eq!(inspected.heartbeat_recent, Some(false));
    assert!(gateway_holder_is_reapable(&inspected));

    drop(holder);
    crate::db::postgres::close_test_pool(pool, "gateway orphan holder pg")
        .await
        .expect("close gateway reap pool");
    crate::db::postgres::drop_test_database(&admin_url, &database_name, "gateway orphan holder pg")
        .await
        .expect("drop gateway reap database");
}
