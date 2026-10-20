//! Seeds rows at the schema just before each status migration, applies only
//! that migration, then compares the stored values and CHECK outcomes.

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::postgres;
use crate::dispatch::dispatch_status::VALID_DISPATCH_STATUSES;
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

async fn migrate_through(pool: &PgPool, last_version: i64) {
    let all = sqlx::migrate!("./migrations/postgres");
    let migrations: Vec<_> = all
        .iter()
        .filter(|migration| migration.version <= last_version)
        .cloned()
        .collect();
    assert!(
        migrations
            .iter()
            .any(|migration| migration.version == last_version),
        "migration {last_version} must exist"
    );
    Migrator {
        migrations: Cow::Owned(migrations),
        ..Migrator::DEFAULT
    }
    .run(pool)
    .await
    .unwrap_or_else(|error| panic!("apply migrations through {last_version}: {error}"));
    let applied: i64 =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(pool)
            .await
            .expect("read the applied migration boundary");
    assert_eq!(applied, last_version);
}

/// Static row writes and reads for the seeded tables; only `id` and `status`
/// vary, and the other NOT NULL text column gets a fixed filler.
fn insert_sql(table: &str) -> &'static str {
    match table {
        "task_dispatches" => "INSERT INTO task_dispatches (id, status) VALUES ($1, $2)",
        "agents" => "INSERT INTO agents (id, status, name) VALUES ($1, $2, 'seed')",
        "kanban_cards" => "INSERT INTO kanban_cards (id, status, title) VALUES ($1, $2, 'seed')",
        other => panic!("no seed statement for {other}"),
    }
}

fn select_status_sql(table: &str) -> &'static str {
    match table {
        "task_dispatches" => "SELECT status FROM task_dispatches WHERE id = $1",
        "agents" => "SELECT status FROM agents WHERE id = $1",
        "kanban_cards" => "SELECT status FROM kanban_cards WHERE id = $1",
        other => panic!("no status read for {other}"),
    }
}

async fn seed(pool: &PgPool, table: &str, rows: &[(&str, Option<&str>)]) {
    for (id, status) in rows {
        sqlx::query(insert_sql(table))
            .bind(id)
            .bind(status)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("seed {table} {id} with {status:?}: {error}"));
    }
}

async fn assert_statuses(pool: &PgPool, table: &str, expected: &[(&str, &str)]) {
    for (id, want) in expected {
        let stored: String = sqlx::query_scalar(select_status_sql(table))
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|error| panic!("read {table} {id}: {error}"));
        assert_eq!(&stored, want, "{table} {id} normalized status");
    }
}

/// Inserts one row in a rolled-back transaction; returns the violated constraint
/// name (or SQLSTATE when unnamed), or `None` when the row was accepted.
async fn write_outcome(pool: &PgPool, table: &str, status: Option<&str>) -> Option<String> {
    let mut tx = pool.begin().await.expect("begin probe transaction");
    let result = sqlx::query(insert_sql(table))
        .bind("probe-row")
        .bind(status)
        .execute(&mut *tx)
        .await;
    tx.rollback().await.expect("roll back probe transaction");
    match result {
        Ok(_) => None,
        Err(error) => {
            let db = error.as_database_error().unwrap_or_else(|| {
                panic!("{table} probe {status:?} failed outside the database: {error}")
            });
            Some(match db.constraint() {
                Some(constraint) => constraint.to_string(),
                None => db.code().map(|code| code.to_string()).unwrap_or_default(),
            })
        }
    }
}

async fn assert_accepts(pool: &PgPool, table: &str, statuses: &[&str]) {
    for status in statuses {
        assert_eq!(
            write_outcome(pool, table, Some(status)).await,
            None,
            "{table} must accept {status:?}"
        );
    }
}

