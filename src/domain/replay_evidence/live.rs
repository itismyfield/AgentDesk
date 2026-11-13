//! Only the keyed observer issuer can construct replay classification evidence.
use super::TerminalKind;

#[path = "../../services/replay_evidence/live_issuer.rs"]
pub(crate) mod issuer;

#[derive(Debug)]
pub(crate) struct TerminalEvidence {
    receipt_id: i64,
    nonce: String,
    kind: TerminalKind,
}

impl TerminalEvidence {
    pub(crate) fn names(&self, receipt_id: i64, nonce: &str) -> bool {
        self.receipt_id == receipt_id && self.nonce == nonce
    }

    pub(crate) fn kind(&self) -> TerminalKind {
        self.kind
    }
}

#[derive(Debug)]
pub(crate) struct NoEffectEvidence {
    receipt_id: i64,
    nonce: String,
}

impl NoEffectEvidence {
    pub(crate) fn names(&self, receipt_id: i64, nonce: &str) -> bool {
        self.receipt_id == receipt_id && self.nonce == nonce
    }
}
