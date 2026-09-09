//! `AttachmentBundleV1` — the minimal byte contract for forwarding one Discord
//! message's attachments to another cluster node (#5713 S1,
//! docs/design/5713-attachment-transfer.md).
//!
//! A bundle carries the attachment *bytes* plus the message identity they
//! belong to, and deliberately carries no CDN URL and no sending-node
//! filesystem path: the receiver materializes from the envelope alone, and a
//! bundle must never be consumable against a different bot/channel/message.
//!
//! Types and the pure validator only. Durable storage, worker-side cache
//! materialization, and the router unblock are #5713 S2/S3; until those land
//! every item here is exercised by the test module beside it, so this slice
//! alone does not make attachments reach an agent.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Envelope version this build understands; any other value is rejected.
pub(crate) const ATTACHMENT_BUNDLE_V1: u16 = 1;

/// Caller-injected size policy. `Default` holds the #5713 S1 starting values;
/// production call sites pass their configured limits instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AttachmentBundleLimits {
    pub max_entries: usize,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for AttachmentBundleLimits {
    fn default() -> Self {
        Self {
            max_entries: 10,
            max_entry_bytes: 8 * 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
        }
    }
}

/// The message a bundle belongs to. All three fields are compared on
/// validation, so a bundle cannot be replayed against another message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachmentMessageIdentity {
    pub provider: String,
    pub channel_id: String,
    pub user_msg_id: String,
}

/// One attachment. `filename` is a *display* name only: receivers derive the
/// real storage name from the ordinal and digest (S2), never from this string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachmentEntryV1 {
    pub filename: String,
    pub byte_len: u64,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

/// Ordered attachment bytes for exactly one message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachmentBundleV1 {
    pub version: u16,
    pub identity: AttachmentMessageIdentity,
    pub entries: Vec<AttachmentEntryV1>,
}

/// A bundle that passed [`validate_attachment_bundle_v1`]. Consumers only
/// receive this type, so a partially-checked bundle is unrepresentable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedAttachmentBundle(AttachmentBundleV1);

impl ValidatedAttachmentBundle {
    pub(crate) fn identity(&self) -> &AttachmentMessageIdentity {
        &self.0.identity
    }

    pub(crate) fn entries(&self) -> &[AttachmentEntryV1] {
        &self.0.entries
    }
}

/// Why a bundle was refused. Every variant is fail-closed: the caller must
/// not degrade to a text-only turn on any of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentBundleError {
    UnsupportedVersion {
        found: u16,
    },
    IdentityMismatch,
    Empty,
    TooManyEntries {
        found: usize,
        max: usize,
    },
    UnsafeFilename {
        index: usize,
    },
    DeclaredLengthMismatch {
        index: usize,
        declared: u64,
        actual: u64,
    },
    EntryTooLarge {
        index: usize,
        len: u64,
        max: u64,
    },
    TotalTooLarge {
        total: u64,
        max: u64,
    },
    HashMismatch {
        index: usize,
    },
}

/// Lowercase hex SHA-256, the digest form stored in [`AttachmentEntryV1`].
pub(crate) fn attachment_sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Safe when it names no directory component and carries no control
/// characters, so a receiver can echo it verbatim in logs and UI.
fn is_safe_display_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.chars().any(char::is_control)
}

/// Validate a bundle against the message it claims to belong to and the
/// caller's size policy. All-or-nothing: one bad entry rejects the message.
pub(crate) fn validate_attachment_bundle_v1(
    bundle: AttachmentBundleV1,
    expected: &AttachmentMessageIdentity,
    limits: &AttachmentBundleLimits,
) -> Result<ValidatedAttachmentBundle, AttachmentBundleError> {
    if bundle.version != ATTACHMENT_BUNDLE_V1 {
        return Err(AttachmentBundleError::UnsupportedVersion {
            found: bundle.version,
        });
    }
    let found = &bundle.identity;
    if found.provider != expected.provider
        || found.channel_id != expected.channel_id
        || found.user_msg_id != expected.user_msg_id
    {
        return Err(AttachmentBundleError::IdentityMismatch);
    }
    if bundle.entries.is_empty() {
        return Err(AttachmentBundleError::Empty);
    }
    if bundle.entries.len() > limits.max_entries {
        return Err(AttachmentBundleError::TooManyEntries {
            found: bundle.entries.len(),
            max: limits.max_entries,
        });
    }
    let mut total: u64 = 0;
    for (index, entry) in bundle.entries.iter().enumerate() {
        if !is_safe_display_filename(&entry.filename) {
            return Err(AttachmentBundleError::UnsafeFilename { index });
        }
        let actual = entry.bytes.len() as u64;
        if entry.byte_len != actual {
            return Err(AttachmentBundleError::DeclaredLengthMismatch {
                index,
                declared: entry.byte_len,
                actual,
            });
        }
        if actual > limits.max_entry_bytes {
            return Err(AttachmentBundleError::EntryTooLarge {
                index,
                len: actual,
                max: limits.max_entry_bytes,
            });
        }
        total = total.saturating_add(actual);
        if total > limits.max_total_bytes {
            return Err(AttachmentBundleError::TotalTooLarge {
                total,
                max: limits.max_total_bytes,
            });
        }
        if !entry
            .sha256
            .eq_ignore_ascii_case(&attachment_sha256_hex(&entry.bytes))
        {
            return Err(AttachmentBundleError::HashMismatch { index });
        }
    }
    Ok(ValidatedAttachmentBundle(bundle))
}

#[cfg(test)]
#[path = "attachment_transfer/tests.rs"]
mod tests;
