//! Launch-bound Claude context-window resolution for auto compaction.
//!
//! A Claude pane can outlive a live-config edit, so this module records the
//! effective gateway decision made at launch. Completion reads are synchronous:
//! they use only fresh entries from a bounded cache and, at most, start one
//! background refresh per gateway URL. The watcher path never waits for OCX I/O.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::services::claude_gateway_proxy::ClaudeGatewayProxyEnv;

pub(crate) const DEFAULT_CONTEXT_COMPACT_LOWER_BOUND_TOKENS: u64 = 300_000;
const COMPACT_SAFETY_RESERVE_TOKENS: u64 = 64_000;
const NATIVE_STANDARD_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
const ONE_MILLION_CONTEXT_WINDOW_TOKENS: u64 = 1_000_000;
const CLAUDE_AUTO_COMPACT_MIN_TOKENS: u64 = 100_000;
pub(crate) const CLAUDE_AUTO_COMPACT_MAX_TOKENS: u64 = 1_000_000;
const CATALOG_TTL: Duration = Duration::from_secs(5 * 60);
const LAUNCH_PROVENANCE_TTL: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_CATALOGS: usize = 32;
const MAX_LAUNCH_PROVENANCE: usize = 512;
const TMUX_LAUNCH_PROVENANCE_OPTION: &str = "@agentdesk_claude_compact_provenance";
pub(crate) const CLAUDE_AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClaudeLaunchProvenance {
    Inject { base_url: String },
    Scrub,
}

impl From<&ClaudeGatewayProxyEnv> for ClaudeLaunchProvenance {
    fn from(value: &ClaudeGatewayProxyEnv) -> Self {
        match value {
            ClaudeGatewayProxyEnv::Inject { base_url } => Self::Inject {
                base_url: normalize_proxy_url(base_url),
            },
            ClaudeGatewayProxyEnv::Scrub => Self::Scrub,
        }
    }
}

#[derive(Clone, Debug)]
struct LaunchProvenanceEntry {
    provenance: ClaudeLaunchProvenance,
    recorded_at: Instant,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedLaunchProvenance {
    mode: String,
    #[serde(default)]
    base_url: Option<String>,
}

#[derive(Clone, Debug)]
struct CatalogEntry {
    windows: HashMap<String, u64>,
    refreshed_at: Instant,
}

#[derive(Default)]
struct CatalogState {
    by_proxy_url: HashMap<String, CatalogEntry>,
    refreshing: HashSet<String>,
}

static LAUNCH_PROVENANCE: LazyLock<Mutex<HashMap<String, LaunchProvenanceEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CATALOG_STATE: LazyLock<Mutex<CatalogState>> =
    LazyLock::new(|| Mutex::new(CatalogState::default()));
static CONTEXT_WINDOW_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnWindowResolution {
    Proven(u64),
    UnprovenLaunchBound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompactThreshold {
    pub actual_window_tokens: u64,
    pub effective_tokens: u64,
    pub rearm_floor_tokens: u64,
}

/// Persist the effective launch environment before the pane receives input.
/// A same-name relaunch overwrites the old entry, rather than reading current
/// config later and accidentally attributing a warm pane to a new proxy.
pub(crate) fn register_launch_provenance(
    tmux_session_name: &str,
    gateway_proxy_env: &ClaudeGatewayProxyEnv,
) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return;
    }
    let mut entries = LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_launch_provenance(&mut entries);
    entries.insert(
        tmux_session_name.to_string(),
        LaunchProvenanceEntry {
            provenance: ClaudeLaunchProvenance::from(gateway_proxy_env),
            recorded_at: Instant::now(),
        },
    );
    trim_oldest_launch_provenance(&mut entries);
}

/// Persist the effective launch decision only after tmux has successfully
/// created the pane. A dcserver restart can then rehydrate warm panes without
/// consulting current live gateway settings.
pub(crate) fn persist_launch_provenance_to_tmux(
    tmux_session_name: &str,
    gateway_proxy_env: &ClaudeGatewayProxyEnv,
) {
    let provenance = ClaudeLaunchProvenance::from(gateway_proxy_env);
    let (mode, base_url) = match provenance {
        ClaudeLaunchProvenance::Inject { base_url } => ("inject", Some(base_url)),
        ClaudeLaunchProvenance::Scrub => ("scrub", None),
    };
    let payload = PersistedLaunchProvenance {
        mode: mode.to_string(),
        base_url,
    };
    let Ok(serialized) = serde_json::to_string(&payload) else {
        return;
    };
    crate::services::platform::tmux::set_option(
        tmux_session_name,
        TMUX_LAUNCH_PROVENANCE_OPTION,
        &serialized,
    );
}

pub(crate) fn clear_launch_provenance_for_tmux(tmux_session_name: &str) {
    LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(tmux_session_name.trim());
}

