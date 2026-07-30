//! Process-lifetime evidence for terminal relay delivery leases.
//!
//! The lease key is the relay contract's turn identity. Recording only at the
//! lease acquire/confirmed-commit boundary excludes placeholders and other
//! auxiliary outbound writes while covering every watcher, bridge, and
//! session-bound terminal transport, including formatting-layer long sends.

use std::sync::OnceLock;

use dashmap::DashMap;

use crate::services::discord::DeliveryLeaseKey;

fn evidence() -> &'static DashMap<DeliveryLeaseKey, bool> {
    static EVIDENCE: OnceLock<DashMap<DeliveryLeaseKey, bool>> = OnceLock::new();
    EVIDENCE.get_or_init(DashMap::new)
}

pub(in crate::services::discord) fn begin_relay_attempt(key: &DeliveryLeaseKey) {
    evidence().entry(key.clone()).or_insert(false);
}

pub(in crate::services::discord) fn record_confirmed_relay(key: &DeliveryLeaseKey) {
    evidence().insert(key.clone(), true);
}

/// `None` means this process has not acquired a terminal relay lease for this
/// exact turn. Recovery must keep that unknown state fail-closed.
pub(in crate::services::discord) fn confirmed_relay_for_turn(
    key: &DeliveryLeaseKey,
) -> Option<bool> {
    evidence().get(key).map(|confirmed| *confirmed)
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
        assert_eq!(confirmed_relay_for_turn(&first), None);

        begin_relay_attempt(&first);
        assert_eq!(confirmed_relay_for_turn(&first), Some(false));
        assert_eq!(confirmed_relay_for_turn(&second), None);

        record_confirmed_relay(&first);
        assert_eq!(confirmed_relay_for_turn(&first), Some(true));
        assert_eq!(confirmed_relay_for_turn(&second), None);
    }

    #[test]
    fn a_later_attempt_cannot_erase_confirmed_relay_evidence() {
        let turn = key(50_220_002, 20);
        begin_relay_attempt(&turn);
        record_confirmed_relay(&turn);
        begin_relay_attempt(&turn);
        assert_eq!(confirmed_relay_for_turn(&turn), Some(true));
    }
}
