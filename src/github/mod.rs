pub mod dod;
pub mod sync;
pub mod triage;

use crate::db::Db;
use crate::services::platform::binary_resolver::{
    apply_runtime_path, resolve_binary_with_login_shell,
};
use regex::Regex;
use sqlx::{PgPool, Row};
use std::fs::File;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const GH_PATH_OVERRIDE_ENV: &str = "AGENTDESK_GH_PATH";

type GitHubFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait GitHubAdapter: Send + Sync {
    fn is_available(&self) -> bool;
    fn run(&self, args: &[&str]) -> Result<String, String>;
    fn run_async<'a>(
        &'a self,
        args: Vec<String>,
        timeout: Duration,
        timeout_context: String,
    ) -> GitHubFuture<'a, Result<String, String>>;
}

#[derive(Debug, Default)]
struct GhCliAdapter;

#[cfg(windows)]
fn is_powershell_script(path: &str) -> bool {
    PathBuf::from(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("ps1"))
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_powershell_script(_path: &str) -> bool {
    false
}

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
    let mut command = if cfg!(windows) && is_powershell_script(&gh) {
        let mut command = std::process::Command::new("pwsh");
        command
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&gh);
        command
    } else {
        std::process::Command::new(&gh)
    };
    apply_runtime_path(&mut command);
    Ok(command)
}

fn tokio_gh_command() -> Result<tokio::process::Command, String> {
    let gh = gh_path().ok_or_else(|| "gh CLI is not available".to_string())?;
    let mut command = if cfg!(windows) && is_powershell_script(&gh) {
        let mut command = tokio::process::Command::new("pwsh");
        command
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&gh);
        command
    } else {
        tokio::process::Command::new(&gh)
    };
    if let Some(path) = crate::services::platform::merged_runtime_path() {
        command.env("PATH", path);
    }
    Ok(command)
}

impl GitHubAdapter for GhCliAdapter {
    fn is_available(&self) -> bool {
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

    fn run(&self, args: &[&str]) -> Result<String, String> {
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

    fn run_async<'a>(
        &'a self,
        args: Vec<String>,
        timeout: Duration,
        timeout_context: String,
    ) -> GitHubFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let mut command = tokio_gh_command()?;
            command.kill_on_drop(true);
            command.args(&args);
            let output = tokio::time::timeout(timeout, command.output())
                .await
                .map_err(|_| timeout_context)?
                .map_err(|err| format!("gh exec: {err}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!(
                    "gh exited with {}: {}",
                    output.status,
                    stderr.trim()
                ));
            }

            String::from_utf8(output.stdout).map_err(|e| format!("invalid utf8 from gh: {e}"))
        })
    }
}

fn adapter() -> &'static dyn GitHubAdapter {
    static ADAPTER: GhCliAdapter = GhCliAdapter;
    &ADAPTER
}

fn close_issue_with(
    adapter: &dyn GitHubAdapter,
    repo: &str,
    issue_number: i64,
) -> Result<(), String> {
    let issue_number = issue_number.to_string();
    adapter
        .run(&["issue", "close", &issue_number, "--repo", repo])
        .map(|_| ())
}

fn comment_issue_with(
    adapter: &dyn GitHubAdapter,
    repo: &str,
    issue_number: i64,
    body: &str,
) -> Result<(), String> {
    let issue_number = issue_number.to_string();
    adapter
        .run(&[
            "issue",
            "comment",
            &issue_number,
            "--repo",
            repo,
            "--body",
            body,
        ])
        .map(|_| ())
}

fn create_issue_with<'a>(
    adapter: &'a dyn GitHubAdapter,
    repo: &'a str,
    title: &'a str,
    body: &'a str,
) -> GitHubFuture<'a, Result<CreatedIssue, String>> {
    Box::pin(async move {
        let repo = repo.trim();
        let title = title.trim();
        let body = body.trim();
        if repo.is_empty() {
            return Err("repo is required".to_string());
        }
        if title.is_empty() {
            return Err("title is required".to_string());
        }

        let body_file_path = write_issue_body_temp_file(body)?;
        let body_file_arg = body_file_path.to_string_lossy().to_string();

        let url = adapter
            .run_async(
                vec![
                    "issue".to_string(),
                    "create".to_string(),
                    "--repo".to_string(),
                    repo.to_string(),
                    "--title".to_string(),
                    title.to_string(),
                    "--body-file".to_string(),
                    body_file_arg,
                ],
                Duration::from_secs(10),
                format!("gh issue create timed out after 10s: {repo}"),
            )
            .await;
        let _ = std::fs::remove_file(&body_file_path);
        let url = url?.trim().to_string();
        let number = parse_issue_number_from_url(&url)
            .ok_or_else(|| format!("gh issue create returned unparseable url: {url}"))?;

        Ok(CreatedIssue { number, url })
    })
}

