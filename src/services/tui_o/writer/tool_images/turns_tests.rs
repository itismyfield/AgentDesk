//! Image turn tracking, span restore, Closed boundaries and the opener GC rule.

use std::path::PathBuf;

use chrono::Utc;

use serde_json::{Value, json};

use super::turns::{ImageTurns, TurnEdge};
use super::{NativeTurn, image_native_key};
use crate::services::tui_o::shadow::identity::{RecordFact, classify};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::tool_images::{
    ImageEntry, ImageObligation, ImageStep, OpenObligation, Waiting,
};
use crate::services::tui_o::store::ledger::{LedgerEntry, LedgerState};
use crate::services::tui_o::store::spool::source_key;

fn source() -> SourceId {
    SourceId {
        session_id: "session".into(),
        path: PathBuf::from("/tmp/i8397-turns.jsonl"),
        dev: 3,
        ino: 4,
    }
}

fn turn(id: &str) -> NativeTurn {
    NativeTurn::new(id)
}

/// A ledger holding the given span entries, as replay would rebuild it.
fn spans(entries: Vec<ImageEntry>) -> LedgerState {
    let mut state = LedgerState::default();
    for entry in entries {
        let entry = LedgerEntry::ToolImage {
            entry: Box::new(entry),
        };
        state.apply(Utc::now(), entry);
    }
    assert_eq!(state.violation(), None);
    state
}

fn opened(id: &str, opener_offset: u64) -> ImageEntry {
    ImageEntry::ImageTurnOpened {
        source: source(),
        opener_offset,
        turn: id.into(),
    }
}

fn closed(id: &str, end_offset: u64) -> ImageEntry {
    ImageEntry::ImageTurnClosed {
        source: source(),
        turn: id.into(),
        end_offset,
    }
}

fn prompt() -> RecordFact {
    RecordFact::Prompt(true, "read the image".into())
}

fn closed_edge(id: &str, end_offset: u64, pending: &[&str]) -> Option<TurnEdge> {
    let pending_calls = pending.iter().map(|call| (*call).to_owned()).collect();
    Some(TurnEdge::Closed {
        turn: turn(id),
        end_offset,
        pending_calls,
    })
}

#[test]
fn closed_span_end_offset_applied_before_prompt() {
    // u1 ran 100..300 and ended by a verified physical end; its opener segment is gone.
    let ledger = spans(vec![opened("u1", 100), closed("u1", 300)]);
    let images = ledger.tool_images();
    let mut turns = ImageTurns::starting_at(source(), 150, 0);
    // Row preparation restores u1 from its span before any fact of the row.
    turns.before_offset(200, images);
    assert_eq!(turns.turn_for_image("call-b", 200, images).turn, turn("u1"));
    // The next record is u2's prompt at the Closed boundary: the boundary applies first.
    turns.before_offset(300, images);
    assert_eq!(turns.observe(&prompt(), Some("u2"), 300), None);
    turns.before_offset(350, images);
    let next = turns.turn_for_image("call-c", 350, images);
    assert_eq!(next.turn, turn("u2"));
    let opener_offset = 300;
    let open_span = Some(TurnEdge::Opened {
        turn: turn("u2"),
        opener_offset,
    });
    assert_eq!(next.open_span, open_span);
}

#[test]
fn replay_mid_turn_prompt_after_opener_gc_keeps_durable_turn() {
    // u1 ran 100..500 with a prompt at 250 inside it; replay starts at that prompt.
    let ledger = spans(vec![opened("u1", 100), closed("u1", 500)]);
    let images = ledger.tool_images();
    let mut turns = ImageTurns::starting_at(source(), 250, 0);
    turns.before_offset(250, images);
    assert_eq!(turns.observe(&prompt(), Some("mid"), 250), None);
    turns.before_offset(300, images);
    let image = turns.turn_for_image("call-1", 300, images);
    assert_eq!(image.turn, turn("u1"));
    assert_eq!(image.open_span, None);
    assert_eq!(ledger.violation(), None);
}

#[test]
fn open_span_keeps_opener_until_closed() {
    let ledger = spans(vec![opened("u1", 100)]);
    let turns = ImageTurns::starting_at(source(), 0, 0);
    assert!(!turns.opener_collectable(ledger.tool_images(), &turn("u1")));
    // A live open turn without any span keeps its opener for the physical-end projection.
    let mut live = ImageTurns::starting_at(source(), 0, 0);
    live.observe(&prompt(), Some("u3"), 500);
    assert!(!live.opener_collectable(ledger.tool_images(), &turn("u3")));
    let edge = live.observe(&RecordFact::Idle(None), None, 900);
    assert_eq!(edge, closed_edge("u3", 900, &[]));
    assert!(live.opener_collectable(ledger.tool_images(), &turn("u3")));
}

