//! Results from canonical provider runtime stop and repair.
pub use crate::domain::runtime_stop::{
    FINISH_UNOBSERVED_PATH, InflightDisposition, RuntimeTurnStopResult, TOKEN_SUPERSEDED_PATH,
};

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
