use super::*;
use ReportOrder::{Accepted, Diverged, OldConnection, SourceChanged, Stale, Unordered};
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
        Accepted,
        "read again after the switch",
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
