//! Continuations enter as machine input with an owner-persisted original, never a user replay.
use super::admission::AdmissionTicket;
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};

pub(crate) fn is_continuation(context: Option<&serde_json::Value>) -> bool {
    ChannelMailboxHandle::effort_input(context)
}

pub(crate) async fn original(
    mailbox: &ChannelMailboxHandle,
    channel: u64,
    context: &mut serde_json::Value,
) -> Result<(PendingQueueItem, AdmissionTicket), String> {
    let generation = crate::services::discord::runtime_store::process_generation();
    let intent = mailbox
        .v2_request(move |slot| {
            slot.as_mut()
                .ok_or("V2 not adopted")?
                .prepare_effort(None, channel, generation)
        })
        .await?;
    let metadata = &context["metadata"]["effort_restart"];
    let expected = intent.context();
    for key in ["operation", "native_session", "interrupted_episode"] {
        if metadata[key] != expected["metadata"]["effort_restart"][key] {
            return Err("effort continuation belongs to another restart".into());
        }
    }
    let c = intent
        .continuation
        .as_ref()
        .ok_or("no interrupted episode")?;
    *context = intent.context();
    Ok((c.original.clone(), AdmissionTicket::stored(&c.ticket)))
}
