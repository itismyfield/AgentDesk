use std::path::PathBuf;

use chrono::{DateTime, Utc};

use super::{Eligibility, InputBindingView, Lifecycle};
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};
use crate::services::tui_o::writer::adoption;
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::input_facts::{ChannelFact, TurnState};

type Named = (&'static str, &'static str);

const A: Named = ("sA", "/a");
const B: Named = ("sB", "/b");
const HOOK: &str = "UserPromptSubmit";

fn source((session, path): Named) -> SourceId {
    SourceId {
        session_id: session.into(),
        path: PathBuf::from(path),
        dev: 1,
        ino: 1,
    }
}

fn event(seq: u64, pane: &str, nonce: &str, record: BindingRecord) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: 7,
        provider: ShadowProvider::Claude,
        tmux_session: pane.into(),
        execution_nonce: nonce.into(),
        record,
        committed_at: DateTime::<Utc>::UNIX_EPOCH,
    }
}

fn bound(new: BindingTarget, old: Option<Named>, hook: &str, reclaims: bool) -> BindingRecord {
    BindingRecord::Bound {
        old: old.map(source),
        new,
        cause: BindingCause::Unknown,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event: hook.into(),
            received_at: DateTime::<Utc>::UNIX_EPOCH,
            reclaims,
        },
    }
}

/// A Pending the writer bound on `pane`; `old` is that pane's current source.
fn pend(
    seq: u64,
    pane: &str,
    (session, path): Named,
    nonce: &str,
    old: Option<Named>,
) -> BindingEvent {
    let new = BindingTarget::Pending {
        payload_session_id: session.into(),
        payload_transcript_path: PathBuf::from(path),
    };
    event(seq, pane, nonce, bound(new, old, "SessionStart", false))
}

fn src(
    seq: u64,
    of: Named,
    nonce: &str,
    old: Option<Named>,
    hook: &str,
    reclaims: bool,
) -> BindingEvent {
    let record = bound(BindingTarget::Source(source(of)), old, hook, reclaims);
    event(seq, "t1", nonce, record)
}

fn res(seq: u64, pane: &str, nonce: &str, resolves_seq: u64, of: Named) -> BindingEvent {
    let record = BindingRecord::Resolved {
        resolves_seq,
        source: source(of),
    };
    event(seq, pane, nonce, record)
}

fn rej(seq: u64) -> BindingEvent {
    let record = BindingRecord::Rejected {
        detail: "late bind".into(),
    };
    event(seq, "t1", "n1", record)
}

fn named(identity: Option<(SourceId, String)>) -> Option<(String, PathBuf)> {
    identity.map(|(source, _)| (source.session_id, source.path))
}

fn want(of: Option<Named>) -> Option<(String, PathBuf)> {
    of.map(|(session, path)| (session.to_string(), PathBuf::from(path)))
}

/// Every way to cut `n` records into consecutive wakes.
fn splits(n: usize) -> Vec<Vec<std::ops::Range<usize>>> {
    (0..1u32 << (n - 1))
        .map(|cuts| {
            let mut wakes = Vec::new();
            let mut start = 0;
            for at in 1..=n {
                if at == n || cuts & (1 << (at - 1)) != 0 {
                    wakes.push(start..at);
                    start = at;
                }
            }
            wakes
        })
        .collect()
}

