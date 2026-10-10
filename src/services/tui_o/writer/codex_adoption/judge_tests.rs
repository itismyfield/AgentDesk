use serde_json::{Value, json};

use super::judge::{Judgment, Verdict, judge};
use super::{Evidence, Obligation};

/// A history channel every boundary check passes: a current source consumed to a quiet suffix with a
/// linked wrapper, one replaced and one exited past source, an empty named one, receipts complete.
pub(super) fn eligible(channel: u64) -> Value {
    let clear: serde_json::Map<String, Value> = Obligation::ALL
        .iter()
        .map(|kind| (super::judge::snake(kind), json!("clear")))
        .collect();
    let past = |retirement| {
        json!({"role": "retired", "bytes": 70_000, "proof": "linked", "prefix": "strict",
            "retirement": retirement, "receipts": "complete"})
    };
    json!({
        "channel": channel, "provider": "codex", "runtime_kind": "codex_tui", "role": "gateway",
        "store": "fresh", "candidate": "pending",
        "discovery": "complete", "recovery": "complete",
        "obligations": clear, "emission_epoch": 7, "anchor": {"status": "latest", "id": 900},
        "sources": [
            {"role": "current", "bytes": 4096, "proof": "linked", "native_cursor": 4000,
             "prefix": "strict", "suffix": "quiet", "closed": "own", "receipts": "complete",
             "wrapper": {"proof": "linked", "cursor": 900, "eof": 900, "floor": 900,
                         "suffix": "quiet", "backlog": "clear"}},
            past("replaced"),
            past("exited"),
            {"role": "named", "bytes": 0},
        ],
    })
}

pub(super) fn judged(evidence: &Value) -> Judgment {
    let evidence: Evidence = serde_json::from_value(evidence.clone()).unwrap();
    judge(&evidence)
}

fn with(pointer: &str, value: Value) -> Value {
    let mut evidence = eligible(1);
    match value {
        Value::Null => {
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            let parent = evidence.pointer_mut(parent).unwrap();
            if let Some(object) = parent.as_object_mut() {
                object.remove(key);
            }
        }
        value => *evidence.pointer_mut(pointer).unwrap() = value,
    }
    evidence
}

#[test]
fn an_evidenced_history_passes_the_boundary_and_strict_rules_only_while_receipts_cover_it() {
    let passed = judged(&eligible(1));
    assert_eq!(
        (passed.boundary, passed.strict),
        (Verdict::Eligible, Verdict::Eligible)
    );
    assert!(
        passed.refused.is_empty() && passed.unknown.is_empty(),
        "{passed:?}"
    );
    // Receipts lost to the 32-entry window, or never kept for direct turns and tools, refuse only
    // the strict rule; the consumption boundary and no obligation carry the boundary rule.
    for (pointer, value, strict) in [
        ("/sources/0/receipts", "gap", Verdict::Refused),
        ("/sources/1/receipts", "unknown", Verdict::Unknown),
    ] {
        let judged = judged(&with(pointer, json!(value)));
        assert_eq!(
            (judged.boundary, judged.strict),
            (Verdict::Eligible, strict),
            "{pointer}"
        );
    }
}

