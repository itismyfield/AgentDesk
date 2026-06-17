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
use std::time::{Duration, Instant, SystemTime};

use dashmap::DashMap;
use notify::EventKind;
use poise::serenity_prelude::ChannelId;
use serde_json::Value;
use tokio::sync::mpsc;

use super::SharedData;

#[derive(Debug, Clone)]
struct CredentialChange {
    path: PathBuf,
    kind: EventKind,
    timestamp: SystemTime,
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
    changes: &[CredentialChange],
    snapshots: &mut HashMap<PathBuf, Option<FileSnapshot>>,
    notify_message: &str,
) -> Option<String> {
    let mut changed_paths = BTreeSet::new();
    for change in changes {
        let _ = &change.kind;
        let _ = change.timestamp;
        changed_paths.insert(change.path.clone());
    }

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
    // Collect every distinct parent directory so we can watch each one non-recursively
    // (one watch per dir). Some watched files live at $HOME (.claude.json) and others
    // under $HOME/.claude/, so we may have multiple parents.
    let watch_dirs: Vec<PathBuf> = {
        let mut dedup: Vec<PathBuf> = paths
            .iter()
            .filter_map(|p| p.parent().map(PathBuf::from))
            .collect();
        dedup.sort();
        dedup.dedup();
        dedup
    };
    if watch_dirs.is_empty() {
        tracing::warn!(
            "MCP credential watcher: no parent dirs derived from credential paths; disabled"
        );
        return;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<CredentialChange>();
    let watch_dirs_for_thread = watch_dirs.clone();
    let paths_for_filter = paths.clone();
    let paths_for_snapshots = paths.clone();

    // Notify watcher runs on its own OS thread (the notify crate uses sync callbacks).
    std::thread::Builder::new()
        .name("mcp-credential-watcher".into())
        .spawn(move || {
            use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

            let watcher_tx = tx.clone();
            let mut watcher: RecommendedWatcher =
                match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    let Ok(event) = res else {
                        return;
                    };
                    if !matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    ) {
                        return;
                    }
                    let changed_paths = event
                        .paths
                        .iter()
                        .filter_map(|path| {
                            paths_for_filter.iter().find(|target| {
                                path.file_name() == target.file_name()
                                    && path.parent() == target.parent()
                            })
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    for path in changed_paths {
                        let _ = watcher_tx.send(CredentialChange {
                            path,
                            kind: event.kind.clone(),
                            timestamp: SystemTime::now(),
                        });
                    }
                }) {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::warn!("MCP credential watcher: failed to create watcher: {e}");
                        return;
                    }
                };

            // Try to attach watches to each parent dir. For dirs that do not yet
            // exist, retry every 30s until they appear; this avoids the previous
            // bug where missing-at-startup dirs were silently never watched.
            let mut pending: Vec<PathBuf> = watch_dirs_for_thread.clone();
            loop {
                let mut still_pending = Vec::new();
                for dir in pending.drain(..) {
                    if !dir.exists() {
                        still_pending.push(dir);
                        continue;
                    }
                    match watcher.watch(&dir, RecursiveMode::NonRecursive) {
                        Ok(()) => {
                            tracing::info!("MCP credential watcher: watching {}", dir.display())
                        }
                        Err(e) => tracing::warn!(
                            "MCP credential watcher: cannot watch {}: {e}",
                            dir.display()
                        ),
                    }
                }
                if still_pending.is_empty() {
                    break;
                }
                pending = still_pending;
                tracing::debug!(
                    "MCP credential watcher: {} dir(s) not yet present; retrying in 30s",
                    pending.len()
                );
                std::thread::sleep(Duration::from_secs(30));
            }

            // Keep the watcher alive forever — it owns its own background thread.
            std::mem::forget(watcher);
        })
        .ok();

    // Async consumer: debounce + dispatch notifications.
    let dedupe = Arc::new(CredentialNotifyDedupe::new());
    tokio::spawn(async move {
        let mut snapshots = snapshot_existing_files(&paths_for_snapshots);
        let debounce = Duration::from_millis(750);
        loop {
            // Wait for an event.
            let Some(first_change) = rx.recv().await else {
                return;
            };
            // Drain any backlog inside the debounce window so we send at most one
            // notification per burst of file events (e.g. an editor rewriting both
            // config files in quick succession).
            tokio::time::sleep(debounce).await;
            let mut changes = vec![first_change];
            while let Ok(change) = rx.try_recv() {
                changes.push(change);
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
        let sqlite_runtime_db = if shared.pg_pool.is_some() {
            None
        } else {
            None::<&crate::db::Db>
        };
        crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
            sqlite_runtime_db,
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

    fn config_snapshot(json: &str) -> FileSnapshot {
        FileSnapshot::Config(canonical_mcp_config(json))
    }

    #[test]
    fn watched_paths_exclude_oauth_token_file() {
        // #3554: the OAuth token file is rewritten on every access-token refresh and
        // carries no MCP signal, so it must not be watched.
        let paths = credential_paths_with_overrides(None, Some(PathBuf::from("/home/u")));
        let rendered: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            rendered.iter().any(|p| p.ends_with("/home/u/.claude.json")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|p| p.ends_with(".claude/.mcp.json")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|p| p.ends_with(".credentials.json")),
            "token file must not be watched: {rendered:?}"
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
