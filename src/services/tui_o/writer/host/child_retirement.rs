//! Thread child writers stay stoppable, so a child store retires only after its exact writer
//! generation settled. Nothing here deletes a store or touches a parent or sibling.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::task::JoinHandle;

use super::{ManagedWriterHandle, WriterStopped, locked, process_readiness};
use crate::services::dispatched_sessions::hosted_cleanup::thread_gc::ChildStore;
use crate::services::tui_o::channel_policy::{BootChannels, RetirementTicket};
use crate::services::tui_o::cutover;

const OWED_READ_LIMIT: Duration = Duration::from_secs(3);
const STOP_LIMIT: Duration = Duration::from_secs(10);

static KEPT: LazyLock<Mutex<BTreeMap<u64, ManagedWriterHandle>>> = LazyLock::new(Mutex::default);

/// Keeps each thread child's handle for its retirement; every other writer is left to the
/// gateway lifecycle and returned detached.
pub(crate) fn keep_stoppable(handles: Vec<ManagedWriterHandle>) -> Vec<JoinHandle<()>> {
    let mut detached = Vec::new();
    for handle in handles {
        if cutover::thread_child(handle.channel) {
            // A replaced handle is of a host that already released the channel's claim.
            locked(&KEPT).insert(handle.channel, handle);
        } else {
            detached.push(handle.into_detached());
        }
    }
    detached
}

/// What thread_gc pins before its row delete: the child's lifecycle ticket and the writer
/// generation hosting it. It grants nothing over a successor, the parent or a sibling.
#[derive(Debug)]
pub(crate) struct RetirementRequest {
    ticket: RetirementTicket,
    writer: u64,
}

/// This process's child stores, as the thread GC boundary reaches them.
pub(crate) struct ChildStores;

impl ChildStore for ChildStores {
    type Pinned = RetirementRequest;

    /// None unless `thread` is an active child this process hosts a writer for.
    fn pin(&self, thread: u64) -> Option<RetirementRequest> {
        let ticket = cutover::child_retirement_ticket(thread)?;
        let writer = process_readiness().hosted_by(thread)?;
        Some(RetirementRequest { ticket, writer })
    }

    fn retire<'a>(
        &'a self,
        pinned: RetirementRequest,
        successor: BoxFuture<'a, Result<bool, String>>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let stopped = retire_child(pinned, successor).await?;
            tracing::info!(
                channel = stopped.channel,
                writer_generation = stopped.generation,
                "[tui_o] thread child writer settled and its store retired"
            );
            Ok(())
        })
    }
}

/// Returns the child's transition unless the retirement committed.
struct Transition<'a> {
    snapshot: &'a BootChannels,
    ticket: RetirementTicket,
    committed: bool,
}

impl Drop for Transition<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.snapshot.abort_child_retirement(&self.ticket);
        }
    }
}

/// After the row's exact delete: fresh zero debt, the pinned writer stopped and joined, no
/// successor row, then the durable mark. Any doubt keeps the store active and is the `Err`.
async fn retire_child(
    request: RetirementRequest,
    successor: impl Future<Output = Result<bool, String>> + Send,
) -> Result<WriterStopped, String> {
    let RetirementRequest { ticket, writer } = request;
    let channel = ticket.channel();
    let snapshot = cutover::boot_snapshot().ok_or("no boot channel policy")?;
    if !snapshot.begin_child_retirement(&ticket) {
        return Err("the request no longer names the child's generation and input epoch".into());
    }
    let mut transition = Transition {
        snapshot: &snapshot,
        ticket,
        committed: false,
    };
    let readiness = process_readiness();
    if readiness.hosted_by(channel) != Some(writer) {
        return Err("another writer generation hosts the child".into());
    }
    // An absent actor or a stale poll is no proof that nothing is owed.
    let owed = readiness
        .undelivered_after(channel, OWED_READ_LIMIT)
        .await
        .ok_or("no fresh read of what the child writer owes")?;
    let debt = [
        owed.owed,
        owed.notice_owed,
        owed.prepared,
        owed.unsealed,
        owed.uncaptured,
        owed.binding_pending,
        owed.verified_work,
        owed.posting,
    ];
    if debt.iter().any(|count| *count != 0) {
        return Err(format!("the child writer still owes output: {debt:?}"));
    }
    let kept = {
        let mut kept = locked(&KEPT);
        match kept.get(&channel) {
            Some(handle) if handle.generation == writer => kept.remove(&channel),
            _ => None,
        }
    };
    let handle = kept.ok_or("no managed handle of the pinned writer generation")?;
    let stopped = tokio::time::timeout(STOP_LIMIT, handle.stop_and_join())
        .await
        .map_err(|_| "the child writer did not settle in time".to_owned())??;
    if successor.await? {
        return Err("a successor session row exists for the thread".into());
    }
    transition.committed = true;
    match snapshot.commit_child_retirement(&ticket)? {
        true => Ok(stopped),
        false => Err("an input superseded the retirement".into()),
    }
}
