//! Results from canonical provider runtime stop and repair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStopResult {
    pub lifecycle_path: &'static str,
    pub had_active_turn: bool,
    pub queue_depth: usize,
    pub inflight: InflightDisposition,
    pub termination_recorded: bool,
    /// #5176 — whether this stop actually took the mailbox foreground anchor.
    /// `true` also covers "the mailbox was already free when we checked": the
    /// contract this field reports is *ownership released*, and the caller only
    /// needs to know whether the channel is still locked.
    pub mailbox_foreground_free: bool,
}

impl RuntimeTurnStopResult {
    /// The host guard kept the judged turn: nothing was cancelled, finished or cleared.
    pub(crate) fn preserved_by_host_guard(queue_depth: usize) -> Self {
        Self {
            lifecycle_path: "host-guard-preserved",
            had_active_turn: true,
            queue_depth,
            inflight: InflightDisposition::PreservedByHostGuard,
            termination_recorded: false,
            mailbox_foreground_free: false,
        }
    }

    /// The judged turn ended and another took the channel: the stop left the successor alone.
    pub(crate) fn token_superseded(queue_depth: usize, termination_recorded: bool) -> Self {
        Self {
            lifecycle_path: TOKEN_SUPERSEDED_PATH,
            had_active_turn: true,
            queue_depth,
            inflight: InflightDisposition::NotNeeded,
            termination_recorded,
            mailbox_foreground_free: false,
        }
    }

    /// The finish went unobserved: the turn, its row and session are kept as a refused host's.
    pub(crate) fn finish_unobserved(queue_depth: usize) -> Self {
        Self {
            lifecycle_path: FINISH_UNOBSERVED_PATH,
            inflight: InflightDisposition::PreservedByHostGuard,
            ..Self::preserved_by_host_guard(queue_depth)
        }
    }
}

/// A stop whose judged turn was replaced before its finish; the channel stays locked.
pub(crate) const TOKEN_SUPERSEDED_PATH: &str = "token-superseded";
/// A stop whose finish the channel's actor did not answer; nothing was cleared.
pub(crate) const FINISH_UNOBSERVED_PATH: &str = "finish-unobserved";

impl super::HardStopRuntimeResult {
    pub(crate) fn token_superseded(has_pending_queue: bool) -> Self {
        Self {
            cleanup_path: TOKEN_SUPERSEDED_PATH,
            has_pending_queue,
            ..Self::default()
        }
    }

    pub(crate) fn finish_unobserved() -> Self {
        Self {
            cleanup_path: FINISH_UNOBSERVED_PATH,
            had_active_turn: true,
            ..Self::default()
        }
    }
}

/// What a runtime stop did with the channel's persistent inflight row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InflightDisposition {
    Cleared,
    NotNeeded,
    /// The stop was not a legacy one: the row stays for the turn's owner, and no caller clears it.
    PreservedByHostGuard,
}

impl InflightDisposition {
    pub(crate) fn cleared_if(cleared: bool) -> Self {
        if cleared {
            Self::Cleared
        } else {
            Self::NotNeeded
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleTmuxStaleTurnRepairResult {
    pub had_active_turn: bool,
    pub has_pending_queue: bool,
    pub persistent_inflight_cleared: bool,
    pub runtime_session_cleared: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinishCancelledMailboxResult {
    pub cleared_active_turn: bool,
    pub global_active_decremented: bool,
    pub has_pending_queue: bool,
    pub runtime_session_cleared: bool,
}
