use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{AgentChannel, AgentDef, BotConfig, Config};
use crate::db::Db;
use crate::runtime_layout;

use crate::services::discord::formatting::normalize_allowed_tools;
use crate::services::discord::internal_api;
use crate::services::discord::runtime_store::atomic_write;
use crate::services::discord::settings::discord_token_hash;

const CONFIG_AUDIT_KV_KEY: &str = "config_audit_report";

#[derive(Debug, Clone, Default)]
pub(crate) struct LegacySourceScan {
    role_map_json: Option<Value>,
    role_map_path: Option<PathBuf>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedRuntimeConfig {
    pub config: Config,
    pub path: PathBuf,
    pub existed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct AuditRunOutcome {
    pub config: Config,
    pub report: ConfigAuditReport,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct ConfigAuditSources {
    pub yaml_path: String,
    pub yaml_present: bool,
    pub role_map_path: Option<String>,
    pub role_map_present: bool,
    pub bot_settings_path: Option<String>,
    pub bot_settings_present: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct ConfigAuditDbSummary {
    pub missing_agents: Vec<String>,
    pub extra_agents: Vec<String>,
    pub mismatched_agents: Vec<String>,
    pub synced_agents: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct ConfigAuditReport {
    pub generated_at: String,
    pub status: String,
    pub dry_run: bool,
    pub warnings_count: usize,
    pub warnings: Vec<String>,
    pub actions: Vec<String>,
    pub sources: ConfigAuditSources,
    #[serde(rename = "db")]
    pub storage: ConfigAuditDbSummary,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ComparableAgent {
    id: String,
    provider: String,
    discord_channel_id: Option<String>,
    discord_channel_alt: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LegacyBotEntry {
    hash_key: String,
    token: Option<String>,
    provider: Option<String>,
    agent: Option<String>,
    allowed_tools: Option<Vec<String>>,
    allowed_channel_ids: Option<Vec<u64>>,
    owner_user_id: Option<u64>,
    allowed_user_ids: Option<Vec<u64>>,
    allow_all_users: Option<bool>,
    allowed_bot_ids: Option<Vec<u64>>,
    channel_model_overrides: BTreeMap<String, String>,
    channel_fast_modes: BTreeMap<String, bool>,
    channel_fast_mode_reset_pending: BTreeSet<String>,
    channel_codex_goals: BTreeMap<String, bool>,
    channel_codex_goals_reset_pending: BTreeSet<String>,
}

impl ConfigAuditReport {
    fn finalize(&mut self) {
        self.warnings.sort();
        self.warnings.dedup();
        self.actions.sort();
        self.actions.dedup();
        self.warnings_count = self.warnings.len();
        self.status = if self.warnings.is_empty() {
            "ok".to_string()
        } else {
            "warn".to_string()
        };
    }
}

impl LegacyBotEntry {
    fn has_migratable_fields(&self) -> bool {
        self.provider.is_some()
            || self.agent.is_some()
            || self.allowed_tools.is_some()
            || self.allowed_channel_ids.is_some()
            || self.owner_user_id.is_some()
            || self.allowed_user_ids.is_some()
            || self.allow_all_users.is_some()
            || self.allowed_bot_ids.is_some()
    }
}

pub(crate) fn scan_legacy_sources(root: &Path) -> LegacySourceScan {
    let mut scan = LegacySourceScan::default();
    let role_map_path = runtime_layout::role_map_path(root);
    if !role_map_path.is_file() {
        return scan;
    }

    scan.role_map_path = Some(role_map_path.clone());
    match fs::read_to_string(&role_map_path) {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(json) => scan.role_map_json = Some(json),
            Err(err) => scan.warnings.push(format!(
                "Failed to parse legacy role map '{}': {err}",
                role_map_path.display()
            )),
        },
        Err(err) => scan.warnings.push(format!(
            "Failed to read legacy role map '{}': {err}",
            role_map_path.display()
        )),
    }
    scan
}

pub(crate) fn load_runtime_config(root: &Path) -> Result<LoadedRuntimeConfig, String> {
    let canonical = runtime_layout::config_file_path(root);
    let legacy = runtime_layout::legacy_config_file_path(root);
    let path = if canonical.is_file() || !legacy.is_file() {
        canonical
    } else {
        legacy
    };

    let existed = path.is_file();
    let mut config = if existed {
        crate::config::load_from_path(&path)
            .map_err(|err| format!("Failed to load config '{}': {err}", path.display()))?
    } else {
        Config::default()
    };

    precheck_voice_alias_collisions(&mut config)?;

    Ok(LoadedRuntimeConfig {
        config,
        path,
        existed,
    })
}

/// Pre-check voice alias collisions at yaml-load time so we never let the
/// boot path hit `sync_agents_from_config_pg → validate_agent_alias_collisions`
/// only to crash dcserver into a launchd restart loop (#2053).
///
/// Behavior:
/// - If no `voice` section is configured (i.e. `voice.is_default()`), this is
///   a no-op — voice alias checks only matter once voice is enabled.
/// - On collision in normal mode: downgrade `config.voice.enabled = false`,
///   emit a `tracing::warn!`, and return `Ok(())` so dcserver keeps booting.
/// - On collision when `AGENTDESK_VOICE_REQUIRE_ALIASES=1`: return an `Err`
///   describing the collision so callers can choose to fail fast.
pub(crate) fn precheck_voice_alias_collisions(config: &mut Config) -> Result<(), String> {
    if config.voice.is_default() {
        return Ok(());
    }

    let collision = match crate::voice::commands::validate_agent_alias_collisions(&config.agents) {
        Ok(()) => return Ok(()),
        Err(collision) => collision,
    };

    let require = std::env::var("AGENTDESK_VOICE_REQUIRE_ALIASES")
        .ok()
        .map(|raw| {
            matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);

    if require {
        return Err(format!(
            "voice alias collision detected (AGENTDESK_VOICE_REQUIRE_ALIASES=1): {collision}"
        ));
    }

    let was_enabled = config.voice.enabled;
    config.voice.enabled = false;
    tracing::warn!(
        collision = %collision,
        previously_enabled = was_enabled,
        "[config-audit] voice alias collision detected; downgrading voice.enabled=false to keep dcserver bootable. \
         Set AGENTDESK_VOICE_REQUIRE_ALIASES=1 to make this a hard error."
    );
    Ok(())
}

pub(crate) fn audit_and_reconcile(
    root: &Path,
    config: Config,
    yaml_path: PathBuf,
    yaml_present: bool,
    _legacy_db: &Db,
    legacy_scan: &LegacySourceScan,
    dry_run: bool,
) -> Result<AuditRunOutcome, String> {
    audit_and_reconcile_config_only(root, config, yaml_path, yaml_present, legacy_scan, dry_run)
}

pub(crate) fn audit_and_reconcile_config_only(
    root: &Path,
    mut config: Config,
    yaml_path: PathBuf,
    yaml_present: bool,
    legacy_scan: &LegacySourceScan,
    dry_run: bool,
) -> Result<AuditRunOutcome, String> {
    let bot_settings_path = runtime_layout::config_dir(root).join("bot_settings.json");
    let mut report = ConfigAuditReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        dry_run,
        sources: ConfigAuditSources {
            yaml_path: yaml_path.display().to_string(),
            yaml_present,
            role_map_path: legacy_scan
                .role_map_path
                .as_ref()
                .map(|path| path.display().to_string()),
            role_map_present: legacy_scan.role_map_json.is_some(),
            bot_settings_path: Some(bot_settings_path.display().to_string()),
            bot_settings_present: bot_settings_path.is_file(),
        },
        ..ConfigAuditReport::default()
    };

    report.warnings.extend(legacy_scan.warnings.iter().cloned());

    audit_role_map(&config, legacy_scan, dry_run, &mut report);

    let config_changed = audit_bot_settings(root, &mut config, dry_run, &mut report);
    if config_changed && !dry_run {
        write_runtime_config(root, &config)?;
    }

    audit_db_agents(&config, yaml_present, dry_run, &mut report)?;
    report.finalize();

    for warning in &report.warnings {
        tracing::warn!("[config-audit] {warning}");
    }
    for action in &report.actions {
        tracing::info!("[config-audit] {action}");
    }

    if !dry_run {
        persist_report(&config, &report);
    }

    Ok(AuditRunOutcome { config, report })
}

fn direct_api_context_unavailable(error: &str) -> bool {
    error.contains("direct runtime API context is unavailable")
        || error.contains("direct runtime pg context is unavailable")
}

fn load_persisted_report_pg(pg_pool: Option<&sqlx::PgPool>) -> Option<String> {
    let pg_pool = pg_pool?;
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        |pool| async move {
            sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
                .bind(CONFIG_AUDIT_KV_KEY)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load config audit report from pg: {error}"))
        },
        |message| message,
    )
    .ok()
    .flatten()
}

fn persist_report_pg(config: &Config, rendered: &str) -> bool {
    let config = config.clone();
    let rendered = rendered.to_string();
    crate::utils::async_bridge::block_on_result(
        async move {
            let Some(pool) = crate::db::postgres::connect(&config).await? else {
                return Ok(false);
            };
            sqlx::query(
                "INSERT INTO kv_meta (key, value)
                 VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE
                     SET value = EXCLUDED.value",
            )
            .bind(CONFIG_AUDIT_KV_KEY)
            .bind(&rendered)
            .execute(&pool)
            .await
            .map_err(|error| format!("persist config audit report to pg: {error}"))?;
            Ok(true)
        },
        |message| message,
    )
    .unwrap_or(false)
}

pub(crate) fn load_persisted_report(
    _legacy_db: &Db,
    pg_pool: Option<&sqlx::PgPool>,
) -> Option<ConfigAuditReport> {
    match internal_api::get_kv_value(CONFIG_AUDIT_KV_KEY) {
        Ok(Some(raw)) => return serde_json::from_str(&raw).ok(),
        Ok(None) => return None,
        Err(error) if !direct_api_context_unavailable(&error) => return None,
        Err(_) => {}
    }

    let raw = load_persisted_report_pg(pg_pool)?;
    serde_json::from_str(&raw).ok()
}

fn persist_report(config: &Config, report: &ConfigAuditReport) {
    let Ok(rendered) = serde_json::to_string(report) else {
        return;
    };

    match internal_api::set_kv_value(CONFIG_AUDIT_KV_KEY, &rendered) {
        Ok(()) => return,
        Err(error) if !direct_api_context_unavailable(&error) => return,
        Err(_) => {}
    }

    if !persist_report_pg(config, &rendered) {
        tracing::warn!(
            "[config-audit] PostgreSQL unavailable; config audit report was not persisted"
        );
    }
}

fn audit_role_map(
    config: &Config,
    legacy_scan: &LegacySourceScan,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) {
    let Some(role_map_json) = legacy_scan.role_map_json.as_ref() else {
        return;
    };

    let mut preview = Config::default();
    let changed = runtime_layout::preview_role_map_merge(&mut preview, role_map_json);

    if dry_run {
        if let Some(path) = legacy_scan.role_map_path.as_ref() {
            let verb = if changed {
                "would migrate"
            } else {
                "would retire"
            };
            report.actions.push(format!(
                "{} legacy role map '{}' with agentdesk.yaml as the winning source-of-truth",
                verb,
                path.display()
            ));
        }
    }

    for legacy_agent in &preview.agents {
        let Some(yaml_agent) = config
            .agents
            .iter()
            .find(|agent| agent.id == legacy_agent.id)
        else {
            report.warnings.push(format!(
                "Legacy role_map defines agent '{}' but agentdesk.yaml does not contain it after startup migration",
                legacy_agent.id
            ));
            continue;
        };

        let differing_fields = compare_agents(yaml_agent, legacy_agent);
        if differing_fields.is_empty() {
            continue;
        }

        report.warnings.push(format!(
            "Legacy role_map defines agent '{}' differently on {}; agentdesk.yaml wins",
            legacy_agent.id,
            differing_fields.join(", ")
        ));
    }
}

fn audit_bot_settings(
    root: &Path,
    config: &mut Config,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let path = runtime_layout::config_dir(root).join("bot_settings.json");
    if !path.is_file() {
        return false;
    }

    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            report.warnings.push(format!(
                "Failed to read legacy bot settings '{}': {err}",
                path.display()
            ));
            return false;
        }
    };

    let json = match serde_json::from_str::<Value>(&content) {
        Ok(json) => json,
        Err(err) => {
            report.warnings.push(format!(
                "Failed to parse legacy bot settings '{}': {err}",
                path.display()
            ));
            return false;
        }
    };

    let Some(obj) = json.as_object() else {
        report.warnings.push(format!(
            "Legacy bot settings '{}' is not a JSON object",
            path.display()
        ));
        return false;
    };

    let mut changed = false;
    let mut runtime_only_entries = Map::new();
    let mut owner_candidates = BTreeSet::new();
    let mut saw_migratable_data = false;
    let mut saw_runtime_rewrite = false;

    for (entry_key, entry_value) in obj {
        let legacy = parse_legacy_bot_entry(entry_key, entry_value);
        if !legacy.channel_model_overrides.is_empty()
            || !legacy.channel_fast_modes.is_empty()
            || !legacy.channel_fast_mode_reset_pending.is_empty()
            || !legacy.channel_codex_goals.is_empty()
            || !legacy.channel_codex_goals_reset_pending.is_empty()
        {
            let mut runtime_entry = Map::new();
            if !legacy.channel_model_overrides.is_empty() {
                runtime_entry.insert(
                    "channel_model_overrides".to_string(),
                    serde_json::json!(legacy.channel_model_overrides),
                );
            }
            if !legacy.channel_fast_modes.is_empty() {
                runtime_entry.insert(
                    "channel_fast_modes".to_string(),
                    serde_json::json!(legacy.channel_fast_modes),
                );
            }
            if !legacy.channel_fast_mode_reset_pending.is_empty() {
                runtime_entry.insert(
                    "channel_fast_mode_reset_pending".to_string(),
                    serde_json::json!(legacy.channel_fast_mode_reset_pending),
                );
            }
            if !legacy.channel_codex_goals.is_empty() {
                runtime_entry.insert(
                    "channel_codex_goals".to_string(),
                    serde_json::json!(legacy.channel_codex_goals),
                );
            }
            if !legacy.channel_codex_goals_reset_pending.is_empty() {
                runtime_entry.insert(
                    "channel_codex_goals_reset_pending".to_string(),
                    serde_json::json!(legacy.channel_codex_goals_reset_pending),
                );
            }
            runtime_only_entries.insert(legacy.hash_key.clone(), Value::Object(runtime_entry));
        }

        if !legacy.has_migratable_fields() {
            continue;
        }

        saw_migratable_data = true;

        let Some(token) = legacy.token.as_deref() else {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' has auth data but no token; skipping migration",
                legacy.hash_key
            ));
            continue;
        };

        let Some(bot_name) = resolved_config_bot_name(config, token) else {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' does not match any bot token in agentdesk.yaml; skipping migration",
                legacy.hash_key
            ));
            continue;
        };

