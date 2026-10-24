//! The approved scope as a pure projection of the judgment's snapshot: which changes move its
//! digest, which leave it, which refuse it, and its canonical bytes.

use std::sync::Arc;

use super::*;
use crate::services::tui_o::writer::transition_cleanup::scope::{ScopeDigest, approve, encode};
use crate::services::tui_o::writer::transition_port::Context;

fn context() -> Context {
    Context {
        codex: None,
        tmux: PANE.into(),
        nonce: "nonce".into(),
        lock: Arc::default(),
    }
}

fn digest(fixture: &Fixture, context: &Context) -> ScopeDigest {
    let approved = approve(&fixture.snapshot, &fixture.request, context);
    approved
        .unwrap_or_else(|error| panic!("no scope: {error}"))
        .digest
}

fn end(fixture: &mut Fixture) -> &mut ExecutionEnd {
    match &mut fixture.request.termination {
        Some(Termination::Confirmed(end)) => end,
        other => panic!("no checked end: {other:?}"),
    }
}

fn next(fixture: &mut Fixture, n: u64) -> &mut Successor {
    fixture
        .snapshot
        .rotation
        .successors
        .get_mut(&key(n))
        .unwrap()
}

type Change = (&'static str, fn(&mut Fixture, &mut Context));

// The request without `at`, the execution and every hop's resolved identity up to the cutoff move
// the digest; time, verdict inputs, retirement, audits and hops past the cutoff leave it.
#[test]
fn transition_scope_digest_follows_the_request_execution_and_hops_only() {
    let base = digest(&Fixture::synthetic(), &context());
    let moved: [Change; 17] = [
        ("cutoff", |f, _| f.request.cutoff -= 1),
        ("job_id", |f, _| f.request.job_id.push('2')),
        ("operator", |f, _| f.request.operator.push('2')),
        ("reason", |f, _| f.request.reason.push('2')),
        ("end_host", |f, _| end(f).host.push('2')),
        ("end_pane", |f, _| end(f).pane.push('2')),
        ("end_first", |f, _| end(f).first_seq += 1),
        ("end_last", |f, _| end(f).last_seq += 1),
        ("end_basis", |f, _| end(f).basis.push('2')),
        ("pane", |_, c| c.tmux.push('2')),
        ("nonce", |_, c| c.nonce.push('2')),
        ("hop_removed", |f, _| {
            drop(f.snapshot.rotation.successors.remove(&key(5)))
        }),
        ("hop_seq", |f, _| next(f, 5).seq = Some(hop_seq(5) + 100)),
        ("hop_pane", |f, _| next(f, 5).tmux_session = None),
        ("successor", |f, _| next(f, 5).source = source(7)),
        ("legacy_seq", |f, _| {
            let legacy = f.snapshot.legacy_hops.as_mut().unwrap();
            legacy.insert(key(LEGACY[0]), vec![(hop_seq(LEGACY[0]) + 1, PANE.into())]);
        }),
        ("hop_added", |f, _| {
            f.snapshot.cursors.push(cursor(OLD + 1));
            let added = Successor::from(source(OLD + 2));
            let added = Successor {
                seq: Some(hop_seq(OLD) - 1),
                ..added
            };
            f.snapshot.rotation.successors.insert(key(OLD + 1), added);
        }),
    ];
    for (case, change) in moved {
        let (mut fixture, mut context) = (Fixture::synthetic(), context());
        change(&mut fixture, &mut context);
        assert_ne!(digest(&fixture, &context), base, "[T4:{case}] moved");
    }
    let kept: [Change; 9] = [
        ("at", |f, _| f.request.at += chrono::TimeDelta::hours(1)),
        ("retired", |f, _| f.cursor(5).retired = true),
        ("audited", |f, _| {
            let audit = match &verdict(&Fixture::synthetic().judge(), 6) {
                Verdict::Eligible(audit) => (**audit).clone(),
                other => panic!("{other:?}"),
            };
            f.cursor(6).transition_cleanup = Some(audit);
        }),
        ("observed", |f, _| f.later(5).at += Duration::from_secs(60)),
        ("grown", |f, _| f.cursor(5).captured_through += 10),
        ("obligations", |f, _| {
            f.snapshot.obligations = Err("unread".into())
        }),
        ("proven", |f, _| next(f, 5).proof = Some(9)),
        ("seq_stored", |f, _| {
            let n = LEGACY[0];
            next(f, n).seq = Some(hop_seq(n));
            next(f, n).tmux_session = Some(PANE.into());
        }),
        ("past_cutoff", |f, _| {
            f.snapshot.cursors.push(cursor(OLD + 1));
            let added = Successor {
                seq: Some(hop_seq(OLD) + 1),
                ..Successor::from(source(OLD + 2))
            };
            f.snapshot.rotation.successors.insert(key(OLD + 1), added);
        }),
    ];
    for (case, change) in kept {
        let (mut fixture, mut context) = (Fixture::synthetic(), context());
        change(&mut fixture, &mut context);
        assert_eq!(digest(&fixture, &context), base, "[T4:{case}] kept");
    }
}

// Every hop up to the cutoff is in the scope whatever its verdict, in seq order; a scope that
// is empty for a cutoff before every hop is still issued.
#[test]
fn transition_scope_holds_every_hop_up_to_the_cutoff_in_seq_order() {
    let fixture = Fixture::synthetic();
    let judged = fixture.judge();
    let refused = judged
        .hops
        .iter()
        .filter(|hop| hop.verdict != judged.hops[0].verdict);
    assert!(refused.count() > 0, "[T2:mixed_verdicts]");
    let scope = approve(&fixture.snapshot, &fixture.request, &context()).unwrap();
    let seqs: Vec<u64> = scope.hops.iter().map(|hop| hop.seq).collect();
    let all: Vec<u64> = (0..OLD).map(hop_seq).collect();
    assert_eq!(seqs, all, "[T2:every_hop]");
    let mut empty = Fixture::synthetic();
    empty.request.cutoff = 1;
    let scope = approve(&empty.snapshot, &empty.request, &context());
    assert_eq!(scope.map(|scope| scope.hops.len()), Ok(0), "[T9:empty]");
}

// A hop that cannot be placed in or out of the scope, an execution, its history or home not
// verified, or a request without a checked end issues no scope rather than a smaller one.
#[test]
fn transition_scope_is_refused_rather_than_narrowed_when_a_hop_cannot_be_placed() {
    let refused: [Change; 11] = [
        ("history_unread", |f, _| {
            f.snapshot.legacy_hops = Err("unread".into())
        }),
        ("no_history", |f, _| {
            drop(
                f.snapshot
                    .legacy_hops
                    .as_mut()
                    .unwrap()
                    .remove(&key(LEGACY[0])),
            );
        }),
        ("ambiguous", |f, _| {
            let legacy = f.snapshot.legacy_hops.as_mut().unwrap();
            let seen = (hop_seq(LEGACY[0]), PANE.to_string());
            legacy.insert(key(LEGACY[0]), vec![seen.clone(), seen]);
        }),
        ("no_cursor", |f, _| {
            f.snapshot.cursors.retain(|c| c.source != source(5))
        }),
        ("unverified", |f, _| f.snapshot.pinned = Err("nonce".into())),
        ("execution", |f, _| {
            f.snapshot.execution = Err("unread".into())
        }),
        ("home_moving", |f, _| f.snapshot.home = Home::Moving),
        ("home_unread", |f, _| {
            f.snapshot.home = Home::Unread("unread".into())
        }),
        ("no_end", |f, _| f.request.termination = None),
        ("implied", |f, _| {
            f.request.termination = Some(Termination::Implied("switch".into()))
        }),
        ("other_channel", |f, _| f.request.channel += 1),
    ];
    for (case, change) in refused {
        let (mut fixture, mut context) = (Fixture::synthetic(), context());
        change(&mut fixture, &mut context);
        let scope = approve(&fixture.snapshot, &fixture.request, &context);
        assert!(scope.is_err(), "[T9:{case}] {scope:?}");
    }
}

/// `n`'s source of the canonical bytes below.
fn small(n: u64) -> SourceId {
    SourceId {
        session_id: format!("s{n}"),
        path: format!("/t/s{n}.jsonl").into(),
        dev: 1,
        ino: 10 + n,
    }
}

/// The bytes the encoding names for the snapshot `golden_snapshot` builds, computed apart from it.
const GOLDEN: &str = concat!(
    "6167656e746465736b2f68726f742f7472616e736974696f6e2d73636f706500010000000000000007",
    "ffffffffffffffff00000000000000056a6f627c310000000000000009ec9ab4ec9881ec9e90000000",
    "000000000a6c696e650a627265616b0100000000000000086d61632d6d696e69000000000000000170",
    "000000000000000200000000000000030000000000000004676f6e6501000000000000000170000000",
    "00000000026e3000000000000000000200000000000000020100000000000000017000000000000000",
    "027330000000000000000b2f742f73302e6a736f6e6c0000000000000001000000000000000a000000",
    "00000000027331000000000000000b2f742f73312e6a736f6e6c000000000000000100000000000000",
    "0b00000000000000030100000000000000017000000000000000027331000000000000000b2f742f73",
    "312e6a736f6e6c0000000000000001000000000000000b00000000000000027332000000000000000b",
    "2f742f73322e6a736f6e6c0000000000000001000000000000000c",
);
const GOLDEN_DIGEST: &str =
    "hrot-scope-v1:d71faafd6b818456beff805819f0308894e6903916ba7026c4d28b20c8b373cf";

// One stored and one legacy hop, a separator, a newline, Korean and the widest cutoff encode to
// the bytes and digest the format fixes, and only that version and length parse back.
#[test]
fn transition_scope_encodes_to_its_golden_bytes_and_digest() {
    let mut rotation = Rotation::default();
    let stored = Successor {
        seq: Some(2),
        tmux_session: Some("p".into()),
        ..Successor::from(small(1))
    };
    rotation.successors.insert(source_key(&small(0)), stored);
    rotation
        .successors
        .insert(source_key(&small(1)), Successor::from(small(2)));
    let legacy = BTreeMap::from([(source_key(&small(1)), vec![(3, "p".to_string())])]);
    let mut snapshot = Fixture::synthetic().snapshot;
    snapshot.rotation = rotation;
    snapshot.cursors = (0..3)
        .map(|n| Cursor {
            source: small(n),
            ..cursor(n)
        })
        .collect();
    snapshot.legacy_hops = Ok(legacy);
    let end = ExecutionEnd {
        host: "mac-mini".into(),
        pane: "p".into(),
        first_seq: 2,
        last_seq: 3,
        basis: "gone".into(),
    };
    let request = Request {
        channel: CHANNEL,
        cutoff: u64::MAX,
        job_id: "job|1".into(),
        operator: "운영자".into(),
        at: chrono::Utc::now(),
        reason: "line\nbreak".into(),
        termination: Some(Termination::Confirmed(end)),
    };
    let context = Context {
        codex: None,
        tmux: "p".into(),
        nonce: "n0".into(),
        ..context()
    };
    let scope = approve(&snapshot, &request, &context).unwrap();
    let bytes = encode(&request, ShadowProvider::Claude, &context, &scope.hops);
    assert_eq!(hex::encode(bytes), GOLDEN, "[T10:bytes]");
    assert_eq!(scope.digest.to_string(), GOLDEN_DIGEST, "[T10:digest]");
    assert_eq!(
        ScopeDigest::parse(GOLDEN_DIGEST),
        Ok(scope.digest),
        "[T10:parse]"
    );
    let hex = &GOLDEN_DIGEST["hrot-scope-v1:".len()..];
    for shown in [
        format!("hrot-scope-v2:{hex}"),
        format!("HROT-SCOPE-V1:{hex}"),
        hex.to_string(),
        format!("hrot-scope-v1:{}", &hex[1..]),
        format!("hrot-scope-v1:{hex}0"),
        format!("hrot-scope-v1:{hex}00"),
    ] {
        assert!(ScopeDigest::parse(&shown).is_err(), "[T10:refused] {shown}");
    }
}
