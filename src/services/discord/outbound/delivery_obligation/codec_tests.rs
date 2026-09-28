use serde_json::Value;

use super::schema::{ObligationLedger, WholeCommit};
use super::state::{BlockedReason, HandoffState, SettlementOutcome, TransportOutcome};

fn fixture() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/delivery_obligation/ledger.json"
    )))
    .unwrap()
}

#[test]
fn schema_roundtrip_retains_cap_token_and_unknown_diagnostics() {
    let value = fixture()["obligation_ledger"].clone();
    let ledger: ObligationLedger = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&ledger).unwrap(), value);
    let mut unknown = value;
    for field in ["prepared_at_ms", "chunk_nonces", "chunk_total"] {
        unknown["open"][0]["attempt"]
            .as_object_mut()
            .unwrap()
            .remove(field);
    }
    unknown["open"][0]
        .as_object_mut()
        .unwrap()
        .remove("unresolved_since_ms");
    let unknown: ObligationLedger = serde_json::from_value(unknown).unwrap();
    let attempt = unknown.open[0].attempt.as_ref().unwrap();
    assert_eq!(
        (
            attempt.prepared_at_ms,
            attempt.chunk_total,
            unknown.open[0].unresolved_since_ms
        ),
        (None, None, None)
    );
    assert_eq!(attempt.chunk_nonces, None);
}

#[test]
fn frontier_serde_preserves_absence_and_rejects_explicit_null_identity() {
    let value = fixture()["delivered_frontier"].clone();
    let frontier: WholeCommit = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&frontier).unwrap(), value);
    for field in ["source_dev", "source_ino"] {
        let mut bad = value.clone();
        bad[field] = Value::Null;
        assert!(serde_json::from_value::<WholeCommit>(bad).is_err());
    }
}

#[test]
fn protocol_one_preserves_unknown_and_handoff_states_roundtrip() {
    for outcome in [
        TransportOutcome::NotIssued,
        TransportOutcome::FirstRejected,
        TransportOutcome::MaybePosted { receipts: vec![] },
    ] {
        assert_eq!(
            super::protocol::settlement(1, &outcome),
            Ok(SettlementOutcome::Unknown)
        );
        assert_eq!(
            super::protocol::settlement(99, &outcome),
            Err(BlockedReason::UnknownProtocol)
        );
    }
    assert_eq!(
        super::protocol::settlement(2, &TransportOutcome::FirstRejected),
        Ok(SettlementOutcome::Withdrawn)
    );
    assert!(!super::protocol::supports_automatic_settlement(1));
    assert!(!super::protocol::supports_automatic_settlement(99));
    for state in [
        HandoffState::Admitted {
            intent_written: false,
        },
        HandoffState::Admitted {
            intent_written: true,
        },
        HandoffState::Issuing,
        HandoffState::Settled,
        HandoffState::Released,
    ] {
        assert_eq!(
            serde_json::from_value::<HandoffState>(serde_json::to_value(state).unwrap()).unwrap(),
            state
        );
    }
}
