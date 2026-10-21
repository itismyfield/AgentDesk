//! Queue effects a turn cancel applies after its lifecycle stop: the preserved-queue drain, the
//! forced purge and, for a Herdr execution, the effects that run under the terminate's exclusion.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::health::HealthRegistry;
use crate::services::discord::session_identity::SessionIdentity;
use crate::services::provider::ProviderKind;
use crate::services::turn_cancel_queue_guard::PurgeCounts;
use crate::services::turn_lifecycle::TurnLifecycleTarget;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, PurgeQueueResult, QueuePersistenceContext,
};

#[cfg(test)]
use crate::services::turn_orchestrator::Intervention;
#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

#[cfg(test)]
static PRE_PURGE_HOOK_FOR_TESTS: LazyLock<Mutex<HashMap<ChannelId, (Vec<Intervention>, String)>>> =
    LazyLock::new(Default::default);

#[cfg(test)]
pub(crate) fn set_pre_purge_hook_for_test(
    channel: ChannelId,
    items: Vec<Intervention>,
    token: &str,
) {
    PRE_PURGE_HOOK_FOR_TESTS
        .lock()
        .unwrap()
        .insert(channel, (items, token.into()));
}

#[cfg(test)]
type PrePurgeProbe = Box<dyn FnOnce() -> futures::future::BoxFuture<'static, ()> + Send>;

#[cfg(test)]
static PRE_PURGE_PROBE_FOR_TESTS: LazyLock<Mutex<HashMap<ChannelId, PrePurgeProbe>>> =
    LazyLock::new(Default::default);

/// Runs `probe` once when a purge of `channel` is about to reach its actor.
#[cfg(all(test, unix))]
pub(crate) fn set_pre_purge_probe_for_test(channel: ChannelId, probe: PrePurgeProbe) {
    PRE_PURGE_PROBE_FOR_TESTS
        .lock()
        .unwrap()
        .insert(channel, probe);
}

#[cfg(test)]
async fn run_pre_purge_hook_for_test(
    provider: &ProviderKind,
    channel: ChannelId,
    handle: &ChannelMailboxHandle,
) {
    let probe = PRE_PURGE_PROBE_FOR_TESTS.lock().unwrap().remove(&channel);
    if let Some(probe) = probe {
        probe().await;
    }
    let entry = PRE_PURGE_HOOK_FOR_TESTS.lock().unwrap().remove(&channel);
    if let Some((items, token)) = entry {
        handle
            .replace_queue(items, QueuePersistenceContext::new(provider, &token, None))
            .await;
    }
}

/// Schedule preserved work and return only the depth observed after the drain attempt.
pub(crate) async fn schedule_post_cancel_queue_drain(
    health_registry: Option<&Arc<HealthRegistry>>,
    target: &TurnLifecycleTarget,
    reason: &'static str,
) -> Option<usize> {
    let registry = health_registry?;
    let provider = target.provider.as_ref()?;
    let channel_id = target.channel_id?;
    let outcome = crate::services::discord::health::schedule_pending_queue_drain_after_cancel(
        registry.as_ref(),
        provider.as_str(),
        channel_id,
        reason,
    )
    .await;
    outcome.queue_depth_after
}

/// Purge through the actor and retain its measured removal counts and post-rollback depth.
/// Without a global mirror, sweep files only; another registry's actor may still hold a queue.
pub(crate) async fn force_purge_channel_mailbox(
    health_registry: Option<&Arc<HealthRegistry>>,
    target: &TurnLifecycleTarget,
    session_key: Option<&str>,
) -> Option<PurgeCounts> {
    let provider = target.provider.as_ref()?;
    let channel_id = target.channel_id?;
    let Some(handle) =
        crate::services::turn_orchestrator::ChannelMailboxRegistry::global_handle(channel_id)
    else {
        let disk_files_removed =
            crate::services::turn_orchestrator::remove_channel_pending_queue_files_all_tokens(
                provider, channel_id,
            );
        tracing::info!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            disk_files_removed,
            "force purge found no live mailbox handle"
        );
        return Some(PurgeCounts {
            drained: 0,
            disk_files_removed,
            own_files_removed: Some(0),
            queue_len_after: None,
        });
    };
    // #3029(D): force purge must also release the active-turn anchor so the
    // next dispatch is not blocked by a stale `cancel_token` /
    // `active_user_message_id`. The handler only clears an anchor whose token
    // is already `cancelled` (the force-kill above flipped it), so a fresh
    // turn that raced in after the kill keeps its anchor (#2706).
    let purge = purge_on(&handle, provider, channel_id, session_key, true).await?;
    let finish = crate::services::discord::health::finish_cancelled_provider_channel_mailbox(
        health_registry.map(Arc::as_ref),
        Some(provider.as_str()),
        Some(channel_id.get()),
        "queue_api_force_cancel_post_purge",
        Some(&handle),
    )
    .await;
    if finish.cleared_active_turn {
        tracing::info!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            global_active_decremented = finish.global_active_decremented,
            runtime_session_cleared = finish.runtime_session_cleared,
            "force purge finalized cancelled active mailbox turn"
        );
    }
    Some(purge.into())
}

