use super::*;

#[cfg_attr(not(test), allow(dead_code))]
fn prune_interventions_at(queue: &mut Vec<Intervention>, now: Instant) {
    queue.retain(|i| now.duration_since(i.created_at) <= INTERVENTION_TTL);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
}

#[allow(dead_code)]
pub(super) fn channel_has_pending_soft_queue(
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
) -> bool {
    channel_has_pending_soft_queue_at(intervention_queue, channel_id, Instant::now())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn channel_has_pending_soft_queue_at(
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
    now: Instant,
) -> bool {
    let mut remove_queue = false;
    let has_pending = if let Some(queue) = intervention_queue.get_mut(&channel_id) {
        prune_interventions_at(queue, now);
        let has_pending = queue.iter().any(|item| item.mode == InterventionMode::Soft);
        remove_queue = queue.is_empty();
        has_pending
    } else {
        false
    };
    if remove_queue {
        intervention_queue.remove(&channel_id);
    }
    has_pending
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn watcher_should_kickoff_idle_queue(
    has_active_turn: bool,
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
) -> bool {
    watcher_should_kickoff_idle_queue_at(
        has_active_turn,
        intervention_queue,
        channel_id,
        Instant::now(),
    )
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn watcher_should_kickoff_idle_queue_at(
    has_active_turn: bool,
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
    now: Instant,
) -> bool {
    if has_active_turn {
        return false;
    }
    channel_has_pending_soft_queue_at(intervention_queue, channel_id, now)
}

pub(super) fn schedule_deferred_idle_queue_kickoff(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) {
    shared
        .deferred_hook_backlog
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if let (Some(ctx), Some(tok)) = (
            shared.cached_serenity_ctx.get(),
            shared.cached_bot_token.get(),
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🚀 Deferred drain: kicking off idle queues for channel {} ({reason})",
                channel_id
            );
            super::kickoff_idle_queues(ctx, &shared, tok, &provider).await;
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ Deferred drain: missing cached context for channel {} ({reason})",
                channel_id
            );
        }
        shared
            .deferred_hook_backlog
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    });
}
