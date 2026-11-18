//! Watches the Claude CLI MCP config files and posts a one-line notification to
//! every active Claude session when the registered MCP server set changes.
//!
//! Watched paths (relative to `$CLAUDE_CONFIG_DIR` if set, otherwise `$HOME`):
//! - `.claude.json` — top-level Claude Code config (MCP server registrations live here per docs)
//! - `.claude/.mcp.json` — project-scoped MCP config (matches the existing repo lookup
//!   in `mcp_config::resolve_claude_user_mcp_config_path`)
//!
//! We deliberately do NOT watch `.claude/.credentials.json` (#3554): it holds the
//! Claude Code OAuth login token, which is rewritten on every routine access-token
//! refresh. Presence-only tracking treated each refresh as an MCP change and
//! spammed a useless `/restart` notification. The authoritative signal for a new
//! OAuth-backed MCP server is its registration landing in `.claude.json`/`.mcp.json`,
//! which the `mcpServers` content diff below already detects.
//!
//! Background: Claude CLI attaches MCP servers at boot and never hot-reloads
//! them. When the operator authenticates a new MCP server (e.g. memento)
//! mid-session, the running Claude process never sees the new tools. This
//! watcher tells operators that a `/restart` is needed to pick up the new tools.
//!
//! Per-channel dedupe is in-memory (HashMap<ChannelId, Instant>); we deliberately
//! do not persist this across restarts because the cooldown is short (5min default)
//! and re-notification after a restart is harmless.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use super::SharedData;
use crate::services::tui_o::shadow::capture::Fingerprint;

const POLL_INTERVAL: Duration = Duration::from_secs(3);

struct ConfigFilePoller {
    stamps: HashMap<PathBuf, Option<Fingerprint>>,
}

impl ConfigFilePoller {
    fn new(paths: &[PathBuf]) -> Self {
        Self {
            stamps: paths.iter().map(|p| (p.clone(), file_stamp(p))).collect(),
        }
    }

    fn changed_paths(&mut self) -> Vec<PathBuf> {
        self.stamps
            .iter_mut()
            .filter_map(|(path, previous)| {
                let current = file_stamp(path);
                // Without Unix file identity, reread content to catch same-mtime replacements.
                let changed = !cfg!(unix) || *previous != current;
                *previous = current;
                changed.then(|| path.clone())
            })
            .collect()
    }

    async fn debounced_changes(&mut self) -> Vec<PathBuf> {
        let mut changes = self.changed_paths();
        if !changes.is_empty() {
            tokio::time::sleep(Duration::from_millis(750)).await;
            changes.extend(self.changed_paths());
        }
        changes
    }
}

fn file_stamp(path: &Path) -> Option<Fingerprint> {
    Fingerprint::of(&std::fs::metadata(path).ok()?).ok()
}

