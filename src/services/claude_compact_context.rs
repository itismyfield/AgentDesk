//! Launch-bound Claude context-window resolution for auto compaction.
//!
//! A Claude pane can outlive a live-config edit, so this module records the
//! effective gateway decision made at launch. Completion reads are synchronous:
//! they use a bounded stale cache and, at most, start one background refresh per
//! gateway URL. The watcher path never waits for OCX I/O.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::services::claude_gateway_proxy::ClaudeGatewayProxyEnv;

pub(crate) const DEFAULT_CONTEXT_COMPACT_LOWER_BOUND_TOKENS: u64 = 300_000;
const COMPACT_SAFETY_RESERVE_TOKENS: u64 = 64_000;
const CLAUDE_AUTO_COMPACT_MIN_TOKENS: u64 = 100_000;
const CLAUDE_AUTO_COMPACT_MAX_TOKENS: u64 = 1_000_000;
const CATALOG_TTL: Duration = Duration::from_secs(5 * 60);
const LAUNCH_PROVENANCE_TTL: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_CATALOGS: usize = 32;
const MAX_LAUNCH_PROVENANCE: usize = 512;

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
    launch_model: Option<String>,
    recorded_at: Instant,
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
    launch_model: Option<&str>,
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
            launch_model: launch_model.and_then(normalize_model_selector),
            recorded_at: Instant::now(),
        },
    );
    trim_oldest_launch_provenance(&mut entries);
}

pub(crate) fn clear_launch_provenance_for_tmux(tmux_session_name: &str) {
    LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(tmux_session_name.trim());
}

/// Resolve the model's actual context window using only the launch-time proxy
/// provenance. `None` means the pane was never registered, so the caller must
/// not invent a new provenance from current live config.
pub(crate) fn context_window_for_turn(
    tmux_session_name: &str,
    current_model: Option<&str>,
) -> Option<u64> {
    let launch = launch_provenance_for_tmux(tmux_session_name)?;
    let model = current_model
        .and_then(normalize_model_selector)
        .or(launch.launch_model);
    match launch.provenance {
        ClaudeLaunchProvenance::Scrub => native_context_window(model.as_deref()).or(Some(100_000)),
        ClaudeLaunchProvenance::Inject { base_url } => {
            let catalog = cached_catalog_and_schedule_refresh(&base_url);
            let selector = model.as_deref();
            if let Some(window) = selector.and_then(|model| {
                catalog
                    .as_ref()
                    .and_then(|windows| windows.get(model).copied())
                    .filter(|window| *window > 0)
            }) {
                return Some(window);
            }
            // Canonical native ids deliberately bypass a routed catalog when an
            // exact alias did not win. Do not family/prefix-match future models.
            native_context_window(selector).or_else(|| {
                catalog
                    .as_ref()
                    .and_then(minimum_positive_catalog_window)
                    .or(Some(100_000))
            })
        }
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

/// Absolute Claude Code launch knob. It is intentionally unavailable without a
/// concrete launch model, and accepts only Claude Code's documented range.
pub(crate) fn launch_auto_compact_window(
    tmux_session_name: &str,
    launch_model: Option<&str>,
    compact_percent: u64,
    lower_bound_tokens: u64,
) -> Option<u64> {
    let launch_model = launch_model.and_then(normalize_model_selector)?;
    let window = context_window_for_turn(tmux_session_name, Some(&launch_model))?;
    let threshold = compact_threshold(window, compact_percent, lower_bound_tokens)?;
    (CLAUDE_AUTO_COMPACT_MIN_TOKENS..=CLAUDE_AUTO_COMPACT_MAX_TOKENS)
        .contains(&threshold.effective_tokens)
        .then_some(threshold.effective_tokens)
}

pub(crate) fn normalize_model_selector(model: &str) -> Option<String> {
    let model = model.trim();
    let model = model.strip_suffix("[1m]").unwrap_or(model).trim_end();
    (!model.is_empty()).then(|| model.to_string())
}

fn native_context_window(model: Option<&str>) -> Option<u64> {
    match model? {
        "claude-opus-4-8" | "claude-sonnet-5" | "claude-fable-5" => Some(1_000_000),
        "claude-haiku-4-5" => Some(200_000),
        _ => None,
    }
}

fn launch_provenance_for_tmux(tmux_session_name: &str) -> Option<LaunchProvenanceEntry> {
    let mut entries = LAUNCH_PROVENANCE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    purge_launch_provenance(&mut entries);
    entries.get(tmux_session_name.trim()).cloned()
}

fn normalize_proxy_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
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
        let stale = cached
            .as_ref()
            .is_none_or(|entry| entry.refreshed_at.elapsed() >= CATALOG_TTL);
        let start_refresh = stale && state.refreshing.insert(proxy_url.clone());
        (cached.map(|entry| entry.windows), start_refresh)
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
        let mut state = CATALOG_STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.refreshing.remove(&proxy_url);
        match result {
            Ok(windows) if !windows.is_empty() => {
                state.by_proxy_url.insert(
                    proxy_url.clone(),
                    CatalogEntry {
                        windows,
                        refreshed_at: Instant::now(),
                    },
                );
                trim_oldest_catalogs(&mut state);
            }
            Ok(_) => tracing::warn!(proxy_url, "Claude context-window catalog was empty"),
            Err(error) => tracing::debug!(proxy_url, %error, "Claude context-window catalog refresh failed; retaining stale cache"),
        }
    });
}

