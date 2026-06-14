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
//! Infra fields (`server` bind/port/auth, `database`, `data`) are bound into
//! long-lived objects at boot and cannot be swapped under a running process; a
//! change to those is still stored in the snapshot but reported by
//! [`restart_required_changes`] and logged as restart-required.
//!
//! The whole-`Config` value is NOT threaded through every reader — instead this
//! follows the existing global-runtime-setter precedent
//! (`set_runtime_cluster_config`) so consumers opt in by reading [`current`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;

use crate::config::Config;

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

/// The infra sections that are bound at boot and cannot be hot-swapped under a
/// running process. A change here is applied to the snapshot but needs a restart
/// to take full effect.
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
}
