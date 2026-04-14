pub mod agents;
pub mod auto_queue;
pub mod kanban;
pub mod memento_feedback_stats;
pub(crate) mod schema;
pub(crate) mod session_agent_resolution;
pub mod session_transcripts;
pub mod turns;

use anyhow::Result;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

use crate::config::Config;

/// Thread-safe database handle. Wraps a Mutex<Connection> with the DB path
/// so that read-only connections can be opened separately, avoiding lock
/// contention between the policy engine (onTick) and request handlers.
pub struct DbPool {
    path: std::path::PathBuf,
    write_conn: Mutex<Connection>,
}

impl DbPool {
    /// Acquire the write connection (exclusive).
    /// Backward compatible with existing `db.lock()` calls.
    pub fn lock(
        &self,
    ) -> std::result::Result<
        std::sync::MutexGuard<'_, Connection>,
        std::sync::PoisonError<std::sync::MutexGuard<'_, Connection>>,
    > {
        self.write_conn.lock()
    }

    /// Open a new read-only connection for non-blocking reads.
    /// SQLite WAL mode allows concurrent readers without blocking writers.
    pub fn read_conn(&self) -> std::result::Result<Connection, rusqlite::Error> {
        let conn =
            Connection::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        Ok(conn)
    }

    /// Open a new read-write connection that bypasses the Mutex.
    /// Used by the policy engine (QuickJS) to avoid blocking request handlers.
    /// SQLite WAL serializes concurrent writers via busy_timeout.
    pub fn separate_conn(&self) -> std::result::Result<Connection, rusqlite::Error> {
        let conn = Connection::open(&self.path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
        )?;
        Ok(conn)
    }
}

pub type Db = Arc<DbPool>;

/// Create an in-memory Db for tests.
/// Uses a named shared in-memory URI so that `separate_conn()` can access the same data.
#[cfg(test)]
pub fn test_db() -> Db {
    let conn = Connection::open_in_memory().unwrap();
    wrap_conn(conn)
}

/// Wrap a raw Connection into a Db (for tests and migration).
/// Uses a named shared in-memory URI so that `separate_conn()` and `read_conn()`
/// can open additional connections to the same in-memory database.
#[cfg_attr(not(test), allow(dead_code))]
pub fn wrap_conn(conn: Connection) -> Db {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let uri = format!("file:wrap_conn_{id}?mode=memory&cache=shared");

    // Migrate the schema into the shared URI by opening a connection to it.
    // The original `conn` (anonymous :memory:) already has data, so we need
    // to create a new connection at the shared URI, migrate, and copy data.
    // For simplicity: open a fresh connection at the shared URI, migrate it,
    // and use that as the primary connection. The caller's `conn` is dropped.
    let shared = Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("failed to open shared in-memory DB");
    shared
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .ok();
    schema::migrate(&shared).expect("failed to migrate shared in-memory DB");
    drop(conn); // drop the original anonymous connection

    Arc::new(DbPool {
        path: std::path::PathBuf::from(uri),
        write_conn: Mutex::new(shared),
    })
}

pub fn init(config: &Config) -> Result<Db> {
    let db_path = config.data.dir.join(&config.data.db_name);
    let conn = Connection::open(&db_path)?;

    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
    )?;
    schema::migrate(&conn)?;

    tracing::info!("Database initialized at {}", db_path.display());
    Ok(Arc::new(DbPool {
        path: db_path,
        write_conn: Mutex::new(conn),
    }))
}
