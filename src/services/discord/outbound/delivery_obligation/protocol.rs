use super::state::{BlockedReason, SettlementOutcome, TransportOutcome};

/// Only typed protocol-two results can establish that a rejected send never posted.
pub(in crate::services::discord) fn settlement(
    protocol: u32,
    outcome: &TransportOutcome,
) -> Result<SettlementOutcome, BlockedReason> {
    match (protocol, outcome) {
        (1 | 2, TransportOutcome::Confirmed { .. }) => Ok(SettlementOutcome::Confirmed),
        (2, TransportOutcome::NotIssued | TransportOutcome::FirstRejected) => {
            Ok(SettlementOutcome::Withdrawn)
        }
        (1 | 2, _) => Ok(SettlementOutcome::Unknown),
        _ => Err(BlockedReason::UnknownProtocol),
    }
}

pub(in crate::services::discord) fn supports_automatic_settlement(protocol: u32) -> bool {
    protocol == 2
}
