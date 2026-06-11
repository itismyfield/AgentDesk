use super::*;

/// Background: poll for the deferred restart marker when idle (leader-only).
/// Behavior-preserving extraction of the inline spawn from run_bot's setup
/// callback. Both clones are used only inside the spawn; the JoinHandle is
/// discarded exactly as the inline code did.
pub(super) fn run_bot_spawn_deferred_restart_poller(
    shared_for_tmux: &Arc<SharedData>,
    provider_for_setup: &ProviderKind,
) {
    let shared_for_deferred = shared_for_tmux.clone();
    let provider_for_deferred = provider_for_setup.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(DEFERRED_RESTART_POLL_INTERVAL).await;
            // Quick-exit restart (#2713): dcserver no longer waits
            // for all active turns to drain. The marker is a deploy
            // request to persist cheap local state and exit; managed
            // TUI/tmux sessions survive and the next process
            // rehydrates transcript tailing from runtime state.
            if let Some(root) = crate::agentdesk_runtime_root() {
                let marker = root.join("restart_pending");
                if marker.exists() {
                    shared_for_deferred
                        .restart
                        .restart_pending
                        .store(true, Ordering::SeqCst);
                    shared_for_deferred
                        .restart
                        .shutting_down
                        .store(true, Ordering::SeqCst);
                    if shared_for_deferred
                        .restart
                        .shutdown_counted
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                        .is_err()
                    {
                        continue;
                    }
                    let drain =
                        mailbox_restart_drain_all(&shared_for_deferred, &provider_for_deferred)
                            .await;
                    let queue_count = drain.queued_count;
                    if !drain.persistence_errors.is_empty() {
                        tracing::error!(
                            failures = drain.persistence_errors.len(),
                            "restart_pending quick exit continuing after pending-queue persistence failure(s)"
                        );
                    }
                    let ids: std::collections::HashMap<u64, u64> = shared_for_deferred
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_deferred.as_str(),
                            &ids,
                        );
                    }
                    // Quick-exit must preserve inflight state with
                    // bumped mtime + DrainRestart marker. Without
                    // this, repeated quick-exits (e.g. destructive
                    // E2E scenarios that restart release multiple
                    // times) leave file mtime frozen at first save,
                    // and stale-removal trips after 1800s even
                    // while the tmux pane is still alive. Mirrors
                    // the graceful-shutdown preserve block below.
                    let inflight_states_qe = inflight::load_inflight_states(&provider_for_deferred);
                    if !inflight_states_qe.is_empty() {
                        let ts2 = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts2}] 👁 preserving {} inflight turn(s) for restart recovery",
                            inflight_states_qe.len()
                        );
                        let marked_qe = inflight::mark_all_inflight_states_restart_mode(
                            &provider_for_deferred,
                            crate::services::discord::InflightRestartMode::DrainRestart,
                        );
                        tracing::info!(
                            "  [{ts2}] 🔖 marked {marked_qe} inflight turn(s) as drain_restart"
                        );
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 🔄 restart_pending detected — quick exit after persisting {queue_count} queued item(s)"
                    );
                    if shared_for_deferred
                        .restart
                        .shutdown_remaining
                        .fetch_sub(1, Ordering::AcqRel)
                        == 1
                    {
                        let _ = std::fs::remove_file(&marker);
                        std::process::exit(0);
                    }
                }
            }
            // Use process-global counters so we wait for ALL providers
            let g_active = shared_for_deferred
                .restart
                .global_active
                .load(Ordering::Relaxed);
            let g_finalizing = shared_for_deferred
                .restart
                .global_finalizing
                .load(Ordering::Relaxed);
            if g_active == 0
                && g_finalizing == 0
                && shared_for_deferred
                    .restart
                    .restart_pending
                    .load(Ordering::Relaxed)
            {
                let drain =
                    mailbox_restart_drain_all(&shared_for_deferred, &provider_for_deferred).await;
                let queue_count = drain.queued_count;
                if !drain.persistence_errors.is_empty() {
                    tracing::error!(
                        failures = drain.persistence_errors.len(),
                        "deferred restart observed pending-queue persistence failure(s)"
                    );
                }
                if queue_count > 0 {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 📋 DRAIN: mailbox persisted {queue_count} pending queue item(s) before deferred restart"
                    );
                }
                check_deferred_restart(&shared_for_deferred);
                // This provider has saved and decremented — stop polling
                return;
            }
        }
    });
}

