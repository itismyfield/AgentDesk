use super::*;

use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::services::tmux_common::current_tmux_owner_marker;
use crate::services::tmux_diagnostics::{record_tmux_exit_reason, tmux_session_has_live_pane};

/// Kill orphan tmux sessions (AgentDesk-*) that don't map to any known channel.
/// Called after restore_tmux_watchers to clean up sessions from renamed/deleted channels.
pub(super) async fn cleanup_orphan_tmux_sessions(shared: &Arc<SharedData>) {
    let provider = shared.settings.read().await.provider.clone();
    let current_owner_marker = current_tmux_owner_marker();

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return,
    };

    let mut protected_dispatch_orphans = Vec::new();
    let orphans: Vec<String> = {
        let data = shared.core.lock().await;
        let mut result = Vec::new();

        for session_name in output.iter().map(|s| s.trim()) {
            let Some((session_provider, _)) =
                parse_provider_and_channel_from_tmux_name(session_name)
            else {
                continue;
            };
            if session_provider != provider {
                continue;
            }
            if !super::tmux::session_belongs_to_current_runtime(session_name, &current_owner_marker)
            {
                continue;
            }

            // Check if any active channel maps to this session
            let has_owner = data.sessions.iter().any(|(_, session)| {
                session
                    .channel_name
                    .as_ref()
                    .map(|ch_name| provider.build_tmux_session_name(ch_name) == session_name)
                    .unwrap_or(false)
            });

            if !has_owner {
                let parsed_channel_name = parse_provider_and_channel_from_tmux_name(session_name)
                    .as_ref()
                    .map(|(_, ch_name)| ch_name.clone());

                // #145: skip orphan cleanup for unified-thread sessions with active runs
                if let Some(ref ch_name) = parsed_channel_name
                    && crate::dispatch::is_unified_thread_channel_name_active(ch_name)
                {
                    continue;
                }

                // #181: Don't kill sessions with live processes in their pane.
                // During restart, dispatch threads may not yet be registered in
                // data.sessions (recover_orphan_pending_dispatches runs AFTER this).
                // A tmux pane with a running process is proof the session is in use.
                if tmux_session_has_live_pane(session_name) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}]   skipped orphan (live pane): {}", session_name);
                    continue;
                }

                if let Some(protection) = super::tmux_lifecycle::resolve_dispatch_tmux_protection(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    &provider,
                    session_name,
                    parsed_channel_name.as_deref(),
                ) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    if protection.active_dispatch_id().is_some() {
                        tracing::warn!(
                            "  [{ts}] orphan cleanup: active dispatch owns dead session {} — {}",
                            session_name,
                            protection.log_reason()
                        );
                        protected_dispatch_orphans
                            .push((session_name.to_string(), protection.clone()));
                    } else {
                        tracing::info!(
                            "  [{ts}] ♻ orphan cleanup: preserving dispatch session {} — {}",
                            session_name,
                            protection.log_reason()
                        );
                        continue;
                    }
                }

                result.push(session_name.to_string());
            }
        }

        result
    };

    for (session_name, protection) in &protected_dispatch_orphans {
        if super::tmux_lifecycle::fail_active_dispatch_for_dead_tmux_session(
            shared.api_port,
            protection,
            session_name,
            "orphan_cleanup",
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] orphan cleanup: failed active dispatch for dead session {} — {}",
                session_name,
                protection.log_reason()
            );
        }
    }

    if orphans.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🧹 Cleaning {} orphan tmux session(s)...",
        orphans.len()
    );

    for name in &orphans {
        let name_clone = name.clone();
        let killed = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                record_tmux_exit_reason(&name_clone, "orphan cleanup: no owning channel session");
                crate::services::platform::tmux::kill_session_with_reason(
                    &name_clone,
                    "orphan cleanup: no owning channel session",
                )
            }),
        )
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false);

        if killed {
            tracing::info!("  [{ts}]   killed orphan: {}", name);
            // Clean both persistent and legacy temp files.
            crate::services::tmux_common::cleanup_session_temp_files(name);
        }
    }
}