fn write_issue_body_temp_file(body: &str) -> Result<PathBuf, String> {
    for attempt in 0..8 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "agentdesk-gh-issue-body-{}-{now}-{attempt}.md",
            std::process::id()
        ));

        match File::options().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(body.as_bytes())
                    .map_err(|err| format!("gh body temp write: {err}"))?;
                file.flush()
                    .map_err(|err| format!("gh body temp flush: {err}"))?;
                return Ok(path);
            }
            Err(_) => continue,
        }
    }

    Err("gh body temp file: unable to allocate unique path".to_string())
}

fn parse_issue_locator_from_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .ok_or_else(|| format!("not a github url: {url}"))?;
    let slash_pos = rest
        .find("/issues/")
        .ok_or_else(|| format!("no /issues/ segment in {url}"))?;
    let repo = rest[..slash_pos].to_string();
    let number = rest[slash_pos + "/issues/".len()..].to_string();
    if number.is_empty() {
        return Err(format!("missing issue number in {url}"));
    }
    Ok((repo, number))
}

fn reopen_issue_by_url_with<'a>(
    adapter: &'a dyn GitHubAdapter,
    url: &'a str,
) -> GitHubFuture<'a, Result<(), String>> {
    Box::pin(async move {
        let (repo, number) = parse_issue_locator_from_url(url)?;

        adapter
            .run_async(
                vec![
                    "issue".to_string(),
                    "reopen".to_string(),
                    number.clone(),
                    "--repo".to_string(),
                    repo.clone(),
                ],
                Duration::from_secs(5),
                format!("gh issue reopen timed out after 5s: {repo}#{number}"),
            )
            .await?;
        Ok(())
    })
}