/// Background: hot-reload skills on file changes (30s polling). Scans
/// home-level AND all active project-level skill directories. Behavior-
/// preserving extraction; JoinHandle discarded as inline.
pub(super) fn run_bot_spawn_skills_hot_reload(
    shared_for_tmux: &Arc<SharedData>,
    provider_for_setup: &ProviderKind,
) {
    let shared_for_skills = shared_for_tmux.clone();
    let provider_for_skills = provider_for_setup.clone();
    tokio::spawn(async move {
        let mut last_fingerprint: (usize, u64) = (0, 0); // (file_count, max_mtime_epoch)
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            // Collect unique project paths from active sessions
            let project_paths: Vec<String> = {
                let data = shared_for_skills.core.lock().await;
                let mut paths: Vec<String> = data
                    .sessions
                    .values()
                    .filter_map(|s| s.current_path.clone())
                    .collect();
                paths.sort();
                paths.dedup();
                paths
            };
            let fp = skill_dir_fingerprint_with_projects(&provider_for_skills, &project_paths);
            if fp != last_fingerprint && last_fingerprint != (0, 0) {
                // Merge home + all project skills (scan_skills deduplicates by name)
                let mut merged = scan_skills(&provider_for_skills, None);
                let mut seen: std::collections::HashSet<String> =
                    merged.iter().map(|(n, _)| n.clone()).collect();
                for path in &project_paths {
                    for skill in scan_skills(&provider_for_skills, Some(path)) {
                        if seen.insert(skill.0.clone()) {
                            merged.push(skill);
                        }
                    }
                }
                merged.sort_by(|a, b| a.0.cmp(&b.0));
                let count = merged.len();
                *shared_for_skills.skills_cache.write().await = merged;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔄 Skills hot-reloaded: {count} skill(s) ({} files, mtime Δ)",
                    fp.0
                );
            }
            last_fingerprint = fp;
        }
    });
}

/// Background: periodic cleanup for stale Discord upload files. No captures;
/// behavior-preserving extraction.
pub(super) fn run_bot_spawn_upload_cleanup() {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(UPLOAD_CLEANUP_INTERVAL).await;
            cleanup_old_uploads(UPLOAD_MAX_AGE);
        }
    });
}

/// Background: periodic GC for stale thread sessions in DB. Normal
/// idle/disconnected thread rows expire after 1 hour, but rows still carrying
/// an active_dispatch_id stay until the 3-hour safety TTL so warm-resume
/// sessions keep DB ownership. Behavior-preserving extraction.
pub(super) fn run_bot_spawn_stale_session_gc(shared_clone: &Arc<SharedData>) {
    let shared_for_session_gc = shared_clone.clone();
    tokio::spawn(async move {
        // Run every 10 minutes, initial delay 2 minutes
        tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;
        loop {
            gc_stale_fixed_working_sessions(&shared_for_session_gc).await;
            gc_stale_thread_sessions(&shared_for_session_gc).await;
            tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;
        }
    });
}

#[cfg(unix)]
pub(super) fn run_bot_spawn_dead_tmux_reaper(shared_clone: &Arc<SharedData>) {
    let shared_for_reaper = shared_clone.clone();
    tokio::spawn(async move {
        // Initial delay: let startup recovery finish first
        tokio::time::sleep(tokio::time::Duration::from_secs(90)).await;
        loop {
            reap_dead_tmux_sessions(&shared_for_reaper).await;
            tokio::time::sleep(DEAD_SESSION_REAP_INTERVAL).await;
        }
    });
}
