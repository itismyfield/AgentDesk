//! A restart's greeting: one ticket and original per operation, admitted before the old execution
//! ends so the owner's next turn launches the successor with it.
#![cfg(unix)]
use super::admission::{Admission, AdmissionTicket, admit_reserved};
pub(crate) use crate::services::turn_orchestrator::SeedStatus;
use crate::services::turn_orchestrator::restart_operation::Operation;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, PendingQueueItem, RESTART_SEED_SOURCE,
};

pub(crate) const PROMPT: &str = "안녕하세요. 세션 재시작 완료.";

pub(crate) struct Seed {
    ticket: AdmissionTicket,
    original: PendingQueueItem,
    context: serde_json::Value,
}

pub(crate) enum Admitted {
    /// This request's own greeting is in the owner's durable custody.
    Created,
    /// A greeting already queued stands for this request too; it is not this request's to withdraw.
    Joined,
    /// The commit is unconfirmed; the same ticket settles it later.
    Unconfirmed,
    /// The ticket already names another original or context: nothing may end under it.
    Conflict,
    /// No greeting can be admitted; the restart goes on without one.
    Refused(String),
}

/// What the greeting was queued against: the execution to end and the dispatch its row held.
pub(crate) struct Target<'a> {
    pub(crate) nonce: Option<&'a str>,
    pub(crate) dispatch: Option<&'a str>,
}

impl Seed {
    /// The operation alone fixes the ticket and the original; a retry reuses the stored original.
    pub(crate) async fn of(
        mailbox: &ChannelMailboxHandle,
        operation: &Operation,
    ) -> Result<Self, String> {
        let (message, raw) = operation.seed_identity();
        let channel = operation.channel();
        let generation = crate::services::discord::runtime_store::process_generation();
        let original: PendingQueueItem = serde_json::from_value(serde_json::json!({
            "author_id":1,"author_is_bot":true,"message_id":message,
            "source_message_ids":[message],"channel_id":channel,
            "queued_generation":generation,"text":PROMPT}))
        .map_err(|error| error.to_string())?;
        let stored = raw.clone();
        let original = mailbox
            .v2_request(move |slot| {
                slot.as_ref()
                    .ok_or("V2 not adopted")?
                    .retry_original(&stored, &original)
            })
            .await?;
        Ok(Self {
            ticket: AdmissionTicket::stored(&raw),
            original,
            context: serde_json::Value::Null,
        })
    }

    pub(crate) async fn status(
        &self,
        mailbox: &ChannelMailboxHandle,
    ) -> Result<SeedStatus, String> {
        mailbox.v2_seed_status(self.ticket.as_str().into()).await
    }

    /// Read before anything is derived from the current row: an operation the owner already
    /// holds a ticket for is answered from that ticket and ends nothing again.
    pub(crate) async fn prior(
        mailbox: &ChannelMailboxHandle,
        operation: &Operation,
    ) -> Result<SeedStatus, String> {
        mailbox.v2_seed_status(operation.seed_identity().1).await
    }

    pub(crate) async fn admit(
        &mut self,
        mailbox: &ChannelMailboxHandle,
        (command, requester): (&str, &str),
        target: Target<'_>,
    ) -> Admitted {
        if self.context.is_null() {
            let turn = format!(
                "discord:{}:{}",
                self.original.channel_id.unwrap_or_default(),
                self.original.message_id
            );
            self.context = serde_json::json!({"origin":"headless","turn_id":turn,
                "request_owner":1,"request_owner_name":requester,"source":RESTART_SEED_SOURCE,
                "metadata":{"command":command,"human_input":{"origin_id":self.ticket.as_str()},"restart_seed":{"operation":self.ticket.as_str(),
                    "target_nonce":target.nonce,"dispatch":target.dispatch}},
                "channel_name_hint":null,"synthetic_label":null,"is_dm":null});
        }
        let before = self.status(mailbox).await;
        let admitted = admit_reserved(
            mailbox,
            &self.original,
            self.ticket.clone(),
            self.context.clone(),
        );
        match admitted.await {
            Admission::Accepted(_) if before == Ok(SeedStatus::Absent) => {
                match mailbox
                    .v2_admission_context(self.original.clone(), self.ticket.as_str().into())
                    .await
                {
                    Ok(Some(context)) if context == self.context => Admitted::Created,
                    _ => Admitted::Joined,
                }
            }
            Admission::Accepted(_) => Admitted::Joined,
            Admission::Pending(_) => Admitted::Unconfirmed,
            Admission::Rejected(reason) if reason.starts_with("Conflict(") => Admitted::Conflict,
            Admission::Rejected(reason) => Admitted::Refused(reason),
        }
    }

    /// Takes this request's still queued greeting out with a cancelled receipt.
    pub(crate) async fn withdraw(&self, mailbox: &ChannelMailboxHandle) {
        if let Err(error) = mailbox.v2_withdraw_seed(self.ticket.as_str().into()).await {
            tracing::warn!(%error, "restart greeting stays queued for its permit to refuse");
        }
    }
}

/// The execution and dispatch a stored greeting was queued against.
pub(crate) fn target(context: &serde_json::Value) -> Target<'_> {
    let read = |key: &str| context["metadata"]["restart_seed"][key].as_str();
    Target {
        nonce: read("target_nonce"),
        dispatch: read("dispatch"),
    }
}

#[cfg(test)]
#[path = "restart_seed_tests.rs"]
mod tests;
