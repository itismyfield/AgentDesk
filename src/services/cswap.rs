use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{Mutex, RwLock};

use crate::runtime_layout::expand_user_path;
use crate::services::platform::binary_resolver::{
    async_resolve_binary_with_login_shell, merged_runtime_path,
};

const CSWAP_PATH_OVERRIDE_ENV: &str = "AGENTDESK_CSWAP_PATH";
const LIST_CACHE_TTL: Duration = Duration::from_secs(60);
const LIST_TIMEOUT: Duration = Duration::from_secs(15);
const SWITCH_TIMEOUT: Duration = Duration::from_secs(20);
const SWITCH_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

type CswapFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum CswapError {
    #[error("cswap is not installed or is not executable")]
    NotInstalled,
    #[error("cswap {operation} timed out after {timeout_secs}s")]
    Timeout {
        operation: &'static str,
        timeout_secs: u64,
    },
    #[error("cswap exited with {status}: {stderr}")]
    CommandFailed {
        status: String,
        stderr: String,
        stdout: String,
    },
    #[error("cswap exec failed: {0}")]
    Exec(String),
    #[error("invalid utf8 from cswap: {0}")]
    InvalidUtf8(String),
    #[error("invalid cswap JSON: {0}")]
    Json(String),
    #[error("account is required")]
    AccountRequired,
    #[error("another Claude account switch is already in progress")]
    SwitchInProgress,
}

