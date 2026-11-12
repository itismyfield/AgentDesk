//! Queue effects a turn cancel applies after its lifecycle stop: the preserved-queue drain, the
//! forced purge and, for a Herdr execution, the effects that run under the terminate's exclusion.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::domain::session_identity::SessionIdentity;
use crate::services::discord::health::HealthRegistry;
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
type PrePurgeHooks = HashMap<ChannelId, (Vec<Intervention>, String)>;

#[cfg(test)]
static PRE_PURGE_HOOK_FOR_TESTS: LazyLock<Mutex<PrePurgeHooks>> = LazyLock::new(Default::default);

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
            refused_reason: None,
        });
    };
    // #3029(D): force purge must also release the active-turn anchor so the
    // next dispatch is not blocked by a stale `cancel_token` /
    // `active_user_message_id`. The handler only clears an anchor whose token
    // is already `cancelled` (the force-kill above flipped it), so a fresh
    // turn that raced in after the kill keeps its anchor (#2706).
    let (purge, refused_reason) = purge_on(
        &handle,
        provider,
        channel_id,
        session_key,
        health_registry.map(Arc::as_ref),
        true,
    )
    .await?;
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
    Some(PurgeCounts {
        refused_reason,
        ..purge.into()
    })
}

/// The forced purge, then the receipts its owner owes for each removed origin.
pub(crate) async fn force_purge_and_settle(
    health_registry: Option<&Arc<HealthRegistry>>,
    target: &TurnLifecycleTarget,
    session_key: Option<&str>,
    pool: Option<&sqlx::PgPool>,
) -> Option<PurgeCounts> {
    let purged = force_purge_channel_mailbox(health_registry, target, session_key).await;
    if let (Some(provider), Some(channel)) = (&target.provider, target.channel_id)
        && let Some(handle) =
            crate::services::turn_orchestrator::ChannelMailboxRegistry::global_handle(channel)
    {
        crate::services::queue::settle_v2_drops(pool, &handle, provider.as_str(), channel).await;
    }
    purged
}

/// Purges `handle`'s queue; `None` when the actor did not answer or refused for a queued head.
/// A V2 owner's refusal answers with nothing drained, the depth it kept and why it kept it.
async fn purge_on(
    handle: &ChannelMailboxHandle,
    provider: &ProviderKind,
    channel_id: ChannelId,
    session_key: Option<&str>,
    health_registry: Option<&HealthRegistry>,
    clear_cancelled_active_anchor: bool,
) -> Option<(PurgeQueueResult, Option<String>)> {
    let mut token_hash = session_key
        .and_then(SessionIdentity::parse)
        .and_then(|identity| identity.token_hash)
        .unwrap_or_default();
    // Resolve only a sessionless request; the actor still checks the actual owner's file and CAS.
    if session_key.is_none()
        && let Some(registry) = health_registry
        && let Some(shared) = registry
            .shared_for_provider_on_channel(provider, channel_id)
            .await
    {
        token_hash = shared.token_hash.clone();
    }
    let persistence = QueuePersistenceContext::new(provider, &token_hash, None);
    #[cfg(test)]
    run_pre_purge_hook_for_test(provider, channel_id, handle).await;
    // A V2 owner's queue purges only as captured; the head it already attempted stays.
    #[cfg(unix)]
    if let Some(purge) = purge_v2(handle, persistence.clone(), || std::future::ready(())).await {
        return purge;
    }
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
    Some((purge, None))
}

#[cfg(all(test, unix))]
#[path = "cancel_effects_v2_tests.rs"]
mod v2_tests;

