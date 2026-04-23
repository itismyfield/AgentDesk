use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use regex::Regex;
use serde::Deserialize;

use crate::services::provider::ProviderKind;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModelCatalogEntry {
    pub value: &'static str,
    pub label: &'static str,
    pub primary_summary: &'static str,
    pub secondary_summary: &'static str,
}

impl ModelCatalogEntry {
    pub(crate) fn picker_description(&self) -> String {
        format!("{} | {}", self.primary_summary, self.secondary_summary)
    }
}

#[derive(Clone, Copy)]
struct CatalogSummary {
    primary: &'static str,
    secondary: &'static str,
}

// Curated from installed provider CLIs and Anthropic Claude Code docs as of 2026-03-30.
const CLAUDE_MODEL_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        value: "sonnet",
        label: "Sonnet 4.6",
        primary_summary: "Latest Sonnet 4.6 alias",
        secondary_summary: "Claude Code plan",
    },
    ModelCatalogEntry {
        value: "opus",
        label: "Opus 4.6",
        primary_summary: "Highest quality 4.6 alias",
        secondary_summary: "Claude Code plan",
    },
    ModelCatalogEntry {
        value: "haiku",
        label: "Haiku 4.5",
        primary_summary: "Fast simple-task 4.5 alias",
        secondary_summary: "Claude Code plan",
    },
    ModelCatalogEntry {
        value: "sonnet[1m]",
        label: "Sonnet 4.6 1M",
        primary_summary: "1M context window",
        secondary_summary: "Sonnet 4.6 alias",
    },
    ModelCatalogEntry {
        value: "opus[1m]",
        label: "Opus 4.6 1M",
        primary_summary: "1M context window",
        secondary_summary: "Opus 4.6 alias",
    },
    ModelCatalogEntry {
        value: "opusplan",
        label: "Opus Plan 4.6",
        primary_summary: "Opus 4.6 planning",
        secondary_summary: "Sonnet 4.6 executes",
    },
];

const CODEX_MODEL_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        value: "gpt-5.5",
        label: "GPT-5.5",
        primary_summary: "Frontier complex work",
        secondary_summary: "Local CLI catalog",
    },
    ModelCatalogEntry {
        value: "gpt-5.4",
        label: "gpt-5.4",
        primary_summary: "Frontier coding baseline",
        secondary_summary: "API $2.5/$15",
    },
    ModelCatalogEntry {
        value: "gpt-5.4-mini",
        label: "GPT-5.4-Mini",
        primary_summary: "Fast strong mini",
        secondary_summary: "API $0.75/$4.5",
    },
    ModelCatalogEntry {
        value: "gpt-5.3-codex",
        label: "gpt-5.3-codex",
        primary_summary: "Fast Codex line",
        secondary_summary: "API $1.75/$14",
    },
    ModelCatalogEntry {
        value: "gpt-5.3-codex-spark",
        label: "GPT-5.3-Codex-Spark",
        primary_summary: "Text-only preview",
        secondary_summary: "No API",
    },
    ModelCatalogEntry {
        value: "gpt-5.2-codex",
        label: "gpt-5.2-codex",
        primary_summary: "Long-horizon coding",
        secondary_summary: "API $1.75/$14",
    },
    ModelCatalogEntry {
        value: "gpt-5.2",
        label: "gpt-5.2",
        primary_summary: "Long-running pro work",
        secondary_summary: "Local CLI catalog",
    },
    ModelCatalogEntry {
        value: "gpt-5.1-codex-max",
        label: "gpt-5.1-codex-max",
        primary_summary: "Legacy max agent model",
        secondary_summary: "API $1.25/$10",
    },
    ModelCatalogEntry {
        value: "gpt-5.1-codex-mini",
        label: "gpt-5.1-codex-mini",
        primary_summary: "Cheap fast codex mini",
        secondary_summary: "Local CLI catalog",
    },
];

