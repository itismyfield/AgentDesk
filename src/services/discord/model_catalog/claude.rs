use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::{ModelCatalogEntry, is_safe_model_selector};
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

const GATEWAY_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const REFRESH_STANDBY_INTERVAL: Duration = Duration::from_secs(1);
const CLOCK_SKEW_ALLOWANCE: Duration = Duration::from_secs(5 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CACHE_BODY_BYTES: usize = 256 * 1024;
const MAX_API_PAGES: usize = 4;
const MAX_MODELS_PER_SOURCE: usize = 100;
const MAX_CATALOG_TEXT_CHARS: usize = 100;
const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const API_CACHE_VERSION: u8 = 1;

static REFRESH_TASK_RUNNING: AtomicBool = AtomicBool::new(false);
static CATALOG_CACHE: OnceLock<RwLock<CatalogCache>> = OnceLock::new();

#[derive(Clone, Debug, Default)]
struct CatalogCache {
    gateway_entries: Vec<ModelCatalogEntry>,
    api_entries: Vec<ModelCatalogEntry>,
    entries: Vec<ModelCatalogEntry>,
}

#[derive(Clone, Debug, Default)]
struct DiskCatalogCandidate {
    gateway_entries: Vec<ModelCatalogEntry>,
    api_entries: Vec<ModelCatalogEntry>,
    api_cache_fresh: bool,
}

#[derive(Clone, Copy)]
struct CuratedAlias {
    selector: &'static str,
    label: &'static str,
    metadata_fallback_id: &'static str,
}

const CURATED_ALIASES: &[CuratedAlias] = &[
    CuratedAlias {
        selector: "fable",
        label: "Fable 5",
        metadata_fallback_id: "claude-fable-5",
    },
    CuratedAlias {
        selector: "opus",
        label: "Opus 5",
        metadata_fallback_id: "claude-opus-5",
    },
    CuratedAlias {
        selector: "sonnet",
        label: "Sonnet 5",
        metadata_fallback_id: "claude-sonnet-5",
    },
    CuratedAlias {
        selector: "haiku",
        label: "Haiku 4.5",
        metadata_fallback_id: "claude-haiku-4-5-20251001",
    },
    CuratedAlias {
        selector: "fable[1m]",
        label: "Fable 1M",
        metadata_fallback_id: "claude-fable-5",
    },
    CuratedAlias {
        selector: "sonnet[1m]",
        label: "Sonnet 1M",
        metadata_fallback_id: "claude-sonnet-5",
    },
    CuratedAlias {
        selector: "opus[1m]",
        label: "Opus 1M",
        metadata_fallback_id: "claude-opus-5",
    },
    CuratedAlias {
        selector: "opusplan",
        label: "Opus Plan",
        metadata_fallback_id: "claude-sonnet-5",
    },
];

const STATIC_FALLBACK_IDS: &[&str] = &[
    "claude-fable-5",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-haiku-4-5-20251001",
];

#[derive(Debug, Deserialize)]
struct GatewayCache {
    #[serde(rename = "fetchedAt")]
    fetched_at_ms: u64,
    #[serde(default)]
    models: Vec<GatewayModel>,
}

#[derive(Debug, Deserialize)]
struct GatewayModel {
    #[serde(alias = "value")]
    id: String,
    #[serde(default, alias = "displayName")]
    display_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApiCache {
    version: u8,
    fetched_at_ms: u64,
    models: Vec<ApiModel>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApiModel {
    id: String,
    display_name: String,
}

#[derive(Debug, Deserialize)]
struct ApiPage {
    #[serde(default)]
    data: Vec<ApiModelWire>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiModelWire {
    id: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FetchFailure {
    Timeout,
    Transport,
    HttpStatus,
    BodyLimit,
    InvalidResponse,
    PaginationLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefreshOutcome {
    Fresh,
    MissingApiKey,
    Updated,
    Empty,
    FetchFailed(FetchFailure),
    CacheWriteFailed,
    CachePathUnavailable,
}

impl RefreshOutcome {
    fn category(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::MissingApiKey => "missing_api_key",
            Self::Updated => "updated",
            Self::Empty => "empty",
            Self::FetchFailed(FetchFailure::Timeout) => "timeout",
            Self::FetchFailed(FetchFailure::Transport) => "transport_failure",
            Self::FetchFailed(FetchFailure::HttpStatus) => "http_status_failure",
            Self::FetchFailed(FetchFailure::BodyLimit) => "body_limit",
            Self::FetchFailed(FetchFailure::InvalidResponse) => "invalid_response",
            Self::FetchFailed(FetchFailure::PaginationLimit) => "pagination_limit",
            Self::CacheWriteFailed => "cache_write_failure",
            Self::CachePathUnavailable => "cache_path_unavailable",
        }
    }
}

struct RefreshTaskGuard;

impl RefreshTaskGuard {
    fn claim() -> Option<Self> {
        REFRESH_TASK_RUNNING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(Self)
    }
}

impl Drop for RefreshTaskGuard {
    fn drop(&mut self) {
        REFRESH_TASK_RUNNING.store(false, Ordering::Release);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn is_fresh(fetched_at_ms: u64, current_ms: u64) -> bool {
    let ttl_ms: u64 = GATEWAY_CACHE_TTL.as_millis().try_into().unwrap_or(u64::MAX);
    let skew_ms: u64 = CLOCK_SKEW_ALLOWANCE
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    fetched_at_ms <= current_ms.saturating_add(skew_ms)
        && current_ms.saturating_sub(fetched_at_ms) <= ttl_ms
}

fn truncate_catalog_text(raw: &str, fallback: &str) -> String {
    let mut text = raw
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_CATALOG_TEXT_CHARS)
        .collect::<String>();
    if text.is_empty() {
        text = fallback
            .chars()
            .take(MAX_CATALOG_TEXT_CHARS)
            .collect::<String>();
    }
    text
}

fn read_cache_file(path: &Path) -> Option<Vec<u8>> {
    super::bounded_cache_file::read_regular_file_bounded(path, MAX_CACHE_BODY_BYTES)
}

fn gateway_cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join(".claude")
            .join("cache")
            .join("gateway-models.json")
    })
}

fn api_cache_path() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join("claude_model_catalog.json"))
}

fn model_entry(id: String, label: String, source: &'static str) -> ModelCatalogEntry {
    ModelCatalogEntry::owned(
        id,
        truncate_catalog_text(&label, "Claude model"),
        source.to_string(),
        "Local bounded cache".to_string(),
    )
}

fn curated_entries() -> Vec<ModelCatalogEntry> {
    CURATED_ALIASES
        .iter()
        .map(|alias| {
            ModelCatalogEntry::borrowed(
                alias.selector,
                alias.label,
                "Evergreen Claude Code selector",
                alias.metadata_fallback_id,
            )
        })
        .collect()
}

fn static_fallback_entries() -> Vec<ModelCatalogEntry> {
    STATIC_FALLBACK_IDS
        .iter()
        .map(|id| {
            ModelCatalogEntry::borrowed(
                id,
                id,
                "Static Claude metadata fallback",
                "AgentDesk fallback",
            )
        })
        .collect()
}

fn gateway_entries_from_raw(raw: &[u8], current_ms: u64) -> Vec<ModelCatalogEntry> {
    if raw.len() > MAX_CACHE_BODY_BYTES {
        return Vec::new();
    }
    let Ok(cache) = serde_json::from_slice::<GatewayCache>(raw) else {
        return Vec::new();
    };
    if !is_fresh(cache.fetched_at_ms, current_ms) {
        return Vec::new();
    }

    cache
        .models
        .into_iter()
        .take(MAX_MODELS_PER_SOURCE)
        .filter_map(|model| {
            let id = model.id.trim().to_string();
            is_safe_model_selector(&id).then(|| {
                let label = truncate_catalog_text(&model.display_name, &id);
                model_entry(id, label, "Claude gateway selector")
            })
        })
        .collect()
}

fn api_entries_from_raw(raw: &[u8], current_ms: u64) -> Vec<ModelCatalogEntry> {
    if raw.len() > MAX_CACHE_BODY_BYTES {
        return Vec::new();
    }
    let Ok(cache) = serde_json::from_slice::<ApiCache>(raw) else {
        return Vec::new();
    };
    if cache.version != API_CACHE_VERSION || !is_fresh(cache.fetched_at_ms, current_ms) {
        return Vec::new();
    }

    cache
        .models
        .into_iter()
        .take(MAX_MODELS_PER_SOURCE)
        .filter_map(|model| {
            let id = model.id.trim().to_string();
            is_safe_model_selector(&id).then(|| {
                let label = truncate_catalog_text(&model.display_name, &id);
                model_entry(id, label, "Anthropic Models API")
            })
        })
        .collect()
}

fn merge_entries(
    gateway_entries: Vec<ModelCatalogEntry>,
    api_entries: Vec<ModelCatalogEntry>,
) -> Vec<ModelCatalogEntry> {
    let mut seen = HashSet::new();
    curated_entries()
        .into_iter()
        .chain(gateway_entries)
        .chain(api_entries)
        .chain(static_fallback_entries())
        .filter(|entry| seen.insert(entry.value.to_ascii_lowercase()))
        .collect()
}

fn catalog_from_raw_sources(
    gateway_raw: Option<&[u8]>,
    api_raw: Option<&[u8]>,
    current_ms: u64,
) -> Vec<ModelCatalogEntry> {
    merge_entries(
        gateway_raw
            .map(|raw| gateway_entries_from_raw(raw, current_ms))
            .unwrap_or_default(),
        api_raw
            .map(|raw| api_entries_from_raw(raw, current_ms))
            .unwrap_or_default(),
    )
}

fn catalog_candidate_from_disk_paths(
    gateway_path: Option<&Path>,
    api_path: Option<&Path>,
    current_ms: u64,
) -> DiskCatalogCandidate {
    let gateway_raw = gateway_path.and_then(read_cache_file);
    let api_raw = api_path.and_then(read_cache_file);
    let api_entries = api_raw
        .as_deref()
        .map(|raw| api_entries_from_raw(raw, current_ms))
        .unwrap_or_default();
    DiskCatalogCandidate {
        gateway_entries: gateway_raw
            .as_deref()
            .map(|raw| gateway_entries_from_raw(raw, current_ms))
            .unwrap_or_default(),
        api_cache_fresh: !api_entries.is_empty(),
        api_entries,
    }
}

fn publish_fresh_catalog_candidate(cache: &RwLock<CatalogCache>, candidate: &DiskCatalogCandidate) {
    if let Ok(mut cached) = cache.write() {
        if !candidate.gateway_entries.is_empty() {
            cached.gateway_entries = candidate.gateway_entries.clone();
        }
        if candidate.api_cache_fresh {
            cached.api_entries = candidate.api_entries.clone();
        }
        cached.entries = merge_entries(cached.gateway_entries.clone(), cached.api_entries.clone());
    }
}

fn publish_api_models(cache: &RwLock<CatalogCache>, models: Vec<ApiModel>) {
    if let Ok(mut cached) = cache.write() {
        cached.api_entries = usable_api_models(models)
            .into_iter()
            .map(|model| model_entry(model.id, model.display_name, "Anthropic Models API"))
            .collect();
        cached.entries = merge_entries(cached.gateway_entries.clone(), cached.api_entries.clone());
    }
}

async fn load_catalog_candidate(
    gateway_path: Option<PathBuf>,
    api_path: Option<PathBuf>,
    current_ms: u64,
) -> DiskCatalogCandidate {
    tokio::task::spawn_blocking(move || {
        catalog_candidate_from_disk_paths(gateway_path.as_deref(), api_path.as_deref(), current_ms)
    })
    .await
    .unwrap_or_default()
}

pub(super) fn resolved_models() -> Vec<ModelCatalogEntry> {
    CATALOG_CACHE
        .get_or_init(|| RwLock::new(CatalogCache::default()))
        .read()
        .ok()
        .filter(|cached| !cached.entries.is_empty())
        .map(|cached| cached.entries.clone())
        .unwrap_or_else(|| merge_entries(Vec::new(), Vec::new()))
}

fn parse_api_page(raw: &[u8]) -> Result<ApiPage, FetchFailure> {
    if raw.len() > MAX_CACHE_BODY_BYTES {
        return Err(FetchFailure::BodyLimit);
    }
    serde_json::from_slice(raw).map_err(|_| FetchFailure::InvalidResponse)
}

async fn fetch_api_models_with<F, Fut>(mut fetch: F) -> Result<Vec<ApiModel>, FetchFailure>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, FetchFailure>>,
{
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    let mut after_id = None;

    for page_index in 0..MAX_API_PAGES {
        let page = parse_api_page(&fetch(after_id.clone()).await?)?;
        for model in page.data {
            let id = model.id.trim().to_string();
            if !is_safe_model_selector(&id) || !seen.insert(id.to_ascii_lowercase()) {
                continue;
            }
            models.push(ApiModel {
                display_name: truncate_catalog_text(&model.display_name, &id),
                id,
            });
            if models.len() == MAX_MODELS_PER_SOURCE {
                return Ok(models);
            }
        }

        if !page.has_more {
            return Ok(models);
        }
        if page_index + 1 == MAX_API_PAGES {
            return Err(FetchFailure::PaginationLimit);
        }
        let next = page
            .last_id
            .map(|value| value.trim().to_string())
            .filter(|value| is_safe_model_selector(value))
            .ok_or(FetchFailure::InvalidResponse)?;
        if after_id.as_ref() == Some(&next) {
            return Err(FetchFailure::InvalidResponse);
        }
        after_id = Some(next);
    }

    unreachable!("bounded page loop returns from every terminal page state")
}

async fn fetch_network_page(
    client: reqwest::Client,
    api_key: String,
    after_id: Option<String>,
) -> Result<Vec<u8>, FetchFailure> {
    let mut request = client
        .get(ANTHROPIC_MODELS_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .query(&[("limit", MAX_MODELS_PER_SOURCE.to_string())]);
    if let Some(after_id) = after_id {
        request = request.query(&[("after_id", after_id)]);
    }

    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            FetchFailure::Timeout
        } else {
            FetchFailure::Transport
        }
    })?;
    if !response.status().is_success() {
        return Err(FetchFailure::HttpStatus);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CACHE_BODY_BYTES as u64)
    {
        return Err(FetchFailure::BodyLimit);
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            if error.is_timeout() {
                FetchFailure::Timeout
            } else {
                FetchFailure::Transport
            }
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_CACHE_BODY_BYTES {
            return Err(FetchFailure::BodyLimit);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn build_anthropic_client() -> Result<reqwest::Client, FetchFailure> {
    reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| FetchFailure::Transport)
}

async fn fetch_api_models(api_key: &str) -> Result<Vec<ApiModel>, FetchFailure> {
    let client = build_anthropic_client()?;
    let api_key = api_key.to_string();
    fetch_api_models_with(move |after_id| {
        let client = client.clone();
        let api_key = api_key.clone();
        async move { fetch_network_page(client, api_key, after_id).await }
    })
    .await
}

fn usable_api_models(models: Vec<ApiModel>) -> Vec<ApiModel> {
    let mut seen = HashSet::new();
    models
        .into_iter()
        .take(MAX_MODELS_PER_SOURCE)
        .filter_map(|model| {
            let id = model.id.trim().to_string();
            if !is_safe_model_selector(&id) || !seen.insert(id.to_ascii_lowercase()) {
                return None;
            }
            Some(ApiModel {
                display_name: truncate_catalog_text(&model.display_name, &id),
                id,
            })
        })
        .collect()
}

fn cache_is_fresh(path: &Path, current_ms: u64) -> bool {
    read_cache_file(path)
        .and_then(|raw| serde_json::from_slice::<ApiCache>(&raw).ok())
        .is_some_and(|cache| {
            cache.version == API_CACHE_VERSION
                && is_fresh(cache.fetched_at_ms, current_ms)
                && !usable_api_models(cache.models).is_empty()
        })
}

async fn refresh_cache_with<F, Fut>(
    path: &Path,
    current_ms: u64,
    api_key: Option<&str>,
    fetch: F,
) -> (RefreshOutcome, Option<Vec<ApiModel>>)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<Vec<ApiModel>, FetchFailure>>,
{
    let freshness_path = path.to_path_buf();
    if tokio::task::spawn_blocking(move || cache_is_fresh(&freshness_path, current_ms))
        .await
        .unwrap_or(false)
    {
        return (RefreshOutcome::Fresh, None);
    }
    let Some(api_key) = api_key.map(str::trim).filter(|key| !key.is_empty()) else {
        return (RefreshOutcome::MissingApiKey, None);
    };
    let models = match fetch(api_key.to_string()).await {
        Ok(models) => usable_api_models(models),
        Err(error) => return (RefreshOutcome::FetchFailed(error), None),
    };
    if models.is_empty() {
        return (RefreshOutcome::Empty, None);
    }

    let cache = ApiCache {
        version: API_CACHE_VERSION,
        fetched_at_ms: current_ms,
        models: models.clone(),
    };
    let Ok(serialized) = serde_json::to_string(&cache) else {
        return (RefreshOutcome::CacheWriteFailed, None);
    };
    let write_path = path.to_path_buf();
    match tokio::task::spawn_blocking(move || runtime_store::atomic_write(&write_path, &serialized))
        .await
    {
        Ok(Ok(())) => (RefreshOutcome::Updated, Some(models)),
        Ok(Err(_)) | Err(_) => (RefreshOutcome::CacheWriteFailed, None),
    }
}

async fn refresh_and_log_with<F, Fut>(
    gateway_path: Option<PathBuf>,
    api_path: Option<PathBuf>,
    current_ms: u64,
    api_key: Option<&str>,
    fetch: F,
) -> RefreshOutcome
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<Vec<ApiModel>, FetchFailure>>,
{
    let candidate = load_catalog_candidate(gateway_path, api_path.clone(), current_ms).await;
    let cache = CATALOG_CACHE.get_or_init(|| RwLock::new(CatalogCache::default()));
    publish_fresh_catalog_candidate(cache, &candidate);

    let Some(path) = api_path else {
        return RefreshOutcome::CachePathUnavailable;
    };
    let (outcome, refreshed_api_models) =
        refresh_cache_with(&path, current_ms, api_key, fetch).await;
    if let Some(models) = refreshed_api_models {
        publish_api_models(cache, models);
    }
    outcome
}

async fn refresh_and_log() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let outcome = refresh_and_log_with(
        gateway_cache_path(),
        api_cache_path(),
        now_ms(),
        api_key.as_deref(),
        |api_key| async move { fetch_api_models(&api_key).await },
    )
    .await;
    tracing::info!(
        target: "agentdesk::claude_model_catalog",
        status = outcome.category(),
        "Claude model catalog refresh check"
    );
}

