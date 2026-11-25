//! A ticket's admission that also says, in the same actor turn, whether it opened the first head
//! of an owner with no running turn: the one input an idle owner may mark at acceptance.
use super::source_admission::{AdmitError, Admitted, resume_in};
use super::*;

impl super::super::super::ChannelMailboxHandle {
    /// Resumes `ticket` like `v2_resume_ticket`; `true` only when this commit queued the owner's
    /// sole input while no turn, legacy input or head attempt was held.
    pub(crate) async fn v2_resume_ticket_at_head(
        &self,
        original: PendingQueueItem,
        ticket: String,
        same_input: fn(&PendingQueueItem, &PendingQueueItem) -> bool,
    ) -> Result<(Admitted, PendingQueueItem, bool), AdmitError> {
        let ticket = Ticket::new(&ticket).map_err(AdmitError::NotCommitted)?;
        self.v2_turn_seen(move |slot, v1_holds, turn_active| {
            let resumed = resume_in(slot, original, ticket, same_input);
            Ok(resumed.map(|(admitted, chosen)| {
                let opened = admitted == Admitted::Queued
                    && !v1_holds
                    && !turn_active
                    && slot
                        .as_ref()
                        .is_some_and(|owner| opened_head(owner, &chosen));
                (admitted, chosen, opened)
            }))
        })
        .await
        .map_err(|_| AdmitError::Pending)?
    }
}

/// The owner holds exactly `chosen` and no head attempt.
fn opened_head(owner: &Owner, chosen: &PendingQueueItem) -> bool {
    owner.snapshot.head_injection.is_none()
        && matches!(owner.snapshot.items.as_slice(), [only] if only.message_id == chosen.message_id)
}

#[cfg(test)]
#[path = "idle_head_tests.rs"]
mod tests;
