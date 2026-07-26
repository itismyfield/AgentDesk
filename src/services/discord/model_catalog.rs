use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use regex::Regex;
use serde::Deserialize;

use crate::services::provider::ProviderKind;

mod bounded_cache_file;
mod claude;

pub(crate) const DISCORD_SELECT_OPTION_VALUE_LIMIT: usize = 100;

/// Sentinel value stored in the picker's pending state when the user selects "Default".
/// Callers use `is_default_picker_value()` rather than comparing this directly.
pub(in crate::services::discord) const DEFAULT_PICKER_VALUE: &str = "__agentdesk_default__";

pub(in crate::services::discord) fn is_default_picker_value(raw: &str) -> bool {
    raw == DEFAULT_PICKER_VALUE
}

/// Source labels used in `EffectiveModelSnapshot` and display functions.
pub(in crate::services::discord) const SOURCE_RUNTIME_OVERRIDE: &str = "runtime override";
pub(in crate::services::discord) const SOURCE_DISPATCH_ROLE: &str = "dispatch role override";
pub(in crate::services::discord) const SOURCE_ROLE_MAP: &str = "role-map";
pub(in crate::services::discord) const SOURCE_PROVIDER_DEFAULT: &str = "provider default";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelCatalogEntry {
    pub value: Cow<'static, str>,
    pub label: Cow<'static, str>,
    pub primary_summary: Cow<'static, str>,
    pub secondary_summary: Cow<'static, str>,
}

impl ModelCatalogEntry {
    const fn borrowed(
        value: &'static str,
        label: &'static str,
        primary_summary: &'static str,
        secondary_summary: &'static str,
    ) -> Self {
        Self {
            value: Cow::Borrowed(value),
            label: Cow::Borrowed(label),
            primary_summary: Cow::Borrowed(primary_summary),
            secondary_summary: Cow::Borrowed(secondary_summary),
        }
    }

    pub(super) fn owned(
        value: String,
        label: String,
        primary_summary: String,
        secondary_summary: String,
    ) -> Self {
        Self {
            value: Cow::Owned(value),
            label: Cow::Owned(label),
            primary_summary: Cow::Owned(primary_summary),
            secondary_summary: Cow::Owned(secondary_summary),
        }
    }

    pub(crate) fn picker_description(&self) -> String {
        format!("{} | {}", self.primary_summary, self.secondary_summary)
    }
}

#[derive(Clone, Copy)]
struct CatalogSummary {
    primary: &'static str,
    secondary: &'static str,
}

#[derive(Clone, Copy)]
struct StaticModelCatalogEntry {
    value: &'static str,
    label: &'static str,
    primary_summary: &'static str,
    secondary_summary: &'static str,
}

impl From<&StaticModelCatalogEntry> for ModelCatalogEntry {
    fn from(entry: &StaticModelCatalogEntry) -> Self {
        Self::borrowed(
            entry.value,
            entry.label,
            entry.primary_summary,
            entry.secondary_summary,
        )
    }
}

const CODEX_MODEL_CATALOG: &[StaticModelCatalogEntry] = &[
    StaticModelCatalogEntry {
        value: "gpt-5.5",
        label: "GPT-5.5",
        primary_summary: "Frontier complex work",
        secondary_summary: "Local CLI catalog",
    },
    StaticModelCatalogEntry {
        value: "gpt-5.4",
        label: "gpt-5.4",
        primary_summary: "Frontier coding baseline",
        secondary_summary: "API $2.5/$15",
    },
    StaticModelCatalogEntry {
        value: "gpt-5.4-mini",
        label: "GPT-5.4-Mini",
        primary_summary: "Fast strong mini",
        secondary_summary: "API $0.75/$4.5",
    },
    StaticModelCatalogEntry {
        value: "gpt-5.3-codex",
        label: "gpt-5.3-codex",
        primary_summary: "Fast Codex line",
        secondary_summary: "API $1.75/$14",
    },
    StaticModelCatalogEntry {
        value: "gpt-5.3-codex-spark",
        label: "GPT-5.3-Codex-Spark",
        primary_summary: "Text-only preview",
        secondary_summary: "No API",
    },
    StaticModelCatalogEntry {
        value: "gpt-5.2-codex",
        label: "gpt-5.2-codex",
        primary_summary: "Long-horizon coding",
        secondary_summary: "API $1.75/$14",
    },
    StaticModelCatalogEntry {
        value: "gpt-5.2",
        label: "gpt-5.2",
        primary_summary: "Long-running pro work",
        secondary_summary: "Local CLI catalog",
    },
    StaticModelCatalogEntry {
        value: "gpt-5.1-codex-max",
        label: "gpt-5.1-codex-max",
        primary_summary: "Legacy max agent model",
        secondary_summary: "API $1.25/$10",
    },
    StaticModelCatalogEntry {
        value: "gpt-5.1-codex-mini",
        label: "gpt-5.1-codex-mini",
        primary_summary: "Cheap fast codex mini",
        secondary_summary: "Local CLI catalog",
    },
];

