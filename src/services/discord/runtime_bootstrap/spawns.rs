use super::*;

pub(super) struct DeferredRestartPermit;

pub(super) fn restart_request_matches(root: &std::path::Path, name: &str, nonce: &str) -> bool {
    std::fs::read_to_string(root.join(name))
        .ok()
        .and_then(|request| {
            request
                .lines()
                .find_map(|line| line.strip_prefix("nonce="))
                .map(str::to_owned)
        })
        .as_deref()
        == Some(nonce)
}

/// Rolls back a restart cycle if its request was cancelled or its task is
/// dropped before its request has been superseded. The nonce prevents an old
/// poller from restoring admission for a newer restart request.
struct DeferredRestartCancellationGuard {
    shared: Arc<SharedData>,
    root: std::path::PathBuf,
    nonce: String,
    armed: bool,
}

impl DeferredRestartCancellationGuard {
    fn new(shared: Arc<SharedData>, root: std::path::PathBuf, nonce: String) -> Self {
        Self {
            shared,
            root,
            nonce,
            armed: true,
        }
    }

    fn cancelled(&self) -> bool {
        restart_request_matches(&self.root, "restart_cancelled", &self.nonce)
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DeferredRestartCancellationGuard {
    fn drop(&mut self) {
        if self.armed
            && (self.cancelled()
                || restart_request_matches(&self.root, "restart_pending", &self.nonce))
        {
            rollback_deferred_restart(&self.shared);
        }
    }
}

/// Publish the admission fence before health can acknowledge the marker. The
/// per-provider CAS gives exactly one poller permission to wait, persist, and
/// consume that provider's shutdown-barrier slot.
pub(super) fn begin_deferred_restart(shared: &SharedData) -> Option<DeferredRestartPermit> {
    shared.restart.intake_worker_lifecycle.fence_admission();
    shared.restart.shutting_down.store(true, Ordering::SeqCst);
    shared
        .restart
        .shutdown_counted
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .ok()
        .map(|_| DeferredRestartPermit)
}

async fn prepare_deferred_restart(
    shared: &Arc<SharedData>,
    root: &std::path::Path,
    nonce: String,
) -> Option<(DeferredRestartPermit, DeferredRestartCancellationGuard)> {
    let permit = begin_deferred_restart(shared)?;
    let guard = DeferredRestartCancellationGuard::new(shared.clone(), root.to_path_buf(), nonce);
    shared
        .restart
        .intake_worker_lifecycle
        .wait_until_drained()
        .await;
    if guard.cancelled() {
        return None;
    }
    // `restart_pending` is the health-visible acknowledgement consumed by the
    // wrapper. Publish it only after an accepted tick has fully executed.
    shared.restart.restart_pending.store(true, Ordering::SeqCst);
    Some((permit, guard))
}

pub(super) fn finish_deferred_restart(shared: &SharedData, _permit: DeferredRestartPermit) -> bool {
    let is_final = shared
        .restart
        .shutdown_remaining
        .fetch_sub(1, Ordering::AcqRel)
        == 1;
    shared
        .restart
        .shutdown_slot_consumed
        .store(true, Ordering::Release);
    is_final
}

/// Write the durable sentinel, then make the final cancellation decision at
/// the closest practical point before the atomic rename. A successful rename
/// is the point of no return: cancellation observed afterwards is intentionally
/// ignored so persistence and process exit remain a single durable outcome.
pub(super) fn commit_deferred_restart_sentinel(
    root: &std::path::Path,
    provider: &ProviderKind,
    nonce: &str,
    guard: &DeferredRestartCancellationGuard,
) -> std::io::Result<bool> {
    let ack = root.join("restart_persisted");
    let ack_tmp = root.join(format!("restart_persisted.{}.tmp", std::process::id()));
    let ack_body = format!(
        "nonce={nonce}\nprovider={}\ncommitted_at={}\n",
        provider.as_str(),
        chrono::Utc::now().to_rfc3339()
    );
    std::fs::write(&ack_tmp, ack_body)?;
    if guard.cancelled() || !restart_request_matches(root, "restart_pending", nonce) {
        let _ = std::fs::remove_file(&ack_tmp);
        return Ok(false);
    }
    std::fs::rename(&ack_tmp, &ack)?;
    // Compare-and-act again after the atomic acknowledgement publish. A newer
    // request may have replaced the marker between the pre-rename check and the
    // rename; never let a stale poller claim or remove that newer request.
    if !restart_request_matches(root, "restart_pending", nonce) {
        if restart_request_matches(root, "restart_persisted", nonce) {
            let _ = std::fs::remove_file(&ack);
        }
        return Ok(false);
    }
    Ok(true)
}

fn release_deferred_restart_ownership(shared: &SharedData) {
    shared
        .restart
        .shutdown_counted
        .store(false, Ordering::Release);
    if shared
        .restart
        .shutdown_slot_consumed
        .swap(false, Ordering::AcqRel)
    {
        shared
            .restart
            .shutdown_remaining
            .fetch_add(1, Ordering::AcqRel);
    }
}

fn rollback_deferred_restart(shared: &SharedData) {
    shared.restart.intake_worker_lifecycle.unfence_admission();
    shared.restart.shutting_down.store(false, Ordering::SeqCst);
    shared
        .restart
        .restart_pending
        .store(false, Ordering::SeqCst);
    release_deferred_restart_ownership(shared);
}

/// Release only the stale poller's per-provider barrier ownership. A newer
/// restart nonce inherits the process-wide admission fence and restart flags;
/// clearing those here would reopen intake underneath the new owner.
pub(super) fn handoff_superseded_restart(shared: &SharedData) {
    release_deferred_restart_ownership(shared);
}

fn restart_request_is_superseded(root: &std::path::Path, nonce: &str) -> bool {
    let marker = root.join("restart_pending");
    marker.exists() && !restart_request_matches(root, "restart_pending", nonce)
}

#[cfg(unix)]
fn capture_reachability_watcher_incarnation(
    shared: &SharedData,
    tmux_session_name: &str,
) -> Option<(
    crate::services::discord::tmux_watcher_registry::TmuxWatcherBinding,
    Option<health::reachability::observation::WatcherIncarnationCapture>,
)> {
    // Reuse the registry's existing handoff mutex so the binding identity,
    // generation, and spawn nonce are captured from one live watcher view.
    let _registry_guard =
        crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry();
    let channel = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name)?;
    let binding = shared.tmux_watchers.channel_binding(&channel)?;
    if binding.tmux_session_name != tmux_session_name {
        return None;
    }
    let snapshot = binding.output_path.as_deref().map(|transcript_path| {
        health::reachability::observation::capture_watcher_incarnation(
            std::path::Path::new(transcript_path),
            &binding.tmux_session_name,
            outbound::delivery_record::current_generation_mtime_ns(&binding.tmux_session_name),
            tmux::execution_identity::capture_spawn_nonce(&binding.tmux_session_name),
        )
    });
    Some((binding, snapshot))
}

/// Observe every canonical watcher binding on the stall-watchdog cadence.
///
/// This task owns no relay or recovery authority. File and ledger failures are
/// reduced to observation state and logs, so they cannot stop either provider
/// runtime or its intake worker.
#[cfg(unix)]
pub(super) fn run_bot_spawn_reachability_observation(
    shared_for_tmux: &Arc<SharedData>,
    provider_for_setup: &ProviderKind,
) {
    let shared = shared_for_tmux.clone();
    let provider = provider_for_setup.clone();
    tokio::spawn(async move {
        let cadence = std::time::Duration::from_secs(health::STALL_WATCHDOG_INTERVAL_SECS);
        loop {
            tokio::time::sleep(cadence).await;

            // Clone keys before resolving channel snapshots: retaining a
            // DashMap guard while consulting another index can deadlock.
            let sessions: Vec<_> = shared
                .tmux_watchers
                .iter()
                .map(|entry| entry.key().clone())
                .collect();
            let provider_for_tick = provider.clone();
            let shared_for_tick = shared.clone();

            let tick = tokio::task::spawn_blocking(move || {
                for session in sessions {
                    let Some((binding, snapshot)) = capture_reachability_watcher_incarnation(
                        &shared_for_tick,
                        &session,
                    ) else {
                        continue;
                    };
                    if binding.output_path.is_none() {
                        tracing::warn!(
                            provider = provider_for_tick.as_str(),
                            channel_id = binding.owner_channel_id.get(),
                            "reachability observation has no watcher transcript"
                        );
                        continue;
                    }
                    let Some(ledger_path) = health::reachability::ledger::ledger_path(
                        &provider_for_tick,
                        binding.owner_channel_id.get(),
                    ) else {
                        tracing::warn!(
                            provider = provider_for_tick.as_str(),
                            channel_id = binding.owner_channel_id.get(),
                            "reachability observation has no runtime ledger path"
                        );
                        continue;
                    };
                    let snapshot = match snapshot.expect("output path checked above") {
                        Ok(snapshot) => snapshot,
                        Err(reason) => {
                            tracing::warn!(
                                provider = provider_for_tick.as_str(),
                                channel_id = binding.owner_channel_id.get(),
                                ?reason,
                                "reachability observation unavailable; provider runtime continues"
                            );
                            continue;
                        }
                    };
                    let observed_at_epoch_ms =
                        chrono::Utc::now().timestamp_millis().max(0) as u64;
                    let session_for_revalidation = binding.tmux_session_name.clone();
                    let state = health::reachability::observation::observe_channel_at(
                        &ledger_path,
                        snapshot,
                        observed_at_epoch_ms,
                        || {
                            capture_reachability_watcher_incarnation(
                                &shared_for_tick,
                                &session_for_revalidation,
                            )
                            .and_then(|(_, snapshot)| snapshot.and_then(Result::ok))
                        },
                    );

                    match state {
                        health::reachability::observation::ReachabilityObservationState::Bootstrapped => {
                            tracing::debug!(
                                provider = provider_for_tick.as_str(),
                                channel_id = binding.owner_channel_id.get(),
                                "reachability observation bootstrapped"
                            );
                        }
                        health::reachability::observation::ReachabilityObservationState::Recorded {
                            commit,
                            unknown_reason,
                        } => {
                            tracing::debug!(
                                provider = provider_for_tick.as_str(),
                                channel_id = binding.owner_channel_id.get(),
                                obligations_appended = commit.obligations_appended,
                                extinctions = commit.extinctions.len(),
                                ?unknown_reason,
                                "reachability observation recorded"
                            );
                        }
                        health::reachability::observation::ReachabilityObservationState::SkippedStaleIncarnation { .. } => {}
                        health::reachability::observation::ReachabilityObservationState::Unknown {
                            reason,
                        } => {
                            tracing::warn!(
                                provider = provider_for_tick.as_str(),
                                channel_id = binding.owner_channel_id.get(),
                                ?reason,
                                "reachability observation unavailable; provider runtime continues"
                            );
                        }
                    }
                }
            })
            .await;
            if let Err(error) = tick {
                tracing::warn!(
                    provider = provider.as_str(),
                    error = %error,
                    "reachability observation task join failed; provider runtime continues"
                );
            }
        }
    });
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
                    let request = std::fs::read_to_string(&marker).unwrap_or_default();
                    let nonce = request
                        .lines()
                        .find_map(|line| line.strip_prefix("nonce="))
                        .unwrap_or_default()
                        .to_owned();
                    if nonce.is_empty() {
                        tracing::error!("restart request lacks nonce; retaining runtime");
                        continue;
                    }
                    if restart_request_matches(&root, "restart_cancelled", &nonce) {
                        rollback_deferred_restart(&shared_for_deferred);
                        tracing::info!(
                            provider = provider_for_deferred.as_str(),
                            "restart request cancelled; intake admission restored"
                        );
                        continue;
                    }
                    let Some((shutdown_permit, mut cancellation_guard)) =
                        prepare_deferred_restart(&shared_for_deferred, &root, nonce.clone()).await
                    else {
                        continue;
                    };
                    let drain =
                        mailbox_restart_drain_all(&shared_for_deferred, &provider_for_deferred)
                            .await;
                    if cancellation_guard.cancelled() {
                        continue;
                    }
                    let queue_count = drain.queued_count;
                    if !drain.persistence_errors.is_empty() {
                        tracing::error!(
                            failures = drain.persistence_errors.len(),
                            "restart_pending persistence failed; retaining marker and runtime"
                        );
                        continue;
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
                    let inflight_states_qe = inflight::load_inflight_states(&provider_for_deferred);
                    if !inflight_states_qe.is_empty() {
                        let ts2 = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts2}] 👁 preserving {} inflight turn(s) for restart recovery",
                            inflight_states_qe.len()
                        );
                        let marked_qe =
                            match inflight::mark_all_inflight_states_restart_mode_checked(
                                &provider_for_deferred,
                                crate::services::discord::InflightRestartMode::DrainRestart,
                            ) {
                                Ok(marked) => marked,
                                Err(error) => {
                                    tracing::error!(
                                        provider = provider_for_deferred.as_str(),
                                        error = %error,
                                        "restart_pending inflight persistence failed; retaining marker and runtime"
                                    );
                                    continue;
                                }
                            };
                        tracing::info!(
                            "  [{ts2}] 🔖 marked {marked_qe} inflight turn(s) as drain_restart"
                        );
                    }
                    if cancellation_guard.cancelled() {
                        continue;
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 🔄 restart_pending detected — quick exit after persisting {queue_count} queued item(s)"
                    );
                    if finish_deferred_restart(&shared_for_deferred, shutdown_permit) {
                        match commit_deferred_restart_sentinel(
                            &root,
                            &provider_for_deferred,
                            &nonce,
                            &cancellation_guard,
                        ) {
                            Ok(false) => {
                                if restart_request_is_superseded(&root, &nonce) {
                                    handoff_superseded_restart(&shared_for_deferred);
                                    cancellation_guard.disarm();
                                }
                                continue;
                            }
                            Err(error) => {
                                tracing::error!(
                                    error = %error,
                                    "restart persistence acknowledgement publish failed; retaining runtime"
                                );
                                continue;
                            }
                            Ok(true) => {}
                        }
                        if !restart_request_matches(&root, "restart_pending", &nonce) {
                            // A newer nonce owns the marker. Preserve its shared
                            // fence, release A's barrier slot, and keep this poller
                            // alive so it can service B on the next iteration.
                            if restart_request_is_superseded(&root, &nonce) {
                                handoff_superseded_restart(&shared_for_deferred);
                            }
                            cancellation_guard.disarm();
                            continue;
                        }
                        cancellation_guard.disarm();
                        let _ = std::fs::remove_file(&marker);
                        std::process::exit(0);
                    }

                    // A non-final provider must keep its guard alive until the
                    // final provider publishes the sentinel or cancellation
                    // arrives. Returning here would strand its consumed slot.
                    loop {
                        tokio::time::sleep(DEFERRED_RESTART_POLL_INTERVAL).await;
                        if cancellation_guard.cancelled() {
                            break;
                        }
                        if !restart_request_matches(&root, "restart_pending", &nonce) {
                            if restart_request_is_superseded(&root, &nonce) {
                                handoff_superseded_restart(&shared_for_deferred);
                            }
                            cancellation_guard.disarm();
                            break;
                        }
                    }
                    continue;
                }
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
#[path = "spawns_tests.rs"]
mod tests;
