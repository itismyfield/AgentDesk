//! Absence evidence for one piece: two completed history passes over one fixed scope. Only the
//! probe builds it, and it is no permit: the slot grant still checks the row it was read at.

use tokio::time::{Duration, Instant};

use super::matcher::ObservedMessage;
use crate::services::tui_o::repost::o_piece_attempts::AttemptRow;
use crate::services::tui_o::repost::o_piece_delivery::{DeliveryRow, PieceKey};

/// Bumped when attribution rules change, so evidence judged by other rules is not reused.
pub(crate) const MATCHER_VERSION: i32 = 1;
/// The first pass starts no sooner than this after the earlier send was seen settled.
pub(crate) const FIRST_PASS_AFTER: Duration = Duration::from_secs(10);
/// The second starts no sooner than this after the first completed, so read time adds to it.
pub(crate) const SECOND_PASS_AFTER: Duration = Duration::from_secs(20);

/// Who reads. A pass begun under one holder, run or credentials proves nothing under another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunScope {
    pub(crate) holder: String,
    pub(crate) run: String,
    pub(crate) credentials: String,
    pub(crate) evidence_generation: u64,
}

/// Everything absence evidence is about; any change means the passes looked for something else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EvidenceScope {
    pub(crate) key: PieceKey,
    pub(crate) payload_sha256: String,
    pub(crate) sender_id: u64,
    pub(crate) identity_version: i32,
    pub(crate) split_version: i32,
    pub(crate) matcher_version: i32,
    pub(crate) row_revision: i64,
    /// Every consumed slot, all settled: the sends the passes looked for.
    pub(crate) checked_slots: Vec<u8>,
    /// The piece's own stored anchor, never the ledger's latest one.
    pub(crate) original_anchor: u64,
    pub(crate) run: RunScope,
}

impl EvidenceScope {
    /// The scope of `row` as read now; `Err(slot)` while a slot is open, as its send may land.
    pub(crate) fn of(
        row: &DeliveryRow,
        attempts: &[AttemptRow],
        run: &RunScope,
    ) -> Result<Self, u8> {
        if let Some(open) = attempts.iter().find(|attempt| attempt.result.is_none()) {
            return Err(open.slot);
        }
        Ok(Self {
            key: row.key.clone(),
            payload_sha256: row.payload_sha256.clone(),
            sender_id: row.sender_id,
            identity_version: row.identity_version,
            split_version: row.split_version,
            matcher_version: MATCHER_VERSION,
            row_revision: row.revision,
            checked_slots: attempts.iter().map(|attempt| attempt.slot).collect(),
            original_anchor: row.original_anchor,
            run: run.clone(),
        })
    }

    /// Whether a send after the original was consumed, so the shared nonce cannot name a slot.
    pub(crate) fn additional_sent(&self) -> bool {
        self.checked_slots.iter().any(|slot| *slot > 0)
    }
}

/// A known message of the expected bot read back by id with the history's credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PermissionProof {
    message_id: u64,
    credentials: String,
}

impl PermissionProof {
    /// `None` unless the read returned the requested message, in the channel, by the sender.
    pub(super) fn verify(
        scope: &EvidenceScope,
        requested: u64,
        read: &ObservedMessage,
    ) -> Option<Self> {
        let proven = read.id == requested
            && read.channel_id == scope.key.unit().channel_id
            && read.author_id == scope.sender_id;
        proven.then(|| Self {
            message_id: requested,
            credentials: scope.run.credentials.clone(),
        })
    }

    pub(crate) fn message_id(&self) -> u64 {
        self.message_id
    }
}

/// One pass that read every page from its fixed upper bound down to the anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedPass {
    scope: EvidenceScope,
    number: u8,
    /// The newest id of the pass's first page; `None` when the channel had no message at all.
    upper: Option<u64>,
    started_at: Instant,
    completed_at: Instant,
    proof: PermissionProof,
}

impl CompletedPass {
    pub(super) fn new(
        scope: EvidenceScope,
        number: u8,
        upper: Option<u64>,
        started_at: Instant,
        proof: PermissionProof,
    ) -> Self {
        Self {
            scope,
            number,
            upper,
            started_at,
            completed_at: Instant::now(),
            proof,
        }
    }

    pub(crate) fn upper(&self) -> Option<u64> {
        self.upper
    }

    pub(crate) fn started_at(&self) -> Instant {
        self.started_at
    }

    pub(crate) fn completed_at(&self) -> Instant {
        self.completed_at
    }

    pub(crate) fn proof(&self) -> &PermissionProof {
        &self.proof
    }
}

/// Two clean passes in order and on time over one scope. Fields stay private so nothing outside
/// the probe can assemble one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NotFoundEvidence {
    passes: [CompletedPass; 2],
}

impl NotFoundEvidence {
    pub(super) fn from_passes(
        settled_at: Instant,
        first: CompletedPass,
        second: CompletedPass,
    ) -> Option<Self> {
        let timed = first.started_at >= settled_at + FIRST_PASS_AFTER
            && second.started_at >= first.completed_at + SECOND_PASS_AFTER;
        let paired = (first.number, second.number) == (1, 2)
            && first.scope == second.scope
            && first.proof.credentials == first.scope.run.credentials
            && second.proof.credentials == second.scope.run.credentials;
        (timed && paired).then_some(Self {
            passes: [first, second],
        })
    }

    pub(crate) fn scope(&self) -> &EvidenceScope {
        &self.passes[0].scope
    }

    pub(crate) fn passes(&self) -> &[CompletedPass; 2] {
        &self.passes
    }

    /// Whether the evidence still describes the row, slots and reader as read now. Approval,
    /// holder liveness, lease and cap stay the grant's to check.
    pub(crate) fn validate_current(
        &self,
        row: &DeliveryRow,
        attempts: &[AttemptRow],
        run: &RunScope,
    ) -> bool {
        EvidenceScope::of(row, attempts, run).is_ok_and(|now| now == *self.scope())
    }
}
