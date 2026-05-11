pub mod agents;
pub mod auto_queue;
pub mod automation_candidates;
pub mod cancel_tombstones;
pub mod dispatch_semaphores;
pub mod dispatched_sessions;
pub mod dispatches;
pub mod intake_outbox;
pub mod kanban;
pub mod kanban_cards;
pub mod memento_feedback_stats;
pub mod postgres;
pub mod prompt_manifests;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) mod schema;
pub(crate) mod session_agent_resolution;
pub mod session_observability;
pub mod session_status;
pub mod session_transcripts;
pub mod table_metadata;
pub mod turns;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use sqlite_test::Connection;
#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
use std::sync::Arc;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use std::sync::{Arc, Mutex, MutexGuard};

/// Thread-safe SQLite handle keyed by DB path.
/// A lightweight mutex serializes write openings while readers and separate
/// writers reopen their own connections against the same WAL-backed store.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub struct TestSqliteDb {
    path: std::path::PathBuf,
    write_gate: Mutex<()>,
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
#[derive(Debug)]
pub enum LegacySqliteDisabled {}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
#[derive(Debug, Clone, Copy)]
pub struct LegacySqliteError;

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
impl std::fmt::Display for LegacySqliteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "legacy sqlite backend is unavailable in production")
    }
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
impl std::error::Error for LegacySqliteError {}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
pub struct LegacySqliteConnection;

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
pub struct LegacySqliteStatement;

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
pub struct LegacySqliteRows;

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
pub struct LegacySqliteRow;

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
impl LegacySqliteDisabled {
    pub fn lock(&self) -> Result<LegacySqliteConnection, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    pub fn read_conn(&self) -> Result<LegacySqliteConnection, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    pub fn separate_conn(&self) -> Result<LegacySqliteConnection, LegacySqliteError> {
        Err(LegacySqliteError)
    }
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
impl LegacySqliteConnection {
    pub fn execute<P>(&self, _sql: &str, _params: P) -> Result<usize, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    pub fn execute_batch(&self, _sql: &str) -> Result<(), LegacySqliteError> {
        Err(LegacySqliteError)
    }

    pub fn prepare(&self, _sql: &str) -> Result<LegacySqliteStatement, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    pub fn query_row<P, F, T>(&self, _sql: &str, _params: P, _f: F) -> Result<T, LegacySqliteError>
    where
        F: FnOnce(&LegacySqliteRow) -> Result<T, LegacySqliteError>,
    {
        Err(LegacySqliteError)
    }
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
impl LegacySqliteStatement {
    pub fn query<P>(&mut self, _params: P) -> Result<LegacySqliteRows, LegacySqliteError> {
        Err(LegacySqliteError)
    }

