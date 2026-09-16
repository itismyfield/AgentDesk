//! Results from canonical provider runtime stop and repair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStopResult {
    pub lifecycle_path: &'static str,
    pub had_active_turn: bool,
    pub queue_depth: usize,
    pub persistent_inflight_cleared: bool,
    pub termination_recorded: bool,
    /// #5176 — whether this stop actually took the mailbox foreground anchor.
    /// `true` also covers "the mailbox was already free when we checked": the
    /// contract this field reports is *ownership released*, and the caller only
    /// needs to know whether the channel is still locked.
    pub mailbox_foreground_free: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleTmuxStaleTurnRepairResult {
    pub had_active_turn: bool,
    pub has_pending_queue: bool,
    pub persistent_inflight_cleared: bool,
    pub runtime_session_cleared: bool,
}
