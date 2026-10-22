use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::super::pieces::{Derived, SummaryInputs, UnitDeriver};
use super::TURN_SUMMARY_OPEN;
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider};

fn record(mut value: Value, timestamp: &str) -> CapturedRecord {
    value["timestamp"] = json!(timestamp);
    CapturedRecord {
        start: 0,
        end: 1,
        line: serde_json::to_vec(&value).unwrap(),
    }
}

fn deriver() -> UnitDeriver {
    UnitDeriver::new(7, ShadowProvider::Claude).with_turn_summary(SummaryInputs {
        turn_marks: BTreeSet::new(),
        disposed: BTreeSet::new(),
        context_window: None,
    })
}

fn prompt(text: &str, timestamp: &str) -> CapturedRecord {
    record(json!({"type":"user","message":{"content":text}}), timestamp)
}

fn body(timestamp: &str) -> CapturedRecord {
    record(
        json!({"type":"assistant","apiBlockIndex":0,"message":{
            "id":"msg","content":[{"type":"text","text":"answer"}]
        }}),
        timestamp,
    )
}

fn closing(timestamp: &str) -> CapturedRecord {
    record(
        json!({"type":"system","subtype":"stop_hook_summary"}),
        timestamp,
    )
}

fn assert_turn(work: &[Derived], expected: &str) {
    let marks = work
        .iter()
        .filter_map(|item| match item {
            Derived::Excluded { unit_key, reason } if reason == TURN_SUMMARY_OPEN => {
                Some(unit_key.native_key.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let posts = work
        .iter()
        .filter_map(|item| match item {
            Derived::Piece(piece) => {
                Some((piece.unit_key.native_key.as_str(), piece.payload.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(marks, ["msg:0#turn"]);
    assert_eq!(posts, [("msg:0", "answer"), ("msg:0#turn_end", expected)]);
    assert!(
        !work
            .iter()
            .any(|item| matches!(item, Derived::Blocked { .. }))
    );
}

#[test]
fn synthetic_resume_chrome_cannot_capture_the_next_prompt_elapsed_or_summary_key() {
    let mut derive = deriver();
    let chrome = record(
        json!({
            "type":"assistant","uuid":"u-synthetic","isApiErrorMessage":false,
            "message":{"id":"resume","model":"<synthetic>","content":[{
                "type":"text","text":"No response requested."
            }]}
        }),
        "2026-10-19T00:00:00Z",
    );
    let mut work = derive.derive(&chrome);
    assert!(
        matches!(work.as_slice(), [Derived::Excluded { unit_key, reason }]
        if unit_key.native_key == "u-synthetic" && reason == "claude_synthetic_resume_chrome")
    );
    work.extend(derive.derive(&prompt("real question", "2026-10-19T03:00:00Z")));
    work.extend(derive.derive(&body("2026-10-19T03:00:30Z")));
    work.extend(derive.derive(&closing("2026-10-19T03:01:00Z")));
    assert_turn(&work, "-# ✅ 1분");
}

#[test]
fn unitless_local_commands_restart_at_the_real_prompt_but_active_units_keep_their_start() {
    let mut derive = deriver();
    assert!(
        derive
            .derive(&prompt(
                "<command-name>/model</command-name>",
                "2026-10-19T00:00:00Z"
            ))
            .is_empty()
    );
    assert!(
        derive
            .derive(&prompt("real question", "2026-10-19T00:10:00Z"))
            .is_empty()
    );
    let mut work = derive.derive(&body("2026-10-19T00:10:01Z"));
    work.extend(derive.derive(&prompt("queued question", "2026-10-19T00:10:03Z")));
    work.extend(derive.derive(&closing("2026-10-19T00:10:05Z")));
    assert_turn(&work, "-# ✅ 5초");
}