/// `None` without a V2 owner; a kept queue answers the owner's depth and reason, nothing drained,
/// an unanswered, unknown or unmeasured one no purge. `captured` runs between capture and purge.
#[cfg(unix)]
async fn purge_v2<F: std::future::Future<Output = ()>>(
    handle: &ChannelMailboxHandle,
    persistence: QueuePersistenceContext,
    captured: impl FnOnce() -> F,
) -> Option<Option<(PurgeQueueResult, Option<String>)>> {
    let expected = match handle.try_capture_v2().await {
        Ok(Some(expected)) => expected,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%error, "V2 force purge capture failed; queue preserved");
            return Some(None);
        }
    };
    captured().await;
    let purge = handle.try_purge_queue_if_unchanged(persistence, expected);
    Some(purge.await.ok().and_then(|purge| {
        // A kept queue is exactly what the owner measured in that actor turn, so the depth stands.
        let kept = purge.queue_preserved() == Some(true);
        if purge.failed.is_some() && !kept {
            return None;
        }
        let result = PurgeQueueResult {
            queued_head_refused: kept,
            drained: purge.receipt.len(),
            queue_len_after: purge.queue_len_after?,
            ..Default::default()
        };
        Some((result, purge.failed.map(|(_, reason)| reason)))
    }))
}

/// The forced Herdr cancel's effects; the terminate service they run under is unix-only.
#[cfg(unix)]
pub(super) mod herdr {
    use sqlx::PgPool;

    use super::{QueuePersistenceContext, SessionIdentity, TurnLifecycleTarget};
    use crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg;
    use futures::FutureExt;
    use futures::future::BoxFuture;

    use crate::services::discord::TmuxCleanupPolicy;
    use crate::services::discord::herdr_terminate::RetiredExclusion;
    use crate::services::discord::herdr_terminate::http_force::Released;
    use crate::services::turn_cancel_finalizer::{
        FinalizeTurnCancelRequest, FinalizedTurnCancel, TurnCancelCorrelation, finalize_turn_cancel,
    };
    use crate::services::turn_cancel_queue_guard::{CancelQueueLoss, record_purge_receipt_on};
    use crate::services::turn_lifecycle::cleanup_policy_observability_surface;
    use crate::services::turn_orchestrator::checked_purge::ExpectedQueue;

    pub(in crate::services::queue) const HERDR_TERMINATE: &str = "herdr-terminate";

    /// What a forced Herdr cancel applied under the terminate's exclusion. `failed` names the first
    /// step that did not apply; nothing after it ran but the record of what the purge removed.
    /// `loss` is `None` when the channel had no actor.
    pub(in crate::services::queue) struct HerdrForceEffects {
        pub(in crate::services::queue) failed: Option<String>,
        /// The dispatch cancel and the session disconnect both applied.
        pub(in crate::services::queue) disconnected: bool,
        pub(in crate::services::queue) purge: Option<QueuePurge>,
        pub(in crate::services::queue) loss: Option<CancelQueueLoss>,
        pub(in crate::services::queue) cancelled: FinalizedTurnCancel,
    }

