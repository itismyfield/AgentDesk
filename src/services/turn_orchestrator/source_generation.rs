use poise::serenity_prelude::MessageId;

#[derive(Clone, Debug)]
pub(crate) struct SourceMessageQueuedGeneration {
    pub(crate) message_id: MessageId,
    pub(crate) queued_generation: u64,
    /// Positive, per-source proof that this queued payload came from a
    /// genuine user instruction. Unmarked sources retain drop-on-exit.
    pub(crate) preserve_on_cancel: bool,
}

impl SourceMessageQueuedGeneration {
    pub(crate) fn new(message_id: MessageId, queued_generation: u64) -> Self {
        Self {
            message_id,
            queued_generation,
            preserve_on_cancel: false,
        }
    }

    pub(crate) fn user_instruction(message_id: MessageId, queued_generation: u64) -> Self {
        Self {
            message_id,
            queued_generation,
            preserve_on_cancel: true,
        }
    }
}
