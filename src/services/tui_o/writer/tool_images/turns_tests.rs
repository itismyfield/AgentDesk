//! Image turn tracking, span restore, Closed boundaries and the opener GC rule.

use std::path::PathBuf;

use chrono::Utc;

use super::NativeTurn;
use super::turns::{ImageTurns, TurnEdge};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::identity::RecordFact;
use crate::services::tui_o::store::ledger::tool_images::ImageEntry;
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
        turn: turn(id),
    }
}

fn closed(id: &str, end_offset: u64) -> ImageEntry {
    ImageEntry::ImageTurnClosed {
        source: source(),
        turn: turn(id),
        end_offset,
    }
}

fn prompt() -> RecordFact {
    RecordFact::Prompt(true, "read the image".into())
}

#[test]
fn closed_span_end_offset_applied_before_prompt() {
    // u1 ran 100..300 and ended by a verified physical end; its opener segment is gone.
    let ledger = spans(vec![opened("u1", 100), closed("u1", 300)]);
    let images = ledger.tool_images();
    let mut turns = ImageTurns::starting_at(source(), 150, 0);
    // A retained duplicate of u1 restores u1 from its span.
    assert_eq!(turns.turn_for_image("call-b", 200, images).turn, turn("u1"));
    // The next record is u2's prompt at the Closed boundary: the boundary applies first.
    turns.before_offset(300);
    assert_eq!(turns.observe(&prompt(), Some("u2"), 300), None);
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
fn open_span_keeps_opener_until_closed() {
    let ledger = spans(vec![opened("u1", 100)]);
    let turns = ImageTurns::starting_at(source(), 0, 0);
    assert!(!turns.opener_collectable(ledger.tool_images(), &turn("u1")));
    // A live open turn without any span keeps its opener for the physical-end projection.
    let mut live = ImageTurns::starting_at(source(), 0, 0);
    live.observe(&prompt(), Some("u3"), 500);
    assert!(!live.opener_collectable(ledger.tool_images(), &turn("u3")));
    let edge = live.observe(&RecordFact::Idle(None), None, 900);
    let end_offset = 900;
    let closed_edge = Some(TurnEdge::Closed {
        turn: turn("u3"),
        end_offset,
    });
    assert_eq!(edge, closed_edge);
    assert!(live.opener_collectable(ledger.tool_images(), &turn("u3")));
}

#[test]
fn closed_span_releases_opener() {
    let ledger = spans(vec![opened("u1", 100), closed("u1", 300)]);
    let mut turns = ImageTurns::starting_at(source(), 150, 0);
    assert!(turns.opener_collectable(ledger.tool_images(), &turn("u1")));
    // Restored from its closed span, the turn still does not pin a collected opener.
    turns.turn_for_image("call-b", 200, ledger.tool_images());
    assert!(turns.opener_collectable(ledger.tool_images(), &turn("u1")));
}

#[test]
fn no_span_falls_to_edge_turn_never_body_key() {
    let ledger = LedgerState::default();
    let mut turns = ImageTurns::starting_at(source(), 500, 42);
    let image = turns.turn_for_image("call-1", 600, ledger.tool_images());
    let edge = format!("edge:{}:42", source_key(&source()));
    assert_eq!(image.turn, turn(&edge));
    assert_eq!(image.open_span, None);
    // A body in between never becomes the turn.
    turns.observe(
        &RecordFact::Unit(
            "msg-1:0".into(),
            crate::services::tui_o::shadow::UnitKind::Body,
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
    let end_offset = 50;
    let edge = Some(TurnEdge::Closed {
        turn: turn("u1"),
        end_offset,
    });
    assert_eq!(turns.physical_end(50), edge);
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