    /// The actor's answer to the checked purge.
    #[derive(Clone, Copy)]
    pub(in crate::services::queue) struct QueuePurge {
        pub(in crate::services::queue) removed: usize,
        /// True only when the actor kept its queue unchanged; `None` when that is unknown.
        pub(in crate::services::queue) preserved: Option<bool>,
        pub(in crate::services::queue) queue_len_after: Option<usize>,
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
        let (failed, disconnected, purge, loss) =
            herdr_force_steps(exclusion, &pool, &session_key).await;
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
            queue_preserved: purge.and_then(|purge| purge.preserved),
            termination_recorded: true,
            completed_at: chrono::Utc::now(),
        };
        HerdrForceEffects {
            failed,
            disconnected,
            purge,
            loss,
            cancelled: finalize_turn_cancel(request),
        }
    }

    type Steps = (
        Option<String>,
        bool,
        Option<QueuePurge>,
        Option<CancelQueueLoss>,
    );

    async fn herdr_force_steps(
        exclusion: &RetiredExclusion,
        pool: &PgPool,
        session_key: &str,
    ) -> Steps {
        if let Some(dispatch) = exclusion.dispatch()
            && let Err(error) =
                cancel_dispatch_and_reset_auto_queue_on_pg(pool, dispatch, None).await
        {
            return (Some(format!("dispatch cancel: {error}")), false, None, None);
        }
        match exclusion.disconnect_session(pool).await {
            Ok(true) => {}
            Ok(false) => return (Some("session row moved on".into()), false, None, None),
            Err(error) => return (Some(error), false, None, None),
        }
        let Some(actor) = exclusion.actor() else {
            return (None, true, None, None);
        };
        let (provider, channel) = (exclusion.provider(), exclusion.channel());
        let target = TurnLifecycleTarget {
            provider: Some(provider.clone()),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        // The capture, the purge it conditions and the record of its receipt all address the
        // exclusion's actor; a queue the actor no longer holds as captured is refused, not purged.
        let token = SessionIdentity::parse(session_key).and_then(|identity| identity.token_hash);
        let persistence =
            QueuePersistenceContext::new(provider, token.as_deref().unwrap_or_default(), None);
        // Only the runtime's own namespace is absorbed first; another one is captured as held.
        let captured = actor.try_capture_v2().await;
        let v2 = matches!(captured, Ok(Some(_)));
        let capture = match (captured, token.as_deref() == exclusion.owner_token()) {
            (Ok(Some(expected)), _) => Ok(expected),
            (Err(why), _) => Err(format!("queue capture: {why}")),
            (Ok(None), true) => match actor.try_capture_own_queue(persistence.clone()).await {
                Ok(captured) => captured.map_err(|why| format!("queue hydrate: {why}")),
                Err(_) => Err("queue hydrate: the mailbox actor did not answer".to_string()),
            }
            .map(|snapshot| ExpectedQueue::of(&snapshot)),
            (Ok(None), false) => (actor.try_snapshot().await)
                .map(|snapshot| ExpectedQueue::of(&snapshot))
                .map_err(|_| "queue capture: the mailbox actor did not answer".to_string()),
        };
        let checked = match capture {
            Ok(expected) => {
                #[cfg(test)]
                super::run_pre_purge_hook_for_test(provider, channel, actor).await;
                let purge = actor.try_purge_queue_if_unchanged(persistence, expected);
                purge
                    .await
                    .map_err(|_| "queue purge: the mailbox actor did not answer".to_string())
            }
            Err(why) => Err(why),
        };
        let receipt = checked
            .as_ref()
            .map_or(&[][..], |purge| purge.receipt.as_slice());
        let failed = match &checked {
            Ok(purge) => {
                (purge.failed.as_ref()).map(|(stage, why)| format!("queue purge {stage:?}: {why}"))
            }
            Err(why) => Some(why.clone()),
        };
        let reason = "queue_api_cancel_turn_force";
        let complete = failed.is_none();
        let loss = record_purge_receipt_on(actor, &target, receipt, complete, Some(pool), reason);
        let mut loss = loss.await;
        // A V2 owner's queue is not in the legacy snapshot the receipt measured: the depth is what
        // the owner measured in its purge turn, or none.
        if v2 {
            loss.queue_depth_after =
                (checked.as_ref().ok()).and_then(|purge| purge.queue_len_after);
        }
        crate::services::queue::settle_v2_drops(Some(pool), actor, provider.as_str(), channel)
            .await;
        let purge = checked.as_ref().ok().map(|purge| QueuePurge {
            removed: purge.receipt.len(),
            preserved: purge.queue_preserved(),
            queue_len_after: purge.queue_len_after,
        });
        (failed, true, purge, Some(loss))
    }
}

/// A removed input's receipt commit is retried twice before it stays owed.
const RECORD_RETRY_DELAYS: [std::time::Duration; 3] = [
    std::time::Duration::ZERO,
    std::time::Duration::from_millis(250),
    std::time::Duration::from_secs(1),
];

