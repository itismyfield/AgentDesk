use serde::{Deserialize, Serialize};

use super::super::delivery_record::DeliveredCommit;

pub(in crate::services::discord) type ExactRange = (u64, u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct SourceToken {
    pub generation_mtime_ns: i64,
    pub source_dev: u64,
    pub source_ino: u64,
    pub serial: u64,
    pub reset_incarnation: u64,
}

/// The prefix digest checks byte continuity, never delivery completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct Publication {
    pub epoch: SourceToken,
    pub extent_end: u64,
    pub digest: [u8; 16],
    pub rev: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct ObligationLedger {
    pub publication: Publication,
    pub open: Vec<OpenObligation>,
    pub held: Vec<WholeCommit>,
    pub intents: Vec<ExactRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct WholeCommit {
    #[serde(flatten)]
    pub commit: DeliveredCommit,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_identity"
    )]
    pub source_dev: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_identity"
    )]
    pub source_ino: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct OpenObligation {
    pub range: ExactRange,
    pub origin: ExactRange,
    pub class: ObligationClass,
    pub attempt: Option<Attempt>,
    pub unresolved: bool,
    pub unresolved_since_ms: Option<u64>,
    pub redrive_capped: Option<RedriveCapped>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) enum ObligationClass {
    Owed,
    InDoubtSink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct Attempt {
    pub key: String,
    pub range: ExactRange,
    pub prepared_at_ms: Option<u64>,
    pub chunk_nonces: Option<Vec<String>>,
    pub chunk_total: Option<u32>,
    pub receipts: Vec<ChunkReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct ChunkReceipt {
    pub chunk: u32,
    pub message_id: u64,
    pub cleanup: CleanupState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) enum CleanupState {
    NotRemoved,
    Removed,
    Unknown,
}

/// Persist at the cap transition for immediate alerting and restart reconstruction.
/// The owning record, epoch and exact range identify diagnostics, never a retry permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct RedriveCapped {
    pub range: ExactRange,
    pub capped_at_ms: u64,
    pub next_rearm_at_ms: u64,
    pub last_rejection: String,
}

fn present_identity<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    u64::deserialize(d).map(Some)
}
