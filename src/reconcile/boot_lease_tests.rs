//! Boot reconcile test that reads inflight episodes through the ROOT lookup.
use crate::reconcile::reconcile_boot_db_pg;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

struct TestPostgresDb {
    _lock: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl TestPostgresDb {
    async fn create() -> Self {
        let lock = crate::db::postgres::lock_test_lifecycle();
        let base = crate::db::postgres::postgres_test_database_url_base().expect(
            "POSTGRES_TEST_DATABASE_URL_BASE required for dispatch delivery reconcile tests",
        ); // agentdesk-audit: allow-unwrap — test-only fixture: a missing or unreachable PostgreSQL fails the test instead of passing silently
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        let admin_url = format!("{base}/{admin_db}");
        let database_name = format!(
            "agentdesk_delivery_reconcile_{}",
            uuid::Uuid::new_v4().simple()
        );
        let database_url = format!("{base}/{database_name}");
        crate::db::postgres::create_test_database(
            &admin_url,
            &database_name,
            "dispatch delivery reconcile tests",
        )
        .await
        .unwrap_or_else(|error| {
            panic!("dispatch delivery reconcile tests: create test database: {error}")
        });

        Self {
            _lock: lock,
            admin_url,
            database_name,
            database_url,
        }
    }

    async fn connect_and_migrate(&self) -> PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(
            &self.database_url,
            "dispatch delivery reconcile tests",
        )
        .await
        .unwrap()
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "dispatch delivery reconcile tests",
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn boot_reconcile_keeps_non_expired_processing_row_owned_by_peer_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, status, claimed_at, claim_owner
         ) VALUES (
            'dispatch-live-peer-lease', 'notify', 'processing', NOW(), 'peer-instance'
         )",
    )
    .execute(&pool)
    .await
    .expect("seed peer-owned processing outbox row");

    let stats = reconcile_boot_db_pg(
        &pool,
        "current-instance",
        None,
        crate::services::discord::zombie_foreground_release::inflight_episode_lookup_for_tmux_name,
    )
    .await
    .expect("boot reconcile succeeds");
    assert_eq!(stats.stale_processing_outbox_reset, 0);

    let row = sqlx::query(
        "SELECT status, claim_owner, claimed_at
           FROM dispatch_outbox
          WHERE dispatch_id = 'dispatch-live-peer-lease'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "processing");
    assert_eq!(
        row.try_get::<String, _>("claim_owner").unwrap(),
        "peer-instance"
    );
    assert!(
        row.try_get::<Option<DateTime<Utc>>, _>("claimed_at")
            .unwrap()
            .is_some()
    );

    pool.close().await;
    pg_db.drop().await;
}
