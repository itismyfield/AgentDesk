//! Only the greeting of an explicit restart may resume the conversation its predecessor ended.
use std::cell::RefCell;

use super::CodexHerdrTurn;
use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
use crate::services::tui_prompt_dedupe::binding_context::ResumeStart;

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
pub(crate) fn scope<R>(operation: Option<String>, turn: impl FnOnce() -> R) -> R {
    let Some(operation) = operation else {
        return turn();
    };
    let _restored = Restored(OPERATION.with(|slot| slot.borrow_mut().replace(operation)));
    turn()
}

/// The native session and the read boundary this turn resumes; `None` launches fresh.
pub(super) fn resume_of(turn: &CodexHerdrTurn<'_>) -> Option<(String, ResumeStart)> {
    let operation = OPERATION.with(|slot| slot.borrow().clone())?;
    match capture(turn, operation) {
        Ok(resume) => Some(resume),
        Err(reason) => {
            let channel = turn.channel_id;
            tracing::info!(channel, %reason, "codex restart greeting launches a fresh session");
            None
        }
    }
}

/// The ended predecessor's one live O cursor names the source; its EOF is the boundary.
fn capture(turn: &CodexHerdrTurn<'_>, operation: String) -> Result<(String, ResumeStart), String> {
    let retired = matches!(turn.row, Some(HostedRecord::Known(record))
        if record.state == HostedState::Retired && record.owner == turn.owner);
    if !retired {
        return Err("no retired predecessor of this owner".into());
    }
    let runtime = crate::config::runtime_root().ok_or("no runtime root")?;
    let store = crate::services::tui_o::store::OStore::existing(&runtime).ok_or("no O store")?;
    let cursors = store
        .peek_cursors(turn.channel_id)
        .map_err(|error| format!("{error:?}"))?;
    let mut live = cursors.into_iter().filter(|cursor| !cursor.retired);
    let source = live.next().ok_or("no ended native session")?.source;
    if live.next().is_some() {
        return Err("more than one live native source".into());
    }
    let start = ResumeStart::capture(operation, turn.channel_id, &source, &store)?;
    Ok((source.session_id, start))
}
