//! Wire values for unfinished marker cleanup; no repository or runtime operations.
use super::{AttemptIdentity, QueueIdentity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarkerOwner {
    pub(crate) provider: String,
    pub(crate) discord_token_hash: String,
    pub(crate) channel_id: String,
    pub(crate) logical_key: String,
    pub(crate) owner_node: String,
    pub(crate) runtime_root: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarkerControl {
    pub(crate) cleanup_id: String,
    pub(crate) identity: QueueIdentity,
    pub(crate) front_receipt: Option<String>,
    pub(crate) epoch: String,
    pub(crate) expected: MarkerRowExpectation,
    pub(crate) paths: Vec<MarkerPathSnapshot>,
    pub(crate) stage: MarkerStage,
    pub(crate) attempt: Option<AttemptIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MarkerRowExpectation {
    Missing {
        owner: MarkerOwner,
    },
    Legacy {
        owner: MarkerOwner,
        row_id: i64,
        exact_session_key: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarkerPathSnapshot {
    pub(crate) path: PathBuf,
    pub(crate) parent: [u64; 2],
    pub(crate) entry: MarkerEntry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MarkerEntry {
    Absent,
    CodexTui { file: [u64; 2] },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MarkerStage {
    Intent,
    Receipt,
    Outbox { event_id: String },
}
