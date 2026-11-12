//! Presence consumes observation only; current Idle readiness remains owned by readiness.
use crate::services::claude_tui::activity::{self, Foreground};

pub(super) fn busy(session: &str) -> bool {
    let snapshot = activity::observe(session);
    snapshot.current()
        && matches!(
            snapshot.foreground,
            Foreground::Submitted | Foreground::Generating | Foreground::Tool
        )
}
pub(super) fn ready(session: &str) -> bool {
    crate::services::claude_tui::readiness::observe(session, None).is_some()
}
