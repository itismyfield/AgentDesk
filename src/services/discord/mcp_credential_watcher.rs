//! Watches the Claude CLI MCP credential / config files and posts a one-line
//! notification to every active Claude session when one of them changes.
//!
//! Watched paths (relative to `$CLAUDE_CONFIG_DIR` if set, otherwise `$HOME`):
//! - `.claude.json` — top-level Claude Code config (MCP server registrations live here per docs)
//! - `.claude/.mcp.json` — project-scoped MCP config (matches the existing repo lookup
//!   in `mcp_config::resolve_claude_user_mcp_config_path`)
//! - `.claude/.credentials.json` — auth tokens for OAuth-backed MCP servers (Linux-style;
//!   on macOS Claude Code stores OAuth tokens in Keychain, but we still watch the file
//!   in case it exists or gets created)
//!
//! Background: Claude CLI attaches MCP servers at boot and never hot-reloads
//! them. When the operator authenticates a new MCP server (e.g. memento)
//! mid-session, the running Claude process never sees the new tools. This
//! watcher tells operators that a `/mcp-reload` is needed.
//!
//! Per-channel dedupe is in-memory (HashMap<ChannelId, Instant>); we deliberately
//! do not persist this across restarts because the cooldown is short (5min default)
//! and re-notification after a restart is harmless.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use poise::serenity_prelude::ChannelId;
use tokio::sync::mpsc;

use super::SharedData;

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

/// Resolve the credential files we should watch. Returns the (existing-or-not)
/// candidate paths so that creation events are also picked up.
pub(super) fn credential_paths() -> Vec<PathBuf> {
    let override_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    credential_paths_with_overrides(override_dir, dirs::home_dir())
}

/// Pure helper for tests. Honors `CLAUDE_CONFIG_DIR` when present; otherwise
/// falls back to the user's home directory. Returns paths covering both the
/// top-level Claude config and the project/credentials files under `.claude/`.
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
    vec![
        base.join(".claude.json"),
        claude_subdir.join(".mcp.json"),
        claude_subdir.join(".credentials.json"),
    ]
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

    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let watch_dirs_for_thread = watch_dirs.clone();
    let paths_for_filter = paths.clone();

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
                    let touched_credential = event.paths.iter().any(|path| {
                        paths_for_filter.iter().any(|target| {
                            path.file_name() == target.file_name()
                                && path.parent() == target.parent()
                        })
                    });
                    if touched_credential {
                        let _ = watcher_tx.send(());
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
        let debounce = Duration::from_millis(750);
        loop {
            // Wait for an event.
            if rx.recv().await.is_none() {
                return;
            }
            // Drain any backlog inside the debounce window so we send at most one
            // notification per burst of file events (e.g. credential rotation that
            // touches both files in quick succession).
            tokio::time::sleep(debounce).await;
            while rx.try_recv().is_ok() {}

            tracing::info!("MCP credential change detected — notifying active Claude sessions");
            broadcast_credential_change(&shared, &dedupe, dedupe_window, notify_message).await;
        }
    });
}

async fn broadcast_credential_change(
    shared: &Arc<SharedData>,
    dedupe: &Arc<CredentialNotifyDedupe>,
    window: Duration,
    notify_message: &str,
) {
    let Some(db) = shared.db.as_ref() else {
        tracing::debug!("MCP credential watcher: no DB handle, skipping broadcast");
        return;
    };

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
        crate::services::message_outbox::enqueue_lifecycle_notification(
            db,
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
    use super::{CredentialNotifyDedupe, credential_paths_with_overrides};
    use poise::serenity_prelude::ChannelId;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    #[test]
    fn credential_paths_covers_top_level_config_and_dot_claude_files() {
        let home = PathBuf::from("/tmp/test-home");
        let paths = credential_paths_with_overrides(None, Some(home.clone()));
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0], home.join(".claude.json"));
        assert_eq!(paths[1], home.join(".claude").join(".mcp.json"));
        assert_eq!(paths[2], home.join(".claude").join(".credentials.json"));
    }

    #[test]
    fn credential_paths_honors_claude_config_dir_override() {
        let home = PathBuf::from("/tmp/test-home");
        let override_dir = PathBuf::from("/tmp/custom-claude");
        let paths = credential_paths_with_overrides(Some(override_dir.clone()), Some(home));
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0], override_dir.join(".claude.json"));
        assert_eq!(paths[1], override_dir.join(".claude").join(".mcp.json"));
        assert_eq!(
            paths[2],
            override_dir.join(".claude").join(".credentials.json")
        );
    }

    #[test]
    fn credential_paths_empty_when_no_home_and_no_override() {
        assert!(credential_paths_with_overrides(None, None).is_empty());
    }

    #[test]
    fn dedupe_allows_first_notification_then_blocks_within_window() {
        let dedupe = CredentialNotifyDedupe::new();
        let channel = ChannelId::new(42);
        let window = Duration::from_secs(300);
        let t0 = Instant::now();

        assert!(dedupe.try_record_at(channel, window, t0));
        // Same channel, 1 second later — still inside the 5-minute window.
        assert!(!dedupe.try_record_at(channel, window, t0 + Duration::from_secs(1)));
        // Same channel, 4 minutes later — still inside the window.
        assert!(!dedupe.try_record_at(channel, window, t0 + Duration::from_secs(240)));
    }

    #[test]
    fn dedupe_allows_again_after_window_elapses() {
        let dedupe = CredentialNotifyDedupe::new();
        let channel = ChannelId::new(42);
        let window = Duration::from_secs(300);
        let t0 = Instant::now();

        assert!(dedupe.try_record_at(channel, window, t0));
        // 5 minutes + 1 second later — window has elapsed.
        assert!(dedupe.try_record_at(channel, window, t0 + Duration::from_secs(301)));
    }

    #[test]
    fn dedupe_tracks_each_channel_independently() {
        let dedupe = CredentialNotifyDedupe::new();
        let window = Duration::from_secs(300);
        let t0 = Instant::now();

        assert!(dedupe.try_record_at(ChannelId::new(1), window, t0));
        // Different channel — must be allowed even at the same instant.
        assert!(dedupe.try_record_at(ChannelId::new(2), window, t0));
        // First channel still inside its own window.
        assert!(!dedupe.try_record_at(ChannelId::new(1), window, t0 + Duration::from_secs(10)));
    }
}
