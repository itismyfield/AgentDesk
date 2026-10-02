use super::*;
use ReportOrder::{
    Accepted, AwaitingRecheck, Diverged, OldConnection, SourceChanged, Stale, Unordered,
};
use ReportSeq::{Absent, Unverified, Verified};

fn scope(pane_id: &str) -> ReportScope {
    ReportScope {
        endpoint: "herdr.default".into(),
        pane_id: pane_id.into(),
        execution_nonce: "n1".into(),
    }
}

// Reports reordered, repeated, re-sourced or read across a reconnect never move a scope
// backwards; only a larger verified seq of the followed source on the current connection does.
#[test]
fn herdr_report_order_accepts_only_a_newer_verified_seq_of_the_followed_source() {
    let mut filter = ReportOrderFilter::default();
    let (a, b) = (scope("w1-1"), scope("w1-2"));
    let mut step = |generation, scope: &ReportScope, source, seq, payload: &str, order, name| {
        let got = filter.observe(generation, scope, source, seq, &payload);
        assert_eq!(got, order, "{name}");
    };
    step(
        1,
        &a,
        "claude",
        Verified(10),
        "working/a",
        Accepted,
        "first",
    );
    step(1, &a, "claude", Verified(12), "idle/a", Accepted, "newer");
    step(1, &a, "claude", Verified(11), "working/a", Stale, "late");
    step(1, &a, "claude", Verified(12), "idle/a", Stale, "repeat");
    step(
        1,
        &a,
        "claude",
        Verified(12),
        "blocked/a",
        Diverged,
        "same seq, other payload",
    );
    step(1, &a, "claude", Absent, "absent", Unordered, "no seq");
    step(
        1,
        &a,
        "claude",
        Unverified,
        "idle/a",
        Unordered,
        "unverified seq",
    );
    step(
        1,
        &a,
        "claude",
        Verified(11),
        "idle/a",
        Stale,
        "no reset by a seqless read",
    );
    step(
        1,
        &a,
        "claude",
        Verified(11),
        "idle/b",
        Stale,
        "new agent session, older seq",
    );
    step(
        1,
        &a,
        "claude",
        Verified(13),
        "idle/b",
        Accepted,
        "new agent session, newer",
    );
    step(
        1,
        &b,
        "claude",
        Verified(1),
        "idle",
        Accepted,
        "another pane has its own order",
    );
    step(
        1,
        &a,
        "herdr:x",
        Verified(99),
        "idle",
        SourceChanged,
        "another source",
    );
    step(
        1,
        &a,
        "claude",
        Verified(14),
        "idle/b",
        AwaitingRecheck,
        "the left source before the pane is read again",
    );
    step(
        2,
        &a,
        "claude",
        Verified(5),
        "idle/b",
        Accepted,
        "new connection drops the cache",
    );
    step(
        1,
        &a,
        "claude",
        Verified(20),
        "idle/b",
        OldConnection,
        "old connection's reply",
    );
    step(
        2,
        &a,
        "claude",
        Verified(4),
        "idle/b",
        Stale,
        "ordered on the new connection",
    );
}

// A switch to another source keeps the left source's order: its late report stays stale and an
// accepted seq is never replaced by another payload under the same seq.
#[test]
fn herdr_report_order_keeps_the_left_source_order_after_a_switch() {
    let mut filter = ReportOrderFilter::default();
    let a = scope("w1-1");
    let mut step = |source, seq, payload: &str, order, name| {
        assert_eq!(
            filter.observe(1, &a, source, seq, &payload),
            order,
            "{name}"
        );
    };
    step("claude", Verified(10), "working", Accepted, "first");
    step("claude", Verified(12), "idle", Accepted, "newer");
    step("herdr:x", Verified(99), "idle", SourceChanged, "switch");
    step(
        "claude",
        Verified(11),
        "working",
        Stale,
        "late report of the left source",
    );
    step(
        "claude",
        Verified(12),
        "blocked",
        Diverged,
        "same seq, other payload",
    );
    step(
        "claude",
        Verified(12),
        "idle",
        Stale,
        "the accepted payload stays",
    );
}

// After a switch no report re-establishes a source by itself; only a fresh read of the pane on
// the current connection names the source that is followed next.
#[test]
fn herdr_report_order_follows_only_the_source_a_fresh_read_names_after_a_switch() {
    let mut filter = ReportOrderFilter::default();
    let a = scope("w1-1");
    assert_eq!(
        filter.observe(2, &a, "claude", Verified(5), &"idle"),
        Accepted
    );
    assert_eq!(
        filter.observe(2, &a, "herdr:x", Verified(1), &"idle"),
        SourceChanged
    );
    assert_eq!(filter.observe(2, &a, "claude", Verified(4), &"idle"), Stale);
    for (source, seq) in [("herdr:x", 2), ("claude", 9), ("herdr:x", 50)] {
        let got = filter.observe(2, &a, source, Verified(seq), &"idle");
        assert_eq!(got, AwaitingRecheck, "{source} {seq} before the fresh read");
    }
    assert!(
        !filter.refollow(1, &a, "claude"),
        "an older connection's read"
    );
    assert_eq!(
        filter.observe(2, &a, "claude", Verified(9), &"idle"),
        AwaitingRecheck
    );
    assert!(filter.refollow(2, &a, "herdr:x"));
    assert_eq!(
        filter.observe(2, &a, "claude", Verified(10), &"idle"),
        SourceChanged
    );
    assert!(filter.refollow(2, &a, "herdr:x"));
    assert_eq!(
        filter.observe(2, &a, "herdr:x", Verified(3), &"idle"),
        Accepted
    );
    assert_eq!(
        filter.observe(2, &a, "herdr:x", Verified(2), &"idle"),
        Stale
    );
}
