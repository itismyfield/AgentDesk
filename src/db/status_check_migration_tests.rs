use crate::db::auto_queue::run_status::ACTIVATE_ELIGIBLE_RUN_STATUSES;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::auto_queue::{
    ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_DONE, ENTRY_STATUS_FAILED, ENTRY_STATUS_PENDING,
    ENTRY_STATUS_SKIPPED, ENTRY_STATUS_USER_CANCELLED,
};
use sqlx::{AssertSqlSafe, PgPool};

const STATUS_CHECKS: &str = include_str!("../../migrations/postgres/0184_core_status_checks.sql");

/// Runs `sql` with `<table>_status_check` lifted and re-adds the same NOT VALID definition,
/// leaving a row an older binary could have written before the check existed.
pub(crate) async fn execute_with_status_check_lifted(pool: &PgPool, table: &str, sql: &str) {
    let name = format!("{table}_status_check");
    let mut tx = pool.begin().await.unwrap();
    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1 AND conrelid = $2::regclass",
    )
    .bind(&name)
    .bind(table)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(definition.ends_with("NOT VALID"), "{name}: {definition}");
    sqlx::query(AssertSqlSafe(format!(
        "ALTER TABLE {table} DROP CONSTRAINT {name}"
    )))
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(AssertSqlSafe(sql.to_string()))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(AssertSqlSafe(format!(
        "ALTER TABLE {table} ADD CONSTRAINT {name} {definition}"
    )))
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

struct StatusTable {
    name: &'static str,
    allowed: Vec<&'static str>,
    /// `$1` is the status and `$2` a unique key; returns the row id as text.
    insert: &'static str,
}

fn status_tables() -> Vec<StatusTable> {
    let mut runs = ACTIVATE_ELIGIBLE_RUN_STATUSES.to_vec();
    runs.extend(["completed", "cancelled"]);
    vec![
        StatusTable {
            name: "auto_queue_entries",
            allowed: vec![
                ENTRY_STATUS_PENDING,
                ENTRY_STATUS_DISPATCHED,
                ENTRY_STATUS_DONE,
                ENTRY_STATUS_SKIPPED,
                ENTRY_STATUS_FAILED,
                ENTRY_STATUS_USER_CANCELLED,
                "cancelled",
            ],
            insert: "INSERT INTO auto_queue_entries (id, status) VALUES ($2, $1) RETURNING id",
        },
        StatusTable {
            name: "auto_queue_runs",
            allowed: runs,
            insert: "INSERT INTO auto_queue_runs (id, status) VALUES ($2, $1) RETURNING id",
        },
        StatusTable {
            name: "message_outbox",
            allowed: vec![
                "pending",
                "held",
                "processing",
                "sent",
                "failed",
                "cancelled",
                "delivered",
            ],
            insert: "INSERT INTO message_outbox (target, content, status, cancelled_at)
                     VALUES ($2, 'body', $1, CASE WHEN $1 = 'cancelled' THEN NOW() END)
                     RETURNING id::TEXT",
        },
        StatusTable {
            name: "dispatch_outbox",
            allowed: vec![
                "pending",
                "processing",
                "done",
                "failed",
                "acknowledged",
                "claimed",
            ],
            insert: "INSERT INTO dispatch_outbox (dispatch_id, action, status)
                     VALUES ($2, 'status_reaction', $1) RETURNING id::TEXT",
        },
        StatusTable {
            name: "routines",
            allowed: vec!["enabled", "paused", "detached"],
            insert: "INSERT INTO routines (id, script_ref, name, status)
                     VALUES ($2, 'routine.js', $2, $1) RETURNING id",
        },
        StatusTable {
            name: "routine_runs",
            allowed: vec![
                "running",
                "succeeded",
                "failed",
                "skipped",
                "paused",
                "interrupted",
                "error",
            ],
            insert: "INSERT INTO routine_runs (id, routine_id, status)
                     VALUES ($2, 'status-check-routine', $1) RETURNING id",
        },
    ]
}

async fn insert_row(
    pool: &PgPool,
    table: &StatusTable,
    status: &str,
) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(table.insert)
        .bind(status)
        .bind(uuid::Uuid::new_v4().to_string())
        .fetch_one(pool)
        .await
}