async fn run_refresh_owner<F, Fut>(refresh_interval: Duration, refresh: &mut F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    refresh().await;
    let start = tokio::time::Instant::now() + refresh_interval;
    let mut interval = tokio::time::interval_at(start, refresh_interval);
    loop {
        interval.tick().await;
        refresh().await;
    }
}

async fn supervise_background_refresh<F, Fut>(
    standby_interval: Duration,
    refresh_interval: Duration,
    mut refresh: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        if let Some(_guard) = RefreshTaskGuard::claim() {
            run_refresh_owner(refresh_interval, &mut refresh).await;
            unreachable!("refresh owner runs until its supervisor is cancelled")
        }
        tokio::time::sleep(standby_interval).await;
    }
}

pub(super) fn spawn_background_refresh(
    provider: &ProviderKind,
) -> Option<tokio::task::JoinHandle<()>> {
    matches!(provider, ProviderKind::Claude).then(|| {
        tokio::spawn(supervise_background_refresh(
            REFRESH_STANDBY_INTERVAL,
            REFRESH_CHECK_INTERVAL,
            refresh_and_log,
        ))
    })
}

#[cfg(test)]
pub(super) fn spawn_test_background_refresh(
    refreshes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    spawn_test_background_refresh_after_claim(refreshes, || {})
}

