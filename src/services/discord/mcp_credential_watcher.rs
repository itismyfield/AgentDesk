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
    if shared.sqlite.is_none() && shared.pg_pool.is_none() {
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
            shared.sqlite.as_ref()
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

fn build_notification_from_changes(
    changes: &[CredentialChange],
    snapshots: &mut HashMap<PathBuf, FileSnapshot>,
    fallback_message: &str,
) -> Option<String> {
    let mut latest_by_path = HashMap::<PathBuf, CredentialChange>::new();
    for change in changes {
        latest_by_path
            .entry(change.path.clone())
            .and_modify(|existing| {
                if change.timestamp > existing.timestamp {
                    *existing = change.clone();
                }
            })
            .or_insert_with(|| change.clone());
    }

    let mut changes_by_path: Vec<CredentialChange> = latest_by_path.into_values().collect();
    changes_by_path.sort_by(|a, b| file_label(&a.path).cmp(&file_label(&b.path)));

    let mut summaries = Vec::new();
    let mut needs_fallback = false;
    for change in changes_by_path {
        let previous = snapshots.get(&change.path).cloned();
        let current = match snapshot_file(&change.path) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::warn!(
                    "MCP credential watcher: failed to snapshot {} after {:?}: {err}",
                    change.path.display(),
                    change.kind
                );
                needs_fallback = true;
                continue;
            }
        };

        match previous {
            None => {
                let summary = match summarize_initial_create(
                    &change.path,
                    &change.kind,
                    current.as_ref(),
                ) {
                    Ok(summary) => summary,
                    Err(err) => {
                        tracing::warn!(
                            "MCP credential watcher: failed to summarize first observation for {} after {:?}: {err}",
                            change.path.display(),
                            change.kind
                        );
                        needs_fallback = true;
                        None
                    }
                };

                if let Some(snapshot) = current {
                    snapshots.insert(change.path.clone(), snapshot);
                }

                if let Some(summary) = summary {
                    summaries.push(summary);
                } else if matches!(change.kind, EventKind::Create(_)) {
                    needs_fallback = true;
                }
            }
            Some(previous_snapshot) => {
                let summary = match summarize_change(
                    &change.path,
                    &change.kind,
                    &previous_snapshot,
                    current.as_ref(),
                ) {
                    Ok(summary) => summary,
                    Err(err) => {
                        tracing::warn!(
                            "MCP credential watcher: failed to summarize {} after {:?}: {err}",
                            change.path.display(),
                            change.kind
                        );
                        needs_fallback = true;
                        None
                    }
                };

                if let Some(snapshot) = current {
                    snapshots.insert(change.path.clone(), snapshot);
                } else {
                    snapshots.remove(&change.path);
                }

                if let Some(summary) = summary {
                    summaries.push(summary);
                } else {
                    needs_fallback = true;
                }
            }
        }
    }

    if summaries.is_empty() {
        needs_fallback.then(|| fallback_message.to_string())
    } else {
        Some(format!(
            "🔔 MCP 변화 감지: {}. `/mcp-reload` 로 적용.",
            summaries
                .iter()
                .map(FileChangeSummary::render)
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }
}

fn summarize_initial_create(
    path: &Path,
    kind: &EventKind,
    current: Option<&FileSnapshot>,
) -> Result<Option<FileChangeSummary>, String> {
    match current {
        Some(FileSnapshot::Config(contents)) => {
            let diff = diff_mcp_servers("{}", Some(contents))?;
            if diff.is_empty() {
                Ok(None)
            } else {
                Ok(Some(FileChangeSummary::Config {
                    file: file_label(path),
                    diff,
                }))
            }
        }
        Some(FileSnapshot::CredentialsPresent) => Ok(Some(FileChangeSummary::Credentials {
            file: file_label(path),
            detail: match kind {
                EventKind::Remove(_) => "OAuth 토큰 파일 제거",
                EventKind::Create(_) => "OAuth 토큰 파일 생성",
                _ => "OAuth 토큰 갱신",
            },
        })),
        None => Ok(None),
    }
}

fn snapshot_existing_files(paths: &[PathBuf]) -> HashMap<PathBuf, FileSnapshot> {
    let mut snapshots = HashMap::new();
    for path in paths {
        match snapshot_file(path) {
            Ok(Some(snapshot)) => {
                snapshots.insert(path.clone(), snapshot);
            }
            Ok(None) => {}
            Err(err) => tracing::debug!(
                "MCP credential watcher: could not snapshot {} at startup: {err}",
                path.display()
            ),
        }
    }
    snapshots
}

fn snapshot_file(path: &Path) -> std::io::Result<Option<FileSnapshot>> {
    if is_credentials_file(path) {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(Some(FileSnapshot::CredentialsPresent)),
            Ok(_) => Ok(None),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    } else {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(Some(FileSnapshot::Config(contents))),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

fn summarize_change(
    path: &Path,
    kind: &EventKind,
    previous: &FileSnapshot,
    current: Option<&FileSnapshot>,
) -> Result<Option<FileChangeSummary>, String> {
    match previous {
        FileSnapshot::Config(previous_contents) => {
            let current_contents = match current {
                Some(FileSnapshot::Config(contents)) => Some(contents.as_str()),
                Some(FileSnapshot::CredentialsPresent) => {
                    return Err("snapshot kind changed unexpectedly".into());
                }
                None => None,
            };
            let diff = diff_mcp_servers(previous_contents, current_contents)?;
            if diff.is_empty() {
                Ok(None)
            } else {
                Ok(Some(FileChangeSummary::Config {
                    file: file_label(path),
                    diff,
                }))
            }
        }
        FileSnapshot::CredentialsPresent => Ok(Some(FileChangeSummary::Credentials {
            file: file_label(path),
            detail: match kind {
                EventKind::Create(_) => "OAuth 토큰 파일 생성",
                EventKind::Remove(_) => "OAuth 토큰 파일 제거",
                _ => "OAuth 토큰 갱신",
            },
        })),
    }
}

fn diff_mcp_servers(previous: &str, current: Option<&str>) -> Result<McpConfigDiff, String> {
    let previous_servers = parse_mcp_servers(previous)?;
    let current_servers = match current {
        Some(current) => parse_mcp_servers(current)?,
        None => HashMap::new(),
    };

    let previous_names: BTreeSet<String> = previous_servers.keys().cloned().collect();
    let current_names: BTreeSet<String> = current_servers.keys().cloned().collect();

    let added = current_names.difference(&previous_names).cloned().collect();
    let removed = previous_names.difference(&current_names).cloned().collect();
    let changed = previous_names
        .intersection(&current_names)
        .filter_map(|name| {
            (previous_servers.get(name) != current_servers.get(name)).then(|| name.clone())
        })
        .collect();

    Ok(McpConfigDiff {
        added,
        removed,
        changed,
    })
}

fn parse_mcp_servers(contents: &str) -> Result<HashMap<String, Value>, String> {
    let root: Value = serde_json::from_str(contents).map_err(|err| err.to_string())?;
    let Some(mcp_servers) = root.get("mcpServers") else {
        return Ok(HashMap::new());
    };
    let Some(servers) = mcp_servers.as_object() else {
        return Err("mcpServers must be a JSON object".into());
    };

    Ok(servers
        .iter()
        .map(|(name, value)| (name.clone(), redact_sensitive_fields(value)))
        .collect())
}

fn redact_sensitive_fields(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(redact_sensitive_fields).collect()),
        Value::Object(map) => {
            let mut redacted = serde_json::Map::new();
            for (key, value) in map {
                if is_sensitive_config_key(key) {
                    continue;
                }
                redacted.insert(key.clone(), redact_sensitive_fields(value));
            }
            Value::Object(redacted)
        }
        _ => value.clone(),
    }
}

fn is_sensitive_config_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "env"
            | "headers"
            | "authorization"
            | "access_token"
            | "accesstoken"
            | "refresh_token"
            | "refreshtoken"
            | "token"
            | "api_key"
            | "apikey"
            | "bearer_token"
            | "bearertoken"
    )
}

