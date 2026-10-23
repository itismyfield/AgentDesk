//! Persisted turn and queue recovery shared by Gateway and REST worker runtimes.
use super::*;

#[cfg(test)]
#[path = "queued_recovery_s2a_tests.rs"]
mod s2a_tests;

#[path = "queued_recovery_held.rs"]
mod held;

pub(super) async fn restore_worker_queues(shared: &Arc<SharedData>, provider: &ProviderKind) {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        tracing::error!(
            provider = provider.as_str(),
            "runner queue recovery needs Discord REST credentials"
        );
        return;
    };
    let stale_cards = restore_queued_and_inflight_work(&http, shared, provider).await;
    delete_stale_queued_placeholder_cards(&http, shared, &stale_cards).await;
    mark_reconcile_complete(shared);
    spawn_turn_completion_idle_queue_listener(shared.clone(), provider.clone());
    spawns::run_bot_spawn_queue_exit_clear_retry(shared);
}

pub(super) async fn restore_queued_and_inflight_work(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> Vec<(ChannelId, MessageId, MessageId)> {
    let (stale_cards, held) = restore_and_hold(http, shared, provider).await;
    held::spawn_retries(http, shared, provider, held);
    stale_cards
}

/// The boot restore: its stale cards, and the channels whose merge failed with the rows they hold.
async fn restore_and_hold(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> (
    Vec<(ChannelId, MessageId, MessageId)>,
    Vec<held::HeldRestore>,
) {
    // Restore pending intervention queues saved during previous SIGTERM
    // before inflight turn recovery. Drain-mode queue snapshots are the
    // source of truth for restart-gap user input; if inflight recovery
    // recreates an active turn first, the active message id can make a
    // persisted queue item look "already known" and incorrectly drop it.
    let (restored_queues, restored_overrides) = load_pending_queues(provider, &shared.token_hash);
    let restored_dispatch_markers = load_pending_dispatch_markers(provider, &shared.token_hash);
    let allowed_bot_ids_for_restore: Vec<u64> = {
        let settings = shared.settings.read().await;
        settings.allowed_bot_ids.clone()
    };
    let announce_bot_id_for_restore = super::resolve_announce_bot_user_id(shared).await;
    // Channels whose merge could not read or write their queue; that input is not discarded.
    let mut held = std::collections::HashMap::new();
    let mut stale_cards_to_delete: Vec<(ChannelId, MessageId, MessageId)> = Vec::new();
    // P1-1: Restore dispatch_role_overrides from queue snapshots
    for (thread_channel_id, alt_channel_id) in &restored_overrides {
        if !matches!(
            resolve_runtime_channel_binding_status(http, *thread_channel_id).await,
            RuntimeChannelBindingStatus::Owned
        ) {
            continue;
        }
        shared
            .dispatch
            .role_overrides
            .insert(*thread_channel_id, *alt_channel_id);
    }
    for marker in &restored_dispatch_markers {
        let Some(alt_channel_id) = marker.restored_override else {
            continue;
        };
        if !matches!(
            resolve_runtime_channel_binding_status(http, marker.channel_id).await,
            RuntimeChannelBindingStatus::Owned
        ) {
            continue;
        }
        shared
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
                resolve_runtime_channel_binding_status(http, channel_id).await,
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
            let candidates = allowed_items.clone();
            let merge =
                mailbox_merge_restored_queue_items(shared, provider, channel_id, allowed_items);
            let already = held.contains_key(&channel_id);
            let (result, installed) =
                held::guarded_merge(shared, provider, channel_id, already, merge).await;
            if let Some(error) = result.persistence_error {
                let stale = &mut stale_cards_to_delete;
                let entry = held::hold(&mut held, stale, channel_id, &error, installed);
                entry.queue = Some(candidates);
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
        let skipped = skipped_unowned + skipped_sender + skipped_duplicate + skipped_persist_error;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📋 FLUSH: restored {added} pending queue item(s) from disk (skipped {skipped}: unowned={skipped_unowned}, sender={skipped_sender}, duplicate_or_discarded={skipped_duplicate}, persist_error={skipped_persist_error})"
        );
    }
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
    let invalidated =
        super::inflight::invalidate_stale_generation(provider, shared.restart.current_generation);
    if invalidated > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🧹 inflight: invalidated {} stale-generation row(s) for {} (current generation {}) — #2437",
            invalidated,
            provider.as_str(),
            shared.restart.current_generation,
        );
    }

    #[cfg(test)]
    held::pause_after_queue_merges().await;
    restore_inflight_turns(http, shared, provider).await;

    if !restored_dispatch_markers.is_empty() {
        let mut added = 0usize;
        let mut skipped_unowned = 0usize;
        let mut skipped_sender = 0usize;
        let mut skipped_duplicate = 0usize;
        let mut skipped_persist_error = 0usize;
        for marker in restored_dispatch_markers {
            if !matches!(
                resolve_runtime_channel_binding_status(http, marker.channel_id).await,
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
            let (channel_id, alt) = (marker.channel_id, marker.restored_override);
            let candidate = (marker.intervention.clone(), alt);
            let merge = mailbox_merge_restored_dispatch_marker(
                shared,
                provider,
                channel_id,
                marker.intervention,
                alt,
            );
            let already = held.contains_key(&channel_id);
            let (result, installed) =
                held::guarded_merge(shared, provider, channel_id, already, merge).await;
            if let Some(error) = result.persistence_error {
                skipped_persist_error += 1;
                let stale = &mut stale_cards_to_delete;
                let entry = held::hold(&mut held, stale, channel_id, &error, installed);
                entry.marker = Some(candidate);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 📋 FLUSH: persist failed merging restored dispatch marker for channel {}: {error}",
                    channel_id
                );
            } else if result.absorbed == 0 {
                skipped_duplicate += 1;
            } else {
                added += result.absorbed;
            }
        }
        let skipped = skipped_unowned + skipped_sender + skipped_duplicate + skipped_persist_error;
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
    let mut restored_queued_placeholders =
        super::queued_placeholders_store::load_queued_placeholders(provider, &shared.token_hash);
    // A held channel's undecided owners are not judged by the queued-only filter here.
    restored_queued_placeholders.retain(|(channel_id, _), _| !held.contains_key(channel_id));
    if !restored_queued_placeholders.is_empty() {
        let observed = super::queued_placeholders::observe_live_queue_message_ids(shared).await;
        // A channel whose actor never answered has no measured queue to judge its cards against.
        let (unmeasured, judged): (std::collections::HashMap<_, _>, _) =
            restored_queued_placeholders
                .into_iter()
                .partition(|((channel_id, _), _)| observed.unmeasured.contains(channel_id));
        let filter_outcome = filter_restored_queued_placeholders(judged, &observed.ids);
        let live: Vec<_> = filter_outcome.live.into_iter().chain(unmeasured).collect();
        let live_count = live.len();
        let uninstalled = super::queued_placeholders::install_restored_queued_placeholders(
            shared,
            live,
            &filter_outcome.channels_with_stale,
        )
        .await;
        let stale_count = filter_outcome.stale_count;
        let ts = chrono::Local::now().format("%H:%M:%S");
        if stale_count > 0 {
            tracing::info!(
                "  [{ts}] 📋 FLUSH: loaded {live_count} live queued-placeholder candidate(s) from disk; pruned {stale_count} stale mapping(s) with no live queue entry"
            );
        } else {
            tracing::info!(
                "  [{ts}] 📋 FLUSH: loaded {live_count} live queued-placeholder candidate(s) from disk"
            );
        }
        stale_cards_to_delete.extend(filter_outcome.stale_cards);
        stale_cards_to_delete.extend(uninstalled);
    }

    // Inputs held by a person's draft get their pane protection and release watch back.
    let live_queue_ids = collect_live_queue_message_ids(shared).await;
    super::turn_bridge::followup_requeue::rewatch_draft_holds(shared, provider, &live_queue_ids);

    // P1-2: Warn about legacy queue files that cannot be restored
    warn_legacy_pending_queue_files(provider);

    (stale_cards_to_delete, held.into_values().collect())
}

#[cfg(all(test, unix))]
#[path = "queued_recovery_draft_tests.rs"]
mod draft_tests;
