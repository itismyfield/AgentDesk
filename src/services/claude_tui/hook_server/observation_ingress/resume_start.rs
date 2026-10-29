//! A Herdr resume's own SessionStart is logged under its execution before the hook is answered,
//! so a restart between that record and the attach still attaches the Pending row.

use super::NotDurableReason;
use crate::services::tui_prompt_dedupe::binding_context::{CapturedContext, HookBindingEnvelope};
use crate::services::tui_prompt_dedupe::binding_events::{
    CauseSource, HookSignal, Proposal, herdr_resume_launch, record_source,
};

/// Only the resumed start its own launch context names is logged; every other hook passes as is.
pub(super) fn record(
    payload_session: &str,
    hook: &HookSignal,
    envelope: Option<&HookBindingEnvelope>,
) -> Result<(), NotDurableReason> {
    if hook.event != "session_start" || hook.source.as_deref() != Some("resume") {
        return Ok(());
    }
    let Some(CapturedContext::Captured(context)) = envelope.map(|envelope| &envelope.context)
    else {
        return Ok(());
    };
    let (Some(path), Some(channel_id)) = (hook.transcript_path.as_deref(), context.channel_id)
    else {
        return Ok(());
    };
    let own = context.provider == "claude"
        && context.launch_mode == "resume"
        && context.expected_native_session_id.as_deref() == Some(payload_session)
        && herdr_resume_launch(&context.tmux_session).as_deref()
            == Some(context.execution_nonce.as_str());
    if !own {
        return Ok(());
    }
    let tmux = context.tmux_session.as_str();
    let proposal = Proposal {
        channel_id,
        provider: "claude",
        tmux_session: tmux,
        session_id: Some(payload_session),
        path,
        replaced: None,
        cause: CauseSource::Launch,
        hook: Some(hook),
    };
    crate::services::tmux_common::with_tmux_source_authority(tmux, |_| record_source(&proposal))
        .map(|_| ())
        .map_err(|error| {
            tracing::warn!(tmux, %error, "resumed start not logged; the hook retries");
            NotDurableReason::Append
        })
}
