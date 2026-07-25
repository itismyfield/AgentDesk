use poise::serenity_prelude::MessageId;

use super::{SourceMessageQueuedGeneration, SourceMessageTextSegment};

pub(super) fn push_unique_message_ids(
    existing: &mut Vec<MessageId>,
    incoming: impl IntoIterator<Item = MessageId>,
) {
    for message_id in incoming {
        if !existing.contains(&message_id) {
            existing.push(message_id);
        }
    }
}

pub(super) fn push_unique_source_message_queued_generations(
    existing: &mut Vec<SourceMessageQueuedGeneration>,
    incoming: impl IntoIterator<Item = SourceMessageQueuedGeneration>,
) {
    for incoming in incoming {
        if !existing
            .iter()
            .any(|owner| owner.message_id == incoming.message_id)
        {
            existing.push(incoming);
        }
    }
}

pub(super) fn push_unique_source_text_segments(
    existing: &mut Vec<SourceMessageTextSegment>,
    incoming: impl IntoIterator<Item = SourceMessageTextSegment>,
) {
    for incoming in incoming {
        if !existing
            .iter()
            .any(|segment| segment.message_id == incoming.message_id)
        {
            existing.push(incoming);
        }
    }
}