async fn assert_rejects(pool: &PgPool, table: &str, constraint: &str, statuses: &[&str]) {
    for status in statuses {
        assert_eq!(
            write_outcome(pool, table, Some(status)).await.as_deref(),
            Some(constraint),
            "{table} must reject {status:?} through {constraint}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn migration_status_normalization_and_constraints_match_values_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = postgres::connect_test_pool_with_max_connections(
        &fixture.database_url,
        "dispatch status migration values",
        2,
    )
    .await
    .expect("connect without running the head migrations");

    migrate_through(&pool, 66).await;
    seed(
        &pool,
        "task_dispatches",
        &[
            ("td-null", None),
            ("td-blank", Some("  ")),
            ("td-mixed", Some(" Dispatched ")),
            ("td-legacy", Some("legacy_running")),
            ("td-typo", Some("complete")),
        ],
    )
    .await;
    migrate_through(&pool, 67).await;
    assert_statuses(
        &pool,
        "task_dispatches",
        &[
            ("td-null", "pending"),
            ("td-blank", "pending"),
            ("td-mixed", "dispatched"),
            ("td-legacy", "failed"),
            ("td-typo", "failed"),
        ],
    )
    .await;
    assert_accepts(&pool, "task_dispatches", VALID_DISPATCH_STATUSES).await;
    assert_rejects(
        &pool,
        "task_dispatches",
        "task_dispatches_status_known_check",
        &["legacy_running", "complete", "Pending", ""],
    )
    .await;
    assert_eq!(
        write_outcome(&pool, "task_dispatches", None)
            .await
            .as_deref(),
        Some("23502"),
        "task_dispatches.status must be NOT NULL"
    );

    seed(
        &pool,
        "agents",
        &[
            ("ag-null", None),
            ("ag-blank", Some(" ")),
            ("ag-working", Some(" WORKING ")),
            ("ag-archived", Some("archived")),
            ("ag-active", Some("active")),
            ("ag-busy", Some("busy")),
            ("ag-running", Some("Running")),
            ("ag-disabled", Some("disabled")),
            ("ag-deleted", Some("deleted")),
            ("ag-inactive", Some("inactive")),
            ("ag-unknown", Some("mystery")),
        ],
    )
    .await;
    seed(
        &pool,
        "kanban_cards",
        &[
            ("kc-null", None),
            ("kc-todo", Some("To-Do")),
            ("kc-doing", Some("Doing")),
            ("kc-qa", Some("QA")),
            ("kc-completed", Some("Completed")),
            ("kc-canceled", Some("canceled")),
            ("kc-custom", Some(" Needs Review! ")),
            ("kc-edges", Some("__custom__")),
            ("kc-digit", Some("9lives")),
            ("kc-symbols", Some("!!!")),
        ],
    )
    .await;
    migrate_through(&pool, 68).await;
    assert_statuses(
        &pool,
        "agents",
        &[
            ("ag-null", "idle"),
            ("ag-blank", "idle"),
            ("ag-working", "working"),
            ("ag-archived", "archived"),
            ("ag-active", "idle"),
            ("ag-busy", "working"),
            ("ag-running", "working"),
            ("ag-disabled", "archived"),
            ("ag-deleted", "archived"),
            ("ag-inactive", "archived"),
            ("ag-unknown", "idle"),
        ],
    )
    .await;
    assert_accepts(&pool, "agents", &["idle", "working", "archived"]).await;
    assert_rejects(
        &pool,
        "agents",
        "agents_status_known_check",
        &["active", "busy", "Idle", ""],
    )
    .await;
    assert_statuses(
        &pool,
        "kanban_cards",
        &[
            ("kc-null", "backlog"),
            ("kc-todo", "backlog"),
            ("kc-doing", "in_progress"),
            ("kc-qa", "qa_test"),
            ("kc-completed", "done"),
            ("kc-canceled", "cancelled"),
            ("kc-custom", "needs_review"),
            ("kc-edges", "custom"),
            ("kc-digit", "backlog"),
            ("kc-symbols", "backlog"),
        ],
    )
    .await;
    assert_accepts(
        &pool,
        "kanban_cards",
        &["backlog", "qa_test", "custom_state_2"],
    )
    .await;
    assert_rejects(
        &pool,
        "kanban_cards",
        "kanban_cards_status_slug_check",
        &["In Progress", "bad-state", "Done", "9lives", "_x", ""],
    )
    .await;

    migrate_through(&pool, 88).await;
    seed(
        &pool,
        "agents",
        &[
            ("ag89-archived", Some("archived")),
            ("ag89-idle", Some("idle")),
            ("ag89-working", Some("working")),
        ],
    )
    .await;
    migrate_through(&pool, 89).await;
    assert_statuses(
        &pool,
        "agents",
        &[
            ("ag89-archived", "idle"),
            ("ag89-idle", "idle"),
            ("ag89-working", "working"),
            ("ag-disabled", "idle"),
        ],
    )
    .await;
    assert_accepts(&pool, "agents", &["idle", "working"]).await;
    assert_rejects(
        &pool,
        "agents",
        "agents_status_known_check",
        &["archived", "active"],
    )
    .await;

    pool.close().await;
    fixture.drop().await;
}
