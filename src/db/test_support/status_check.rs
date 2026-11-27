use sqlx::{AssertSqlSafe, PgPool};

/// Runs `sql` with `<table>_status_check` lifted and re-adds the same NOT VALID definition,
/// leaving a row an older binary could have written before the check existed.
pub async fn execute_with_status_check_lifted(pool: &PgPool, table: &str, sql: &str) {
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
