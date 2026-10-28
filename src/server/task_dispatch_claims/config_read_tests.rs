//! Public claims use the last validated snapshot and keep rejected edits out of scheduling.

use super::{claim_after_reading, claim_task_dispatches};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::server::task_dispatch_claims::TaskDispatchClaimRequest;
use crate::server::task_dispatch_claims::task_dispatch_claims_pg_tests as fixture;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;
use std::time::Duration;

const OWNER: &str = "mac-book-release";

/// One online claim owner, a dispatch holding one `ue_editor` slot, and two pending dispatches
/// that need it: capacity 2 claims exactly one of them.
async fn seed(pool: &sqlx::PgPool) {
    let needs = || serde_json::json!({"required": {"semaphores": ["ue_editor"]}});
    fixture::seed_worker_node(pool, OWNER, "mac-book", 0).await;
    fixture::seed_agent_and_card(pool).await;
    fixture::seed_dispatch_with_required_capabilities(
        pool,
        "disp-holder",
        "H",
        needs(),
        "dispatched",
    )
    .await;
    for id in ["disp-a", "disp-b"] {
        fixture::seed_dispatch_with_required_capabilities(pool, id, id, needs(), "pending").await;
    }
    fixture::seed_dispatch_semaphore_holding(
        pool,
        "per-cluster",
        "cluster",
        "mac-mini-release",
        "disp-holder",
        600,
    )
    .await;
}

fn request() -> TaskDispatchClaimRequest {
    TaskDispatchClaimRequest {
        claim_owner: OWNER.to_string(),
        ttl_secs: Some(60),
        limit: Some(10),
        to_agent_id: None,
        dispatch_type: None,
        lease_ttl_secs: Some(60),
    }
}

#[tokio::test]
async fn claim_uses_snapshot_and_retains_it_after_rejected_reload() {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agentdesk.yaml");
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    let write = |capacity: &str| {
        std::fs::write(&path, format!(
            "server: {{}}\ndata:\n  dir: {}\ncluster:\n  semaphores:\n    ue_editor:\n      capacity: {capacity}\n      scope: per-cluster\n",
            root.path().join("data").display()
        )).unwrap();
    };
    write("1");
    crate::config_live_reload::install(crate::config::load_from_path(&path).unwrap());
    write("2");
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let outcome = claim_task_dispatches(&pool, &request()).await.unwrap();
    assert!(
        outcome.claimed.is_empty(),
        "disk capacity must not bypass the installed semaphore"
    );
    assert_eq!(outcome.skipped.len(), 2);
    write("invalid");
    assert!(matches!(
        crate::config_live_reload::reload_from_path(&path),
        crate::config_live_reload::ReloadOutcome::Rejected { .. }
    ));
    assert!(
        claim_task_dispatches(&pool, &request())
            .await
            .unwrap()
            .claimed
            .is_empty()
    );
    write("2");
    assert!(matches!(
        crate::config_live_reload::reload_from_path(&path),
        crate::config_live_reload::ReloadOutcome::Applied { .. }
    ));
    let outcome = claim_task_dispatches(&pool, &request()).await.unwrap();
    assert_eq!(
        outcome
            .claimed
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["disp-a"]
    );
    assert_eq!(outcome.skipped.len(), 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn claim_config_read_panic_fails_the_claim_before_any_sql() {
    let db = TestPostgresDb::create().await;
    let direct = db.connect_and_migrate().await;
    seed(&direct).await;
    let proxy = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
    let pool = proxy.pool(2);

    let error = claim_after_reading(&pool, &request(), || panic!("the config read panicked"))
        .await
        .expect_err("a panicked read fails the claim");
    assert!(
        error.starts_with("read cluster config for task dispatch claims: "),
        "{error}"
    );
    assert_eq!(
        (proxy.accepted(), proxy.executions().len()),
        (0, 0),
        "no worker lookup or transaction started"
    );

    pool.close().await;
    direct.close().await;
    db.drop().await;
}
