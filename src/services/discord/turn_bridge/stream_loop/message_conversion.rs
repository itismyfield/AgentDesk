use super::*;

pub(super) fn into_content_message(message: StreamMessage) -> Option<StreamContentArmMessage> {
    match message {
        StreamMessage::RetryBoundary => Some(StreamContentArmMessage::RetryBoundary),
        StreamMessage::ActiveUsageSnapshot {
            model,
            input_tokens,
            cache_create_tokens,
            cache_read_tokens,
        } => Some(StreamContentArmMessage::ActiveUsageSnapshot {
            model,
            input_tokens,
            cache_create_tokens,
            cache_read_tokens,
        }),
        StreamMessage::Init {
            session_id,
            raw_session_id,
        } => Some(StreamContentArmMessage::Init {
            session_id,
            raw_session_id,
        }),
        StreamMessage::Text { content } => Some(StreamContentArmMessage::Text { content }),
        StreamMessage::Thinking { summary } => Some(StreamContentArmMessage::Thinking { summary }),
        StreamMessage::Done { result, session_id } => {
            Some(StreamContentArmMessage::Done { result, session_id })
        }
        StreamMessage::Error {
            message, stderr, ..
        } => Some(StreamContentArmMessage::Error { message, stderr }),
        StreamMessage::StatusUpdate {
            input_tokens,
            cache_create_tokens,
            cache_read_tokens,
            output_tokens,
            ..
        } => Some(StreamContentArmMessage::StatusUpdate {
            input_tokens,
            cache_create_tokens,
            cache_read_tokens,
            output_tokens,
        }),
        StreamMessage::StatusEvents { events } => {
            Some(StreamContentArmMessage::StatusEvents { events })
        }
        _ => None,
    }
}
