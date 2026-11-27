use serde_json::{Value, json};

use super::test_support::*;
use super::*;

#[test]
fn hosted_execution_decode_keeps_unreadable_payloads_unknown() {
    let owner = owner("100");
    assert_eq!(HostedRecord::decode(None), HostedRecord::Legacy);
    assert!(HostedRecord::decode(None).deletable());
    for state in [
        HostedState::Pending,
        HostedState::Bound,
        HostedState::Retired,
    ] {
        let known = record(&owner, "n1", state);
        let decoded = HostedRecord::decode(Some(&wire(&known)));
        assert_eq!(decoded, HostedRecord::Known(known));
        assert_eq!(
            decoded.deletable(),
            state == HostedState::Retired,
            "{state:?}"
        );
    }

    let bound = wire(&record(&owner, "n1", HostedState::Bound));
    let edit = |change: &dyn Fn(&mut Value)| {
        let mut raw = bound.clone();
        change(&mut raw);
        raw
    };
    let retired_with = |change: &dyn Fn(&mut Value)| {
        let mut raw = edit(change);
        raw["state"] = json!("retired");
        raw
    };
    let cases = [
        ("future schema", future_schema(&owner)),
        (
            "lost location key",
            edit(&|raw| drop(raw.as_object_mut().unwrap().remove("location"))),
        ),
        (
            "lost nested key",
            retired_with(&|raw| {
                drop(
                    raw["source_ref"]
                        .as_object_mut()
                        .unwrap()
                        .remove("baseline_event_seq"),
                )
            }),
        ),
        ("extra field", retired_with(&|raw| raw["lease"] = json!(1))),
        (
            "bound without location",
            edit(&|raw| raw["location"] = Value::Null),
        ),
        (
            "source nonce disagrees",
            retired_with(&|raw| raw["source_ref"]["execution_nonce"] = json!("n0")),
        ),
        (
            "evidence nonce disagrees",
            retired_with(&|raw| raw["expected"]["binding_nonce"] = json!("n0")),
        ),
        (
            "non-herdr location",
            retired_with(&|raw| raw["location"]["host"] = json!("tmux")),
        ),
        (
            "blank pane",
            retired_with(&|raw| raw["location"]["pane_id"] = json!(" ")),
        ),
        (
            "unknown state",
            edit(&|raw| raw["state"] = json!("adopted")),
        ),
        ("json null", Value::Null),
        ("array", json!([1, "bound"])),
        ("empty object", json!({})),
    ];
    for (label, raw) in cases {
        let decoded = HostedRecord::decode(Some(&raw));
        assert_eq!(decoded, HostedRecord::Unknown(raw), "{label}");
        assert!(!decoded.deletable(), "{label} must keep its row");
    }
}
