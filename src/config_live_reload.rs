//! Live config file hot-reload.
//!
//! Mirrors the policies watcher (`engine::loader::start_hot_reload`) for the
//! on-disk AgentDesk config file (`agentdesk.yaml`): a `notify` filesystem
//! watcher debounces change events, the candidate file is **pre-validated**
//! (parsed + runtime defaults applied via [`crate::config::load_from_path`]),
//! and only on success is the new config **atomically swapped** into a shared
//! process-global snapshot. A parse/validation failure keeps the currently
//! running config (logged), so a half-written or broken edit never takes the
//! server down.
//!
//! ## What is live vs. restart-required
//!
//! Subsystems read the live snapshot via [`current`] each cycle, so settings
//! they re-read per tick (e.g. routine tunables) take effect without a restart.
//! Infra fields (`server` bind/port/auth, `database`, `data`, `discord` client
//! and bot bindings, `providers` runtimes, `agents` per-channel runtime hosting
//! overrides, `mcp_servers` child processes, the `mcp` credential watcher, and
//! the `memory` backend) are bound into long-lived objects at boot and cannot be
//! swapped under a running process; a change to those is still stored in the
//! snapshot but reported by
//! [`restart_required_changes`] and logged as restart-required.
//!
//! The whole-`Config` value is NOT threaded through every reader — instead this
//! follows the existing global-runtime-setter precedent
//! (`set_runtime_cluster_config`) so consumers opt in by reading [`current`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;

use crate::config::{BotConfig, Config, DiscordBotAuthConfig, DiscordConfig};

/// Process-global live config snapshot. `None` until [`install`] runs at boot.
static LIVE: OnceLock<RwLock<Arc<Config>>> = OnceLock::new();

/// Debounce window for collapsing bursts of filesystem events (editors emit
/// several per save). Matches the policies watcher.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Outcome of a single reload attempt. Returned by [`reload_from_path`] so the
/// watcher, an on-demand trigger, and tests can all share the same core path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The candidate validated and was swapped in. `restart_required` lists the
    /// infra sections that changed and need a restart to take full effect.
    Applied { restart_required: Vec<&'static str> },
    /// The candidate failed to parse/validate; the running config was kept.
    Rejected { error: String },
}

/// Install the boot config as the initial live snapshot. Safe to call again to
/// overwrite (e.g. after a post-migration reload).
pub fn install(config: Config) {
    store(Arc::new(config));
}

/// The current live config snapshot, or `None` if [`install`] has not run yet
/// (callers should fall back to their boot-captured config in that case).
pub fn current() -> Option<Arc<Config>> {
    LIVE.get().map(|lock| {
        lock.read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    })
}

fn store(config: Arc<Config>) {
    let lock = LIVE.get_or_init(|| RwLock::new(config.clone()));
    *lock
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = config;
}

/// Two config values' section serializes differently. Used to detect changes
/// without requiring `PartialEq` on every config sub-struct; an un-serializable
/// section conservatively counts as changed.
fn section_changed<T: Serialize>(old: &T, new: &T) -> bool {
    match (serde_yaml::to_string(old), serde_yaml::to_string(new)) {
        (Ok(a), Ok(b)) => a != b,
        _ => true,
    }
}

#[derive(PartialEq, Eq)]
struct DiscordRestartFingerprint {
    bots: BTreeMap<String, DiscordBotRestartFingerprint>,
    guild_id: Option<String>,
    dm_default_agent: Option<String>,
    owner_id: Option<u64>,
}

#[derive(PartialEq, Eq)]
struct DiscordBotRestartFingerprint {
    token: Option<String>,
    description: Option<String>,
    provider: Option<String>,
    agent: Option<String>,
    auth: DiscordBotAuthConfig,
}