        if let Some(owner_user_id) = legacy.owner_user_id {
            owner_candidates.insert(owner_user_id);
        }

        let Some(bot) = config.discord.bots.get_mut(&bot_name) else {
            continue;
        };

        changed |= migrate_provider(&legacy, bot, dry_run, report);
        changed |= migrate_agent(&legacy, bot, dry_run, report);
        changed |= migrate_u64_list(
            &legacy.hash_key,
            "allowed_channel_ids",
            &legacy.allowed_channel_ids,
            &mut bot.auth.allowed_channel_ids,
            dry_run,
            report,
        );
        changed |= migrate_u64_list(
            &legacy.hash_key,
            "allowed_user_ids",
            &legacy.allowed_user_ids,
            &mut bot.auth.allowed_user_ids,
            dry_run,
            report,
        );
        changed |= migrate_tool_list(
            &legacy.hash_key,
            &legacy.allowed_tools,
            &mut bot.auth.allowed_tools,
            dry_run,
            report,
        );
        changed |= migrate_bool_field(
            &legacy.hash_key,
            "allow_all_users",
            legacy.allow_all_users,
            &mut bot.auth.allow_all_users,
            dry_run,
            report,
        );
        changed |= migrate_u64_list(
            &legacy.hash_key,
            "allowed_bot_ids",
            &legacy.allowed_bot_ids,
            &mut bot.auth.allowed_bot_ids,
            dry_run,
            report,
        );
    }

    if config.discord.owner_id.is_none() {
        match owner_candidates.len() {
            0 => {}
            1 => {
                let owner_user_id = *owner_candidates.iter().next().unwrap();
                report.actions.push(format!(
                    "{} discord.owner_id={} from legacy bot_settings.json",
                    dry_run_action_prefix(dry_run),
                    owner_user_id
                ));
                if !dry_run {
                    config.discord.owner_id = Some(owner_user_id);
                }
                changed = true;
            }
            _ => report.warnings.push(format!(
                "Legacy bot_settings.json contains multiple owner_user_id candidates ({}) while agentdesk.yaml is unset; refusing automatic migration",
                owner_candidates
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    } else if let Some(owner_user_id) = config.discord.owner_id {
        for candidate in owner_candidates {
            if candidate != owner_user_id {
                report.warnings.push(format!(
                    "Legacy bot_settings.json owner_user_id={} conflicts with agentdesk.yaml owner_id={}; agentdesk.yaml wins",
                    candidate, owner_user_id
                ));
            }
        }
    }

    let desired_runtime_value = Value::Object(runtime_only_entries.clone());
    let desired_runtime_rendered =
        serde_json::to_string_pretty(&desired_runtime_value).unwrap_or_else(|_| "{}".to_string());
    if Value::Object(obj.clone()) != desired_runtime_value {
        saw_runtime_rewrite = true;
    }

    if runtime_only_entries.is_empty() {
        if saw_migratable_data || saw_runtime_rewrite {
            let migrated = path.with_extension("json.migrated");
            report.actions.push(format!(
                "{} legacy bot_settings '{}' after migrating runtime auth into agentdesk.yaml",
                if dry_run { "would retire" } else { "retired" },
                path.display()
            ));
            if !dry_run {
                match fs::rename(&path, &migrated) {
                    Ok(()) => {}
                    Err(err) => report.warnings.push(format!(
                        "Failed to retire legacy bot settings '{}' → '{}': {err}",
                        path.display(),
                        migrated.display()
                    )),
                }
            }
        }
    } else if saw_migratable_data || saw_runtime_rewrite {
        report.actions.push(format!(
            "{} '{}' to runtime-only channel settings entries",
            if dry_run { "would rewrite" } else { "rewrote" },
            path.display()
        ));
        if !dry_run {
            if let Err(err) = atomic_write(&path, &desired_runtime_rendered) {
                report.warnings.push(format!(
                    "Failed to rewrite legacy bot settings '{}': {err}",
                    path.display()
                ));
            }
        }
    }

    changed
}

fn audit_db_agents(
    config: &Config,
    yaml_present: bool,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> Result<(), String> {
    let db_agents = match load_db_agents_pg(config) {
        Ok(Some(agents)) => agents,
        Ok(None) => {
            report
                .actions
                .push("skipped DB agent audit because PostgreSQL is not enabled".to_string());
            return Ok(());
        }
        Err(error) => {
            report.warnings.push(format!(
                "Failed to load agents from PostgreSQL during config audit: {error}; legacy fallback disabled"
            ));
            return Ok(());
        }
    };
    let yaml_agents = config
        .agents
        .iter()
        .map(|agent| (agent.id.clone(), ComparableAgent::from_yaml(agent)))
        .collect::<BTreeMap<_, _>>();

    for (agent_id, yaml_agent) in &yaml_agents {
        match db_agents.get(agent_id) {
            None => {
                report.storage.missing_agents.push(agent_id.clone());
                if yaml_present {
                    report.warnings.push(format!(
                        "DB is missing agent '{}' from agentdesk.yaml; agentdesk.yaml will restore it",
                        agent_id
                    ));
                }
            }
            Some(db_agent) => {
                let differing_fields = compare_comparable_agents(yaml_agent, db_agent);
                if !differing_fields.is_empty() {
                    report.storage.mismatched_agents.push(agent_id.clone());
                    report.warnings.push(format!(
                        "DB agent '{}' differs from agentdesk.yaml on {}; agentdesk.yaml wins",
                        agent_id,
                        differing_fields.join(", ")
                    ));
                }
            }
        }
    }

    for agent_id in db_agents.keys() {
        if yaml_agents.contains_key(agent_id) {
            continue;
        }
        if yaml_present {
            report.storage.extra_agents.push(agent_id.clone());
            report.warnings.push(format!(
                "DB contains extra agent '{}' not present in agentdesk.yaml; agentdesk.yaml will remove it",
                agent_id
            ));
        }
    }

    report.storage.missing_agents.sort();
    report.storage.extra_agents.sort();
    report.storage.mismatched_agents.sort();

    if dry_run || !yaml_present {
        return Ok(());
    }

    match sync_db_agents_pg(config, &config.agents) {
        Ok(Some(count)) => {
            report.storage.synced_agents = Some(count);
            report.actions.push(format!(
                "synced {} agent definitions from agentdesk.yaml into the PostgreSQL agents table",
                count
            ));
            return Ok(());
        }
        Ok(None) => {}
        Err(error) => report.warnings.push(format!(
            "Failed to sync agents from agentdesk.yaml into PostgreSQL: {error}; legacy fallback disabled"
        )),
    }

    Ok(())
}

fn sync_db_agents_pg(config: &Config, agents: &[AgentDef]) -> Result<Option<usize>, String> {
    if !crate::db::postgres::database_enabled(config) {
        return Ok(None);
    }

    let config = config.clone();
    let agents = agents.to_vec();
    crate::utils::async_bridge::block_on_result(
        async move {
            let Some(pool) = crate::db::postgres::connect(&config).await? else {
                return Ok(None);
            };
            let count = crate::db::postgres::sync_agents_from_config_pg(&pool, &agents).await?;
            Ok(Some(count))
        },
        |message| message,
    )
}

fn load_db_agents_pg(config: &Config) -> Result<Option<BTreeMap<String, ComparableAgent>>, String> {
    if !crate::db::postgres::database_enabled(config) {
        return Ok(None);
    }

    let config = config.clone();
    crate::utils::async_bridge::block_on_result(
        async move {
            let Some(pool) = crate::db::postgres::connect(&config).await? else {
                return Ok(None);
            };
            let rows = crate::db::agents::load_all_agent_channel_bindings_pg(&pool)
                .await
                .map_err(|error| format!("Failed to query postgres agent audit rows: {error}"))?;

            let mut agents = BTreeMap::new();
            for (id, bindings) in rows {
                agents.insert(
                    id.clone(),
                    ComparableAgent {
                        id,
                        provider: normalize_provider(bindings.provider.as_deref())
                            .unwrap_or_else(|| "claude".to_string()),
                        discord_channel_id: normalize_optional_string(bindings.discord_channel_id),
                        discord_channel_alt: normalize_optional_string(
                            bindings.discord_channel_alt,
                        ),
                        discord_channel_cc: normalize_optional_string(bindings.discord_channel_cc),
                        discord_channel_cdx: normalize_optional_string(
                            bindings.discord_channel_cdx,
                        ),
                    },
                );
            }
            Ok(Some(agents))
        },
        |message| message,
    )
}

fn parse_legacy_bot_entry(entry_key: &str, entry_value: &Value) -> LegacyBotEntry {
    let token = entry_value
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let hash_key = token
        .as_deref()
        .map(discord_token_hash)
        .unwrap_or_else(|| entry_key.to_string());

    LegacyBotEntry {
        hash_key,
        token,
        provider: entry_value
            .get("provider")
            .and_then(Value::as_str)
            .and_then(|raw| normalize_provider(Some(raw))),
        agent: entry_value
            .get("agent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        allowed_tools: entry_value
            .get("allowed_tools")
            .map(parse_string_array)
            .map(|values| normalize_allowed_tools(values.unwrap_or_default())),
        allowed_channel_ids: entry_value
            .get("allowed_channel_ids")
            .map(parse_u64_array)
            .unwrap_or(None),
        owner_user_id: entry_value.get("owner_user_id").and_then(parse_u64_value),
        allowed_user_ids: entry_value
            .get("allowed_user_ids")
            .map(parse_u64_array)
            .unwrap_or(None),
        allow_all_users: entry_value.get("allow_all_users").and_then(Value::as_bool),
        allowed_bot_ids: entry_value
            .get("allowed_bot_ids")
            .map(parse_u64_array)
            .unwrap_or(None),
        channel_model_overrides: entry_value
            .get("channel_model_overrides")
            .and_then(Value::as_object)
            .map(|obj| {
                obj.iter()
                    .filter_map(|(channel_id, model)| {
                        model
                            .as_str()
                            .map(|model| (channel_id.clone(), model.to_string()))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default(),
        channel_fast_modes: entry_value
            .get("channel_fast_modes")
            .and_then(Value::as_object)
            .map(|obj| {
                obj.iter()
                    .filter_map(|(channel_id, enabled)| {
                        enabled
                            .as_bool()
                            .map(|enabled| (channel_id.clone(), enabled))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default(),
        channel_fast_mode_reset_pending: entry_value
            .get("channel_fast_mode_reset_pending")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default(),
        channel_codex_goals: entry_value
            .get("channel_codex_goals")
            .and_then(Value::as_object)
            .map(|obj| {
                obj.iter()
                    .filter_map(|(channel_id, enabled)| {
                        enabled
                            .as_bool()
                            .map(|enabled| (channel_id.clone(), enabled))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default(),
        channel_codex_goals_reset_pending: entry_value
            .get("channel_codex_goals_reset_pending")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default(),
    }
}

fn migrate_provider(
    legacy: &LegacyBotEntry,
    bot: &mut BotConfig,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_provider) = legacy.provider.as_ref() else {
        return false;
    };
    match bot
        .provider
        .as_deref()
        .and_then(|raw| normalize_provider(Some(raw)))
    {
        None => {
            report.actions.push(format!(
                "{} provider={} for bot '{}'",
                dry_run_action_prefix(dry_run),
                legacy_provider,
                legacy.hash_key
            ));
            if !dry_run {
                bot.provider = Some(legacy_provider.clone());
            }
            true
        }
        Some(existing) if existing == *legacy_provider => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' provider={} conflicts with agentdesk.yaml provider={}; agentdesk.yaml wins",
                legacy.hash_key, legacy_provider, existing
            ));
            false
        }
    }
}

fn migrate_agent(
    legacy: &LegacyBotEntry,
    bot: &mut BotConfig,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_agent) = legacy.agent.as_ref() else {
        return false;
    };
    match bot
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        None => {
            report.actions.push(format!(
                "{} agent={} for bot '{}'",
                dry_run_action_prefix(dry_run),
                legacy_agent,
                legacy.hash_key
            ));
            if !dry_run {
                bot.agent = Some(legacy_agent.clone());
            }
            true
        }
        Some(existing) if existing == legacy_agent => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' agent={} conflicts with agentdesk.yaml agent={}; agentdesk.yaml wins",
                legacy.hash_key, legacy_agent, existing
            ));
            false
        }
    }
}

fn migrate_tool_list(
    hash_key: &str,
    legacy_value: &Option<Vec<String>>,
    yaml_value: &mut Option<Vec<String>>,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_tools) = legacy_value.as_ref() else {
        return false;
    };
    match yaml_value {
        None => {
            report.actions.push(format!(
                "{} allowed_tools for bot '{}'",
                dry_run_action_prefix(dry_run),
                hash_key
            ));
            if !dry_run {
                *yaml_value = Some(legacy_tools.clone());
            }
            true
        }
        Some(existing) if *existing == *legacy_tools => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' allowed_tools={:?} conflicts with agentdesk.yaml allowed_tools={:?}; agentdesk.yaml wins",
                hash_key, legacy_tools, existing
            ));
            false
        }
    }
}

fn migrate_u64_list(
    hash_key: &str,
    field: &str,
    legacy_value: &Option<Vec<u64>>,
    yaml_value: &mut Option<Vec<u64>>,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_ids) = legacy_value.as_ref() else {
        return false;
    };
    match yaml_value {
        None => {
            report.actions.push(format!(
                "{} {} for bot '{}'",
                dry_run_action_prefix(dry_run),
                field,
                hash_key
            ));
            if !dry_run {
                *yaml_value = Some(legacy_ids.clone());
            }
            true
        }
        Some(existing) if normalized_u64s(existing) == normalized_u64s(legacy_ids) => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' {}={:?} conflicts with agentdesk.yaml {}={:?}; agentdesk.yaml wins",
                hash_key, field, legacy_ids, field, existing
            ));
            false
        }
    }
}