#[test]
fn closed_span_releases_opener() {
    let ledger = spans(vec![opened("u1", 100), closed("u1", 300)]);
    let mut turns = ImageTurns::starting_at(source(), 150, 0);
    assert!(turns.opener_collectable(ledger.tool_images(), &turn("u1")));
    // Restored from its closed span, the turn still does not pin a collected opener.
    turns.before_offset(200, ledger.tool_images());
    assert!(turns.opener_collectable(ledger.tool_images(), &turn("u1")));
}

#[test]
fn no_span_falls_to_edge_turn_never_body_key() {
    let ledger = LedgerState::default();
    let mut turns = ImageTurns::starting_at(source(), 500, 42);
    turns.before_offset(600, ledger.tool_images());
    let image = turns.turn_for_image("call-1", 600, ledger.tool_images());
    let edge = format!("edge:{}:42", source_key(&source()));
    assert_eq!(image.turn, turn(&edge));
    assert_eq!(image.open_span, None);
    // A body in between never becomes the turn.
    turns.observe(
        &RecordFact::Unit(
            "msg-1:0".into(),
            UnitKind::Body,
            crate::services::tui_o::shadow::identity::UnitContent::Payload("text".into()),
        ),
        Some("row-1"),
        700,
    );
    let again = turns.turn_for_image("call-2", 800, ledger.tool_images());
    assert_eq!(again.turn, turn(&edge));
}

#[test]
fn physical_end_closes_tracker_and_keeps_call_turns() {
    let ledger = LedgerState::default();
    let images = ledger.tool_images();
    let mut turns = ImageTurns::starting_at(source(), 0, 0);
    turns.observe(&prompt(), Some("u1"), 0);
    turns.note_call("call-1");
    // The call has no result yet, so the end reports it for a durable attribution.
    assert_eq!(turns.physical_end(50), closed_edge("u1", 50, &["call-1"]));
    // Without the end, this prompt would be ignored inside the open u1.
    turns.observe(&prompt(), Some("u2"), 60);
    assert_eq!(turns.turn_for_image("call-1", 70, images).turn, turn("u1"));
    assert_eq!(turns.turn_for_image("call-2", 80, images).turn, turn("u2"));
}

#[test]
fn prompt_inside_open_turn_never_reopens() {
    let ledger = LedgerState::default();
    let mut turns = ImageTurns::starting_at(source(), 0, 0);
    turns.observe(&prompt(), Some("u1"), 0);
    turns.observe(&prompt(), Some("u2"), 40);
    let image = turns.turn_for_image("call-1", 50, ledger.tool_images());
    assert_eq!(image.turn, turn("u1"));
}

fn apply_image(ledger: &mut LedgerState, entry: ImageEntry) {
    let entry = LedgerEntry::ToolImage {
        entry: Box::new(entry),
    };
    ledger.apply(Utc::now(), entry);
}

/// Records the image obligation the wiring would append; returns its unit key.
fn owe(
    ledger: &mut LedgerState,
    turn: &NativeTurn,
    call_id: &str,
    sha256: &str,
    at: u64,
) -> UnitKey {
    let unit_key = UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: image_native_key(turn, sha256),
        kind: UnitKind::ToolImage,
    };
    let obligation = ImageObligation {
        unit_key: unit_key.clone(),
        turn: turn.as_str().into(),
        call_id: call_id.into(),
        image_index: 0,
        source: source(),
        record_start: at,
        record_end: at + 10,
        sha256: Some(sha256.into()),
        media_type: Some("image/png".into()),
        size: Some(8),
        display_name: "image.png".into(),
        marker: format!("m-{sha256}"),
    };
    apply_image(ledger, ImageEntry::ImageObligation(Box::new(obligation)));
    unit_key
}

/// One attempt of `key`: posted, or left Unresolved; returns its serial.
fn attempt(ledger: &mut LedgerState, key: &UnitKey, msg_id: Option<u64>) -> u64 {
    let serial = ledger.next_serial();
    let prepared = LedgerEntry::Prepared {
        serial,
        unit_key: key.clone(),
        piece_index: 0,
        payload: format!("caption {serial}"),
        anchor_id: ledger.anchor(),
        epoch: 1,
        local_retry: None,
        notice_origin: None,
    };
    ledger.apply(Utc::now(), prepared);
    let result = match msg_id {
        Some(msg_id) => LedgerEntry::Posted {
            serial,
            msg_id,
            confirmation: None,
        },
        None => LedgerEntry::Unresolved {
            serial,
            reason: "history unreadable".into(),
        },
    };
    ledger.apply(Utc::now(), result);
    serial
}

