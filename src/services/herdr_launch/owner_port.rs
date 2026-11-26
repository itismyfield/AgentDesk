//! What a Herdr launch and an unplaced resolution hand back to the channel owner's runtime.

#[cfg(any(unix, test))]
use futures::future::BoxFuture;

#[cfg(any(unix, test))]
use crate::db::dispatched_sessions::hosted_execution::HostedExecution;

/// Closes `orphan`, an execution no row records, if `holds` still reads true once its fence is
/// shut; `Ok` only when its pane and provider are gone.
#[cfg(any(unix, test))]
pub(crate) type UnrecordedClose =
    for<'a> fn(&'a HostedExecution, BoxFuture<'a, bool>) -> BoxFuture<'a, Result<(), String>>;

/// The owner runtime's side of a launch; the runtime that owns the channel supplies it.
#[cfg(any(unix, test))]
#[derive(Clone, Copy)]
pub struct OwnerPort {
    /// Keeps the end of execution `nonce` when it is the one a Claude takeover of `channel` took.
    pub takeover_ended: fn(u64, &str, &str),
    pub close_unrecorded: UnrecordedClose,
}