const GEMINI_MODEL_CATALOG: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        value: "auto-gemini-3",
        label: "Auto (Gemini 3)",
        primary_summary: "Preview auto routing",
        secondary_summary: "Pro/Flash preview",
    },
    ModelCatalogEntry {
        value: "auto-gemini-2.5",
        label: "Auto (Gemini 2.5)",
        primary_summary: "Stable auto routing",
        secondary_summary: "Pro/Flash stable",
    },
    ModelCatalogEntry {
        value: "gemini-3.1-pro-preview",
        label: "gemini-3.1-pro-preview",
        primary_summary: "Gemini 3.1 Pro preview",
        secondary_summary: "Local CLI catalog",
    },
    ModelCatalogEntry {
        value: "gemini-3-pro-preview",
        label: "gemini-3-pro-preview",
        primary_summary: "Frontier reasoning and coding",
        secondary_summary: "$2/$12",
    },
    ModelCatalogEntry {
        value: "gemini-3-flash-preview",
        label: "gemini-3-flash-preview",
        primary_summary: "Low-latency frontier work",
        secondary_summary: "$0.5/$3",
    },
    ModelCatalogEntry {
        value: "gemini-2.5-pro",
        label: "gemini-2.5-pro",
        primary_summary: "Stable advanced reasoning",
        secondary_summary: "$1.25/$10",
    },
    ModelCatalogEntry {
        value: "gemini-2.5-flash",
        label: "gemini-2.5-flash",
        primary_summary: "Stable fast fallback",
        secondary_summary: "$0.3/$2.5",
    },
    ModelCatalogEntry {
        value: "gemini-2.5-flash-lite",
        label: "gemini-2.5-flash-lite",
        primary_summary: "Low-cost flash-lite",
        secondary_summary: "Local CLI catalog",
    },
    ModelCatalogEntry {
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
    default_model: Option<&'static str>,
}

fn intern_owned(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
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
    fallback: &[ModelCatalogEntry],
    parse: fn(&str) -> Option<Vec<ModelCatalogEntry>>,
) -> Vec<ModelCatalogEntry> {
    let Some(path) = path else {
        return fallback.to_vec();
    };
    let Some(cache_key) = file_backed_catalog_cache_key(path) else {
        return fallback.to_vec();
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
        .unwrap_or_else(|| fallback.to_vec());

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
        entries.push(ModelCatalogEntry {
            value: intern_owned(model.slug),
            label: intern_owned(label),
            primary_summary: summary.primary,
            secondary_summary: summary.secondary,
        });
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
        entries.push(ModelCatalogEntry {
            value: intern_owned(model.clone()),
            label: intern_owned(gemini_display_label(model)),
            primary_summary: summary.primary,
            secondary_summary: summary.secondary,
        });
    }

    (!entries.is_empty()).then_some(entries)
}

const CLAUDE_MODEL_ALIASES: &[(&str, &str)] = &[
    ("opus", "claude-opus-4-6"),
    ("sonnet", "claude-sonnet-4-6"),
    ("haiku", "claude-haiku-4-5-20251001"),
];

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
    #[cfg(test)]
    if let Some(path) = std::env::var_os("AGENTDESK_TEST_HOME") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }

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
    let mut default_model: Option<&'static str> = None;

    for settings in layers.iter().flatten().filter_map(load_qwen_settings_file) {
        if let Some(default_name) = settings
            .model
            .as_ref()
            .and_then(|model| model.name.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            default_model = Some(intern_owned(default_name.to_string()));
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
                        ModelCatalogEntry {
                            value: intern_owned(model_id.to_string()),
                            label: intern_owned(label.to_string()),
                            primary_summary: intern_owned(primary_summary),
                            secondary_summary: intern_owned(secondary_summary),
                        },
                    ),
                );
            }
        }
    }

    let mut entries: Vec<(usize, ModelCatalogEntry)> = merged_entries.into_values().collect();
    entries.sort_by_key(|(order, _)| *order);
    let mut entries: Vec<ModelCatalogEntry> = entries.into_iter().map(|(_, entry)| entry).collect();

    if let Some(default_model) = default_model {
        let exists = entries
            .iter()
            .any(|entry| entry.value.eq_ignore_ascii_case(default_model));
        if !exists {
            entries.insert(
                0,
                ModelCatalogEntry {
                    value: default_model,
                    label: default_model,
                    primary_summary: "Configured default model",
                    secondary_summary: "Qwen settings.model.name",
                },
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
        ProviderKind::Qwen => resolve_qwen_model_catalog(working_dir)
            .default_model
            .map(str::to_string),
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
        ProviderKind::Claude | ProviderKind::Codex | ProviderKind::Gemini | ProviderKind::Qwen
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
        ProviderKind::Claude => CLAUDE_MODEL_CATALOG.to_vec(),
        ProviderKind::Codex => build_codex_model_catalog(),
        ProviderKind::Gemini => build_gemini_model_catalog(),
        ProviderKind::Qwen => Vec::new(),
        ProviderKind::Unsupported(_) => Vec::new(),
    }
}

fn model_aliases(provider: &ProviderKind) -> &'static [(&'static str, &'static str)] {
    match provider {
        ProviderKind::Claude => CLAUDE_MODEL_ALIASES,
        ProviderKind::Codex => CODEX_MODEL_ALIASES,
        ProviderKind::Gemini => GEMINI_MODEL_ALIASES,
        ProviderKind::Qwen => &[],
        ProviderKind::Unsupported(_) => &[],
    }
}