/// Check whether the `gh` CLI is available on this system.
pub fn gh_available() -> bool {
    adapter().is_available()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedIssue {
    pub number: i64,
    pub url: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DashboardRepo {
    #[serde(rename = "nameWithOwner")]
    pub name_with_owner: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "isPrivate")]
    pub is_private: bool,
    #[serde(rename = "viewerPermission")]
    pub viewer_permission: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct IssueListEntry {
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub url: String,
    #[serde(default)]
    pub labels: Vec<sync::GhLabel>,
    #[serde(default)]
    pub assignees: Vec<serde_json::Value>,
    #[serde(rename = "createdAt")]
    pub created_at: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct IssueView {
    pub number: i64,
    pub state: String,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    #[serde(default)]
    pub labels: Vec<sync::GhLabel>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct IssueComments {
    #[serde(default)]
    pub comments: Vec<serde_json::Value>,
    pub body: Option<String>,
}

pub fn close_issue(repo: &str, issue_number: i64) -> Result<(), String> {
    close_issue_with(adapter(), repo, issue_number)
}

pub fn comment_issue(repo: &str, issue_number: i64, body: &str) -> Result<(), String> {
    comment_issue_with(adapter(), repo, issue_number, body)
}

fn viewer_login_with(adapter: &dyn GitHubAdapter) -> Result<String, String> {
    adapter
        .run(&["api", "user", "--jq", ".login"])
        .map(|value| value.trim().to_string())
}

pub fn viewer_login() -> Result<String, String> {
    viewer_login_with(adapter())
}

fn list_dashboard_repos_with(adapter: &dyn GitHubAdapter) -> Result<Vec<DashboardRepo>, String> {
    let raw = adapter.run(&[
        "api",
        "user/repos",
        "--paginate",
        "--jq",
        r#"[.[] | {nameWithOwner: .full_name, updatedAt: .updated_at, isPrivate: .private, viewerPermission: .permissions.admin}]"#,
    ])?;

    let mut repos = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let page = serde_json::from_str::<Vec<DashboardRepo>>(line)
            .map_err(|e| format!("parse gh repo list output: {e}"))?;
        repos.extend(page);
    }

    Ok(repos)
}

pub fn list_dashboard_repos() -> Result<Vec<DashboardRepo>, String> {
    list_dashboard_repos_with(adapter())
}

fn list_issue_summaries_with(
    adapter: &dyn GitHubAdapter,
    repo: &str,
    state: &str,
    limit: u32,
) -> Result<Vec<IssueListEntry>, String> {
    let raw = adapter.run(&[
        "issue",
        "list",
        "--repo",
        repo,
        "--state",
        state,
        "--limit",
        &limit.to_string(),
        "--json",
        "number,title,body,state,url,labels,assignees,createdAt,updatedAt",
    ])?;
    serde_json::from_str(&raw).map_err(|e| format!("parse gh issue list output: {e}"))
}

pub fn list_issue_summaries(
    repo: &str,
    state: &str,
    limit: u32,
) -> Result<Vec<IssueListEntry>, String> {
    list_issue_summaries_with(adapter(), repo, state, limit)
}

fn fetch_issue_with(
    adapter: &dyn GitHubAdapter,
    repo: &str,
    issue_number: i64,
) -> Result<IssueView, String> {
    let number = issue_number.to_string();
    let output = adapter.run(&[
        "issue",
        "view",
        &number,
        "--repo",
        repo,
        "--json",
        "number,state,title,body,labels,url",
    ])?;
    serde_json::from_str(&output).map_err(|e| format!("parse gh issue view output: {e}"))
}

pub fn fetch_issue(repo: &str, issue_number: i64) -> Result<IssueView, String> {
    fetch_issue_with(adapter(), repo, issue_number)
}

fn fetch_issue_comments_with(
    adapter: &dyn GitHubAdapter,
    repo: &str,
    issue_number: i64,
) -> Result<IssueComments, String> {
    let number = issue_number.to_string();
    let output = adapter.run(&[
        "issue",
        "view",
        &number,
        "--repo",
        repo,
        "--json",
        "comments,body",
    ])?;
    serde_json::from_str(&output).map_err(|e| format!("parse gh issue comments output: {e}"))
}

pub fn fetch_issue_comments(repo: &str, issue_number: i64) -> Result<IssueComments, String> {
    fetch_issue_comments_with(adapter(), repo, issue_number)
}

/// Reopen a GitHub issue given its full URL (e.g. https://github.com/owner/repo/issues/42).
pub async fn reopen_issue_by_url(url: &str) -> Result<(), String> {
    reopen_issue_by_url_with(adapter(), url).await
}

pub async fn create_issue(repo: &str, title: &str, body: &str) -> Result<CreatedIssue, String> {
    create_issue_with(adapter(), repo, title, body).await
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

pub async fn list_repos_pg(pool: &PgPool) -> Result<Vec<RepoRow>, String> {
    let rows = sqlx::query(
        "SELECT id, display_name, sync_enabled, last_synced_at::text AS last_synced_at
         FROM github_repos
         ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("list github_repos pg: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| RepoRow {
            id: row.try_get::<String, _>("id").unwrap_or_default(),
            display_name: row
                .try_get::<Option<String>, _>("display_name")
                .ok()
                .flatten(),
            sync_enabled: row
                .try_get::<Option<bool>, _>("sync_enabled")
                .ok()
                .flatten()
                .unwrap_or(true),
            last_synced_at: row
                .try_get::<Option<String>, _>("last_synced_at")
                .ok()
                .flatten(),
        })
        .collect())
}

/// Register a new repo (or update display_name if already exists).
pub fn register_repo(db: &Db, repo_id: &str) -> Result<RepoRow, String> {
    let conn = db.lock().map_err(|e| format!("db lock: {e}"))?;
    conn.execute(
        "INSERT OR IGNORE INTO github_repos (id, display_name, sync_enabled) VALUES (?1, ?1, 1)",
        [repo_id],
    )
    .map_err(|e| format!("insert: {e}"))?;
    crate::db::schema::seed_builtin_pipeline_stages(&conn)
        .map_err(|e| format!("seed builtin pipeline stages: {e}"))?;

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
pub(crate) mod test_utils {
    use super::{GitHubAdapter, GitHubFuture};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Debug, Default)]
    pub(crate) struct RecordingAdapter {
        calls: Mutex<Vec<Vec<String>>>,
        sync_responses: Mutex<VecDeque<Result<String, String>>>,
        async_responses: Mutex<VecDeque<Result<String, String>>>,
    }

    impl RecordingAdapter {
        pub(crate) fn with_sync_responses(sync_responses: Vec<Result<String, String>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                sync_responses: Mutex::new(sync_responses.into()),
                async_responses: Mutex::new(VecDeque::new()),
            }
        }

        pub(crate) fn with_async_responses(async_responses: Vec<Result<String, String>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                sync_responses: Mutex::new(VecDeque::new()),
                async_responses: Mutex::new(async_responses.into()),
            }
        }

        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GitHubAdapter for RecordingAdapter {
        fn is_available(&self) -> bool {
            true
        }

        fn run(&self, args: &[&str]) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            self.sync_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()))
        }

        fn run_async<'a>(
            &'a self,
            args: Vec<String>,
            _timeout: Duration,
            _timeout_context: String,
        ) -> GitHubFuture<'a, Result<String, String>> {
            self.calls.lock().unwrap().push(args);
            let response = self
                .async_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()));
            Box::pin(async move { response })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_utils::RecordingAdapter;
    use super::*;

    fn test_db() -> Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
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

    #[test]
    fn close_issue_routes_through_adapter_interface() {
        let adapter = RecordingAdapter::with_sync_responses(vec![Ok(String::new())]);
        close_issue_with(&adapter, "owner/repo", 42).unwrap();

        assert_eq!(
            adapter.calls(),
            vec![vec![
                "issue".to_string(),
                "close".to_string(),
                "42".to_string(),
                "--repo".to_string(),
                "owner/repo".to_string(),
            ]]
        );
    }

    #[test]
    fn comment_issue_routes_through_adapter_interface() {
        let adapter = RecordingAdapter::with_sync_responses(vec![Ok(String::new())]);
        comment_issue_with(&adapter, "owner/repo", 7, "body text").unwrap();

        assert_eq!(
            adapter.calls(),
            vec![vec![
                "issue".to_string(),
                "comment".to_string(),
                "7".to_string(),
                "--repo".to_string(),
                "owner/repo".to_string(),
                "--body".to_string(),
                "body text".to_string(),
            ]]
        );
    }

    #[tokio::test]
    async fn create_issue_routes_through_adapter_interface() {
        let adapter = RecordingAdapter::with_async_responses(vec![Ok(
            "https://github.com/itismyfield/AgentDesk/issues/458\n".to_string(),
        )]);

        let created = create_issue_with(
            &adapter,
            "itismyfield/AgentDesk",
            "Refactor gh adapter",
            "Body",
        )
        .await
        .unwrap();

        assert_eq!(created.number, 458);
        assert_eq!(
            created.url,
            "https://github.com/itismyfield/AgentDesk/issues/458"
        );
        let calls = adapter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0][..7],
            [
                "issue".to_string(),
                "create".to_string(),
                "--repo".to_string(),
                "itismyfield/AgentDesk".to_string(),
                "--title".to_string(),
                "Refactor gh adapter".to_string(),
                "--body-file".to_string(),
            ]
        );
        assert_eq!(calls[0].len(), 8);
        assert!(calls[0][7].contains("agentdesk-gh-issue-body-"));
    }

    #[tokio::test]
    async fn reopen_issue_routes_through_adapter_interface() {
        let adapter = RecordingAdapter::with_async_responses(vec![Ok(String::new())]);

        reopen_issue_by_url_with(
            &adapter,
            "https://github.com/itismyfield/AgentDesk/issues/458",
        )
        .await
        .unwrap();

        assert_eq!(
            adapter.calls(),
            vec![vec![
                "issue".to_string(),
                "reopen".to_string(),
                "458".to_string(),
                "--repo".to_string(),
                "itismyfield/AgentDesk".to_string(),
            ]]
        );
    }
}