/// Periodically reap dead tmux sessions (pane_dead=1) that still have DB rows
/// showing working/dispatched status. This catches cases where the watcher
/// missed cleanup (e.g. crashed, or session died between watcher polls).
pub(super) async fn reap_dead_tmux_sessions(shared: &Arc<SharedData>) {
    let provider = shared.settings.read().await.provider.clone();
    let current_owner_marker = current_tmux_owner_marker();
    let api_port = shared.api_port;

    // List all tmux sessions
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(|| crate::services::platform::tmux::list_session_names()),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return,
    };

    let mut reaped = 0u32;

    for session_name in output.iter().map(|s| s.trim()) {
        let Some((session_provider, _)) = parse_provider_and_channel_from_tmux_name(session_name)
        else {
            continue;
        };
        if session_provider != provider {
            continue;
        }
        if !super::tmux::session_belongs_to_current_runtime(session_name, &current_owner_marker) {
            continue;
        }

        // Skip sessions that have a live pane (actually working)
        if tmux_session_has_live_pane(session_name) {
            continue;
        }

        // Skip sessions that already have an active watcher (watcher handles its own cleanup)
        let channel_id_for_session = {
            let data = shared.core.lock().await;
            data.sessions
                .iter()
                .find(|(_, s)| {
                    s.channel_name
                        .as_ref()
                        .map(|ch| provider.build_tmux_session_name(ch) == session_name)
                        .unwrap_or(false)
                })
                .map(|(ch, s)| (*ch, s.channel_name.clone()))
        };

        let Some((channel_id, channel_name)) = channel_id_for_session else {
            continue; // orphan — handled by cleanup_orphan_tmux_sessions
        };

        // If a watcher is attached, tmux liveness is the termination authority:
        // the watcher observes pane death, clears the registry, and applies the
        // same lifecycle/audit semantics as the live tail path.
        if shared.tmux_watchers.contains_key(&channel_id) {
            continue;
        }

        if let Some(protection) = super::tmux_lifecycle::resolve_dispatch_tmux_protection(
            None::<&crate::db::Db>,
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            &provider,
            session_name,
            channel_name.as_deref(),
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            if super::tmux_lifecycle::fail_active_dispatch_for_dead_tmux_session(
                api_port,
                &protection,
                session_name,
                "tmux_reaper",
            )
            .await
            {
                tracing::warn!(
                    "  [{ts}] reaper: failed active dispatch for dead session {} — {}",
                    session_name,
                    protection.log_reason()
                );
            } else {
                tracing::info!(
                    "  [{ts}] ♻ reaper: preserving dispatch session {} — {}",
                    session_name,
                    protection.log_reason()
                );
                continue;
            }
        }

        // Dead session with no watcher — report idle to DB and kill
        let tmux_name =
            provider.build_tmux_session_name(channel_name.as_deref().unwrap_or("unknown"));
        let session_key = super::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &provider,
            &tmux_name,
        );

        // Check if this is a thread session (channel name contains -t{15+digit})
        let is_thread = channel_name
            .as_deref()
            .and_then(|n| {
                let pos = n.rfind("-t")?;
                let suffix = &n[pos + 2..];
                (suffix.len() >= 15 && suffix.chars().all(|c| c.is_ascii_digit())).then_some(())
            })
            .is_some();

        // #145: unified-thread sessions should NOT be killed or deleted while
        // the auto-queue run is still active — mark idle instead and skip kill.
        let is_unified_active =
            is_thread && crate::dispatch::is_unified_thread_channel_active(channel_id.get());

        if is_thread && !is_unified_active {
            // Dead/orphan thread sessions: remove the DB row entirely.
            super::adk_session::delete_adk_session(&session_key, api_port).await;
        } else {
            // Fixed-channel sessions or active unified-thread: just mark idle
            super::adk_session::post_adk_session_status(
                Some(&session_key),
                channel_name.as_deref(),
                None,
                "idle",
                &provider,
                None,
                None,
                None,
                None,
                channel_name
                    .as_deref()
                    .and_then(super::adk_session::parse_thread_channel_id_from_name),
                Some(channel_id),
                None,
                api_port,
            )
            .await;
        }

        if is_unified_active {
            // Don't kill unified-thread sessions — they'll be reused
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻ reaper: skipping kill for unified-thread session {session_name} — run still active"
            );
            continue;
        }

        // Kill the dead tmux session
        let sess = session_name.to_string();
        let kill_result = tokio::task::spawn_blocking(move || {
            record_tmux_exit_reason(&sess, "reaper: dead session with no watcher");
            crate::services::platform::tmux::kill_session_output_with_reason(
                &sess,
                "reaper: dead session with no watcher",
            )
        })
        .await;
        match &kill_result {
            Ok(Ok(o)) if !o.status.success() => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ reaper: tmux kill-session failed for {session_name}: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            Ok(Err(e)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ reaper: tmux kill-session error for {session_name}: {e}"
                );
            }
            _ => {}
        }

        reaped += 1;
    }

    if reaped > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] 🪦 Reaped {reaped} dead tmux session(s)");
    }

    // #145: Process kill_unified_thread signals from auto-queue.js
    // When a unified-thread run completes, the JS policy writes a kv_meta flag
    // for us to pick up and kill the shared tmux session.
    process_unified_thread_kill_signals(shared).await;

    if matches!(provider, ProviderKind::Codex) {
        reap_orphan_codex_wrapper_processes().await;
    }
}

