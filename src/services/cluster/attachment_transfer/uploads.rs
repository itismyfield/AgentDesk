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

pub(crate) type Upload = String;
pub(crate) type PendingUploads = Vec<Upload>;