/// Purges `handle`'s queue; `None` when the actor did not answer or refused for a queued head.
async fn purge_on(
    handle: &ChannelMailboxHandle,
    provider: &ProviderKind,
    channel_id: ChannelId,
    session_key: Option<&str>,
    clear_cancelled_active_anchor: bool,
) -> Option<PurgeQueueResult> {
    let token_hash = session_key
        .and_then(SessionIdentity::parse)
        .and_then(|identity| identity.token_hash)
        .unwrap_or_default();
    let persistence = QueuePersistenceContext::new(provider, &token_hash, None);
    #[cfg(test)]
    run_pre_purge_hook_for_test(provider, channel_id, handle).await;
    let purge = handle
        .try_purge_queue(persistence, clear_cancelled_active_anchor)
        .await
        .ok()?;
    if purge.queued_head_refused {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            "force purge refused; mailbox preserved"
        );
        return None;
    }
    tracing::info!(
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        purged = purge.drained,
        disk_files_removed = purge.disk_files_removed,
        cleared_active_anchor = purge.cleared_active_anchor,
        session_key,
        "force purged channel mailbox queue"
    );
    Some(purge)
}

/// The forced Herdr cancel's effects; the terminate service they run under is unix-only.
#[cfg(unix)]
pub(super) mod herdr {
    use sqlx::PgPool;

    use super::{PurgeCounts, TurnLifecycleTarget, purge_on};
    use crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg;
    use futures::FutureExt;
    use futures::future::BoxFuture;

    use crate::services::discord::TmuxCleanupPolicy;
    use crate::services::discord::herdr_terminate::RetiredExclusion;
    use crate::services::discord::herdr_terminate::http_force::Released;
    use crate::services::turn_cancel_finalizer::{
        FinalizeTurnCancelRequest, FinalizedTurnCancel, TurnCancelCorrelation, finalize_turn_cancel,
    };
    use crate::services::turn_cancel_queue_guard::{
        CancelQueueLoss, CancelRemovalWitness, capture_queue_before_cancel_on,
        record_queue_loss_after_cancel_on,
    };
    use crate::services::turn_lifecycle::cleanup_policy_observability_surface;

    pub(in crate::services::queue) const HERDR_TERMINATE: &str = "herdr-terminate";

    /// What a forced Herdr cancel applied under the terminate's exclusion. `failed` names the first
    /// step that did not apply; nothing after it ran. `loss` is `None` when the channel had no actor.
    pub(in crate::services::queue) struct HerdrForceEffects {
        pub(in crate::services::queue) failed: Option<String>,
        pub(in crate::services::queue) purge: Option<PurgeCounts>,
        pub(in crate::services::queue) loss: Option<CancelQueueLoss>,
        pub(in crate::services::queue) cancelled: FinalizedTurnCancel,
    }

    /// A forced cancel purged its queue under the exclusion, so nothing is left to release.
    impl Released for HerdrForceEffects {
        fn released(self) -> BoxFuture<'static, Self> {
            futures::future::ready(self).boxed()
        }
    }

    /// Cancels the bound dispatch, disconnects the retired row, then captures, purges and records
    /// the queue of the exclusion's own actor; the exclusion is held throughout.
    pub(in crate::services::queue) async fn herdr_force_effects(
        exclusion: &RetiredExclusion,
        pool: PgPool,
        session_key: String,
    ) -> HerdrForceEffects {
        let (failed, purge, loss) = herdr_force_steps(exclusion, &pool, &session_key).await;
        let turn_id = exclusion
            .settled_turn()
            .and_then(|token| token.turn_nonce());
        let request = FinalizeTurnCancelRequest {
            correlation: TurnCancelCorrelation {
                provider: Some(exclusion.provider().clone()),
                channel_id: Some(exclusion.channel()),
                dispatch_id: exclusion.dispatch().map(str::to_string),
                session_key: Some(session_key),
                turn_id: turn_id.map(str::to_string),
            },
            reason: "queue-api cancel_turn (force)".into(),
            surface: cleanup_policy_observability_surface(TmuxCleanupPolicy::CleanupSession {
                termination_reason_code: Some("queue_api_cancel_turn"),
            })
            .into(),
            lifecycle_path: HERDR_TERMINATE.into(),
            tmux_killed: false,
            inflight_cleared: exclusion.settled_turn().is_some(),
            queue_depth: loss.as_ref().and_then(|loss| loss.queue_depth_after),
            queue_preserved: purge.map(|_| false),
            termination_recorded: true,
            completed_at: chrono::Utc::now(),
        };
        HerdrForceEffects {
            failed,
            purge,
            loss,
            cancelled: finalize_turn_cancel(request),
        }
    }

    async fn herdr_force_steps(
        exclusion: &RetiredExclusion,
        pool: &PgPool,
        session_key: &str,
    ) -> (Option<String>, Option<PurgeCounts>, Option<CancelQueueLoss>) {
        if let Some(dispatch) = exclusion.dispatch()
            && let Err(error) =
                cancel_dispatch_and_reset_auto_queue_on_pg(pool, dispatch, None).await
        {
            return (Some(format!("dispatch cancel: {error}")), None, None);
        }
        match exclusion.disconnect_session(pool).await {
            Ok(true) => {}
            Ok(false) => return (Some("session row moved on".into()), None, None),
            Err(error) => return (Some(error), None, None),
        }
        let Some(actor) = exclusion.actor() else {
            return (None, None, None);
        };
        let (provider, channel) = (exclusion.provider(), exclusion.channel());
        let target = TurnLifecycleTarget {
            provider: Some(provider.clone()),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        // Capture, purge and record all address the exclusion's actor, whatever the channel registers.
        let capture = capture_queue_before_cancel_on(actor).await;
        let purge = purge_on(actor, provider, channel, Some(session_key), false).await;
        let purge = purge.map(PurgeCounts::from);
        let witness = CancelRemovalWitness::Purge(purge);
        let reason = "queue_api_cancel_turn_force";
        let pool = Some(pool);
        let loss =
            record_queue_loss_after_cancel_on(actor, &target, &capture, pool, witness, reason);
        let loss = loss.await;
        let failed = purge.is_none().then(|| "queue purge refused".to_string());
        (failed, purge, Some(loss))
    }
}
