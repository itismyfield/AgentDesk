//! #4260: the intervention-queue capacity-overflow eviction primitive, split
//! out of the giant `turn_orchestrator` root. Capacity eviction (drop-oldest
//! when the queue exceeds `MAX_INTERVENTIONS_PER_CHANNEL`) is the genuine
//! input-loss vector (silent-loss vector 2), so it emits the dedicated
//! `QueueExitKind::Overflow` — the only kind the sink
//! (`apply_queue_exit_feedback`) dead-letters + notifies on. Benign producers
//! (Clear full drain, active-source purge) keep `Superseded` and are never
//! dead-lettered. Every head-drain site routes through here so a capacity evict
//! always produces exit events — never a bare `queue.drain(..)`.

use super::{Intervention, MAX_INTERVENTIONS_PER_CHANNEL, QueueExitEvent, QueueExitKind};

/// Drain the oldest `queue.len() - MAX` entries as `Overflow` exit events.
pub(super) fn drain_head_overflow(queue: &mut Vec<Intervention>) -> Vec<QueueExitEvent> {
    if queue.len() <= MAX_INTERVENTIONS_PER_CHANNEL {
        return Vec::new();
    }
    let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
    queue
        .drain(0..overflow)
        .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Overflow))
        .collect()
}

/// Result of the soft-queue probe. Carries the overflow `QueueExitEvent`s
/// instead of draining eventlessly. Defensive refactor (#4260 dual-review r1):
/// the previous bare `queue.drain(..)` in `has_soft_intervention_at` never
/// caused a real loss — its only live caller (diagnostics `reports.rs`)
/// operates on a throwaway CLONE of the queue — but an eventless drain
/// primitive was one new caller away from becoming one, so the probe now
/// surfaces the events and lets clone-path callers discard them explicitly.
pub(crate) struct SoftInterventionProbe {
    pub(crate) has_pending: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
}
