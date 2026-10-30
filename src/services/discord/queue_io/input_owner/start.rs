//! A synthetic start on an owned channel: the caller's preflight runs first, then the owner admits
//! the original under the caller's ticket with its provenance, and an acceptance wakes the drive.
#![cfg(unix)]
use std::sync::Arc;

use super::admission::{Admission, AdmissionTicket, admit_with_context};
use super::drive::{Drive, wake};
use super::ingress::{Intake, OwnerPending};
use super::origin::{Execution, InputOrigin};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};

/// Whether the owner takes a start on this target; it reads only and never adopts or admits.
pub(crate) async fn owner_will_take(
    handle: &ChannelMailboxHandle,
    execution: Execution,
    origin: InputOrigin,
) -> Result<bool, String> {
    Ok(execution.owner_takes(origin) && super::ingress::installed(handle).await?)
}

/// `None` leaves the start to its legacy path. Otherwise the preflight refuses before any
/// admission, and Pending stays its own non-success answer with the caller's ticket.
pub(crate) async fn admit_synthetic(
    drive: Arc<Drive>,
    (execution, origin): (Execution, InputOrigin),
    (original, ticket): (&PendingQueueItem, AdmissionTicket),
    provenance: serde_json::Value,
    preflight: impl std::future::Future<Output = Result<(), String>>,
) -> Option<Intake> {
    match owner_will_take(&drive.mailbox, execution, origin).await {
        Ok(true) => {}
        Ok(false) => return None,
        Err(error) => return Some(Intake::Rejected(error)),
    }
    if let Err(refused) = preflight.await {
        return Some(Intake::Rejected(refused));
    }
    Some(
        match admit_with_context(&drive.mailbox, original, ticket, provenance).await {
            Admission::Accepted(_) => {
                wake(drive);
                Intake::Accepted
            }
            Admission::Pending(ticket) => Intake::Pending(OwnerPending(ticket)),
            Admission::Rejected(reason) => Intake::Rejected(reason),
        },
    )
}

#[cfg(test)]
#[path = "start_tests.rs"]
mod tests;
