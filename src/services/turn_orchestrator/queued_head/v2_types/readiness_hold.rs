//! Durable records of a readiness hold on the owner's unattempted front and of its refusal without
//! an attempt; neither keeps a body or a readiness proof.
use super::{NoticeIntent, QueueIdentity, RefusedMember};
use serde::{Deserialize, Serialize};

/// The one deadline the owner fixed when readiness first held this unstarted original.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReadinessHold {
    pub(crate) identity: QueueIdentity,
    /// Digest of the original's canonical body: a changed input under the same identity is another.
    pub(crate) fingerprint: String,
    pub(crate) cause: String,
    pub(crate) started_at_ms: u64,
    pub(crate) due_at_ms: u64,
}

impl ReadinessHold {
    /// Whether the due passed, or the clock went back past the start and cannot be trusted.
    pub(crate) fn expired(&self, now: u64) -> bool {
        now >= self.due_at_ms || now < self.started_at_ms
    }
}

/// The front original a refusal before any attempt names; it carries no attempt by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FrontRef {
    pub(crate) identity: QueueIdentity,
}

/// A front refused for good before any attempt: what blocks its sources and the notice it owes.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RefusedFront {
    pub(crate) input: FrontRef,
    pub(crate) members: Vec<RefusedMember>,
    pub(crate) reason: String,
    pub(crate) notice: NoticeIntent,
    pub(crate) refused_at_ms: u64,
}

impl RefusedFront {
    /// Whether any source of this refusal is `message`.
    pub(crate) fn names(&self, message: u64) -> bool {
        self.members.iter().any(|member| {
            let id = &member.identity;
            id.primary == message || id.sources.iter().any(|s| s.message_id == message)
        })
    }
}