/// Commits each receipt `channel`'s V2 owner owes for an input it removed unrun, at its removal
/// instant, then acks exactly the committed ones; the rest stay owed for the next settle or boot.
pub(crate) async fn settle_v2_drops(
    pool: Option<&sqlx::PgPool>,
    mailbox: &crate::services::turn_orchestrator::ChannelMailboxHandle,
    provider: &str,
    channel: ChannelId,
) {
    let Some(pool) = pool else {
        return;
    };
    let owed = mailbox.v2_request(|slot| Ok(slot.as_ref().map(|o| o.owed_drops())));
    let owed = match owed.await {
        Ok(Some(owed)) => owed,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(channel_id = channel.get(), %error, "owed drop receipts unread");
            return;
        }
    };
    let mut done = Vec::new();
    for owed in owed {
        if record_drop(pool, provider, channel, &owed).await {
            done.push(owed);
        }
    }
    if done.is_empty() {
        return;
    }
    let acked = mailbox.v2_request(move |slot| match slot.as_mut() {
        Some(owner) => owner.ack_owed_drops(&done),
        None => Ok(()),
    });
    if let Err(error) = acked.await {
        tracing::warn!(channel_id = channel.get(), %error, "owed drop receipts stay owed");
    }
}

async fn record_drop(
    pool: &sqlx::PgPool,
    provider: &str,
    channel: ChannelId,
    owed: &crate::services::turn_orchestrator::OwedDrop,
) -> bool {
    let Some(at) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(owed.at_ms as i64) else {
        return false;
    };
    let turn_id = format!("discord:{}:{}", channel.get(), owed.message_id);
    let channel_id = channel.get().to_string();
    let receipt = crate::db::turns::TurnReceipt {
        turn_id: &turn_id,
        channel_id: &channel_id,
        agent_id: None,
        provider,
        session_key: None,
        started_at: None,
        consumed_origin_ids: &owed.origins,
        outcome: owed.outcome,
        confirmed_at: at,
    };
    for delay in RECORD_RETRY_DELAYS {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        match commit_drop(pool, &receipt).await {
            Ok(()) => return true,
            Err(error) => tracing::warn!(%error, turn_id, "dropped input receipt not committed"),
        }
    }
    false
}

/// Commits a removed input's receipt in one statement: the first terminal wins and origins
/// accumulate, and a row an unrun attempt left without a terminal takes the removal's shape.
async fn commit_drop(
    pool: &sqlx::PgPool,
    receipt: &crate::db::turns::TurnReceipt<'_>,
) -> Result<(), sqlx::Error> {
    let sql = "INSERT INTO turns (turn_id, channel_id, provider, started_at, finished_at,
                   consumed_origin_ids, terminal_outcome, terminal_confirmed_at)
               VALUES ($1, $2, $3, $4, $4, $5, $6, $4)
               ON CONFLICT(turn_id) DO UPDATE SET
                   consumed_origin_ids = ARRAY(
                       SELECT DISTINCT origin
                       FROM unnest(turns.consumed_origin_ids || EXCLUDED.consumed_origin_ids) AS origin
                       ORDER BY origin),
                   started_at = CASE WHEN turns.terminal_outcome IS NULL
                       THEN EXCLUDED.started_at ELSE turns.started_at END,
                   finished_at = CASE WHEN turns.terminal_outcome IS NULL
                       THEN EXCLUDED.finished_at ELSE turns.finished_at END,
                   session_id = CASE WHEN turns.terminal_outcome IS NULL
                       THEN NULL ELSE turns.session_id END,
                   duration_ms = CASE WHEN turns.terminal_outcome IS NULL
                       THEN NULL ELSE turns.duration_ms END,
                   terminal_outcome = COALESCE(turns.terminal_outcome, EXCLUDED.terminal_outcome),
                   terminal_confirmed_at =
                       COALESCE(turns.terminal_confirmed_at, EXCLUDED.terminal_confirmed_at)";
    sqlx::query(sql)
        .bind(receipt.turn_id)
        .bind(receipt.channel_id)
        .bind(receipt.provider)
        .bind(receipt.confirmed_at)
        .bind(receipt.consumed_origin_ids)
        .bind(receipt.outcome.as_str())
        .execute(pool)
        .await?;
    Ok(())
}
