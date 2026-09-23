use super::*;

/// Restore inflight turns FIRST, then flush restart reports (leader-only).
/// Recovery skips channels that have a pending restart report, so the report
/// must still be on disk when recovery runs. After recovery completes, the
/// flush loop starts and delivers/clears reports. Behavior-preserving
/// extraction; JoinHandle discarded as inline. `api_port` is captured by the
/// spawn (used by run_startup_diagnostic_after_reconcile_barrier).
#[allow(clippy::too_many_arguments)]
pub(super) fn run_bot_spawn_recovery_and_flush_restart_reports(
    ctx: &serenity::Context,
    shared_for_tmux: &Arc<SharedData>,
    token_owned: &str,
    provider_for_setup: &ProviderKind,
    startup_reconcile_remaining: &Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started: &Arc<std::sync::atomic::AtomicBool>,
    health_registry_for_setup: &Arc<health::HealthRegistry>,
    api_port: u16,
) {
    let http_for_tmux = ctx.http.clone();
    let shared_for_tmux2 = shared_for_tmux.clone();
    let http_for_restart_reports = ctx.http.clone();
    let ctx_for_kickoff = ctx.clone();
    let token_for_kickoff = token_owned.to_string();
    let shared_for_restart_reports = shared_for_tmux.clone();
    let provider_for_restore = provider_for_setup.clone();
    let startup_reconcile_remaining_for_restore = startup_reconcile_remaining.clone();
    let startup_doctor_started_for_restore = startup_doctor_started.clone();
    let health_registry_for_startup_doctor = health_registry_for_setup.clone();
    tokio::spawn(async move {
        let is_utility_bot = {
            let s = shared_for_tmux2.settings.read().await;
            s.agent.is_some()
        };
        if is_utility_bot {
            mark_reconcile_complete(&shared_for_tmux2);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ✓ Utility bot reconcile — skipped recovery");
        } else {
            // #429: Recover restart-gap messages first so new user input gets queued
            // within seconds of bot ready instead of waiting behind slower
            // Discord API-heavy inflight/thread-map recovery passes.
            catch_up_missed_messages(&http_for_tmux, &shared_for_tmux2, &provider_for_restore)
                .await;

            gc_stale_fixed_working_sessions(&shared_for_tmux2).await;

            let stale_cards_to_delete = super::queued_recovery::restore_queued_and_inflight_work(
                &http_for_tmux,
                &shared_for_tmux2,
                &provider_for_restore,
            )
            .await;

            // #226: Collect channels that recovery already handled (spawned + ended watchers).
            // restore_tmux_watchers must skip these to prevent duplicate watcher creation.
            // The issue: recovery watcher starts → session ends quickly → watcher removes
            // itself from DashMap → restore_tmux_watchers sees empty slot → creates second watcher.
            #[cfg(unix)]
            {
                // Mark all channels that recovery touched as "recently handled"
                // by inserting a recovery_handled marker in kv_meta.
                // restore_tmux_watchers checks this and skips those channels.
                let recovery_channels: Vec<u64> = shared_for_tmux2
                    .restart
                    .recovering_channels
                    .iter()
                    .map(|entry| entry.key().get())
                    .collect();
                super::tmux::store_recovery_handled_channels(&shared_for_tmux2, &recovery_channels)
                    .await;

                restore_tmux_watchers(&http_for_tmux, &shared_for_tmux2).await;
                cleanup_orphan_tmux_sessions(&shared_for_tmux2).await;

                // Clean up recovery markers
                super::tmux::clear_recovery_handled_channels(&shared_for_tmux2).await;
            }

            // Remove retired durable handoffs so stale legacy JSON cannot
            // influence startup.
            purge_legacy_durable_handoffs();

            // #164: Re-deliver orphan pending dispatches from before restart
            recover_orphan_pending_dispatches(&shared_for_restart_reports).await;

            // Kick off turns for channels that have queued messages but no
            // active turn. Without this, restored pending queues and handoff
            // injections sit idle until the next user message arrives.
            kickoff_idle_queues(
                &ctx_for_kickoff,
                &shared_for_restart_reports,
                &token_for_kickoff,
                &provider_for_restore,
            )
            .await;

            // codex review round-7 P2 (#1332): now that the
            // gateway has had a chance to settle and live
            // queues have been kicked off, best-effort
            // delete any `📬 메시지 대기 중` Discord cards
            // whose mapping the round-6 filter pruned.
            // Without this loop the cards stay forever (the
            // owning mapping was just removed, so no future
            // dispatch / queue-exit event can reach them).
            // #5035 (A8): the helper re-decides each card through the gate.
            delete_stale_queued_placeholder_cards(
                &http_for_tmux,
                &shared_for_tmux2,
                &stale_cards_to_delete,
            )
            .await;

            // #122: Reconcile phase complete — open intake
            mark_reconcile_complete(&shared_for_restart_reports);
            let ts = chrono::Local::now().format("%H:%M:%S");
            // #5462 S5 §7.2-4: every provider bot prints this line, so untagged
            // it cannot say WHICH intake opened — and the reconcile-window
            // incidents are diagnosed by lining this moment up against one
            // provider's own destructive log entries.
            tracing::info!(
                provider = %provider_for_restore.as_str(),
                "  [{ts}] ✓ Reconcile complete — intake open"
            );
        } // end of !is_utility_bot recovery block

        // Kick off again to drain messages queued during reconcile window
        kickoff_idle_queues(
            &ctx_for_kickoff,
            &shared_for_restart_reports,
            &token_for_kickoff,
            &provider_for_restore,
        )
        .await;

        // Thread-map validation is best-effort hygiene and can spend
        // multiple REST round-trips on startup. Do not block intake
        // reopening or queued-turn kickoff on it.
        if shared_for_tmux2.pg_pool.is_some()
            && STARTUP_THREAD_MAP_VALIDATION_STARTED
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] 🧹 THREAD-MAP: continuing validation in background");
            spawn_startup_thread_map_validation(
                shared_for_tmux2.pg_pool.clone(),
                token_for_kickoff.clone(),
            );
        }

        // §7.2-4: the one arrival that IS a provider reconcile completing.
        run_startup_diagnostic_after_reconcile_barrier_for_provider(
            &provider_for_restore,
            startup_reconcile_remaining_for_restore,
            startup_doctor_started_for_restore,
            health_registry_for_startup_doctor,
            api_port,
        )
        .await;

        // NOW flush restart reports (recovery is done, safe to delete them)
        flush_restart_reports(
            &http_for_restart_reports,
            &shared_for_restart_reports,
            &provider_for_restore,
        )
        .await;
        // Continue flushing in a loop for any reports created later
        loop {
            tokio::time::sleep(RESTART_REPORT_FLUSH_INTERVAL).await;
            flush_restart_reports(
                &http_for_restart_reports,
                &shared_for_restart_reports,
                &provider_for_restore,
            )
            .await;
        }
    });
}
