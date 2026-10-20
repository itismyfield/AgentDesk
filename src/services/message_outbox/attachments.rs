//! Binary payloads share the existing durable outbox identity and lease.

use crate::db::scheduled_messages::MAX_IMAGE_BYTES;
use sqlx::Row;
use std::{fmt, sync::Arc};

#[derive(Clone, Copy)]
pub(crate) struct OutboxAttachment<'a> {
    pub filename: &'a str,
    pub content_type: &'a str,
    pub data: &'a [u8],
}

pub(crate) struct StoredOutboxAttachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

impl StoredOutboxAttachment {
    pub fn borrowed(&self) -> OutboxAttachment<'_> {
        OutboxAttachment {
            filename: &self.filename,
            content_type: &self.content_type,
            data: &self.data,
        }
    }
}

impl fmt::Debug for StoredOutboxAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredOutboxAttachment")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("size_bytes", &self.data.len())
            .finish()
    }
}

pub(crate) fn decode_claimed_attachment(
    row: &sqlx::postgres::PgRow,
) -> Result<Option<Arc<StoredOutboxAttachment>>, &'static str> {
    let filename: Option<String> = row
        .try_get("attachment_filename")
        .map_err(|_| "invalid attachment filename projection")?;
    let content_type: Option<String> = row
        .try_get("attachment_content_type")
        .map_err(|_| "invalid attachment MIME projection")?;
    let data: Option<Vec<u8>> = row
        .try_get("attachment_data")
        .map_err(|_| "invalid attachment bytes projection")?;
    match (filename, content_type, data) {
        (None, None, None) => Ok(None),
        (Some(filename), Some(content_type), Some(data))
            if (1..=MAX_IMAGE_BYTES).contains(&data.len()) =>
        {
            Ok(Some(Arc::new(StoredOutboxAttachment {
                filename,
                content_type,
                data,
            })))
        }
        _ => Err("message_outbox row contains an incomplete or invalid binary attachment"),
    }
}

pub(super) fn parts(
    attachment: Option<OutboxAttachment<'_>>,
) -> (Option<&str>, Option<&str>, Option<&[u8]>) {
    attachment
        .map(|a| (Some(a.filename), Some(a.content_type), Some(a.data)))
        .unwrap_or((None, None, None))
}

impl<'a> TryFrom<&'a crate::db::scheduled_messages::ScheduledMessageImage>
    for OutboxAttachment<'a>
{
    type Error = std::io::Error;
    fn try_from(
        image: &'a crate::db::scheduled_messages::ScheduledMessageImage,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            filename: &image.filename,
            content_type: &image.content_type,
            data: image.require_loaded()?,
        })
    }
}

impl fmt::Debug for OutboxAttachment<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboxAttachment")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("size_bytes", &self.data.len())
            .finish()
    }
}
