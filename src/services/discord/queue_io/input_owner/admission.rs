//! Admission by a stable ticket: the caller fixes the ticket before recording its own intent, asks
//! again with it after Pending or a restart, and looks it up without admitting.
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdmissionTicket(String);

impl AdmissionTicket {
    /// A fresh ticket, stored with the caller's intent before the owner is asked.
    pub(crate) fn new() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    /// The ticket a caller stored earlier; the owner refuses one it cannot use.
    pub(crate) fn stored(raw: &str) -> Self {
        Self(raw.to_string())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// The original is in the owner's durable custody under this ticket.
    Accepted(AdmissionTicket),
    /// Publication is unconfirmed. Not accepted, not consumed and not a refusal: the caller keeps
    /// its original and asks again with the same ticket.
    Pending(AdmissionTicket),
    /// Refused; the caller still holds its original.
    Rejected(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdmissionState {
    Committed,
    Pending,
    Absent,
}

/// Where a committed ticket's original stands in the owner: the owner's own single definition.
pub(crate) use crate::services::turn_orchestrator::TurnBind;

#[cfg(test)]
pub(crate) async fn admit(
    handle: &ChannelMailboxHandle,
    original: &PendingQueueItem,
    ticket: AdmissionTicket,
) -> Admission {
    match handle
        .v2_admit_ticket(original.clone(), ticket.0.clone())
        .await
    {
        Ok(_) => Admission::Accepted(ticket),
        Err(error) if error.is_pending() => Admission::Pending(ticket),
        Err(error) => Admission::Rejected(error.into()),
    }
}

/// Reads where the ticket stands; it never recovers, commits or admits.
pub(crate) async fn status(
    handle: &ChannelMailboxHandle,
    ticket: &AdmissionTicket,
) -> Result<AdmissionState, String> {
    let status = handle.v2_admission_status(ticket.0.clone()).await?;
    Ok(match (status.committed(), status.pending()) {
        (true, _) => AdmissionState::Committed,
        (false, true) => AdmissionState::Pending,
        (false, false) => AdmissionState::Absent,
    })
}

/// A ticket that names another original is an error, never one of the states.
pub(crate) async fn exact_turn(
    handle: &ChannelMailboxHandle,
    ticket: &AdmissionTicket,
    original: &PendingQueueItem,
) -> Result<TurnBind, String> {
    handle
        .v2_exact_turn(original.clone(), ticket.0.clone())
        .await
}

pub(crate) async fn admit_with_context(
    handle: &ChannelMailboxHandle,
    original: &PendingQueueItem,
    ticket: AdmissionTicket,
    context: serde_json::Value,
) -> Admission {
    match handle
        .v2_admit_ticket_with_context(original.clone(), ticket.0.clone(), context)
        .await
    {
        Ok(_) => Admission::Accepted(ticket),
        Err(error) if error.is_pending() => Admission::Pending(ticket),
        Err(error) => Admission::Rejected(error.into()),
    }
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
