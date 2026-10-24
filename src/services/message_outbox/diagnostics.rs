//! Existing enqueue error formatting and structured warning helpers.

use super::{OutboxEnqueueError, OutboxMessage, normalized_reason_code, normalized_session_key};

impl std::fmt::Display for OutboxEnqueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceNotAllowed { source } => write!(
                formatter,
                "message_outbox source `{source}` is not registered for LoopbackInternal"
            ),
            Self::Database(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OutboxEnqueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SourceNotAllowed { .. } => None,
            Self::Database(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for OutboxEnqueueError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

pub(super) fn warn_outbox_enqueue_failure(
    backend: &'static str,
    message: OutboxMessage<'_>,
    error: impl std::fmt::Display,
) {
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);
    tracing::warn!(
        backend,
        target = message.target,
        bot = message.bot,
        source = message.source,
        reason_code,
        session_key = session_key.as_deref(),
        "failed to enqueue outbox message: {error}"
    );
}

pub(super) fn warn_lifecycle_enqueue_failure(
    backend: &'static str,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    error: impl std::fmt::Display,
) {
    let session_key = normalized_session_key(target, session_key);
    tracing::warn!(
        backend,
        target,
        reason_code,
        session_key = session_key.as_deref(),
        "failed to enqueue lifecycle notification: {error}"
    );
}
