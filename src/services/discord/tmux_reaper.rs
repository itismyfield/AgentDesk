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
                crate::services::platform::tmux::kill_session(
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

    // #3877: completion teardown can miss a `fresh` routine's DISTINCT tmux
    // session (e.g. a thread-less migrated-launchd run), leaving a dead-pane
    // orphan with no channel mapping that the loop below skips ("orphan —
    // handled by cleanup_orphan_tmux_sessions", which only runs at boot). Build
    // the set of reapable fresh-routine sessions (fresh + no in-flight run) so
    // the orphan branch can collect them here instead of waiting for a restart.
    let reapable_fresh_sessions = build_reapable_fresh_routine_sessions(shared, &provider).await;

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
            // orphan — no channel mapping. #3877: if this dead session is a
            // completed `fresh` routine's distinct session that escaped
            // completion teardown, reap it now (backstop) instead of leaving it
            // for the boot-only `cleanup_orphan_tmux_sessions`. The snapshot is
            // only ever non-empty when a PG pool exists, so the pool guard here
            // is a no-op in non-PG deployments and feeds the kill-time re-read.
            if let Some(pool) = shared.pg_pool.as_ref()
                && let Some(routine_id) = reapable_fresh_sessions.get(session_name)
                && reap_fresh_routine_orphan(pool, session_name, routine_id).await
            {
                reaped += 1;
            }
            continue;
        };

        // If a watcher is attached, tmux liveness is the termination authority:
        // the watcher observes pane death, clears the registry, and applies the
        // same lifecycle/audit semantics as the live tail path.
        if shared.tmux_watchers.contains_key(&channel_id) {
            continue;
        }

        if let Some(protection) = super::tmux_lifecycle::resolve_dispatch_tmux_protection(
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
            crate::services::platform::tmux::kill_session_output(
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

    reap_orphan_tmux_wrapper_processes().await;
}

/// (#3877) Builds the lookup the periodic reaper uses to collect a completed
/// `fresh` routine's orphaned dead-pane session (no channel mapping). Maps each
/// deterministic owned tmux session name to its routine id (for log context).
/// Empty when there is no PG pool, so non-PG deployments are a no-op. All routine
/// scoping/safety lives in
/// `fresh_session_reaper::reapable_fresh_routine_sessions` (fresh-only, no
/// in-flight run, DM-safe).
async fn build_reapable_fresh_routine_sessions(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> std::collections::HashMap<String, String> {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return std::collections::HashMap::new();
    };
    match crate::services::routines::fresh_session_reaper::reapable_fresh_routine_sessions(
        pool, provider,
    )
    .await
    {
        Ok(sessions) => sessions
            .into_iter()
            .map(|session| (session.tmux_session, session.routine.id))
            .collect(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "tmux reaper: failed to list reapable fresh routine sessions (#3877)"
            );
            std::collections::HashMap::new()
        }
    }
}

/// (#3877) Reaps one completed fresh-routine orphan by directly killing its
/// dead-pane tmux session — exactly like `cleanup_orphan_tmux_sessions`, NOT via
/// the routine teardown path. The teardown path force-kills the owning provider
/// CHANNEL runtime, which for a thread-less routine that ran in the agent's
/// primary channel would cancel an unrelated live primary-agent turn. The
/// backstop only ever fires for a session the caller already proved DEAD (no
/// live pane) and matched to a `fresh` routine with no in-flight run, so killing
/// the session and cleaning its temp files is sufficient and side-effect free.
/// Returns `true` when the kill succeeded so the caller can count it.
///
/// TOCTOU close (#3877, codex P1): the matched set is a SNAPSHOT taken once
/// before the loop, and the loop's pane-liveness check ran earlier in this
/// iteration. Between that snapshot/check and this kill, a new claim can set
/// `in_flight_run_id` and re-launch a fresh pane under the SAME deterministic
/// tmux name — killing then would tear down a just-re-triggered LIVE routine. So
/// immediately before the kill we RE-VALIDATE against fresh state: re-read the
/// routine row (must still be a `fresh`, agent-bound, no-in-flight orphan) AND
/// re-probe pane liveness (must still be definitively dead). Only when BOTH still
/// hold do we kill; otherwise we log the skip reason and preserve the session.
async fn reap_fresh_routine_orphan(
    pool: &sqlx::PgPool,
    session_name: &str,
    routine_id: &str,
) -> bool {
    let routine = match crate::services::routines::fresh_session_reaper::reread_routine(
        pool, routine_id,
    )
    .await
    {
        Ok(routine) => routine,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "tmux reaper backstop: re-read of routine {routine_id} failed — skipping kill of {session_name} (#3877)"
            );
            return false;
        }
    };

    // Re-probe pane liveness as a three-state answer: a transient probe failure
    // must NOT be mistaken for death (treated as "unknown ⇒ preserve").
    let pane = {
        let name = session_name.to_string();
        tokio::task::spawn_blocking(move || {
            crate::services::tmux_diagnostics::tmux_session_pane_liveness(&name)
        })
        .await
        .unwrap_or(crate::services::platform::tmux::PaneLiveness::ProbeError)
    };

    if let Err(reason) =
        crate::services::routines::fresh_session_reaper::revalidate_fresh_orphan_before_kill(
            routine.as_ref(),
            pane,
        )
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ reaper backstop: preserving fresh routine session {session_name} (routine {routine_id}) — {reason} (#3877)"
        );
        return false;
    }

    let name = session_name.to_string();
    let killed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            record_tmux_exit_reason(
                &name,
                "tmux reaper backstop: completed fresh routine orphan (#3877)",
            );
            crate::services::platform::tmux::kill_session(
                &name,
                "tmux reaper backstop: completed fresh routine orphan (#3877)",
            )
        }),
    )
    .await
    .unwrap_or(Ok(false))
    .unwrap_or(false);

    if killed {
        crate::services::tmux_common::cleanup_session_temp_files(session_name);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🪦 reaper backstop: reaped completed fresh routine orphan {session_name} (routine {routine_id}) (#3877)"
        );
    }
    killed
}