#[test]
fn each_missing_or_failing_piece_of_evidence_refuses_or_stays_unknown() {
    use Verdict::{Refused, Unknown};
    let cases: &[(&str, Value, Verdict, &str)] = &[
        ("/role", json!("standby"), Refused, "role_unsupported"),
        ("/role", Value::Null, Unknown, "role"),
        ("/discovery", json!("spawned"), Refused, "discovery_spawned"),
        ("/discovery", json!("skipped"), Refused, "discovery_skipped"),
        ("/recovery", json!("timeout"), Refused, "recovery_timeout"),
        ("/recovery", Value::Null, Unknown, "recovery"),
        (
            "/anchor",
            json!({"status": "skipped"}),
            Unknown,
            "anchor_skipped",
        ),
        ("/anchor", Value::Null, Unknown, "anchor"),
        ("/provider", json!("gemini"), Unknown, "scope.provider"),
        (
            "/sources/0/proof",
            json!("mismatch"),
            Refused,
            "current.proof_mismatch",
        ),
        (
            "/sources/0/proof",
            json!("unlinked"),
            Refused,
            "current.proof_unlinked",
        ),
        ("/sources/0/proof", Value::Null, Unknown, "current.proof"),
        (
            "/sources/0/bytes",
            Value::Null,
            Unknown,
            "current.unreadable",
        ),
        (
            "/sources/0/native_cursor",
            Value::Null,
            Unknown,
            "current.cursor",
        ),
        (
            "/sources/0/native_cursor",
            json!(4097),
            Refused,
            "current.cursor_past_eof",
        ),
        (
            "/sources/0/prefix",
            json!("malformed"),
            Refused,
            "current.prefix_malformed",
        ),
        (
            "/sources/0/suffix",
            json!("prompt"),
            Refused,
            "current.suffix_prompt",
        ),
        (
            "/sources/0/suffix",
            json!("output"),
            Refused,
            "current.suffix_output",
        ),
        (
            "/sources/0/suffix",
            json!("start"),
            Refused,
            "current.suffix_start",
        ),
        (
            "/sources/0/suffix",
            json!("partial"),
            Refused,
            "current.suffix_partial",
        ),
        (
            "/sources/0/suffix",
            json!("unrecognized"),
            Refused,
            "current.suffix_unrecognized",
        ),
        ("/sources/0/suffix", Value::Null, Unknown, "current.suffix"),
        (
            "/sources/0/closed",
            json!("open"),
            Refused,
            "current.turn_open",
        ),
        ("/sources/0/closed", Value::Null, Unknown, "current.closed"),
        (
            "/sources/0/wrapper/backlog",
            json!("busy"),
            Refused,
            "wrapper.backlog",
        ),
        (
            "/sources/0/wrapper/backlog",
            Value::Null,
            Unknown,
            "wrapper.backlog",
        ),
        (
            "/sources/0/wrapper/suffix",
            json!("output"),
            Refused,
            "wrapper.suffix_output",
        ),
        (
            "/sources/0/wrapper/proof",
            Value::Null,
            Refused,
            "wrapper.unsupported",
        ),
        (
            "/sources/0/wrapper/floor",
            Value::Null,
            Unknown,
            "wrapper.floor",
        ),
        (
            "/sources/0/wrapper/floor",
            json!(901),
            Refused,
            "wrapper.floor_past_eof",
        ),
        (
            "/sources/0/wrapper/cursor",
            json!(901),
            Refused,
            "wrapper.cursor_past_eof",
        ),
        (
            "/sources/0/role",
            json!("retired"),
            Refused,
            "no_current_source",
        ),
        (
            "/sources/1/proof",
            json!("mismatch"),
            Refused,
            "retired.proof_mismatch",
        ),
        ("/sources/1/prefix", Value::Null, Unknown, "retired.prefix"),
        (
            "/sources/1/retirement",
            json!("live"),
            Refused,
            "retired.live",
        ),
        (
            "/sources/1/retirement",
            json!("missing"),
            Refused,
            "retired.unproven",
        ),
        (
            "/sources/2/retirement",
            Value::Null,
            Unknown,
            "retired.retirement",
        ),
        ("/sources/3/bytes", json!(1), Refused, "named.nonempty"),
        ("/sources/3/bytes", Value::Null, Unknown, "named.unreadable"),
        ("/sources/3/role", json!("parent"), Unknown, "source_role"),
        (
            "/sources/1/bytes",
            json!(128u64 << 20),
            Refused,
            "budget_bytes",
        ),
    ];
    for (pointer, value, verdict, code) in cases {
        let judged = judged(&with(pointer, value.clone()));
        let codes = if *verdict == Refused {
            &judged.refused
        } else {
            &judged.unknown
        };
        assert_eq!(judged.boundary, *verdict, "{pointer}={value}: {judged:?}");
        assert!(codes.contains(*code), "{pointer}={value}: {judged:?}");
    }
}

