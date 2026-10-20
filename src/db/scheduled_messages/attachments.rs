//! Durable definition images and metadata-only list projections.

use std::{fmt, io};

use sqlx::{Postgres, QueryBuilder};

use super::ScheduledMessagePatch;

pub(super) const IMAGE_COLUMNS: &str = "image_filename, image_content_type, image_data, \
    octet_length(image_data)::bigint AS image_size_bytes";
pub(super) const LIST_IMAGE_COLUMNS: &str = "image_filename, image_content_type, \
    NULL::bytea AS image_data, octet_length(image_data)::bigint AS image_size_bytes";

#[derive(Clone, PartialEq, Eq)]
pub struct ScheduledMessageImageAttachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// A list row carries metadata only; detail and worker rows also load `data`.
#[derive(Clone, PartialEq, Eq)]
pub struct ScheduledMessageImage {
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub data: Option<Vec<u8>>,
}

impl fmt::Debug for ScheduledMessageImageAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScheduledMessageImageAttachment")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("size_bytes", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ScheduledMessageImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScheduledMessageImage")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("size_bytes", &self.size_bytes)
            .field("bytes_loaded", &self.data.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(sqlx::FromRow)]
pub(super) struct ImageColumns {
    image_filename: Option<String>,
    image_content_type: Option<String>,
    image_size_bytes: Option<i64>,
    image_data: Option<Vec<u8>>,
}

impl TryFrom<ImageColumns> for Option<ScheduledMessageImage> {
    type Error = io::Error;

    fn try_from(columns: ImageColumns) -> Result<Self, Self::Error> {
        match (
            columns.image_filename,
            columns.image_content_type,
            columns.image_size_bytes,
            columns.image_data,
        ) {
            (None, None, None, None) => Ok(None),
            (Some(filename), Some(content_type), Some(size_bytes), data)
                if (1..=8_388_608).contains(&size_bytes)
                    && data
                        .as_ref()
                        .is_none_or(|bytes| bytes.len() as i64 == size_bytes) =>
            {
                Ok(Some(ScheduledMessageImage {
                    filename,
                    content_type,
                    size_bytes,
                    data,
                }))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete scheduled image metadata or invalid byte length",
            )),
        }
    }
}

pub(super) fn apply_definition_patch(
    builder: &mut QueryBuilder<Postgres>,
    patch: &ScheduledMessagePatch,
) {
    if let Some(attachment) = &patch.image_attachment {
        builder
            .push(", image_filename = ")
            .push_bind(attachment.as_ref().map(|image| image.filename.as_str()))
            .push(", image_content_type = ")
            .push_bind(attachment.as_ref().map(|image| image.content_type.as_str()))
            .push(", image_data = ")
            .push_bind(attachment.as_ref().map(|image| image.data.as_slice()));
    }
}
