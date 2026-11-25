//! #8397 S2a: tool-image ledger entries, open obligations and the late-confirmation judgment.

use std::path::PathBuf;

use chrono::Utc;

use super::tool_images::{
    ImageEntry, ImageObligation, ImageStep, LateConfirm, NoticeStep, OpenObligation, Waiting,
    late_confirm, notice_native_key,
};
use super::{LedgerEntry, LedgerState};
use crate::services::discord::outbound::notification_policy::{NoticeClass, unit_class};
use crate::services::tui_o::repost::o_piece_delivery::{PieceKey, kind_name};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, UnitKey, UnitKind};
use crate::services::tui_o::writer::tool_images::{NativeTurn, OmitReason, image_native_key};

/// Discord snowflake for `ms` milliseconds after the Discord epoch.
fn snowflake(ms: u64) -> u64 {
    ms << 22
}

fn source() -> SourceId {
    SourceId {
        session_id: "session".into(),
        path: PathBuf::from("/tmp/i8397-s2a.jsonl"),
        dev: 1,
        ino: 2,
    }
}

fn unit(native_key: &str, kind: UnitKind) -> UnitKey {
    UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: native_key.into(),
        kind,
    }
}

fn image(turn: &str, sha256: &str) -> UnitKey {
    unit(
        &image_native_key(&NativeTurn::new(turn), sha256),
        UnitKind::ToolImage,
    )
}

fn image_entry(entry: ImageEntry) -> LedgerEntry {
    LedgerEntry::ToolImage {
        entry: Box::new(entry),
    }
}

fn obligation(unit_key: &UnitKey, turn: &str, call_id: &str, display_name: &str) -> LedgerEntry {
    image_entry(ImageEntry::ImageObligation(Box::new(ImageObligation {
        unit_key: unit_key.clone(),
        turn: NativeTurn::new(turn),
        call_id: call_id.into(),
        image_index: 0,
        source: source(),
        record_start: 10,
        record_end: 20,
        sha256: Some("ab".into()),
        media_type: Some("image/png".into()),
        size: Some(4),
        display_name: display_name.into(),
        marker: "marker".into(),
    })))
}

fn opened(turn: &str, opener_offset: u64) -> LedgerEntry {
    image_entry(ImageEntry::ImageTurnOpened {
        source: source(),
        opener_offset,
        turn: NativeTurn::new(turn),
    })
}

fn closed(turn: &str, end_offset: u64) -> LedgerEntry {
    image_entry(ImageEntry::ImageTurnClosed {
        source: source(),
        turn: NativeTurn::new(turn),
        end_offset,
    })
}

fn apply(state: &mut LedgerState, entry: LedgerEntry) {
    state.apply(Utc::now(), entry);
}

/// Prepares the next serial for `unit_key` against the current anchor; returns its serial.
fn prepare(state: &mut LedgerState, unit_key: &UnitKey) -> u64 {
    let serial = state.next_serial();
    let entry = LedgerEntry::Prepared {
        serial,
        unit_key: unit_key.clone(),
        piece_index: 0,
        payload: format!("payload {serial}"),
        anchor_id: state.anchor(),
        epoch: 1,
        local_retry: None,
        notice_origin: None,
    };
    apply(state, entry);
    serial
}

fn unresolved(state: &mut LedgerState, serial: u64) {
    let reason = "history unreadable".into();
    apply(state, LedgerEntry::Unresolved { serial, reason });
}

fn posted(state: &mut LedgerState, serial: u64, msg_id: u64) {
    let confirmation = None;
    apply(
        state,
        LedgerEntry::Posted {
            serial,
            msg_id,
            confirmation,
        },
    );
}

const BASE_MS: u64 = 1_000_000;

/// An image attempt left Unresolved, then a body posted 41 s after the image's id.
struct LateImage {
    state: LedgerState,
    image_serial: u64,
    image_id: u64,
    body_id: u64,
}

