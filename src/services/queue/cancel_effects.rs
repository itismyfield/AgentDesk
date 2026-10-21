//! Queue effects a turn cancel applies after its lifecycle stop: the preserved-queue drain and
//! the forced purge.

use std::sync::Arc;

use crate::services::discord::health::HealthRegistry;
use crate::services::discord::session_identity::SessionIdentity;
#[cfg(test)]
use crate::services::provider::ProviderKind;
use crate::services::turn_cancel_queue_guard::PurgeCounts;
use crate::services::turn_lifecycle::TurnLifecycleTarget;
#[cfg(test)]
use poise::serenity_prelude::ChannelId;

#[cfg(test)]
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, Intervention, QueuePersistenceContext,
};
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
async fn run_pre_purge_hook_for_test(
    provider: &ProviderKind,
    channel: ChannelId,
    handle: &ChannelMailboxHandle,
) {
    let entry = PRE_PURGE_HOOK_FOR_TESTS.lock().unwrap().remove(&channel);
    if let Some((items, token)) = entry {
        handle
            .replace_queue(items, QueuePersistenceContext::new(provider, &token, None))
            .await;
    }
}

/// Schedule preserved work and return only the depth observed after the drain attempt.
pub(super) async fn schedule_post_cancel_queue_drain(
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
    let token_hash = session_key
        .and_then(SessionIdentity::parse)
        .and_then(|identity| identity.token_hash)
        .unwrap_or_default();
    let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        provider,
        &token_hash,
        None,
    );
    // #3029(D): force purge must also release the active-turn anchor so the
    // next dispatch is not blocked by a stale `cancel_token` /
    // `active_user_message_id`. The handler only clears an anchor whose token
    // is already `cancelled` (the force-kill above flipped it), so a fresh
    // turn that raced in after the kill keeps its anchor (#2706).
    #[cfg(test)]
    run_pre_purge_hook_for_test(provider, channel_id, &handle).await;
    let purge = handle.try_purge_queue(persistence, true).await.ok()?;
    if purge.queued_head_refused {
        tracing::warn!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            "force purge refused; mailbox preserved"
        );
        return None;
    }
    let purged = purge.drained;
    tracing::info!(
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        purged,
        disk_files_removed = purge.disk_files_removed,
        cleared_active_anchor = purge.cleared_active_anchor,
        session_key,
        "force purged channel mailbox queue"
    );
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
