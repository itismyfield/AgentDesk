//! The Discord runtime's side of a Herdr launch and of an unplaced Pending's resolution.

use futures::future::BoxFuture;

use crate::db::dispatched_sessions::hosted_execution::HostedExecution;
use crate::services::herdr_launch::OwnerPort;

pub(crate) const HERDR_OWNER_PORT: OwnerPort = OwnerPort {
    takeover_ended: super::note_takeover_ended,
    close_unrecorded,
};

#[cfg(unix)]
fn close_unrecorded<'a>(
    orphan: &'a HostedExecution,
    holds: BoxFuture<'a, bool>,
) -> BoxFuture<'a, Result<(), String>> {
    Box::pin(async move {
        let closed = super::herdr_terminate::close_unrecorded(orphan, holds).await;
        closed.map_err(|result| format!("{result:?}"))
    })
}

/// The terminate service is Unix only; elsewhere an orphan is never closed and the row holds.
#[cfg(not(unix))]
fn close_unrecorded<'a>(
    _: &'a HostedExecution,
    _: BoxFuture<'a, bool>,
) -> BoxFuture<'a, Result<(), String>> {
    Box::pin(async { Err("unsupported on this platform".into()) })
}
