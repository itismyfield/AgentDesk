//! Shared queue dispatch for Gateway and REST runtimes.
use super::super::*;

pub(in crate::services::discord) async fn kickoff_idle_queue_channel(
    deps: &router::IntakeDeps<'_>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> IdleQueueKickoffChannelOutcome {
    let shared = deps.shared;
    let settings_snapshot = shared.settings.read().await.clone();
    if let Err(reason) = session_runtime::validate_rest_channel_routing(
        deps.http,
        deps.cache,
        provider,
        &settings_snapshot,
        channel_id,
        None,
    )
    .await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ KICKOFF-GUARD: preserving queued item(s) for channel {} (reason={})",
            channel_id,
            reason
        );
        return IdleQueueKickoffChannelOutcome::default();
    }

    let fresh_snapshot = mailbox_snapshot(shared, channel_id).await;
    if !idle_queue_channel_has_kickable_backlog(shared, provider, channel_id, &fresh_snapshot).await
    {
        turn_finalizer::handle_idle_queue_guard_skip(shared, provider, channel_id, &fresh_snapshot)
            .await;
        return IdleQueueKickoffChannelOutcome::default();
    }

    // #4270 A — pre-dequeue hosted-TUI readiness gate. A verifiably busy hosted
    // TUI defers the promotion BEFORE `take_next_soft` and BEFORE the queued-view
    // teardown below (turn-view started/⏳ flip + 📬 marker drain + merged-card
    // deletion), so a still-busy channel keeps its steady `📬 Queued` view with
    // zero churn. No-start here is fail-open: callers arm the slow (60s)
    // backstop on a no-start with backlog, and the watcher-idle re-drain
    // delivers the fast edge once the TUI reaches Idle.
    if router::hosted_tui_promote_readiness_blocked(shared, provider, channel_id).await {
        return IdleQueueKickoffChannelOutcome::default();
    }

    let take_next = idle_queue_take_next_soft_if_ready(shared, provider, channel_id).await;
    if let Some(error) = take_next.persistence_error.as_ref() {
        tracing::error!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            error = %error,
            "KICKOFF: preserving queued turn after pending-queue persistence failure"
        );
        return IdleQueueKickoffChannelOutcome::default();
    }
    let Some((intervention, has_more, dispatch_lease)) = take_next.into_intervention() else {
        return IdleQueueKickoffChannelOutcome::default();
    };

    let owner_name = if intervention.author_id.get() <= 1 {
        "system".to_string()
    } else {
        intervention
            .author_id
            .to_user(deps.http)
            .await
            .map(|u| u.name.clone())
            .unwrap_or_else(|_| format!("user-{}", intervention.author_id.get()))
    };

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🚀 KICKOFF: starting queued turn for channel {}",
        channel_id
    );

    let admitted = match router::admit_queued_intake(
        deps,
        provider.clone(),
        channel_id,
        &intervention,
        intervention.author_id,
        owner_name,
        has_more,
        false,
        "intake_admission_pre_kickoff_defer",
        dispatch_lease.clone(),
    )
    .await
    {
        router::QueuedAdmissionDisposition::Admitted(admitted) => admitted,
        router::QueuedAdmissionDisposition::Deferred
        | router::QueuedAdmissionDisposition::RejectedNonPortableAttachment => {
            drop(dispatch_lease);
            return IdleQueueKickoffChannelOutcome::default();
        }
        router::QueuedAdmissionDisposition::RejectedRestore => {
            queue_dispatch::log_kickoff_rejected_restore(provider, channel_id);
            drop(dispatch_lease);
            return IdleQueueKickoffChannelOutcome::default();
        }
    };

    let source_message_generations = intervention.source_message_queued_generations();
    queue_marker::start_and_drain_kickoff_markers(
        shared,
        deps.http,
        channel_id,
        intervention.message_id,
        &source_message_generations,
    )
    .await;

    let drained_cards = gateway::drain_merged_queued_placeholders(
        shared,
        channel_id,
        intervention.message_id,
        &intervention.source_message_ids,
    )
    .await;
    // #5035 (A5): the drain now yields tokens only for gate-released cards.
    for teardown in drained_cards {
        let _ = queued_card_gate::teardown_delete(deps.http, shared, teardown).await;
    }

    let dispatch_result =
        router::finish_admitted_queued_intake(deps, admitted, &intervention).await;
    match dispatch_result {
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}]   ⚠ KICKOFF: failed to start turn for channel {}: {e}",
                channel_id
            );
            let restored = mailbox_restore_dequeued_head(
                shared,
                provider,
                channel_id,
                intervention,
                dispatch_lease
                    .as_ref()
                    .expect("dequeued kickoff intervention must carry its lease")
                    .clone(),
            )
            .await;
            if !restored.enqueued {
                tracing::error!(
                    provider = provider.as_str(),
                    channel_id = channel_id.get(),
                    refusal_reason = restored
                        .refusal_reason
                        .map(|reason| reason.as_str())
                        .unwrap_or("none"),
                    persistence_error = restored.persistence_error.as_deref().unwrap_or("none"),
                    "KICKOFF: dequeued-head restore rejected after dispatch failure"
                );
            }
            drop(dispatch_lease);
            IdleQueueKickoffChannelOutcome { started: false }
        }
        Ok(()) => {
            mailbox_abandon_unclaimed_dispatch_after_success(
                shared,
                provider,
                channel_id,
                intervention.message_id,
                dispatch_lease
                    .as_ref()
                    .expect("dequeued kickoff intervention must carry its lease")
                    .clone(),
            )
            .await;
            drop(dispatch_lease);
            IdleQueueKickoffChannelOutcome { started: true }
        }
    }
}

/// Kick off turns for channels that have queued interventions but no active
/// turn running. This bridges the gap where restored pending queues or
/// handoff injections sit idle because no turn-completion event triggers
/// the dequeue chain.
pub(in crate::services::discord) async fn kickoff_idle_queues(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
) -> usize {
    kickoff_idle_queues_with_deps(
        &router::IntakeDeps {
            http: &ctx.http,
            cache: Some(&ctx.cache),
            ctx_for_chained_dispatch: Some(ctx),
            shared,
            token,
        },
        provider,
    )
    .await
}

pub(in crate::services::discord) async fn kickoff_idle_queues_with_deps(
    deps: &router::IntakeDeps<'_>,
    provider: &ProviderKind,
) -> usize {
    let shared = deps.shared;
    // Collect channels with queued items that are idle (no active turn). Dequeue only
    // after the routing guard passes so a rejected channel stays preserved on disk/in memory.
    let mailbox_snapshots = shared.mailboxes.snapshot_all().await;
    let mut channels_to_kick: Vec<ChannelId> = Vec::new();
    for (channel_id, snapshot) in mailbox_snapshots {
        if idle_queue_channel_has_kickable_backlog(shared, provider, channel_id, &snapshot).await {
            channels_to_kick.push(channel_id);
        }
    }

    if channels_to_kick.is_empty() {
        return 0;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🚀 KICKOFF: starting turns for {} idle channel(s) with queued messages",
        channels_to_kick.len()
    );

    let mut started_count = 0usize;
    for channel_id in channels_to_kick {
        let outcome = kickoff_idle_queue_channel(deps, provider, channel_id).await;
        if outcome.started {
            started_count += 1;
        }
    }
    started_count
}
