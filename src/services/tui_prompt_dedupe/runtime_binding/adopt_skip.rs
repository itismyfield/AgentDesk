//! Why a hook's continuation adoption changed nothing, so the receiver can tell a pane it does
//! not manage from one whose binding is not ready yet.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdoptSkip {
    PayloadNotUuid,
    UnmappedCommandSession,
    NotClaudeTui,
    MalformedBindingPath,
    /// The pane has no channel, so there is no log to write; a verified transcript is adopted in
    /// memory only.
    NoChannel,
    /// A file-less candidate on a pane the rehydrate pass maps, whose channel mapping lapsed.
    ChannelNotRestored,
    /// The command session names a pane whose runtime binding lapsed; the next pass registers it.
    RuntimeNotRestored,
    /// A return to a session the pane left that the hook could not prove; nothing is adopted.
    ResumeConflict,
    /// The hook names no transcript, so there is no candidate to check.
    PayloadPathMissing,
    /// The pane's binding event log could not be loaded; the hook is retried.
    HistoryUnreadable,
    SourceRejected(crate::services::claude_tui::source_verify::SourceRejection),
    /// The bound transcript was replaced or rewritten; the pane stays as it is.
    SourceAnomaly,
    /// The bound transcript's pinned file could not be read; the hook is retried.
    SourceUnreadable,
    /// A Herdr pane whose execution the latest reconcile did not admit; the hook is retried.
    HostNotAdmitted,
}

impl AdoptSkip {
    /// The pane a command session names and its binding. Only Claude TUI panes get a claude alias
    /// and explicit clears drop it with the binding, so a named pane without one only lapsed.
    /// A withheld Herdr pane is not handed out, so its source stays where it is.
    pub(super) fn bound_pane<'s>(
        state: &'s TuiPromptDedupeState,
        command_key: &PromptKey,
        skip: &mut Option<Self>,
    ) -> Option<(String, &'s TimedValue<TuiRuntimeBinding>)> {
        *skip = Some(Self::UnmappedCommandSession);
        let tmux_session_name = &state.tmux_by_provider_session.get(command_key)?.value;
        *skip = Some(Self::RuntimeNotRestored);
        let binding = state.runtime_by_tmux.get(tmux_session_name)?;
        *skip = Some(Self::HostNotAdmitted);
        if herdr_execution_withheld(tmux_session_name) {
            return None;
        }
        Some((tmux_session_name.clone(), binding))
    }

    /// Why a candidate logs nothing on a pane without a channel mapping; a file-less one on a pane
    /// the rehydrate pass maps is refused until that pass restores the mapping.
    pub(super) fn unlogged(no_channel: bool, tmux_session: &str, candidate: &str) -> Option<Self> {
        let mapped_by_pass = || super::super::pending::last_restore_outcome(tmux_session).is_some();
        let file_less = || !std::path::Path::new(candidate).is_file();
        match no_channel {
            true if file_less() && mapped_by_pass() => Some(Self::ChannelNotRestored),
            true => Some(Self::NoChannel),
            false => None,
        }
    }
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

/// Herdr panes by logical key: the latest installed or judged execution's nonce and whether a
/// reconcile admitted it. A withheld pane keeps its binding and cursor; tmux panes are not listed.
static HERDR_EXECUTIONS: LazyLock<Mutex<HashMap<String, (String, bool)>>> =
    LazyLock::new(Default::default);
// Until a Herdr pane is listed the hook path reads this flag and takes no lock; tests keep the flag
// per thread but run the same atomic operations on it.
#[cfg(not(test))]
static LISTED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
thread_local! {
    static LISTED: AtomicBool = const { AtomicBool::new(false) };
    pub(crate) static HERDR_HOLD_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
use std::sync::atomic::{
    AtomicBool,
    Ordering::{AcqRel, Acquire},
};

fn listed<R>(read: impl FnOnce(&AtomicBool) -> R) -> R {
    #[cfg(not(test))]
    return read(&LISTED);
    #[cfg(test)]
    LISTED.with(read)
}

/// Changes `logical`'s hold in the source authority hooks adopt under, so a hook past its check
/// commits before the change returns and a later hook sees the change.
fn change_hold(logical: &str, change: impl FnOnce(&mut HashMap<String, (String, bool)>)) {
    crate::services::tmux_common::with_tmux_source_authority(logical, |_| {
        listed(|flag| flag.fetch_or(true, AcqRel));
        change(&mut HERDR_EXECUTIONS.lock().unwrap_or_else(|p| p.into_inner()));
    });
}

/// A launch installed execution `nonce` on `logical`: it is held until a reconcile admits it, and
/// an earlier execution's admission ends here.
pub(crate) fn install_herdr_execution(logical: &str, nonce: &str) {
    change_hold(logical, |executions| {
        executions.insert(logical.to_owned(), (nonce.to_owned(), false));
    });
}

/// The reconcile admitted execution `nonce` on `logical`, unless a newer one is installed there.
pub(crate) fn admit_herdr_execution(logical: &str, nonce: &str) {
    change_hold(logical, |executions| match executions.get(logical) {
        Some((listed, _)) if listed != nonce => {}
        _ => drop(executions.insert(logical.to_owned(), (nonce.to_owned(), true))),
    });
}

/// The reconcile refused execution `nonce` on `logical`, or could not name one (`None`): that
/// only withholds a listed pane, and a refusal of another execution leaves the listed one alone.
pub(crate) fn withhold_herdr_execution(logical: &str, nonce: Option<&str>) {
    change_hold(logical, |executions| {
        match (executions.get_mut(logical), nonce) {
            (Some((listed, _)), Some(nonce)) if nonce != listed => {}
            (Some((_, admitted)), _) => *admitted = false,
            (None, Some(nonce)) => {
                drop(executions.insert(logical.to_owned(), (nonce.to_owned(), false)))
            }
            (None, None) => {}
        }
    });
}

/// Whether hooks must leave the pane on its current source; the lock is a leaf.
fn herdr_execution_withheld(logical: &str) -> bool {
    if !listed(|flag| flag.load(Acquire)) {
        return false;
    }
    #[cfg(test)]
    HERDR_HOLD_LOOKUPS.set(HERDR_HOLD_LOOKUPS.get() + 1);
    let executions = HERDR_EXECUTIONS.lock();
    let executions = executions.unwrap_or_else(|poison| poison.into_inner());
    executions
        .get(logical)
        .is_some_and(|(_, admitted)| !admitted)
}