/// Snapshot of a watched file's MCP-relevant content. Kept as a single-variant
/// enum (rather than a bare `String`) so a future watched-file kind can be added
/// without rippling the `Option<FileSnapshot>` plumbing. `.credentials.json` used
/// to add a second variant; it was dropped in #3554.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileSnapshot {
    Config(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpConfigDiff {
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<String>,
}

impl McpConfigDiff {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// One human-readable line describing what changed in a watched file. Single
/// variant since #3554 (the `.credentials.json` "created/updated/removed" variant
/// was removed); kept as an enum to leave room for future summary kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileChangeSummary {
    Config { file: String, diff: McpConfigDiff },
}

impl FileChangeSummary {
    fn render(&self) -> String {
        let Self::Config { file, diff } = self;
        let mut parts = Vec::new();
        if !diff.added.is_empty() {
            parts.push(prefix_names("+", &diff.added));
        }
        if !diff.removed.is_empty() {
            parts.push(prefix_names("-", &diff.removed));
        }
        if !diff.changed.is_empty() {
            parts.push(prefix_names("변경 ", &diff.changed));
        }
        format!("{file}: {}", parts.join(", "))
    }
}

fn prefix_names(prefix: &str, names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("{prefix}{name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn snapshot_existing_files(paths: &[PathBuf]) -> HashMap<PathBuf, Option<FileSnapshot>> {
    paths
        .iter()
        .map(|path| (path.clone(), snapshot_file(path).unwrap_or(None)))
        .collect()
}

fn snapshot_file(path: &Path) -> std::io::Result<Option<FileSnapshot>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(FileSnapshot::Config(canonical_mcp_config(&contents)))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn canonical_mcp_config(contents: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(contents) else {
        return "{}".to_string();
    };
    let entries = extract_mcp_server_entries(&value);
    serde_json::to_string(&entries).unwrap_or_else(|_| "{}".to_string())
}

fn extract_mcp_server_entries(value: &Value) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();
    for key in ["mcpServers", "mcp_servers"] {
        let Some(servers) = value.get(key).and_then(Value::as_object) else {
            continue;
        };
        for (name, server_config) in servers {
            let rendered =
                serde_json::to_string(server_config).unwrap_or_else(|_| "null".to_string());
            entries.insert(name.clone(), rendered);
        }
    }
    entries
}

fn parse_config_snapshot(snapshot: Option<&FileSnapshot>) -> BTreeMap<String, String> {
    let Some(FileSnapshot::Config(config)) = snapshot else {
        return BTreeMap::new();
    };
    serde_json::from_str::<BTreeMap<String, String>>(config).unwrap_or_default()
}

fn diff_mcp_config(
    previous: Option<&FileSnapshot>,
    current: Option<&FileSnapshot>,
) -> McpConfigDiff {
    let previous = parse_config_snapshot(previous);
    let current = parse_config_snapshot(current);
    let previous_names = previous.keys().cloned().collect::<BTreeSet<_>>();
    let current_names = current.keys().cloned().collect::<BTreeSet<_>>();

    let added = current_names
        .difference(&previous_names)
        .cloned()
        .collect::<Vec<_>>();
    let removed = previous_names
        .difference(&current_names)
        .cloned()
        .collect::<Vec<_>>();
    let changed = current_names
        .intersection(&previous_names)
        .filter(|name| previous.get(*name) != current.get(*name))
        .cloned()
        .collect::<Vec<_>>();

    McpConfigDiff {
        added,
        removed,
        changed,
    }
}

fn summarize_change(
    path: &Path,
    previous: Option<&FileSnapshot>,
    current: Option<&FileSnapshot>,
) -> Option<FileChangeSummary> {
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp config")
        .to_string();
    let diff = diff_mcp_config(previous, current);
    if diff.is_empty() {
        return None;
    }
    Some(FileChangeSummary::Config { file, diff })
}

fn build_notification_from_changes(
    changes: &[PathBuf],
    snapshots: &mut HashMap<PathBuf, Option<FileSnapshot>>,
    notify_message: &str,
) -> Option<String> {
    let changed_paths = changes.iter().cloned().collect::<BTreeSet<_>>();

    let mut summaries = Vec::new();
    for path in changed_paths {
        let previous = snapshots.get(&path).cloned().flatten();
        let current = snapshot_file(&path).unwrap_or(None);
        snapshots.insert(path.clone(), current.clone());
        if let Some(summary) = summarize_change(&path, previous.as_ref(), current.as_ref()) {
            summaries.push(summary.render());
        }
    }

    if summaries.is_empty() {
        None
    } else {
        Some(format!("{notify_message} ({})", summaries.join("; ")))
    }
}

/// In-memory per-channel cooldown tracker — skip notifying a channel if it was
/// notified within the dedupe window.
#[derive(Default)]
pub(super) struct CredentialNotifyDedupe {
    last_notified: DashMap<ChannelId, Instant>,
}

impl CredentialNotifyDedupe {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Returns true if the given channel may be notified now (and records the
    /// timestamp). Returns false if a previous notification is still inside the
    /// dedupe window.
    pub(super) fn try_record(&self, channel_id: ChannelId, window: Duration) -> bool {
        self.try_record_at(channel_id, window, Instant::now())
    }

    /// Test-friendly variant — caller supplies the "now" timestamp so the dedupe
    /// window can be exercised deterministically.
    pub(super) fn try_record_at(
        &self,
        channel_id: ChannelId,
        window: Duration,
        now: Instant,
    ) -> bool {
        let mut allowed = true;
        self.last_notified
            .entry(channel_id)
            .and_modify(|last| {
                if now.saturating_duration_since(*last) < window {
                    allowed = false;
                } else {
                    *last = now;
                }
            })
            .or_insert(now);
        allowed
    }
}

/// Resolve the MCP config files we should watch. Returns the (existing-or-not)
/// candidate paths so that creation events are also picked up.
pub(super) fn credential_paths() -> Vec<PathBuf> {
    if crate::config::test_instance::requested() {
        return Vec::new();
    }
    let override_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    credential_paths_with_overrides(override_dir, dirs::home_dir())
}

/// Pure helper for tests. Honors `CLAUDE_CONFIG_DIR` when present; otherwise
/// falls back to the user's home directory. Returns the MCP config files under
/// the Claude config root. The OAuth token file (`.claude/.credentials.json`) is
/// intentionally excluded — see the module-level docs (#3554).
pub(super) fn credential_paths_with_overrides(
    override_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Vec<PathBuf> {
    // CLAUDE_CONFIG_DIR semantics (per Claude Code docs): when set, all config
    // files normally rooted at $HOME live under that directory instead.
    let base = match override_dir.or(home) {
        Some(base) => base,
        None => return Vec::new(),
    };
    let claude_subdir = base.join(".claude");
    vec![base.join(".claude.json"), claude_subdir.join(".mcp.json")]
}

/// Spawn the credential watcher. Runs forever in a dedicated background task.
/// Safe to call once per Claude bot startup.
pub(super) fn spawn_watcher(
    shared: Arc<SharedData>,
    dedupe_window: Duration,
    notify_message: &'static str,
) {
    let paths = credential_paths();
    if paths.is_empty() {
        tracing::warn!(
            "MCP credential watcher: could not resolve home directory; watcher disabled"
        );
        return;
    }
    let dedupe = Arc::new(CredentialNotifyDedupe::new());
    tokio::spawn(async move {
        // Poll only the candidate files, including missing paths; never subscribe to their parents.
        let mut poller = ConfigFilePoller::new(&paths);
        let mut snapshots = snapshot_existing_files(&paths);
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let changes = poller.debounced_changes().await;
            if changes.is_empty() {
                continue;
            }
            match build_notification_from_changes(&changes, &mut snapshots, notify_message) {
                Some(notification) => {
                    tracing::info!(
                        "MCP credential change detected — notifying active Claude sessions"
                    );
                    broadcast_credential_change(&shared, &dedupe, dedupe_window, &notification)
                        .await;
                }
                None => {
                    tracing::debug!(
                        "MCP config file touched but no mcpServers change — skipping broadcast"
                    );
                }
            }
        }
    });
}

async fn broadcast_credential_change(
    shared: &Arc<SharedData>,
    dedupe: &Arc<CredentialNotifyDedupe>,
    window: Duration,
    notify_message: &str,
) {
    if !shared.has_runtime_storage() {
        tracing::debug!("MCP credential watcher: no DB or PG handle, skipping broadcast");
        return;
    }

    // Snapshot the active session channel set under a single lock.
    let channels: Vec<ChannelId> = {
        let data = shared.core.lock().await;
        data.sessions.keys().copied().collect()
    };

    let mut delivered = 0usize;
    let mut suppressed = 0usize;
    for channel_id in channels {
        if !dedupe.try_record(channel_id, window) {
            suppressed += 1;
            continue;
        }
        let target = format!("channel:{}", channel_id.get());
        crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
            shared.pg_pool.as_ref(),
            &target,
            None,
            "lifecycle.mcp_credential_change",
            notify_message,
        );
        delivered += 1;
    }
    tracing::info!(
        "MCP credential watcher: broadcast complete (delivered={delivered}, dedupe-suppressed={suppressed})"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_instance_mcp_watcher_registers_no_credential_paths() {
        crate::services::provider_auth_profile::instance_env_tests::with_instance(
            concat!(
                module_path!(),
                "::test_instance_mcp_watcher_registers_no_credential_paths"
            ),
            || assert!(credential_paths().is_empty()),
        );
    }

    fn config_snapshot(json: &str) -> FileSnapshot {
        FileSnapshot::Config(canonical_mcp_config(json))
    }

    #[test]
    fn watched_paths_exclude_oauth_token_file() {
        // #3554: the OAuth token file is rewritten on every access-token refresh and
        // carries no MCP signal, so it must not be watched.
        let home = PathBuf::from("/home/u");
        let paths = credential_paths_with_overrides(None, Some(home.clone()));
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&home.join(".claude.json")), "{paths:?}");
        assert!(
            paths.contains(&home.join(".claude").join(".mcp.json")),
            "{paths:?}"
        );
        assert!(
            !paths.contains(&home.join(".claude").join(".credentials.json")),
            "token file must not be watched: {paths:?}"
        );
    }

    #[test]
    fn claude_config_dir_override_reroots_watched_paths() {
        let paths = credential_paths_with_overrides(
            Some(PathBuf::from("/cfg")),
            Some(PathBuf::from("/home/u")),
        );
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|p| p.starts_with("/cfg")), "{paths:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn poll_only_config_files_detects_lifecycle_and_coalesces_edits() {
        let root = tempfile::tempdir().unwrap();
        let paths = credential_paths_with_overrides(Some(root.path().into()), None);
        let mut poller = ConfigFilePoller::new(&paths);
        let mut targets = poller.stamps.keys().cloned().collect::<Vec<_>>();
        targets.sort();
        let mut expected = paths.clone();
        expected.sort();
        assert_eq!(targets, expected);
        let mut snapshots = snapshot_existing_files(&paths);
        std::fs::create_dir_all(root.path().join(".claude")).unwrap();
        for path in &paths {
            let name = path.file_name().unwrap().to_str().unwrap();
            let original = r#"{"mcpServers":{"memento":{"command":"one"}}}"#;
            std::fs::write(path, original).unwrap();
            let changes = poller.debounced_changes().await;
            assert_eq!(
                build_notification_from_changes(&changes, &mut snapshots, "restart"),
                Some(format!("restart ({name}: +memento)"))
            );
            let mtime =
                filetime::FileTime::from_last_modification_time(&std::fs::metadata(path).unwrap());
            for (command, replace) in [("two", false), ("tri", true)] {
                let contents = original.replace("one", command);
                if replace {
                    let replacement = root.path().join("replacement.json");
                    std::fs::write(&replacement, &contents).unwrap();
                    filetime::set_file_mtime(&replacement, mtime).unwrap();
                    std::fs::rename(replacement, path).unwrap();
                } else {
                    std::fs::write(path, &contents).unwrap();
                    filetime::set_file_mtime(path, mtime).unwrap();
                }
                let changes = poller.debounced_changes().await;
                assert_eq!(
                    build_notification_from_changes(&changes, &mut snapshots, "restart"),
                    Some(format!("restart ({name}: 변경 memento)"))
                );
            }
            std::fs::remove_file(path).unwrap();
            let changes = poller.debounced_changes().await;
            assert_eq!(
                build_notification_from_changes(&changes, &mut snapshots, "restart"),
                Some(format!("restart ({name}: -memento)"))
            );
        }
        std::fs::write(&paths[0], r#"{"mcp_servers":{"first":{}}}"#).unwrap();
        let burst = tokio::spawn(async move { poller.debounced_changes().await });
        tokio::task::yield_now().await;
        std::fs::write(&paths[1], r#"{"mcpServers":{"second":{}}}"#).unwrap();
        assert_eq!(
            build_notification_from_changes(&burst.await.unwrap(), &mut snapshots, "restart"),
            Some("restart (.mcp.json: +second; .claude.json: +first)".into())
        );
        let mut poller = ConfigFilePoller::new(&paths);
        std::fs::write(root.path().join("unrelated.json"), "{}").unwrap();
        std::fs::write(&paths[0], r#"{"theme":"dark","mcp_servers":{"first":{}}}"#).unwrap();
        let changes = poller.debounced_changes().await;
        assert!(build_notification_from_changes(&changes, &mut snapshots, "restart").is_none());
    }

    #[test]
    fn dedupe_preserves_channel_scope_and_exact_window_boundary() {
        let dedupe = CredentialNotifyDedupe::new();
        let window = Duration::from_secs(300);
        let now = Instant::now();
        let first = ChannelId::new(1);
        assert!(dedupe.try_record_at(first, window, now));
        assert!(!dedupe.try_record_at(first, window, now + window - Duration::from_nanos(1)));
        assert!(dedupe.try_record_at(ChannelId::new(2), window, now));
        assert!(dedupe.try_record_at(first, window, now + window));
        assert!(!dedupe.try_record_at(first, window, now + window));
    }

    #[test]
    fn added_mcp_server_is_reported() {
        let prev = config_snapshot(r#"{"mcpServers":{}}"#);
        let curr = config_snapshot(r#"{"mcpServers":{"memento":{"command":"x"}}}"#);
        let summary = summarize_change(
            &PathBuf::from("/home/u/.claude.json"),
            Some(&prev),
            Some(&curr),
        )
        .expect("adding a server should notify");
        assert_eq!(summary.render(), ".claude.json: +memento");
    }

    #[test]
    fn removed_mcp_server_is_reported() {
        let prev = config_snapshot(r#"{"mcpServers":{"memento":{"command":"x"}}}"#);
        let curr = config_snapshot(r#"{"mcpServers":{}}"#);
        let summary = summarize_change(
            &PathBuf::from("/home/u/.claude.json"),
            Some(&prev),
            Some(&curr),
        )
        .expect("removing a server should notify");
        assert_eq!(summary.render(), ".claude.json: -memento");
    }

    #[test]
    fn changed_mcp_server_config_is_reported() {
        let prev = config_snapshot(r#"{"mcpServers":{"memento":{"command":"old"}}}"#);
        let curr = config_snapshot(r#"{"mcpServers":{"memento":{"command":"new"}}}"#);
        let summary = summarize_change(
            &PathBuf::from("/home/u/.claude.json"),
            Some(&prev),
            Some(&curr),
        )
        .expect("editing a server config should notify");
        assert_eq!(summary.render(), ".claude.json: 변경 memento");
    }

    #[test]
    fn unchanged_mcp_servers_produce_no_summary() {
        let snap = config_snapshot(r#"{"mcpServers":{"memento":{"command":"x"}}}"#);
        let same = snap.clone();
        assert!(
            summarize_change(
                &PathBuf::from("/home/u/.claude.json"),
                Some(&snap),
                Some(&same)
            )
            .is_none()
        );
    }

    #[test]
    fn token_file_rotation_produces_no_summary() {
        // Even if a stray event for a token-shaped JSON reaches summarize_change, it
        // has no mcpServers, so a rewrite (refresh) canonicalizes identically -> silence.
        let before = config_snapshot(r#"{"claudeAiOauth":{"accessToken":"old"}}"#);
        let after = config_snapshot(r#"{"claudeAiOauth":{"accessToken":"new"}}"#);
        assert_eq!(before, after);
        assert!(
            summarize_change(
                &PathBuf::from("/home/u/.claude/.credentials.json"),
                Some(&before),
                Some(&after),
            )
            .is_none()
        );
    }
}
