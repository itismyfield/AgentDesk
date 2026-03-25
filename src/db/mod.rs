pub mod agents;
pub(crate) mod schema;

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
        let conn = Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        Ok(conn)
    }

    /// Open a new read-write connection that bypasses the Mutex.
    /// Used by the policy engine (QuickJS) to avoid blocking request handlers.
    /// SQLite WAL serializes concurrent writers via busy_timeout.
    pub fn separate_conn(&self) -> std::result::Result<Connection, rusqlite::Error> {
        let conn = Connection::open_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
        Ok(conn)
    }
}

pub type Db = Arc<DbPool>;

/// Create an in-memory Db for tests.
#[cfg(test)]
pub fn test_db() -> Db {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;").ok();
    schema::migrate(&conn).unwrap();
    Arc::new(DbPool {
        path: std::path::PathBuf::from(":memory:"),
        write_conn: Mutex::new(conn),
    })
}

/// Wrap a raw Connection into a Db (for tests and migration).
pub fn wrap_conn(conn: Connection) -> Db {
    Arc::new(DbPool {
        path: std::path::PathBuf::from(":memory:"),
        write_conn: Mutex::new(conn),
    })
}

pub fn init(config: &Config) -> Result<Db> {
    let db_path = config.data.dir.join(&config.data.db_name);
    let conn = Connection::open(&db_path)?;

    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
    schema::migrate(&conn)?;

    tracing::info!("Database initialized at {}", db_path.display());
    Ok(Arc::new(DbPool {
        path: db_path,
        write_conn: Mutex::new(conn),
    }))
}