fn late_image() -> LateImage {
    let mut state = LedgerState::starting_at(snowflake(BASE_MS));
    let key = image("u1", "ab");
    apply(&mut state, obligation(&key, "u1", "call-1", "a.png"));
    let image_serial = prepare(&mut state, &key);
    unresolved(&mut state, image_serial);
    let body_serial = prepare(&mut state, &unit("body-1", UnitKind::Body));
    let body_id = snowflake(BASE_MS + 41_000);
    posted(&mut state, body_serial, body_id);
    assert_eq!(state.violation(), None);
    LateImage {
        state,
        image_serial,
        image_id: snowflake(BASE_MS + 1_000),
        body_id,
    }
}

#[test]
fn unresolved_reconfirm_behind_newer_anchor_accepted() {
    let late = late_image();
    let verdict = late_confirm(&late.state, late.image_serial, late.image_id);
    // The anchor stays on the newer body; nothing here weakens the ordinary Posted check.
    let anchor = late.body_id;
    assert_eq!(verdict, LateConfirm::Accept { anchor });
}

#[test]
fn reconfirm_before_own_prepared_anchor_rejected() {
    let late = late_image();
    let own_anchor = snowflake(BASE_MS);
    for msg_id in [own_anchor, own_anchor - 1] {
        let verdict = late_confirm(&late.state, late.image_serial, msg_id);
        assert!(
            matches!(verdict, LateConfirm::Reject(_)),
            "{msg_id}: {verdict:?}"
        );
    }
}

#[test]
fn reconfirm_id_owned_by_other_serial_rejected() {
    let late = late_image();
    let verdict = late_confirm(&late.state, late.image_serial, late.body_id);
    assert!(matches!(verdict, LateConfirm::Reject(_)), "{verdict:?}");
}

#[test]
fn plain_posted_anchor_skew_check_unchanged() {
    let mut state = LedgerState::starting_at(snowflake(BASE_MS));
    let body = unit("body-0", UnitKind::Body);
    let body_serial = prepare(&mut state, &body);
    unresolved(&mut state, body_serial);
    let next = prepare(&mut state, &unit("body-1", UnitKind::Body));
    posted(&mut state, next, snowflake(BASE_MS + 41_000));
    let old_id = snowflake(BASE_MS + 1_000);
    assert_eq!(
        late_confirm(&state, body_serial, old_id),
        LateConfirm::Ordinary
    );
    // A tool-image piece this ledger does not owe is ordinary too.
    let stray = image("u9", "ff");
    let stray_serial = prepare(&mut state, &stray);
    unresolved(&mut state, stray_serial);
    assert_eq!(
        late_confirm(&state, stray_serial, old_id),
        LateConfirm::Ordinary
    );
    // The reducer still stops a plain late id behind the anchor beyond the skew.
    posted(&mut state, body_serial, old_id);
    let violation = state.violation().unwrap_or_default();
    assert!(violation.contains("behind the anchor"), "{violation}");
}

#[test]
fn tool_image_kind_not_postable_and_progress() {
    let key = image("u1", "ab");
    assert!(PieceKey::new(key, 0).is_none());
    assert_eq!(kind_name(UnitKind::ToolImage), "tool_image");
    assert_eq!(unit_class(UnitKind::ToolImage), NoticeClass::Progress);
}

fn steps(state: &LedgerState) -> Vec<OpenObligation> {
    state.tool_images().open_obligations(state)
}

