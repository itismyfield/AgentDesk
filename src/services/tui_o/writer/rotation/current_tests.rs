use super::Current;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::renumbered::{Logged, map_sources};
use chrono::Utc;

fn source(session: &str, slot: u64) -> SourceId {
    SourceId {
        session_id: session.into(),
        path: format!("/fixture/current-{slot}.jsonl").into(),
        dev: 1,
        ino: slot,
    }
}

fn event(seq: u64, pane: &str, record: BindingRecord) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: 7,
        provider: ShadowProvider::Claude,
        tmux_session: pane.into(),
        execution_nonce: "execution".into(),
        record,
        committed_at: Utc::now(),
    }
}

fn bind(seq: u64, pane: &str, old: Option<&SourceId>, target: BindingTarget) -> BindingEvent {
    event(
        seq,
        pane,
        BindingRecord::Bound {
            old: old.cloned(),
            new: target,
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "SessionStart".into(),
                received_at: Utc::now(),
                reclaims: false,
            },
        },
    )
}

fn pending(seq: u64, pane: &str, old: &SourceId) -> BindingEvent {
    bind(
        seq,
        pane,
        Some(old),
        BindingTarget::Pending {
            payload_session_id: "waiting".into(),
            payload_transcript_path: "/fixture/waiting.jsonl".into(),
        },
    )
}

fn named(raw: Vec<BindingEvent>) -> Logged {
    let mut named = raw.clone();
    for event in &mut named {
        map_sources(event, |source| {
            let mut source = source.clone();
            source.session_id.clear();
            source.dev = 9;
            source
        });
    }
    (raw, named)
}

fn stored(source: &SourceId) -> SourceId {
    let mut source = source.clone();
    source.session_id.clear();
    source.dev = 9;
    source
}

fn known(logged: &Logged, checkpoint: u64, expected: &SourceId) {
    let current = Current::of(Some(logged), Some(checkpoint));
    assert!(
        !current.historical(expected),
        "the named current must never be historical"
    );
    assert!(
        matches!(current, Current::Known(ref source) if source == expected),
        "current must compare stored names"
    );
}

#[test]
fn current_uses_raw_supersede_and_compares_named_sources() {
    let a = source("old-session", 1);
    let b = source("fresh-session", 2);
    let logged = named(vec![
        bind(1, "main", None, BindingTarget::Source(a.clone())),
        pending(2, "main", &a),
        bind(3, "main", Some(&a), BindingTarget::Source(b.clone())),
    ]);
    // Raw session movement supersedes Pending; the stored empty sessions do not show it.
    known(&logged, 1, &stored(&b));
    assert!(Current::of(Some(&logged), Some(1)).historical(&stored(&a)));
}

#[test]
fn current_skips_only_pending_closed_by_explicit_rejection() {
    let a = source("a", 1);
    let c = source("c", 3);
    for closes in [Some(2), None] {
        let logged = named(vec![
            bind(1, "main", None, BindingTarget::Source(a.clone())),
            pending(2, "waiting-pane", &a),
            event(
                3,
                "waiting-pane",
                BindingRecord::Rejected {
                    detail: "refused".into(),
                    closes,
                },
            ),
            bind(4, "other-pane", None, BindingTarget::Source(c.clone())),
        ]);
        if closes.is_some() {
            known(&logged, 1, &stored(&c));
        } else {
            assert!(matches!(
                Current::of(Some(&logged), Some(1)),
                Current::Unknown
            ));
        }
    }
}

#[test]
fn late_resolution_fills_its_bound_without_becoming_the_latest_bind() {
    let a = source("a", 1);
    let b = source("b", 2);
    let c = source("c", 3);
    let logged = named(vec![
        bind(1, "main", None, BindingTarget::Source(a.clone())),
        pending(2, "waiting-pane", &a),
        bind(3, "other-pane", None, BindingTarget::Source(b.clone())),
        event(
            4,
            "waiting-pane",
            BindingRecord::Resolved {
                resolves_seq: 2,
                source: c.clone(),
            },
        ),
    ]);
    known(&logged, 1, &stored(&b));
    assert!(Current::of(Some(&logged), Some(1)).historical(&stored(&c)));
}

#[test]
fn pending_in_applied_prefix_uses_resolution_from_the_whole_log() {
    let a = source("a", 1);
    let c = source("c", 3);
    let logged = named(vec![
        bind(1, "main", None, BindingTarget::Source(a.clone())),
        pending(2, "waiting-pane", &a),
        event(
            3,
            "waiting-pane",
            BindingRecord::Resolved {
                resolves_seq: 2,
                source: c.clone(),
            },
        ),
    ]);
    known(&logged, 2, &stored(&c));
}

#[test]
fn unresolved_applied_prefix_is_skipped_but_unapplied_suffix_is_unknown() {
    let a = source("a", 1);
    let b = source("b", 2);
    let logged = named(vec![
        bind(1, "main", None, BindingTarget::Source(a.clone())),
        pending(2, "waiting-pane", &a),
        bind(3, "other-pane", None, BindingTarget::Source(b.clone())),
    ]);
    known(&logged, 2, &stored(&b));
    assert!(matches!(
        Current::of(Some(&logged), Some(1)),
        Current::Unknown
    ));
}