impl CswapError {
    pub fn code(&self) -> &'static str {
        match self {
            CswapError::NotInstalled => "not_installed",
            CswapError::Timeout { .. } => "timeout",
            CswapError::CommandFailed { .. } => "execution_failure",
            CswapError::Exec(_) => "execution_failure",
            CswapError::InvalidUtf8(_) => "execution_failure",
            CswapError::Json(_) => "execution_failure",
            CswapError::AccountRequired => "bad_request",
            CswapError::SwitchInProgress => "switch_in_progress",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccountUsageWindow {
    pub pct: Option<f64>,
    pub resets_at: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccountUsage {
    pub five_hour: Option<ClaudeAccountUsageWindow>,
    pub seven_day: Option<ClaudeAccountUsageWindow>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccount {
    pub number: Option<u64>,
    pub email: Option<String>,
    #[serde(default)]
    pub active: bool,
    pub usage_status: Option<String>,
    pub usage: Option<ClaudeAccountUsage>,
    pub usage_fetched_at: Option<String>,
    pub usage_age_seconds: Option<u64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CswapListPayload {
    schema_version: Option<u64>,
    active_account_number: Option<u64>,
    accounts: Option<Vec<ClaudeAccount>>,
    error: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccountsResponse {
    pub schema_version: u64,
    pub status: ClaudeAccountsStatus,
    pub hostname: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub served_at: DateTime<Utc>,
    pub cache_ttl_seconds: u64,
    pub usage_data_stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
    pub active_account_number: Option<u64>,
    pub accounts: Vec<ClaudeAccount>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeAccountsStatus {
    Ok,
    UsageDataStale,
}

#[derive(Debug, Clone)]
struct CachedList {
    response: ClaudeAccountsResponse,
    cached_at: Instant,
}

#[derive(Debug, Clone)]
struct CompletedListFetch {
    completed_at: Instant,
    result: Result<ClaudeAccountsResponse, CswapError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CswapSwitchResult {
    #[serde(default)]
    pub switched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub(crate) trait CswapAdapter: Send + Sync {
    fn run<'a>(
        &'a self,
        args: Vec<String>,
        timeout: Duration,
        operation: &'static str,
    ) -> CswapFuture<'a, Result<String, CswapError>>;
}

#[derive(Debug, Default)]
struct CswapCliAdapter;

fn normalize_override_path(raw: &str) -> Option<PathBuf> {
    let expanded = expand_user_path(raw).unwrap_or_else(|| PathBuf::from(raw));
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().ok()?.join(expanded)
    };
    Some(absolute)
}

fn existing_absolute_path(path: impl AsRef<Path>) -> Option<String> {
    let path = path.as_ref();
    if !path.is_file() {
        return None;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    Some(
        std::fs::canonicalize(&absolute)
            .unwrap_or(absolute)
            .to_string_lossy()
            .to_string(),
    )
}

async fn resolve_cswap_path() -> Option<String> {
    if let Ok(raw_override) = std::env::var(CSWAP_PATH_OVERRIDE_ENV) {
        if let Some(path) = normalize_override_path(raw_override.trim()) {
            return existing_absolute_path(path);
        }
    }

    static CSWAP_PATH: OnceLock<String> = OnceLock::new();
    if let Some(cached) = CSWAP_PATH.get() {
        return Some(cached.clone());
    }
    let resolved = async_resolve_binary_with_login_shell("cswap").await;
    let absolute = resolved
        .as_deref()
        .and_then(|path| existing_absolute_path(PathBuf::from(path)));
    if let Some(path) = absolute.as_ref() {
        let _ = CSWAP_PATH.set(path.clone());
    }
    absolute
}

impl CswapAdapter for CswapCliAdapter {
    fn run<'a>(
        &'a self,
        args: Vec<String>,
        timeout: Duration,
        operation: &'static str,
    ) -> CswapFuture<'a, Result<String, CswapError>> {
        Box::pin(async move {
            let path = resolve_cswap_path().await.ok_or(CswapError::NotInstalled)?;
            let mut command = tokio::process::Command::new(&path);
            command.kill_on_drop(true);
            command.args(args);
            if let Some(path) = merged_runtime_path() {
                command.env("PATH", path);
            }
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
            configure_cswap_process_group(&mut command);

            let mut child = command
                .spawn()
                .map_err(|err| CswapError::Exec(err.to_string()))?;
            let child_pid = child.id();
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| CswapError::Exec("failed to capture cswap stdout".to_string()))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| CswapError::Exec("failed to capture cswap stderr".to_string()))?;
            let stdout_task = tokio::spawn(read_child_pipe(stdout));
            let stderr_task = tokio::spawn(read_child_pipe(stderr));

            let status = tokio::select! {
                status = child.wait() => status.map_err(|err| CswapError::Exec(err.to_string()))?,
                _ = tokio::time::sleep(timeout) => {
                    kill_cswap_process_group(child_pid);
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    return Err(CswapError::Timeout {
                        operation,
                        timeout_secs: timeout.as_secs(),
                    });
                }
            };

            let stdout = stdout_task
                .await
                .map_err(|err| CswapError::Exec(err.to_string()))?
                .map_err(|err| CswapError::Exec(err.to_string()))?;
            let stderr = stderr_task
                .await
                .map_err(|err| CswapError::Exec(err.to_string()))?
                .map_err(|err| CswapError::Exec(err.to_string()))?;
            let stdout = String::from_utf8(stdout)
                .map_err(|err| CswapError::InvalidUtf8(err.to_string()))?;
            let stderr = String::from_utf8_lossy(&stderr).trim().to_string();

            if !status.success() {
                let error_message = if stderr.is_empty() {
                    stdout.trim().to_string()
                } else {
                    stderr
                };
                return Err(CswapError::CommandFailed {
                    status: status.to_string(),
                    stderr: error_message,
                    stdout,
                });
            }

            Ok(stdout)
        })
    }
}

pub struct CswapService {
    adapter: Arc<dyn CswapAdapter>,
    list_cache: RwLock<Option<CachedList>>,
    list_fetch_lock: Mutex<()>,
    last_list_fetch: RwLock<Option<CompletedListFetch>>,
    switch_lock: Mutex<()>,
}

impl CswapService {
    fn new(adapter: Arc<dyn CswapAdapter>) -> Self {
        Self {
            adapter,
            list_cache: RwLock::new(None),
            list_fetch_lock: Mutex::new(()),
            last_list_fetch: RwLock::new(None),
            switch_lock: Mutex::new(()),
        }
    }

    pub fn global() -> &'static CswapService {
        static SERVICE: OnceLock<CswapService> = OnceLock::new();
        SERVICE.get_or_init(|| CswapService::new(Arc::new(CswapCliAdapter)))
    }

    pub async fn list_accounts(
        &self,
        hostname: String,
        instance_id: Option<String>,
    ) -> Result<ClaudeAccountsResponse, CswapError> {
        let request_started_at = Instant::now();
        if let Some(cached) = self.fresh_cached_list().await {
            return Ok(cached);
        }

        let _fetch_guard = self.list_fetch_lock.lock().await;
        if let Some(cached) = self.fresh_cached_list().await {
            return Ok(cached);
        }
        if let Some(completed) = self.completed_list_fetch_since(request_started_at).await {
            return completed.map(|response| response_for_serve(&response, Utc::now()));
        }

        match self.fetch_list(hostname.clone(), instance_id.clone()).await {
            Ok(response) => {
                *self.list_cache.write().await = Some(CachedList {
                    response: response.clone(),
                    cached_at: Instant::now(),
                });
                *self.last_list_fetch.write().await = Some(CompletedListFetch {
                    completed_at: Instant::now(),
                    result: Ok(response.clone()),
                });
                Ok(response)
            }
            Err(error) => {
                let result = if let Some(stale) = self.stale_cached_list(error.to_string()).await {
                    Ok(stale)
                } else {
                    Err(error)
                };
                *self.last_list_fetch.write().await = Some(CompletedListFetch {
                    completed_at: Instant::now(),
                    result: result.clone(),
                });
                result
            }
        }
    }

    async fn fresh_cached_list(&self) -> Option<ClaudeAccountsResponse> {
        let cached = self.list_cache.read().await;
        let cached = cached.as_ref()?;
        if cached.cached_at.elapsed() <= LIST_CACHE_TTL {
            return Some(response_for_serve(&cached.response, Utc::now()));
        }
        None
    }

    async fn completed_list_fetch_since(
        &self,
        request_started_at: Instant,
    ) -> Option<Result<ClaudeAccountsResponse, CswapError>> {
        let completed = self.last_list_fetch.read().await;
        let completed = completed.as_ref()?;
        (completed.completed_at >= request_started_at).then(|| completed.result.clone())
    }

    async fn stale_cached_list(&self, stale_reason: String) -> Option<ClaudeAccountsResponse> {
        let cached = self.list_cache.read().await;
        let cached = cached.as_ref()?;
        let mut response = response_for_serve(&cached.response, Utc::now());
        response.status = ClaudeAccountsStatus::UsageDataStale;
        response.usage_data_stale = true;
        response.stale_reason = Some(stale_reason);
        Some(response)
    }

    async fn fetch_list(
        &self,
        hostname: String,
        instance_id: Option<String>,
    ) -> Result<ClaudeAccountsResponse, CswapError> {
        let stdout = self
            .adapter
            .run(
                vec!["--list".to_string(), "--json".to_string()],
                LIST_TIMEOUT,
                "list",
            )
            .await?;
        let payload = parse_list_json(&stdout)?;
        let now = Utc::now();
        let mut response = ClaudeAccountsResponse {
            schema_version: payload.schema_version.unwrap_or(1),
            status: ClaudeAccountsStatus::Ok,
            hostname,
            instance_id,
            fetched_at: now,
            served_at: now,
            cache_ttl_seconds: LIST_CACHE_TTL.as_secs(),
            usage_data_stale: false,
            stale_reason: None,
            active_account_number: payload.active_account_number,
            accounts: payload.accounts.unwrap_or_default(),
        };
        recompute_usage_age_seconds(&mut response, now);
        Ok(response)
    }

    pub async fn switch_account(&self, account: &str) -> Result<CswapSwitchResult, CswapError> {
        let account = account.trim();
        if account.is_empty() {
            return Err(CswapError::AccountRequired);
        }

        let _guard = tokio::time::timeout(SWITCH_LOCK_TIMEOUT, self.switch_lock.lock())
            .await
            .map_err(|_| CswapError::SwitchInProgress)?;

        let stdout = self
            .adapter
            .run(
                vec![
                    "--switch-to".to_string(),
                    account.to_string(),
                    "--json".to_string(),
                ],
                SWITCH_TIMEOUT,
                "switch",
            )
            .await?;
        let result = parse_switch_json(&stdout)?;
        self.invalidate_list_cache().await;
        Ok(result)
    }

    async fn invalidate_list_cache(&self) {
        *self.list_cache.write().await = None;
    }
}

async fn read_child_pipe<R>(mut pipe: R) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

fn configure_cswap_process_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn kill_cswap_process_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_cswap_process_group(_pid: Option<u32>) {}

fn response_for_serve(
    response: &ClaudeAccountsResponse,
    served_at: DateTime<Utc>,
) -> ClaudeAccountsResponse {
    let mut response = response.clone();
    response.served_at = served_at;
    recompute_usage_age_seconds(&mut response, served_at);
    response
}

fn recompute_usage_age_seconds(response: &mut ClaudeAccountsResponse, served_at: DateTime<Utc>) {
    let base_elapsed = served_at
        .signed_duration_since(response.fetched_at)
        .num_seconds()
        .max(0) as u64;
    for account in &mut response.accounts {
        if let Some(fetched_at) = account
            .usage_fetched_at
            .as_deref()
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        {
            account.usage_age_seconds = Some(
                served_at
                    .signed_duration_since(fetched_at.with_timezone(&Utc))
                    .num_seconds()
                    .max(0) as u64,
            );
        } else if let Some(age) = account.usage_age_seconds {
            account.usage_age_seconds = Some(age.saturating_add(base_elapsed));
        }
    }
}

fn parse_list_json(stdout: &str) -> Result<CswapListPayload, CswapError> {
    let payload: CswapListPayload =
        serde_json::from_str(stdout).map_err(|err| CswapError::Json(err.to_string()))?;
    if let Some(error) = payload.error.as_ref() {
        return Err(CswapError::Json(format!(
            "cswap returned error payload: {error}"
        )));
    }
    if payload.accounts.is_none() {
        return Err(CswapError::Json(
            "missing accounts field in cswap list payload".to_string(),
        ));
    }
    Ok(payload)
}

fn parse_switch_json(stdout: &str) -> Result<CswapSwitchResult, CswapError> {
    serde_json::from_str(stdout).map_err(|err| CswapError::Json(err.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;

    #[derive(Clone)]
    enum MockAction {
        Output(String),
        Error(CswapError),
        Sleep(Duration),
        NotifyAndWait {
            entered: Arc<Notify>,
            release: Arc<Notify>,
            output: String,
        },
    }

    #[derive(Clone)]
    struct MockCswapAdapter {
        action: MockAction,
        calls: Arc<AtomicUsize>,
    }

    impl MockCswapAdapter {
        fn new(action: MockAction) -> Self {
            Self {
                action,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl CswapAdapter for MockCswapAdapter {
        fn run<'a>(
            &'a self,
            _args: Vec<String>,
            timeout: Duration,
            operation: &'static str,
        ) -> CswapFuture<'a, Result<String, CswapError>> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let action = self.action.clone();
            Box::pin(async move {
                tokio::time::timeout(timeout, async move {
                    match action {
                        MockAction::Output(output) => Ok(output),
                        MockAction::Error(error) => Err(error),
                        MockAction::Sleep(duration) => {
                            tokio::time::sleep(duration).await;
                            Ok("{}".to_string())
                        }
                        MockAction::NotifyAndWait {
                            entered,
                            release,
                            output,
                        } => {
                            entered.notify_waiters();
                            release.notified().await;
                            Ok(output)
                        }
                    }
                })
                .await
                .map_err(|_| CswapError::Timeout {
                    operation,
                    timeout_secs: timeout.as_secs(),
                })?
            })
        }
    }

    fn list_fixture() -> &'static str {
        r#"{
          "schemaVersion": 1,
          "activeAccountNumber": 2,
          "accounts": [
            {
              "number": 2,
              "email": "you@example.com",
              "active": true,
              "usageStatus": "ok",
              "usageFetchedAt": "2026-06-22T20:29:59Z",
              "usageAgeSeconds": 12,
              "usage": {
                "fiveHour": { "pct": 25.0, "resetsAt": "2026-06-22T23:29:59Z" },
                "sevenDay": { "pct": 16.0, "resetsAt": "2026-06-26T17:59:59Z" }
              }
            }
          ]
        }"#
    }

    fn switch_fixture() -> &'static str {
        r#"{"switched":true,"from":"old@example.com","to":"new@example.com","reason":"manual"}"#
    }

    #[tokio::test]
    async fn cswap_list_parsing_success() {
        let service = CswapService::new(Arc::new(MockCswapAdapter::new(MockAction::Output(
            list_fixture().to_string(),
        ))));

        let response = service
            .list_accounts("mac-mini".to_string(), Some("node-1".to_string()))
            .await
            .expect("list parses");

        assert_eq!(response.schema_version, 1);
        assert_eq!(response.hostname, "mac-mini");
        assert_eq!(response.instance_id.as_deref(), Some("node-1"));
        assert_eq!(response.active_account_number, Some(2));
        assert_eq!(response.accounts.len(), 1);
        assert_eq!(
            response.accounts[0].email.as_deref(),
            Some("you@example.com")
        );
        assert!(response.accounts[0].active);
        assert_eq!(
            response.accounts[0]
                .usage
                .as_ref()
                .and_then(|usage| usage.five_hour.as_ref())
                .and_then(|window| window.pct),
            Some(25.0)
        );
    }

    #[tokio::test]
    async fn cswap_list_parsing_error_payload_is_error() {
        let service = CswapService::new(Arc::new(MockCswapAdapter::new(MockAction::Output(
            r#"{"schemaVersion":1,"error":{"code":"boom","message":"failed"}}"#.to_string(),
        ))));

        let error = service
            .list_accounts("mac-mini".to_string(), None)
            .await
            .expect_err("error payload should fail");

        assert_eq!(error.code(), "execution_failure");
        assert!(error.to_string().contains("error payload"));
    }

    #[tokio::test(start_paused = true)]
    async fn cswap_subprocess_timeout_is_reported() {
        let service = CswapService::new(Arc::new(MockCswapAdapter::new(MockAction::Sleep(
            Duration::from_secs(60),
        ))));

        let task =
            tokio::spawn(async move { service.list_accounts("mac-mini".to_string(), None).await });
        tokio::time::advance(LIST_TIMEOUT + Duration::from_secs(1)).await;
        let error = task
            .await
            .expect("task joins")
            .expect_err("slow subprocess should timeout");

        assert_eq!(
            error,
            CswapError::Timeout {
                operation: "list",
                timeout_secs: LIST_TIMEOUT.as_secs(),
            }
        );
    }

    #[tokio::test]
    async fn list_cache_miss_is_single_flight() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let adapter = MockCswapAdapter::new(MockAction::NotifyAndWait {
            entered: entered.clone(),
            release: release.clone(),
            output: list_fixture().to_string(),
        });
        let calls = adapter.calls.clone();
        let service = Arc::new(CswapService::new(Arc::new(adapter)));

        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.list_accounts("mac-mini".to_string(), None).await })
        };
        entered.notified().await;

        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.list_accounts("mac-mini".to_string(), None).await })
        };
        tokio::task::yield_now().await;
        release.notify_waiters();

        let first_response = first
            .await
            .expect("first joins")
            .expect("first list succeeds");
        let second_response = second
            .await
            .expect("second joins")
            .expect("second list succeeds");

        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(first_response.accounts.len(), 1);
        assert_eq!(second_response.accounts.len(), 1);
    }

    #[test]
    fn usage_age_seconds_recomputes_at_serve_time() {
        let fetched_at = DateTime::parse_from_rfc3339("2026-06-22T20:30:00Z")
            .expect("valid timestamp")
            .with_timezone(&Utc);
        let served_at = fetched_at + chrono::Duration::seconds(75);
        let response = ClaudeAccountsResponse {
            schema_version: 1,
            status: ClaudeAccountsStatus::Ok,
            hostname: "mac-mini".to_string(),
            instance_id: None,
            fetched_at,
            served_at: fetched_at,
            cache_ttl_seconds: LIST_CACHE_TTL.as_secs(),
            usage_data_stale: false,
            stale_reason: None,
            active_account_number: Some(2),
            accounts: vec![ClaudeAccount {
                number: Some(2),
                email: Some("you@example.com".to_string()),
                active: true,
                usage_status: Some("ok".to_string()),
                usage: None,
                usage_fetched_at: Some("2026-06-22T20:29:00Z".to_string()),
                usage_age_seconds: Some(12),
                extra: BTreeMap::new(),
            }],
        };

        let served = response_for_serve(&response, served_at);

        assert_eq!(served.served_at, served_at);
        assert_eq!(served.accounts[0].usage_age_seconds, Some(135));
    }

    #[tokio::test(start_paused = true)]
    async fn switch_single_flight_times_out_second_switch() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let adapter = MockCswapAdapter::new(MockAction::NotifyAndWait {
            entered: entered.clone(),
            release: release.clone(),
            output: switch_fixture().to_string(),
        });
        let service = Arc::new(CswapService::new(Arc::new(adapter)));

        let entered_wait = entered.notified();
        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.switch_account("old@example.com").await })
        };
        entered_wait.await;

        let second = {
            let service = service.clone();
            tokio::spawn(async move { service.switch_account("new@example.com").await })
        };
        tokio::time::advance(SWITCH_LOCK_TIMEOUT + Duration::from_secs(1)).await;

        let second_error = second
            .await
            .expect("second task joins")
            .expect_err("second switch should not enter while first holds lock");
        assert_eq!(second_error, CswapError::SwitchInProgress);

        release.notify_waiters();
        let first_result = first
            .await
            .expect("first task joins")
            .expect("first succeeds");
        assert!(first_result.switched);
    }

    #[tokio::test]
    async fn cswap_command_error_is_forwarded() {
        let service = CswapService::new(Arc::new(MockCswapAdapter::new(MockAction::Error(
            CswapError::CommandFailed {
                status: "exit status: 2".to_string(),
                stderr: "bad account".to_string(),
                stdout: String::new(),
            },
        ))));

        let error = service
            .switch_account("missing@example.com")
            .await
            .expect_err("command failure should be returned");

        assert_eq!(error.code(), "execution_failure");
        assert!(error.to_string().contains("bad account"));
    }
}
