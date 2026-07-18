use super::*;

struct DeferredRestartPermit;

/// Publish the admission fence before health can acknowledge the marker. The
/// per-provider CAS gives exactly one poller permission to wait, persist, and
/// consume that provider's shutdown-barrier slot.
fn begin_deferred_restart(shared: &SharedData) -> Option<DeferredRestartPermit> {
    shared.restart.intake_worker_lifecycle.fence_admission();
    shared.restart.shutting_down.store(true, Ordering::SeqCst);
    shared
        .restart
        .shutdown_counted
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .ok()
        .map(|_| DeferredRestartPermit)
}

async fn prepare_deferred_restart(shared: &SharedData) -> Option<DeferredRestartPermit> {
    let permit = begin_deferred_restart(shared)?;
    shared
        .restart
        .intake_worker_lifecycle
        .wait_until_drained()
        .await;
    // `restart_pending` is the health-visible acknowledgement consumed by the
    // wrapper. Publish it only after an accepted tick has fully executed.
    shared.restart.restart_pending.store(true, Ordering::SeqCst);
    Some(permit)
}

fn finish_deferred_restart(shared: &SharedData, _permit: DeferredRestartPermit) -> bool {
    shared
        .restart
        .shutdown_remaining
        .fetch_sub(1, Ordering::AcqRel)
        == 1
}

/// Background: poll for the deferred restart marker for gateway and standby
/// runtimes. The marker first fences admissions and cancels intake polling;
/// health counters then provide the drain proof before the wrapper boots out.
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
                    let Some(shutdown_permit) =
                        prepare_deferred_restart(&shared_for_deferred).await
                    else {
                        continue;
                    };
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
                    if finish_deferred_restart(&shared_for_deferred, shutdown_permit) {
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

fn periodic_catch_up_interval() -> std::time::Duration {
    const DEFAULT_SECS: u64 = 60;
    const MIN_SECS: u64 = 10;
    let secs = std::env::var("AGENTDESK_CATCH_UP_POLL_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(|secs| secs.max(MIN_SECS))
        .unwrap_or(DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Background: bounded REST backstop for gateway message gaps. Startup
/// recovery covers restart windows; this loop covers the rarer case where the
/// gateway stays connected but a provider channel misses a MessageCreate event.
pub(super) fn run_bot_spawn_periodic_catch_up(
    http: Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let shared_for_catch_up = shared.clone();
    let provider_for_catch_up = provider.clone();
    task_supervisor::spawn_observed("periodic_catch_up", async move {
        let is_utility_bot = {
            let settings = shared_for_catch_up.settings.read().await;
            settings.agent.is_some()
        };
        if is_utility_bot {
            return;
        }

        let interval = periodic_catch_up_interval();
        tokio::time::sleep(interval).await;
        loop {
            catch_up_missed_messages(&http, &shared_for_catch_up, &provider_for_catch_up).await;
            tokio::time::sleep(interval).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn standby_marker_fences_intake_exposes_ack_and_counts_shutdown_once() {
        let registry = health::HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared.restart.shutdown_remaining.store(1, Ordering::SeqCst);
        registry
            .register_standby("codex".to_string(), shared.clone())
            .await;

        let execute_started = Arc::new(tokio::sync::Notify::new());
        let execute_release = Arc::new(tokio::sync::Notify::new());
        let shared_for_worker = shared.clone();
        let started_for_worker = execute_started.clone();
        let release_for_worker = execute_release.clone();
        let worker = tokio::spawn(async move {
            let _active_tick = shared_for_worker
                .restart
                .intake_worker_lifecycle
                .try_begin_tick()
                .expect("tick admitted before restart fence");
            started_for_worker.notify_one();
            release_for_worker.notified().await;
        });
        execute_started.notified().await;

        let shared_for_prepare = shared.clone();
        let prepare =
            tokio::spawn(async move { prepare_deferred_restart(&shared_for_prepare).await });
        while !shared.restart.shutting_down.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }

        assert!(
            !shared.restart.restart_pending.load(Ordering::Acquire),
            "health must not acknowledge while the accepted execute future is active"
        );
        assert_eq!(
            shared.restart.shutdown_remaining.load(Ordering::Acquire),
            1,
            "the shutdown token must remain unconsumed while execute is active"
        );
        assert!(begin_deferred_restart(&shared).is_none());

        execute_release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), worker)
            .await
            .expect("accepted execute drain")
            .expect("worker join");
        let permit = tokio::time::timeout(std::time::Duration::from_secs(1), prepare)
            .await
            .expect("marker acknowledgement after execute drain")
            .expect("prepare join")
            .expect("first marker acknowledgement");

        let snapshot = serde_json::to_value(health::build_health_snapshot(&registry).await)
            .expect("serialize acknowledged standby health");
        assert_eq!(snapshot["providers"][0]["restart_pending"], true);
        assert!(shared.restart.shutting_down.load(Ordering::Acquire));

        assert!(finish_deferred_restart(&shared, permit));
        assert_eq!(
            shared.restart.shutdown_remaining.load(Ordering::Acquire),
            0,
            "the standby provider consumes its barrier slot exactly once"
        );
    }
}
