use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{AgentChannel, AgentDef, BotConfig, Config};
use crate::db::Db;
use crate::runtime_layout;

use super::formatting::normalize_allowed_tools;
use super::runtime_store::atomic_write;
use super::settings::discord_token_hash;

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
    pub db: ConfigAuditDbSummary,
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
    let config = if existed {
        crate::config::load_from_path(&path)
            .map_err(|err| format!("Failed to load config '{}': {err}", path.display()))?
    } else {
        Config::default()
    };

    Ok(LoadedRuntimeConfig {
        config,
        path,
        existed,
    })
}

pub(crate) fn audit_and_reconcile(
    root: &Path,
    mut config: Config,
    yaml_path: PathBuf,
    yaml_present: bool,
    db: &Db,
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

    audit_db_agents(&config, yaml_present, db, dry_run, &mut report)?;
    report.finalize();

    for warning in &report.warnings {
        tracing::warn!("[config-audit] {warning}");
    }
    for action in &report.actions {
        tracing::info!("[config-audit] {action}");
    }

    if !dry_run {
        persist_report(db, &report);
    }

    Ok(AuditRunOutcome { config, report })
}

pub(crate) fn load_persisted_report(db: &Db) -> Option<ConfigAuditReport> {
    let conn = db.read_conn().ok()?;
    let raw: String = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = ?1",
            [CONFIG_AUDIT_KV_KEY],
            |row| row.get(0),
        )
        .ok()?;
    serde_json::from_str(&raw).ok()
}

fn persist_report(db: &Db, report: &ConfigAuditReport) {
    let Ok(rendered) = serde_json::to_string(report) else {
        return;
    };
    let Ok(conn) = db.lock() else {
        return;
    };
    let _ = conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![CONFIG_AUDIT_KV_KEY, rendered],
    );
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
        if !legacy.channel_model_overrides.is_empty() {
            runtime_only_entries.insert(
                legacy.hash_key.clone(),
                serde_json::json!({
                    "channel_model_overrides": legacy.channel_model_overrides,
                }),
            );
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
            "{} '{}' to runtime-only channel_model_overrides entries",
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
    db: &Db,
    dry_run: bool,
    report: &mut ConfigAuditReport,
) -> Result<(), String> {
    let db_agents = load_db_agents(db)?;
    let yaml_agents = config
        .agents
        .iter()
        .map(|agent| (agent.id.clone(), ComparableAgent::from_yaml(agent)))
        .collect::<BTreeMap<_, _>>();

    for (agent_id, yaml_agent) in &yaml_agents {
        match db_agents.get(agent_id) {
            None => {
                report.db.missing_agents.push(agent_id.clone());
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
                    report.db.mismatched_agents.push(agent_id.clone());
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
            report.db.extra_agents.push(agent_id.clone());
            report.warnings.push(format!(
                "DB contains extra agent '{}' not present in agentdesk.yaml; agentdesk.yaml will remove it",
                agent_id
            ));
        }
    }

    report.db.missing_agents.sort();
    report.db.extra_agents.sort();
    report.db.mismatched_agents.sort();

    if dry_run || !yaml_present {
        return Ok(());
    }

    match crate::db::agents::sync_agents_from_config(db, &config.agents) {
        Ok(result) => {
            report.db.synced_agents = Some(result.upserted);
            report.actions.push(format!(
                "synced {} agent definitions from agentdesk.yaml into the agents table",
                result.upserted
            ));
        }
        Err(err) => report.warnings.push(format!(
            "Failed to sync agents from agentdesk.yaml into DB: {err}"
        )),
    }

    Ok(())
}

fn load_db_agents(db: &Db) -> Result<BTreeMap<String, ComparableAgent>, String> {
    let conn = db
        .lock()
        .map_err(|err| format!("DB lock error while auditing config: {err}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
             FROM agents",
        )
        .map_err(|err| format!("Failed to prepare agent audit query: {err}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ComparableAgent {
                id: row.get::<_, String>(0)?,
                provider: normalize_provider(row.get::<_, Option<String>>(1)?.as_deref())
                    .unwrap_or_else(|| "claude".to_string()),
                discord_channel_id: normalize_optional_string(row.get(2)?),
                discord_channel_alt: normalize_optional_string(row.get(3)?),
                discord_channel_cc: normalize_optional_string(row.get(4)?),
                discord_channel_cdx: normalize_optional_string(row.get(5)?),
            })
        })
        .map_err(|err| format!("Failed to query agent audit rows: {err}"))?;

    let mut agents = BTreeMap::new();
    for row in rows {
        let agent = row.map_err(|err| format!("Failed to decode agent audit row: {err}"))?;
        agents.insert(agent.id.clone(), agent);
    }
    Ok(agents)
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
                    }
                }
            }))
            .unwrap(),
        );

        let loaded = load_runtime_config(root).unwrap();
        let db = crate::db::init(&loaded.config).unwrap();
        let outcome = audit_and_reconcile(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &db,
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
        let db = crate::db::init(&loaded.config).unwrap();
        let scan = scan_legacy_sources(root);
        let outcome = audit_and_reconcile(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &db,
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
