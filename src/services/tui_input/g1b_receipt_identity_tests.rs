//! One envelope order for identity and coverage; an explicit identity never falls back.
use super::*;
use crate::services::tui_input::ledger::Record;
use serde_json::json;

fn identity(key: u64, sources: &[u64], original: u64, execution: u64) -> ReceiptIdentity {
    ReceiptIdentity::new(key, sources.to_vec(), 7, original, execution).unwrap()
}

fn fold(key: u64, input: Value) -> Rows {
    let record = Record {
        seq: 1,
        prev_crc: 0,
        crc: 0,
        kind: "received".into(),
        payload: json!({"key":key,"input":input}),
    };
    Rows::fold(None, &[record]).unwrap()
}

fn explicit(sources: Value, author: u64) -> Value {
    json!({"source_ids":sources,"author_id":author,"original_channel_id":8,"execution_channel_id":9})
}

#[test]
fn g1b_explicit_priority_and_malformed_do_not_fallback() {
    let top = explicit(json!([10, 11]), 7);
    let nested = explicit(json!([10, 12]), 7);
    // The nested-only envelope from the review restores in full without a channel_id.
    let rows = fold(10, json!({"legacy_input":{"receipt_identity":nested}}));
    assert_eq!(
        rows.receipt_identity(10),
        Some(identity(10, &[10, 12], 8, 9))
    );
    assert_eq!(
        rows.responsibility(&identity(12, &[12], 8, 9)),
        Responsibility::Known {
            key: 10,
            received_seq: 1
        }
    );
    // A present top-level identity wins over the nested one for identity and coverage alike.
    let both = fold(
        10,
        json!({"receipt_identity":top,"legacy_input":{"receipt_identity":nested}}),
    );
    assert_eq!(
        both.receipt_identity(10),
        Some(identity(10, &[10, 11], 8, 9))
    );
    assert_eq!(
        both.responsibility(&identity(12, &[12], 8, 9)),
        Responsibility::Absent
    );
    // A malformed top-level identity names the coverage but restores nothing from below it.
    let malformed_top = fold(
        10,
        json!({"receipt_identity":explicit(json!([10, 11]), 0),"legacy_input":{"receipt_identity":nested}}),
    );
    assert_eq!(malformed_top.receipt_identity(10), None);
    for alias in [11, 12] {
        let query = identity(alias, &[alias], 8, 9);
        let expected = match alias {
            11 => Responsibility::Unknown,
            _ => Responsibility::Absent,
        };
        assert_eq!(malformed_top.responsibility(&query), expected, "{alias}");
    }
    // A malformed nested identity never falls back to complete Legacy fields.
    let legacy = json!({"receipt_identity":explicit(json!([10]), 0),"message_id":10,"source_message_ids":[10,11],"author_id":7,"channel_id":9});
    let malformed_nested = fold(10, json!({"legacy_input":legacy}));
    assert_eq!(malformed_nested.receipt_identity(10), None);
    assert_eq!(
        malformed_nested.responsibility(&identity(10, &[10], 9, 9)),
        Responsibility::Unknown
    );
}

#[test]
fn g1b_identity_unknown_zero_and_normalization_regressions() {
    let legacy = json!({"message_id":10,"source_message_ids":[10,11],"author_id":7,"channel_id":9});
    for bad in [
        explicit(json!([10, 0]), 7),
        explicit(json!([10, 10]), 7),
        explicit(json!([11]), 7),
        explicit(json!([10]), 0),
        json!({"source_ids":[10],"author_id":7}),
        Value::Null,
    ] {
        let mut nested = legacy.clone();
        nested["receipt_identity"] = bad.clone();
        let mut top = json!({"legacy_input":legacy});
        top["receipt_identity"] = bad.clone();
        for input in [json!({"legacy_input":nested}), top] {
            let rows = fold(10, input);
            assert_eq!(rows.receipt_identity(10), None, "{bad}");
            assert!(
                !matches!(
                    rows.responsibility(&identity(10, &[10], 9, 9)),
                    Responsibility::Known { .. } | Responsibility::Legacy { .. }
                ),
                "{bad}"
            );
        }
    }
    // Source order and rendering enrichment never change the canonical identity.
    let reordered = fold(
        11,
        json!({"text":"enriched","legacy_input":{"receipt_identity":explicit(json!([11, 10]), 7)}}),
    );
    assert_eq!(
        reordered.receipt_identity(11),
        Some(identity(11, &[10, 11], 8, 9))
    );
    assert!(identity(10, &[10, 11], 8, 9).same(&ReceiptIdentity {
        source_ids: vec![11, 10],
        ..identity(10, &[10, 11], 8, 9)
    }));
    assert!(!identity(10, &[10, 11], 8, 9).same(&identity(10, &[10, 12], 8, 9)));
}
