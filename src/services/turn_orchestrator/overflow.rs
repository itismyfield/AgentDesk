//! #4260: the intervention-queue overflow-eviction primitive, split out of the
//! giant `turn_orchestrator` root. Draining the oldest entries WITHOUT surfacing
//! a `QueueExitEvent` silently loses queued user input (silent-loss vector 2);
//! every head-drain site routes through here so overflow ALWAYS produces
//! `Superseded` events the sink (`apply_queue_exit_feedback`) can dead-letter +
//! notify on — never a bare `queue.drain(..)`.

use super::{Intervention, MAX_INTERVENTIONS_PER_CHANNEL, QueueExitEvent, QueueExitKind};

/// Drain the oldest `queue.len() - MAX` entries as `Superseded` exit events.
pub(super) fn drain_overflow_superseded(queue: &mut Vec<Intervention>) -> Vec<QueueExitEvent> {
    if queue.len() <= MAX_INTERVENTIONS_PER_CHANNEL {
        return Vec::new();
    }
    let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
    queue
        .drain(0..overflow)
        .map(|intervention| QueueExitEvent::new(intervention, QueueExitKind::Superseded))
        .collect()
}

/// Result of the soft-queue probe. Carries the overflow `QueueExitEvent`s so the
/// eviction can no longer happen silently — the previous `queue.drain(..)` in
/// `has_soft_intervention_at` dropped queued user input with no exit event
/// (silent-loss vector 2, the top-priority defect). Callers on a throwaway CLONE
/// (diagnostics) may discard the events; a live-queue caller must not.
pub(crate) struct SoftInterventionProbe {
    pub(crate) has_pending: bool,
    pub(crate) queue_exit_events: Vec<QueueExitEvent>,
}
