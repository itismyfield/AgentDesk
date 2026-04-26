use anyhow::Result;
use libsql_rusqlite::Connection;

/// Temporary SQLite compatibility shim for #868.
///
/// Runtime server startup now uses PostgreSQL via `db::postgres::connect_and_migrate`.
/// This module remains only because out-of-scope slices still call
/// `crate::db::schema::migrate` in tests and legacy helpers. The integration
/// cleanup should delete this module with the remaining `crate::db::Db` callers.
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kv_meta (
            key   TEXT PRIMARY KEY,
            value TEXT
        );",
    )?;
    Ok(())
}

/// Temporary compatibility stub for out-of-scope route tests that still call
/// the old SQLite auto-queue migration directly.
///
/// Remove this with the final SQLite/AppState integration slice after the
/// auto-queue routes and tests are fully Postgres-backed.
pub(crate) fn ensure_auto_queue_schema(_conn: &Connection) -> Result<()> {
    Ok(())
}
