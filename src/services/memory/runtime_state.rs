use std::sync::{LazyLock, RwLock};
use std::time::{Duration, SystemTime};

use crate::runtime_layout;
use crate::services::discord::settings::MemoryBackendKind;

const MEMENTO_HEALTH_PATH: &str = "/health";
const MEMENTO_MCP_PATH: &str = "/mcp";
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const FAILURE_THRESHOLD: u8 = 3;
const BACKOFF_DURATION: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExternalMemoryBackendState {
    pub configured: bool,
    pub active: bool,
    pub consecutive_failures: u8,
    pub backoff_until: Option<SystemTime>,
    pub last_checked_at: Option<SystemTime>,
    pub last_error: Option<String>,
}

impl ExternalMemoryBackendState {
    fn seeded_active() -> Self {
        Self {
            configured: true,
            active: true,
            consecutive_failures: 0,
            backoff_until: None,
            last_checked_at: None,
            last_error: None,
        }
    }

    fn unconfigured() -> Self {
        Self::default()
    }

    pub(crate) fn summary(&self, label: &str) -> String {
        if !self.configured {
            return format!("{label}=unconfigured");
        }
        if self.active {
            if self.consecutive_failures == 0 {
                return format!("{label}=active");
            }
            return format!("{label}=active(failures={})", self.consecutive_failures);
        }
        format!(
            "{label}=backoff(failures={}, until={})",
            self.consecutive_failures,
            self.backoff_until
                .map(|value| {
                    chrono::DateTime::<chrono::Utc>::from(value)
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                })
                .unwrap_or_else(|| "unknown".to_string())
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MemoryBackendRuntimeSnapshot {
    pub memento: ExternalMemoryBackendState,
}

#[derive(Clone, Debug, Default)]
struct MemoryBackendRuntimeState {
    memento: ExternalMemoryBackendState,
}

static MEMORY_BACKEND_STATE: LazyLock<RwLock<MemoryBackendRuntimeState>> =
    LazyLock::new(|| RwLock::new(MemoryBackendRuntimeState::default()));
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
static LAST_REFRESH_REASON: LazyLock<RwLock<Option<String>>> = LazyLock::new(|| RwLock::new(None));

#[derive(Clone, Debug)]
struct MementoRuntimeConfig {
    endpoint: String,
    access_key: String,
    workspace: Option<String>,
}

enum ProbeOutcome {
    Unconfigured,
    Success,
    Failure(String),
}

fn lock_write() -> std::sync::RwLockWriteGuard<'static, MemoryBackendRuntimeState> {
    MEMORY_BACKEND_STATE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// #2049 Finding 14: hot-path readers (every turn's recall) acquire the
/// read-side lock so they don't serialize behind sync/refresh writers. The
/// `configured` mismatch correction is handled lazily — if the cached state
/// disagrees with the latest config we upgrade to the write side in `snapshot`.
fn lock_read() -> std::sync::RwLockReadGuard<'static, MemoryBackendRuntimeState> {
    MEMORY_BACKEND_STATE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn runtime_memory_backend_config() -> Option<runtime_layout::MemoryBackendConfig> {
    crate::config::runtime_root().map(|root| runtime_layout::load_memory_backend(&root))
}

fn env_var_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn memento_runtime_config() -> Option<MementoRuntimeConfig> {
    let config = runtime_memory_backend_config()?;
    let endpoint = normalize_memento_endpoint(&config.mcp.endpoint);
    let access_key_env = config.mcp.access_key_env.trim().to_string();
    if endpoint.is_empty() || access_key_env.is_empty() {
        return None;
    }
    let access_key = env_var_value(&access_key_env)?;
    Some(MementoRuntimeConfig {
        endpoint: endpoint.trim_end_matches('/').to_string(),
        access_key,
        workspace: env_var_value("MEMENTO_WORKSPACE"),
    })
}

fn normalize_memento_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    trimmed
        .strip_suffix(MEMENTO_MCP_PATH)
        .unwrap_or(trimmed)
        .to_string()
}

fn sync_configured_backends(state: &mut MemoryBackendRuntimeState) {
    sync_backend_slot(&mut state.memento, memento_runtime_config().is_some());
}

fn sync_backend_slot(slot: &mut ExternalMemoryBackendState, configured: bool) {
    if configured {
        if !slot.configured {
            *slot = ExternalMemoryBackendState::seeded_active();
        } else {
            slot.configured = true;
        }
    } else {
        *slot = ExternalMemoryBackendState::unconfigured();
    }
}

fn apply_probe_outcome(
    slot: &mut ExternalMemoryBackendState,
    outcome: ProbeOutcome,
    now: SystemTime,
) {
    match outcome {
        ProbeOutcome::Unconfigured => {
            *slot = ExternalMemoryBackendState {
                last_checked_at: Some(now),
                ..ExternalMemoryBackendState::unconfigured()
            };
        }
        ProbeOutcome::Success => {
            *slot = ExternalMemoryBackendState {
                configured: true,
                active: true,
                consecutive_failures: 0,
                backoff_until: None,
                last_checked_at: Some(now),
                last_error: None,
            };
        }
        ProbeOutcome::Failure(error) => {
            let failures = slot.consecutive_failures.saturating_add(1);
            let in_backoff = failures >= FAILURE_THRESHOLD;
            slot.configured = true;
            slot.active = !in_backoff;
            slot.consecutive_failures = failures;
            slot.backoff_until = if in_backoff {
                now.checked_add(BACKOFF_DURATION)
            } else {
                None
            };
            slot.last_checked_at = Some(now);
            slot.last_error = Some(error);
        }
    }
}

async fn probe_memento() -> ProbeOutcome {
    let Some(config) = memento_runtime_config() else {
        return ProbeOutcome::Unconfigured;
    };

    let client = match reqwest::Client::builder()
        .timeout(HEALTH_PROBE_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return ProbeOutcome::Failure(format!("memento health client build failed: {err}"));
        }
    };

    let request = client
        .get(format!("{}{}", config.endpoint, MEMENTO_HEALTH_PATH))
        .header("Authorization", format!("Bearer {}", config.access_key))
        .header("Accept", "application/json")
        .build();

    let response = match request {
        Ok(request) => {
            let mut request = reqwest::Request::try_from(request).expect("request conversion");
            if let Some(workspace) = config.workspace.filter(|value| !value.is_empty()) {
                request.headers_mut().insert(
                    "X-Memento-Workspace",
                    reqwest::header::HeaderValue::from_str(&workspace)
                        .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("default")),
                );
            }
            client.execute(request).await
        }
        Err(err) => Err(err),
    };

