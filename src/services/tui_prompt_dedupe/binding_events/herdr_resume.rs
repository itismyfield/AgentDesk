//! A Herdr launch that resumes the transcript its pane already names.

use super::*;

/// The nonce of the Herdr execution a resume launched on `tmux_session`, the one its launch listed
/// there; its own transcript may be the one the pane already names.
pub(crate) fn herdr_resume_launch(tmux_session: &str) -> Option<String> {
    let SpawnNonceMarker::Known(nonce) = observe_spawn_nonce_marker(tmux_session) else {
        return None;
    };
    if launch_mode("claude", &nonce).as_deref() != Some("resume") {
        return None;
    }
    let listed = crate::services::tui_prompt_dedupe::herdr_listed_execution(tmux_session);
    (listed.as_deref() == Some(nonce.as_str())).then_some(nonce)
}

/// Whether the pane's latest record names execution `nonce` on `session_id`.
pub(crate) fn logged_by(
    channel_id: u64,
    tmux_session: &str,
    nonce: &str,
    session_id: &str,
) -> bool {
    let Ok(events) = binding_events_since(channel_id, 0) else {
        return false;
    };
    events
        .iter()
        .rev()
        .find(|event| event.tmux_session == tmux_session)
        .is_some_and(|event| {
            event.execution_nonce.as_deref() == Some(nonce)
                && matches!(&event.new, BindingTarget::Source(source)
                    | BindingTarget::Resolved { source, .. } if source.session_id == session_id)
        })
}

/// A Herdr launch resuming the source its pane already names is still a new execution there: its
/// first record names it, so the row can bind it. Every other repeat of a source stays unchanged.
pub(super) fn resumes_under_new_execution(p: &Proposal, logged: Option<&str>) -> bool {
    p.provider == "claude"
        && matches!(p.cause, CauseSource::Launch)
        && herdr_resume_launch(p.tmux_session).is_some_and(|nonce| logged != Some(nonce.as_str()))
}
