//! The read that ends a Pending execution's retry wait before its due: only a change of the cause
//! that refused it. A logged start never clears a pane whose evidence is missing.
use std::cell::RefCell;

use sqlx::PgPool;

use super::{HerdrTurn, load, start_logged};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedOwner, HostedRecord, HostedState,
};
use crate::services::claude_tui::busy_inject::Screen;
use crate::services::turn_orchestrator::RetryWaitCause as Cause;

/// One read of the cause a retry wait holds on.
pub(crate) type CauseRead = Box<dyn FnMut() -> Screen + Send>;

thread_local! {
    static LEFT: RefCell<Option<CauseRead>> = const { RefCell::new(None) };
}

/// The typed refusal of `cause` with `detail`, leaving the read of that cause on this thread.
pub(super) fn refused(
    turn: &HerdrTurn<'_>,
    session_id: &str,
    cause: Cause,
    detail: &str,
) -> String {
    armed(turn, session_id, cause.refusal(detail))
}

/// Leaves the read of the cause `error` names, if it names one, for the owner's wait.
pub(super) fn armed(turn: &HerdrTurn<'_>, session_id: &str, error: String) -> String {
    if let Some(cause) = Cause::from_refusal(&error) {
        let session = session_id.to_owned();
        let read = read(
            turn.pool.clone(),
            turn.owner.clone(),
            turn.channel_id,
            session,
            cause,
        );
        LEFT.with(|left| *left.borrow_mut() = Some(read));
    }
    error
}

/// The read the last typed refusal on this thread left; taken once.
pub(crate) fn take() -> Option<CauseRead> {
    LEFT.with(|left| left.borrow_mut().take())
}

/// Reads `owner`'s row (and, for a never-started pane, the binding log) for a change of `cause`:
/// `Ready` once it cleared, `Unknown` while it holds or the read failed.
pub(crate) fn read(
    pool: PgPool,
    owner: HostedOwner,
    channel: u64,
    session: String,
    cause: Cause,
) -> CauseRead {
    Box::new(move || {
        let Some(nonce) = cause.nonce() else {
            return Screen::Gone;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Screen::Unknown;
        };
        let record = match runtime.block_on(load(&pool, &owner)) {
            HostedLookup::Found(found) => found.record,
            HostedLookup::Missing => return Screen::Ready,
            _ => return Screen::Unknown,
        };
        let record = match record {
            HostedRecord::Known(record) => record,
            HostedRecord::Legacy => return Screen::Ready,
            HostedRecord::Unknown(_) => return Screen::Unknown,
        };
        let other = record.execution_nonce != nonce;
        let ended = matches!(record.state, HostedState::Bound | HostedState::Retired);
        let cleared = match &cause {
            Cause::PendingUnplaced { .. } => other || record.state == HostedState::Retired,
            Cause::PendingUnproven { .. } => other || ended || record.expected.is_some(),
            _ => {
                let logical = owner.logical_key.as_str();
                other || ended || start_logged(channel, logical, nonce, &session)
            }
        };
        if cleared {
            Screen::Ready
        } else {
            Screen::Unknown
        }
    })
}