#[test]
fn open_obligations_follow_image_and_notice_attempts() {
    let mut state = LedgerState::starting_at(snowflake(BASE_MS));
    let key = image("u1", "ab");
    let image_step = |step| OpenObligation::Image {
        key: key.clone(),
        step,
    };
    apply(&mut state, obligation(&key, "u1", "call-1", "a.png"));
    assert_eq!(steps(&state), vec![image_step(ImageStep::Attempt)]);
    let first = prepare(&mut state, &key);
    let waiting = Waiting::Prepared { serial: first };
    assert_eq!(steps(&state), vec![image_step(ImageStep::Waiting(waiting))]);
    apply(&mut state, LedgerEntry::NotFound { serial: first });
    let retry = ImageStep::RetryAfterNotFound { serial: first };
    assert_eq!(steps(&state), vec![image_step(retry)]);
    let second = prepare(&mut state, &key);
    apply(&mut state, LedgerEntry::NotFound { serial: second });
    let omit = ImageStep::Omit { serial: second };
    assert_eq!(steps(&state), vec![image_step(omit)]);

    let reason = OmitReason::NotConfirmed;
    let omitted = ImageEntry::ImageOmitted {
        unit_key: key.clone(),
        reason,
    };
    apply(&mut state, image_entry(omitted));
    let notice = unit(
        &notice_native_key(&NativeTurn::new("u1"), "call-1"),
        UnitKind::ToolImage,
    );
    let notice_step = |step| OpenObligation::Notice {
        key: notice.clone(),
        step,
    };
    assert_eq!(steps(&state), vec![notice_step(NoticeStep::Attempt)]);
    let mut last = 0;
    for attempt in 1..=3 {
        last = prepare(&mut state, &notice);
        apply(&mut state, LedgerEntry::NotFound { serial: last });
        let expected = match attempt {
            3 => NoticeStep::Exhausted { serial: last },
            _ => NoticeStep::RetryAfterNotFound { serial: last },
        };
        assert_eq!(steps(&state), vec![notice_step(expected)]);
    }
    let unresolved_notice = prepare(&mut state, &notice);
    unresolved(&mut state, unresolved_notice);
    let waiting = Waiting::Unresolved {
        serial: unresolved_notice,
    };
    assert_eq!(
        steps(&state),
        vec![notice_step(NoticeStep::Waiting(waiting))]
    );
    assert!(last < unresolved_notice);
    assert_eq!(state.violation(), None);
}

#[test]
fn retry_count_ignores_not_sent() {
    let mut state = LedgerState::starting_at(snowflake(BASE_MS));
    let key = image("u1", "ab");
    apply(&mut state, obligation(&key, "u1", "call-1", "a.png"));
    for _ in 0..2 {
        let serial = prepare(&mut state, &key);
        apply(&mut state, LedgerEntry::NotSent { serial });
    }
    let serial = prepare(&mut state, &key);
    apply(&mut state, LedgerEntry::NotFound { serial });
    let step = ImageStep::RetryAfterNotFound { serial };
    assert_eq!(steps(&state), vec![OpenObligation::Image { key, step }]);
}

#[test]
fn first_obligation_metadata_stands() {
    let mut state = LedgerState::default();
    let key = image("u1", "ab");
    apply(&mut state, obligation(&key, "u1", "call-1", "a.png"));
    apply(&mut state, obligation(&key, "u1", "call-2", "copy.png"));
    let stored = state
        .tool_images()
        .obligation(&key)
        .map(|o| o.display_name.as_str());
    assert_eq!(stored, Some("a.png"));
    assert_eq!(state.violation(), None);
}

#[test]
fn replayed_closed_is_idempotent_and_a_moved_end_is_a_violation() {
    let mut state = LedgerState::default();
    apply(&mut state, opened("u1", 100));
    apply(&mut state, closed("u1", 300));
    apply(&mut state, closed("u1", 300));
    assert_eq!(state.violation(), None);
    let spans = state.tool_images();
    let turn = spans.span_at(&source(), 299).map(|span| span.turn.clone());
    assert_eq!(turn, Some(NativeTurn::new("u1")));
    assert_eq!(spans.span_at(&source(), 300), None);
    assert_eq!(spans.span_at(&source(), 99), None);
    apply(&mut state, closed("u1", 400));
    assert!(state.violation().is_some());
}

#[test]
fn a_turn_opening_inside_an_open_span_is_a_violation() {
    let mut state = LedgerState::default();
    apply(&mut state, opened("u1", 100));
    apply(&mut state, opened("u2", 200));
    assert!(state.violation().is_some());
}

#[test]
fn tool_image_entries_round_trip_through_the_ledger_line() {
    let entry = opened("u1", 100);
    let text = serde_json::to_string(&entry).expect("serialize");
    assert!(text.contains("\"type\":\"tool_image\""), "{text}");
    assert!(text.contains("\"kind\":\"image_turn_opened\""), "{text}");
    let back: LedgerEntry = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back, entry);
}
