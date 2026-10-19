//! Persisted turn and queue recovery shared by Gateway and REST worker runtimes, and a standby
//! holder's single-channel boot restore.
use super::*;
use crate::services::turn_orchestrator::{
    load_channel_pending_dispatch_marker_checked, load_channel_pending_queue_checked,
};

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

type Restore = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>;
type StandbyRestore = (
    Arc<std::sync::atomic::AtomicBool>,
    Arc<dyn Fn() -> Option<bool> + Send + Sync>,
    Arc<dyn Fn() -> Restore + Send + Sync>,
);

/// A standby holder's port parts: its own restored flag, which the provider reconcile never sets,
/// a read of the channel's persisted queue and marker, and the channel's boot restore.
pub(super) fn standby_restore(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: u64,
) -> StandbyRestore {
    let restored = Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(test)]
    let restored = match super::restore_mutant("reconcile_as_channel_restored") {
        true => Arc::clone(&shared.restart.reconcile_done),
        false => restored,
    };
    let (owner, token_hash) = (provider.clone(), shared.token_hash.clone());
    let persisted = Arc::new(move || {
        let channel = (channel != 0).then(|| ChannelId::new(channel))?;
        persisted_input(&owner, &token_hash, channel)
    });
    let (shared, owner) = (Arc::clone(shared), provider.clone());
    let run = Arc::new(move || -> Restore {
        let (shared, provider) = (Arc::clone(&shared), owner.clone());
        Box::pin(async move { restore_standby_channel(&shared, &provider, channel).await })
    });
    (restored, persisted, run)
}

/// Whether the channel's persisted queue or dispatch marker still holds input; `None` unreadable.
fn persisted_input(provider: &ProviderKind, token_hash: &str, channel: ChannelId) -> Option<bool> {
    let (queue, _) = load_channel_pending_queue_checked(provider, token_hash, channel).ok()?;
    let marker =
        load_channel_pending_dispatch_marker_checked(provider, token_hash, channel).ok()?;
    Some(!queue.is_empty() || marker.is_some())
}

/// Restores one held channel's persisted input in the boot order: queue, inflight turn, marker,
/// placeholders. `false` when a read, merge or install fails, or a restored row is left unrun.
async fn restore_standby_channel(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: u64,
) -> bool {
    use crate::services::discord::mailbox_finish::try_restitution;
    if channel == 0 || !super::herdr_runs_here(channel) {
        return false;
    }
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return false;
    };
    let (channel_id, token) = (ChannelId::new(channel), shared.token_hash.as_str());
    let queue = load_channel_pending_queue_checked(provider, token, channel_id);
    let marker = load_channel_pending_dispatch_marker_checked(provider, token, channel_id);
    let placeholders = super::queued_placeholders_store::load_channel_queued_placeholders_checked(
        provider, token, channel_id,
    );
    let row = super::inflight::load_channel_inflight_for_probe(provider, channel);
    let (Ok((queue, queue_override)), Ok(marker), Ok(placeholders), Ok(row)) =
        (queue, marker, placeholders, row)
    else {
        return false;
    };
    #[cfg(test)]
    boot_restore_tests::paused(channel).await;
    if !matches!(
        resolve_runtime_channel_binding_status(&http, channel_id).await,
        RuntimeChannelBindingStatus::Owned
    ) {
        return false;
    }
    let marker_override = marker.as_ref().and_then(|(_, alt)| *alt);
    for alt in queue_override.into_iter().chain(marker_override) {
        shared.dispatch.role_overrides.insert(channel_id, alt);
    }
    let allowed_bot_ids = shared.settings.read().await.allowed_bot_ids.clone();
    let announce = super::resolve_announce_bot_user_id(shared).await;
    let allowed = |item: &Intervention| {
        let (author, bot) = (item.author_id.get(), item.author_is_bot);
        super::is_allowed_turn_sender(&allowed_bot_ids, announce, author, bot, &item.text)
    };
    let persistence =
        crate::services::discord::queue_persistence_context(shared, provider, channel_id);
    let items: Vec<Intervention> = queue.into_iter().filter(|item| allowed(item)).collect();
    if !items.is_empty() {
        let merged = try_restitution(shared, channel_id, |h| {
            let (items, p) = (items.clone(), persistence.clone());
            async move { h.merge_restored_queue_items_or_refused(items, p).await }
        });
        if !matches!(merged.await, Ok(result) if result.persistence_error.is_none()) {
            return false;
        }
    }
    let Ok(before) = shared.mailbox(channel_id).try_snapshot().await else {
        return false;
    };
    // A live turn of this process owns the row now; only a row left by the last process restores.
    if let Some(state) = row.filter(|_| before.cancel_token.is_none()) {
        let states = vec![state];
        let restore = crate::services::discord::recovery_engine::restore_inflight_states(
            &http, shared, provider, states,
        );
        Box::pin(restore).await;
    }
    let left = super::inflight::load_channel_inflight_for_probe(provider, channel);
    let Ok(after) = shared.mailbox(channel_id).try_snapshot().await else {
        return false;
    };
    // A held or unverified row left without a running turn is not restored.
    let unrun = match left {
        Ok(None) => false,
        Ok(Some(_)) => after.cancel_token.is_none(),
        Err(_) => true,
    };
    if unrun {
        return false;
    }
    if let Some((marker, alt)) = marker.filter(|(item, _)| allowed(item)) {
        let merged = try_restitution(shared, channel_id, |h| {
            let (marker, p) = (marker.clone(), persistence.clone());
            async move {
                h.merge_restored_dispatch_marker_or_refused(marker, alt, p)
                    .await
            }
        });
        if !matches!(merged.await, Ok(result) if result.persistence_error.is_none()) {
            return false;
        }
    }
    let installed = super::queued_placeholders::install_channel_queued_placeholders_checked(
        shared,
        channel_id,
        placeholders,
    );
    let Ok(stale) = installed.await else {
        return false;
    };
    delete_stale_queued_placeholder_cards(&http, shared, &stale).await;
    true
}

#[cfg(test)]
#[path = "queued_recovery_boot_restore_tests.rs"]
pub(super) mod boot_restore_tests;

#[cfg(all(test, unix))]
#[path = "queued_recovery_draft_tests.rs"]
mod draft_tests;