async fn fetch_catalog(proxy_url: &str) -> Result<HashMap<String, u64>, String> {
    let endpoint = format!("{proxy_url}/api/claude-code.contextWindows");
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

/// Parses both OCX's map-shaped catalog and its array-shaped compatibility
/// response. Only positive numeric values are admitted; one malformed model
/// must not poison the conservative unknown-model fallback.
pub(crate) fn parse_context_window_catalog(body: &str) -> Result<HashMap<String, u64>, String> {
    let value: Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let mut windows = HashMap::new();
    collect_catalog_windows(&value, &mut windows);
    Ok(windows)
}

fn collect_catalog_windows(value: &Value, windows: &mut HashMap<String, u64>) {
    match value {
        Value::Array(entries) => {
            for entry in entries {
                collect_catalog_windows(entry, windows);
            }
        }
        Value::Object(object) => {
            if let Some(model) = object
                .get("model")
                .or_else(|| object.get("id"))
                .or_else(|| object.get("name"))
                .and_then(Value::as_str)
                .and_then(normalize_model_selector)
                && let Some(window) = object_context_window(object)
            {
                windows.insert(model, window);
            }
            for (key, child) in object {
                if let Some(model) = normalize_model_selector(key)
                    && let Some(window) = value_context_window(child)
                {
                    windows.insert(model, window);
                }
                if matches!(key.as_str(), "contextWindows" | "models" | "data" | "items") {
                    collect_catalog_windows(child, windows);
                }
            }
        }
        _ => {}
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

fn minimum_positive_catalog_window(windows: &HashMap<String, u64>) -> Option<u64> {
    windows.values().copied().filter(|window| *window > 0).min()
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
pub(crate) fn reset_for_test() {
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

    #[test]
    fn parser_accepts_catalog_maps_arrays_and_only_positive_windows() {
        let parsed = parse_context_window_catalog(
            r#"{"contextWindows":{"routed-sonnet":{"contextWindow":372000},"bad":0},"models":[{"id":"claude-haiku-4-5","context_window":"200000"}]}"#,
        )
        .unwrap();
        assert_eq!(parsed.get("routed-sonnet"), Some(&372_000));
        assert_eq!(parsed.get("claude-haiku-4-5"), Some(&200_000));
        assert!(!parsed.contains_key("bad"));
    }

    #[test]
    fn injected_catalog_alias_wins_then_native_and_unknown_are_conservative() {
        reset_for_test();
        let proxy = "http://proxy.test";
        put_catalog_for_test(
            proxy,
            HashMap::from([
                ("routed-sonnet".to_string(), 372_000),
                ("small-route".to_string(), 128_000),
            ]),
        );
        let gateway = ClaudeGatewayProxyEnv::Inject {
            base_url: proxy.to_string(),
        };
        register_launch_provenance("tmux-a", Some("routed-sonnet"), &gateway);
        assert_eq!(
            context_window_for_turn("tmux-a", Some("routed-sonnet[1m]")),
            Some(372_000)
        );
        assert_eq!(
            context_window_for_turn("tmux-a", Some("claude-sonnet-5")),
            Some(1_000_000)
        );
        assert_eq!(
            context_window_for_turn("tmux-a", Some("unknown")),
            Some(128_000)
        );
    }

    #[test]
    fn scrub_uses_exact_native_table_and_does_not_follow_later_config() {
        reset_for_test();
        register_launch_provenance(
            "tmux-native",
            Some("claude-haiku-4-5"),
            &ClaudeGatewayProxyEnv::Scrub,
        );
        assert_eq!(context_window_for_turn("tmux-native", None), Some(200_000));
        assert_eq!(
            context_window_for_turn("tmux-native", Some("future-model")),
            Some(100_000)
        );
        assert_eq!(
            context_window_for_turn("unregistered", Some("claude-sonnet-5")),
            None
        );
    }
}