const GEMINI_MODEL_CATALOG: &[StaticModelCatalogEntry] = &[
    StaticModelCatalogEntry {
        value: "auto-gemini-3",
        label: "Auto (Gemini 3)",
        primary_summary: "Preview auto routing",
        secondary_summary: "Pro/Flash preview",
    },
    StaticModelCatalogEntry {
        value: "auto-gemini-2.5",
        label: "Auto (Gemini 2.5)",
        primary_summary: "Stable auto routing",
        secondary_summary: "Pro/Flash stable",
    },
    StaticModelCatalogEntry {
        value: "gemini-3.1-pro-preview",
        label: "gemini-3.1-pro-preview",
        primary_summary: "Gemini 3.1 Pro preview",
        secondary_summary: "Local CLI catalog",
    },
    StaticModelCatalogEntry {
        value: "gemini-3-pro-preview",
        label: "gemini-3-pro-preview",
        primary_summary: "Frontier reasoning and coding",
        secondary_summary: "$2/$12",
    },
    StaticModelCatalogEntry {
        value: "gemini-3-flash-preview",
        label: "gemini-3-flash-preview",
        primary_summary: "Low-latency frontier work",
        secondary_summary: "$0.5/$3",
    },
    StaticModelCatalogEntry {
        value: "gemini-2.5-pro",
        label: "gemini-2.5-pro",
        primary_summary: "Stable advanced reasoning",
        secondary_summary: "$1.25/$10",
    },
    StaticModelCatalogEntry {
        value: "gemini-2.5-flash",
        label: "gemini-2.5-flash",
        primary_summary: "Stable fast fallback",
        secondary_summary: "$0.3/$2.5",
    },
    StaticModelCatalogEntry {
        value: "gemini-2.5-flash-lite",
        label: "gemini-2.5-flash-lite",
        primary_summary: "Low-cost flash-lite",
        secondary_summary: "Local CLI catalog",
    },
    StaticModelCatalogEntry {
        value: "gemini-3.1-flash-lite-preview",
        label: "gemini-3.1-flash-lite-preview",
        primary_summary: "Preview flash-lite variant",
        secondary_summary: "Local CLI catalog",
    },
];

static CODEX_MODEL_CATALOG_DYNAMIC: OnceLock<Mutex<FileBackedCatalogCache>> = OnceLock::new();
static GEMINI_MODEL_CATALOG_DYNAMIC: OnceLock<Mutex<FileBackedCatalogCache>> = OnceLock::new();
static QWEN_MODEL_CATALOG_CACHE: OnceLock<Mutex<HashMap<String, QwenResolvedCatalog>>> =
    OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileBackedCatalogCacheKey {
    path: PathBuf,
    modified_secs: u64,
    modified_nanos: u32,
    len: u64,
}

#[derive(Clone, Debug, Default)]
struct FileBackedCatalogCache {
    key: Option<FileBackedCatalogCacheKey>,
    entries: Vec<ModelCatalogEntry>,
}

#[derive(Debug, Deserialize)]
struct CodexModelsCache {
    models: Vec<CodexModelsCacheEntry>,
}