/// Resolve a live interactive TUI's context window from launch-bound evidence.
/// A fresh, unambiguous catalog hit proves the exact window. Otherwise, known
/// launch provenance plus a non-empty model permits only the conservative
/// maximum-window trigger bound. `None` remains reserved for panes without
/// managed launch provenance or usable model evidence.
pub(crate) fn context_window_for_turn(
    tmux_session_name: &str,
    current_model: Option<&str>,
) -> Option<TurnWindowResolution> {
    let launch = launch_provenance_for_tmux(tmux_session_name)?;
    let current_model = current_model.and_then(preserve_model_selector)?;
    match launch.provenance {
        ClaudeLaunchProvenance::Inject { base_url } => {
            match live_catalog_context_window_for_selector(&base_url, &current_model) {
                Some(window) => Some(TurnWindowResolution::Proven(window)),
                None => Some(TurnWindowResolution::UnprovenLaunchBound),
            }
        }
        ClaudeLaunchProvenance::Scrub => Some(TurnWindowResolution::UnprovenLaunchBound),
    }
}

/// Calculate AgentDesk's authoritative absolute trigger. The multiplication is
/// deliberately widened: a malformed large context window or percentage must
/// still clamp safely rather than overflowing before the safety ceiling applies.
pub(crate) fn compact_threshold(
    actual_window_tokens: u64,
    compact_percent: u64,
    lower_bound_tokens: u64,
) -> Option<CompactThreshold> {
    // Zero is the explicit per-provider disable setting. Check it before the
    // lower-bound max so a configured floor cannot accidentally re-enable
    // automatic compaction.
    if compact_percent == 0 {
        return None;
    }
    let ceiling = actual_window_tokens.saturating_sub(COMPACT_SAFETY_RESERVE_TOKENS);
    if ceiling == 0 {
        return None;
    }
    let ratio_tokens = ((u128::from(actual_window_tokens) * u128::from(compact_percent)) / 100)
        .min(u128::from(u64::MAX)) as u64;
    let effective_tokens = ratio_tokens.max(lower_bound_tokens).min(ceiling);
    if effective_tokens == 0 {
        return None;
    }
    let five_percent_tokens =
        ((u128::from(actual_window_tokens) * 5) / 100).min(u128::from(u64::MAX)) as u64;
    Some(CompactThreshold {
        actual_window_tokens,
        effective_tokens,
        rearm_floor_tokens: effective_tokens.saturating_sub(five_percent_tokens),
    })
}

/// Absolute Claude Code launch knob for immutable headless/process launches.
/// Unlike an interactive TUI completion, the launch argv is authoritative for
/// this process and may retain an explicit `[1m]` selector.
pub(crate) fn launch_auto_compact_window(
    launch_model: Option<&str>,
    compact_percent: u64,
    lower_bound_tokens: u64,
    gateway_proxy_env: &ClaudeGatewayProxyEnv,
) -> Option<u64> {
    if compact_percent == 0 {
        return None;
    }
    let launch_model = launch_model.and_then(preserve_model_selector)?;
    let window = immutable_launch_context_window(&launch_model, gateway_proxy_env)?;
    let threshold = compact_threshold(window, compact_percent, lower_bound_tokens)?;
    (CLAUDE_AUTO_COMPACT_MIN_TOKENS..=CLAUDE_AUTO_COMPACT_MAX_TOKENS)
        .contains(&threshold.effective_tokens)
        .then_some(threshold.effective_tokens)
}

/// Extract the effective Claude model selector from a launch argv.
pub(crate) fn claude_model_from_args(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find(|pair| pair[0] == "--model")
        .map(|pair| pair[1].as_str())
}

/// Register the launch-bound gateway decision before deriving Claude Code's
/// optional absolute auto-compact setting for this process launch.
pub(crate) fn launch_auto_compact_window_for_session(
    launch_key: &str,
    model: Option<&str>,
    compact_percent: Option<u64>,
    compact_lower_bound_tokens: u64,
    gateway_proxy_env: &ClaudeGatewayProxyEnv,
) -> Option<u64> {
    register_launch_provenance(launch_key, gateway_proxy_env);
    compact_percent.and_then(|percent| {
        launch_auto_compact_window(
            model,
            percent,
            compact_lower_bound_tokens,
            gateway_proxy_env,
        )
    })
}

/// Render an isolation fence for shell-based launches. An inherited absolute
/// Claude window is never valid unless this launch resolved a fresh value.
pub(crate) fn append_auto_compact_window_shell_env(output: &mut String, window: Option<u64>) {
    output.push_str("unset ");
    output.push_str(CLAUDE_AUTO_COMPACT_WINDOW_ENV);
    output.push('\n');
    if let Some(window) = window {
        output.push_str("export ");
        output.push_str(CLAUDE_AUTO_COMPACT_WINDOW_ENV);
        output.push('=');
        output.push_str(&window.to_string());
        output.push('\n');
    }
}

/// Apply the same isolation fence to direct process launches.
pub(crate) fn apply_auto_compact_window_to_command(command: &mut Command, window: Option<u64>) {
    command.env_remove(CLAUDE_AUTO_COMPACT_WINDOW_ENV);
    if let Some(window) = window {
        command.env(CLAUDE_AUTO_COMPACT_WINDOW_ENV, window.to_string());
    }
}

