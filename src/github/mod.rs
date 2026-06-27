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

fn issue_state_with(
    adapter: &dyn GitHubAdapter,
    repo: &str,
    issue_number: i64,
) -> Result<String, String> {
    let issue_number = issue_number.to_string();
    adapter
        .run(&[
            "issue",
            "view",
            &issue_number,
            "--repo",
            repo,
            "--json",
            "state",
            "--jq",
            ".state",
        ])
        .map(|value| value.trim().to_string())
}

fn create_issue_with_options<'a>(
    adapter: &'a dyn GitHubAdapter,
    repo: &'a str,
    title: &'a str,
    body: &'a str,
    labels: &'a [String],
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
        let mut args = vec![
            "issue".to_string(),
            "create".to_string(),
            "--repo".to_string(),
            repo.to_string(),
            "--title".to_string(),
            title.to_string(),
            "--body-file".to_string(),
            body_file_arg,
        ];
        for label in labels
            .iter()
            .map(|label| label.trim())
            .filter(|label| !label.is_empty())
        {
            args.push("--label".to_string());
            args.push(label.to_string());
        }

        let url = adapter
            .run_async(
                args,
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

fn create_issue_with<'a>(
    adapter: &'a dyn GitHubAdapter,
    repo: &'a str,
    title: &'a str,
    body: &'a str,
) -> GitHubFuture<'a, Result<CreatedIssue, String>> {
    create_issue_with_options(adapter, repo, title, body, &[])
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

pub fn issue_state(repo: &str, issue_number: i64) -> Result<String, String> {
    issue_state_with(adapter(), repo, issue_number)
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

/// JSON payload returned by `gh pr view --json` for the fields used by the
/// `pr_summary` cache. We deserialize loosely so future GitHub API additions
/// do not break parsing.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PrView {
    pub number: i64,
    pub state: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub url: String,
    #[serde(default, rename = "isDraft")]
    pub is_draft: bool,
    #[serde(default, rename = "headRefOid")]
    pub head_ref_oid: Option<String>,
    #[serde(default, rename = "headRefName")]
    pub head_ref_name: Option<String>,
    #[serde(default, rename = "baseRefName")]
    pub base_ref_name: Option<String>,
    #[serde(default, rename = "mergeable")]
    pub mergeable: Option<String>,
    #[serde(default, rename = "mergeStateStatus")]
    pub merge_state_status: Option<String>,
    #[serde(default)]
    pub author: Option<serde_json::Value>,
    #[serde(default)]
    pub labels: Vec<sync::GhLabel>,
    #[serde(default)]
    pub files: Vec<serde_json::Value>,
    #[serde(default)]
    pub reviews: Vec<serde_json::Value>,
    #[serde(default)]
    pub comments: Vec<serde_json::Value>,
    #[serde(default, rename = "statusCheckRollup")]
    pub status_check_rollup: Vec<serde_json::Value>,
    #[serde(default, rename = "createdAt")]
    pub created_at: Option<String>,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: Option<String>,
    #[serde(default, rename = "mergedAt")]
    pub merged_at: Option<String>,
    #[serde(default, rename = "closedAt")]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub additions: Option<i64>,
    #[serde(default)]
    pub deletions: Option<i64>,
    #[serde(default, rename = "changedFiles")]
    pub changed_files: Option<i64>,
}

const PR_VIEW_JSON_FIELDS: &str = "number,state,title,body,url,isDraft,headRefOid,headRefName,baseRefName,mergeable,mergeStateStatus,author,labels,files,reviews,comments,statusCheckRollup,createdAt,updatedAt,mergedAt,closedAt,additions,deletions,changedFiles";

fn fetch_pr_view_with(
    adapter: &dyn GitHubAdapter,
    repo: &str,
    pr_number: i64,
) -> Result<PrView, String> {
    let number = pr_number.to_string();
    let output = adapter.run(&[
        "pr",
        "view",
        &number,
        "--repo",
        repo,
        "--json",
        PR_VIEW_JSON_FIELDS,
    ])?;
    serde_json::from_str(&output).map_err(|e| format!("parse gh pr view output: {e}"))
}

