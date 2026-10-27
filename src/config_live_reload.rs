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
//! [`crate::config::reload_classes::FIELDS`] classifies every field. A changed `restart` or
//! `seeded-at-boot` field is stored anyway; [`restart_required_changes`] names it in a WARN.
//!
//! The whole-`Config` value is NOT threaded through every reader — instead this
//! follows the existing global-runtime-setter precedent
//! (`set_runtime_cluster_config`) so consumers opt in by reading [`current`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::config::Config;
use crate::config::reload_classes::ReloadClass;

#[cfg(test)]
#[path = "config/readers_live_reload_tests.rs"]
mod readers_tests;

#[cfg(all(test, unix))]
#[path = "config/daemon_snapshot_pg_tests.rs"]
mod daemon_snapshot_pg_tests;

/// Process-global live config snapshot. `None` until [`install`] runs at boot.
static LIVE: OnceLock<RwLock<Arc<Config>>> = OnceLock::new();

/// Every successfully installed snapshot, including a same-value reload.
/// Consumers that must redo external validation subscribe here instead of
/// polling [`current`] from their hot paths.
static LIVE_UPDATES: OnceLock<tokio::sync::watch::Sender<Option<Arc<Config>>>> = OnceLock::new();

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

/// Safe fallback for transient invalid or missing on-disk configuration.
pub(crate) fn graceful_fallback_config(path_display: &str, reason: String) -> Config {
    if let Some(live) = current() {
        tracing::warn!(
            "  ⚠ {reason} — keeping the last validated live config (source: {path_display})"
        );
        return (*live).clone();
    }

    tracing::warn!("  ⚠ {reason} — using defaults");
    Config::default()
}

pub(crate) fn subscribe() -> tokio::sync::watch::Receiver<Option<Arc<Config>>> {
    LIVE_UPDATES
        .get_or_init(|| tokio::sync::watch::channel(None).0)
        .subscribe()
}

fn store(config: Arc<Config>) {
    let lock = LIVE.get_or_init(|| RwLock::new(config.clone()));
    *lock
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = config.clone();
    LIVE_UPDATES
        .get_or_init(|| tokio::sync::watch::channel(None).0)
        .send_replace(Some(config));
}

/// Paths of the non-live fields in [`crate::config::reload_classes::FIELDS`] whose
/// value changed. The snapshot still takes the edit; these act on it only after a restart.
pub fn restart_required_changes(old: &Config, new: &Config) -> Vec<&'static str> {
    crate::config::reload_classes::changed_non_live(old, new)
        .into_iter()
        .map(|field| field.path)
        .collect()
}