#[test]
fn every_obligation_kind_must_read_clear() {
    for kind in Obligation::ALL.iter().copied() {
        let name = super::judge::snake(&kind);
        let pointer = format!("/obligations/{name}");
        for (value, verdict, code) in [
            (json!("busy"), Verdict::Refused, format!("busy.{name}")),
            (
                json!("unknown"),
                Verdict::Unknown,
                format!("obligation.{name}"),
            ),
            (Value::Null, Verdict::Unknown, format!("obligation.{name}")),
        ] {
            let judged = judged(&with(&pointer, value.clone()));
            let codes = if verdict == Verdict::Refused {
                &judged.refused
            } else {
                &judged.unknown
            };
            assert_eq!(judged.boundary, verdict, "{pointer}={value}");
            assert!(codes.contains(&code), "{pointer}={value}: {judged:?}");
        }
    }
    let mut unrecognized = eligible(1);
    unrecognized["obligations"]["outbox"] = json!("busy");
    assert_eq!(judged(&unrecognized).boundary, Verdict::Unknown);
}

#[test]
fn only_a_definite_state_leaves_the_denominator() {
    let budget = (0..65).map(|_| json!({"role": "named", "bytes": 0}));
    let mut many = eligible(1);
    many["sources"].as_array_mut().unwrap().extend(budget);
    assert!(judged(&many).refused.contains("budget_sources"));
    for (pointer, value) in [
        ("/provider", json!("claude")),
        ("/store", json!("store")),
        ("/candidate", json!("released")),
    ] {
        assert_eq!(
            judged(&with(pointer, value)).boundary,
            Verdict::OutOfScope,
            "{pointer}"
        );
    }
    let mut empty = eligible(1);
    for source in empty["sources"].as_array_mut().unwrap() {
        source["bytes"] = json!(0);
    }
    assert_eq!(judged(&empty).boundary, Verdict::OutOfScope);
    // An unstated store or candidate keeps the channel counted, as Unknown.
    for pointer in ["/store", "/candidate", "/runtime_kind"] {
        assert_eq!(
            judged(&with(pointer, Value::Null)).boundary,
            Verdict::Unknown,
            "{pointer}"
        );
    }
}

#[test]
fn a_wrapper_is_skipped_only_when_its_absence_is_stated() {
    let wrapper = "/sources/0/wrapper";
    let mut null = eligible(1);
    null["sources"][0]["wrapper"] = Value::Null;
    for (case, evidence) in [("deleted", with(wrapper, Value::Null)), ("null", null)] {
        let judged = judged(&evidence);
        assert_eq!(judged.boundary, Verdict::Unknown, "{case}");
        assert!(judged.unknown.contains("wrapper"), "{case}: {judged:?}");
    }
    assert_eq!(
        judged(&with(wrapper, json!("absent"))).boundary,
        Verdict::Eligible
    );
    let busy = judged(&with("/sources/0/wrapper/backlog", json!("busy")));
    assert!(busy.boundary == Verdict::Refused && busy.refused.contains("wrapper.backlog"));
    let mut null_cursor = eligible(1);
    null_cursor["sources"][0]["wrapper"]["cursor"] = Value::Null;
    for evidence in [with("/sources/0/wrapper/cursor", Value::Null), null_cursor] {
        let judged = judged(&evidence);
        assert_eq!(judged.boundary, Verdict::Unknown);
        assert!(judged.unknown.contains("wrapper.cursor"), "{judged:?}");
    }
}

#[test]
fn every_wrapper_spool_counts_against_the_file_budget() {
    let sized = |currents: usize| {
        let mut evidence = eligible(1);
        let current = evidence["sources"][0].clone();
        evidence["sources"] = Value::Array(vec![current; currents]);
        judged(&evidence)
    };
    assert_eq!(
        sized(32).boundary,
        Verdict::Eligible,
        "32 rollouts and 32 spools"
    );
    let over = sized(33);
    assert!(over.refused.contains("budget_sources"), "{over:?}");
}