    match response {
        Ok(response) if response.status().is_success() => ProbeOutcome::Success,
        Ok(response) => ProbeOutcome::Failure(format!(
            "memento health probe failed with {}",
            response.status()
        )),
        Err(err) => ProbeOutcome::Failure(format!("memento health probe request failed: {err}")),
    }
}

pub(crate) fn snapshot() -> MemoryBackendRuntimeSnapshot {
    // #2049 Finding 14: try the read-side first so concurrent hot-path callers
    // don't serialize on a write lock. Only upgrade to write when the cached
    // `configured` slot disagrees with the latest runtime config.
    let configured_now = memento_runtime_config().is_some();
    {
        let state = lock_read();
        if state.memento.configured == configured_now {
            return MemoryBackendRuntimeSnapshot {
                memento: state.memento.clone(),
            };
        }
    }
    let mut state = lock_write();
    sync_configured_backends(&mut state);
    MemoryBackendRuntimeSnapshot {
        memento: state.memento.clone(),
    }
}

pub(crate) fn backend_state(kind: MemoryBackendKind) -> Option<ExternalMemoryBackendState> {
    match kind {
        MemoryBackendKind::File => None,
        MemoryBackendKind::Memento => Some(snapshot().memento),
    }
}

pub(crate) fn backend_is_active(kind: MemoryBackendKind) -> bool {
    match kind {
        MemoryBackendKind::File => true,
        MemoryBackendKind::Memento => snapshot().memento.active,
    }
}