pub(crate) fn normalize_model_selector(model: &str) -> Option<String> {
    let model = model.trim();
    let model = model.strip_suffix("[1m]").unwrap_or(model).trim_end();
    (!model.is_empty()).then(|| model.to_string())
}

fn preserve_model_selector(model: &str) -> Option<String> {
    let model = model.trim();
    (!model.is_empty()).then(|| model.to_string())
}

fn is_one_m_model_selector(model: &str) -> bool {
    model
        .trim()
        .strip_suffix("[1m]")
        .is_some_and(|base| !base.trim().is_empty())
}

/// Resolve a launch-time model whose argv cannot change underneath this process.
/// An injected launch still belongs to a gateway route, so every selector needs
/// an exact fresh catalog entry. A scrubbed launch may use Claude's exact native
/// selector table, but unknown selectors remain ambiguous and therefore disable
/// the absolute launch knob.
fn immutable_launch_context_window(
    launch_model: &str,
    gateway_proxy_env: &ClaudeGatewayProxyEnv,
) -> Option<u64> {
    match ClaudeLaunchProvenance::from(gateway_proxy_env) {
        ClaudeLaunchProvenance::Scrub => native_context_window(Some(launch_model)),
        ClaudeLaunchProvenance::Inject { base_url } => {
            catalog_context_window_for_selector(&base_url, launch_model)
        }
    }
}

/// Read one exact selector from the launch gateway catalog. A populated catalog
/// with no matching selector is still insufficient evidence; choosing its
/// smallest window could compact a larger routed model early.
fn catalog_context_window_for_selector(base_url: &str, model: &str) -> Option<u64> {
    let selector = preserve_model_selector(model)?;
    cached_catalog_and_schedule_refresh(base_url)?
        .get(&selector)
        .copied()
        .filter(|window| *window > 0)
}

/// Resolve a selector reported by a mutable live TUI. Unlike immutable launch
/// argv, a completion's base selector is ambiguous when the same fresh catalog
/// also advertises an explicit `[1m]` sibling: the completion can have
/// canonicalized away the selected 1M suffix. Fail closed instead of arming a
/// smaller auto-compact threshold for that potentially 1M session.
fn live_catalog_context_window_for_selector(base_url: &str, model: &str) -> Option<u64> {
    let selector = preserve_model_selector(model)?;
    let Some(catalog) = cached_catalog_and_schedule_refresh(base_url) else {
        tracing::debug!(
            proxy_url = base_url,
            %selector,
            "Claude context-window catalog is cold or stale; using launch-bound fallback"
        );
        return None;
    };
    let Some(window) = catalog.get(&selector).copied().filter(|window| *window > 0) else {
        tracing::debug!(
            proxy_url = base_url,
            %selector,
            catalog_key_count = catalog.len(),
            "Claude context-window selector missed the live catalog; using launch-bound fallback"
        );
        return None;
    };
    if !is_one_m_model_selector(&selector)
        && catalog
            .get(&format!("{selector}[1m]"))
            .is_some_and(|window| *window > 0)
    {
        tracing::debug!(
            proxy_url = base_url,
            %selector,
            "Claude context-window selector has an ambiguous [1m] sibling; using launch-bound fallback"
        );
        return None;
    }
    Some(window)
}

/// Classify only exact native selectors known to Claude Code. This deliberately
/// has no prefix/family fallback: an unrecognized future id must take the
/// conservative unknown policy instead of inheriting a stale mapping.
fn native_model_family(model: &str) -> Option<&'static str> {
    let model = normalize_model_selector(model)?;
    match model.as_str() {
        "sonnet"
        | "claude-sonnet-5"
        | "claude-sonnet-4-6"
        | "claude-sonnet-4-5"
        | "claude-sonnet-4-5-20250929"
        | "claude-sonnet-4"
        | "claude-sonnet-4-20250514"
        | "claude-3-7-sonnet"
        | "claude-3-7-sonnet-20250219"
        | "claude-3-5-sonnet"
        | "claude-3-5-sonnet-20241022" => Some("sonnet"),
        "opus"
        | "claude-opus-4-8"
        | "claude-opus-4-7"
        | "claude-opus-4-6"
        | "claude-opus-4-5"
        | "claude-opus-4-5-20251101"
        | "claude-opus-4-1"
        | "claude-opus-4-1-20250805"
        | "claude-opus-4"
        | "claude-opus-4-20250514" => Some("opus"),
        "haiku"
        | "claude-haiku-4-5"
        | "claude-haiku-4-5-20251001"
        | "claude-3-5-haiku"
        | "claude-3-5-haiku-20241022" => Some("haiku"),
        // Opus Plan launches an Opus planning shell but its execution model is
        // Sonnet; classify it accordingly for transcript model reconciliation.
        "opusplan" => Some("sonnet"),
        _ => None,
    }
}

