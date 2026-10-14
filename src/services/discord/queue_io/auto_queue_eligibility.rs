//! Whether an automatic entry point may offer the queue's first ready input to a busy TUI,
//! decided apart from whether it may start an idle turn: a live turn ends the start, not the paste.
use super::*;
use crate::services::discord::busy_followup_retry_store::input_class;
use crate::services::turn_orchestrator::prefix::{self, Prefix};

/// What one look at a channel found for the queued-only lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum QueuedReach {
    /// The switch or the channel gate is closed: nothing beyond the entry point's own rule runs.
    Closed,
    /// Nothing typable is ready to paste, or the channel is held by recovery or cleanup.
    NoCandidate,
    /// The gate, the thread's parent, a retry record or the REST handle could not be read; look
    /// again in a minute.
    Wait,
    /// A typable first ready input stands behind nothing but parked ones.
    Ready,
}

impl QueuedReach {
    /// Whether the slow backstop keeps watching the channel for this lane.
    pub(in crate::services::discord) fn rearms(self) -> bool {
        matches!(self, Self::Ready | Self::Wait)
    }
}

/// The holds every automatic lane keeps: recovery, a terminal cleanup retry and a pending
/// synthetic start.
fn held(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> bool {
    snapshot.recovery_started_at.is_some()
        || cleanup_retry_inflight_blocks_idle_kickoff(shared, provider, channel_id)
        || tui_direct_pending_start::pending_synthetic_start_blocks_idle_kickoff(
            provider.as_str(),
            channel_id.get(),
        )
}

/// The switch is read first: closed, no REST call, classifier read or claim follows. The gate is
/// then asked with the channel's actual parent, over `http` or, for `None`, the runtime's handle.
pub(in crate::services::discord) async fn queued_reach(
    http: Option<&Arc<serenity::Http>>,
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> QueuedReach {
    if !health::InjectOrigin::queued_head_ready(provider, channel_id) {
        return QueuedReach::Closed;
    }
    if snapshot.intervention_queue.is_empty() || held(shared, provider, channel_id, snapshot) {
        return QueuedReach::NoCandidate;
    }
    let runtime_http;
    let http = match http {
        Some(http) => http,
        None => {
            runtime_http = shared.serenity_http_or_token_fallback();
            let Some(http) = runtime_http.as_ref() else {
                return QueuedReach::Wait;
            };
            http
        }
    };
    // An unread parent is no parent-less channel: a thread its parent gates would read closed.
    let Ok(parent) = session_runtime::checked_thread_parent(http, channel_id).await else {
        return QueuedReach::Wait;
    };
    let gate = router::queued_head_inject_covers;
    if !health::InjectOrigin::queued_head_gate_enabled(provider, channel_id, parent, gate) {
        return QueuedReach::Closed;
    }
    let class = |item: &Intervention| input_class(provider, channel_id, item);
    let queue = &snapshot.intervention_queue;
    match prefix::select(queue, class) {
        Prefix::Ready { at } if prefix::eligible(&queue[at]) => QueuedReach::Ready,
        Prefix::Unavailable => QueuedReach::Wait,
        // The actor refuses an untypable first ready input; the idle start takes it later.
        Prefix::Ready { .. } | Prefix::None => QueuedReach::NoCandidate,
    }
}

/// The queued-only lane of an entry point whose own idle rule said no: paste the first ready
/// input if one stands, then leave the slow backstop to watch whatever remains.
pub(in crate::services::discord) async fn run_queued_lane(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
    reason: &'static str,
) {
    let reach = queued_reach(None, shared, provider, channel_id, snapshot).await;
    if reach == QueuedReach::Ready {
        kick_queued_only_channel_if_context_available(shared, provider, channel_id, reason).await;
    }
    if reach.rearms() {
        arm_slow_idle_queue_backstop_if_queue_nonempty(shared, provider, channel_id, reason).await;
    }
}
