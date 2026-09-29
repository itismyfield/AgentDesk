//! Why a hook's continuation adoption changed nothing, so the receiver can tell a pane it does
//! not manage from one whose binding is not ready yet.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdoptSkip {
    PayloadNotUuid,
    UnmappedCommandSession,
    NotClaudeTui,
    MalformedBindingPath,
    /// Adopted in memory only: the pane has no channel, so there is no log to write.
    NoChannel,
    MtimeUnreadable,
    OlderThanBound,
}

type Explained = (Option<(String, String)>, Option<AdoptSkip>);

/// `adopt_claude_continuation_session` plus the reason when nothing was adopted or logged.
pub(crate) fn adopt_claude_continuation_explained(
    command_session_id: &str,
    payload_session_id: &str,
    hook: &HookSignal,
) -> Result<Explained, BindingPersistError> {
    let (mut failure, mut skip) = (None, None);
    let adopted = adopt_continuation(
        command_session_id,
        payload_session_id,
        hook,
        &mut failure,
        &mut skip,
    );
    failure.map_or(Ok((adopted, skip)), Err)
}
