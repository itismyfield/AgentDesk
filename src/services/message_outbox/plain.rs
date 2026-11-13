//! Plain row write after V and D: normalize, INSERT, then capture and store the census.

use super::{
    OutboxAttachment, OutboxEnqueueError, OutboxMessage, attachments, body_census,
    normalized_reason_code, normalized_session_key,
};
use crate::db::message_outbox::{census, notice};
use serde_json::Value;

pub(super) async fn insert_on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    message: OutboxMessage<'_>,
    attachment: Option<OutboxAttachment<'_>>,
    designation: Option<Value>,
) -> Result<i64, OutboxEnqueueError> {
    let (filename, content_type, data) = attachments::parts(attachment);
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);
    let id = notice::insert_plain(
        &mut **tx,
        notice::PlainRow {
            target: message.target,
            content: message.content,
            bot: message.bot,
            source: message.source,
            reason_code,
            session_key: session_key.as_deref(),
            attachment_filename: filename,
            attachment_content_type: content_type,
            attachment_data: data,
            session_notice: designation,
        },
    )
    .await?;
    // Capture after the row exists and in this task, so config and task-local tags are current.
    let observed = body_census::capture(message.source);
    census::store(tx, id, observed.as_ref()).await?;
    Ok(id)
}

#[cfg(test)]
#[path = "plain_tests.rs"]
mod tests;
