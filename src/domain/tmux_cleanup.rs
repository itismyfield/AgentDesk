//! Session cleanup choice a turn stop carries: keep the tmux session, keep it with the inflight
//! record for a restart, or tear it down with an optional termination reason.
use crate::domain::lifecycle::InflightRestartMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TmuxCleanupPolicy {
    PreserveSession,
    PreserveSessionAndInflight {
        restart_mode: InflightRestartMode,
    },
    CleanupSession {
        termination_reason_code: Option<&'static str>,
    },
}

impl TmuxCleanupPolicy {
    pub const fn preserves_inflight(self) -> Option<InflightRestartMode> {
        match self {
            Self::PreserveSessionAndInflight { restart_mode } => Some(restart_mode),
            Self::PreserveSession | Self::CleanupSession { .. } => None,
        }
    }

    pub const fn should_cleanup_tmux(self) -> bool {
        matches!(self, Self::CleanupSession { .. })
    }

    pub const fn should_clear_inflight(self) -> bool {
        !matches!(self, Self::PreserveSessionAndInflight { .. })
    }
}