#[cfg(unix)]
async fn reap_orphan_tmux_wrapper_processes() {
    let wrappers = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(list_orphan_tmux_wrapper_processes),
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
                "  [{ts}] 🧹 killing orphan {} wrapper pid={} session={}",
                wrapper.provider.as_str(),
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
        tracing::info!("  [{ts}] 🧹 Reaped {killed}/{count} orphan tmux wrapper process(es)");
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanTmuxWrapperProcess {
    pid: i32,
    provider: ProviderKind,
    tmux_session_name: Option<String>,
}

#[cfg(unix)]
fn list_orphan_tmux_wrapper_processes() -> Vec<OrphanTmuxWrapperProcess> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_orphan_tmux_wrapper_process_line)
        .collect()
}

#[cfg(unix)]
fn parse_orphan_tmux_wrapper_process_line(line: &str) -> Option<OrphanTmuxWrapperProcess> {
    let trimmed = line.trim_start();
    let first_end = trimmed.find(char::is_whitespace)?;
    let pid = trimmed[..first_end].parse::<i32>().ok()?;
    let rest = trimmed[first_end..].trim_start();
    let second_end = rest.find(char::is_whitespace)?;
    let ppid = rest[..second_end].parse::<i32>().ok()?;
    let command = rest[second_end..].trim_start();

    if ppid != 1 {
        return None;
    }

    let argv: Vec<&str> = command.split_whitespace().collect();
    if !command_invokes_agentdesk_binary(&argv) {
        return None;
    }
    let provider = argv
        .iter()
        .find_map(|token| provider_for_managed_tmux_wrapper_subcommand(token))?;

    if extract_arg_value(&argv, "--output-file").is_none() {
        return None;
    }

    let has_input_fifo = extract_arg_value(&argv, "--input-fifo").is_some();
    let has_pipe_input_mode = extract_arg_value(&argv, "--input-mode") == Some("pipe");
    if !has_input_fifo && !has_pipe_input_mode {
        return None;
    }

    Some(OrphanTmuxWrapperProcess {
        pid,
        provider,
        tmux_session_name: extract_tmux_session_name_from_wrapper_command(command),
    })
}

#[cfg(unix)]
fn command_invokes_agentdesk_binary(argv: &[&str]) -> bool {
    argv.first()
        .and_then(|exe| std::path::Path::new(exe).file_name())
        .and_then(|basename| basename.to_str())
        == Some("agentdesk")
}

#[cfg(unix)]
fn provider_for_managed_tmux_wrapper_subcommand(token: &str) -> Option<ProviderKind> {
    crate::services::provider::provider_registry()
        .iter()
        .filter(|entry| entry.managed_tmux_backend)
        .find(|entry| entry.managed_tmux_wrapper_subcommand == Some(token))
        .and_then(|entry| ProviderKind::from_str(entry.id))
}

#[cfg(unix)]
fn extract_arg_value<'a>(argv: &'a [&str], flag: &str) -> Option<&'a str> {
    argv.windows(2)
        .find_map(|window| (window[0] == flag).then_some(window[1]))
}