#[cfg(unix)]
async fn reap_orphan_codex_wrapper_processes() {
    let wrappers = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(list_orphan_codex_wrapper_processes),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();

    if wrappers.is_empty() {
        return;
    }

    let count = wrappers.len();
    let killed = tokio::task::spawn_blocking(move || {
        let mut killed = 0u32;
        for wrapper in wrappers {
            if wrapper.pid <= 0 || wrapper.pid as u32 == std::process::id() {
                continue;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 killing orphan codex wrapper pid={} session={}",
                wrapper.pid,
                wrapper.tmux_session_name.as_deref().unwrap_or("unknown")
            );
            crate::services::process::kill_pid_tree(wrapper.pid as u32);
            killed += 1;
        }
        killed
    })
    .await
    .unwrap_or(0);

    if killed > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] 🧹 Reaped {killed}/{count} orphan codex wrapper process(es)");
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanCodexWrapperProcess {
    pid: i32,
    tmux_session_name: Option<String>,
}

#[cfg(unix)]
fn list_orphan_codex_wrapper_processes() -> Vec<OrphanCodexWrapperProcess> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_orphan_codex_wrapper_process_line)
        .collect()
}

#[cfg(unix)]
fn parse_orphan_codex_wrapper_process_line(line: &str) -> Option<OrphanCodexWrapperProcess> {
    let trimmed = line.trim_start();
    let first_end = trimmed.find(char::is_whitespace)?;
    let pid = trimmed[..first_end].parse::<i32>().ok()?;
    let rest = trimmed[first_end..].trim_start();
    let second_end = rest.find(char::is_whitespace)?;
    let ppid = rest[..second_end].parse::<i32>().ok()?;
    let command = rest[second_end..].trim_start();

    if ppid != 1 || !command.contains(" codex-tmux-wrapper ") {
        return None;
    }

    // Restrict this reaper to legacy/orphaned wrappers. Current managed tmux
    // wrappers are owned by the tmux server and use persistent runtime paths.
    if !command.contains("--input-mode pipe")
        && !command.contains(".unused-fifo")
        && !command.contains("/var/folders/")
        && !command.contains("/tmp/")
    {
        return None;
    }

    Some(OrphanCodexWrapperProcess {
        pid,
        tmux_session_name: extract_tmux_session_name_from_wrapper_command(command),
    })
}

