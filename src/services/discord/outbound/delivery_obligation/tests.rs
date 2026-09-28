use serde_json::{Value, json};

use super::load::load_ledger;
use super::reader::{FenceState, LedgerLoad};
use super::schema::ObligationLedger;
use super::state::{BlockedReason, SourceObs};

fn fixture() -> Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/delivery_obligation/ledger.json"
    )))
    .unwrap()
}

fn source() -> SourceObs {
    let ledger: ObligationLedger =
        serde_json::from_value(fixture()["obligation_ledger"].clone()).unwrap();
    SourceObs::Available {
        token: ledger.publication.epoch,
        size: 300,
        publication: Some(ledger.publication),
    }
}

fn read(record: Option<&Value>, fence: Option<&str>, obs: SourceObs) -> LedgerLoad {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("77.json");
    let fence_path = dir.path().join("77.fence.json");
    if let Some(record) = record {
        std::fs::write(&path, record.to_string()).unwrap();
    }
    if let Some(fence) = fence {
        std::fs::write(&fence_path, fence).unwrap();
    }
    let before = (std::fs::read(&path).ok(), std::fs::read(&fence_path).ok());
    let loaded = load_ledger(&path, &fence_path, obs);
    assert_eq!(
        before,
        (std::fs::read(&path).ok(), std::fs::read(&fence_path).ok())
    );
    loaded
}

#[test]
fn unknown_schema_protocol_and_variants_fail_closed() {
    for (field, reason) in [
        ("ledger_schema", BlockedReason::UnknownSchema),
        ("ledger_protocol", BlockedReason::UnknownProtocol),
    ] {
        for version in [0, 2, u32::MAX] {
            let mut value = fixture();
            value[field] = json!(version);
            assert_eq!(
                read(Some(&value), Some(r#"{"rev":5}"#), source()),
                LedgerLoad::Blocked(reason)
            );
        }
    }
    for pointer in [
        "/obligation_ledger/open/0/class",
        "/obligation_ledger/open/0/attempt/receipts/0/cleanup",
    ] {
        let mut value = fixture();
        *value.pointer_mut(pointer).unwrap() = json!("FutureVariant");
        assert_eq!(
            read(Some(&value), None, source()),
            LedgerLoad::Blocked(BlockedReason::Corrupt)
        );
    }
}

#[test]
fn publication_bundle_rejects_stale_or_incomplete_snapshots() {
    for pointer in [
        "/rev",
        "/extent_end",
        "/epoch/serial",
        "/epoch/reset_incarnation",
        "/epoch/generation_mtime_ns",
        "/epoch/source_dev",
        "/epoch/source_ino",
        "/digest/0",
    ] {
        let mut value = fixture();
        *value["obligation_ledger"]["publication"]
            .pointer_mut(pointer)
            .unwrap() = json!(99);
        assert_eq!(
            read(Some(&value), None, source()),
            LedgerLoad::Blocked(BlockedReason::PublicationMismatch),
            "{pointer}"
        );
    }
    for field in ["epoch", "rev", "extent_end", "digest"] {
        let mut value = fixture();
        value["obligation_ledger"]["publication"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert_eq!(
            read(Some(&value), None, source()),
            LedgerLoad::Blocked(BlockedReason::Corrupt)
        );
    }
    for pointer in ["/open/0/range/1", "/intents/0/1", "/open/0/attempt/range/1"] {
        let mut value = fixture();
        *value["obligation_ledger"].pointer_mut(pointer).unwrap() = json!(301);
        assert_eq!(
            read(Some(&value), None, source()),
            LedgerLoad::Blocked(BlockedReason::PublicationMismatch)
        );
    }
    assert_eq!(
        read(Some(&fixture()), None, SourceObs::Unavailable),
        LedgerLoad::Blocked(BlockedReason::SourceUnavailable)
    );
}

#[test]
fn fence_classification_never_repairs_or_uses_revision_as_authority() {
    for (record, fence, reason) in [
        (None, Some(r#"{"rev":5}"#), BlockedReason::Unavailable),
        (
            Some(json!({})),
            Some(r#"{"rev":5}"#),
            BlockedReason::Incompatible,
        ),
        (Some(fixture()), Some("malformed"), BlockedReason::Corrupt),
    ] {
        assert_eq!(
            read(record.as_ref(), fence, source()),
            LedgerLoad::Blocked(reason)
        );
    }
    let mut empty = fixture();
    for field in ["open", "held", "intents"] {
        empty["obligation_ledger"][field] = json!([]);
    }
    for (record, fence, expected) in [
        (None, None, FenceState::Absent),
        (Some(json!({})), None, FenceState::Absent),
        (Some(fixture()), None, FenceState::RecreateRequired),
        (Some(fixture()), Some(r#"{"rev":99}"#), FenceState::Present),
        (
            Some(empty),
            Some(r#"{"rev":1}"#),
            FenceState::IncompleteClear,
        ),
    ] {
        let LedgerLoad::Loaded(loaded) = read(record.as_ref(), fence, source()) else {
            panic!("unexpected blocked")
        };
        assert_eq!(loaded.fence, expected);
    }
}

#[test]
fn malformed_and_unreadable_records_never_become_empty_ledgers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("77.json");
    let fence = dir.path().join("77.fence.json");
    std::fs::create_dir(&path).unwrap();
    assert_eq!(
        load_ledger(&path, &fence, source()),
        LedgerLoad::Blocked(BlockedReason::Unavailable)
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, "{malformed").unwrap();
    assert_eq!(
        load_ledger(&path, &fence, source()),
        LedgerLoad::Blocked(BlockedReason::Corrupt)
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "{malformed");
}

#[test]
fn unknown_top_level_fields_survive_loading_and_document_rewrite() {
    let value = fixture();
    let LedgerLoad::Loaded(mut loaded) = read(Some(&value), Some(r#"{"rev":5}"#), source()) else {
        panic!("fixture blocked")
    };
    assert_eq!(serde_json::to_value(&loaded.document).unwrap(), value);
    assert_eq!(
        loaded.document.extensions()["future_scalar"],
        value["future_scalar"]
    );
    let original = loaded
        .document
        .obligation_ledger
        .as_ref()
        .unwrap()
        .publication;
    loaded
        .document
        .obligation_ledger
        .as_mut()
        .unwrap()
        .publication
        .rev += 1;
    let encoded = serde_json::to_string(&loaded.document).unwrap();
    let reread: super::reader::LedgerDocument = serde_json::from_str(&encoded).unwrap();
    assert_eq!(reread.extensions(), loaded.document.extensions());
    assert_eq!(
        reread.extensions()["future_section"],
        value["future_section"]
    );
    assert_ne!(reread.obligation_ledger.unwrap().publication, original);
}

#[test]
fn legacy_loading_preserves_absence_and_refuses_partial_binding() {
    let LedgerLoad::Loaded(loaded) = read(Some(&fixture()), None, source()) else {
        panic!("legacy frontier blocked")
    };
    assert_eq!(
        loaded.identity,
        super::state::SourceIdentityState::LegacyUnbound
    );
    let frontier = serde_json::to_value(loaded.document.delivered_frontier.unwrap()).unwrap();
    assert!(frontier.get("source_dev").is_none());
    assert!(frontier.get("source_ino").is_none());
    for field in ["source_dev", "source_ino"] {
        let mut partial = fixture();
        partial["delivered_frontier"][field] = json!(7);
        assert_eq!(
            read(Some(&partial), None, source()),
            LedgerLoad::Blocked(BlockedReason::IdentityIncomplete)
        );
    }
}
