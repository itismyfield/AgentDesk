//! `AttachmentBundleV1` — the byte contract for forwarding one Discord message's
//! attachments to another cluster node (#5713 S1). Types and the pure validator
//! only; S2 (durable storage, worker consumption) and S3 (live download, router
//! unblock) own all I/O, so nothing here has a production caller yet.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Envelope version this build understands; any other value is rejected.
pub(crate) const ATTACHMENT_BUNDLE_V1: u16 = 1;

/// The message a bundle belongs to. Every field is compared, so a bundle can
/// never be consumed against another bot, channel, or message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachmentMessageIdentity {
    pub provider: String,
    pub channel_id: String,
    pub user_msg_id: String,
}

/// One attachment. `filename` is untrusted display text: S2 derives the storage
/// name from ordinal and digest, and a consumer escapes it before echoing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachmentEntryV1 {
    pub filename: String,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

/// Ordered attachment bytes for exactly one message. No CDN URL and no sending-
/// node path: the receiver materializes from the envelope alone, so an expired
/// or re-signed URL cannot turn a stored bundle into an unreadable one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachmentBundleV1 {
    pub version: u16,
    pub identity: AttachmentMessageIdentity,
    /// Attachments on the source message. `entries` must carry exactly this many,
    /// so a producer that lost one download cannot ship a quietly shorter turn.
    pub source_attachment_count: u32,
    pub entries: Vec<AttachmentEntryV1>,
}

/// A bundle that passed [`validate_attachment_bundle_v1`]. Consumers receive
/// only this type, so a partially checked bundle is unrepresentable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedAttachmentBundle(AttachmentBundleV1);

impl ValidatedAttachmentBundle {
    pub(crate) fn as_bundle(&self) -> &AttachmentBundleV1 {
        &self.0
    }
}

/// Why a bundle was refused. Every variant is fail-closed: no caller may degrade
/// an attachment turn into a text-only one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentBundleError {
    UnsupportedVersion,
    IdentityMismatch,
    IncompleteBundle,
    HashMismatch,
}

/// Lowercase hex SHA-256, the digest form stored in [`AttachmentEntryV1`].
pub(crate) fn attachment_sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Validate a bundle against the message it claims to belong to. All-or-nothing
/// per message: a wrong version, a foreign identity, a short or padded entry
/// set, or one corrupted entry rejects the whole bundle. Size ceilings belong to
/// S2 transport, which sees the encoded envelope before it is decoded.
pub(crate) fn validate_attachment_bundle_v1(
    bundle: AttachmentBundleV1,
    expected: &AttachmentMessageIdentity,
) -> Result<ValidatedAttachmentBundle, AttachmentBundleError> {
    if bundle.version != ATTACHMENT_BUNDLE_V1 {
        return Err(AttachmentBundleError::UnsupportedVersion);
    }
    if bundle.identity != *expected {
        return Err(AttachmentBundleError::IdentityMismatch);
    }
    let declared = u64::from(bundle.source_attachment_count);
    if declared == 0 || bundle.entries.len() as u64 != declared {
        return Err(AttachmentBundleError::IncompleteBundle);
    }
    for entry in &bundle.entries {
        let digest = attachment_sha256_hex(&entry.bytes);
        if !entry.sha256.eq_ignore_ascii_case(&digest) {
            return Err(AttachmentBundleError::HashMismatch);
        }
    }
    Ok(ValidatedAttachmentBundle(bundle))
}

#[cfg(test)]
#[path = "attachment_transfer/tests.rs"]
mod tests;
