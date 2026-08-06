/// #5170: why this intake is being handed back to the durable queue.
///
/// The two causes look identical downstream (same `Intervention`, same durable
/// enqueue) but they carry opposite information about *who else is running*:
///
/// * [`Self::RaceLoss`] — the mailbox start-turn claim was taken by another
///   turn. There is a live opponent that owns the completion wake edge, and the
///   post-enqueue recheck exists only to cover the case where that opponent had
///   already finished before our enqueue became visible.
/// * [`Self::SessionTransitionBusy`] — nobody claimed the mailbox. The
///   per-channel session transition lock merely happened to be held at the
///   instant intake tried to take it. Treating that as a lost race is what
///   armed an immediate re-kick, whose own transition wait then guaranteed the
///   next intake would fail the same way (#5170).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::router::message_handler) enum QueuedIntakeCause {
    RaceLoss,
    SessionTransitionBusy,
}

impl QueuedIntakeCause {
    /// A real race loss owns an edge-trigger recheck (the opponent may already
    /// be gone). A transition-busy requeue owns no edge at all — the transition
    /// holder is still mid-flight — so it hands the channel to the slow
    /// fail-open backstop instead of re-kicking into the same busy lock.
    pub(super) fn wants_immediate_idle_recheck(self) -> bool {
        matches!(self, Self::RaceLoss)
    }
}
