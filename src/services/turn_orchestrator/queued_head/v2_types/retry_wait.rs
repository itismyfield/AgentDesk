//! The owner’s retry-wait state.
use super::RefusedMember;
pub(crate) use crate::domain::retry_wait::RetryCause;

/// One front's wait, kept across restarts; callbacks name it by front and `wait_seq` alone.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RetryWait {
    pub(crate) ticket: Option<String>,
    pub(crate) front: RefusedMember,
    pub(crate) wait_seq: u64,
    pub(crate) cause: RetryCause,
    pub(crate) due_at_ms: u64,
    /// The dues this front consumed for the same execution.
    pub(crate) attempts: u32,
    pub(crate) armed: bool,
}

/// The exact queued original whose first payload was proven unsent on one execution.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct UnsentExecution {
    pub(crate) ticket: Option<String>,
    pub(crate) front: RefusedMember,
    pub(crate) nonce: String,
}

/// The current wait and the last wait number issued; a number is never issued twice.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Waits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) wait: Option<RetryWait>,
    #[serde(default)]
    pub(crate) seq: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) unsent: Vec<UnsentExecution>,
}

impl Waits {
    pub(crate) fn is_unused(&self) -> bool {
        self.wait.is_none() && self.seq == 0 && self.unsent.is_empty()
    }
}
