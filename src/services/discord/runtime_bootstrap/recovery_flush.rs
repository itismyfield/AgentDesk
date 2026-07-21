use super::*;

static ORPHAN_INFLIGHT_LOCK_SWEEP_ONCE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

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

            // Restore pending intervention queues saved during previous SIGTERM
            // before inflight turn recovery. Drain-mode queue snapshots are the
            // source of truth for restart-gap user input; if inflight recovery
            // recreates an active turn first, the active message id can make a
            // persisted queue item look "already known" and incorrectly drop it.
            let (restored_queues, restored_overrides) =
                load_pending_queues(&provider_for_restore, &shared_for_tmux2.token_hash);
            let restored_dispatch_markers =
                load_pending_dispatch_markers(&provider_for_restore, &shared_for_tmux2.token_hash);
            let allowed_bot_ids_for_restore: Vec<u64> = {
                let settings = shared_for_tmux2.settings.read().await;
                settings.allowed_bot_ids.clone()
            };
            let announce_bot_id_for_restore =
                super::resolve_announce_bot_user_id(&shared_for_tmux2).await;
            // P1-1: Restore dispatch_role_overrides from queue snapshots
            for (thread_channel_id, alt_channel_id) in &restored_overrides {
                if !matches!(
                    resolve_runtime_channel_binding_status(&http_for_tmux, *thread_channel_id)
                        .await,
                    RuntimeChannelBindingStatus::Owned
                ) {
                    continue;
                }
                shared_for_tmux2
                    .dispatch
                    .role_overrides
                    .insert(*thread_channel_id, *alt_channel_id);
            }
            for marker in &restored_dispatch_markers {
                let Some(alt_channel_id) = marker.restored_override else {
                    continue;
                };
                if !matches!(
                    resolve_runtime_channel_binding_status(&http_for_tmux, marker.channel_id).await,
                    RuntimeChannelBindingStatus::Owned
                ) {
                    continue;
                }
                shared_for_tmux2
                    .dispatch
                    .role_overrides
                    .insert(marker.channel_id, alt_channel_id);
            }
            if !restored_overrides.is_empty() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 📋 FLUSH: restored {} dispatch_role_override(s) from queue snapshots",
                    restored_overrides.len()
                );
            }
            if !restored_queues.is_empty() {
                let mut added = 0usize;
                let mut skipped_unowned = 0usize;
                let mut skipped_sender = 0usize;
                let mut skipped_duplicate = 0usize;
                let mut skipped_persist_error = 0usize;
                for (channel_id, items) in restored_queues {
                    if !matches!(
                        resolve_runtime_channel_binding_status(&http_for_tmux, channel_id).await,
                        RuntimeChannelBindingStatus::Owned
                    ) {
                        skipped_unowned += items.len();
                        continue;
                    }
                    // #3864: the sender filter is stateless, so it stays
                    // out-of-actor; collect the allowed items here. The merge
                    // into the live queue (dedup + front-insert + persist) then
                    // happens INSIDE the mailbox actor in one serialized step,
                    // so a live reconcile-window `Enqueue` can no longer be lost
                    // between an out-of-actor snapshot and a blind replace.
                    let mut allowed_items: Vec<Intervention> = Vec::with_capacity(items.len());
                    for item in items {
                        if super::is_allowed_turn_sender(
                            &allowed_bot_ids_for_restore,
                            announce_bot_id_for_restore,
                            item.author_id.get(),
                            item.author_is_bot,
                            &item.text,
                        ) {
                            allowed_items.push(item);
                        } else {
                            skipped_sender += 1;
                        }
                    }
                    let allowed_count = allowed_items.len();
                    if allowed_count == 0 {
                        continue;
                    }
                    let result = mailbox_merge_restored_queue_items(
                        &shared_for_tmux2,
                        &provider_for_restore,
                        channel_id,
                        allowed_items,
                    )
                    .await;
                    if let Some(error) = result.persistence_error {
                        // Merge-persist failed → the actor rolled the in-memory
                        // queue back. The live reconcile-window enqueue survives
                        // (it was persisted by its own `Enqueue` and lives in the
                        // rolled-back-to previous queue). Surface the failure;
                        // don't miscount the restored items as duplicates.
                        skipped_persist_error += allowed_count;
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] 📋 FLUSH: persist failed merging {allowed_count} restored queue item(s) for channel {channel_id}: {error}"
                        );
                    } else {
                        added += result.absorbed;
                        skipped_duplicate += allowed_count - result.absorbed;
                    }
                }
                let skipped =
                    skipped_unowned + skipped_sender + skipped_duplicate + skipped_persist_error;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 📋 FLUSH: restored {added} pending queue item(s) from disk (skipped {skipped}: unowned={skipped_unowned}, sender={skipped_sender}, duplicate={skipped_duplicate}, persist_error={skipped_persist_error})"
                );
            }
            // #3641: orphan `.json.lock` sidecars are invisible to the `.json`
            // row scans below, so sweep them once per process before inflight
            // recovery starts. The sweep itself enumerates provider subdirs.
            let _ = ORPHAN_INFLIGHT_LOCK_SWEEP_ONCE
                .get_or_init(super::inflight::reap_orphan_inflight_locks);

            // #2437 (#2427 C wire) boot-time generation
            // invalidate. Remove non-planned-restart inflight
            // rows whose `restart_generation` does not match
            // the current generation so recovery does not
            // revive a row whose tmux session no longer
            // exists. Must run BEFORE `restore_inflight_turns`
            // — otherwise recovery would attempt to revive
            // ghost rows and the placeholder sweeper would
            // eventually have to time-guess them at 1800s.
            // Planned-restart / hot-swap rows survive (their
            // generation gate in `stale_removal_reason`
            // already handles them with longer retention).
            let invalidated = super::inflight::invalidate_stale_generation(
                &provider_for_restore,
                shared_for_tmux2.restart.current_generation,
            );
            if invalidated > 0 {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🧹 inflight: invalidated {} stale-generation row(s) for {} (current generation {}) — #2437",
                    invalidated,
                    provider_for_restore.as_str(),
                    shared_for_tmux2.restart.current_generation,
                );
            }

            restore_inflight_turns(&http_for_tmux, &shared_for_tmux2, &provider_for_restore).await;

            if !restored_dispatch_markers.is_empty() {
                let mut added = 0usize;
                let mut skipped_unowned = 0usize;
                let mut skipped_sender = 0usize;
                let mut skipped_duplicate = 0usize;
                let mut skipped_persist_error = 0usize;
                for marker in restored_dispatch_markers {
                    if !matches!(
                        resolve_runtime_channel_binding_status(&http_for_tmux, marker.channel_id)
                            .await,
                        RuntimeChannelBindingStatus::Owned
                    ) {
                        skipped_unowned += 1;
                        continue;
                    }
                    if !super::is_allowed_turn_sender(
                        &allowed_bot_ids_for_restore,
                        announce_bot_id_for_restore,
                        marker.intervention.author_id.get(),
                        marker.intervention.author_is_bot,
                        &marker.intervention.text,
                    ) {
                        skipped_sender += 1;
                        continue;
                    }
                    let result = mailbox_merge_restored_dispatch_marker(
                        &shared_for_tmux2,
                        &provider_for_restore,
                        marker.channel_id,
                        marker.intervention,
                        marker.restored_override,
                    )
                    .await;
                    if let Some(error) = result.persistence_error {
                        skipped_persist_error += 1;
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] 📋 FLUSH: persist failed merging restored dispatch marker for channel {}: {error}",
                            marker.channel_id
                        );
                    } else if result.absorbed == 0 {
                        skipped_duplicate += 1;
                    } else {
                        added += result.absorbed;
                    }
                }
                let skipped =
                    skipped_unowned + skipped_sender + skipped_duplicate + skipped_persist_error;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 📋 FLUSH: restored {added} pending dispatch marker item(s) from disk after inflight recovery (skipped {skipped}: unowned={skipped_unowned}, sender={skipped_sender}, duplicate_or_active={skipped_duplicate}, persist_error={skipped_persist_error})"
                );
            }

            // Restore queued placeholder mappings after both queue snapshots and
            // dispatch markers have been merged. Marker merge must wait for
            // `restore_inflight_turns` so active turn ids are visible to mailbox
            // dedup; the placeholder live-queue filter then sees the final
            // restored queue state before kickoff.
            let mut stale_cards_to_delete: Vec<(ChannelId, MessageId, MessageId)> = Vec::new();
            let restored_queued_placeholders =
                super::queued_placeholders_store::load_queued_placeholders(
                    &provider_for_restore,
                    &shared_for_tmux2.token_hash,
                );
            if !restored_queued_placeholders.is_empty() {
                let live_queue_ids = collect_live_queue_message_ids(&shared_for_tmux2).await;
                let filter_outcome = filter_restored_queued_placeholders(
                    restored_queued_placeholders,
                    &live_queue_ids,
                );
                for (key, placeholder_msg_id) in &filter_outcome.live {
                    shared_for_tmux2
                        .queued
                        .queued_placeholders
                        .insert(*key, *placeholder_msg_id);
                }
                for channel_id in &filter_outcome.channels_with_stale {
                    super::queued_placeholders_store::persist_channel_from_map(
                        &shared_for_tmux2.queued.queued_placeholders,
                        &shared_for_tmux2.provider,
                        &shared_for_tmux2.token_hash,
                        *channel_id,
                    );
                }
                let live_count = filter_outcome.live.len();
                let stale_count = filter_outcome.stale_count;
                let ts = chrono::Local::now().format("%H:%M:%S");
                if stale_count > 0 {
                    tracing::warn!(
                        "  [{ts}] 📋 FLUSH: restored {live_count} queued-placeholder mapping(s) from disk; pruned {stale_count} stale mapping(s) with no live queue entry"
                    );
                } else {
                    tracing::info!(
                        "  [{ts}] 📋 FLUSH: restored {live_count} queued-placeholder mapping(s) from disk"
                    );
                }
                stale_cards_to_delete = filter_outcome.stale_cards;
            }

            // P1-2: Warn about legacy queue files that cannot be restored
            warn_legacy_pending_queue_files(&provider_for_restore);

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
            delete_stale_queued_placeholder_cards(&http_for_tmux, &stale_cards_to_delete).await;

            // #122: Reconcile phase complete — open intake
            mark_reconcile_complete(&shared_for_restart_reports);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ✓ Reconcile complete — intake open");
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

        run_startup_diagnostic_after_reconcile_barrier(
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
