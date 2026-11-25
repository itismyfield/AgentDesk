//! Tool-image ledger entries, open obligations and the late-confirmation judgment.

use std::path::PathBuf;

use chrono::Utc;

use super::tool_images::{
    ImageEntry, ImageObligation, ImageStep, LateConfirm, NoticeStep, OpenObligation, Waiting,
    late_confirm, notice_native_key,
};
use super::{LedgerEntry, LedgerState};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, UnitKey, UnitKind};

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

/// The writer's `image_native_key` shape: `img:{turn}:{sha256}`.
fn image(turn: &str, sha256: &str) -> UnitKey {
    unit(&format!("img:{turn}:{sha256}"), UnitKind::ToolImage)
}

fn image_entry(entry: ImageEntry) -> LedgerEntry {
    LedgerEntry::ToolImage {
        entry: Box::new(entry),
    }
}

fn obligation(unit_key: &UnitKey, turn: &str, call_id: &str, display_name: &str) -> LedgerEntry {
    image_entry(ImageEntry::ImageObligation(Box::new(ImageObligation {
        unit_key: unit_key.clone(),
        turn: turn.into(),
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
        turn: turn.into(),
    })
}

fn closed(turn: &str, end_offset: u64) -> LedgerEntry {
    image_entry(ImageEntry::ImageTurnClosed {
        source: source(),
        turn: turn.into(),
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

    let reason = "not_confirmed".to_owned();
    let omitted = ImageEntry::ImageOmitted {
        unit_key: key.clone(),
        reason,
    };
    apply(&mut state, image_entry(omitted));
    let notice = unit(&notice_native_key("u1", "call-1"), UnitKind::ToolImage);
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
    assert_eq!(turn.as_deref(), Some("u1"));
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

fn obligation_at(unit_key: &UnitKey, call_id: &str, record_start: u64) -> LedgerEntry {
    image_entry(ImageEntry::ImageObligation(Box::new(ImageObligation {
        unit_key: unit_key.clone(),
        turn: "u1".into(),
        call_id: call_id.into(),
        image_index: 0,
        source: source(),
        record_start,
        record_end: record_start + 10,
        sha256: Some(unit_key.native_key.clone()),
        media_type: Some("image/png".into()),
        size: Some(4),
        display_name: "a.png".into(),
        marker: "marker".into(),
    })))
}

fn omit(unit_key: &UnitKey, reason: &str) -> LedgerEntry {
    let (unit_key, reason) = (unit_key.clone(), reason.to_owned());
    image_entry(ImageEntry::ImageOmitted { unit_key, reason })
}

fn call(call_id: &str, turn: &str) -> LedgerEntry {
    let (source, call_id, turn) = (source(), call_id.to_owned(), turn.to_owned());
    image_entry(ImageEntry::ImageCall {
        source,
        call_id,
        turn,
    })
}

#[test]
fn call_attribution_is_durable_and_one_turn_per_call() {
    let mut state = LedgerState::default();
    apply(&mut state, call("c1", "u1"));
    apply(&mut state, call("c1", "u1"));
    assert_eq!(state.violation(), None);
    let images = state.tool_images();
    assert_eq!(images.call_turn(&source(), "c1"), Some("u1"));
    assert_eq!(images.call_turn(&source(), "c2"), None);
    apply(&mut state, call("c1", "u2"));
    assert!(state.violation().is_some());
}

/// A ledger owing `key`, with the attempts `attempt` adds before the omission.
fn omission_after(attempt: impl FnOnce(&mut LedgerState, &UnitKey), reason: &str) -> LedgerState {
    let mut state = LedgerState::starting_at(snowflake(BASE_MS));
    let key = image("u1", "ab");
    apply(&mut state, obligation(&key, "u1", "call-1", "a.png"));
    attempt(&mut state, &key);
    apply(&mut state, omit(&key, reason));
    state
}

#[test]
fn omission_after_posted_is_rejected() {
    let state = omission_after(
        |state, key| {
            let serial = prepare(state, key);
            posted(state, serial, snowflake(BASE_MS + 1_000));
        },
        "not_confirmed",
    );
    assert!(state.violation().is_some());
}

#[test]
fn omission_while_prepared_or_unresolved_is_rejected() {
    let open = omission_after(
        |state, key| {
            prepare(state, key);
        },
        "not_confirmed",
    );
    assert!(open.violation().is_some());
    let unresolved_attempt = omission_after(
        |state, key| {
            let serial = prepare(state, key);
            unresolved(state, serial);
        },
        "not_confirmed",
    );
    assert!(unresolved_attempt.violation().is_some());
}

#[test]
fn omission_while_ambiguous_is_rejected() {
    let state = omission_after(
        |state, key| {
            let serial = prepare(state, key);
            let candidates = vec![snowflake(BASE_MS + 1), snowflake(BASE_MS + 2)];
            apply(state, LedgerEntry::Ambiguous { serial, candidates });
        },
        "not_confirmed",
    );
    assert!(state.violation().is_some());
}

#[test]
fn omission_before_any_attempt_is_accepted() {
    let state = omission_after(|_, _| {}, "too_large");
    assert_eq!(state.violation(), None);
}

#[test]
fn omission_after_spent_not_found_is_accepted() {
    let spent = omission_after(
        |state, key| {
            for _ in 0..2 {
                let serial = prepare(state, key);
                apply(state, LedgerEntry::NotFound { serial });
            }
        },
        "not_confirmed",
    );
    assert_eq!(spent.violation(), None);
    // One NotFound still owes the dedicated retry.
    let early = omission_after(
        |state, key| {
            let serial = prepare(state, key);
            apply(state, LedgerEntry::NotFound { serial });
        },
        "not_confirmed",
    );
    assert!(early.violation().is_some());
}

#[test]
fn omission_after_rejected_upload_follows_the_status() {
    let rejected = |status: u16| {
        move |state: &mut LedgerState, key: &UnitKey| {
            let serial = prepare(state, key);
            apply(state, LedgerEntry::Rejected { serial, status });
        }
    };
    // 400 and 413 refuse the upload itself: it ends as an omission with its notice.
    for status in [400, 413] {
        let state = omission_after(rejected(status), "unsupported_type");
        assert_eq!(state.violation(), None, "{status}");
    }
    // 403 and 404 refuse the channel: the existing Rejected path owns them.
    for status in [403, 404] {
        let state = omission_after(rejected(status), "unsupported_type");
        assert!(state.violation().is_some(), "{status}");
    }
}

/// One tool result with two images omitted for different reasons.
fn two_omitted() -> (LedgerState, UnitKey) {
    let mut state = LedgerState::starting_at(snowflake(BASE_MS));
    let (first, second) = (image("u1", "aa"), image("u1", "bb"));
    apply(&mut state, obligation_at(&first, "call-1", 100));
    apply(&mut state, obligation_at(&second, "call-1", 100));
    apply(&mut state, omit(&first, "too_large"));
    apply(&mut state, omit(&second, "unsupported_type"));
    let notice = unit(&notice_native_key("u1", "call-1"), UnitKind::ToolImage);
    assert_eq!(state.violation(), None);
    (state, notice)
}

#[test]
fn notice_members_list_each_omitted_image_with_reason_and_range() {
    let (state, notice) = two_omitted();
    let notices: Vec<_> = steps(&state)
        .into_iter()
        .filter(|open| matches!(open, OpenObligation::Notice { .. }))
        .collect();
    assert_eq!(notices.len(), 1);
    let members = state.tool_images().notice_members(&notice);
    let mut reasons: Vec<_> = members
        .iter()
        .map(|member| member.reason.as_str())
        .collect();
    reasons.sort_unstable();
    assert_eq!(reasons, ["too_large", "unsupported_type"]);
    for member in &members {
        assert_eq!(
            (member.source.clone(), member.record_start),
            (source(), 100)
        );
        assert_eq!(member.record_end, 110);
    }
}

#[test]
fn protected_ranges_keep_notice_members_until_the_notice_posts() {
    let (mut state, notice) = two_omitted();
    let protects = |state: &LedgerState| {
        let ranges = state.tool_images().protected_ranges(state);
        ranges.contains(&(source(), 100, 110))
    };
    assert!(protects(&state), "Attempt");
    let serial = prepare(&mut state, &notice);
    assert!(protects(&state), "Waiting");
    apply(&mut state, LedgerEntry::NotFound { serial });
    for _ in 0..2 {
        let serial = prepare(&mut state, &notice);
        apply(&mut state, LedgerEntry::NotFound { serial });
    }
    let exhausted = matches!(
        steps(&state).as_slice(),
        [OpenObligation::Notice {
            step: NoticeStep::Exhausted { .. },
            ..
        }]
    );
    assert!(exhausted && protects(&state), "Exhausted");
    let serial = prepare(&mut state, &notice);
    posted(&mut state, serial, snowflake(BASE_MS + 5_000));
    assert!(!protects(&state), "posted");
}
