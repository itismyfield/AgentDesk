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

/// A synthetic start's original: its reservation's message as a bot input of this process's
/// generation, and the ticket that turn names on every re-request.
pub(crate) fn synthetic_original(
    turn_id: &str,
    channel: u64,
    text: &str,
    author: u64,
) -> Result<(PendingQueueItem, AdmissionTicket), String> {
    let message = (turn_id.rsplit(':').next())
        .and_then(|id| id.parse::<u64>().ok())
        .ok_or("reserved source missing")?;
    let generation = crate::services::discord::runtime_store::process_generation();
    let original = serde_json::from_value(serde_json::json!({"author_id":author,
        "author_is_bot":true,"message_id":message,"source_message_ids":[message],
        "channel_id":channel,"queued_generation":generation,"text":text}))
    .map_err(|error| error.to_string())?;
    let ticket = AdmissionTicket::stored(&format!("turn-{channel}-{message}"));
    Ok((original, ticket))
}

/// What a headless start's caller resolved before it, kept with the original it admits.
pub(crate) struct HeadlessProvenance<'a> {
    pub(crate) owner: (u64, &'a str),
    pub(crate) source: Option<&'a str>,
    pub(crate) metadata: Option<serde_json::Value>,
    pub(crate) channel_hint: Option<&'a str>,
    pub(crate) label: Option<&'a str>,
    pub(crate) dm: Option<bool>,
}

impl HeadlessProvenance<'_> {
    pub(crate) fn json(self, turn_id: &str) -> serde_json::Value {
        serde_json::json!({"origin":"headless","turn_id":turn_id,"request_owner":self.owner.0,
            "request_owner_name":self.owner.1,"source":self.source,"metadata":self.metadata,
            "channel_name_hint":self.channel_hint,"synthetic_label":self.label,"is_dm":self.dm})
    }
}

#[cfg(test)]
#[path = "start_tests.rs"]
mod tests;
