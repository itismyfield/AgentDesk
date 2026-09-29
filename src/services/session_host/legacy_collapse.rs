//! Named bool collapses of the three-state probes, so each lossy call site
//! stays countable while it keeps today's semantics.

use super::model::{HostKind, HostLiveness, HostPresence, HostSessionRef};
use super::tmux_host::TmuxHost;
use super::traits::InteractiveSessionHost;
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
/// A Herdr ref never reaches tmux and reads as not-dead.
pub(crate) fn has_live_pane_bool(session: HostSessionRef<'_>) -> bool {
    if session.kind == HostKind::Herdr {
        return true;
    }
    debug_assert_eq!(session.kind, HostKind::Tmux);
    tmux_diagnostics::tmux_session_has_live_pane(session.name)
}

/// `probe_failed_to_missing` over the tmux presence probe, taking the name.
pub(crate) fn tmux_present_bool(name: &str) -> bool {
    probe_failed_to_missing(TmuxHost.presence(HostSessionRef::tmux(name)))
}

/// `has_live_pane_bool` for a tmux session name.
pub(crate) fn tmux_live_pane_bool(name: &str) -> bool {
    has_live_pane_bool(HostSessionRef::tmux(name))
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

    #[test]
    fn name_helpers_match_the_wrappers_they_replace() {
        for name in ["", "   "] {
            assert_eq!(
                tmux_present_bool(name),
                tmux_diagnostics::tmux_session_exists(name)
            );
            assert_eq!(
                tmux_live_pane_bool(name),
                tmux_diagnostics::tmux_session_has_live_pane(name)
            );
            assert!(!tmux_present_bool(name));
            assert!(!tmux_live_pane_bool(name));
        }
    }

    #[test]
    fn herdr_ref_is_never_collapsed_into_a_tmux_probe() {
        // A missing tmux session named like the pane would read false.
        assert!(
            has_live_pane_bool(HostSessionRef::herdr_pane(
                "session-host-herdr-no-such-tmux"
            )),
            "a Herdr ref must not reach the tmux live-pane probe"
        );
    }
}
