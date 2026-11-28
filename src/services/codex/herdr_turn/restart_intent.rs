//! Only the greeting of an explicit restart, or a launch an effort restart or an idle end owes its
//! thread, may resume the conversation its predecessor ended.
use std::cell::RefCell;

use super::CodexHerdrTurn;
use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
use crate::services::tui_o::store::OStore;
use crate::services::tui_prompt_dedupe::binding_context::ResumeStart;
use crate::services::tui_prompt_dedupe::binding_events::SourceId;

pub(crate) mod idle_resume;

thread_local! {
    static OPERATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

struct Restored(Option<String>);
impl Drop for Restored {
    fn drop(&mut self) {
        OPERATION.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

/// Runs `turn` on this thread as the greeting of restart `operation`; `None` changes nothing.
pub fn scope<R>(operation: Option<String>, turn: impl FnOnce() -> R) -> R {
    let Some(operation) = operation else {
        return turn();
    };
    let _restored = Restored(OPERATION.with(|slot| slot.borrow_mut().replace(operation)));
    turn()
}

/// The thread an effort restart ended with execution `retired_nonce`, which the next launch from
/// that retired row resumes. `required`: a continuation waits for it and runs nowhere else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffortResume {
    pub operation: String,
    pub native_session: String,
    pub retired_nonce: String,
    pub required: bool,
}

/// The native session and the read boundary this turn resumes; `Ok(None)` launches fresh. A
/// restart greeting falls back to fresh; an effort resume never launches another thread.
pub(super) fn resume_of(
    turn: &CodexHerdrTurn<'_>,
    effort: Option<EffortResume>,
) -> Result<Option<(String, ResumeStart)>, String> {
    if let Some(operation) = OPERATION.with(|slot| slot.borrow().clone()) {
        return Ok(match capture(turn, operation) {
            Ok(resume) => Some(resume),
            Err(reason) => {
                let channel = turn.channel_id;
                tracing::info!(channel, %reason, "codex restart greeting launches a fresh session");
                None
            }
        });
    }
    // An owner's effort intent for this execution goes first; else the thread an idle end kept.
    let retired = match turn.row {
        Some(HostedRecord::Known(record))
            if record.state == HostedState::Retired && record.owner == turn.owner =>
        {
            Some(record.execution_nonce.as_str())
        }
        _ => None,
    };
    let effort = match effort {
        Some(e) if e.required || retired == Some(e.retired_nonce.as_str()) => Some(e),
        other => retired
            .and_then(|nonce| idle_resume::kept(&turn.owner, nonce))
            .or(other),
    };
    let Some(effort) = effort else {
        return Ok(None);
    };
    // Once another execution holds the row, the restart's resume is spent.
    let ours = matches!(turn.row, Some(HostedRecord::Known(record))
        if record.state == HostedState::Retired && record.owner == turn.owner
            && record.execution_nonce == effort.retired_nonce);
    if !ours {
        return match effort.required {
            true => {
                Err("codex effort resume: the row is not the execution the restart ended".into())
            }
            false => Ok(None),
        };
    }
    let (session, start) = capture(turn, effort.operation)?;
    if session != effort.native_session {
        return Err("codex effort resume: the ended thread is not the restarted one".into());
    }
    Ok(Some((session, start)))
}

/// The channel's one live O cursor's source: the native session its ended execution read.
pub fn live_source(channel: u64) -> Result<(OStore, SourceId), String> {
    let runtime = crate::config::runtime_root().ok_or("no runtime root")?;
    let store = OStore::existing(&runtime).ok_or("no O store")?;
    let cursors = store
        .peek_cursors(channel)
        .map_err(|error| format!("{error:?}"))?;
    let mut live = cursors.into_iter().filter(|cursor| !cursor.retired);
    let source = live.next().ok_or("no ended native session")?.source;
    if live.next().is_some() {
        return Err("more than one live native source".into());
    }
    Ok((store, source))
}

/// The ended predecessor's one live O cursor names the source; its EOF is the boundary.
fn capture(turn: &CodexHerdrTurn<'_>, operation: String) -> Result<(String, ResumeStart), String> {
    let retired = matches!(turn.row, Some(HostedRecord::Known(record))
        if record.state == HostedState::Retired && record.owner == turn.owner);
    if !retired {
        return Err("no retired predecessor of this owner".into());
    }
    let (store, source) = live_source(turn.channel_id)?;
    let start = ResumeStart::capture(operation, turn.channel_id, &source, &store)?;
    Ok((source.session_id, start))
}