#[derive(Debug, Deserialize)]
struct CodexModelsCacheEntry {
    slug: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    visibility: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct QwenSettingsFile {
    #[serde(default, rename = "modelProviders")]
    model_providers: HashMap<String, Vec<QwenModelProviderEntry>>,
    #[serde(default)]
    model: Option<QwenSettingsModel>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct QwenSettingsModel {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct QwenModelProviderEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct QwenResolvedCatalog {
    entries: Vec<ModelCatalogEntry>,
    default_model: Option<String>,
}

fn codex_model_cache_path() -> Option<PathBuf> {
    model_catalog_home_dir().map(|home| home.join(".codex").join("models_cache.json"))
}

fn gemini_models_js_path() -> Option<PathBuf> {
    let gemini_bin = crate::services::gemini::resolve_gemini_path()?;
    let resolved = fs::canonicalize(gemini_bin).ok()?;
    let package_root = resolved.parent()?.parent()?;
    Some(
        package_root
            .join("node_modules")
            .join("@google")
            .join("gemini-cli-core")
            .join("dist")
            .join("src")
            .join("config")
            .join("models.js"),
    )
}

fn codex_catalog_summary(model: &str) -> CatalogSummary {
    match model {
        "gpt-5.4" => CatalogSummary {
            primary: "Frontier coding baseline",
            secondary: "API $2.5/$15",
        },
        "gpt-5.5" => CatalogSummary {
            primary: "Frontier complex work",
            secondary: "Local CLI catalog",
        },
        "gpt-5.4-mini" => CatalogSummary {
            primary: "Fast strong mini",
            secondary: "API $0.75/$4.5",
        },
        "gpt-5.3-codex-spark" => CatalogSummary {
            primary: "Text-only preview",
            secondary: "No API",
        },
        "gpt-5" => CatalogSummary {
            primary: "Prior frontier baseline",
            secondary: "API $1.25/$10",
        },
        "gpt-5.3-codex" => CatalogSummary {
            primary: "Fast Codex line",
            secondary: "API $1.75/$14",
        },
        "gpt-5.2-codex" => CatalogSummary {
            primary: "Long-horizon coding",
            secondary: "API $1.75/$14",
        },
        "gpt-5.2" => CatalogSummary {
            primary: "Long-running pro work",
            secondary: "Local CLI catalog",
        },
        "gpt-5.1-codex-max" => CatalogSummary {
            primary: "Legacy max agent model",
            secondary: "API $1.25/$10",
        },
        "gpt-5.1-codex-mini" => CatalogSummary {
            primary: "Cheap fast codex mini",
            secondary: "Local CLI catalog",
        },
        _ => CatalogSummary {
            primary: "Installed Codex model",
            secondary: "Local CLI catalog",
        },
    }
}

fn codex_visibility_allows_picker(visibility: Option<&str>) -> bool {
    match visibility {
        Some(raw) => raw.eq_ignore_ascii_case("list"),
        None => true,
    }
}

fn gemini_catalog_summary(model: &str) -> CatalogSummary {
    match model {
        "auto-gemini-3" => CatalogSummary {
            primary: "Preview auto routing",
            secondary: "Pro/Flash preview",
        },
        "auto-gemini-2.5" => CatalogSummary {
            primary: "Stable auto routing",
            secondary: "Pro/Flash stable",
        },
        "gemini-3.1-pro-preview" => CatalogSummary {
            primary: "Gemini 3.1 Pro preview",
            secondary: "Local CLI catalog",
        },
        "gemini-3-pro-preview" => CatalogSummary {
            primary: "Frontier reasoning and coding",
            secondary: "$2/$12",
        },
        "gemini-3-flash-preview" => CatalogSummary {
            primary: "Low-latency frontier work",
            secondary: "$0.5/$3",
        },
        "gemini-2.5-pro" => CatalogSummary {
            primary: "Stable advanced reasoning",
            secondary: "$1.25/$10",
        },
        "gemini-2.5-flash" => CatalogSummary {
            primary: "Stable fast fallback",
            secondary: "$0.3/$2.5",
        },
        "gemini-2.5-flash-lite" => CatalogSummary {
            primary: "Low-cost flash-lite",
            secondary: "Local CLI catalog",
        },
        "gemini-3.1-flash-lite-preview" => CatalogSummary {
            primary: "Preview flash-lite variant",
            secondary: "Local CLI catalog",
        },
        _ => CatalogSummary {
            primary: "Installed Gemini model",
            secondary: "Local CLI catalog",
        },
    }
}

fn file_backed_catalog_cache_key(path: PathBuf) -> Option<FileBackedCatalogCacheKey> {
    let metadata = fs::metadata(&path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .unwrap_or(Duration::ZERO);
    Some(FileBackedCatalogCacheKey {
        path,
        modified_secs: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
        len: metadata.len(),
    })
}

fn build_file_backed_catalog(
    cache: &'static OnceLock<Mutex<FileBackedCatalogCache>>,
    path: Option<PathBuf>,
    fallback: &[StaticModelCatalogEntry],
    parse: fn(&str) -> Option<Vec<ModelCatalogEntry>>,
) -> Vec<ModelCatalogEntry> {
    let Some(path) = path else {
        return fallback.iter().map(ModelCatalogEntry::from).collect();
    };
    let Some(cache_key) = file_backed_catalog_cache_key(path) else {
        return fallback.iter().map(ModelCatalogEntry::from).collect();
    };

    let cache_cell = cache.get_or_init(|| Mutex::new(FileBackedCatalogCache::default()));
    if let Ok(cached) = cache_cell.lock() {
        if cached.key.as_ref() == Some(&cache_key) {
            return cached.entries.clone();
        }
    }

    let entries = fs::read_to_string(&cache_key.path)
        .ok()
        .and_then(|raw| parse(&raw))
        .unwrap_or_else(|| fallback.iter().map(ModelCatalogEntry::from).collect());

    if let Ok(mut cached) = cache_cell.lock() {
        cached.key = Some(cache_key);
        cached.entries = entries.clone();
    }

    entries
}

fn build_codex_model_catalog() -> Vec<ModelCatalogEntry> {
    build_file_backed_catalog(
        &CODEX_MODEL_CATALOG_DYNAMIC,
        codex_model_cache_path(),
        CODEX_MODEL_CATALOG,
        build_codex_model_catalog_from_cache,
    )
}

fn build_codex_model_catalog_from_cache(raw: &str) -> Option<Vec<ModelCatalogEntry>> {
    let parsed: CodexModelsCache = serde_json::from_str(raw).ok()?;
    let mut seen = HashSet::new();
    let mut entries = Vec::new();

    for model in parsed.models {
        if model.slug.trim().is_empty() {
            continue;
        }
        if !codex_visibility_allows_picker(model.visibility.as_deref()) {
            continue;
        }
        if !seen.insert(model.slug.to_ascii_lowercase()) {
            continue;
        }
        let summary = codex_catalog_summary(&model.slug);
        let label = if model.display_name.trim().is_empty() {
            model.slug.clone()
        } else {
            model.display_name
        };
        entries.push(ModelCatalogEntry::owned(
            model.slug,
            label,
            summary.primary.to_string(),
            summary.secondary.to_string(),
        ));
    }

    (!entries.is_empty()).then_some(entries)
}

fn parse_gemini_model_exports(raw: &str) -> HashMap<String, String> {
    static EXPORT_RE: OnceLock<Regex> = OnceLock::new();
    let export_re =
        EXPORT_RE.get_or_init(|| Regex::new(r#"export const ([A-Z0-9_]+) = '([^']+)';"#).unwrap());

    export_re
        .captures_iter(raw)
        .filter_map(|caps| {
            Some((
                caps.get(1)?.as_str().to_string(),
                caps.get(2)?.as_str().to_string(),
            ))
        })
        .collect()
}

fn parse_gemini_valid_model_exports(raw: &str) -> HashSet<String> {
    static VALID_SET_RE: OnceLock<Regex> = OnceLock::new();
    static EXPORT_NAME_RE: OnceLock<Regex> = OnceLock::new();

    let Some(block) = VALID_SET_RE
        .get_or_init(|| {
            Regex::new(r#"VALID_GEMINI_MODELS\s*=\s*new Set\(\[(?s)(.*?)\]\)"#).unwrap()
        })
        .captures(raw)
        .and_then(|caps| caps.get(1).map(|value| value.as_str().to_string()))
    else {
        return HashSet::new();
    };

    let exports = parse_gemini_model_exports(raw);
    EXPORT_NAME_RE
        .get_or_init(|| Regex::new(r#"[A-Z0-9_]+"#).unwrap())
        .find_iter(&block)
        .filter_map(|name| exports.get(name.as_str()).cloned())
        .collect()
}

fn gemini_display_label(model: &str) -> String {
    match model {
        "auto-gemini-3" => "Auto (Gemini 3)".to_string(),
        "auto-gemini-2.5" => "Auto (Gemini 2.5)".to_string(),
        other => other.to_string(),
    }
}

fn build_gemini_model_catalog() -> Vec<ModelCatalogEntry> {
    build_file_backed_catalog(
        &GEMINI_MODEL_CATALOG_DYNAMIC,
        gemini_models_js_path(),
        GEMINI_MODEL_CATALOG,
        build_gemini_model_catalog_from_models_js,
    )
}

fn build_gemini_model_catalog_from_models_js(raw: &str) -> Option<Vec<ModelCatalogEntry>> {
    const GEMINI_EXPORT_ORDER: &[&str] = &[
        "PREVIEW_GEMINI_MODEL_AUTO",
        "DEFAULT_GEMINI_MODEL_AUTO",
        "PREVIEW_GEMINI_3_1_MODEL",
        "PREVIEW_GEMINI_MODEL",
        "PREVIEW_GEMINI_FLASH_MODEL",
        "DEFAULT_GEMINI_MODEL",
        "DEFAULT_GEMINI_FLASH_MODEL",
        "DEFAULT_GEMINI_FLASH_LITE_MODEL",
        "PREVIEW_GEMINI_3_1_FLASH_LITE_MODEL",
    ];

    let exports = parse_gemini_model_exports(raw);
    let valid_models = parse_gemini_valid_model_exports(raw);
    let mut seen = HashSet::new();
    let mut entries = Vec::new();

    for export_name in GEMINI_EXPORT_ORDER {
        let Some(model) = exports.get(*export_name) else {
            continue;
        };
        let is_auto_model = export_name.ends_with("_AUTO");
        if !valid_models.is_empty() && !is_auto_model && !valid_models.contains(model) {
            continue;
        }
        if !seen.insert(model.to_ascii_lowercase()) {
            continue;
        }
        let summary = gemini_catalog_summary(model);
        entries.push(ModelCatalogEntry::owned(
            model.clone(),
            gemini_display_label(model),
            summary.primary.to_string(),
            summary.secondary.to_string(),
        ));
    }

    (!entries.is_empty()).then_some(entries)
}

const CODEX_MODEL_ALIASES: &[(&str, &str)] = &[
    ("gpt-5-codex", "gpt-5-codex"),
    ("o3", "o3"),
    ("o4-mini", "o4-mini"),
];

const GEMINI_MODEL_ALIASES: &[(&str, &str)] = &[
    ("auto", "auto-gemini-3"),
    ("pro", "gemini-3.1-pro-preview"),
    ("flash", "gemini-3-flash-preview"),
    ("flash-lite", "gemini-2.5-flash-lite"),
    ("gemini-3.1-pro", "gemini-3.1-pro-preview"),
    ("gemini-3-pro", "gemini-3-pro-preview"),
    ("gemini-3-flash", "gemini-3-flash-preview"),
    ("gemini-2.5-pro", "gemini-2.5-pro"),
    ("gemini-2.5-flash", "gemini-2.5-flash"),
];

fn qwen_system_defaults_path() -> Option<PathBuf> {
    std::env::var("QWEN_CODE_SYSTEM_DEFAULTS_PATH")
        .ok()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn qwen_user_settings_path() -> Option<PathBuf> {
    model_catalog_home_dir()
        .map(|home| home.join(".qwen").join("settings.json"))
        .filter(|path| path.is_file())
}

fn model_catalog_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

fn qwen_project_settings_path(working_dir: Option<&str>) -> Option<PathBuf> {
    working_dir
        .map(PathBuf::from)
        .map(|path| path.join(".qwen").join("settings.json"))
        .filter(|path| path.is_file())
}

fn qwen_system_settings_path() -> Option<PathBuf> {
    std::env::var("QWEN_CODE_SYSTEM_SETTINGS_PATH")
        .ok()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn load_qwen_settings_file(path: &PathBuf) -> Option<QwenSettingsFile> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn qwen_catalog_cache_key(layers: &[Option<PathBuf>]) -> String {
    layers
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let Some(path) = path else {
                return format!("{index}:<none>");
            };
            let metadata = fs::metadata(path).ok();
            let len = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
            let modified = metadata
                .as_ref()
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| format!("{}.{:09}", time.as_secs(), time.subsec_nanos()))
                .unwrap_or_else(|| "unknown".to_string());
            format!("{index}:{}|{len}|{modified}", path.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_catalog_text(raw: &str, fallback: &str, max_chars: usize) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    let collected: String = trimmed.chars().take(max_chars).collect();
    if trimmed.chars().count() > max_chars {
        format!("{}...", collected)
    } else {
        collected
    }
}

fn qwen_secondary_summary(auth_type: &str) -> String {
    format!("Qwen settings ({})", auth_type)
}

fn resolve_qwen_model_catalog(working_dir: Option<&str>) -> QwenResolvedCatalog {
    let layers = [
        qwen_system_defaults_path(),
        qwen_user_settings_path(),
        qwen_project_settings_path(working_dir),
        qwen_system_settings_path(),
    ];
    let cache_key = qwen_catalog_cache_key(&layers);
    if let Some(cached) = QWEN_MODEL_CATALOG_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|cache| cache.get(&cache_key).cloned())
    {
        return cached;
    }

    let mut merged_entries: HashMap<String, (usize, ModelCatalogEntry)> = HashMap::new();
    let mut next_order = 0usize;
    let mut default_model: Option<String> = None;

    for settings in layers.iter().flatten().filter_map(load_qwen_settings_file) {
        if let Some(default_name) = settings
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            default_model = Some(default_name.to_string());
        }

        for (auth_type, models) in settings.model_providers {
            for model in models {
                let model_id = model.id.trim();
                if model_id.is_empty() {
                    continue;
                }
                let dedupe_key = format!(
                    "{}/{}",
                    auth_type.to_ascii_lowercase(),
                    model_id.to_ascii_lowercase()
                );
                let label = model
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(model_id);
                let primary_summary = truncate_catalog_text(
                    model.description.as_deref().unwrap_or(""),
                    &format!("Configured {} model", auth_type),
                    52,
                );
                let secondary_summary = qwen_secondary_summary(&auth_type);
                next_order += 1;
                merged_entries.insert(
                    dedupe_key,
                    (
                        next_order,
                        ModelCatalogEntry::owned(
                            model_id.to_string(),
                            label.to_string(),
                            primary_summary,
                            secondary_summary,
                        ),
                    ),
                );
            }
        }
    }

    let mut entries: Vec<(usize, ModelCatalogEntry)> = merged_entries.into_values().collect();
    entries.sort_by_key(|(order, _)| *order);
    let mut entries: Vec<ModelCatalogEntry> = entries.into_iter().map(|(_, entry)| entry).collect();

    if let Some(default_model) = default_model.as_deref() {
        let exists = entries
            .iter()
            .any(|entry| entry.value.eq_ignore_ascii_case(default_model));
        if !exists {
            entries.insert(
                0,
                ModelCatalogEntry::owned(
                    default_model.to_string(),
                    default_model.to_string(),
                    "Configured default model".to_string(),
                    "Qwen settings.model.name".to_string(),
                ),
            );
        }
    }

    let resolved = QwenResolvedCatalog {
        entries,
        default_model,
    };

    if let Ok(mut cache) = QWEN_MODEL_CATALOG_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        cache.insert(cache_key, resolved.clone());
    }

    resolved
}

pub(crate) fn resolved_default_model(
    provider: &ProviderKind,
    working_dir: Option<&str>,
) -> Option<String> {
    match provider {
        ProviderKind::Qwen => resolve_qwen_model_catalog(working_dir).default_model,
        _ => None,
    }
}

pub(crate) fn resolved_models(
    provider: &ProviderKind,
    working_dir: Option<&str>,
) -> Vec<ModelCatalogEntry> {
    match provider {
        ProviderKind::Qwen => resolve_qwen_model_catalog(working_dir).entries,
        _ => known_models(provider),
    }
}

pub(in crate::services::discord) fn provider_supports_model_override(
    provider: &ProviderKind,
) -> bool {
    matches!(
        provider,
        ProviderKind::Claude
            | ProviderKind::Codex
            | ProviderKind::Gemini
            | ProviderKind::OpenCode
            | ProviderKind::Qwen
    )
}

pub(in crate::services::discord) fn model_hint(
    provider: &ProviderKind,
    working_dir: Option<&str>,
) -> String {
    match provider {
        ProviderKind::Claude => "default + curated Claude models + custom model id".to_string(),
        ProviderKind::Codex => {
            "default + models resolved from local Codex catalog + custom model id".to_string()
        }
        ProviderKind::Gemini => {
            "default + models resolved from local Gemini catalog + custom model id".to_string()
        }
        ProviderKind::OpenCode => "default + custom providerID/modelID".to_string(),
        ProviderKind::Qwen => {
            let catalog = resolve_qwen_model_catalog(working_dir);
            if catalog.entries.is_empty() {
                "Qwen settings catalog is empty. Check ~/.qwen/settings.json or <workspace>/.qwen/settings.json".to_string()
            } else {
                "default + models resolved from Qwen settings files".to_string()
            }
        }
        ProviderKind::Unsupported(_) => "모델 이름 또는 default".to_string(),
    }
}

pub(crate) fn known_models(provider: &ProviderKind) -> Vec<ModelCatalogEntry> {
    match provider {
        ProviderKind::Claude => claude::resolved_models(),
        ProviderKind::Codex => build_codex_model_catalog(),
        ProviderKind::Gemini => build_gemini_model_catalog(),
        ProviderKind::OpenCode | ProviderKind::Qwen => Vec::new(),
        ProviderKind::Unsupported(_) => Vec::new(),
    }
}

pub(in crate::services::discord) fn spawn_claude_model_catalog_refresh(
    provider: &ProviderKind,
) -> Option<tokio::task::JoinHandle<()>> {
    claude::spawn_background_refresh(provider)
}

#[cfg(test)]
pub(in crate::services::discord) fn spawn_test_claude_model_catalog_refresh(
    refreshes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    claude::spawn_test_background_refresh(refreshes)
}

#[cfg(test)]
pub(in crate::services::discord) fn spawn_test_claude_model_catalog_refresh_after_claim(
    refreshes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    after_claim: impl Fn() + Send + Sync + 'static,
) -> tokio::task::JoinHandle<()> {
    claude::spawn_test_background_refresh_after_claim(refreshes, after_claim)
}

#[cfg(test)]
pub(in crate::services::discord) fn with_test_claude_model_catalog_refresh_state<T>(
    operation: impl FnOnce() -> T,
) -> T {
    claude::with_test_refresh_task_state(operation)
}

#[cfg(test)]
pub(in crate::services::discord) fn test_claude_model_catalog_refresh_running() -> bool {
    claude::test_refresh_task_running()
}

fn model_aliases(provider: &ProviderKind) -> &'static [(&'static str, &'static str)] {
    match provider {
        ProviderKind::Codex => CODEX_MODEL_ALIASES,
        ProviderKind::Gemini => GEMINI_MODEL_ALIASES,
        ProviderKind::Claude | ProviderKind::OpenCode | ProviderKind::Qwen => &[],
        ProviderKind::Unsupported(_) => &[],
    }
}

fn canonical_known_model(provider: &ProviderKind, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    for entry in known_models(provider) {
        if entry.value.eq_ignore_ascii_case(trimmed) {
            return Some(entry.value.into_owned());
        }
    }

    model_aliases(provider)
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(trimmed))
        .map(|(_, canonical)| (*canonical).to_string())
}

fn is_safe_model_selector(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed.is_ascii()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

pub(crate) fn is_valid_discord_select_option_value(raw: &str) -> bool {
    !raw.is_empty() && raw.chars().count() <= DISCORD_SELECT_OPTION_VALUE_LIMIT
}

pub(in crate::services::discord) fn validate_model_input(
    provider: &ProviderKind,
    raw: &str,
    working_dir: Option<&str>,
) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Model name cannot be empty.".to_string());
    }
    if !is_valid_discord_select_option_value(trimmed) {
        return Err(format!(
            "Model name cannot exceed {DISCORD_SELECT_OPTION_VALUE_LIMIT} characters."
        ));
    }

    if matches!(provider, ProviderKind::Qwen) {
        if let Some(entry) = resolved_models(provider, working_dir)
            .iter()
            .find(|entry| entry.value.eq_ignore_ascii_case(trimmed))
        {
            return Ok(entry.value.to_string());
        }

        return Err(format!(
            "Unrecognized model `{}` for {}.\n{}\nUse `/model` to open the interactive picker.",
            trimmed,
            provider.display_name(),
            model_hint(provider, working_dir)
        ));
    }

    if matches!(provider, ProviderKind::OpenCode) {
        if is_valid_opencode_model_override(trimmed) {
            return Ok(trimmed.to_string());
        }

        return Err(format!(
            "Unrecognized model `{}` for {}.\nOpenCode model overrides must use providerID/modelID, for example `anthropic/claude-sonnet-4-5`.\nUse `/model` to open the interactive picker.",
            trimmed,
            provider.display_name()
        ));
    }

    if let Some(canonical) = canonical_known_model(provider, trimmed) {
        return Ok(canonical);
    }

    if is_safe_model_selector(trimmed) {
        return Ok(trimmed.to_string());
    }

    Err(format!(
        "Unrecognized model `{}` for {}.\n{}\nUse `/model` to open the interactive picker.",
        trimmed,
        provider.display_name(),
        model_hint(provider, working_dir)
    ))
}

fn is_valid_opencode_model_override(raw: &str) -> bool {
    let Some((provider_id, model_id)) = raw.split_once('/') else {
        return false;
    };
    !provider_id.trim().is_empty()
        && !model_id.trim().is_empty()
        && is_valid_discord_select_option_value(raw)
        && raw.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '/' | '[' | ']')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invariant_claude_model_alias_validation_preserves_runtime_selector() {
        for selector in ["fable", "opus", "sonnet", "haiku"] {
            assert_eq!(
                validate_model_input(&ProviderKind::Claude, selector, None).unwrap(),
                selector
            );
        }
    }

    #[test]
    fn invariant_claude_model_selector_validation_is_ascii_and_bounded() {
        assert!(validate_model_input(&ProviderKind::Claude, &"a".repeat(64), None).is_ok());
        assert!(validate_model_input(&ProviderKind::Claude, &"a".repeat(65), None).is_err());
        assert!(validate_model_input(&ProviderKind::Claude, "claude/unsafe", None).is_err());
        assert!(validate_model_input(&ProviderKind::Claude, "클로드", None).is_err());
    }

    #[test]
    fn invariant_model_input_rejects_values_over_discord_option_limit_without_truncating() {
        let valid = format!("provider/{}", "x".repeat(91));
        let invalid = format!("provider/{}", "x".repeat(92));

        assert_eq!(valid.chars().count(), DISCORD_SELECT_OPTION_VALUE_LIMIT);
        assert_eq!(
            validate_model_input(&ProviderKind::OpenCode, &valid, None).unwrap(),
            valid
        );
        assert!(validate_model_input(&ProviderKind::OpenCode, &invalid, None).is_err());
    }
}