pub(crate) async fn refresh_backend_health(reason: &str) -> MemoryBackendRuntimeSnapshot {
    // #2049 Finding 14: capture `configured` at the start of the refresh so
    // we can detect a config change that landed between the probe and the
    // apply, and avoid resurrecting a dropped backend with a stale probe.
    let configured_at_start = memento_runtime_config().is_some();
    {
        let mut state = lock_write();
        sync_configured_backends(&mut state);
    }

    let memento_outcome = probe_memento().await;
    let now = SystemTime::now();

    let snapshot = {
        let configured_now = memento_runtime_config().is_some();
        let mut state = lock_write();
        if configured_now != configured_at_start {
            // Config flipped under us during the probe. Discard the stale
            // outcome and re-sync from the latest config.
            let _ = memento_outcome;
            sync_configured_backends(&mut state);
            tracing::info!(
                "[memory] {} discarded stale memento probe (configured flipped {} -> {})",
                reason,
                configured_at_start,
                configured_now,
            );
        } else {
            apply_probe_outcome(&mut state.memento, memento_outcome, now);
        }
        MemoryBackendRuntimeSnapshot {
            memento: state.memento.clone(),
        }
    };

    tracing::info!(
        "[memory] {} health refresh: {}",
        reason,
        snapshot.memento.summary("memento")
    );
    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        *LAST_REFRESH_REASON
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason.to_string());
    }

    snapshot
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn reset_for_tests() {
    let mut state = lock_write();
    *state = MemoryBackendRuntimeState::default();
    *LAST_REFRESH_REASON
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn last_refresh_reason_for_tests() -> Option<String> {
    LAST_REFRESH_REASON
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::path::Path;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_memory_backend_config(root: &Path, value: serde_json::Value) {
        let config_dir = root.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("memory-backend.json"),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();
    }

    fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    async fn spawn_fixed_response_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (format!("http://{}", addr), handle)
    }

    #[tokio::test]
    async fn memento_health_backoff_deactivates_after_three_failures_and_recovers() {
        let _lock = env_lock();
        reset_for_tests();

        let temp = tempfile::tempdir().unwrap();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_key = std::env::var_os("MEMENTO_TEST_KEY");
        let previous_workspace = std::env::var_os("MEMENTO_WORKSPACE");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        write_memory_backend_config(
            temp.path(),
            serde_json::json!({
                "version": 2,
                "backend": "auto",
                "mcp": {
                    "endpoint": "http://127.0.0.1:8765",
                    "access_key_env": "MEMENTO_TEST_KEY"
                }
            }),
        );
        unsafe {
            std::env::set_var("MEMENTO_TEST_KEY", "memento-key");
            std::env::set_var("MEMENTO_WORKSPACE", "agentdesk-project-agentdesk");
        }

        for attempt in 1..=3 {
            let (base_url, handle) =
                spawn_fixed_response_server("500 Internal Server Error", "{\"error\":\"boom\"}")
                    .await;
            write_memory_backend_config(
                temp.path(),
                serde_json::json!({
                    "version": 2,
                    "backend": "auto",
                    "mcp": {
                        "endpoint": base_url,
                        "access_key_env": "MEMENTO_TEST_KEY"
                    }
                }),
            );

            let snapshot = refresh_backend_health("test-memento-failure").await;
            handle.abort();

            assert!(snapshot.memento.configured);
            assert_eq!(snapshot.memento.consecutive_failures, attempt);
            if attempt < 3 {
                assert!(snapshot.memento.active);
                assert!(snapshot.memento.backoff_until.is_none());
            } else {
                assert!(!snapshot.memento.active);
                assert!(snapshot.memento.backoff_until.is_some());
            }
        }

        let (base_url, handle) = spawn_fixed_response_server("200 OK", "{\"ok\":true}").await;
        write_memory_backend_config(
            temp.path(),
            serde_json::json!({
                "version": 2,
                "backend": "auto",
                "mcp": {
                    "endpoint": base_url,
                    "access_key_env": "MEMENTO_TEST_KEY"
                }
            }),
        );
        let snapshot = refresh_backend_health("test-memento-recovery").await;
        handle.abort();

        assert!(snapshot.memento.active);
        assert_eq!(snapshot.memento.consecutive_failures, 0);
        assert!(snapshot.memento.backoff_until.is_none());

        restore_env("AGENTDESK_ROOT_DIR", previous_root);
        restore_env("MEMENTO_TEST_KEY", previous_key);
        restore_env("MEMENTO_WORKSPACE", previous_workspace);
        reset_for_tests();
    }
}
