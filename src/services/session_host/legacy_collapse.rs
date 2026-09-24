//! Named bool collapses of the three-state probes, so each lossy call site
//! stays countable while it keeps today's semantics.

use super::model::{HostKind, HostLiveness, HostPresence, HostSessionRef};
use crate::services::tmux_diagnostics;

/// Same answer as `platform::tmux::has_session`: `ProbeFailed` reads as missing.
pub(crate) fn probe_failed_to_missing(presence: HostPresence) -> bool {
    presence == HostPresence::Present
}

/// Only a confirmed dead pane counts as dead; `ProbeError` preserves.
pub(crate) fn dead_only_if_dead_or_absent(liveness: HostLiveness) -> bool {
    liveness == HostLiveness::DeadOrAbsent
}

/// The existing bool probe unchanged, including its unbounded `list-panes`.
pub(crate) fn has_live_pane_bool(session: HostSessionRef<'_>) -> bool {
    debug_assert_eq!(session.kind, HostKind::Tmux);
    tmux_diagnostics::tmux_session_has_live_pane(session.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_collapse_matches_has_session() {
        assert!(probe_failed_to_missing(HostPresence::Present));
        assert!(!probe_failed_to_missing(HostPresence::Missing));
        assert!(!probe_failed_to_missing(HostPresence::ProbeFailed));
    }

    #[test]
    fn liveness_collapse_treats_probe_error_as_not_dead() {
        assert!(dead_only_if_dead_or_absent(HostLiveness::DeadOrAbsent));
        assert!(!dead_only_if_dead_or_absent(HostLiveness::Live));
        assert!(!dead_only_if_dead_or_absent(HostLiveness::ProbeError));
    }

    #[test]
    fn blank_name_collapses_like_the_platform_probes() {
        use crate::services::platform::tmux;
        // Blank names short-circuit before any tmux process is spawned.
        let blank = HostSessionRef::tmux("");
        assert_eq!(
            probe_failed_to_missing(tmux::session_presence("").into()),
            tmux::has_session("")
        );
        assert_eq!(
            has_live_pane_bool(blank),
            tmux_diagnostics::tmux_session_has_live_pane("")
        );
        assert!(!has_live_pane_bool(blank));
    }
}