fn native_context_window(model: Option<&str>) -> Option<u64> {
    let model = model?.trim();
    // Validate the stripped base against the exact native table first. The
    // `[1m]` picker suffix changes a known family window; it cannot turn an
    // arbitrary future/typo selector into a supported native model.
    native_model_family(model)?;
    // This suffix is emitted by Claude Code's model picker and means the
    // selected model has explicitly opted into the 1M context beta. It must
    // be checked before `normalize_model_selector` erases the suffix.
    if is_one_m_model_selector(model) {
        return Some(ONE_MILLION_CONTEXT_WINDOW_TOKENS);
    }
    Some(NATIVE_STANDARD_CONTEXT_WINDOW_TOKENS)
}

fn launch_provenance_for_tmux(tmux_session_name: &str) -> Option<LaunchProvenanceEntry> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return None;
    }
    let cached = {
        let mut entries = LAUNCH_PROVENANCE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        purge_launch_provenance(&mut entries);
        entries.get(tmux_session_name).cloned()
    };
    cached.or_else(|| rehydrate_launch_provenance_from_tmux(tmux_session_name))
}

fn rehydrate_launch_provenance_from_tmux(tmux_session_name: &str) -> Option<LaunchProvenanceEntry> {
    let raw = crate::services::platform::tmux::get_option(
        tmux_session_name,
        TMUX_LAUNCH_PROVENANCE_OPTION,
    )?;
    let provenance = parse_persisted_launch_provenance(&raw)?;
    let entry = LaunchProvenanceEntry {
        provenance,
        recorded_at: Instant::now(),
    };
    let mut entries = LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_launch_provenance(&mut entries);
    entries.insert(tmux_session_name.to_string(), entry.clone());
    trim_oldest_launch_provenance(&mut entries);
    Some(entry)
}

fn parse_persisted_launch_provenance(raw: &str) -> Option<ClaudeLaunchProvenance> {
    let payload: PersistedLaunchProvenance = serde_json::from_str(raw).ok()?;
    let provenance = match payload.mode.as_str() {
        "scrub" => ClaudeLaunchProvenance::Scrub,
        "inject" => {
            let base_url = normalize_proxy_url(payload.base_url?.as_str());
            if base_url.is_empty() {
                return None;
            }
            ClaudeLaunchProvenance::Inject { base_url }
        }
        _ => return None,
    };
    Some(provenance)
}

fn normalize_proxy_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

fn catalog_endpoint(proxy_url: &str) -> Option<String> {
    let proxy_url = normalize_proxy_url(proxy_url);
    (!proxy_url.is_empty()).then(|| format!("{proxy_url}/api/claude-code"))
}

fn cached_catalog_and_schedule_refresh(proxy_url: &str) -> Option<HashMap<String, u64>> {
    let proxy_url = normalize_proxy_url(proxy_url);
    if proxy_url.is_empty() {
        return None;
    }
    let (cached, start_refresh) = {
        let mut state = CATALOG_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let cached = state.by_proxy_url.get(&proxy_url).cloned();
        let fresh = cached
            .as_ref()
            .filter(|entry| entry.refreshed_at.elapsed() < CATALOG_TTL);
        let stale = fresh.is_none();
        let start_refresh = stale && state.refreshing.insert(proxy_url.clone());
        (fresh.map(|entry| entry.windows.clone()), start_refresh)
    };
    if start_refresh {
        spawn_catalog_refresh(proxy_url);
    }
    cached
}

fn spawn_catalog_refresh(proxy_url: String) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        CATALOG_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .refreshing
            .remove(&proxy_url);
        return;
    };
    handle.spawn(async move {
        let result = fetch_catalog(&proxy_url).await;
        finish_catalog_refresh(&proxy_url, result);
    });
}

/// Commit a catalog refresh. Failed or empty refreshes deliberately retain a
/// stale map for diagnostics/retry, but callers can never consume that map:
/// [`cached_catalog_and_schedule_refresh`] returns only fresh entries.
fn finish_catalog_refresh(proxy_url: &str, result: Result<HashMap<String, u64>, String>) {
    let mut state = CATALOG_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state.refreshing.remove(proxy_url);
    match result {
        Ok(windows) if !windows.is_empty() => {
            state.by_proxy_url.insert(
                proxy_url.to_string(),
                CatalogEntry {
                    windows,
                    refreshed_at: Instant::now(),
                },
            );
            trim_oldest_catalogs(&mut state);
        }
        Ok(_) => tracing::warn!(proxy_url, "Claude context-window catalog was empty"),
        Err(error) => {
            tracing::debug!(proxy_url, %error, "Claude context-window catalog refresh failed; stale catalog remains unusable")
        }
    }
}

