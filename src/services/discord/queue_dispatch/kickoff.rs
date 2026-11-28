//! Shared queue dispatch for Gateway and REST runtimes.
use super::super::*;
use crate::services::cluster::intake_turn::IntakeExecution;
use crate::services::discord::queue_io::transport::IntakeDeps;
use crate::services::discord::shared_state::{PortFuture, QueuedAdmission, QueuedRun};

#[cfg(test)]
#[path = "cancel_backstop_race_support_tests.rs"]
pub(in crate::services::discord) mod cancel_backstop_test_support;

#[cfg(any(test, feature = "test-support"))]
#[path = "owner_race_hook_tests.rs"]
mod owner_race_hook_tests;
#[cfg(any(test, feature = "test-support"))]
pub(in crate::services::discord) use owner_race_hook_tests::{
    install_owner_race_pause, owner_race_kick_attempts, owner_race_pause,
};

/// The authority one kickoff runs with: the idle start, or only the busy-TUI paste of the first
/// ready input, named by the automatic entry point that asked for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum KickoffLane {
    Idle,
    QueuedOnly(&'static str),
}

pub(in crate::services::discord) async fn kickoff_idle_queue_channel(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> IdleQueueKickoffChannelOutcome {
    kickoff_queue_channel(deps, provider, channel_id, KickoffLane::Idle).await
}

pub(in crate::services::discord) async fn kickoff_queue_channel(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    lane: KickoffLane,
) -> IdleQueueKickoffChannelOutcome {
    // Boxed so the futures of callers that await a kickoff stay small.
    Box::pin(kickoff_queue_channel_inner(
        deps, provider, channel_id, lane,
    ))
    .await
}

async fn kickoff_queue_channel_inner(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    lane: KickoffLane,
) -> IdleQueueKickoffChannelOutcome {
    let shared = deps.shared;
    // An owner's queue is driven by its owner after a completion, never dequeued here.
    #[cfg(unix)]
    if queue_io::input_ingress::installed(&shared.mailbox(channel_id)).await == Ok(true) {
        (shared.router_ports.owner_wake)(shared, channel_id);
        return IdleQueueKickoffChannelOutcome::default();
    }
    // No owner drives this queue, so its Decision starts settle here, at each turn boundary.
    let pool = shared.pg_pool.as_ref();
    queue_io::decision_drain::queued::settle_started(pool, channel_id.get()).await;
    let settings_snapshot = shared.settings.read().await.clone();
    let thread_parent = match session_runtime::validate_rest_channel_routing_with_parent(
        deps.http,
        provider,
        &settings_snapshot,
        channel_id,
        None,
    )
    .await
    {
        Ok(parent) => parent,
        Err(reason) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ KICKOFF-GUARD: preserving queued item(s) for channel {} (reason={})",
                channel_id,
                reason
            );
            return IdleQueueKickoffChannelOutcome::default();
        }
    };

    let queued_gate = Some(
        shared.router_ports.queued_head_inject_covers as fn(ChannelId, Option<ChannelId>) -> bool,
    );
    let trigger = match lane {
        KickoffLane::Idle => "idle_kickoff",
        KickoffLane::QueuedOnly(reason) => reason,
    };
    let held = queue_io::queued_head_inject::try_busy(
        deps,
        provider,
        channel_id,
        Some(thread_parent),
        queued_gate,
        trigger,
    )
    .await;
    // The queued-only lane ends here whatever the paste answered: it never owns an idle start.
    if held || lane != KickoffLane::Idle {
        return IdleQueueKickoffChannelOutcome::default();
    }
    let fresh_snapshot = mailbox_snapshot(shared, channel_id).await;
    if !idle_queue_channel_has_kickable_backlog(shared, provider, channel_id, &fresh_snapshot).await
    {
        turn_finalizer::handle_idle_queue_guard_skip(shared, provider, channel_id, &fresh_snapshot)
            .await;
        return IdleQueueKickoffChannelOutcome::default();
    }

    #[cfg(unix)]
    if queue_dispatch::canary_workspace::parked(shared, provider, channel_id).await {
        return IdleQueueKickoffChannelOutcome::default();
    }

    // #4270 A — pre-dequeue hosted-TUI readiness gate. A verifiably busy hosted
    // TUI defers the promotion BEFORE `take_next_soft` and BEFORE the queued-view
    // teardown below (turn-view started/⏳ flip + 📬 marker drain + merged-card
    // deletion), so a still-busy channel keeps its steady `📬 Queued` view with
    // zero churn. No-start here is fail-open: callers arm the slow (60s)
    // backstop on a no-start with backlog, and the watcher-idle re-drain
    // delivers the fast edge once the TUI reaches Idle.
    if (shared.router_ports.promote_readiness_blocked)(shared, provider, channel_id).await {
        return IdleQueueKickoffChannelOutcome::default();
    }

    #[cfg(test)]
    let _cancel_backstop_race = cancel_backstop_test_support::pause(channel_id).await;

    #[cfg(any(test, feature = "test-support"))]
    owner_race_pause(channel_id, "kickoff_before_take").await;

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

    let owner_name = queued_owner_name(deps.http, intervention.author_id).await;

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🚀 KICKOFF: starting queued turn for channel {}",
        channel_id
    );

    let after_admission: PortFuture<'_, bool> = Box::pin(async {
        let late_settlement = (shared.router_ports.checkpoint_waits)(channel_id);
        if !late_settlement {
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
        }
        late_settlement
    });
    let admission = QueuedAdmission {
        provider: provider.clone(),
        channel_id,
        intervention: &intervention,
        request_owner: intervention.author_id,
        request_owner_name: owner_name,
        defer_watcher_resume: has_more,
        wait_for_completion: false,
        backstop_reason: "intake_admission_pre_kickoff_defer",
        dispatch_lease: dispatch_lease.clone(),
    };
    let admit = shared.router_ports.admit_and_finish_queued;
    let (late_settlement, dispatch_result) = match admit(deps, admission, after_admission).await {
        QueuedRun::Finished {
            after_admission,
            result,
        } => (after_admission, result),
        QueuedRun::NotAdmitted => {
            drop(dispatch_lease);
            return IdleQueueKickoffChannelOutcome::default();
        }
        QueuedRun::RejectedRestore => {
            queue_dispatch::log_kickoff_rejected_restore(provider, channel_id);
            drop(dispatch_lease);
            return IdleQueueKickoffChannelOutcome::default();
        }
    };
    match dispatch_result {
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}]   ⚠ KICKOFF: failed to start turn for channel {}: {e}",
                channel_id
            );
            // A Decision start that failed after its journal is closed: it never runs again.
            if queue_io::decision_drain::queued::origin(&intervention).is_some() {
                let abandon = crate::services::discord::mailbox_abandon_pending_dispatch;
                let _ = abandon(shared, provider, channel_id, intervention.message_id).await;
                drop(dispatch_lease);
                return IdleQueueKickoffChannelOutcome { started: false };
            }
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
        Ok(IntakeExecution::Deferred | IntakeExecution::ConsumedProtected) => {
            drop(dispatch_lease);
            IdleQueueKickoffChannelOutcome { started: false }
        }
        Ok(IntakeExecution::Executed) => {
            if late_settlement {
                queue_dispatch::settle_executed_queued_intake(
                    shared,
                    deps.http,
                    provider,
                    channel_id,
                    &intervention,
                    "idle_kickoff_queue_executed",
                )
                .await;
            }
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
/// The name a queued turn runs under: its author's Discord name, or a stable stand-in.
pub(in crate::services::discord) async fn queued_owner_name(
    http: &serenity::Http,
    author: UserId,
) -> String {
    match author.get() {
        0 | 1 => "system".to_string(),
        id => author
            .to_user(http)
            .await
            .map_or_else(|_| format!("user-{id}"), |u| u.name),
    }
}

pub(in crate::services::discord) async fn kickoff_idle_queues(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
) -> usize {
    kickoff_idle_queues_with_deps(
        &IntakeDeps {
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
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
) -> usize {
    let shared = deps.shared;
    // Collect channels with queued items that are idle (no active turn). Dequeue only
    // after the routing guard passes so a rejected channel stays preserved on disk/in memory.
    // A busy channel whose first ready input may be pasted joins on the queued-only lane.
    let mailbox_snapshots = shared.mailboxes.snapshot_all().await;
    let mut channels_to_kick: Vec<(ChannelId, KickoffLane)> = Vec::new();
    for (channel_id, snapshot) in mailbox_snapshots {
        if idle_queue_channel_has_kickable_backlog(shared, provider, channel_id, &snapshot).await {
            channels_to_kick.push((channel_id, KickoffLane::Idle));
            continue;
        }
        use queue_io::auto_queue_eligibility::{QueuedReach, queued_reach};
        let reason = "full_reconcile_queued_head";
        match queued_reach(Some(deps.http), shared, provider, channel_id, &snapshot).await {
            QueuedReach::Ready => {
                channels_to_kick.push((channel_id, KickoffLane::QueuedOnly(reason)))
            }
            QueuedReach::Wait => {
                super::super::arm_slow_idle_queue_backstop_if_queue_nonempty(
                    shared, provider, channel_id, reason,
                )
                .await;
            }
            QueuedReach::Closed | QueuedReach::NoCandidate => {}
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
    for (channel_id, lane) in channels_to_kick {
        let outcome = kickoff_queue_channel(deps, provider, channel_id, lane).await;
        if outcome.started {
            started_count += 1;
        } else if lane != KickoffLane::Idle
            || crate::services::tui_o::turn_mode::transcript_turns(channel_id.get())
        {
            // Boot and full reconcile have no later no-start arm; a held direct turn needs one,
            // and so does whatever a queued-only paste left behind.
            let reason = match lane {
                KickoffLane::Idle => "boot kickoff turn-mode no-start",
                KickoffLane::QueuedOnly(reason) => reason,
            };
            super::super::arm_slow_idle_queue_backstop_if_queue_nonempty(
                shared, provider, channel_id, reason,
            )
            .await;
        }
    }
    started_count
}
