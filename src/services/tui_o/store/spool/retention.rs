//! Durable retirement explanations; retention evidence is not a delivery receipt or provider seal.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Cursor;
use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};

/// The record of one unproven hop's cleanup; it never authorizes another retirement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionCleanup {
    pub job_id: String,
    pub operator: String,
    pub at: DateTime<Utc>,
    pub reason: String,
    pub cutoff: u64,
    pub hop_seq: u64,
    pub verified: VerifiedEnd,
    pub ended: ExecutionEnd,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionEvidenceV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedEnd {
    pub offset: u64,
    pub prefix_hash: String,
    pub fingerprint: Fingerprint,
}

/// An execution-end statement, distinct from the provider's durable End+Seal proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionEnd {
    pub host: String,
    pub pane: String,
    pub first_seq: u64,
    pub last_seq: u64,
    pub basis: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionBasisV1 {
    SamePane,
    LocalHistorical { context_digest: String },
}

/// One exact hop's non-holding basis, bound to a complete current observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionClaimV1 {
    pub version: u8,
    pub provider: ShadowProvider,
    pub channel: u64,
    pub old: SourceId,
    pub hop_seq: u64,
    /// Source publication, or the original Pending sequence for a Resolved source.
    pub binding_seq: u64,
    /// The actual last Source/Resolved row; it is not the original Pending sequence.
    pub binder_seq: u64,
    pub binder_nonce: String,
    pub successor: SourceId,
    pub observation: String,
    pub end: ExecutionEnd,
    pub basis: RetentionBasisV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionEvidenceV1 {
    pub claim: RetentionClaimV1,
    pub min_age_secs: u64,
    pub last_dialogue: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub scope: String,
}

impl RetentionEvidenceV1 {
    pub(super) fn valid(&self, cursor: &Cursor, audit: &TransitionCleanup, channel: u64) -> bool {
        let claim = &self.claim;
        let digest = |value: &str, prefix: &str| {
            value.strip_prefix(prefix).is_some_and(|hex| {
                hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        };
        claim.version == 1
            && claim.provider == ShadowProvider::Codex
            && claim.channel == channel
            && channel != 0
            && claim.old == cursor.source
            && claim.old != claim.successor
            && claim.hop_seq == audit.hop_seq
            && claim.hop_seq > 0
            && claim.hop_seq <= audit.cutoff
            && claim.binding_seq > 0
            && claim.binder_seq >= claim.binding_seq
            && claim.binder_seq < claim.hop_seq
            && !claim.binder_nonce.trim().is_empty()
            && claim.end == audit.ended
            && claim.end.first_seq == claim.hop_seq
            && claim.end.last_seq == claim.hop_seq
            && [&claim.end.host, &claim.end.pane, &claim.end.basis]
                .iter()
                .all(|field| !field.trim().is_empty())
            && match &claim.basis {
                RetentionBasisV1::SamePane => true,
                RetentionBasisV1::LocalHistorical { context_digest } => !context_digest.is_empty(),
            }
            && digest(&claim.observation, "hrot-codex-observation-v1:")
            && digest(&self.scope, "hrot-scope-v2:")
            && self.min_age_secs == 7 * 24 * 60 * 60
            && self.observed_at.signed_duration_since(self.last_dialogue)
                >= chrono::Duration::seconds(self.min_age_secs as i64)
            && audit.verified.fingerprint.len == cursor.captured_through
            && audit.verified.fingerprint.dev == cursor.source.dev
            && audit.verified.fingerprint.ino == cursor.source.ino
    }
}