#[cfg(unix)]
fn extract_tmux_session_name_from_wrapper_command(command: &str) -> Option<String> {
    let argv: Vec<&str> = command.split_whitespace().collect();
    let output_path = extract_arg_value(&argv, "--output-file")?;
    let basename = std::path::Path::new(output_path).file_name()?.to_str()?;
    let stem = basename.strip_suffix(".jsonl").unwrap_or(basename);
    let pos = stem.find("AgentDesk-")?;
    Some(stem[pos..].to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        extract_tmux_session_name_from_wrapper_command, parse_orphan_tmux_wrapper_process_line,
        provider_for_managed_tmux_wrapper_subcommand,
    };
    use crate::services::provider::ProviderKind;

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
    fn parses_ppid_one_managed_provider_wrappers() {
        let claude_line = " 6259     1 /Users/me/.adk/release/bin/agentdesk tmux-wrapper --output-file /var/folders/x/agentdesk-AgentDesk-claude-adk-cc.jsonl --input-fifo /var/folders/x/agentdesk-AgentDesk-claude-adk-cc.unused-fifo";
        let claude = parse_orphan_tmux_wrapper_process_line(claude_line).unwrap();
        assert_eq!(claude.pid, 6259);
        assert_eq!(claude.provider, ProviderKind::Claude);
        assert_eq!(
            claude.tmux_session_name.as_deref(),
            Some("AgentDesk-claude-adk-cc")
        );

        let line = " 6260     1 /Users/me/.adk/release/bin/agentdesk codex-tmux-wrapper --output-file /var/folders/x/agentdesk-AgentDesk-codex-adk-cdx.jsonl --input-mode pipe";
        let parsed = parse_orphan_tmux_wrapper_process_line(line).unwrap();
        assert_eq!(parsed.pid, 6260);
        assert_eq!(parsed.provider, ProviderKind::Codex);
        assert_eq!(
            parsed.tmux_session_name.as_deref(),
            Some("AgentDesk-codex-adk-cdx")
        );

        let qwen_line = " 6262     1 /Users/me/.adk/release/bin/agentdesk qwen-tmux-wrapper --output-file /var/folders/x/agentdesk-AgentDesk-qwen-adk-qw.jsonl --input-fifo /var/folders/x/agentdesk-AgentDesk-qwen-adk-qw.unused-fifo";
        let qwen = parse_orphan_tmux_wrapper_process_line(qwen_line).unwrap();
        assert_eq!(qwen.pid, 6262);
        assert_eq!(qwen.provider, ProviderKind::Qwen);
        assert_eq!(
            qwen.tmux_session_name.as_deref(),
            Some("AgentDesk-qwen-adk-qw")
        );
    }

    #[test]
    fn rejects_live_parent_wrappers() {
        let live_line = " 6261  6988 /Users/me/.adk/release/bin/agentdesk codex-tmux-wrapper --output-file /Users/me/.adk/release/runtime/sessions/host-AgentDesk-codex-adk-cdx.jsonl";
        assert!(parse_orphan_tmux_wrapper_process_line(live_line).is_none());
    }

    #[test]
    fn rejects_partial_substring_matches() {
        let line = " 6260     1 /Users/me/.adk/release/bin/agentdesk not-codex-tmux-wrapper --output-file /var/folders/x/agentdesk-AgentDesk-codex-adk-cdx.jsonl --input-mode pipe";
        assert!(parse_orphan_tmux_wrapper_process_line(line).is_none());

        let flag_line = " 6260     1 /Users/me/.adk/release/bin/agentdesk codex-tmux-wrapper --not-output-file /var/folders/x/agentdesk-AgentDesk-codex-adk-cdx.jsonl --input-mode pipe";
        assert!(parse_orphan_tmux_wrapper_process_line(flag_line).is_none());
    }

    #[test]
    fn rejects_non_agentdesk_commands_with_wrapper_tokens() {
        let line = " 6260     1 /usr/bin/python3 codex-tmux-wrapper --output-file /var/folders/x/agentdesk-AgentDesk-codex-adk-cdx.jsonl --input-mode pipe";
        assert!(parse_orphan_tmux_wrapper_process_line(line).is_none());
    }

    #[test]
    fn accepts_persistent_runtime_session_paths() {
        let line = " 6260     1 /Users/me/.adk/release/bin/agentdesk codex-tmux-wrapper --output-file /Users/me/.adk/release/runtime/sessions/host-AgentDesk-codex-adk-cdx.jsonl --input-mode pipe";
        let parsed = parse_orphan_tmux_wrapper_process_line(line).unwrap();
        assert_eq!(parsed.provider, ProviderKind::Codex);
        assert_eq!(
            parsed.tmux_session_name.as_deref(),
            Some("AgentDesk-codex-adk-cdx")
        );
    }

    #[test]
    fn rejects_wrappers_without_required_input_flags() {
        let line = " 6260     1 /Users/me/.adk/release/bin/agentdesk codex-tmux-wrapper --output-file /var/folders/x/agentdesk-AgentDesk-codex-adk-cdx.jsonl";
        assert!(parse_orphan_tmux_wrapper_process_line(line).is_none());
    }

    #[test]
    fn provider_helper_maps_only_managed_wrapper_subcommands() {
        assert_eq!(
            provider_for_managed_tmux_wrapper_subcommand("tmux-wrapper"),
            Some(ProviderKind::Claude)
        );
        assert_eq!(
            provider_for_managed_tmux_wrapper_subcommand("codex-tmux-wrapper"),
            Some(ProviderKind::Codex)
        );
        assert_eq!(
            provider_for_managed_tmux_wrapper_subcommand("qwen-tmux-wrapper"),
            Some(ProviderKind::Qwen)
        );
        assert_eq!(provider_for_managed_tmux_wrapper_subcommand("gemini"), None);
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
                    crate::services::platform::tmux::kill_session(
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
