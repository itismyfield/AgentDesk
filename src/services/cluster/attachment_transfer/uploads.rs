//! Durable references survive queue merge, requeue, and process restart.
//! Legacy local records retain their JSON string representation.
use serde::{Deserialize, Serialize};

use super::AttachmentMessageIdentity;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BundleRef {
    pub bundle_id: uuid::Uuid,
    pub identity: AttachmentMessageIdentity,
    pub source_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Upload {
    Local(String),
    Bundle(BundleRef),
}

impl Upload {
    pub(crate) fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    pub(crate) fn history_record(&self) -> String {
        match self {
            Self::Local(record) => record.clone(),
            Self::Bundle(reference) => format!(
                "[Attachments] {} file(s), message {}",
                reference.source_count, reference.identity.user_msg_id
            ),
        }
    }
}

impl From<String> for Upload {
    fn from(value: String) -> Self {
        Self::Local(value)
    }
}

impl From<&str> for Upload {
    fn from(value: &str) -> Self {
        Self::Local(value.to_string())
    }
}

#[cfg(test)]
impl PartialEq<&str> for Upload {
    fn eq(&self, other: &&str) -> bool {
        matches!(self, Self::Local(value) if value == other)
    }
}

pub(crate) type PendingUploads = Vec<Upload>;

#[cfg(test)]
impl PartialEq<String> for Upload {
    fn eq(&self, other: &String) -> bool {
        matches!(self, Self::Local(value) if value == other)
    }
}