fn is_credentials_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".credentials.json")
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn prefix_names(prefix: &str, names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("{prefix}{name}"))
        .collect::<Vec<_>>()
        .join(", ")
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

    #[test]
    fn diff_mcp_servers_reports_added_removed_and_changed_names_without_secret_values() {
        let previous = r#"{
            "mcpServers": {
                "memento": {
                    "transport": "stdio",
                    "command": "memento",
                    "env": { "MEMENTO_TOKEN": "old-secret" }
                },
                "slack": {
                    "transport": "http",
                    "url": "https://old.example.com",
                    "headers": { "Authorization": "Bearer old-token" }
                }
            }
        }"#;
        let current = r#"{
            "mcpServers": {
                "slack": {
                    "transport": "http",
                    "url": "https://new.example.com",
                    "headers": { "Authorization": "Bearer new-token" }
                },
                "brave-search": {
                    "transport": "stdio",
                    "command": "brave"
                }
            }
        }"#;

        let diff = diff_mcp_servers(previous, Some(current)).unwrap();
        assert_eq!(diff.added, vec!["brave-search".to_string()]);
        assert_eq!(diff.removed, vec!["memento".to_string()]);
        assert_eq!(diff.changed, vec!["slack".to_string()]);
    }

    #[test]
    fn first_created_file_emits_notification_and_snapshots() {
        let temp = tempdir().unwrap();
        let config_path = temp.path().join(".claude.json");
        let mut snapshots = HashMap::new();
        let changes = vec![CredentialChange {
            path: config_path.clone(),
            kind: EventKind::Create(CreateKind::File),
            timestamp: SystemTime::now(),
        }];

        std::fs::write(
            &config_path,
            r#"{"mcpServers":{"memento":{"transport":"stdio","command":"memento"}}}"#,
        )
        .unwrap();

        let message =
            build_notification_from_changes(&changes, &mut snapshots, "fallback notification");

        let message = message.unwrap();
        assert!(message.contains(".claude.json: +memento"));
        assert!(matches!(
            snapshots.get(&config_path),
            Some(FileSnapshot::Config(contents))
                if contents.contains("\"memento\"")
        ));
    }

    #[test]
    fn credentials_update_notification_never_includes_file_contents() {
        let temp = tempdir().unwrap();
        let credentials_path = temp.path().join(".credentials.json");
        std::fs::write(
            &credentials_path,
            r#"{"memento":{"access_token":"super-secret-token"}}"#,
        )
        .unwrap();

        let mut snapshots = snapshot_existing_files(std::slice::from_ref(&credentials_path));
        std::fs::write(
            &credentials_path,
            r#"{"memento":{"access_token":"even-more-secret-token"}}"#,
        )
        .unwrap();

        let changes = vec![CredentialChange {
            path: credentials_path,
            kind: EventKind::Modify(ModifyKind::Any),
            timestamp: SystemTime::now(),
        }];

        let message =
            build_notification_from_changes(&changes, &mut snapshots, "fallback notification")
                .unwrap();

        assert!(message.contains(".credentials.json: OAuth 토큰 갱신"));
        assert!(!message.contains("super-secret-token"));
        assert!(!message.contains("even-more-secret-token"));
    }

    #[test]
    fn unchanged_mcp_server_diff_falls_back_to_static_notification() {
        let temp = tempdir().unwrap();
        let config_path = temp.path().join(".claude.json");
        std::fs::write(
            &config_path,
            r#"{"mcpServers":{"memento":{"transport":"stdio","env":{"TOKEN":"secret"}}}}"#,
        )
        .unwrap();
        let mut snapshots = snapshot_existing_files(std::slice::from_ref(&config_path));
        std::fs::write(
            &config_path,
            r#"{"mcpServers":{"memento":{"transport":"stdio","env":{"TOKEN":"rotated-secret"}}}}"#,
        )
        .unwrap();

        let changes = vec![CredentialChange {
            path: config_path,
            kind: EventKind::Modify(ModifyKind::Any),
            timestamp: SystemTime::now(),
        }];

        let message =
            build_notification_from_changes(&changes, &mut snapshots, "fallback notification")
                .unwrap();

        assert_eq!(message, "fallback notification");
    }

    #[test]
    fn mixed_burst_keeps_later_summaries_and_updates_snapshots_after_fallback_file() {
        let temp = tempdir().unwrap();
        let config_path = temp.path().join(".claude.json");
        let credentials_path = temp.path().join(".credentials.json");
        std::fs::write(
            &config_path,
            r#"{"mcpServers":{"memento":{"transport":"stdio","env":{"TOKEN":"secret"}}}}"#,
        )
        .unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"memento":{"access_token":"old-secret-token"}}"#,
        )
        .unwrap();

        let mut snapshots =
            snapshot_existing_files(&[config_path.clone(), credentials_path.clone()]);

        std::fs::write(
            &config_path,
            r#"{"mcpServers":{"memento":{"transport":"stdio","env":{"TOKEN":"rotated-secret"}}}}"#,
        )
        .unwrap();
        std::fs::write(
            &credentials_path,
            r#"{"memento":{"access_token":"new-secret-token"}}"#,
        )
        .unwrap();

        let changes = vec![
            CredentialChange {
                path: config_path.clone(),
                kind: EventKind::Modify(ModifyKind::Any),
                timestamp: SystemTime::now(),
            },
            CredentialChange {
                path: credentials_path.clone(),
                kind: EventKind::Modify(ModifyKind::Any),
                timestamp: SystemTime::now(),
            },
        ];

        let message =
            build_notification_from_changes(&changes, &mut snapshots, "fallback notification")
                .unwrap();

        assert!(message.contains(".credentials.json: OAuth 토큰 갱신"));
        assert!(!message.contains("fallback notification"));
        assert!(matches!(
            snapshots.get(&config_path),
            Some(FileSnapshot::Config(contents))
                if contents.contains("rotated-secret")
        ));
        assert!(matches!(
            snapshots.get(&credentials_path),
            Some(FileSnapshot::CredentialsPresent)
        ));
    }

    #[test]
    fn change_summary_render_includes_file_and_server_names() {
        let summary = FileChangeSummary::Config {
            file: ".claude.json".into(),
            diff: super::McpConfigDiff {
                added: vec!["brave-search".into()],
                removed: vec!["memento".into()],
                changed: vec!["slack".into()],
            },
        };

        assert_eq!(
            summary.render(),
            ".claude.json: +brave-search, -memento, 변경 slack"
        );
    }
}