fn canonical_known_model(provider: &ProviderKind, raw: &str) -> Option<&'static str> {
    let trimmed = raw.trim();
    for entry in known_models(provider) {
        if entry.value.eq_ignore_ascii_case(trimmed) {
            return Some(entry.value);
        }
    }

    model_aliases(provider)
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(trimmed))
        .map(|(_, canonical)| *canonical)
}

fn looks_like_model_identifier(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
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

    if let Some(canonical) = canonical_known_model(provider, trimmed) {
        return Ok(canonical.to_string());
    }

    if looks_like_model_identifier(trimmed) {
        return Ok(trimmed.to_string());
    }

    Err(format!(
        "Unrecognized model `{}` for {}.\n{}\nUse `/model` to open the interactive picker.",
        trimmed,
        provider.display_name(),
        model_hint(provider, working_dir)
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::services::provider::ProviderKind;

    use super::{build_codex_model_catalog_from_cache, build_gemini_model_catalog_from_models_js};

    fn with_temp_model_catalog_home<F>(f: F)
    where
        F: FnOnce(&TempDir),
    {
        let _guard = super::super::runtime_store::lock_test_env();
        let temp_home = TempDir::new().unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_test_home = std::env::var_os("AGENTDESK_TEST_HOME");

        unsafe {
            std::env::set_var("HOME", temp_home.path());
            std::env::set_var("USERPROFILE", temp_home.path());
            std::env::set_var("AGENTDESK_TEST_HOME", temp_home.path());
        }

        f(&temp_home);

        match prev_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_userprofile {
            Some(value) => unsafe { std::env::set_var("USERPROFILE", value) },
            None => unsafe { std::env::remove_var("USERPROFILE") },
        }
        match prev_test_home {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_TEST_HOME", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_TEST_HOME") },
        }
    }

    fn write_codex_models_cache(home: &TempDir, raw: &str) {
        let codex_dir = home.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(codex_dir.join("models_cache.json"), raw).unwrap();
    }

    fn with_temp_qwen_env<F>(f: F)
    where
        F: FnOnce(&TempDir, &TempDir),
    {
        let _guard = super::super::runtime_store::lock_test_env();
        let temp_home = TempDir::new().unwrap();
        let temp_project = TempDir::new().unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_test_home = std::env::var_os("AGENTDESK_TEST_HOME");
        let prev_system_defaults = std::env::var_os("QWEN_CODE_SYSTEM_DEFAULTS_PATH");
        let prev_system_settings = std::env::var_os("QWEN_CODE_SYSTEM_SETTINGS_PATH");

        unsafe {
            std::env::set_var("HOME", temp_home.path());
            std::env::set_var("USERPROFILE", temp_home.path());
            std::env::set_var("AGENTDESK_TEST_HOME", temp_home.path());
            std::env::remove_var("QWEN_CODE_SYSTEM_DEFAULTS_PATH");
            std::env::remove_var("QWEN_CODE_SYSTEM_SETTINGS_PATH");
        }

        f(&temp_home, &temp_project);

        match prev_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_userprofile {
            Some(value) => unsafe { std::env::set_var("USERPROFILE", value) },
            None => unsafe { std::env::remove_var("USERPROFILE") },
        }
        match prev_test_home {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_TEST_HOME", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_TEST_HOME") },
        }
        match prev_system_defaults {
            Some(value) => unsafe { std::env::set_var("QWEN_CODE_SYSTEM_DEFAULTS_PATH", value) },
            None => unsafe { std::env::remove_var("QWEN_CODE_SYSTEM_DEFAULTS_PATH") },
        }
        match prev_system_settings {
            Some(value) => unsafe { std::env::set_var("QWEN_CODE_SYSTEM_SETTINGS_PATH", value) },
            None => unsafe { std::env::remove_var("QWEN_CODE_SYSTEM_SETTINGS_PATH") },
        }
    }

    #[test]
    fn codex_dynamic_catalog_uses_local_display_names() {
        let raw = r#"{
          "models": [
            { "slug": "gpt-5.4", "display_name": "gpt-5.4", "visibility": "list" },
            { "slug": "gpt-5.5", "display_name": "GPT-5.5", "visibility": "list" },
            { "slug": "gpt-5.4-mini", "display_name": "GPT-5.4-Mini", "visibility": "list" },
            { "slug": "gpt-5.1", "display_name": "gpt-5.1", "visibility": "hide" },
            { "slug": "gpt-5.3-codex-spark", "display_name": "GPT-5.3-Codex-Spark", "visibility": "list" }
          ]
        }"#;

        let catalog = build_codex_model_catalog_from_cache(raw).expect("catalog");
        assert_eq!(catalog[0].value, "gpt-5.4");
        assert_eq!(catalog[0].label, "gpt-5.4");
        assert_eq!(catalog[1].value, "gpt-5.5");
        assert_eq!(catalog[1].label, "GPT-5.5");
        assert_eq!(catalog[2].label, "GPT-5.4-Mini");
        assert_eq!(
            catalog[3].picker_description(),
            "Text-only preview | No API"
        );
        assert!(!catalog.iter().any(|entry| entry.value == "gpt-5.1"));
    }

    #[test]
    fn codex_known_models_reloads_when_local_cache_changes() {
        with_temp_model_catalog_home(|home| {
            write_codex_models_cache(
                home,
                r#"{
                  "models": [
                    { "slug": "gpt-5.4", "display_name": "gpt-5.4", "visibility": "list" }
                  ]
                }"#,
            );
            assert!(
                super::known_models(&ProviderKind::Codex)
                    .iter()
                    .any(|entry| entry.value == "gpt-5.4")
            );

            write_codex_models_cache(
                home,
                r#"{
                  "models": [
                    { "slug": "gpt-5.5", "display_name": "GPT-5.5", "visibility": "list" },
                    { "slug": "gpt-5.4-mini", "display_name": "GPT-5.4-Mini", "visibility": "list" }
                  ]
                }"#,
            );
            let catalog = super::known_models(&ProviderKind::Codex);
            assert!(catalog.iter().any(|entry| entry.value == "gpt-5.5"));
            assert!(!catalog.iter().any(|entry| entry.value == "gpt-5.4"));
        });
    }

    #[test]
    fn gemini_dynamic_catalog_reads_models_js_exports() {
        let raw = r#"
export const PREVIEW_GEMINI_MODEL = 'gemini-3-pro-preview';
export const PREVIEW_GEMINI_3_1_MODEL = 'gemini-3.1-pro-preview';
export const PREVIEW_GEMINI_FLASH_MODEL = 'gemini-3-flash-preview';
export const PREVIEW_GEMINI_3_1_FLASH_LITE_MODEL = 'gemini-3.1-flash-lite-preview';
export const DEFAULT_GEMINI_MODEL = 'gemini-2.5-pro';
export const DEFAULT_GEMINI_FLASH_MODEL = 'gemini-2.5-flash';
export const DEFAULT_GEMINI_FLASH_LITE_MODEL = 'gemini-obsolete-lite';
export const PREVIEW_GEMINI_MODEL_AUTO = 'auto-gemini-3';
export const DEFAULT_GEMINI_MODEL_AUTO = 'auto-gemini-2.5';
export const VALID_GEMINI_MODELS = new Set([
  PREVIEW_GEMINI_MODEL,
  PREVIEW_GEMINI_3_1_MODEL,
  PREVIEW_GEMINI_FLASH_MODEL,
  PREVIEW_GEMINI_3_1_FLASH_LITE_MODEL,
  DEFAULT_GEMINI_MODEL,
  DEFAULT_GEMINI_FLASH_MODEL
]);
"#;

        let catalog = build_gemini_model_catalog_from_models_js(raw).expect("catalog");
        assert_eq!(catalog[0].value, "auto-gemini-3");
        assert_eq!(catalog[0].label, "Auto (Gemini 3)");
        assert!(
            catalog
                .iter()
                .any(|entry| entry.value == "gemini-3.1-pro-preview")
        );
        assert!(
            catalog
                .iter()
                .any(|entry| entry.value == "gemini-3.1-flash-lite-preview"
                    && entry.picker_description()
                        == "Preview flash-lite variant | Local CLI catalog")
        );
        assert!(
            !catalog
                .iter()
                .any(|entry| entry.value == "gemini-obsolete-lite")
        );
    }

    #[test]
    fn qwen_resolved_models_merge_user_and_project_settings() {
        with_temp_qwen_env(|temp_home, temp_project| {
            let user_qwen_dir = temp_home.path().join(".qwen");
            let project_qwen_dir = temp_project.path().join(".qwen");
            fs::create_dir_all(&user_qwen_dir).unwrap();
            fs::create_dir_all(&project_qwen_dir).unwrap();

            fs::write(
                user_qwen_dir.join("settings.json"),
                r#"{
                  "modelProviders": {
                    "openai": [
                      {
                        "id": "user-model",
                        "name": "User Model",
                        "description": "User scoped model"
                      }
                    ]
                  },
                  "model": { "name": "user-model" }
                }"#,
            )
            .unwrap();

            fs::write(
                project_qwen_dir.join("settings.json"),
                r#"{
                  "modelProviders": {
                    "openai": [
                      {
                        "id": "project-model",
                        "name": "Project Model",
                        "description": "Project scoped model"
                      }
                    ]
                  },
                  "model": { "name": "project-model" }
                }"#,
            )
            .unwrap();

            let working_dir = temp_project.path().to_str().unwrap();
            let default_model =
                super::resolved_default_model(&ProviderKind::Qwen, Some(working_dir)).unwrap();
            assert_eq!(default_model, "project-model");

            let catalog = super::resolved_models(&ProviderKind::Qwen, Some(working_dir));
            assert!(
                catalog
                    .iter()
                    .any(|entry| entry.value == "user-model" && entry.label == "User Model")
            );
            assert!(catalog.iter().any(|entry| entry.value == "project-model"
                && entry.label == "Project Model"
                && entry.picker_description() == "Project scoped model | Qwen settings (openai)"));
        });
    }

    #[test]
    fn qwen_resolved_models_reuse_cached_static_strings() {
        with_temp_qwen_env(|temp_home, temp_project| {
            let user_qwen_dir = temp_home.path().join(".qwen");
            fs::create_dir_all(&user_qwen_dir).unwrap();

            fs::write(
                user_qwen_dir.join("settings.json"),
                r#"{
                  "modelProviders": {
                    "openai": [
                      {
                        "id": "cached-model",
                        "name": "Cached Model",
                        "description": "Cached model description"
                      }
                    ]
                  },
                  "model": { "name": "cached-model" }
                }"#,
            )
            .unwrap();

            let working_dir = temp_project.path().to_str().unwrap();
            let first = super::resolve_qwen_model_catalog(Some(working_dir));
            let second = super::resolve_qwen_model_catalog(Some(working_dir));
            assert_eq!(first.entries.len(), 1);
            assert_eq!(second.entries.len(), 1);
            assert!(std::ptr::eq(
                first.entries[0].value,
                second.entries[0].value
            ));
            assert!(std::ptr::eq(
                first.entries[0].label,
                second.entries[0].label
            ));
            assert!(std::ptr::eq(
                first.default_model.unwrap(),
                second.default_model.unwrap()
            ));
        });
    }
}
