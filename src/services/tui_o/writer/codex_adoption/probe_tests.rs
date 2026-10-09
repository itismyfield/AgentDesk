use serde_json::json;

use super::judge_tests::eligible;
use super::probe::{Counts, probe};

fn snapshot(channels: Vec<serde_json::Value>) -> String {
    json!({"schema": 1, "sample_id": "s1", "boot_epoch": "b1", "channels": channels}).to_string()
}

#[test]
fn a_probe_counts_n_k_u_per_distinct_channel_without_body_text() {
    let mut refused = eligible(2);
    refused["sources"][0]["suffix"] = json!("prompt");
    let mut unknown = eligible(3);
    unknown["anchor"] = json!({"status": "skipped"});
    let mut strict_gap = eligible(4);
    strict_gap["sources"][1]["receipts"] = json!("gap");
    let mut excluded = eligible(5);
    excluded["store"] = json!("store");
    // A field the producer added beyond the schema is never echoed.
    let mut body = eligible(6);
    body["sources"][0]["text"] = json!("SECRET-BODY");
    body["sources"][0]["closed"] = json!("open");
    let channels = vec![eligible(1), refused, unknown, strict_gap, excluded, body];
    let report = probe(&snapshot(channels)).unwrap();
    let expected = Counts {
        n: 5,
        k: 2,
        u: 1,
        refused: 2,
        strict_k: 1,
        strict_u: 1,
        excluded: 1,
    };
    assert_eq!(report.counts, expected);
    assert_eq!(report.reasons.get("current.suffix_prompt"), Some(&1));
    assert_eq!(report.reasons.get("strict.receipts_gap"), Some(&1));
    let rendered = report.render();
    assert!(!rendered.contains("SECRET-BODY"));
    assert_eq!(rendered.lines().count(), 7);
    let first = &report.lines[0];
    for field in [
        "event=codex_adoption_probe ",
        "channel=1 ",
        "native_cursor=4000 ",
        "relay_namespace=wrapper ",
        "retirement_proof=replaced,exited ",
        "anchor_status=latest ",
        "strict_eligible=eligible r2_eligible=eligible reasons=[]",
    ] {
        assert!(first.contains(field), "{field}: {first}");
    }
    let summary = report.lines.last().unwrap();
    assert!(
        summary.contains(" n=5 k=2 u=1 ") && summary.contains("range=2..3"),
        "{summary}"
    );
}

#[test]
fn a_probe_fails_instead_of_shrinking_its_input() {
    let twice = snapshot(vec![eligible(1), eligible(1)]);
    assert!(probe(&twice).unwrap_err().contains("listed twice"));
    let unnamed = snapshot(vec![json!({"provider": "codex"})]);
    assert!(probe(&unnamed).is_err());
    let foreign = json!({"schema": 2, "sample_id": "s", "boot_epoch": "b", "channels": []});
    assert!(
        probe(&foreign.to_string())
            .unwrap_err()
            .contains("schema 2")
    );
    let partial = json!({"schema": 1, "sample_id": "s", "channels": []});
    assert!(probe(&partial.to_string()).is_err());
    // A channel stated by its id alone stays in the denominator as Unknown.
    let bare = probe(&snapshot(vec![json!({"channel": 9})])).unwrap();
    assert_eq!((bare.counts.n, bare.counts.k, bare.counts.u), (1, 0, 1));
}