fn late_image_after_gc(late_posted: bool) {
    let mut ledger = LedgerState::starting_at(1 << 22);
    let mut live = ImageTurns::starting_at(source(), 0, 0);
    live.before_offset(100, ledger.tool_images());
    live.observe(&prompt(), Some("u1"), 100);
    live.note_call("c0");
    live.note_call("c1");
    live.before_offset(200, ledger.tool_images());
    live.note_result("c0");
    let first = live.turn_for_image("c0", 200, ledger.tool_images());
    let Some(TurnEdge::Opened {
        turn,
        opener_offset,
    }) = first.open_span
    else {
        panic!("u1's first image opens its span");
    };
    let turn = turn.as_str().to_owned();
    apply_image(&mut ledger, opened(&turn, opener_offset));
    let early = owe(&mut ledger, &first.turn, "c0", "bb", 200);
    attempt(&mut ledger, &early, Some(2 << 22));
    // u1 ends while c1's result is still out; the end reports c1 for its durable attribution.
    live.before_offset(300, ledger.tool_images());
    let edge = live.observe(&RecordFact::Idle(None), None, 300);
    assert_eq!(edge, closed_edge("u1", 300, &["c1"]));
    let call_id = "c1".to_owned();
    let source = source();
    apply_image(
        &mut ledger,
        ImageEntry::ImageCall {
            source,
            call_id,
            turn,
        },
    );
    apply_image(&mut ledger, closed("u1", 300));
    // c1's image arrives after the Closed boundary.
    live.before_offset(350, ledger.tool_images());
    live.note_result("c1");
    let late = live.turn_for_image("c1", 350, ledger.tool_images());
    assert_eq!(
        (late.turn.clone(), late.open_span.clone()),
        (self::turn("u1"), None)
    );
    let key = owe(&mut ledger, &late.turn, "c1", "hh", 350);
    let serial = attempt(&mut ledger, &key, late_posted.then_some(3 << 22));
    assert_eq!(ledger.violation(), None);
    // Everything before 350 is collected; a new writer replays only the late result.
    let mut replay = ImageTurns::starting_at(self::source(), 350, 0);
    replay.before_offset(350, ledger.tool_images());
    let again = replay.turn_for_image("c1", 350, ledger.tool_images());
    assert_eq!(image_native_key(&again.turn, "hh"), key.native_key);
    let steps = ledger.tool_images().open_obligations(&ledger);
    let expected = match late_posted {
        true => Vec::new(),
        false => vec![OpenObligation::Image {
            key,
            step: ImageStep::Waiting(Waiting::Unresolved { serial }),
        }],
    };
    assert_eq!(steps, expected, "no new obligation and no new POST");
}

#[test]
fn late_image_call_turn_survives_closed_opener_gc_and_restart() {
    late_image_after_gc(true);
    late_image_after_gc(false);
}

fn codex_event(kind: &str, turn_id: &str) -> Value {
    json!({"type": "event_msg", "payload": {"type": kind, "turn_id": turn_id}})
}

/// Classifies one raw Codex row and feeds every fact to the tracker.
fn observe_row(turns: &mut ImageTurns, row: &Value, offset: u64) -> Vec<TurnEdge> {
    let facts = classify(ShadowProvider::Codex, row);
    assert!(!facts.is_empty(), "{row}");
    let edges = facts
        .iter()
        .filter_map(|fact| turns.observe(fact, None, offset));
    edges.collect()
}

#[test]
fn foreign_idle_keeps_parent_image_turn() {
    let ledger = LedgerState::default();
    let images = ledger.tool_images();
    let mut turns = ImageTurns::starting_at(source(), 0, 0);
    let started = observe_row(&mut turns, &codex_event("task_started", "parent"), 0);
    assert!(started.is_empty());
    turns.note_call("c1");
    let first = turns.turn_for_image("c1", 10, images);
    assert_eq!(first.turn, turn("parent"));
    // A child's or a foreign turn's named end leaves the parent running.
    for (offset, closer) in [(20, "child"), (30, "foreign")] {
        let edges = observe_row(&mut turns, &codex_event("turn_aborted", closer), offset);
        assert!(edges.is_empty(), "{closer}");
    }
    assert!(!turns.opener_collectable(images, &turn("parent")));
    turns.note_call("c2");
    let second = turns.turn_for_image("c2", 40, images);
    let sha = "ab";
    assert_eq!(
        image_native_key(&second.turn, sha),
        image_native_key(&first.turn, sha)
    );
    // The parent's own end closes it exactly once.
    let edges = observe_row(&mut turns, &codex_event("task_complete", "parent"), 50);
    let closed = closed_edge("parent", 50, &["c1", "c2"]);
    assert_eq!(edges.into_iter().next(), closed);
    let again = observe_row(&mut turns, &codex_event("task_complete", "parent"), 60);
    assert!(again.is_empty());
}
