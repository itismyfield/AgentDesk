use super::{
    KIND_TERMINAL_NO_DELIVERY_OWNER, REDELIVERY_CLAIMED, REDELIVERY_DECLINED, REDELIVERY_DELIVERED,
    REDELIVERY_PENDING, REDELIVERY_SUPERSEDED,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::postgres;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::migrate::{Migration, Migrator};
use std::borrow::Cow;

const ORIGINAL_0120: &str =
    include_str!("../../../tests/fixtures/postgres/0120_relay_dead_letter_redelivery.original.sql");

fn original_migrator_through_0141() -> Migrator {
    assert_eq!(
        format!("{:x}", Sha256::digest(ORIGINAL_0120.as_bytes())),
        "ba6981fecb0e07c30dabf307d7b948e358ccfc9958ad6ab6c6f525cff62cf8a6",
        "the upgrade fixture must retain the originally applied migration bytes"
    );
    let all = sqlx::migrate!("./migrations/postgres");
    let mut replaced = 0;
    let migrations = all
        .iter()
        .filter(|migration| migration.version <= 141)
        .map(|migration| {
            if migration.version == 120 {
                replaced += 1;
                Migration::new(
                    migration.version,
                    migration.description.clone(),
                    migration.migration_type,
                    sqlx::SqlStr::from_static(ORIGINAL_0120),
                    migration.no_tx,
                )
            } else {
                migration.clone()
            }
        })
        .collect();
    assert_eq!(replaced, 1, "the original 0120 migration must be applied");
    Migrator {
        migrations: Cow::Owned(migrations),
        ..Migrator::DEFAULT
    }
}

async fn original_database() -> (TestPostgresDb, PgPool) {
    let fixture = TestPostgresDb::create().await;
    let pool = postgres::connect_test_pool_with_max_connections(
        &fixture.database_url,
        "relay dead letter migration upgrade",
        2,
    )
    .await
    .expect("connect without running the head migrations");
    original_migrator_through_0141()
        .run(&pool)
        .await
        .expect("apply the original migrations through 0141");
    let last_applied: i64 =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(&pool)
            .await
            .expect("read the original migration boundary");
    assert_eq!(last_applied, 141);
    (fixture, pool)
}

async fn upgrade_to_head(pool: &PgPool) {
    postgres::migrate(pool)
        .await
        .expect("upgrade the original database with the head migrator");
    let mismatches = postgres::applied_migration_checksum_mismatch_details(pool)
        .await
        .expect("inspect applied migration checksums after upgrade");
    assert!(mismatches.is_empty(), "checksum drift: {mismatches:?}");
    let applied: bool =
        sqlx::query_scalar("SELECT success FROM _sqlx_migrations WHERE version = 142")
            .fetch_one(pool)
            .await
            .expect("the state constraint migration must be recorded");
    assert!(applied);
    let validated: bool = sqlx::query_scalar(
        "SELECT convalidated FROM pg_constraint \
         WHERE conrelid = 'relay_dead_letter'::regclass \
           AND conname = 'relay_dead_letter_redelivery_state_check'",
    )
    .fetch_one(pool)
    .await
    .expect("the state constraint must exist after upgrade");
    assert!(!validated, "legacy rows must await separate validation");
}

fn assert_state_constraint(error: sqlx::Error, operation: &str) {
    assert_eq!(
        error.as_database_error().and_then(|db| db.constraint()),
        Some("relay_dead_letter_redelivery_state_check"),
        "{operation}: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn upgrade_path_preserves_original_migration_checksums_pg() {
    let (fixture, pool) = original_database().await;
    upgrade_to_head(&pool).await;
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn state_constraint_preserves_unknown_legacy_rows_and_fences_new_writes_pg() {
    let (fixture, pool) = original_database().await;
    let legacy_id: i64 = sqlx::query_scalar(
        "INSERT INTO relay_dead_letter (kind, channel_id, content, reason, redelivery_state) \
         VALUES ($1, 'legacy-channel', 'legacy body', 'legacy reason', 'legacy_unknown') \
         RETURNING id",
    )
    .bind(KIND_TERMINAL_NO_DELIVERY_OWNER)
    .fetch_one(&pool)
    .await
    .expect("seed an unknown state before the constraint exists");

    upgrade_to_head(&pool).await;

    let preserved: (String, String, String) = sqlx::query_as(
        "SELECT redelivery_state, content, reason FROM relay_dead_letter WHERE id = $1",
    )
    .bind(legacy_id)
    .fetch_one(&pool)
    .await
    .expect("the legacy row must survive the migration");
    assert_eq!(
        preserved,
        (
            "legacy_unknown".to_string(),
            "legacy body".to_string(),
            "legacy reason".to_string(),
        )
    );

    let states = [
        REDELIVERY_PENDING,
        REDELIVERY_CLAIMED,
        REDELIVERY_DELIVERED,
        REDELIVERY_DECLINED,
        REDELIVERY_SUPERSEDED,
    ];
    let mut last_valid_id = None;
    for state in states {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO relay_dead_letter (kind, channel_id, content, reason, redelivery_state) \
             VALUES ($1, 'new-channel', 'valid body', 'valid state', $2) RETURNING id",
        )
        .bind(KIND_TERMINAL_NO_DELIVERY_OWNER)
        .bind(state)
        .fetch_one(&pool)
        .await
        .expect("each supported state must remain insertable");
        last_valid_id = Some(id);
    }
    let valid_id = last_valid_id.expect("five supported rows must have been inserted");
    let error = sqlx::query(
        "INSERT INTO relay_dead_letter (kind, channel_id, content, reason, redelivery_state) \
         VALUES ($1, 'new-channel', 'invalid body', 'invalid state', 'new_unknown')",
    )
    .bind(KIND_TERMINAL_NO_DELIVERY_OWNER)
    .execute(&pool)
    .await
    .expect_err("the unvalidated constraint must reject new unknown states");
    assert_state_constraint(error, "insert an unknown state");
    let error =
        sqlx::query("UPDATE relay_dead_letter SET redelivery_state = 'new_unknown' WHERE id = $1")
            .bind(valid_id)
            .execute(&pool)
            .await
            .expect_err("the unvalidated constraint must reject updates to unknown states");
    assert_state_constraint(error, "update a valid row to an unknown state");
    let state: String =
        sqlx::query_scalar("SELECT redelivery_state FROM relay_dead_letter WHERE id = $1")
            .bind(valid_id)
            .fetch_one(&pool)
            .await
            .expect("read the valid row after the rejected update");
    assert_eq!(state, REDELIVERY_SUPERSEDED);

    pool.close().await;
    fixture.drop().await;
}