/// Fetch a full PR view from GitHub. This is the network-bound call wrapped by
/// the [`crate::services::pr_summary`] cache; production callers should
/// prefer the cache rather than invoking this directly.
pub fn fetch_pr_view(repo: &str, pr_number: i64) -> Result<PrView, String> {
    fetch_pr_view_with(adapter(), repo, pr_number)
}

/// Fetch only the head SHA + state of a PR. Used by the cache to cheaply
/// validate freshness — if the SHA matches the cached entry we can serve a
/// stale body without paying for the full payload.
// reason: pub gh-integration probe (PR head/state) wired by the runtime-gated
// PR-cache freshness path, not the default lib/test build. See #3034.
#[allow(dead_code)]
pub fn fetch_pr_head_state(repo: &str, pr_number: i64) -> Result<(Option<String>, String), String> {
    fetch_pr_head_state_with(adapter(), repo, pr_number)
}

// reason: private impl of the runtime-gated fetch_pr_head_state probe above. See #3034.
#[allow(dead_code)]
fn fetch_pr_head_state_with(
    adapter: &dyn GitHubAdapter,
    repo: &str,
    pr_number: i64,
) -> Result<(Option<String>, String), String> {
    let number = pr_number.to_string();
    let output = adapter.run(&[
        "pr",
        "view",
        &number,
        "--repo",
        repo,
        "--json",
        "headRefOid,state",
    ])?;
    let parsed: serde_json::Value =
        serde_json::from_str(&output).map_err(|e| format!("parse gh pr head state: {e}"))?;
    let head = parsed
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let state = parsed
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    Ok((head, state))
}

/// Reopen a GitHub issue given its full URL (e.g. https://github.com/owner/repo/issues/42).
pub async fn reopen_issue_by_url(url: &str) -> Result<(), String> {
    reopen_issue_by_url_with(adapter(), url).await
}

pub async fn create_issue(repo: &str, title: &str, body: &str) -> Result<CreatedIssue, String> {
    create_issue_with(adapter(), repo, title, body).await
}

pub async fn create_issue_with_labels(
    repo: &str,
    title: &str,
    body: &str,
    labels: &[String],
) -> Result<CreatedIssue, String> {
    create_issue_with_options(adapter(), repo, title, body, labels).await
}

fn parse_issue_number_from_url(url: &str) -> Option<i64> {
    static ISSUE_URL_RE: OnceLock<Regex> = OnceLock::new();
    ISSUE_URL_RE
        .get_or_init(|| Regex::new(r"/issues/(\d+)\s*$").expect("valid issue url regex"))
        .captures(url)
        .and_then(|caps| caps.get(1))
        .and_then(|value| value.as_str().parse::<i64>().ok())
}

// reason: production-build twin of the removed SQLite list_repos path; the live
// path is list_repos_pg, this stub keeps the symbol present for non-test builds.
// See #3034 (M2 dead-twin).
#[allow(dead_code)]
pub fn list_repos(_db: &Db) -> Result<Vec<RepoRow>, String> {
    Err("sqlite github repo registry is unavailable in production".to_string())
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

// reason: production-build twin of the removed SQLite register_repo path; the
// live path is db::postgres::register_repo, this stub keeps the symbol present
// for non-test builds. See #3034 (M2 dead-twin).
#[allow(dead_code)]
pub fn register_repo(_db: &Db, repo_id: &str) -> Result<RepoRow, String> {
    Err(format!(
        "sqlite github repo registry is unavailable in production for {repo_id}"
    ))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoRow {
    pub id: String,
    pub display_name: Option<String>,
    pub sync_enabled: bool,
    pub last_synced_at: Option<String>,
}