async fn set_status(
    pool: &PgPool,
    table: &str,
    id: &str,
    status: &str,
) -> Result<u64, sqlx::Error> {
    let cancelled = if table == "message_outbox" {
        ", cancelled_at = CASE WHEN $1 = 'cancelled' THEN NOW() ELSE cancelled_at END"
    } else {
        ""
    };
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {table} SET status = $1{cancelled} WHERE id::TEXT = $2"
    )))
    .bind(status)
    .bind(id)
    .execute(pool)
    .await
    .map(|done| done.rows_affected())
}

fn assert_status_check_rejected(result: Result<impl std::fmt::Debug, sqlx::Error>, table: &str) {
    let error = result.expect_err("bogus status must be rejected");
    let db = error.as_database_error().expect("database error");
    assert_eq!(db.code().as_deref(), Some("23514"), "{table}: {db}");
    assert_eq!(
        db.constraint(),
        Some(format!("{table}_status_check").as_str())
    );
}

fn add_statement(table: &str) -> &'static str {
    let head = format!("ALTER TABLE {table}\n");
    let start = STATUS_CHECKS
        .find(&head)
        .unwrap_or_else(|| panic!("0184 has no statement for {table}"));
    let len = STATUS_CHECKS[start..].find(';').unwrap();
    &STATUS_CHECKS[start..start + len]
}

async fn migrated_pool() -> (TestPostgresDb, PgPool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('status-check-routine', 'routine.js', 'status-check-routine')")
        .execute(&pool)
        .await
        .unwrap();
    (db, pool)
}

#[tokio::test]
async fn core_status_checks_admit_every_writer_and_past_value_and_reject_others_pg() {
    let (db, pool) = migrated_pool().await;
    for table in status_tables() {
        let row = insert_row(&pool, &table, table.allowed[0]).await.unwrap();
        for status in &table.allowed {
            insert_row(&pool, &table, status)
                .await
                .unwrap_or_else(|error| panic!("{} INSERT {status}: {error}", table.name));
            let updated = set_status(&pool, table.name, &row, status)
                .await
                .unwrap_or_else(|error| panic!("{} UPDATE {status}: {error}", table.name));
            assert_eq!(updated, 1, "{} UPDATE {status}", table.name);
        }
        assert_status_check_rejected(insert_row(&pool, &table, "bogus").await, table.name);
        assert_status_check_rejected(
            set_status(&pool, table.name, &row, "bogus").await,
            table.name,
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn core_status_checks_add_over_legacy_rows_and_let_them_move_into_the_set_pg() {
    let (db, pool) = migrated_pool().await;
    for table in status_tables() {
        let row = insert_row(&pool, &table, table.allowed[0]).await.unwrap();
        let name = format!("{}_status_check", table.name);
        sqlx::query(AssertSqlSafe(format!(
            "ALTER TABLE {} DROP CONSTRAINT {name}",
            table.name
        )))
        .execute(&pool)
        .await
        .unwrap();
        set_status(&pool, table.name, &row, "legacy_unknown")
            .await
            .unwrap();
        sqlx::raw_sql(AssertSqlSafe(add_statement(table.name).to_string()))
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("0184 ADD over a legacy {} row: {error}", table.name));
        let validated: bool =
            sqlx::query_scalar("SELECT convalidated FROM pg_constraint WHERE conname = $1")
                .bind(&name)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!validated, "{name} must stay NOT VALID");
        let updated = set_status(&pool, table.name, &row, table.allowed[0])
            .await
            .unwrap();
        assert_eq!(updated, 1, "{} legacy row moves into the set", table.name);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn operator_acknowledge_of_failed_dispatch_outbox_rows_passes_the_status_check_pg() {
    let (db, pool) = migrated_pool().await;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO dispatch_outbox (dispatch_id, action, status)
         VALUES ('status-check-ack', 'status_reaction', 'failed') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let acknowledged =
        crate::services::health_diagnostics::acknowledge_failed_dispatch_outbox_rows(
            &pool,
            &[id],
            "status check",
        )
        .await
        .unwrap();
    assert_eq!(acknowledged, vec![id]);
    let status: String = sqlx::query_scalar("SELECT status FROM dispatch_outbox WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "acknowledged");
    pool.close().await;
    db.drop().await;
}