fn log_applied(path: &Path, restart_required: &[&'static str]) {
    if restart_required.is_empty() {
        tracing::info!(path = %path.display(), "config hot-reload applied");
        return;
    }
    let of = |class| {
        restart_required
            .iter()
            .filter(|field| crate::config::reload_classes::class_of(field) == Some(class))
            .copied()
            .collect::<Vec<_>>()
            .join(",")
    };
    tracing::warn!(
        path = %path.display(),
        restart = of(ReloadClass::Restart),
        seeded_at_boot = of(ReloadClass::SeededAtBoot),
        "config hot-reload stored the edit, but these fields take effect only after a restart (seeded-at-boot fields are re-seeded into the DB/kv at boot)"
    );
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
            "config hot-reload: config path has no parent dir; retaining the last validated config until restart"
        );
        return None;
    };
    if !dir.exists() {
        tracing::warn!(
            dir = %dir.display(),
            "config hot-reload: config directory does not exist; retaining the last validated config until restart"
        );
        return None;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    // Watch the parent directory (not the file) so atomic rename-on-save —
    // write `agentdesk.yaml.tmp`, then rename over `agentdesk.yaml` — is still
    // observed; watching the inode directly misses the swap.
    let mut watcher = match notify::recommended_watcher(
        move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res
                && matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
            {
                let _ = tx.send(event);
            }
        },
    ) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(%error, "config hot-reload: failed to create watcher; retaining the last validated config until restart");
            return None;
        }
    };
    if let Err(error) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        tracing::warn!(%error, dir = %dir.display(), "config hot-reload: watch failed; retaining the last validated config until restart");
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
                            ReloadOutcome::Applied { restart_required } => {
                                log_applied(&path, &restart_required);
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
        tracing::warn!(
            "config hot-reload: failed to spawn worker thread; retaining the last validated config until restart"
        );
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
pub(crate) struct TestLiveGuard {
    previous: Config,
    _lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

#[cfg(test)]
impl Drop for TestLiveGuard {
    fn drop(&mut self) {
        install(self.previous.clone());
    }
}

#[cfg(test)]
pub(crate) fn test_live_guard(
    lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
) -> TestLiveGuard {
    let previous = current()
        .map(|config| (*config).clone())
        .unwrap_or_default();
    TestLiveGuard {
        previous,
        _lock: lock,
    }
}

#[cfg(test)]
#[path = "config/late_live_reload_tests.rs"]
mod late_live_reload_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate/read the process-global [`LIVE`] snapshot so
    /// they do not race each other under the parallel test runner.
    fn global_test_guard() -> TestLiveGuard {
        test_live_guard(crate::config::test_env_lock::acquire_shared_test_env_lock())
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
            channels: crate::config::AgentChannels::new().with(
                "claude",
                crate::config::AgentChannel::Detailed(crate::config::AgentChannelConfig {
                    id: Some(channel_id.to_string()),
                    provider: Some("claude".to_string()),
                    tui_hosting,
                    runtime: runtime.map(str::to_string),
                    ..crate::config::AgentChannelConfig::default()
                }),
            ),
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
            preferred_intake_node_labels: None,
            memory_group: None,
        }
    }

    fn test_agent_with_named_claude_channel(channel_name: &str) -> crate::config::AgentDef {
        let mut agent = test_agent_with_claude_channel("123", None, None);
        agent.channels.insert(
            "claude",
            crate::config::AgentChannel::Detailed(crate::config::AgentChannelConfig {
                name: Some(channel_name.to_string()),
                provider: Some("claude".to_string()),
                ..crate::config::AgentChannelConfig::default()
            }),
        );
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

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = LogBuffer;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn logged_lines(path: &Path) -> Vec<String> {
        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(buffer.clone())
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        let ReloadOutcome::Applied { restart_required } = reload_from_path(path) else {
            panic!("valid reload rejected");
        };
        tracing::subscriber::with_default(subscriber, || log_applied(path, &restart_required));
        let text = String::from_utf8_lossy(&buffer.0.lock().unwrap()).into_owned();
        text.lines().map(str::to_string).collect()
    }

    // A reload that touches restart and seeded-at-boot fields logs exactly one WARN
    // naming them by class; a live-only reload logs only "applied".
    #[test]
    fn reload_warns_once_for_restart_and_seeded_fields() {
        let _guard = global_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        write(&path, "server:\n  port: 8791\n");
        install(crate::config::load_from_path(&path).unwrap());

        let live = "server:\n  port: 8791\nroutines:\n  max_due_per_tick: 13\n";
        write(&path, live);
        let lines = logged_lines(&path);
        assert!(
            lines.iter().all(|line| !line.contains("WARN")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("config hot-reload applied"))
        );

        let both =
            "server:\n  port: 8799\nreview:\n  enabled: false\nroutines:\n  max_due_per_tick: 14\n";
        write(&path, both);
        let warnings: Vec<_> = logged_lines(&path)
            .into_iter()
            .filter(|line| line.contains("WARN"))
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(
            warnings[0].contains("restart=\"server.port\""),
            "{warnings:#?}"
        );
        assert!(
            warnings[0].contains("seeded_at_boot=\"review.enabled\""),
            "{warnings:#?}"
        );
        assert!(!warnings[0].contains("max_due_per_tick"), "{warnings:#?}");
    }

    #[test]
    fn subscribers_observe_same_value_install() {
        let _guard = global_test_guard();
        let config = Config::default();
        install(config.clone());
        let receiver = subscribe();

        install(config);

        assert!(matches!(receiver.has_changed(), Ok(true)));
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

    #[test]
    fn retired_repost_keys_are_ignored_on_reload() {
        let _guard = global_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate.yaml");
        let retained = "server: {port: 8791}\ntui_o:\n  writer: {all_tui: true}\n  tool_cards: {enabled: false}\n";
        write(&path, retained);
        install(crate::config::load_from_path(&path).unwrap());
        let expected = current().unwrap().tui_o.clone();
        let receiver = subscribe();
        for retired in [
            "{enabled: true, channels: [41]}",
            "{enabled: false, channels: []}",
            "{enabled: obsolete, channels: malformed, enabeld: true}",
        ] {
            write(&path, &format!("{retained}  repost: {retired}\n"));
            assert!(matches!(
                reload_from_path(&path),
                ReloadOutcome::Applied { .. }
            ));
            let installed = current().unwrap();
            assert_eq!(installed.tui_o, expected);
            assert_eq!(receiver.borrow().as_ref().unwrap().tui_o, expected);
            assert!(
                serde_yaml::to_value(&installed.tui_o)
                    .unwrap()
                    .get("repost")
                    .is_none()
            );
        }
    }

    #[test]
    fn load_graceful_uses_live_snapshot_when_file_is_invalid() {
        let _guard = global_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        let mut config = crate::config::Config::default();
        config.server.port = 8797;
        crate::config::save_to_path(&path, &config).unwrap();
        install(crate::config::load_from_path(&path).unwrap());

        let _config_path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &path,
        );
        write(&path, "server:\n  port: : invalid yaml\n");

        let loaded = crate::config::load_graceful();
        assert_eq!(loaded.server.port, 8797);
    }

    // Infra-section changes are surfaced as restart-required.
    #[test]
    fn restart_required_changes_flags_infra_sections() {
        let mut old = Config::default();
        old.server.port = 8791;
        let mut new = old.clone();

        assert!(restart_required_changes(&old, &new).is_empty());

        new.server.port = 9000;
        assert_eq!(restart_required_changes(&old, &new), vec!["server.port"]);

        new = old.clone();
        new.data.dir = old.data.dir.join("moved");
        assert_eq!(restart_required_changes(&old, &new), vec!["data.dir"]);

        new = old.clone();
        new.policies.hook_timeout_ms = old.policies.hook_timeout_ms.wrapping_add(1);
        assert_eq!(restart_required_changes(&old, &new), vec!["policies"]);
    }

    #[test]
    fn gateway_handback_breaker_changes_need_no_restart() {
        let old = Config::default();
        let mut new = old.clone();
        new.cluster.gateway_handback_breaker.enabled = false;
        new.cluster.gateway_handback_breaker.window_secs += 1;
        new.cluster.gateway_handback_breaker.max_empty += 1;
        new.cluster.gateway_handback_breaker.suppress_secs += 1;
        assert!(restart_required_changes(&old, &new).is_empty());
        assert!(restart_required_changes(&new, &old).is_empty());

        new.cluster.lease_ttl_secs += 1;
        assert_eq!(
            restart_required_changes(&old, &new),
            vec!["cluster.lease_ttl_secs"]
        );
        assert_eq!(
            restart_required_changes(&new, &old),
            vec!["cluster.lease_ttl_secs"]
        );
    }

    #[test]
    fn intake_owner_authority_allowlist_change_needs_no_restart() {
        let old = Config::default();
        let mut new = old.clone();
        new.cluster
            .intake_routing
            .owner_authority_channel_ids
            .push("123456789012345678".to_string());

        assert!(restart_required_changes(&old, &new).is_empty());
    }

    // A hot-swappable-only change (routine tunable) reports no restart-required.
    #[test]
    fn routine_tunable_change_needs_no_restart() {
        let mut old = Config::default();
        old.routines.enabled = true;
        let mut new = old.clone();
        new.routines.max_agent_polls_per_tick =
            old.routines.max_agent_polls_per_tick.wrapping_add(1);
        assert!(restart_required_changes(&old, &new).is_empty());

        new = old.clone();
        new.routines.max_due_per_tick = old.routines.max_due_per_tick.wrapping_add(1);
        assert!(restart_required_changes(&old, &new).is_empty());

        new = old.clone();
        new.routines.hot_reload = !old.routines.hot_reload;
        assert!(restart_required_changes(&old, &new).is_empty());

        new = old.clone();
        new.routines.stale_paused_alert_secs = old.routines.stale_paused_alert_secs.wrapping_add(1);
        assert!(restart_required_changes(&old, &new).is_empty());

        new = old.clone();
        new.routines.stale_paused_alert_ttl_secs =
            old.routines.stale_paused_alert_ttl_secs.wrapping_add(1);
        assert!(restart_required_changes(&old, &new).is_empty());

        new = old.clone();
        new.routines.startup_grace_secs = old.routines.startup_grace_secs.wrapping_add(1);
        assert!(restart_required_changes(&old, &new).is_empty());

        // The tick reads these live, but `run_routine_now` reads the boot AppState copy.
        new = old.clone();
        new.routines.failure_pause_auto_resume_secs =
            old.routines.failure_pause_auto_resume_secs.wrapping_add(1);
        new.routines.max_consecutive_failures =
            old.routines.max_consecutive_failures.wrapping_add(1);
        assert_eq!(
            restart_required_changes(&old, &new),
            vec![
                "routines.failure_pause_auto_resume_secs",
                "routines.max_consecutive_failures"
            ]
        );
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
        assert_eq!(
            restart_required_changes(&base, &discord),
            vec!["discord.owner_id"]
        );

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

        // Memory backend paths and endpoints are re-read from disk per call.
        let mut memory = base.clone();
        memory.memory = Some(crate::config::MemoryConfig::default());
        assert!(restart_required_changes(&base, &memory).is_empty());

        let mut cluster = base.clone();
        cluster.cluster.enabled = !base.cluster.enabled;
        assert_eq!(
            restart_required_changes(&base, &cluster),
            vec!["cluster.enabled"]
        );

        let mut global_voice = base.clone();
        global_voice.voice.enabled = !base.voice.enabled;
        assert_eq!(
            restart_required_changes(&base, &global_voice),
            vec!["voice.enabled"]
        );

        let mut review_mcp_allowlist = base.clone();
        review_mcp_allowlist
            .review_mcp_allowlist
            .push("memento".to_string());
        // Read from disk on every Claude spawn.
        assert!(restart_required_changes(&base, &review_mcp_allowlist).is_empty());

        let mut github = base.clone();
        github.github.sync_interval_minutes = base.github.sync_interval_minutes.wrapping_add(1);
        assert_eq!(
            restart_required_changes(&base, &github),
            vec!["github.sync_interval_minutes"]
        );

        let mut placeholder = base.clone();
        placeholder.placeholder.live_events_enabled = !base.placeholder.live_events_enabled;
        assert_eq!(
            restart_required_changes(&base, &placeholder),
            vec!["placeholder"]
        );

        let mut prompt_retention = base.clone();
        prompt_retention.prompt_manifest_retention.full_content_days = base
            .prompt_manifest_retention
            .full_content_days
            .wrapping_add(1);
        assert_eq!(
            restart_required_changes(&base, &prompt_retention),
            vec!["prompt_manifest_retention"]
        );

        let mut config_hot_reload = base.clone();
        config_hot_reload.config_hot_reload = !base.config_hot_reload;
        assert_eq!(
            restart_required_changes(&base, &config_hot_reload),
            vec!["config_hot_reload"]
        );
    }

    #[test]
    fn restart_required_changes_flags_boot_bound_routine_settings() {
        let base = Config::default();

        let mut enabled = base.clone();
        enabled.routines.enabled = !base.routines.enabled;
        assert_eq!(
            restart_required_changes(&base, &enabled),
            vec!["routines.enabled"]
        );

        let mut headless = base.clone();
        headless.routines.headless_fresh_agents = true;
        assert_eq!(
            restart_required_changes(&base, &headless),
            vec!["routines.headless_fresh_agents"]
        );

        let mut dir = base.clone();
        dir.routines.dir = PathBuf::from("./alternate-routines");
        assert_eq!(restart_required_changes(&base, &dir), vec!["routines.dir"]);

        let mut additional_dirs = base.clone();
        additional_dirs
            .routines
            .additional_dirs
            .push(PathBuf::from("./operator-routines"));
        assert_eq!(
            restart_required_changes(&base, &additional_dirs),
            vec!["routines.additional_dirs"]
        );

        let mut tick_interval = base.clone();
        tick_interval.routines.tick_interval_secs =
            base.routines.tick_interval_secs.wrapping_add(1);
        assert_eq!(
            restart_required_changes(&base, &tick_interval),
            vec!["routines.tick_interval_secs"]
        );

        let mut timezone = base.clone();
        timezone.routines.default_timezone = "UTC".to_string();
        assert_eq!(
            restart_required_changes(&base, &timezone),
            vec!["routines.default_timezone"]
        );

        let mut agent_timeout = base.clone();
        agent_timeout.routines.agent_timeout_secs =
            base.routines.agent_timeout_secs.wrapping_add(1);
        assert_eq!(
            restart_required_changes(&base, &agent_timeout),
            vec!["routines.agent_timeout_secs"]
        );

        let mut checkpoint_limit = base.clone();
        checkpoint_limit.routines.max_checkpoint_bytes =
            base.routines.max_checkpoint_bytes.wrapping_add(1);
        assert_eq!(
            restart_required_changes(&base, &checkpoint_limit),
            vec!["routines.max_checkpoint_bytes"]
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

        assert_eq!(
            restart_required_changes(&old, &new),
            vec!["discord.bots.*.token"]
        );
    }

    /// `server.auth_token` is `skip_serializing`, so serialized equality cannot see it appear,
    /// vanish or rotate; it is compared directly as the HTTP and `/ws` gates hold the boot copy.
    #[test]
    fn restart_required_changes_detects_server_auth_token_presence_flip() {
        let mut old = Config::default();
        old.server.auth_token = Some("dashboard-secret-token".to_string());
        let mut new = old.clone();
        new.server.auth_token = None;

        assert_eq!(
            restart_required_changes(&old, &new),
            vec!["server.auth_token"]
        );
        assert_eq!(
            restart_required_changes(&new, &old),
            vec!["server.auth_token"]
        );

        let mut rotated = old.clone();
        rotated.server.auth_token = Some("rotated-secret-token".to_string());
        assert_eq!(
            restart_required_changes(&old, &rotated),
            vec!["server.auth_token"]
        );
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
            vec!["agents.*.channels.*.runtime"]
        );

        let mut tui_changed = old.clone();
        tui_changed.agents[0] = test_agent_with_claude_channel("123", Some(true), Some("pipe"));
        assert_eq!(
            restart_required_changes(&old, &tui_changed),
            vec!["agents.*.channels.*.tui_hosting"]
        );
    }

    #[test]
    fn restart_required_changes_flags_agent_channel_launch_bindings() {
        let old = Config::default();
        let mut new = old.clone();
        new.agents
            .push(test_agent_with_claude_channel("123", None, None));

        let added = restart_required_changes(&old, &new);
        for field in ["agents.*.id", "agents.*.name", "agents.*.channels.*.id"] {
            assert!(added.contains(&field), "{field}: {added:?}");
        }

        let mut name_target = old.clone();
        name_target
            .agents
            .push(test_agent_with_named_claude_channel("voice-codex"));
        let named = restart_required_changes(&old, &name_target);
        assert!(named.contains(&"agents.*.channels.*.name"), "{named:?}");

        let mut renamed_agent = new.clone();
        renamed_agent.agents[0].id = "renamed-dispatcher".to_string();
        assert_eq!(
            restart_required_changes(&new, &renamed_agent),
            vec!["agents.*.id"]
        );
    }

    #[test]
    fn restart_required_changes_flags_agent_voice_bindings() {
        let mut old = Config::default();
        old.agents
            .push(test_agent_with_claude_channel("123", None, None));

        let mut voice_channel_changed = old.clone();
        voice_channel_changed.agents[0].voice.channel_id = Some("456".to_string());
        assert_eq!(
            restart_required_changes(&old, &voice_channel_changed),
            vec!["agents.*.voice.channel_id"]
        );

        let mut foreground_changed = voice_channel_changed.clone();
        foreground_changed.agents[0].voice.foreground.provider = Some("codex".to_string());
        assert_eq!(
            restart_required_changes(&voice_channel_changed, &foreground_changed),
            vec!["agents.*.voice.foreground.provider"]
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

        assert_eq!(
            restart_required_changes(&old, &new),
            vec!["agents.*.channels.*.runtime"]
        );
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

        assert_eq!(
            restart_required_changes(&old, &new),
            vec!["agents.*.channels.*.runtime"]
        );
    }
}
