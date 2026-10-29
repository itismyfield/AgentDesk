//! A Herdr launch that resumes the transcript its pane already names.

use super::*;

/// The nonce of the Herdr execution a resume launched on `tmux_session`; its own transcript may be
/// the one the pane already names.
pub(crate) fn herdr_resume_launch(tmux_session: &str) -> Option<String> {
    use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
    let herdr = crate::services::session_host::HostKind::Herdr;
    if read_host_kind_marker(tmux_session) != HostKindMarker::Known(herdr) {
        return None;
    }
    let SpawnNonceMarker::Known(nonce) = observe_spawn_nonce_marker(tmux_session) else {
        return None;
    };
    (launch_mode("claude", &nonce).as_deref() == Some("resume")).then_some(nonce)
}

/// A Herdr launch resuming the source its pane already names is still a new execution there: its
/// first record names it, so the row can bind it. Every other repeat of a source stays unchanged.
pub(super) fn resumes_under_new_execution(p: &Proposal, logged: Option<&str>) -> bool {
    p.provider == "claude"
        && matches!(p.cause, CauseSource::Launch)
        && herdr_resume_launch(p.tmux_session).is_some_and(|nonce| logged != Some(nonce.as_str()))
}
