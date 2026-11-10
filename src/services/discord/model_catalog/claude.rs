use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::{ModelCatalogEntry, is_safe_model_selector};
use crate::services::discord::runtime_store;

const GATEWAY_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const CLOCK_SKEW_ALLOWANCE: Duration = Duration::from_secs(5 * 60);
const MAX_CACHE_BODY_BYTES: usize = 256 * 1024;
const MAX_MODELS_PER_SOURCE: usize = 100;
const MAX_CATALOG_TEXT_CHARS: usize = 100;

static GATEWAY_ENTRIES: OnceLock<RwLock<Vec<ModelCatalogEntry>>> = OnceLock::new();
static OBSERVATION_WRITE: Mutex<()> = Mutex::new(());

type Observations = BTreeMap<String, String>;

#[derive(Clone, Copy)]
struct CuratedAlias {
    selector: &'static str,
    label: &'static str,
}

const CURATED_ALIASES: &[CuratedAlias] = &[
    CuratedAlias {
        selector: "fable",
        label: "Fable",
    },
    CuratedAlias {
        selector: "opus",
        label: "Opus",
    },
    CuratedAlias {
        selector: "sonnet",
        label: "Sonnet",
    },
    CuratedAlias {
        selector: "haiku",
        label: "Haiku",
    },
    CuratedAlias {
        selector: "fable[1m]",
        label: "Fable (extended context)",
    },
    CuratedAlias {
        selector: "sonnet[1m]",
        label: "Sonnet (extended context)",
    },
    CuratedAlias {
        selector: "opus[1m]",
        label: "Opus (extended context)",
    },
    CuratedAlias {
        selector: "opusplan",
        label: "Opus Plan",
    },
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

fn observations_path() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join("claude_model_observations.json"))
}

fn observable_pair(selector: &str, model: &str) -> bool {
    selector != "opusplan"
        && CURATED_ALIASES
            .iter()
            .any(|alias| alias.selector == selector)
        && is_safe_model_selector(model)
}

fn observations_from_raw(raw: &[u8]) -> Observations {
    serde_json::from_slice::<Observations>(raw)
        .unwrap_or_default()
        .into_iter()
        .filter(|(selector, model)| observable_pair(selector, model))
        .collect()
}

fn read_observations(path: Option<&Path>) -> Observations {
    path.and_then(read_cache_file)
        .map(|raw| observations_from_raw(&raw))
        .unwrap_or_default()
}

fn record_observation(
    path: &Path,
    selector: &str,
    response: &serde_json::Value,
) -> Result<(), String> {
    let Some(model) = response["message"]["model"].as_str() else {
        return Ok(());
    };
    if response["type"] != "assistant"
        || response["isApiErrorMessage"] == true
        || !observable_pair(selector, model)
    {
        return Ok(());
    }
    let guard = OBSERVATION_WRITE
        .lock()
        .map_err(|error| error.to_string())?;
    let mut observations = read_observations(Some(path));
    if observations.get(selector).is_some_and(|seen| seen == model) {
        return Ok(());
    }
    observations.insert(selector.to_string(), model.to_string());
    let serialized = serde_json::to_string(&observations).map_err(|error| error.to_string())?;
    let result = runtime_store::atomic_write(path, &serialized);
    drop(guard);
    result
}

pub(in crate::services::discord) fn record_claude_model_observation(
    selector: &str,
    response: &serde_json::Value,
) {
    if let Some(path) = observations_path()
        && let Err(error) = record_observation(&path, selector, response)
    {
        tracing::warn!(%error, "Claude model observation could not be saved");
    }
}

fn model_entry(id: String, label: String, source: &'static str) -> ModelCatalogEntry {
    ModelCatalogEntry::owned(
        id,
        truncate_catalog_text(&label, "Claude model"),
        source.to_string(),
        "Local bounded cache".to_string(),
    )
}

