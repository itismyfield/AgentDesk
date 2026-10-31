use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

#[tokio::test]
async fn node_pg_commit_readback_reset_and_late_seed_without_owner_pg() {
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    let db = TestPostgresDb::create().await;
    let source_pool = db.connect_and_migrate().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(source_pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let id = OwnerIdentity::new("claude", "733601");
    let operation = Uuid::new_v4();
    let first = tokio::time::timeout(
        Duration::from_secs(2),
        update_channel_node_override(&pool, &id, 0, operation, Some("desk-A")),
    )
    .await
    .expect("one-slot writer must reuse its owner transaction")
    .unwrap();
    assert!(matches!(first, WriteOutcome::Applied(_)));
    let other_mac = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
        source_pool.clone(),
    ));
    other_mac
        .overrides
        .node_overrides
        .insert(serenity::ChannelId::new(733601), "desk-B-local".into());
    assert_eq!(
        channel_node_override(&other_mac, serenity::ChannelId::new(733601))
            .await
            .unwrap(),
        Some("desk-A".into())
    );
    let reset = update_channel_node_override(&pool, &id, 1, Uuid::new_v4(), None)
        .await
        .unwrap();
    assert!(matches!(reset, WriteOutcome::Applied(_)));
    let uncertain = update_channel_node_override(&pool, &id, 0, operation, Some("desk-A"))
        .await
        .unwrap();
    assert!(matches!(uncertain, WriteOutcome::Conflict(_)));
    assert!(home_write_note(&uncertain).contains("미확정"));
    assert!(!home_write_note(&uncertain).contains("저장했습니다"));
    assert!(matches!(
        homes::write(&pool, &id, 0, Uuid::new_v4(), Some("stale-local"))
            .await
            .unwrap(),
        WriteOutcome::Conflict(_)
    ));
    pool.close().await;
    let restarted = db.connect_and_migrate().await;
    let old_local =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(restarted.clone()));
    old_local
        .overrides
        .node_overrides
        .insert(serenity::ChannelId::new(733601), "stale-local".into());
    assert_eq!(
        channel_node_override(&old_local, serenity::ChannelId::new(733601))
            .await
            .unwrap(),
        None
    );
    let stored = homes::read(&restarted, &id).await.unwrap().unwrap();
    assert_eq!((stored.mode.as_str(), stored.revision), ("inherit", 2));
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM intake_session_owners WHERE raw_channel_id='733601'",
    )
    .fetch_one(&restarted)
    .await
    .unwrap();
    assert_eq!(owners, 0);
    restarted.close().await;
    source_pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn node_pg_borrowed_owner_transaction_rolls_back_and_read_failure_is_not_local_pg() {
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let id = OwnerIdentity::new("claude", "733602");
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(id.advisory_key())
        .execute(&mut *tx)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        homes::write_on_tx(&mut tx, &id, 0, Uuid::new_v4(), Some("desk-B")),
    )
    .await
    .unwrap()
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(homes::read(&pool, &id).await.unwrap(), None);
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared
        .overrides
        .node_overrides
        .insert(serenity::ChannelId::new(733602), "stale-local".into());
    pool.close().await;
    assert!(
        channel_node_override(&shared, serenity::ChannelId::new(733602))
            .await
            .is_err()
    );
    db.drop().await;
}

struct RuntimeRoot {
    _live: crate::config_live_reload::TestLiveGuard,
    root: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}
impl RuntimeRoot {
    fn new() -> Self {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let live = crate::config_live_reload::test_live_guard(lock);
        let root = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        // SAFETY: the process-wide test environment lock is held until Drop restores the root.
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
        }
        let mut config = crate::config::Config::default();
        config.data.dir = root.path().join("data");
        crate::config_live_reload::install(config);
        Self {
            _live: live,
            root,
            previous,
        }
    }
}
impl Drop for RuntimeRoot {
    fn drop(&mut self) {
        // SAFETY: the environment lock still belongs to this fixture.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

#[test]
fn node_pg_conflict_response_is_uncertain_never_success() {
    let outcome = WriteOutcome::Conflict(None);
    assert!(home_write_note(&outcome).contains("미확정"));
    assert!(!home_write_note(&outcome).contains("저장했습니다"));
}