async fn fetch_catalog(proxy_url: &str) -> Result<HashMap<String, u64>, String> {
    let endpoint = catalog_endpoint(proxy_url)
        .ok_or_else(|| "Claude context-window proxy URL is empty".to_string())?;
    let client = CONTEXT_WINDOW_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build Claude context-window HTTP client")
    });
    let response = client
        .get(&endpoint)
        .send()
        .await
        .map_err(|error| format!("GET {endpoint}: {error}"))?
        .error_for_status()
        .map_err(|error| format!("GET {endpoint}: {error}"))?;
    let body = response
        .text()
        .await
        .map_err(|error| format!("read {endpoint}: {error}"))?;
    parse_context_window_catalog(&body).map_err(|error| format!("parse {endpoint}: {error}"))
}

/// Parses both OCX's `contextWindows` map and array/object compatibility
/// entries. Keyed numeric values are accepted only inside `contextWindows`;
/// compatibility entries must name a model and its context-window field so
/// root response metadata cannot poison the conservative unknown-model fallback.
pub(crate) fn parse_context_window_catalog(body: &str) -> Result<HashMap<String, u64>, String> {
    let value: Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let mut windows = HashMap::new();
    collect_catalog_windows(&value, &mut windows);
    Ok(windows)
}

fn collect_catalog_windows(value: &Value, windows: &mut HashMap<String, u64>) {
    match value {
        Value::Array(_) => collect_compatibility_entries(value, windows),
        Value::Object(object) => collect_compatibility_entry(object, windows),
        _ => {}
    }
}

/// OCX's canonical catalog shape: `contextWindows` is a model-keyed map. This
/// is the sole location where a key may stand in for a model selector.
fn collect_context_window_map(value: &Value, windows: &mut HashMap<String, u64>) {
    let Some(entries) = value.as_object() else {
        return;
    };
    for (model, value) in entries {
        if let Some(model) = preserve_model_selector(model)
            && let Some(window) = value_context_window(value)
        {
            windows.insert(model, window);
        }
    }
}

/// Compatibility responses may contain arrays or wrapper objects under these
/// explicit container keys. Their entries must carry their own model id/name;
/// arbitrary object keys (such as response metadata) are never treated as
/// model selectors.
fn collect_compatibility_entries(value: &Value, windows: &mut HashMap<String, u64>) {
    match value {
        Value::Array(entries) => {
            for entry in entries {
                collect_compatibility_entries(entry, windows);
            }
        }
        Value::Object(object) => collect_compatibility_entry(object, windows),
        _ => {}
    }
}

fn collect_compatibility_entry(
    object: &serde_json::Map<String, Value>,
    windows: &mut HashMap<String, u64>,
) {
    if let Some(model) = object
        .get("model")
        .or_else(|| object.get("id"))
        .or_else(|| object.get("name"))
        .and_then(Value::as_str)
        .and_then(preserve_model_selector)
        && let Some(window) = object_context_window(object)
    {
        windows.insert(model, window);
    }
    if let Some(context_windows) = object.get("contextWindows") {
        collect_context_window_map(context_windows, windows);
    }
    for key in ["models", "data", "items"] {
        if let Some(entries) = object.get(key) {
            collect_compatibility_entries(entries, windows);
        }
    }
}

fn object_context_window(object: &serde_json::Map<String, Value>) -> Option<u64> {
    ["contextWindow", "context_window", "contextTokens", "window"]
        .iter()
        .find_map(|key| object.get(*key).and_then(value_context_window))
}

fn value_context_window(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse::<u64>().ok()))
        .filter(|value| *value > 0)
        .or_else(|| value.as_object().and_then(object_context_window))
}

fn purge_launch_provenance(entries: &mut HashMap<String, LaunchProvenanceEntry>) {
    entries.retain(|_, entry| entry.recorded_at.elapsed() <= LAUNCH_PROVENANCE_TTL);
}

fn trim_oldest_launch_provenance(entries: &mut HashMap<String, LaunchProvenanceEntry>) {
    while entries.len() > MAX_LAUNCH_PROVENANCE {
        let Some(key) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.recorded_at)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        entries.remove(&key);
    }
}

fn trim_oldest_catalogs(state: &mut CatalogState) {
    while state.by_proxy_url.len() > MAX_CATALOGS {
        let Some(key) = state
            .by_proxy_url
            .iter()
            .min_by_key(|(_, entry)| entry.refreshed_at)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        state.by_proxy_url.remove(&key);
    }
}

#[cfg(test)]
fn reset_for_test() {
    LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    *CATALOG_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = CatalogState::default();
}

#[cfg(test)]
pub(crate) fn put_catalog_for_test(proxy_url: &str, windows: HashMap<String, u64>) {
    CATALOG_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .by_proxy_url
        .insert(
            normalize_proxy_url(proxy_url),
            CatalogEntry {
                windows,
                refreshed_at: Instant::now(),
            },
        );
}