fn migrate_bool_field(
    hash_key: &str,
    field: &str,
    legacy_value: Option<bool>,
    yaml_value: &mut Option<bool>,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> bool {
    let Some(legacy_flag) = legacy_value else {
        return false;
    };
    match yaml_value {
        None => {
            report.actions.push(format!(
                "{} {}={} for bot '{}'",
                dry_run_action_prefix(dry_run),
                field,
                legacy_flag,
                hash_key
            ));
            if !dry_run {
                *yaml_value = Some(legacy_flag);
            }
            true
        }
        Some(existing) if *existing == legacy_flag => false,
        Some(existing) => {
            report.warnings.push(format!(
                "Legacy bot_settings entry '{}' {}={} conflicts with agentdesk.yaml {}={}; agentdesk.yaml wins",
                hash_key, field, legacy_flag, field, existing
            ));
            false
        }
    }
}

fn resolved_config_bot_name(config: &Config, token: &str) -> Option<String> {
    let mut bot_names = config.discord.bots.keys().cloned().collect::<Vec<_>>();
    bot_names.sort();
    bot_names.into_iter().find(|name| {
        config
            .discord
            .bots
            .get(name)
            .and_then(|bot| resolve_bot_token(name, bot))
            .as_deref()
            == Some(token)
    })
}

fn resolve_bot_token(bot_name: &str, bot: &BotConfig) -> Option<String> {
    bot.token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| crate::credential::read_bot_token(bot_name))
}