fn discord_restart_fingerprint(discord: &DiscordConfig) -> DiscordRestartFingerprint {
    DiscordRestartFingerprint {
        bots: discord
            .bots
            .iter()
            .map(|(name, bot)| (name.clone(), discord_bot_restart_fingerprint(bot)))
            .collect(),
        guild_id: discord.guild_id.clone(),
        dm_default_agent: discord.dm_default_agent.clone(),
        owner_id: discord.owner_id,
    }
}

fn discord_bot_restart_fingerprint(bot: &BotConfig) -> DiscordBotRestartFingerprint {
    DiscordBotRestartFingerprint {
        token: bot.token.clone(),
        description: bot.description.clone(),
        provider: bot.provider.clone(),
        agent: bot.agent.clone(),
        auth: bot.auth.clone(),
    }
}

fn discord_boot_config_changed(old: &DiscordConfig, new: &DiscordConfig) -> bool {
    discord_restart_fingerprint(old) != discord_restart_fingerprint(new)
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct AgentProviderRuntimeBindingKey {
    provider_id: String,
    target: String,
}

#[derive(Default, PartialEq, Eq)]
struct AgentProviderRuntimeBindingValue {
    tui_hosting: Option<bool>,
    runtime: Option<String>,
}

#[derive(Default, PartialEq, Eq)]
struct AgentLaunchFingerprint {
    provider_keys: BTreeSet<String>,
    agent_ids: BTreeSet<String>,
    channel_ids: BTreeSet<u64>,
    runtime_bindings: BTreeMap<AgentProviderRuntimeBindingKey, AgentProviderRuntimeBindingValue>,
}

fn agent_launch_fingerprint(config: &Config) -> AgentLaunchFingerprint {
    let mut bindings: BTreeMap<AgentProviderRuntimeBindingKey, AgentProviderRuntimeBindingValue> =
        BTreeMap::new();
    let mut provider_keys = BTreeSet::new();
    let mut agent_ids = BTreeSet::new();
    let mut channel_ids = BTreeSet::new();
    for agent in &config.agents {
        let agent_id = agent.id.trim().to_ascii_lowercase();
        if !agent_id.is_empty() {
            agent_ids.insert(agent_id);
        }
        for (channel_kind, channel) in agent.channels.iter() {
            let Some(channel) = channel else {
                continue;
            };
            let Some(target) = channel.target() else {
                continue;
            };
            let normalized_provider_key = channel_kind.trim().to_ascii_lowercase();
            if !normalized_provider_key.is_empty() {
                provider_keys.insert(normalized_provider_key);
            }
            if let Some(channel_id) = channel
                .channel_id()
                .and_then(|value| value.parse::<u64>().ok())
            {
                channel_ids.insert(channel_id);
            }
            let tui_hosting = channel.tui_hosting();
            let runtime = channel.runtime_mode_raw();
            let provider_id = channel
                .provider()
                .unwrap_or_else(|| channel_kind.to_string())
                .trim()
                .to_ascii_lowercase();
            let target = target.trim().to_ascii_lowercase();
            let entry = bindings
                .entry(AgentProviderRuntimeBindingKey {
                    provider_id,
                    target,
                })
                .or_default();
            if tui_hosting.is_some() {
                entry.tui_hosting = tui_hosting;
            }
            if runtime.is_some() {
                entry.runtime = runtime;
            }
        }
    }
    AgentLaunchFingerprint {
        provider_keys,
        agent_ids,
        channel_ids,
        runtime_bindings: bindings,
    }
}

/// The infra sections that are bound into long-lived objects at boot and cannot
/// be hot-swapped under a running process. A change here is applied to the
/// snapshot but needs a restart to take full effect, so it is reported to the
/// operator (logged as restart-required) instead of silently appearing to apply.
///
/// Without this list, editing e.g. a Discord bot token, an `mcp_servers` entry,
/// or a `providers` runtime in `agentdesk.yaml` would swap into the snapshot
/// with no observable effect and no warning — the exact "the edit did nothing"
/// trap. Most sections use serialized equality for broad coverage, while
/// secret-bearing or order-sensitive sections use deterministic non-logging
/// fingerprints.
pub fn restart_required_changes(old: &Config, new: &Config) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if section_changed(&old.server, &new.server) {
        changed.push("server");
    }
    if section_changed(&old.database, &new.database) {
        changed.push("database");
    }
    if section_changed(&old.data, &new.data) {
        changed.push("data");
    }
    // The policy engine constructs its QuickJS runtime, directory watcher, and
    // hook timeout/memory limits at boot.
    if section_changed(&old.policies, &new.policies) {
        changed.push("policies");
    }
    // The Discord client + bot bindings/ids are constructed at boot.
    if discord_boot_config_changed(&old.discord, &new.discord) {
        changed.push("discord");
    }
    // Provider runtimes (TUI hosting, remote SSH) are wired up at boot.
    if section_changed(&old.providers, &new.providers) {
        changed.push("providers");
    }
    // Per-agent channel provider runtime/tui_hosting bindings are installed
    // into process-global maps at boot by `services::provider_hosting`; the
    // Discord bot launcher also uses provider keys, agent ids, and channel ids
    // to decide which configured bots actually run.
    if agent_launch_fingerprint(old) != agent_launch_fingerprint(new) {
        changed.push("agents");
    }
    // MCP servers are spawned as child processes at boot.
    if section_changed(&old.mcp_servers, &new.mcp_servers) {
        changed.push("mcp_servers");
    }
    // The MCP credential watcher (and its dedupe window) is started at boot.
    if section_changed(&old.mcp, &new.mcp) {
        changed.push("mcp");
    }
    // The memory backend (file paths / MCP endpoint) is bound at boot.
    if section_changed(&old.memory, &new.memory) {
        changed.push("memory");
    }
    // Prompt-manifest retention is installed into a set-once boot snapshot.
    if section_changed(
        &old.prompt_manifest_retention,
        &new.prompt_manifest_retention,
    ) {
        changed.push("prompt_manifest_retention");
    }
    changed
}

