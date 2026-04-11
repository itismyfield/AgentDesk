pub mod dod;
pub mod sync;
pub mod triage;

use crate::db::Db;
use crate::services::platform::binary_resolver::{
    apply_runtime_path, resolve_binary_with_login_shell,
};
use regex::Regex;
use std::path::PathBuf;
use std::sync::OnceLock;

const GH_PATH_OVERRIDE_ENV: &str = "AGENTDESK_GH_PATH";

fn gh_path() -> Option<String> {
    if let Some(override_path) = std::env::var_os(GH_PATH_OVERRIDE_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(override_path).to_string_lossy().to_string());
    }

    static GH_PATH: OnceLock<Option<String>> = OnceLock::new();
    GH_PATH
        .get_or_init(|| resolve_binary_with_login_shell("gh"))
        .clone()
}

fn gh_command() -> Result<std::process::Command, String> {
    let gh = gh_path().ok_or_else(|| "gh CLI is not available".to_string())?;
    let mut command = std::process::Command::new(&gh);
    apply_runtime_path(&mut command);
    Ok(command)
}

fn tokio_gh_command() -> Result<tokio::process::Command, String> {
    let gh = gh_path().ok_or_else(|| "gh CLI is not available".to_string())?;
    let mut command = tokio::process::Command::new(&gh);
    if let Some(path) = crate::services::platform::merged_runtime_path() {
        command.env("PATH", path);
    }
    Ok(command)
}

/// Check whether the `gh` CLI is available on this system.
pub fn gh_available() -> bool {
    let Ok(mut command) = gh_command() else {
        return false;
    };
    command
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedIssue {
    pub number: i64,
    pub url: String,
}

/// Run a `gh` CLI command and return its stdout as a String.
/// Returns an error if the command fails or is not available.
pub(crate) fn run_gh(args: &[&str]) -> Result<String, String> {
    let output = gh_command()?
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

/// Reopen a GitHub issue given its full URL (e.g. https://github.com/owner/repo/issues/42).
pub async fn reopen_issue_by_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .ok_or_else(|| format!("not a github url: {url}"))?;
    let slash_pos = rest
        .find("/issues/")
        .ok_or_else(|| format!("no /issues/ segment in {url}"))?;
    let repo = &rest[..slash_pos];
    let number = &rest[slash_pos + "/issues/".len()..];

    // gh issue reopen <number> --repo <owner/repo>
    let mut cmd = tokio_gh_command()?;
    cmd.kill_on_drop(true);
    cmd.args(["issue", "reopen", number, "--repo", repo]);
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output())
        .await
        .map_err(|_| format!("gh issue reopen timed out after 5s: {repo}#{number}"))?
        .map_err(|e| format!("gh exec: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh issue reopen failed: {}", stderr.trim()));
    }
    Ok(())
}

pub async fn create_issue(repo: &str, title: &str, body: &str) -> Result<CreatedIssue, String> {
    let repo = repo.trim();
    let title = title.trim();
    let body = body.trim();
    if repo.is_empty() {
        return Err("repo is required".to_string());
    }
    if title.is_empty() {
        return Err("title is required".to_string());
    }

    let mut cmd = tokio_gh_command()?;
    cmd.kill_on_drop(true);
    cmd.args([
        "issue", "create", "--repo", repo, "--title", title, "--body", body,
    ]);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| format!("gh issue create timed out after 10s: {repo}"))?
        .map_err(|err| format!("gh exec: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh issue create failed: {}", stderr.trim()));
    }

    let url = String::from_utf8(output.stdout)
        .map_err(|err| format!("invalid utf8 from gh issue create: {err}"))?
        .trim()
        .to_string();
    let number = parse_issue_number_from_url(&url)
        .ok_or_else(|| format!("gh issue create returned unparseable url: {url}"))?;

    Ok(CreatedIssue { number, url })
}

fn parse_issue_number_from_url(url: &str) -> Option<i64> {
    static ISSUE_URL_RE: OnceLock<Regex> = OnceLock::new();
    ISSUE_URL_RE
        .get_or_init(|| Regex::new(r"/issues/(\d+)\s*$").expect("valid issue url regex"))
        .captures(url)
        .and_then(|caps| caps.get(1))
        .and_then(|value| value.as_str().parse::<i64>().ok())
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

    #[test]
    fn parse_issue_number_from_url_reads_numeric_suffix() {
        assert_eq!(
            parse_issue_number_from_url("https://github.com/itismyfield/AgentDesk/issues/427"),
            Some(427)
        );
        assert_eq!(
            parse_issue_number_from_url("https://example.com/not-an-issue"),
            None
        );
    }
}
