//! Durable V2 data only; this slice never admits or executes a V2 input.
use super::{HeadPhase, PendingQueueItem, QueueIdentity};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) struct SnapshotV2 {
    pub(crate) version: u32,
    pub(crate) items: Vec<PendingQueueItem>,
    pub(crate) head_injection: Option<InputAttempt>,
    pub(crate) retained_inputs: Vec<RetainedInput>,
    pub(crate) terminal_identities: Vec<TerminalIdentity>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct InputAttempt {
    pub(crate) identity: QueueIdentity,
    pub(crate) original: PendingQueueItem,
    pub(crate) phase: HeadPhase,
    pub(crate) attempt: Option<AttemptIdentity>,
    pub(crate) payload: Option<PreparedPayload>,
    pub(crate) confirmation_deadline_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct AttemptIdentity {
    pub(crate) nonce: String,
    pub(crate) provider: String,
    pub(crate) target: TargetIdentity,
    pub(crate) source: AttemptSource,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct TargetIdentity {
    pub(crate) host: String,
    pub(crate) owner: String,
    pub(crate) root: String,
    pub(crate) execution_nonce: String,
}

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
pub(crate) struct PreparedPayload {
    pub(crate) frame: String,
    pub(crate) digest: String,
    pub(crate) attribution: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct RetainedInput {
    pub(crate) input: InputAttempt,
    pub(crate) reason: RetainedReason,
    pub(crate) notice: NoticeIntent,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RetainedReason {
    Unconfirmed,
    Rejected { reason: String },
}

#[derive(Serialize, Deserialize)]
pub(crate) struct NoticeIntent {
    pub(crate) kind: String,
    pub(crate) enqueued: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct TerminalIdentity {
    pub(crate) identity: QueueIdentity,
    pub(crate) settled_at_ms: u64,
}