    pub fn query_map<P, F, T>(
        &mut self,
        _params: P,
        _f: F,
    ) -> Result<std::vec::IntoIter<Result<T, LegacySqliteError>>, LegacySqliteError>
    where
        F: FnMut(&LegacySqliteRow) -> Result<T, LegacySqliteError>,
    {
        Err(LegacySqliteError)
    }
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
impl LegacySqliteRows {
    pub fn next(&mut self) -> Result<Option<LegacySqliteRow>, LegacySqliteError> {
        Err(LegacySqliteError)
    }
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
impl LegacySqliteRow {
    pub fn get<I, T: Default>(&self, _idx: I) -> Result<T, LegacySqliteError> {
        Err(LegacySqliteError)
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[derive(Debug)]
pub enum DbLockError {
    Poisoned,
    Open(sqlite_test::Error),
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
impl std::fmt::Display for DbLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Poisoned => write!(f, "db write gate poisoned"),
            Self::Open(error) => write!(f, "open sqlite write connection: {error}"),
        }
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
impl std::error::Error for DbLockError {}

/// Fresh SQLite write connection guarded by the per-DB write gate.
/// The connection field is declared before the gate so the connection is
/// dropped before the mutex unlocks, keeping write serialization intact.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub struct DbWriteGuard<'a> {
    conn: Connection,
    _write_gate: MutexGuard<'a, ()>,
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
impl std::ops::Deref for DbWriteGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
impl std::ops::DerefMut for DbWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
impl TestSqliteDb {
    /// Acquire the write connection (exclusive).
    /// Backward compatible with existing `db.lock()` calls.
    pub fn lock(&self) -> std::result::Result<DbWriteGuard<'_>, DbLockError> {
        let write_gate = self.write_gate.lock().map_err(|_| DbLockError::Poisoned)?;
        let conn = open_write_connection(&self.path).map_err(DbLockError::Open)?;
        Ok(DbWriteGuard {
            conn,
            _write_gate: write_gate,
        })
    }

    /// Open a new read-only connection for non-blocking reads.
    /// SQLite WAL mode allows concurrent readers without blocking writers.
    pub fn read_conn(&self) -> std::result::Result<Connection, sqlite_test::Error> {
        open_read_only_connection(&self.path)
    }

    /// Open a new read-write connection that bypasses the Mutex.
    /// Used by the policy engine (QuickJS) to avoid blocking request handlers.
    /// SQLite WAL serializes concurrent writers via busy_timeout.
    pub fn separate_conn(&self) -> std::result::Result<Connection, sqlite_test::Error> {
        open_write_connection(&self.path)
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn open_read_only_connection(
    path: &std::path::Path,
) -> std::result::Result<Connection, sqlite_test::Error> {
    let conn = Connection::open_with_flags(
        path,
        sqlite_test::OpenFlags::SQLITE_OPEN_READ_ONLY | sqlite_test::OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA busy_timeout=5000;")?;
    Ok(conn)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn open_write_connection(
    path: &std::path::Path,
) -> std::result::Result<Connection, sqlite_test::Error> {
    let conn = Connection::open_with_flags(
        path,
        sqlite_test::OpenFlags::SQLITE_OPEN_READ_WRITE
            | sqlite_test::OpenFlags::SQLITE_OPEN_CREATE
            | sqlite_test::OpenFlags::SQLITE_OPEN_URI
            | sqlite_test::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
    )?;
    Ok(conn)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub type Db = Arc<TestSqliteDb>;

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
pub type Db = Arc<LegacySqliteDisabled>;

/// Create an in-memory Db for tests.
/// The wrapped Db uses a unique file-backed SQLite path so `read_conn()` and
/// `separate_conn()` can reopen it without keeping a shared-memory anchor alive.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn test_db() -> Db {
    let conn = Connection::open_in_memory().unwrap();
    wrap_conn(conn)
}

/// Wrap a raw Connection into a Db (for tests and migration).
/// The source connection is checkpointed into a unique temp SQLite file so
/// subsequent connections can reopen the same store without a resident anchor.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn wrap_conn(conn: Connection) -> Db {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("agentdesk_wrap_conn_{id}.sqlite3"));
    let _ = std::fs::remove_file(&path);
    let escaped_path = path.display().to_string().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM main INTO '{escaped_path}'"))
        .expect("failed to checkpoint sqlite test db");
    drop(conn);

    let reopened = open_write_connection(&path).expect("failed to reopen wrapped sqlite db");
    schema::migrate(&reopened).expect("failed to migrate wrapped sqlite db");
    drop(reopened);

    Arc::new(TestSqliteDb {
        path,
        write_gate: Mutex::new(()),
    })
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[test]
    fn lock_and_read_conn_share_the_same_store() {
        let db = test_db();

        {
            let conn = db.lock().unwrap();
            conn.execute_batch(
                "CREATE TABLE db_pool_guard_test (
                    id INTEGER PRIMARY KEY,
                    value TEXT NOT NULL
                );
                INSERT INTO db_pool_guard_test (value) VALUES ('first');",
            )
            .unwrap();
        }

        let read_conn = db.read_conn().unwrap();
        let count: i64 = read_conn
            .query_row("SELECT COUNT(*) FROM db_pool_guard_test", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn separate_conn_observes_writes_from_lock() {
        let db = test_db();

        {
            let conn = db.lock().unwrap();
            conn.execute_batch(
                "CREATE TABLE db_pool_separate_conn_test (
                    id INTEGER PRIMARY KEY,
                    value TEXT NOT NULL
                );
                INSERT INTO db_pool_separate_conn_test (value) VALUES ('first');",
            )
            .unwrap();
        }

        {
            let separate = db.separate_conn().unwrap();
            separate
                .execute(
                    "INSERT INTO db_pool_separate_conn_test (value) VALUES (?1)",
                    ["second"],
                )
                .unwrap();
        }

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM db_pool_separate_conn_test",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }
}
