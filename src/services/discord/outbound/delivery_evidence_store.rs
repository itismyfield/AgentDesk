//! Process-lifetime evidence for terminal relay delivery leases.
//!
//! The lease key is the relay contract's turn identity. Recording only at the
//! lease acquire/confirmed-commit boundary excludes placeholders and other
//! auxiliary outbound writes while covering watcher, bridge, and session-bound
//! terminal transports that actually use a delivery lease. Markerless and
//! otherwise unobservable paths are represented explicitly as `Unknown`.

use std::sync::OnceLock;

use dashmap::DashMap;
use serde::Serialize;

use crate::services::discord::DeliveryLeaseKey;

/// What the process-local lease evidence can prove for one exact turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum RelayDeliveryEvidence {
    /// The path is outside this evidence system or the identity is not exact.
    Unknown,
    /// The path is observable, but no terminal lease attempt has been recorded.
    NotAttempted,
    /// A terminal lease ended in a confirmed non-delivery outcome.
    NotDelivered,
    /// A terminal lease is currently active; health must yield to its blocker.
    AttemptedUnconfirmed,
    /// A terminal lease committed confirmed Discord delivery.
    Delivered,
}

fn evidence() -> &'static DashMap<DeliveryLeaseKey, RelayDeliveryEvidence> {
    static EVIDENCE: OnceLock<DashMap<DeliveryLeaseKey, RelayDeliveryEvidence>> = OnceLock::new();
    EVIDENCE.get_or_init(DashMap::new)
}

pub(in crate::services::discord) fn begin_relay_attempt(key: &DeliveryLeaseKey) {
    if key.is_degenerate_legacy() {
        return;
    }
    evidence()
        .entry(key.clone())
        .or_insert(RelayDeliveryEvidence::AttemptedUnconfirmed);
}

pub(in crate::services::discord) fn record_confirmed_relay(key: &DeliveryLeaseKey) {
    if key.is_degenerate_legacy() {
        return;
    }
    evidence().insert(key.clone(), RelayDeliveryEvidence::Delivered);
}

pub(in crate::services::discord) fn record_not_delivered(key: &DeliveryLeaseKey) {
    if key.is_degenerate_legacy() {
        return;
    }
    evidence()
        .entry(key.clone())
        .and_modify(|state| {
            if *state != RelayDeliveryEvidence::Delivered {
                *state = RelayDeliveryEvidence::NotDelivered;
            }
        })
        .or_insert(RelayDeliveryEvidence::NotDelivered);
}

/// Record that transport may have landed but this lease evidence cannot prove
/// the result. Confirmed evidence remains authoritative if already present.
pub(in crate::services::discord) fn record_unknown_relay(key: &DeliveryLeaseKey) {
    if key.is_degenerate_legacy() {
        return;
    }
    evidence()
        .entry(key.clone())
        .and_modify(|state| {
            if *state != RelayDeliveryEvidence::Delivered {
                *state = RelayDeliveryEvidence::Unknown;
            }
        })
        .or_insert(RelayDeliveryEvidence::Unknown);
}

/// Return the process-local evidence state for this exact turn.
///
/// Degenerate id-0 keys are never authoritative across semantic turns, so they
/// always return `Unknown` and writes for them are ignored.
pub(in crate::services::discord) fn relay_evidence_for_turn(
    key: &DeliveryLeaseKey,
) -> RelayDeliveryEvidence {
    if key.is_degenerate_legacy() {
        return RelayDeliveryEvidence::Unknown;
    }
    evidence()
        .get(key)
        .map(|state| *state)
        .unwrap_or(RelayDeliveryEvidence::NotAttempted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use poise::serenity_prelude::ChannelId;

    fn key(channel: u64, turn: u64) -> DeliveryLeaseKey {
        DeliveryLeaseKey::new(ChannelId::new(channel), 7, turn, None, None)
    }

    #[test]
    fn relay_evidence_is_scoped_to_the_exact_turn_key() {
        let first = key(50_220_001, 10);
        let second = key(50_220_001, 11);
        assert_eq!(
            relay_evidence_for_turn(&first),
            RelayDeliveryEvidence::NotAttempted
        );

        begin_relay_attempt(&first);
        assert_eq!(
            relay_evidence_for_turn(&first),
            RelayDeliveryEvidence::AttemptedUnconfirmed
        );
        record_not_delivered(&first);
        assert_eq!(
            relay_evidence_for_turn(&first),
            RelayDeliveryEvidence::NotDelivered
        );
        assert_eq!(
            relay_evidence_for_turn(&second),
            RelayDeliveryEvidence::NotAttempted
        );

        record_confirmed_relay(&first);
        assert_eq!(
            relay_evidence_for_turn(&first),
            RelayDeliveryEvidence::Delivered
        );
        assert_eq!(
            relay_evidence_for_turn(&second),
            RelayDeliveryEvidence::NotAttempted
        );
    }

    #[test]
    fn a_later_attempt_or_unknown_result_cannot_erase_confirmed_evidence() {
        let turn = key(50_220_002, 20);
        begin_relay_attempt(&turn);
        record_confirmed_relay(&turn);
        begin_relay_attempt(&turn);
        record_unknown_relay(&turn);
        assert_eq!(
            relay_evidence_for_turn(&turn),
            RelayDeliveryEvidence::Delivered
        );
    }

    #[test]
    fn ambiguous_transport_is_explicitly_unknown() {
        let turn = key(50_220_005, 30);
        begin_relay_attempt(&turn);
        record_unknown_relay(&turn);
        assert_eq!(
            relay_evidence_for_turn(&turn),
            RelayDeliveryEvidence::Unknown
        );
    }

    #[test]
    fn distinct_degenerate_turns_never_share_authoritative_evidence() {
        let first = DeliveryLeaseKey::new(ChannelId::new(50_220_006), 7, 0, None, None);
        let later_semantic_turn =
            DeliveryLeaseKey::new(ChannelId::new(50_220_006), 7, 0, None, None);
        assert_eq!(first, later_semantic_turn);

        begin_relay_attempt(&first);
        record_confirmed_relay(&first);
        assert_eq!(
            relay_evidence_for_turn(&first),
            RelayDeliveryEvidence::Unknown
        );
        assert_eq!(
            relay_evidence_for_turn(&later_semantic_turn),
            RelayDeliveryEvidence::Unknown,
            "a degenerate prior turn must not lend attempted or delivered evidence to a later turn"
        );
    }
}