/// Re-read, validate, and (on success) atomically swap in the config at `path`.
/// On failure the running snapshot is left untouched. This is the shared core
/// used by the watcher; expose it so an on-demand trigger can reuse it.
pub fn reload_from_path(path: &Path) -> ReloadOutcome {
    match crate::config::load_from_path(path) {
        Ok(new_config) => {
            let restart_required = current()
                .map(|old| restart_required_changes(&old, &new_config))
                .unwrap_or_default();
            store(Arc::new(new_config));
            ReloadOutcome::Applied { restart_required }
        }
        Err(error) => ReloadOutcome::Rejected {
            error: format!("{error:#}"),
        },
    }
}

/// Guard returned by [`start`]; keeps the watcher and worker thread alive and
/// joins the worker on drop. Mirrors `engine::loader::HotReloadGuard`.
pub struct ConfigHotReloadGuard {
    _watcher: Option<RecommendedWatcher>,
    join: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Drop for ConfigHotReloadGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Start watching `path`'s directory and hot-reloading it on change. Returns
/// `None` (and logs) when disabled or when the watch cannot be established
/// (e.g. the parent directory does not exist), so the caller simply runs without
/// live reload — never a hard failure.
pub fn start(path: PathBuf, enabled: bool) -> Option<ConfigHotReloadGuard> {
    if !enabled {
        return None;
    }
    let Some(dir) = path.parent().map(Path::to_path_buf) else {
        tracing::warn!(
            path = %path.display(),
            "config hot-reload: config path has no parent dir; disabled"
        );
        return None;
    };
    if !dir.exists() {
        tracing::warn!(
            dir = %dir.display(),
            "config hot-reload: config directory does not exist; disabled"
        );
        return None;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    // Watch the parent directory (not the file) so atomic rename-on-save —
    // write `agentdesk.yaml.tmp`, then rename over `agentdesk.yaml` — is still
    // observed; watching the inode directly misses the swap.
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res
                && matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
            {
                let _ = tx.send(event);
            }
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!(%error, "config hot-reload: failed to create watcher; disabled");
                return None;
            }
        };
    if let Err(error) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        tracing::warn!(%error, dir = %dir.display(), "config hot-reload: watch failed; disabled");
        return None;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let target_name = path.file_name().map(std::ffi::OsString::from);
    let path_display = path.display().to_string();
    let join = std::thread::Builder::new()
        .name("config-hot-reload".into())
        .spawn(move || {
            let mut last_reload = Instant::now() - DEBOUNCE;
            loop {
                if stop_worker.load(Ordering::Acquire) {
                    break;
                }
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(event) => {
                        if stop_worker.load(Ordering::Acquire) {
                            break;
                        }
                        // Ignore events for sibling files in the directory.
                        let touches_target = target_name.as_ref().is_none_or(|name| {
                            event
                                .paths
                                .iter()
                                .any(|p| p.file_name() == Some(name.as_os_str()))
                        });
                        if !touches_target {
                            continue;
                        }
                        if last_reload.elapsed() < DEBOUNCE {
                            while rx.try_recv().is_ok() {}
                            continue;
                        }
                        // Drain the rest of the burst, then reload once.
                        while rx.try_recv().is_ok() {}
                        last_reload = Instant::now();
                        match reload_from_path(&path) {
                            ReloadOutcome::Applied { restart_required } if restart_required.is_empty() => {
                                tracing::info!(
                                    path = %path.display(),
                                    "config hot-reload applied"
                                );
                            }
                            ReloadOutcome::Applied { restart_required } => {
                                tracing::warn!(
                                    path = %path.display(),
                                    restart_required = restart_required.join(","),
                                    "config hot-reload applied; some sections changed that need a restart to take full effect"
                                );
                            }
                            ReloadOutcome::Rejected { error } => {
                                tracing::warn!(
                                    path = %path.display(),
                                    %error,
                                    "config hot-reload rejected invalid config; keeping running config"
                                );
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .ok();

    if join.is_none() {
        tracing::warn!("config hot-reload: failed to spawn worker thread; disabled");
        return None;
    }

    tracing::info!(path = %path_display, "config file hot-reload watching enabled");
    Some(ConfigHotReloadGuard {
        _watcher: Some(watcher),
        join,
        stop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate/read the process-global [`LIVE`] snapshot so
    /// they do not race each other under the parallel test runner.
    fn global_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    fn test_bot(token: &str) -> crate::config::BotConfig {
        crate::config::BotConfig {
            token: Some(token.to_string()),
            description: Some("test bot".to_string()),
            provider: Some("claude".to_string()),
            agent: Some("dispatcher".to_string()),
            auth: crate::config::DiscordBotAuthConfig {
                allowed_channel_ids: Some(vec![42]),
                ..crate::config::DiscordBotAuthConfig::default()
            },
        }
    }

    fn test_agent_with_claude_channel(
        channel_id: &str,
        tui_hosting: Option<bool>,
        runtime: Option<&str>,
    ) -> crate::config::AgentDef {
        crate::config::AgentDef {
            id: "dispatcher".to_string(),
            name: "Dispatcher".to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: crate::config::AgentVoiceConfig::default(),
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels {
                claude: Some(crate::config::AgentChannel::Detailed(
                    crate::config::AgentChannelConfig {
                        id: Some(channel_id.to_string()),
                        provider: Some("claude".to_string()),
                        tui_hosting,
                        runtime: runtime.map(str::to_string),
                        ..crate::config::AgentChannelConfig::default()
                    },
                )),
                ..crate::config::AgentChannels::default()
            },
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
            preferred_intake_node_labels: None,
        }
    }

    fn test_agent_with_named_claude_channel(channel_name: &str) -> crate::config::AgentDef {
        let mut agent = test_agent_with_claude_channel("123", None, None);
        agent.channels.claude = Some(crate::config::AgentChannel::Detailed(
            crate::config::AgentChannelConfig {
                name: Some(channel_name.to_string()),
                provider: Some("claude".to_string()),
                ..crate::config::AgentChannelConfig::default()
            },
        ));
        agent
    }

    // A valid edit validates and swaps into the live snapshot.
    #[test]
    fn reload_applies_valid_config() {
        let _guard = global_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        write(&path, "server:\n  port: 8791\n");
        install(crate::config::load_from_path(&path).unwrap());

        write(&path, "server:\n  port: 8799\n");
        let outcome = reload_from_path(&path);
        assert!(matches!(outcome, ReloadOutcome::Applied { .. }));
        assert_eq!(current().unwrap().server.port, 8799);
    }

    // A broken edit is rejected and the running snapshot is preserved.
    #[test]
    fn reload_rejects_invalid_config_and_keeps_current() {
        let _guard = global_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        write(&path, "server:\n  port: 8791\n");
        install(crate::config::load_from_path(&path).unwrap());
        let before = current().unwrap().server.port;

        write(&path, "server:\n  port: : not valid yaml :::\n");
        let outcome = reload_from_path(&path);
        assert!(matches!(outcome, ReloadOutcome::Rejected { .. }));
        assert_eq!(
            current().unwrap().server.port,
            before,
            "rejected reload must not mutate the live snapshot"
        );
    }

    // Infra-section changes are surfaced as restart-required.
    #[test]
    fn restart_required_changes_flags_infra_sections() {
        let mut old = Config::default();
        old.server.port = 8791;
        let mut new = old.clone();

        assert!(restart_required_changes(&old, &new).is_empty());

        new.server.port = 9000;
        assert_eq!(restart_required_changes(&old, &new), vec!["server"]);

        new = old.clone();
        new.data.dir = old.data.dir.join("moved");
        assert_eq!(restart_required_changes(&old, &new), vec!["data"]);

        new = old.clone();
        new.policies.hook_timeout_ms = old.policies.hook_timeout_ms.wrapping_add(1);
        assert_eq!(restart_required_changes(&old, &new), vec!["policies"]);
    }

    // A hot-swappable-only change (routine tunable) reports no restart-required.
    #[test]
    fn routine_tunable_change_needs_no_restart() {
        let old = Config::default();
        let mut new = old.clone();
        new.routines.max_agent_polls_per_tick =
            old.routines.max_agent_polls_per_tick.wrapping_add(1);
        assert!(restart_required_changes(&old, &new).is_empty());
    }

    // Each boot-bound section is surfaced as restart-required, so editing e.g. a
    // Discord binding, a provider runtime, an MCP server, the credential watcher,
    // or the memory backend in `agentdesk.yaml` is reported rather than silently
    // swapped into the snapshot with no running effect.
    #[test]
    fn restart_required_changes_flags_boot_bound_sections() {
        let base = Config::default();

        let mut discord = base.clone();
        discord.discord.owner_id = Some(42);
        assert_eq!(restart_required_changes(&base, &discord), vec!["discord"]);

        let mut providers = base.clone();
        providers.providers.insert(
            "codex".to_string(),
            crate::config::ProviderConfig::default(),
        );
        assert_eq!(
            restart_required_changes(&base, &providers),
            vec!["providers"]
        );

        let mut mcp_servers = base.clone();
        mcp_servers.mcp_servers.insert(
            "memento".to_string(),
            crate::config::McpServerConfig::default(),
        );
        assert_eq!(
            restart_required_changes(&base, &mcp_servers),
            vec!["mcp_servers"]
        );

        let mut mcp = base.clone();
        mcp.mcp.watch_credentials = !base.mcp.watch_credentials;
        assert_eq!(restart_required_changes(&base, &mcp), vec!["mcp"]);

        let mut memory = base.clone();
        memory.memory = Some(crate::config::MemoryConfig::default());
        assert_eq!(restart_required_changes(&base, &memory), vec!["memory"]);

        let mut prompt_retention = base.clone();
        prompt_retention.prompt_manifest_retention.full_content_days = base
            .prompt_manifest_retention
            .full_content_days
            .wrapping_add(1);
        assert_eq!(
            restart_required_changes(&base, &prompt_retention),
            vec!["prompt_manifest_retention"]
        );
    }

    #[test]
    fn restart_required_changes_detects_discord_token_only_rotation() {
        let mut old = Config::default();
        old.discord
            .bots
            .insert("notify".to_string(), test_bot("old"));
        let mut new = old.clone();
        new.discord.bots.get_mut("notify").unwrap().token = Some("new".to_string());

        assert_eq!(restart_required_changes(&old, &new), vec!["discord"]);
    }

    #[test]
    fn restart_required_changes_ignores_discord_bot_hashmap_order() {
        let mut old = Config::default();
        old.discord.bots.insert("alpha".to_string(), test_bot("a"));
        old.discord.bots.insert("beta".to_string(), test_bot("b"));

        let mut new = Config::default();
        new.discord.bots.insert("beta".to_string(), test_bot("b"));
        new.discord.bots.insert("alpha".to_string(), test_bot("a"));

        assert!(restart_required_changes(&old, &new).is_empty());
    }

    #[test]
    fn restart_required_changes_flags_agent_channel_runtime_bindings() {
        let mut old = Config::default();
        old.agents.push(test_agent_with_claude_channel(
            "123",
            Some(false),
            Some("pipe"),
        ));

        let mut runtime_changed = old.clone();
        runtime_changed.agents[0] = test_agent_with_claude_channel("123", Some(false), Some("tui"));
        assert_eq!(
            restart_required_changes(&old, &runtime_changed),
            vec!["agents"]
        );

        let mut tui_changed = old.clone();
        tui_changed.agents[0] = test_agent_with_claude_channel("123", Some(true), Some("pipe"));
        assert_eq!(restart_required_changes(&old, &tui_changed), vec!["agents"]);
    }

    #[test]
    fn restart_required_changes_flags_agent_channel_launch_bindings() {
        let old = Config::default();
        let mut new = old.clone();
        new.agents
            .push(test_agent_with_claude_channel("123", None, None));

        assert_eq!(restart_required_changes(&old, &new), vec!["agents"]);

        let mut name_target = old.clone();
        name_target
            .agents
            .push(test_agent_with_named_claude_channel("voice-codex"));
        assert_eq!(restart_required_changes(&old, &name_target), vec!["agents"]);

        let mut renamed_agent = new.clone();
        renamed_agent.agents[0].id = "renamed-dispatcher".to_string();
        assert_eq!(
            restart_required_changes(&new, &renamed_agent),
            vec!["agents"]
        );
    }

    #[test]
    fn restart_required_changes_preserves_split_duplicate_agent_channel_overrides() {
        let mut old = Config::default();
        old.agents
            .push(test_agent_with_claude_channel("123", None, Some("pipe")));
        old.agents
            .push(test_agent_with_claude_channel("123", Some(false), None));

        let mut new = Config::default();
        new.agents
            .push(test_agent_with_claude_channel("123", None, Some("tui")));
        new.agents
            .push(test_agent_with_claude_channel("123", Some(false), None));

        assert_eq!(restart_required_changes(&old, &new), vec!["agents"]);
    }

    #[test]
    fn restart_required_changes_preserves_duplicate_agent_channel_last_wins() {
        let mut old = Config::default();
        old.agents.push(test_agent_with_claude_channel(
            "123",
            Some(false),
            Some("pipe"),
        ));
        old.agents.push(test_agent_with_claude_channel(
            "123",
            Some(false),
            Some("tui"),
        ));

        let mut new = Config::default();
        new.agents.push(test_agent_with_claude_channel(
            "123",
            Some(false),
            Some("tui"),
        ));
        new.agents.push(test_agent_with_claude_channel(
            "123",
            Some(false),
            Some("pipe"),
        ));

        assert_eq!(restart_required_changes(&old, &new), vec!["agents"]);
    }
}
