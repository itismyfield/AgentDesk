use super::{DeliveryLeaseKey, LeaseOutcome};

pub(super) fn begin_relay_attempt(key: &DeliveryLeaseKey) {
    super::outbound::delivery_evidence_store::begin_relay_attempt(key);
}

pub(super) fn record_relay_commit(key: &DeliveryLeaseKey, outcome: LeaseOutcome) {
    match outcome {
        LeaseOutcome::Delivered => {
            super::outbound::delivery_evidence_store::record_confirmed_relay(key);
        }
        LeaseOutcome::NotDelivered => {
            super::outbound::delivery_evidence_store::record_not_delivered(key);
        }
        LeaseOutcome::Unknown => {
            super::outbound::delivery_evidence_store::record_unknown_relay(key);
        }
    }
}

pub(super) fn record_relay_unknown(key: &DeliveryLeaseKey) {
    super::outbound::delivery_evidence_store::record_unknown_relay(key);
}
