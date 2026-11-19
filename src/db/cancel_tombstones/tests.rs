use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, PgConnection};

use super::*;

const MIGRATION: &str = include_str!("../../../migrations/postgres/0024_cancel_tombstones.sql");

pub(crate) struct StorePgFixture {
    pub(crate) pool: PgPool,
    admin: PgConnection,
    schema: String,
}

impl StorePgFixture {
    pub(crate) async fn connect_test_pool(migration: &'static str) -> Self {
        let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
            .expect("store PG tests require an explicit fixture URL");
        let url = url::Url::parse(&base).expect("parse store test PostgreSQL URL");
        assert!(
            url.host_str().is_some() && url.port().is_some(),
            "store test PostgreSQL URL must specify a host and port"
        );
        let database =
            std::env::var("POSTGRES_TEST_ADMIN_DB").unwrap_or_else(|_| "postgres".into());
        let options = base
            .parse::<PgConnectOptions>()
            .expect("parse store test PostgreSQL options")
            .database(&database);
        let mut admin = PgConnection::connect_with(&options)
            .await
            .expect("connect to store test PostgreSQL");
        // The schema identifier contains only the fixed prefix and UUID hex.
        let schema = format!("adk_store_test_{}", Uuid::new_v4().simple());
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&mut admin)
            .await
            .expect("create isolated store test schema");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(
                options.options([("search_path", schema.as_str()), ("lock_timeout", "2s")]),
            )
            .await
            .expect("connect pool to isolated store test schema");
        sqlx::raw_sql(migration)
            .execute(&pool)
            .await
            .expect("apply store repository migration");
        Self {
            pool,
            admin,
            schema,
        }
    }

    pub(crate) async fn close(mut self) {
        self.pool.close().await;
        sqlx::query(AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&mut self.admin)
        .await
        .expect("drop isolated store test schema");
    }
}

#[tokio::test]
async fn concurrent_consumers_suppress_a_cancel_only_once_pg() {
    let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
    let pool = &fixture.pool;
    insert_cancel_tombstone(pool, Uuid::new_v4(), 42, Some("pane"), Some(100), "cancel")
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        consume_cancel_tombstone(pool, 42, "pane", Some(100)),
        consume_cancel_tombstone(pool, 42, "pane", Some(100))
    );
    assert_ne!(first.unwrap(), second.unwrap());
    assert!(
        !consume_cancel_tombstone(pool, 42, "pane", None)
            .await
            .unwrap()
    );
    fixture.close().await;
}

#[tokio::test]
async fn consume_respects_session_and_inclusive_output_boundary_pg() {
    let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
    let pool = &fixture.pool;
    insert_cancel_tombstone(pool, Uuid::new_v4(), 42, Some("pane"), Some(100), "cancel")
        .await
        .unwrap();

    let boundary = 100 + CANCEL_TEARDOWN_GRACE_BYTES;
    assert!(
        !consume_cancel_tombstone(pool, 42, "other", None)
            .await
            .unwrap()
    );
    assert!(
        !consume_cancel_tombstone(pool, 42, "pane", Some(boundary + 1))
            .await
            .unwrap()
    );
    assert!(
        consume_cancel_tombstone(pool, 42, "pane", Some(boundary))
            .await
            .unwrap()
    );
    insert_cancel_tombstone(pool, Uuid::new_v4(), 42, None, None, "cancel")
        .await
        .unwrap();
    assert!(
        consume_cancel_tombstone(pool, 42, "new-pane", Some(boundary + 1))
            .await
            .unwrap()
    );
    fixture.close().await;
}

#[tokio::test]
async fn old_rows_are_not_consumed_and_expiry_prunes_only_expired_rows_pg() {
    let fixture = StorePgFixture::connect_test_pool(MIGRATION).await;
    let pool = &fixture.pool;
    let old = Uuid::new_v4();
    let fresh = Uuid::new_v4();
    for (id, channel) in [(old, 41), (fresh, 42)] {
        insert_cancel_tombstone(pool, id, channel, None, None, "cancel")
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE cancel_tombstones SET recorded_at = NOW() - interval '2 minutes',
             expires_at = NOW() - interval '1 minute' WHERE client_id = $1",
    )
    .bind(old)
    .execute(pool)
    .await
    .unwrap();

    assert!(
        !consume_cancel_tombstone(pool, 41, "pane", None)
            .await
            .unwrap()
    );
    assert_eq!(prune_expired_cancel_tombstones(pool).await.unwrap(), 1);
    assert_eq!(
        delete_cancel_tombstones_by_client_ids(pool, &[fresh])
            .await
            .unwrap(),
        1
    );
    assert!(
        !consume_cancel_tombstone(pool, 42, "pane", None)
            .await
            .unwrap()
    );
    fixture.close().await;
}
