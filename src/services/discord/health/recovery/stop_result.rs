//! Results from canonical provider runtime stop and repair.
pub use crate::services::engine_runtime::health_port::{
    FINISH_UNOBSERVED_PATH, FinishCancelledMailboxResult, InflightDisposition,
    RuntimeTurnStopResult, TOKEN_SUPERSEDED_PATH,
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
