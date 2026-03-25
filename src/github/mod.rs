pub mod dod;
pub mod sync;
pub mod triage;

use crate::db::Db;

/// Check whether the `gh` CLI is available on this system.
pub fn gh_available() -> bool {
    std::process::Command::new("gh")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a `gh` CLI command and return its stdout as a String.
/// Returns an error if the command fails or is not available.
pub(crate) fn run_gh(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| format!("gh command failed to execute: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "gh exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    String::from_utf8(output.stdout).map_err(|e| format!("invalid utf8 from gh: {e}"))
}

/// List all registered repos from the database.
pub fn list_repos(db: &Db) -> Result<Vec<RepoRow>, String> {
    let conn = db.lock().map_err(|e| format!("db lock: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, display_name, sync_enabled, last_synced_at FROM github_repos ORDER BY id",
        )
        .map_err(|e| format!("prepare: {e}"))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(RepoRow {
                id: row.get(0)?,
                display_name: row.get(1)?,
                sync_enabled: row.get(2)?,
                last_synced_at: row.get(3)?,
            })
        })
        .map_err(|e| format!("query: {e}"))?;

    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Register a new repo (or update display_name if already exists).
pub fn register_repo(db: &Db, repo_id: &str) -> Result<RepoRow, String> {
    let conn = db.lock().map_err(|e| format!("db lock: {e}"))?;
    conn.execute(
        "INSERT OR IGNORE INTO github_repos (id, display_name, sync_enabled) VALUES (?1, ?1, 1)",
        [repo_id],
    )
    .map_err(|e| format!("insert: {e}"))?;

    let row = conn
        .query_row(
            "SELECT id, display_name, sync_enabled, last_synced_at FROM github_repos WHERE id = ?1",
            [repo_id],
            |row| {
                Ok(RepoRow {
                    id: row.get(0)?,
                    display_name: row.get(1)?,
                    sync_enabled: row.get(2)?,
                    last_synced_at: row.get(3)?,
                })
            },
        )
        .map_err(|e| format!("readback: {e}"))?;

    Ok(row)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoRow {
    pub id: String,
    pub display_name: Option<String>,
    pub sync_enabled: bool,
    pub last_synced_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    #[test]
    fn register_and_list_repos() {
        let db = test_db();
        assert!(list_repos(&db).unwrap().is_empty());

        register_repo(&db, "owner/repo1").unwrap();
        register_repo(&db, "owner/repo2").unwrap();

        let repos = list_repos(&db).unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].id, "owner/repo1");
        assert_eq!(repos[1].id, "owner/repo2");
    }

    #[test]
    fn register_repo_idempotent() {
        let db = test_db();
        register_repo(&db, "owner/repo1").unwrap();
        register_repo(&db, "owner/repo1").unwrap();

        let repos = list_repos(&db).unwrap();
        assert_eq!(repos.len(), 1);
    }
}
