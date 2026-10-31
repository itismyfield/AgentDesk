use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{AgentChannel, AgentDef, BotConfig, Config};
use crate::runtime_layout;

use crate::services::discord::formatting::normalize_allowed_tools;
use crate::services::discord::internal_api;
use crate::services::discord::runtime_store::atomic_write;
use crate::services::discord::settings::discord_token_hash;

mod bot_settings;
mod db_agents;
mod legacy_bot;
use bot_settings::*;
use db_agents::*;
use legacy_bot::*;

const CONFIG_AUDIT_KV_KEY: &str = "config_audit_report";

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
struct DbAgentDrift {
    storage: ConfigAuditDbSummary,
    warnings: Vec<String>,
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

pub(crate) fn load_runtime_config(root: &Path) -> Result<LoadedRuntimeConfig, String> {
    let path = crate::config::config_path::config_path_in(root);
    let loaded = crate::config::config_path::load_config_file(&path)
        .map_err(|err| format!("Failed to load config '{}': {err:#}", path.display()))?;
    let existed = loaded.is_some();
    let mut config = loaded.unwrap_or_default();

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

    let require = crate::utils::env_flag("AGENTDESK_VOICE_REQUIRE_ALIASES", false);

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

pub(crate) fn audit_and_reconcile_config_only(
    root: &Path,
    mut config: Config,
    yaml_path: PathBuf,
    yaml_present: bool,
    dry_run: bool,
) -> Result<AuditRunOutcome, String> {
    let bot_settings_path = runtime_layout::config_dir(root).join("bot_settings.json");
    let mut report = ConfigAuditReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        dry_run,
        sources: ConfigAuditSources {
            yaml_path: yaml_path.display().to_string(),
            yaml_present,
            bot_settings_path: Some(bot_settings_path.display().to_string()),
            bot_settings_present: bot_settings_path.is_file(),
        },
        ..ConfigAuditReport::default()
    };

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

impl DbAgentDrift {
    fn without_warnings(self) -> Self {
        Self {
            warnings: Vec::new(),
            ..self
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
    let path = crate::config::config_path::config_path_in(root);
    crate::config::save_to_path(&path, config)
        .map_err(|err| format!("Failed to write config '{}': {err}", path.display()))
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
        let discord_channel_cc = agent.channels.get("claude").and_then(AgentChannel::target);
        let discord_channel_cdx = agent.channels.get("codex").and_then(AgentChannel::target);
        let provider_primary =
            match crate::services::provider::ProviderKind::from_str(&agent.provider) {
                Some(crate::services::provider::ProviderKind::Claude)
                | Some(crate::services::provider::ProviderKind::Codex)
                | None => None,
                Some(_) => agent
                    .channels
                    .get(&agent.provider)
                    .and_then(AgentChannel::target),
            };

        Self {
            id: agent.id.clone(),
            provider: normalize_provider(Some(agent.provider.as_str()))
                .unwrap_or_else(|| "claude".to_string()),
            discord_channel_id: provider_primary.or_else(|| discord_channel_cc.clone()),
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

fn dry_run_action_prefix(dry_run: bool) -> &'static str {
    if dry_run { "would migrate" } else { "migrated" }
}

#[cfg(test)]
mod db_agent_drift_tests {
    use super::*;
    use crate::config::AgentChannels;

    fn agent(id: &str, provider: &str, cc: Option<&str>, cdx: Option<&str>) -> ComparableAgent {
        ComparableAgent {
            id: id.to_string(),
            provider: provider.to_string(),
            discord_channel_id: cc.map(str::to_string),
            discord_channel_alt: cdx.map(str::to_string),
            discord_channel_cc: cc.map(str::to_string),
            discord_channel_cdx: cdx.map(str::to_string),
        }
    }

    fn agent_with_primary(
        id: &str,
        provider: &str,
        primary: Option<&str>,
        cc: Option<&str>,
        cdx: Option<&str>,
    ) -> ComparableAgent {
        ComparableAgent {
            id: id.to_string(),
            provider: provider.to_string(),
            discord_channel_id: primary.map(str::to_string),
            discord_channel_alt: cdx.map(str::to_string),
            discord_channel_cc: cc.map(str::to_string),
            discord_channel_cdx: cdx.map(str::to_string),
        }
    }
    fn yaml_agent_with_provider_channel(provider: &str, provider_channel: &str) -> AgentDef {
        AgentDef {
            id: provider.to_string(),
            name: provider.to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: Default::default(),
            provider: provider.to_string(),
            channels: {
                let mut channels = AgentChannels::new()
                    .with("claude", AgentChannel::from("claude-channel"))
                    .with("codex", AgentChannel::from("codex-channel"));
                if !matches!(provider, "claude" | "codex") {
                    channels.insert(provider, AgentChannel::from(provider_channel));
                }
                channels
            },
            peer_keywords: Vec::new(),
            peer_directory: true,
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
            preferred_intake_node_labels: None,
            memory_group: None,
        }
    }

    #[test]
    fn db_agent_drift_reports_mismatch_before_sync() {
        let yaml_agents = BTreeMap::from([(
            "chef-goat".to_string(),
            agent("chef-goat", "claude", Some("1503373026060800081"), None),
        )]);
        let db_agents = BTreeMap::from([(
            "chef-goat".to_string(),
            agent("chef-goat", "claude", Some("old-channel"), None),
        )]);

        let drift = compute_db_agent_drift(&yaml_agents, &db_agents, true);

        assert_eq!(
            drift.storage.mismatched_agents,
            vec!["chef-goat".to_string()]
        );
        assert_eq!(drift.warnings.len(), 1);
        assert!(
            drift.warnings[0].contains("discord_channel_id, discord_channel_cc"),
            "warning should name the differing channel fields: {:?}",
            drift.warnings
        );
    }

    #[test]
    fn post_sync_clean_drift_does_not_persist_pre_sync_warning() {
        let yaml_agents = BTreeMap::from([(
            "chef-goat".to_string(),
            agent("chef-goat", "claude", Some("1503373026060800081"), None),
        )]);
        let pre_sync_agents = BTreeMap::from([(
            "chef-goat".to_string(),
            agent("chef-goat", "claude", Some("old-channel"), None),
        )]);
        let post_sync_agents = yaml_agents.clone();
        let pre_sync_drift = compute_db_agent_drift(&yaml_agents, &pre_sync_agents, true);
        assert!(
            !pre_sync_drift.warnings.is_empty(),
            "precondition: initial DB state must have drift"
        );

        let post_sync_drift = compute_db_agent_drift(&yaml_agents, &post_sync_agents, true);
        let mut report = ConfigAuditReport::default();
        report.storage.synced_agents = Some(1);
        apply_db_agent_drift(&mut report, post_sync_drift);
        report.finalize();

        assert_eq!(report.status, "ok");
        assert!(report.warnings.is_empty());
        assert!(report.storage.mismatched_agents.is_empty());
        assert_eq!(report.storage.synced_agents, Some(1));
    }

    #[test]
    fn provider_specific_primary_channel_matches_post_sync_db_shape() {
        let yaml_agent = yaml_agent_with_provider_channel("qwen", "qwen-channel");
        let yaml_agents =
            BTreeMap::from([("qwen".to_string(), ComparableAgent::from_yaml(&yaml_agent))]);
        let post_sync_agents = BTreeMap::from([(
            "qwen".to_string(),
            agent_with_primary(
                "qwen",
                "qwen",
                Some("qwen-channel"),
                Some("claude-channel"),
                Some("codex-channel"),
            ),
        )]);

        let drift = compute_db_agent_drift(&yaml_agents, &post_sync_agents, true);

        assert!(
            drift.storage.mismatched_agents.is_empty(),
            "provider-specific channel should not remain drift after DB sync: {:?}",
            drift
        );
        assert!(drift.warnings.is_empty());
    }

    #[test]
    fn post_sync_verification_failure_keeps_storage_without_stale_advisory() {
        let pre_sync_drift = DbAgentDrift {
            storage: ConfigAuditDbSummary {
                mismatched_agents: vec!["chef-goat".to_string()],
                ..ConfigAuditDbSummary::default()
            },
            warnings: vec![
                "DB agent 'chef-goat' differs from agentdesk.yaml on discord_channel_id, discord_channel_cc; agentdesk.yaml wins"
                    .to_string(),
            ],
        };
        let mut report = ConfigAuditReport::default();
        report.storage.synced_agents = Some(1);
        apply_db_agent_drift(&mut report, pre_sync_drift.without_warnings());
        report
            .warnings
            .push("Failed to verify PostgreSQL agents after config sync: boom".to_string());
        report.finalize();

        assert_eq!(report.status, "warn");
        assert_eq!(
            report.storage.mismatched_agents,
            vec!["chef-goat".to_string()]
        );
        assert_eq!(report.storage.synced_agents, Some(1));
        assert_eq!(
            report.warnings,
            vec!["Failed to verify PostgreSQL agents after config sync: boom".to_string()]
        );
    }
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
        channels.insert(
            "codex",
            AgentChannel::Detailed(AgentChannelConfig {
                id: Some("9999".to_string()),
                name: channel_name.map(str::to_string),
                aliases: Vec::new(),
                prompt_file: None,
                workspace: None,
                provider: Some("codex".to_string()),
                tui_hosting: None,
                runtime: None,
                model: None,
                reasoning_effort: None,
                peer_agents: None,
                thread_inherit: true,
                quality_feedback_injection: None,
                dispatch_profile: None,
                isolate_override: None,
                cache_ttl_minutes: None,
            }),
        );
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
            peer_keywords: Vec::new(),
            peer_directory: true,
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
            preferred_intake_node_labels: None,
            memory_group: None,
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

#[cfg(test)]
mod config_secret_write_tests {
    use super::*;
    use crate::config::disk_write::test_support::*;

    #[test]
    fn runtime_config_write_keeps_disk_secrets() {
        let root = tempfile::tempdir().unwrap();
        let path = runtime_layout::config_file_path(root.path());
        write_secret_config(&path, "");
        let mut loaded = load_runtime_config(root.path()).unwrap();
        loaded.config.discord.owner_id = Some(123_456_789_012_345_678);

        write_runtime_config(root.path(), &loaded.config).unwrap();

        assert_secrets_on_disk(&path);
        let saved = crate::config::load_from_path(&path).unwrap();
        assert_eq!(saved.discord.owner_id, Some(123_456_789_012_345_678));
    }
}

#[cfg(test)]
mod role_map_retirement_tests {
    use super::*;

    #[test]
    fn audit_report_omits_retired_role_map_sources() {
        let report = serde_json::to_value(ConfigAuditReport::default()).unwrap();
        assert!(report["sources"].get("role_map_path").is_none());
        assert!(report["sources"].get("role_map_present").is_none());
        assert!(report["sources"].get("bot_settings_path").is_some());
    }
}
