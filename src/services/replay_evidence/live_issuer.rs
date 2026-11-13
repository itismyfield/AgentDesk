//! The original observer's private key authorizes evidence issuance.
use super::{NoEffectEvidence, TerminalEvidence};
use crate::domain::replay_evidence::TerminalKind;
use crate::services::discord::replay_policy::live::issuer_key::ObservationKey;

pub(crate) fn terminal(
    _key: ObservationKey,
    receipt_id: i64,
    nonce: String,
    kind: TerminalKind,
) -> TerminalEvidence {
    TerminalEvidence {
        receipt_id,
        nonce,
        kind,
    }
}

pub(crate) fn no_effect(_key: ObservationKey, receipt_id: i64, nonce: String) -> NoEffectEvidence {
    NoEffectEvidence { receipt_id, nonce }
}