#[cfg(unix)]
fn extract_tmux_session_name_from_wrapper_command(command: &str) -> Option<String> {
    let output_path = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|window| (window[0] == "--output-file").then_some(window[1]))?;
    let basename = std::path::Path::new(output_path).file_name()?.to_str()?;
    let stem = basename.strip_suffix(".jsonl").unwrap_or(basename);
    let pos = stem.find("AgentDesk-")?;
    Some(stem[pos..].to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        extract_tmux_session_name_from_wrapper_command, parse_orphan_codex_wrapper_process_line,
    };

    #[test]
    fn extracts_tmux_session_name_from_legacy_output_path() {
        let command = "/Users/me/.adk/release/bin/agentdesk codex-tmux-wrapper \
            --output-file /var/folders/x/agentdesk-AgentDesk-codex-adk-cdx.jsonl \
            --input-fifo /var/folders/x/agentdesk-AgentDesk-codex-adk-cdx.unused-fifo";

        assert_eq!(
            extract_tmux_session_name_from_wrapper_command(command).as_deref(),
            Some("AgentDesk-codex-adk-cdx")
        );
    }

    #[test]
    fn parses_only_ppid_one_legacy_codex_wrappers() {
        let line = " 6260     1 /Users/me/.adk/release/bin/agentdesk codex-tmux-wrapper --output-file /var/folders/x/agentdesk-AgentDesk-codex-adk-cdx.jsonl --input-mode pipe";
        let parsed = parse_orphan_codex_wrapper_process_line(line).unwrap();
        assert_eq!(parsed.pid, 6260);
        assert_eq!(
            parsed.tmux_session_name.as_deref(),
            Some("AgentDesk-codex-adk-cdx")
        );

        let live_line = " 6261  6988 /Users/me/.adk/release/bin/agentdesk codex-tmux-wrapper --output-file /Users/me/.adk/release/runtime/sessions/host-AgentDesk-codex-adk-cdx.jsonl";
        assert!(parse_orphan_codex_wrapper_process_line(live_line).is_none());
    }
}

/// Kill tmux sessions flagged for cleanup by auto-queue.js after unified run completion.
async fn process_unified_thread_kill_signals(_shared: &Arc<SharedData>) {
    let channels = tokio::task::spawn_blocking(crate::dispatch::drain_unified_thread_kill_signals)
        .await
        .unwrap_or_default();

    for thread_channel_id in channels {
        // The kill signal carries the raw thread channel ID. Thread tmux sessions
        // are named "{parent_channel_name}-t{thread_channel_id}{env_suffix}".
        // We must find the matching tmux session by scanning for the exact suffix
        // including env isolation to avoid killing sessions from other environments.
        let env_suffix = crate::services::provider::tmux_env_suffix();
        let full_suffix = format!("-t{thread_channel_id}{env_suffix}");
        let suffix_c = full_suffix.clone();
        let killed = tokio::task::spawn_blocking(move || {
            let prefix = format!("{}-", crate::services::provider::TMUX_SESSION_PREFIX);
            let names = crate::services::platform::tmux::list_session_names().ok()?;
            for name in &names {
                if name.starts_with(&prefix) && name.ends_with(&suffix_c) {
                    record_tmux_exit_reason(name, "unified-thread run completed");
                    crate::services::platform::tmux::kill_session_with_reason(
                        name,
                        "unified-thread run completed",
                    );
                    return Some(name.clone());
                }
            }
            None
        })
        .await
        .unwrap_or(None);

        let ts = chrono::Local::now().format("%H:%M:%S");
        if let Some(name) = killed {
            tracing::info!(
                "  [{ts}] ♻ Killed unified-thread tmux session: {name} (thread: {thread_channel_id})"
            );
        }
    }
}