fn write_runtime_config(root: &Path, config: &Config) -> Result<(), String> {
    let canonical = runtime_layout::config_file_path(root);
    let legacy = runtime_layout::legacy_config_file_path(root);
    let path = if canonical.is_file() || !legacy.is_file() {
        canonical
    } else {
        legacy
    };
    crate::config::save_to_path(&path, config)
        .map_err(|err| format!("Failed to write config '{}': {err}", path.display()))
}

fn compare_agents(yaml_agent: &AgentDef, legacy_agent: &AgentDef) -> Vec<&'static str> {
    let left = ComparableAgent::from_yaml(yaml_agent);
    let right = ComparableAgent::from_yaml(legacy_agent);
    compare_comparable_agents(&left, &right)
}

fn compare_comparable_agents(left: &ComparableAgent, right: &ComparableAgent) -> Vec<&'static str> {
    let mut differing_fields = Vec::new();
    if normalize_provider(Some(left.provider.as_str()))
        != normalize_provider(Some(right.provider.as_str()))
    {
        differing_fields.push("provider");
    }
    if left.discord_channel_id != right.discord_channel_id {
        differing_fields.push("discord_channel_id");
    }
    if left.discord_channel_alt != right.discord_channel_alt {
        differing_fields.push("discord_channel_alt");
    }
    if left.discord_channel_cc != right.discord_channel_cc {
        differing_fields.push("discord_channel_cc");
    }
    if left.discord_channel_cdx != right.discord_channel_cdx {
        differing_fields.push("discord_channel_cdx");
    }
    differing_fields
}

