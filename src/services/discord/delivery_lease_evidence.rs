use super::{DeliveryLeaseKey, LeaseOutcome};

pub(super) fn begin_relay_attempt(key: &DeliveryLeaseKey) {
    super::outbound::delivery_evidence_store::begin_relay_attempt(key);
}

pub(super) fn record_relay_commit(key: &DeliveryLeaseKey, outcome: LeaseOutcome) {
    if outcome == LeaseOutcome::Delivered {
        super::outbound::delivery_evidence_store::record_confirmed_relay(key);
    }
}