#[test]
fn input_view_reaches_each_boundary_value_under_every_wake_split() {
    use Eligibility::{Eligible, EmptyNonce, Pending, Rejected};
    let n1 = "n1";
    // Expected values are the reference model's outputs for the same logs, not this fold's.
    let cases: Vec<(&str, Vec<BindingEvent>, Option<Named>, Eligibility)> = vec![
        (
            "a later Pending supersedes the first; its Resolved binds",
            vec![
                pend(1, "t1", A, n1, None),
                pend(2, "t1", B, n1, None),
                res(3, "t1", n1, 2, B),
            ],
            Some(B),
            Eligible,
        ),
        (
            "a repeated Pending of one session supersedes the first",
            vec![
                pend(1, "t1", A, n1, None),
                pend(2, "t1", A, n1, None),
                res(3, "t1", n1, 2, A),
            ],
            Some(A),
            Eligible,
        ),
        (
            "a late Resolved of a superseded Pending leaves the later one waiting",
            vec![
                pend(1, "t1", A, n1, None),
                pend(2, "t1", B, n1, None),
                res(3, "t1", n1, 1, A),
            ],
            None,
            Pending,
        ),
        (
            "a reclaiming Bound drops the waiting Pending",
            vec![
                src(1, A, n1, None, HOOK, false),
                pend(2, "t1", B, n1, Some(A)),
                src(3, A, n1, Some(A), HOOK, true),
            ],
            Some(A),
            Eligible,
        ),
        (
            "a Bound without a hook supersedes nothing",
            vec![
                src(1, A, n1, None, HOOK, false),
                pend(2, "t1", B, n1, Some(A)),
                src(3, A, n1, Some(A), "", false),
            ],
            None,
            Pending,
        ),
        (
            "a moved Bound drops the waiting Pending",
            vec![
                src(1, A, n1, None, HOOK, false),
                pend(2, "t1", A, n1, Some(A)),
                src(3, B, n1, Some(A), HOOK, false),
            ],
            Some(B),
            Eligible,
        ),
        (
            "session mismatch",
            vec![
                pend(1, "t1", A, n1, None),
                res(2, "t1", n1, 1, ("sB", "/a")),
            ],
            Some(("sB", "/a")),
            Rejected,
        ),
        (
            "path mismatch",
            vec![
                pend(1, "t1", A, n1, None),
                res(2, "t1", n1, 1, ("sA", "/b")),
            ],
            Some(("sA", "/b")),
            Rejected,
        ),
        (
            "nonce mismatch",
            vec![pend(1, "t1", A, n1, None), res(2, "t1", "n2", 1, A)],
            Some(A),
            Rejected,
        ),
        (
            "pane mismatch",
            vec![pend(1, "t2", A, n1, None), res(2, "t1", n1, 1, A)],
            Some(A),
            Rejected,
        ),
        (
            "a Resolved with nothing waiting",
            vec![res(1, "t1", n1, 999, A)],
            Some(A),
            Rejected,
        ),
        (
            "a second Resolved of an already resolved Pending",
            vec![
                pend(1, "t1", A, n1, None),
                res(2, "t1", n1, 1, A),
                res(3, "t1", n1, 1, A),
            ],
            Some(A),
            Rejected,
        ),
        (
            "a session anchor alone suffices without a path",
            vec![pend(1, "t1", ("sA", ""), n1, None), res(2, "t1", n1, 1, A)],
            Some(A),
            Eligible,
        ),
        (
            "an empty Pending nonce never matches",
            vec![pend(1, "t1", A, "", None), res(2, "t1", "", 1, A)],
            Some(A),
            Rejected,
        ),
        (
            "an empty Bound nonce",
            vec![src(1, A, "", None, HOOK, false)],
            Some(A),
            EmptyNonce,
        ),
        (
            "Rejected holds the source O still names",
            vec![src(1, A, n1, None, HOOK, false), rej(2)],
            Some(A),
            Rejected,
        ),
        (
            "the same source bound again recovers",
            vec![
                src(1, A, n1, None, HOOK, false),
                rej(2),
                src(3, A, n1, Some(A), HOOK, false),
            ],
            Some(A),
            Eligible,
        ),
        (
            "another source bound recovers",
            vec![
                src(1, A, n1, None, HOOK, false),
                rej(2),
                src(3, B, n1, Some(A), HOOK, false),
            ],
            Some(B),
            Eligible,
        ),
    ];
    for (label, events, identity, eligibility) in cases {
        let key = (eligibility == Eligible).then(|| (want(identity).unwrap(), n1.to_string()));
        let whole = InputBindingView::of(&events);
        assert_eq!(named(whole.identity()), want(identity), "{label}");
        assert_eq!(whole.identity(), adoption::current(&events), "{label}");
        for wakes in splits(events.len()) {
            let mut life = Lifecycle::<()>::default();
            for wake in &wakes {
                life.apply(events[wake.clone()].to_vec(), |_, _| ());
            }
            let view = life.view();
            assert_eq!(named(view.identity()), want(identity), "{label} {wakes:?}");
            assert_eq!(view.eligibility(), eligibility, "{label} {wakes:?}");
            let got = view.key().map(|key| {
                let (session, path) = (key.source.session_id, key.source.path);
                ((session, path), key.execution_nonce)
            });
            assert_eq!(got, key, "{label} {wakes:?}");
            assert_eq!(view.eligibility(), whole.eligibility(), "{label} {wakes:?}");
        }
    }
}

enum Seen {
    Record(Box<BindingEvent>),
    Idle,
}

/// Runs the drive's wake loop: records settle the instance one by one, Idle reaches only the
/// instance that exists when it is polled, and one offer per wake submits the single row.
fn drive(seen: &[Seen], wakes: &[std::ops::Range<usize>]) -> (Vec<usize>, Vec<usize>) {
    let mut life = Lifecycle::<()>::default();
    let (mut built, mut submitted) = (Vec::new(), Vec::new());
    let mut settle = |life: &mut Lifecycle<()>, batch: &mut Vec<BindingEvent>| {
        let at_seq = |seq| {
            seen.iter()
                .position(|s| matches!(s, Seen::Record(e) if e.seq == seq))
        };
        life.apply(std::mem::take(batch), |_, seq| {
            built.push(at_seq(seq).unwrap())
        });
    };
    for wake in wakes {
        let mut batch = Vec::new();
        for at in wake.clone() {
            match &seen[at] {
                Seen::Record(event) => batch.push((**event).clone()),
                Seen::Idle => {
                    settle(&mut life, &mut batch);
                    if let Some(instance) = life.instance() {
                        let binding = SourceBinding {
                            channel_id: 7,
                            provider: ShadowProvider::Claude,
                            source: instance.key.source.clone(),
                        };
                        let state = TurnState::Idle;
                        instance.polled(ChannelFact {
                            binding,
                            through: 0,
                            state,
                        });
                    }
                }
            }
        }
        settle(&mut life, &mut batch);
        let ready = life.instance().is_some_and(|instance| {
            (instance.fact()).is_some_and(|fact| fact.state == TurnState::Idle)
        });
        if ready && submitted.is_empty() {
            submitted.push(wake.end - 1);
        }
    }
    (built, submitted)
}

#[test]
fn a_record_inside_a_wake_ends_the_instance_and_only_its_successors_idle_submits() {
    let seen = [
        Seen::Record(Box::new(src(1, A, "n1", None, HOOK, false))),
        Seen::Idle,
        Seen::Record(Box::new(rej(2))),
        Seen::Record(Box::new(src(3, A, "n1", Some(A), HOOK, false))),
        Seen::Idle,
    ];
    // Idle 1 shares a wake with Rejected, so no offer can use it before the binding ends.
    let split = [0..1, 1..3, 3..4, 4..5];
    let merged = [0..1, 1..4, 4..5];
    for wakes in [&split[..], &merged[..]] {
        let (built, submitted) = drive(&seen, wakes);
        assert_eq!(built, [0, 3], "{wakes:?}");
        assert_eq!(submitted, [4], "{wakes:?}");
    }
}