impl ComparableAgent {
    fn from_yaml(agent: &AgentDef) -> Self {
        let discord_channel_cc = agent
            .channels
            .claude
            .as_ref()
            .and_then(AgentChannel::target);
        let discord_channel_cdx = agent.channels.codex.as_ref().and_then(AgentChannel::target);

        Self {
            id: agent.id.clone(),
            provider: normalize_provider(Some(agent.provider.as_str()))
                .unwrap_or_else(|| "claude".to_string()),
            discord_channel_id: discord_channel_cc.clone(),
            discord_channel_alt: discord_channel_cdx.clone(),
            discord_channel_cc,
            discord_channel_cdx,
        }
    }
}

fn normalize_provider(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(crate::services::provider::ProviderKind::from_str)
        .map(|provider| provider.as_str().to_string())
}

fn normalize_optional_string(raw: Option<String>) -> Option<String> {
    raw.map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_u64_value(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_str()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
    })
}

fn parse_u64_array(value: &Value) -> Option<Vec<u64>> {
    value.as_array().map(|values| {
        values
            .iter()
            .filter_map(parse_u64_value)
            .collect::<Vec<_>>()
    })
}

fn parse_string_array(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    })
}

fn normalized_u64s(values: &[u64]) -> Vec<u64> {
    let mut normalized = values.to_vec();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

fn dry_run_action_prefix(dry_run: bool) -> &'static str {
    if dry_run { "would migrate" } else { "migrated" }
}