fn curated_entries(observations: &Observations) -> Vec<ModelCatalogEntry> {
    CURATED_ALIASES
        .iter()
        .map(|alias| {
            let label = observations.get(alias.selector).map_or_else(
                || alias.label.to_string(),
                |model| format!("{} · {model}", alias.label),
            );
            ModelCatalogEntry::owned(
                alias.selector.to_string(),
                truncate_catalog_text(&label, alias.label),
                "Evergreen Claude Code selector".to_string(),
                "Claude Code alias".to_string(),
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
            if !is_safe_model_selector(&id) {
                return None;
            }
            let label = truncate_catalog_text(&model.display_name, &id);
            Some(model_entry(id, label, "Claude gateway selector"))
        })
        .collect()
}

fn merge_entries(
    gateway_entries: Vec<ModelCatalogEntry>,
    observations: Observations,
) -> Vec<ModelCatalogEntry> {
    let observed_ids = observations
        .values()
        .map(|id| id.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut entries = curated_entries(&observations)
        .into_iter()
        .chain(gateway_entries)
        .chain(
            observations
                .into_values()
                .map(|id| model_entry(id.clone(), id, "Observed Claude response")),
        )
        .filter(|entry| seen.insert(entry.value.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    // Choose metadata before promotion so shared IDs retain the gateway label.
    entries.sort_by_key(|entry| {
        if CURATED_ALIASES
            .iter()
            .any(|alias| alias.selector == entry.value)
        {
            0
        } else if observed_ids.contains(&entry.value.to_ascii_lowercase()) {
            1
        } else {
            2
        }
    });
    entries
}

fn resolve_models(
    cache: &RwLock<Vec<ModelCatalogEntry>>,
    gateway_path: Option<&Path>,
    observed_path: Option<&Path>,
    current_ms: u64,
) -> Vec<ModelCatalogEntry> {
    let gateway = gateway_path
        .and_then(read_cache_file)
        .map(|raw| gateway_entries_from_raw(&raw, current_ms))
        .unwrap_or_default();
    if !gateway.is_empty()
        && let Ok(mut entries) = cache.write()
    {
        *entries = gateway;
    }
    let gateway = cache
        .read()
        .map(|entries| entries.clone())
        .unwrap_or_default();
    merge_entries(gateway, read_observations(observed_path))
}

pub(super) fn resolved_models() -> Vec<ModelCatalogEntry> {
    #[cfg(test)]
    if let Some(entries) = tests::resolve_test_sources() {
        return entries;
    }
    resolve_models(
        GATEWAY_ENTRIES.get_or_init(|| RwLock::new(Vec::new())),
        gateway_cache_path().as_deref(),
        observations_path().as_deref(),
        now_ms(),
    )
}

#[cfg(test)]
pub(in crate::services::discord) use tests::with_test_claude_catalog_sources;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    struct TestSources {
        gateway: PathBuf,
        observations: PathBuf,
        cache: RwLock<Vec<ModelCatalogEntry>>,
    }

    thread_local! {
        static SOURCES: std::cell::RefCell<Option<TestSources>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn resolve_test_sources() -> Option<Vec<ModelCatalogEntry>> {
        SOURCES.with(|slot| {
            slot.borrow().as_ref().map(|sources| {
                resolve_models(
                    &sources.cache,
                    Some(&sources.gateway),
                    Some(&sources.observations),
                    now_ms(),
                )
            })
        })
    }

    pub(in crate::services::discord) fn with_test_claude_catalog_sources<T>(
        gateway: PathBuf,
        observations: PathBuf,
        operation: impl FnOnce() -> T,
    ) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                SOURCES.with(|slot| slot.take());
            }
        }
        SOURCES.with(|slot| {
            assert!(slot.borrow().is_none());
            slot.replace(Some(TestSources {
                gateway,
                observations,
                cache: RwLock::new(Vec::new()),
            }));
        });
        let reset = Reset;
        let result = operation();
        drop(reset);
        result
    }

    #[test]
    fn invariant_claude_model_catalog_curated_aliases_remain_evergreen_runtime_selectors() {
        let entries = merge_entries(Vec::new(), Observations::new());
        assert_eq!(entries.len(), CURATED_ALIASES.len());
        for alias in CURATED_ALIASES {
            let entry = entries
                .iter()
                .find(|entry| entry.value == alias.selector)
                .unwrap();
            assert_eq!(entry.label, alias.label);
            assert!(!entry.label.chars().any(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn invariant_claude_model_catalog_invalid_and_stale_caches_fall_back_safely() {
        let stale = serde_json::json!({"fetchedAt":1,"models":[{"id":"gateway-only"}]}).to_string();
        let gateway = gateway_entries_from_raw(
            stale.as_bytes(),
            GATEWAY_CACHE_TTL.as_millis() as u64 + 10_000,
        );
        let entries = merge_entries(gateway, observations_from_raw(b"{"));
        assert_eq!(entries.len(), CURATED_ALIASES.len());
        assert_eq!(entries[0].label, "Fable");
    }

    #[test]
    fn invariant_claude_model_catalog_merge_priority_and_dedupe_are_stable() {
        let raw = serde_json::json!({"fetchedAt":10_000, "models":[
            {"id":"claude-shared","display_name":"Gateway winner"},
            {"id":"CLAUDE-SHARED","display_name":"Duplicate"}]})
        .to_string();
        let observations = BTreeMap::from([
            ("fable".to_string(), "claude-shared".to_string()),
            ("sonnet".to_string(), "claude-observed".to_string()),
        ]);
        let entries = merge_entries(
            gateway_entries_from_raw(raw.as_bytes(), 10_000),
            observations,
        );
        assert_eq!(entries[0].value, "fable");
        assert_eq!(entries[0].label, "Fable · claude-shared");
        let shared = entries
            .iter()
            .filter(|entry| entry.value.eq_ignore_ascii_case("claude-shared"))
            .collect::<Vec<_>>();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].label, "Gateway winner");
        assert!(entries.iter().any(|entry| entry.value == "claude-observed"));
    }

    #[test]
    fn invariant_claude_model_catalog_gateway_value_selector_is_supported() {
        let raw = serde_json::json!({"fetchedAt":10_000,
            "models":[{"value":"gateway[1m]","displayName":"Gateway extended context"}]})
        .to_string();
        let entries = merge_entries(
            gateway_entries_from_raw(raw.as_bytes(), 10_000),
            Observations::new(),
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.value == "gateway[1m]")
                .unwrap()
                .label,
            "Gateway extended context"
        );
    }

    #[test]
    fn invariant_claude_model_catalog_gateway_base_url_and_unknown_fields_never_surface() {
        let secret = "https://user:password@example.invalid/private?token=secret";
        let raw = serde_json::json!({"baseUrl":secret,"credentials":{"token":"credential-secret"},
            "fetchedAt":10_000,"models":[{"id":"claude-safe","display_name":"Safe label"}]})
        .to_string();
        let entries = merge_entries(
            gateway_entries_from_raw(raw.as_bytes(), 10_000),
            Observations::new(),
        );
        let rendered = format!("{entries:?}");
        assert!(rendered.contains("claude-safe"));
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("credential-secret"));
    }

    #[test]
    fn model_catalog_observation_concurrent_writes_preserve_aliases_and_complete_json() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("observations.json");
        std::thread::scope(|scope| {
            for alias in CURATED_ALIASES
                .iter()
                .filter(|alias| alias.selector != "opusplan")
            {
                let path = &path;
                scope.spawn(move || {
                    let response = serde_json::json!({"type":"assistant", "message":{"model":"claude-observed"}});
                    record_observation(path, alias.selector, &response).unwrap();
                });
            }
        });
        assert_eq!(
            read_observations(Some(&path)).len(),
            CURATED_ALIASES.len() - 1
        );
        assert!(serde_json::from_slice::<Observations>(&std::fs::read(path).unwrap()).is_ok());
    }

    #[test]
    fn model_catalog_observation_rejects_oversized_files_and_untrusted_pairs() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("observations.json");
        std::fs::write(&path, vec![b' '; MAX_CACHE_BODY_BYTES + 1]).unwrap();
        assert!(read_observations(Some(&path)).is_empty());
        let raw = br#"{"opusplan":"claude-model","not-alias":"claude-model","fable":"<synthetic>","opus":"unsafe/model"}"#;
        std::fs::write(&path, raw).unwrap();
        assert!(read_observations(Some(&path)).is_empty());
        let response =
            serde_json::json!({"type":"assistant","message":{"model":"claude-observed"}});
        record_observation(&path, "fable", &response).unwrap();
        assert_eq!(
            read_observations(Some(&path)).get("fable").unwrap(),
            "claude-observed"
        );
        assert!(std::fs::read_dir(directory.path()).unwrap().all(|file| {
            !file
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }
}