/// Context provenance and catalog fixtures are process-global. Keep every test
/// that touches either map behind this single guard under normal parallel test
/// execution.
#[cfg(test)]
pub(crate) static STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn state_test_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    reset_for_test();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_table_preserves_safety_ceiling_and_token_hysteresis() {
        let cases = [
            (100_000, 60, 300_000, 36_000),
            (200_000, 60, 300_000, 136_000),
            (372_000, 50, 300_000, 300_000),
            (1_000_000, 50, 300_000, 500_000),
            (200_000, 200, 1, 136_000),
        ];
        for (window, percent, lower, expected) in cases {
            let threshold = compact_threshold(window, percent, lower).unwrap();
            assert_eq!(threshold.effective_tokens, expected);
            assert_eq!(
                threshold.rearm_floor_tokens,
                expected.saturating_sub(window * 5 / 100)
            );
        }
    }

    /// Mutation guard: replacing the unproven trigger bound with any sampled
    /// smaller window breaks at least one comparison below. The maximum-window
    /// threshold must never be earlier than the threshold for a supported window.
    #[test]
    fn compact_threshold_is_monotonic_through_the_maximum_supported_window() {
        for percent in [1, 5, 25, 50, 60, 80, 100, 200] {
            for lower in [1, 100_000, 300_000, 900_000, u64::MAX] {
                let max = compact_threshold(CLAUDE_AUTO_COMPACT_MAX_TOKENS, percent, lower)
                    .expect("maximum-window threshold");
                for window in [64_001, 100_000, 128_000, 200_000, 372_000, 500_000, 999_999] {
                    let current = compact_threshold(window, percent, lower)
                        .expect("sampled supported-window threshold");
                    assert!(
                        current.effective_tokens <= max.effective_tokens,
                        "window={window}, percent={percent}, lower={lower}"
                    );
                }
            }
        }
    }

    #[test]
    fn zero_compact_percent_disables_before_the_lower_bound_can_reenable_it() {
        assert_eq!(compact_threshold(1_000_000, 0, 300_000), None);
        assert_eq!(compact_threshold(100_000, 0, u64::MAX), None);
    }

    #[test]
    fn parser_accepts_catalog_maps_arrays_and_only_positive_windows() {
        let parsed = parse_context_window_catalog(
            r#"{"contextWindows":{"routed-sonnet":{"contextWindow":372000},"routed-sonnet[1m]":{"contextWindow":1000000},"bad":0},"models":[{"id":"claude-haiku-4-5","context_window":"200000"}]}"#,
        )
        .unwrap();
        assert_eq!(parsed.get("routed-sonnet"), Some(&372_000));
        assert_eq!(parsed.get("routed-sonnet[1m]"), Some(&1_000_000));
        assert_eq!(parsed.get("claude-haiku-4-5"), Some(&200_000));
        assert!(!parsed.contains_key("bad"));
    }

    #[test]
    fn parser_rejects_root_metadata_as_context_window() {
        let parsed = parse_context_window_catalog(
            r#"{"port":10100,"autoCompactWindow":350000,"contextWindows":{"gpt":372000}}"#,
        )
        .unwrap();
        assert_eq!(parsed, HashMap::from([("gpt".to_string(), 372_000)]));
    }

    #[test]
    fn catalog_endpoint_uses_the_ocx_claude_code_catalog_contract() {
        assert_eq!(
            catalog_endpoint(" http://proxy.test/ "),
            Some("http://proxy.test/api/claude-code".to_string())
        );
        assert_eq!(catalog_endpoint("   "), None);
    }

    #[test]
    fn persisted_tmux_launch_provenance_option_rehydrates_only_valid_launch_data() {
        assert_eq!(
            parse_persisted_launch_provenance(
                r#"{"mode":"inject","base_url":" http://proxy.test/ ","launch_model":"routed-sonnet[1m]"}"#,
            ),
            Some(ClaudeLaunchProvenance::Inject {
                base_url: "http://proxy.test".to_string(),
            })
        );
        assert_eq!(
            parse_persisted_launch_provenance(
                r#"{"mode":"scrub","launch_model":"claude-haiku-4-5"}"#
            ),
            Some(ClaudeLaunchProvenance::Scrub)
        );
        assert_eq!(
            parse_persisted_launch_provenance(r#"{"mode":"inject","base_url":"   "}"#),
            None
        );
        assert_eq!(
            parse_persisted_launch_provenance(r#"{"mode":"unknown"}"#),
            None
        );
    }

    #[test]
    fn live_injected_tui_fails_closed_for_a_base_selector_with_a_one_million_sibling() {
        let _guard = state_test_guard();
        let proxy = "http://proxy.test";
        put_catalog_for_test(
            proxy,
            HashMap::from([
                ("routed-sonnet".to_string(), 372_000),
                ("routed-sonnet[1m]".to_string(), 1_000_000),
                ("small-route".to_string(), 128_000),
            ]),
        );
        let gateway = ClaudeGatewayProxyEnv::Inject {
            base_url: proxy.to_string(),
        };
        register_launch_provenance("tmux-a", &gateway);
        assert_eq!(
            context_window_for_turn("tmux-a", Some("routed-sonnet")),
            Some(TurnWindowResolution::UnprovenLaunchBound),
            "an ambiguous mutable completion must use only the maximum-window trigger bound"
        );
        assert_eq!(
            context_window_for_turn("tmux-a", Some("routed-sonnet[1m]")),
            Some(TurnWindowResolution::Proven(1_000_000)),
            "an explicit [1m] selector must not fall through to its base route"
        );
        assert_eq!(
            context_window_for_turn("tmux-a", Some("claude-sonnet-5")),
            Some(TurnWindowResolution::UnprovenLaunchBound),
            "a canonical completion without an exact hit must use only the launch-bound fallback"
        );
        assert_eq!(
            context_window_for_turn("tmux-a", Some("unknown")),
            Some(TurnWindowResolution::UnprovenLaunchBound),
            "a different catalog entry must never become a falsely proven window"
        );
    }

    /// Mutation guard: mapping an exact miss back to `None` reproduces #4678 and
    /// fails this assertion for a proxy that advertises only suffixed route keys.
    #[test]
    fn suffixed_only_catalog_keeps_bare_completion_launch_bound() {
        let _guard = state_test_guard();
        let proxy = "http://proxy-suffixed-only.test";
        put_catalog_for_test(
            proxy,
            HashMap::from([
                ("claude-opus-4-8-hgq".to_string(), 1_000_000),
                ("claude-opus-4-8-j97".to_string(), 1_000_000),
            ]),
        );
        register_launch_provenance(
            "tmux-suffixed-only",
            &ClaudeGatewayProxyEnv::Inject {
                base_url: proxy.to_string(),
            },
        );

        assert_eq!(
            context_window_for_turn("tmux-suffixed-only", Some("claude-opus-4-8")),
            Some(TurnWindowResolution::UnprovenLaunchBound)
        );
    }

    #[test]
    fn injected_routed_alias_waits_for_a_cold_catalog_then_resolves_normally() {
        let _guard = state_test_guard();
        let proxy = "http://proxy-cold-catalog.test";
        let gateway = ClaudeGatewayProxyEnv::Inject {
            base_url: proxy.to_string(),
        };
        register_launch_provenance("tmux-cold-routed", &gateway);

        // A dcserver restart loses the in-memory catalog but the warm pane
        // keeps its launch provenance in tmux. Schedule a refresh and wait for
        // a real catalog rather than using the old 100K fallback.
        assert_eq!(
            context_window_for_turn("tmux-cold-routed", Some("routed-sonnet")),
            Some(TurnWindowResolution::UnprovenLaunchBound)
        );

        put_catalog_for_test(
            proxy,
            HashMap::from([("routed-sonnet".to_string(), 372_000)]),
        );
        assert_eq!(
            context_window_for_turn("tmux-cold-routed", Some("routed-sonnet")),
            Some(TurnWindowResolution::Proven(372_000))
        );
    }

    #[test]
    fn injected_launch_requires_a_fresh_exact_catalog_selector() {
        let _guard = state_test_guard();
        let proxy = "http://proxy-cold-native.test";
        let gateway = ClaudeGatewayProxyEnv::Inject {
            base_url: proxy.to_string(),
        };
        for selector in [
            "claude-sonnet-4-6",
            "sonnet",
            "opus",
            "haiku",
            "opusplan",
            "routed-sonnet",
            "sonnet[1m]",
            "claude-sonnet-4-6[1m]",
            "arbitrary-route[1m]",
        ] {
            assert_eq!(
                immutable_launch_context_window(selector, &gateway),
                None,
                "cold injected catalog must not bypass selector {selector}"
            );
        }

        put_catalog_for_test(
            proxy,
            HashMap::from([
                ("claude-sonnet-4-6".to_string(), 200_000),
                ("claude-sonnet-4-6[1m]".to_string(), 1_000_000),
            ]),
        );
        assert_eq!(
            immutable_launch_context_window("claude-sonnet-4-6", &gateway),
            Some(200_000)
        );
        assert_eq!(
            immutable_launch_context_window("claude-sonnet-4-6[1m]", &gateway),
            Some(1_000_000),
            "Inject keeps [1m] as a distinct exact catalog key"
        );
    }

    #[test]
    fn native_table_handles_exact_aliases_versions_and_one_million_suffixes() {
        for model in [
            "sonnet",
            "opus",
            "haiku",
            "opusplan",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-8",
            "claude-opus-4-5-20251101",
            "claude-haiku-4-5-20251001",
        ] {
            assert_eq!(
                native_context_window(Some(model)),
                Some(NATIVE_STANDARD_CONTEXT_WINDOW_TOKENS),
                "model {model}"
            );
        }
        for model in ["sonnet[1m]", "opus[1m]", "claude-sonnet-4-6[1m]"] {
            assert_eq!(
                native_context_window(Some(model)),
                Some(ONE_MILLION_CONTEXT_WINDOW_TOKENS),
                "model {model}"
            );
        }
        for model in ["future-model", "future-model[1m]", "claude-sonnet-typo[1m]"] {
            assert_eq!(
                native_context_window(Some(model)),
                None,
                "an unknown base must not gain native 1M support through its suffix"
            );
        }
    }

    #[test]
    fn live_scrub_tui_uses_launch_bound_fallback_only_with_a_model() {
        let _guard = state_test_guard();
        register_launch_provenance("tmux-native", &ClaudeGatewayProxyEnv::Scrub);
        assert_eq!(context_window_for_turn("tmux-native", None), None);
        assert_eq!(
            context_window_for_turn("tmux-native", Some("sonnet")),
            Some(TurnWindowResolution::UnprovenLaunchBound),
            "a canonicalized base selector must not be falsely proven as a 200K window"
        );
        assert_eq!(
            context_window_for_turn("tmux-native", Some("claude-sonnet-4-6")),
            Some(TurnWindowResolution::UnprovenLaunchBound),
            "scrub provenance plus a model permits only the maximum-window trigger bound"
        );
    }

    #[test]
    fn immutable_scrub_launch_uses_its_exact_selector_and_disables_unknown_native_models() {
        assert_eq!(
            immutable_launch_context_window("sonnet", &ClaudeGatewayProxyEnv::Scrub),
            Some(NATIVE_STANDARD_CONTEXT_WINDOW_TOKENS)
        );
        assert_eq!(
            immutable_launch_context_window("sonnet[1m]", &ClaudeGatewayProxyEnv::Scrub),
            Some(1_000_000),
            "the immutable argv preserves an explicit [1m] selector"
        );
        assert_eq!(
            immutable_launch_context_window("future-model", &ClaudeGatewayProxyEnv::Scrub),
            None,
            "an unknown native selector must not invent a conservative launch window"
        );
        assert_eq!(
            immutable_launch_context_window("future-model[1m]", &ClaudeGatewayProxyEnv::Scrub),
            None,
            "an unknown native selector must not gain a 1M launch window through its suffix"
        );
    }

    #[test]
    fn expired_catalog_remains_unusable_after_refresh_failure() {
        let _guard = state_test_guard();
        let proxy = "http://proxy-expired.test";
        put_catalog_for_test(
            proxy,
            HashMap::from([("routed-sonnet".to_string(), 372_000)]),
        );
        {
            let mut state = CATALOG_STATE
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let entry = state
                .by_proxy_url
                .get_mut(proxy)
                .expect("fresh catalog fixture");
            entry.refreshed_at = Instant::now() - CATALOG_TTL - Duration::from_secs(1);
            state.refreshing.insert(proxy.to_string());
        }
        finish_catalog_refresh(proxy, Err("network unavailable".to_string()));

        assert_eq!(
            catalog_context_window_for_selector(proxy, "routed-sonnet"),
            None,
            "a retained stale catalog must not become a fallback after refresh failure"
        );
        let state = CATALOG_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(
            state.by_proxy_url.contains_key(proxy),
            "failure may retain stale data for retry/diagnostics, but never for resolution"
        );
        assert!(
            !state.refreshing.contains(proxy),
            "the failed refresh releases its single-flight marker for a later retry"
        );
    }

    #[test]
    fn launch_zero_percent_disables_before_any_context_resolution() {
        assert_eq!(
            launch_auto_compact_window(
                Some("sonnet"),
                0,
                DEFAULT_CONTEXT_COMPACT_LOWER_BOUND_TOKENS,
                &ClaudeGatewayProxyEnv::Scrub,
            ),
            None
        );
    }

    #[test]
    fn auto_compact_environment_helpers_always_scrub_before_optionally_exporting() {
        use std::ffi::OsStr;

        let mut disabled_shell = String::new();
        append_auto_compact_window_shell_env(&mut disabled_shell, None);
        assert_eq!(disabled_shell, "unset CLAUDE_CODE_AUTO_COMPACT_WINDOW\n");

        let mut enabled_shell = String::new();
        append_auto_compact_window_shell_env(&mut enabled_shell, Some(700_000));
        assert_eq!(
            enabled_shell,
            "unset CLAUDE_CODE_AUTO_COMPACT_WINDOW\nexport CLAUDE_CODE_AUTO_COMPACT_WINDOW=700000\n"
        );

        let mut disabled_command = Command::new("claude");
        disabled_command.env(CLAUDE_AUTO_COMPACT_WINDOW_ENV, "stale");
        apply_auto_compact_window_to_command(&mut disabled_command, None);
        assert!(disabled_command.get_envs().any(|(key, value)| {
            key == OsStr::new(CLAUDE_AUTO_COMPACT_WINDOW_ENV) && value.is_none()
        }));

        let mut enabled_command = Command::new("claude");
        enabled_command.env(CLAUDE_AUTO_COMPACT_WINDOW_ENV, "stale");
        apply_auto_compact_window_to_command(&mut enabled_command, Some(700_000));
        assert!(enabled_command.get_envs().any(|(key, value)| {
            key == OsStr::new(CLAUDE_AUTO_COMPACT_WINDOW_ENV) && value == Some(OsStr::new("700000"))
        }));
    }
}