#[cfg(test)]
pub(super) fn spawn_test_background_refresh_after_claim(
    refreshes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    after_claim: impl Fn() + Send + Sync + 'static,
) -> tokio::task::JoinHandle<()> {
    let after_claim = std::sync::Arc::new(after_claim);
    tokio::spawn(async move {
        let mut attempts = tokio::time::interval(REFRESH_STANDBY_INTERVAL);
        loop {
            attempts.tick().await;
            if let Some(_guard) = RefreshTaskGuard::claim() {
                after_claim();
                refreshes.fetch_add(1, Ordering::AcqRel);
                std::future::pending::<()>().await;
            }
        }
    })
}

#[cfg(test)]
pub(super) fn with_test_refresh_task_state<T>(operation: impl FnOnce() -> T) -> T {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    REFRESH_TASK_RUNNING.store(false, Ordering::Release);
    operation()
}

#[cfg(test)]
pub(super) fn test_refresh_task_running() -> bool {
    REFRESH_TASK_RUNNING.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use tempfile::tempdir;

    use super::*;

    fn api_cache_json(fetched_at_ms: u64, models: serde_json::Value) -> Vec<u8> {
        serde_json::json!({
            "version": API_CACHE_VERSION,
            "fetched_at_ms": fetched_at_ms,
            "models": models,
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn invariant_claude_model_catalog_global_test_state_uses_canonical_lock_scope() {
        const REFRESH_STATE_STORE: &str = concat!("REFRESH_TASK_RUNNING", ".store");

        let source = include_str!("claude.rs");
        let scope = source
            .split("pub(super) fn with_test_refresh_task_state")
            .nth(1)
            .and_then(|suffix| suffix.split("\n}\n").next())
            .expect("test refresh state scope");
        let lock = scope
            .find("acquire_shared_test_env_lock")
            .expect("canonical shared test lock");
        let reset = scope
            .find(REFRESH_STATE_STORE)
            .expect("scoped refresh state reset");
        assert!(lock < reset, "the canonical lock must precede the reset");

        let test_module = source
            .split("#[cfg(test)]\nmod tests")
            .nth(1)
            .expect("Claude catalog test module");
        assert!(
            !test_module.contains(REFRESH_STATE_STORE),
            "tests must mutate refresh state only through the locked scope"
        );
    }

    #[test]
    fn invariant_claude_model_catalog_refresh_claim_is_singleflight() {
        with_test_refresh_task_state(|| {
            let guard = RefreshTaskGuard::claim().expect("first Claude task claim");
            assert!(RefreshTaskGuard::claim().is_none());
            drop(guard);
            let reclaimed = RefreshTaskGuard::claim().expect("claim reset after drop");
            drop(reclaimed);
            assert!(!REFRESH_TASK_RUNNING.load(Ordering::Acquire));
        });
    }

    #[test]
    fn invariant_claude_model_catalog_non_claude_provider_has_no_supervisor() {
        with_test_refresh_task_state(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                assert!(spawn_background_refresh(&ProviderKind::Codex).is_none());
                let handle = spawn_background_refresh(&ProviderKind::Claude)
                    .expect("Claude gateway must keep a refresh supervisor");
                handle.abort();
                let _ = handle.await;
            });
        });
    }

    #[test]
    fn invariant_claude_model_catalog_owner_exit_hands_off_to_live_supervisor() {
        with_test_refresh_task_state(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                tokio::time::pause();
                let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let spawn_supervisor = |refreshes: Arc<std::sync::atomic::AtomicUsize>| {
                    tokio::spawn(supervise_background_refresh(
                        Duration::from_secs(1),
                        Duration::from_secs(60),
                        move || {
                            let refreshes = Arc::clone(&refreshes);
                            async move {
                                refreshes.fetch_add(1, Ordering::AcqRel);
                                std::future::pending::<()>().await;
                            }
                        },
                    ))
                };

                let first = spawn_supervisor(Arc::clone(&refreshes));
                let second = spawn_supervisor(Arc::clone(&refreshes));
                tokio::task::yield_now().await;
                assert_eq!(refreshes.load(Ordering::Acquire), 1);
                tokio::time::advance(Duration::from_millis(500)).await;
                tokio::task::yield_now().await;

                first.abort();
                let _ = first.await;
                assert!(!REFRESH_TASK_RUNNING.load(Ordering::Acquire));
                tokio::time::advance(Duration::from_secs(1)).await;
                tokio::task::yield_now().await;
                assert_eq!(refreshes.load(Ordering::Acquire), 2);
                assert!(REFRESH_TASK_RUNNING.load(Ordering::Acquire));

                second.abort();
                let _ = second.await;
                assert!(!REFRESH_TASK_RUNNING.load(Ordering::Acquire));
            });
        });
    }

    #[test]
    fn invariant_claude_model_catalog_curated_aliases_remain_evergreen_runtime_selectors() {
        let entries = catalog_from_raw_sources(None, None, 1_000_000);
        let values = entries
            .iter()
            .map(|entry| entry.value.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(&values[..4], ["fable", "opus", "sonnet", "haiku"]);
        for selector in ["fable[1m]", "sonnet[1m]", "opus[1m]", "opusplan"] {
            assert!(
                values.contains(&selector),
                "runtime-only selector {selector} must survive without caches"
            );
        }
        for alias in CURATED_ALIASES {
            let entry = entries
                .iter()
                .find(|entry| entry.value == alias.selector)
                .expect("curated alias");
            assert_eq!(entry.secondary_summary, alias.metadata_fallback_id);
            assert_ne!(entry.value, alias.metadata_fallback_id);
        }
    }

    #[test]
    fn invariant_claude_model_catalog_invalid_and_stale_caches_fall_back_safely() {
        let stale = serde_json::json!({
            "fetchedAt": 1,
            "models": [{"id": "gateway-only", "display_name": "Gateway"}],
        })
        .to_string();
        let invalid_api = br#"{"version":1,"fetched_at_ms":"secret"}"#;
        let entries = catalog_from_raw_sources(
            Some(stale.as_bytes()),
            Some(invalid_api),
            GATEWAY_CACHE_TTL.as_millis() as u64 + 10_000,
        );

        assert!(!entries.iter().any(|entry| entry.value == "gateway-only"));
        assert!(entries.iter().any(|entry| entry.value == "fable"));
        assert!(entries.iter().any(|entry| entry.value == "claude-fable-5"));
    }

    #[test]
    fn invariant_claude_model_catalog_merge_priority_and_dedupe_are_stable() {
        let current_ms = 9_000_000;
        let gateway = serde_json::json!({
            "fetchedAt": current_ms,
            "models": [
                {"id": "claude-shared", "display_name": "Gateway winner"},
                {"id": "CLAUDE-SHARED", "display_name": "Duplicate"}
            ],
        })
        .to_string();
        let api = api_cache_json(
            current_ms,
            serde_json::json!([
                {"id": "claude-shared", "display_name": "API loser"},
                {"id": "claude-api-only", "display_name": "API only"},
                {"id": "claude-fable-5", "display_name": "API fallback winner"}
            ]),
        );
        let entries = catalog_from_raw_sources(Some(gateway.as_bytes()), Some(&api), current_ms);

        assert_eq!(entries[0].value, "fable");
        let shared = entries
            .iter()
            .find(|entry| entry.value.eq_ignore_ascii_case("claude-shared"))
            .expect("shared model");
        assert_eq!(shared.label, "Gateway winner");
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.value.eq_ignore_ascii_case("claude-shared"))
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.value == "claude-fable-5")
                .count(),
            1
        );
    }

    #[test]
    fn invariant_claude_model_catalog_gateway_value_selector_is_supported() {
        let raw = serde_json::json!({
            "fetchedAt": 10_000,
            "models": [{"value": "fable[1m]", "displayName": "Fable 1M"}],
        })
        .to_string();
        let entries = catalog_from_raw_sources(Some(raw.as_bytes()), None, 10_000);

        let discovered = entries
            .iter()
            .find(|entry| entry.value == "fable[1m]")
            .expect("gateway selector");
        assert_eq!(discovered.label, "Fable 1M");
    }

    #[test]
    fn invariant_claude_model_catalog_gateway_base_url_and_unknown_fields_never_surface() {
        let secret = "https://user:password@example.invalid/private?token=secret";
        let raw = serde_json::json!({
            "baseUrl": secret,
            "credentials": {"token": "credential-secret"},
            "fetchedAt": 10_000,
            "models": [{"id": "claude-safe", "display_name": "Safe label"}],
        })
        .to_string();
        let entries = catalog_from_raw_sources(Some(raw.as_bytes()), None, 10_000);
        let rendered = entries
            .iter()
            .map(|entry| {
                format!(
                    "{} {} {} {}",
                    entry.value, entry.label, entry.primary_summary, entry.secondary_summary
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("claude-safe"));
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("credential-secret"));
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_http_client_does_not_follow_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 1_024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/credential-leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let client = build_anthropic_client().unwrap();
        let response = client
            .get(format!("http://{address}/models"))
            .header("x-api-key", "test-only-secret")
            .send()
            .await
            .unwrap();

        server.await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_http_client_ignores_environment_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        struct EnvRestore {
            values: Vec<(&'static str, Option<std::ffi::OsString>)>,
        }

        impl Drop for EnvRestore {
            fn drop(&mut self) {
                for (name, value) in self.values.drain(..) {
                    match value {
                        Some(value) => unsafe { std::env::set_var(name, value) },
                        None => unsafe { std::env::remove_var(name) },
                    }
                }
            }
        }

        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let variables = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];
        let _restore = EnvRestore {
            values: variables
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        };
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            unsafe { std::env::set_var(name, format!("http://{proxy_address}")) };
        }
        unsafe { std::env::remove_var("NO_PROXY") };

        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_address = origin.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.unwrap();
            let mut request = vec![0_u8; 1_024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });

        let response = build_anthropic_client()
            .unwrap()
            .get(format!("http://{origin_address}/models"))
            .send()
            .await
            .unwrap();

        server.await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), proxy.accept())
                .await
                .is_err(),
            "environment proxy must not receive the request"
        );
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_api_pagination_uses_bounded_cursor_sequence() {
        let pages = Arc::new(Mutex::new(VecDeque::from([
            Ok(serde_json::json!({
                "data": [{"id": "claude-page-1", "display_name": "Page one"}],
                "has_more": true,
                "last_id": "claude-page-1"
            })
            .to_string()
            .into_bytes()),
            Ok(serde_json::json!({
                "data": [{"id": "claude-page-2", "display_name": "Page two"}],
                "has_more": false,
                "last_id": "claude-page-2"
            })
            .to_string()
            .into_bytes()),
        ])));
        let cursors = Arc::new(Mutex::new(Vec::new()));
        let models = fetch_api_models_with({
            let pages = Arc::clone(&pages);
            let cursors = Arc::clone(&cursors);
            move |cursor| {
                cursors.lock().unwrap().push(cursor);
                let response = pages.lock().unwrap().pop_front().unwrap();
                async move { response }
            }
        })
        .await
        .unwrap();

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["claude-page-1", "claude-page-2"]
        );
        assert_eq!(
            *cursors.lock().unwrap(),
            [None, Some("claude-page-1".to_string())]
        );
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_api_rejects_body_and_pagination_bounds() {
        let body = vec![b'x'; MAX_CACHE_BODY_BYTES + 1];
        let error = fetch_api_models_with(move |_| {
            let body = body.clone();
            async move { Ok(body) }
        })
        .await
        .unwrap_err();
        assert_eq!(error, FetchFailure::BodyLimit);

        let error = fetch_api_models_with(|_| async {
            Ok(serde_json::json!({
                "data": [],
                "has_more": true,
                "last_id": "same-cursor"
            })
            .to_string()
            .into_bytes())
        })
        .await
        .unwrap_err();
        assert_eq!(error, FetchFailure::InvalidResponse);

        let cursor_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let error = fetch_api_models_with({
            let cursor_count = Arc::clone(&cursor_count);
            move |_| {
                let page = cursor_count.fetch_add(1, Ordering::Relaxed);
                async move {
                    Ok(serde_json::json!({
                        "data": [],
                        "has_more": true,
                        "last_id": format!("cursor-{page}")
                    })
                    .to_string()
                    .into_bytes())
                }
            }
        })
        .await
        .unwrap_err();
        assert_eq!(error, FetchFailure::PaginationLimit);
        assert_eq!(cursor_count.load(Ordering::Relaxed), MAX_API_PAGES);
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_api_caps_count_and_sanitizes_text() {
        let models = (0..MAX_MODELS_PER_SOURCE + 10)
            .map(|index| {
                serde_json::json!({
                    "id": format!("claude-{index}"),
                    "display_name": format!("{}\nsecret", "가".repeat(120)),
                })
            })
            .collect::<Vec<_>>();
        let fetched = fetch_api_models_with(move |_| {
            let models = models.clone();
            async move {
                Ok(serde_json::json!({
                    "data": models,
                    "has_more": false,
                    "last_id": null
                })
                .to_string()
                .into_bytes())
            }
        })
        .await
        .unwrap();

        assert_eq!(fetched.len(), MAX_MODELS_PER_SOURCE);
        assert!(fetched.iter().all(|model| {
            model.display_name.chars().count() <= MAX_CATALOG_TEXT_CHARS
                && !model.display_name.chars().any(char::is_control)
        }));
    }

    #[test]
    fn invariant_claude_model_catalog_failed_and_empty_refreshes_preserve_memory_lkg() {
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory = tempdir().unwrap();
            let api_path = directory.path().join("cache.json");
            let fresh_ms = 55_000;
            let stale_ms = fresh_ms + GATEWAY_CACHE_TTL.as_millis() as u64 + 1;
            let original = api_cache_json(
                fresh_ms,
                serde_json::json!([{"id": "claude-memory-lkg", "display_name": "Memory LKG"}]),
            );
            std::fs::write(&api_path, &original).unwrap();
            let cache = CATALOG_CACHE.get_or_init(|| RwLock::new(CatalogCache::default()));
            if let Ok(mut cached) = cache.write() {
                *cached = CatalogCache::default();
            }

            let initial =
                refresh_and_log_with(None, Some(api_path.clone()), fresh_ms, None, |_| async {
                    unreachable!("fresh cache must not fetch")
                })
                .await;
            assert_eq!(initial, RefreshOutcome::Fresh);
            assert!(
                resolved_models()
                    .iter()
                    .any(|entry| entry.value == "claude-memory-lkg")
            );

            let missing_key =
                refresh_and_log_with(None, Some(api_path.clone()), stale_ms, None, |_| async {
                    unreachable!("missing key must not fetch")
                })
                .await;
            assert_eq!(missing_key, RefreshOutcome::MissingApiKey);
            assert!(
                resolved_models()
                    .iter()
                    .any(|entry| entry.value == "claude-memory-lkg")
            );
            assert_eq!(std::fs::read(&api_path).unwrap(), original);

            let failed = refresh_and_log_with(
                None,
                Some(api_path.clone()),
                stale_ms,
                Some("key"),
                |_| async { Err(FetchFailure::Transport) },
            )
            .await;
            assert_eq!(failed, RefreshOutcome::FetchFailed(FetchFailure::Transport));
            assert!(
                resolved_models()
                    .iter()
                    .any(|entry| entry.value == "claude-memory-lkg")
            );
            assert_eq!(std::fs::read(&api_path).unwrap(), original);

            let empty = refresh_and_log_with(
                None,
                Some(api_path.clone()),
                stale_ms,
                Some("key"),
                |_| async { Ok(Vec::new()) },
            )
            .await;
            assert_eq!(empty, RefreshOutcome::Empty);
            assert_eq!(std::fs::read(&api_path).unwrap(), original);
            assert!(
                resolved_models()
                    .iter()
                    .any(|entry| entry.value == "claude-memory-lkg")
            );

            let updated = refresh_and_log_with(
                None,
                Some(api_path.clone()),
                stale_ms,
                Some("key"),
                |_| async {
                    Ok(vec![ApiModel {
                        id: "claude-replacement".to_string(),
                        display_name: "Replacement".to_string(),
                    }])
                },
            )
            .await;
            assert_eq!(updated, RefreshOutcome::Updated);
            let resolved = resolved_models();
            assert!(
                resolved
                    .iter()
                    .any(|entry| entry.value == "claude-replacement")
            );
            assert!(
                !resolved
                    .iter()
                    .any(|entry| entry.value == "claude-memory-lkg")
            );
        });
    }

    #[test]
    fn invariant_claude_model_catalog_successful_api_refresh_preserves_gateway_memory_source() {
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory = tempdir().unwrap();
            let gateway_path = directory.path().join("gateway.json");
            let api_path = directory.path().join("api.json");
            let current_ms = 55_000;
            let stale_ms = current_ms + GATEWAY_CACHE_TTL.as_millis() as u64 + 1;
            std::fs::write(
                &gateway_path,
                serde_json::json!({
                    "fetchedAt": current_ms,
                    "models": [{"id": "gateway-memory", "display_name": "Memory gateway"}],
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                &api_path,
                api_cache_json(
                    current_ms,
                    serde_json::json!([{"id": "claude-old-api", "display_name": "Old API"}]),
                ),
            )
            .unwrap();
            let cache = CATALOG_CACHE.get_or_init(|| RwLock::new(CatalogCache::default()));
            if let Ok(mut cached) = cache.write() {
                *cached = CatalogCache::default();
            }
            let initial = refresh_and_log_with(
                Some(gateway_path.clone()),
                Some(api_path.clone()),
                current_ms,
                None,
                |_| async { unreachable!("fresh cache must not fetch") },
            )
            .await;
            assert_eq!(initial, RefreshOutcome::Fresh);

            let updated = refresh_and_log_with(
                Some(gateway_path.clone()),
                Some(api_path),
                stale_ms,
                Some("key"),
                move |_| async move {
                    std::fs::write(
                        &gateway_path,
                        serde_json::json!({
                            "fetchedAt": stale_ms,
                            "models": [{"id": "gateway-disk-race", "display_name": "Disk race"}],
                        })
                        .to_string(),
                    )
                    .unwrap();
                    Ok(vec![ApiModel {
                        id: "claude-new-api".to_string(),
                        display_name: "New API".to_string(),
                    }])
                },
            )
            .await;

            assert_eq!(updated, RefreshOutcome::Updated);
            let resolved = resolved_models();
            assert!(resolved.iter().any(|entry| entry.value == "gateway-memory"));
            assert!(
                !resolved
                    .iter()
                    .any(|entry| entry.value == "gateway-disk-race")
            );
            assert!(resolved.iter().any(|entry| entry.value == "claude-new-api"));
            assert!(!resolved.iter().any(|entry| entry.value == "claude-old-api"));
        });
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_fresh_cache_skips_refresh_fetch() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("cache.json");
        let current_ms = 55_000;
        std::fs::write(
            &path,
            api_cache_json(
                current_ms,
                serde_json::json!([{"id": "claude-lkg", "display_name": "LKG"}]),
            ),
        )
        .unwrap();
        let fetch_called = Arc::new(AtomicBool::new(false));

        let (outcome, published) = refresh_cache_with(&path, current_ms, Some("key"), {
            let fetch_called = Arc::clone(&fetch_called);
            move |_| {
                fetch_called.store(true, Ordering::Release);
                async { Ok(Vec::new()) }
            }
        })
        .await;

        assert_eq!(outcome, RefreshOutcome::Fresh);
        assert!(published.is_none());
        assert!(!fetch_called.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_invalid_only_fresh_cache_triggers_fetch() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("cache.json");
        let current_ms = 55_000;
        std::fs::write(
            &path,
            api_cache_json(
                current_ms,
                serde_json::json!([
                    {"id": "bad selector!", "display_name": "Invalid"},
                    {"id": "x".repeat(65), "display_name": "Oversize"}
                ]),
            ),
        )
        .unwrap();
        let fetch_called = Arc::new(AtomicBool::new(false));

        let (outcome, published) = refresh_cache_with(&path, current_ms, Some("key"), {
            let fetch_called = Arc::clone(&fetch_called);
            move |_| {
                fetch_called.store(true, Ordering::Release);
                async {
                    Ok(vec![ApiModel {
                        id: "claude-valid-refresh".to_string(),
                        display_name: "Valid refresh".to_string(),
                    }])
                }
            }
        })
        .await;

        assert_eq!(outcome, RefreshOutcome::Updated);
        assert!(fetch_called.load(Ordering::Acquire));
        assert_eq!(published.unwrap()[0].id, "claude-valid-refresh");
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_refresh_failure_preserves_last_known_good_cache() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("cache.json");
        let original = api_cache_json(
            1,
            serde_json::json!([{"id": "claude-lkg", "display_name": "LKG"}]),
        );
        std::fs::write(&path, &original).unwrap();

        let (outcome, published) = refresh_cache_with(
            &path,
            GATEWAY_CACHE_TTL.as_millis() as u64 + 2,
            Some("key"),
            |_| async { Err(FetchFailure::Transport) },
        )
        .await;

        assert_eq!(
            outcome,
            RefreshOutcome::FetchFailed(FetchFailure::Transport)
        );
        assert!(published.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_empty_refresh_does_not_replace_cache() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("cache.json");
        let original = api_cache_json(
            1,
            serde_json::json!([{"id": "claude-lkg", "display_name": "LKG"}]),
        );
        std::fs::write(&path, &original).unwrap();

        let (outcome, published) = refresh_cache_with(
            &path,
            GATEWAY_CACHE_TTL.as_millis() as u64 + 2,
            Some("key"),
            |_| async { Ok(Vec::new()) },
        )
        .await;

        assert_eq!(outcome, RefreshOutcome::Empty);
        assert!(published.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn invariant_claude_model_catalog_successful_refresh_atomically_updates_cache() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested").join("cache.json");
        let current_ms = 55_000;
        let (outcome, published) = refresh_cache_with(&path, current_ms, Some("key"), |_| async {
            Ok(vec![ApiModel {
                id: "claude-new".to_string(),
                display_name: "New model".to_string(),
            }])
        })
        .await;

        assert_eq!(outcome, RefreshOutcome::Updated);
        assert_eq!(published.unwrap()[0].id, "claude-new");
        let cache: ApiCache = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(cache.fetched_at_ms, current_ms);
        assert_eq!(cache.models[0].id, "claude-new");
    }
}