#[cfg(test)]
mod voice_alias_precheck_tests {
    use super::*;
    use crate::config::{AgentChannel, AgentChannelConfig, AgentChannels, AgentDef};
    use crate::voice::VoiceConfig;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn make_agent(id: &str, name: &str, channel_name: Option<&str>) -> AgentDef {
        let mut channels = AgentChannels::default();
        channels.codex = Some(AgentChannel::Detailed(AgentChannelConfig {
            id: Some("9999".to_string()),
            name: channel_name.map(str::to_string),
            aliases: Vec::new(),
            prompt_file: None,
            workspace: None,
            provider: Some("codex".to_string()),
            model: None,
            reasoning_effort: None,
            peer_agents: None,
            quality_feedback_injection: None,
            dispatch_profile: None,
            cache_ttl_minutes: None,
        }));
        AgentDef {
            id: id.to_string(),
            name: name.to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: crate::config::AgentVoiceConfig::default(),
            provider: "codex".to_string(),
            channels,
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        }
    }

    fn enabled_voice_config() -> VoiceConfig {
        let mut voice = VoiceConfig::default();
        voice.enabled = true;
        voice
    }

    #[test]
    fn precheck_no_voice_section_is_noop_even_with_collision() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("AGENTDESK_VOICE_REQUIRE_ALIASES");
        }
        let mut config = Config::default();
        // Inject a collision pair but leave voice config at default (no voice section).
        config.agents = vec![
            make_agent("adk-cdx", "AgentDesk", None),
            make_agent("adkcdx-stub", "stub", Some("adk-cdx")),
        ];
        assert!(config.voice.is_default());
        precheck_voice_alias_collisions(&mut config).expect("noop when voice section is default");
        // voice.enabled must remain at its default (false) without being touched as warning.
        assert!(!config.voice.enabled);
    }

    #[test]
    fn precheck_collision_with_voice_enabled_downgrades_to_disabled() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("AGENTDESK_VOICE_REQUIRE_ALIASES");
        }
        let mut config = Config::default();
        config.voice = enabled_voice_config();
        // Two agents both expose alias `adk-cdx` (one via id, one via channel name).
        config.agents = vec![
            make_agent("adk-cdx", "AgentDesk", None),
            make_agent("adkcdx-stub", "stub", Some("adk-cdx")),
        ];

        let outcome = precheck_voice_alias_collisions(&mut config);
        assert!(
            outcome.is_ok(),
            "soft-fallback must not abort boot: {outcome:?}"
        );
        assert!(
            !config.voice.enabled,
            "voice.enabled must be downgraded to false on collision"
        );
    }

    #[test]
    fn precheck_collision_returns_err_when_require_env_set() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("AGENTDESK_VOICE_REQUIRE_ALIASES", "1");
        }
        let mut config = Config::default();
        config.voice = enabled_voice_config();
        config.agents = vec![
            make_agent("adk-cdx", "AgentDesk", None),
            make_agent("adkcdx-stub", "stub", Some("adk-cdx")),
        ];

        let outcome = precheck_voice_alias_collisions(&mut config);
        unsafe {
            std::env::remove_var("AGENTDESK_VOICE_REQUIRE_ALIASES");
        }
        let err = outcome.expect_err("require mode must hard-error on collision");
        assert!(
            err.contains("voice alias collision detected"),
            "error message must mention collision; got: {err}"
        );
    }

    #[test]
    fn precheck_no_collision_keeps_voice_enabled() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("AGENTDESK_VOICE_REQUIRE_ALIASES");
        }
        let mut config = Config::default();
        config.voice = enabled_voice_config();
        config.agents = vec![
            make_agent("agent-one", "AgentOne", None),
            make_agent("agent-two", "AgentTwo", None),
        ];

        precheck_voice_alias_collisions(&mut config).expect("no collision => Ok");
        assert!(
            config.voice.enabled,
            "voice.enabled must stay true when there is no collision"
        );
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn write_text(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn audit_migrates_legacy_bot_auth_and_rewrites_runtime_file() {
        let _lock = env_lock();
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", temp.path());
        }

        let root = temp.path();
        let yaml_path = runtime_layout::config_file_path(root);
        write_text(
            &yaml_path,
            r#"
server:
  port: 8791
discord:
  bots:
    command:
      token: "test-token"
"#,
        );
        write_text(
            &runtime_layout::config_dir(root).join("bot_settings.json"),
            &serde_json::to_string_pretty(&serde_json::json!({
                "legacy": {
                    "token": "test-token",
                    "provider": "codex",
                    "allowed_channel_ids": ["123"],
                    "allowed_user_ids": [456],
                    "allow_all_users": true,
                    "channel_model_overrides": {
                        "789": "gpt-5.4"
                    },
                    "channel_fast_modes": {
                        "789": true,
                        "790": false
                    },
                    "channel_fast_mode_reset_pending": [
                        "codex:789"
                    ]
                }
            }))
            .unwrap(),
        );

        let loaded = load_runtime_config(root).unwrap();
        let outcome = audit_and_reconcile_config_only(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &LegacySourceScan::default(),
            false,
        )
        .unwrap();

        let config = crate::config::load_from_path(&yaml_path).unwrap();
        let bot = config.discord.bots.get("command").unwrap();
        assert_eq!(bot.provider.as_deref(), Some("codex"));
        assert_eq!(bot.auth.allowed_channel_ids.as_deref(), Some(&[123][..]));
        assert_eq!(bot.auth.allowed_user_ids.as_deref(), Some(&[456][..]));
        assert_eq!(bot.auth.allow_all_users, Some(true));
        assert!(
            outcome
                .report
                .actions
                .iter()
                .any(|action| action.contains("rewrote"))
        );

        let hash_key = discord_token_hash("test-token");
        let rewritten: Value = serde_json::from_str(
            &fs::read_to_string(runtime_layout::config_dir(root).join("bot_settings.json"))
                .unwrap(),
        )
        .unwrap();
        assert!(rewritten[&hash_key]["provider"].is_null());
        assert_eq!(
            rewritten[&hash_key]["channel_model_overrides"]["789"],
            "gpt-5.4"
        );
        assert_eq!(rewritten[&hash_key]["channel_fast_modes"]["789"], true);
        assert_eq!(rewritten[&hash_key]["channel_fast_modes"]["790"], false);
        assert_eq!(
            rewritten[&hash_key]["channel_fast_mode_reset_pending"],
            serde_json::json!(["codex:789"])
        );

        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }

    #[test]
    fn audit_dry_run_reports_planned_role_map_migration_without_writing() {
        let _lock = env_lock();
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", temp.path());
        }

        let root = temp.path();
        write_text(
            &runtime_layout::role_map_path(root),
            &serde_json::to_string_pretty(&serde_json::json!({
                "byChannelId": {
                    "123": {
                        "roleId": "project-agentdesk",
                        "provider": "codex"
                    }
                }
            }))
            .unwrap(),
        );

        let loaded = load_runtime_config(root).unwrap();
        let scan = scan_legacy_sources(root);
        let outcome = audit_and_reconcile_config_only(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &scan,
            true,
        )
        .unwrap();

        assert!(runtime_layout::role_map_path(root).is_file());
        assert!(
            outcome
                .report
                .actions
                .iter()
                .any(|action| action.contains("would migrate legacy role map"))
        );

        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }
}
