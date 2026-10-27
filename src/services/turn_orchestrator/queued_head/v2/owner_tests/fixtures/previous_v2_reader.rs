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
    pub(crate) notice: NoticeIntent,
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

#[test]
fn previous_reader_decodes_current_writer_optional_metadata() {
    let bytes = super::current_writer_compat_golden();
    let old: SnapshotV2 = serde_json::from_str(&bytes)
        .expect("previous V2 read-only decoder must accept current writer");
    assert_eq!(old.version, 2);
    assert!(matches!(
        old.retained_inputs[0].reason,
        RetainedReason::Rejected { .. }
    ));
    assert!(old.head_injection.is_some());
}
