use serde_json::Value;

use super::schema::{ObligationLedger, WholeCommit};
use super::state::{BlockedReason, SourceIdentityState, SourceObs};

fn fixture() -> (ObligationLedger, WholeCommit, SourceObs) {
    let value: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/delivery_obligation/ledger.json"
    )))
    .unwrap();
    let ledger: ObligationLedger =
        serde_json::from_value(value["obligation_ledger"].clone()).unwrap();
    let frontier = serde_json::from_value(value["delivered_frontier"].clone()).unwrap();
    let observed = SourceObs::Available {
        token: ledger.publication.epoch,
        size: 300,
        publication: Some(ledger.publication),
    };
    (ledger, frontier, observed)
}

#[test]
fn identity_states_use_one_observed_token_without_binding_legacy() {
    let (ledger, mut frontier, source) = fixture();
    let token = ledger.publication.epoch;
    assert_eq!(
        super::validation::source_identity(&frontier, source),
        Ok(SourceIdentityState::LegacyUnbound)
    );
    assert_eq!((frontier.source_dev, frontier.source_ino), (None, None));
    frontier.source_dev = Some(token.source_dev);
    assert_eq!(
        super::validation::source_identity(&frontier, source),
        Err(BlockedReason::IdentityIncomplete)
    );
    frontier.source_ino = Some(token.source_ino);
    assert_eq!(
        super::validation::source_identity(&frontier, source),
        Ok(SourceIdentityState::BoundAndCurrent(token))
    );
    frontier.source_ino = Some(token.source_ino + 1);
    assert_eq!(
        super::validation::source_identity(&frontier, source),
        Ok(SourceIdentityState::BoundButChanged(token))
    );
    assert_eq!(
        super::validation::source_identity(&frontier, SourceObs::Unavailable),
        Ok(SourceIdentityState::SourceUnavailable)
    );
}

#[test]
fn publication_requires_complete_token_and_unchanged_captured_bundle() {
    let (ledger, frontier, source) = fixture();
    assert_eq!(
        super::validation::publication(&ledger, Some(&frontier), source),
        Ok(())
    );
    for field in [
        "generation",
        "dev",
        "ino",
        "serial",
        "reset",
        "extent",
        "digest",
        "rev",
        "intent",
    ] {
        let mut bad = ledger.clone();
        match field {
            "generation" => bad.publication.epoch.generation_mtime_ns += 1,
            "dev" => bad.publication.epoch.source_dev += 1,
            "ino" => bad.publication.epoch.source_ino += 1,
            "serial" => bad.publication.epoch.serial += 1,
            "reset" => bad.publication.epoch.reset_incarnation += 1,
            "extent" => bad.publication.extent_end += 1,
            "digest" => bad.publication.digest[0] += 1,
            "rev" => bad.publication.rev += 1,
            "intent" => bad.intents[0].1 += 1,
            _ => unreachable!(),
        }
        assert_eq!(
            super::validation::publication(&bad, Some(&frontier), source),
            Err(BlockedReason::PublicationMismatch),
            "{field}"
        );
    }
    assert_eq!(
        super::validation::publication(&ledger, Some(&frontier), SourceObs::Unavailable),
        Err(BlockedReason::SourceUnavailable)
    );
}
