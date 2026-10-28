//! Durable V2 data; production admission remains closed.
use super::{HeadPhase, PendingQueueItem, QueueIdentity};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SnapshotV2 {
    pub(crate) version: u32,
    pub(crate) items: Vec<PendingQueueItem>,
    pub(crate) head_injection: Option<InputAttempt>,
    pub(crate) retained_inputs: Vec<RetainedInput>,
    pub(crate) terminal_identities: Vec<TerminalIdentity>,
    #[serde(default)]
    pub(crate) namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) admission_receipts: Vec<String>,
    #[serde(default)]
    pub(crate) admission_sequence: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InputAttempt {
    pub(crate) identity: QueueIdentity,
    pub(crate) original: PendingQueueItem,
    pub(crate) phase: HeadPhase,
    pub(crate) attempt: Option<AttemptIdentity>,
    pub(crate) payload: Option<PreparedPayload>,
    pub(crate) confirmation_deadline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resolved_source: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) payload_window: Option<PayloadWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resolved_target: Option<crate::services::tui_input::target_epoch::PaneEpoch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) preparation_hold: Option<serde_json::Value>,
    /// The attempt a legacy snapshot recorded, kept verbatim because it names no provider or target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) legacy_attempt: Option<super::mend::HeadAttempt>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PayloadWindow {
    Bootstrap,
    Started { at_ms: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttemptIdentity {
    pub(crate) nonce: String,
    pub(crate) provider: String,
    pub(crate) target: TargetIdentity,
    pub(crate) source: AttemptSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TargetIdentity {
    pub(crate) host: String,
    pub(crate) owner: String,
    pub(crate) root: String,
    pub(crate) execution_nonce: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AttemptSource {
    Existing {
        session_id: String,
        transcript: String,
        file: [u64; 2],
        offset: u64,
    },
    PreSource {
        expected_session_id: Option<String>,
        expected_transcript: Option<String>,
        binding_watermark: Option<String>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PreparedPayload {
    pub(crate) frame: String,
    pub(crate) digest: String,
    pub(crate) attribution: serde_json::Value,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RetainedInput {
    pub(crate) input: InputAttempt,
    pub(crate) reason: RetainedReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) disposition: Option<RetainedDisposition>,
    pub(crate) notice: NoticeIntent,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetainedDisposition {
    OpaqueUnsent,
    KnownNotSent,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RetainedReason {
    Unconfirmed,
    Rejected { reason: String },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct NoticeIntent {
    pub(crate) kind: String,
    pub(crate) enqueued: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TerminalIdentity {
    pub(crate) identity: QueueIdentity,
    pub(crate) settled_at_ms: u64,
}

/// Optional caller provenance stored with an admission receipt in the existing snapshot.
#[cfg(test)]
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct AdmissionContext {
    #[serde(default)]
    pub(crate) original: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) execution: Option<serde_json::Value>,
}
