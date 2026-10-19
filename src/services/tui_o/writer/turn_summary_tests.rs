use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};

use super::pieces::{Derived, PieceWork, SummaryInputs, ToolCardInputs, UnitDeriver};
use super::turn_summary::{TURN_SUMMARY_OPEN, unit_hash};
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, UnitKey, UnitKind};

const START: &str = "2026-10-19T00:00:00.000Z";
const END: &str = "2026-10-19T00:07:14.000Z";

fn record(value: Value) -> CapturedRecord {
    CapturedRecord {
        start: 0,
        end: 1,
        line: serde_json::to_vec(&value).unwrap(),
    }
}

fn timed(mut value: Value, timestamp: Option<&str>) -> CapturedRecord {
    if let Some(timestamp) = timestamp {
        value["timestamp"] = json!(timestamp);
    }
    record(value)
}

fn inputs(context_window: Option<u64>) -> SummaryInputs {
    SummaryInputs {
        turn_marks: BTreeSet::new(),
        disposed: BTreeSet::new(),
        context_window,
    }
}

fn enabled(provider: ShadowProvider, inputs: SummaryInputs) -> UnitDeriver {
    UnitDeriver::new(7, provider).with_turn_summary(inputs)
}

fn with_cards(provider: ShadowProvider, inputs: SummaryInputs) -> UnitDeriver {
    enabled(provider, inputs).with_tool_cards(ToolCardInputs {
        known: BTreeSet::new(),
        recorded_cards: BTreeSet::new(),
        switch: Arc::new(|| true),
    })
}

fn prompt(timestamp: Option<&str>) -> CapturedRecord {
    timed(
        json!({"type":"user","message":{"content":"native prompt"}}),
        timestamp,
    )
}

fn body(id: &str, text: &str, timestamp: Option<&str>) -> CapturedRecord {
    timed(
        json!({"type":"assistant","apiBlockIndex":0,"message":{
            "id":id,"content":[{"type":"text","text":text}]
        }}),
        timestamp,
    )
}

fn with_usage(mut row: CapturedRecord, usage: Value) -> CapturedRecord {
    let mut value: Value = serde_json::from_slice(&row.line).unwrap();
    value["message"]["usage"] = usage;
    row.line = serde_json::to_vec(&value).unwrap();
    row
}

fn idle(subtype: &str, timestamp: Option<&str>) -> CapturedRecord {
    timed(
        json!({"type":"system","subtype":subtype,"durationMs":999_999}),
        timestamp,
    )
}

fn interrupt(timestamp: Option<&str>) -> CapturedRecord {
    timed(
        json!({"type":"user","message":{"content":[{
            "type":"text","text":"[Request interrupted by user]"
        }]}}),
        timestamp,
    )
}

fn event(kind: &str, id: Option<&str>, timestamp: Option<&str>) -> CapturedRecord {
    let mut value = json!({"type":"event_msg","payload":{"type":kind}});
    if let Some(id) = id {
        value["payload"]["turn_id"] = json!(id);
    }
    timed(value, timestamp)
}

fn codex_body(id: &str) -> CapturedRecord {
    record(json!({"type":"response_item","payload":{
        "type":"message","role":"assistant","id":id,
        "content":[{"type":"output_text","text":"codex answer"}]
    }}))
}

fn pieces(work: &[Derived]) -> Vec<&PieceWork> {
    work.iter()
        .filter_map(|item| match item {
            Derived::Piece(piece) => Some(piece),
            _ => None,
        })
        .collect()
}

fn summaries(work: &[Derived]) -> Vec<&PieceWork> {
    pieces(work)
        .into_iter()
        .filter(|piece| piece.unit_key.native_key.ends_with("#turn_end"))
        .collect()
}

fn marks(work: &[Derived]) -> Vec<&UnitKey> {
    work.iter()
        .filter_map(|item| match item {
            Derived::Excluded { unit_key, reason } if reason == TURN_SUMMARY_OPEN => Some(unit_key),
            _ => None,
        })
        .collect()
}

#[test]
fn claude_first_idle_emits_one_summary_after_the_card_and_final_body() {
    let mut derive = with_cards(ShadowProvider::Claude, inputs(Some(1_000)));
    let mut work = derive.derive(&prompt(Some(START)));
    work.extend(derive.derive(&with_usage(
        body("first", "before", Some(START)),
        json!({"input_tokens":100,"cache_creation_input_tokens":100,"cache_read_input_tokens":100}),
    )));
    work.extend(derive.derive(&record(json!({
        "type":"assistant","apiBlockIndex":0,"message":{"id":"tool","content":[{
            "type":"tool_use","id":"call","name":"Bash","input":{"command":"cargo check"}
        }]}
    }))));
    work.extend(derive.derive(&record(json!({"type":"user","message":{"content":[{
        "type":"tool_result","tool_use_id":"call","is_error":true,"content":"PRIVATE_TOOL_FAILURE"
    }]}}))));
    work.extend(derive.derive(&prompt(Some("2026-10-19T00:07:00.000Z"))));
    work.extend(derive.derive(&with_usage(
        body("last", "after", Some(END)),
        json!({"input_tokens":100,"cache_creation_input_tokens":200,"cache_read_input_tokens":150,"output_tokens":999_999}),
    )));
    work.extend(derive.derive(&idle("stop_hook_summary", Some(END))));
    assert!(
        derive
            .derive(&idle("turn_duration", Some("2026-10-19T00:07:15.000Z")))
            .is_empty()
    );
    let posted = pieces(&work);
    assert_eq!(
        posted
            .iter()
            .map(|piece| piece.payload.as_str())
            .collect::<Vec<_>>(),
        vec![
            "before",
            "-# ▸ Bash  cargo check  ✗",
            "after",
            "-# ✅ 7분 14초 · 📦 45%"
        ]
    );
    assert_eq!(summaries(&work).len(), 1);
    assert_eq!(posted[3].unit_key.native_key, "first:0#turn_end");
    assert_eq!(posted[3].unit_key.kind, UnitKind::Tool);
    assert_eq!(posted[3].index, 0);
    assert_eq!(marks(&work).len(), 1);
    assert_eq!(marks(&work)[0].native_key, "first:0#turn");
    assert!(matches!(&work[0], Derived::Excluded { reason, .. } if reason == TURN_SUMMARY_OPEN));
    assert!(!format!("{work:?}").contains("PRIVATE_TOOL_FAILURE"));
}

#[test]
fn claude_interrupt_closes_a_nonempty_turn_as_cancelled_once() {
    let mut derive = enabled(ShadowProvider::Claude, inputs(None));
    derive.derive(&prompt(Some(START)));
    derive.derive(&body("answer", "partial", Some(START)));
    let work = derive.derive(&interrupt(Some("2026-10-19T00:00:05.000Z")));
    assert_eq!(summaries(&work).len(), 1);
    assert_eq!(summaries(&work)[0].payload, "-# ⏹ 중단 · 5초");
    assert!(derive.derive(&idle("turn_duration", Some(END))).is_empty());
}

#[test]
fn claude_last_assistant_controls_api_failure_and_recovery() {
    for (errors, expected) in [
        (vec![true], "-# ⚠️ 오류 · 5초"),
        (vec![true, false], "-# ✅ 5초"),
        (vec![false, true], "-# ⚠️ 오류 · 5초"),
    ] {
        let mut derive = enabled(ShadowProvider::Claude, inputs(None));
        derive.derive(&prompt(Some(START)));
        for (index, error) in errors.into_iter().enumerate() {
            let row = body(&format!("answer-{index}"), "answer", Some(START));
            let mut value: Value = serde_json::from_slice(&row.line).unwrap();
            value["isApiErrorMessage"] = json!(error);
            derive.derive(&record(value));
        }
        let work = derive.derive(&idle("stop_hook_summary", Some("2026-10-19T00:00:05.000Z")));
        assert_eq!(summaries(&work).len(), 1);
        assert_eq!(summaries(&work)[0].payload, expected);
    }
}

#[test]
fn claude_terminal_result_error_overrides_a_normal_last_assistant() {
    let mut derive = enabled(ShadowProvider::Claude, inputs(None));
    derive.derive(&prompt(None));
    derive.derive(&body("answer", "partial answer", None));
    let work = derive.derive(&record(json!({"type":"result","is_error":true})));
    assert_eq!(summaries(&work).len(), 1);
    assert_eq!(summaries(&work)[0].payload, "-# ⚠️ 오류");
}

#[test]
fn empty_and_repeat_only_turns_emit_neither_summary_nor_new_opening_mark() {
    let mut derive = enabled(ShadowProvider::Claude, inputs(None));
    derive.derive(&prompt(Some(START)));
    assert!(derive.derive(&interrupt(Some(END))).is_empty());
    derive.derive(&prompt(Some(START)));
    let answer = body("repeated", "answer", Some(START));
    assert_eq!(marks(&derive.derive(&answer)).len(), 1);
    assert_eq!(
        summaries(&derive.derive(&idle("stop_hook_summary", Some(END)))).len(),
        1
    );
    assert!(derive.sealed("repeated:0", UnitKind::Body));
    derive.derive(&prompt(Some(START)));
    assert!(
        derive.derive(&answer).is_empty(),
        "Idle must not clear native seals"
    );
    assert!(
        derive
            .derive(&idle("stop_hook_summary", Some(END)))
            .is_empty()
    );
}

#[test]
fn autonomous_claude_turn_uses_the_first_assistant_timestamp() {
    let mut derive = enabled(ShadowProvider::Claude, inputs(None));
    assert!(
        derive
            .derive(&idle("turn_duration", Some(START)))
            .is_empty()
    );
    let opening = derive.derive(&body(
        "autonomous",
        "answer",
        Some("2026-10-19T00:01:00.000Z"),
    ));
    assert_eq!(marks(&opening).len(), 1);
    let work = derive.derive(&idle("stop_hook_summary", Some("2026-10-19T00:02:03.000Z")));
    assert_eq!(summaries(&work).len(), 1);
    assert_eq!(summaries(&work)[0].payload, "-# ✅ 1분 3초");
}

#[test]
fn scratch_derivation_has_no_summary_or_marks_and_preserves_body_bytes() {
    let text = format!("한글\n```rust\n{}\n```", "😀".repeat(2_000));
    let rows = [
        prompt(Some(START)),
        body("answer", &text, Some(START)),
        idle("stop_hook_summary", Some(END)),
    ];
    let mut scratch = UnitDeriver::new(7, ShadowProvider::Claude);
    let mut summary = enabled(ShadowProvider::Claude, inputs(None));
    let mut plain = Vec::new();
    let mut enhanced = Vec::new();
    for row in &rows {
        plain.extend(scratch.derive(row));
        enhanced.extend(summary.derive(row));
    }
    assert!(
        !pieces(&plain).is_empty(),
        "body comparison must not be vacuous"
    );
    assert!(marks(&plain).is_empty() && summaries(&plain).is_empty());
    let body_work = |work: Vec<Derived>| {
        work.into_iter()
        .filter(|item| matches!(item, Derived::Piece(piece) if piece.unit_key.kind == UnitKind::Body))
        .collect::<Vec<_>>()
    };
    assert_eq!(body_work(plain), body_work(enhanced));
    assert!(scratch.derive(&rows[1]).is_empty());
}

#[test]
fn missing_invalid_or_negative_timestamps_never_use_duration_metadata() {
    for (start, end) in [
        (None, Some(END)),
        (Some(START), None),
        (Some("not-a-timestamp"), Some(END)),
        (Some(END), Some(START)),
    ] {
        let mut derive = enabled(ShadowProvider::Claude, inputs(None));
        derive.derive(&prompt(start));
        derive.derive(&body("answer", "answer", Some(START)));
        let work = derive.derive(&idle("turn_duration", end));
        assert_eq!(summaries(&work).len(), 1);
        assert_eq!(summaries(&work)[0].payload, "-# ✅ 완료");
    }
}

#[test]
fn context_uses_latest_usage_sum_and_clamps_overflow_without_a_warning_suffix() {
    for (usage, window, expected) in [
        (
            json!({"input_tokens":1,"cache_creation_input_tokens":2,"cache_read_input_tokens":3}),
            Some(10),
            "-# ✅ · 📦 60%",
        ),
        (
            json!({"input_tokens":u64::MAX,"cache_creation_input_tokens":u64::MAX,"cache_read_input_tokens":u64::MAX}),
            Some(u64::MAX),
            "-# ✅ · 📦 100%",
        ),
        (json!({"input_tokens":9}), Some(0), "-# ✅ 완료"),
        (json!({"input_tokens":9}), None, "-# ✅ 완료"),
    ] {
        let mut derive = enabled(ShadowProvider::Claude, inputs(window));
        derive.derive(&prompt(None));
        derive.derive(&with_usage(
            body("first", "first", None),
            json!({"input_tokens":999}),
        ));
        derive.derive(&with_usage(body("last", "last", None), usage));
        let work = derive.derive(&idle("stop_hook_summary", None));
        assert_eq!(summaries(&work).len(), 1);
        assert_eq!(summaries(&work)[0].payload, expected);
        assert!(summaries(&work)[0].payload.encode_utf16().count() <= 80);
    }
}

#[test]
fn disposed_first_unit_without_a_turn_mark_never_backfills_even_with_a_later_new_body() {
    let old = UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: "old:0".into(),
        kind: UnitKind::Body,
    };
    let mut snapshot = inputs(None);
    snapshot.disposed.insert(unit_hash(&old));
    let mut derive = enabled(ShadowProvider::Claude, snapshot);
    derive.derive(&prompt(Some(START)));
    let first = derive.derive(&body("old", "old answer", Some(START)));
    assert!(marks(&first).is_empty());
    assert!(marks(&derive.derive(&body("new", "later answer", Some(START)))).is_empty());
    assert!(summaries(&derive.derive(&idle("stop_hook_summary", Some(END)))).is_empty());
    derive.derive(&prompt(Some(START)));
    assert_eq!(
        marks(&derive.derive(&body("fresh", "fresh answer", Some(START)))).len(),
        1
    );
    assert_eq!(
        summaries(&derive.derive(&idle("stop_hook_summary", Some(END)))).len(),
        1
    );
}

#[test]
fn durable_opening_mark_restores_eligibility_after_the_first_body_is_disposed() {
    let mut before = enabled(ShadowProvider::Claude, inputs(None));
    before.derive(&prompt(Some(START)));
    let opening = before.derive(&body("first", "answer", Some(START)));
    let mut snapshot = inputs(None);
    snapshot
        .turn_marks
        .insert(marks(&opening)[0].native_key.clone());
    snapshot
        .disposed
        .insert(unit_hash(&pieces(&opening)[0].unit_key));
    let original = before.derive(&idle("stop_hook_summary", Some(END)));
    let mut resumed = enabled(ShadowProvider::Claude, snapshot);
    resumed.derive(&prompt(Some(START)));
    assert_eq!(
        marks(&resumed.derive(&body("first", "answer", Some(START)))).len(),
        1
    );
    let replayed = resumed.derive(&idle("stop_hook_summary", Some(END)));
    assert_eq!(summaries(&original).len(), 1);
    assert_eq!(summaries(&replayed), summaries(&original));
    assert_eq!(
        summaries(&replayed)[0].unit_key.native_key,
        "first:0#turn_end"
    );
}

#[test]
fn codex_native_turn_emits_card_body_and_one_summary_without_claiming_tool_success() {
    let mut derive = with_cards(ShadowProvider::Codex, inputs(Some(1_000)));
    let mut work = derive.derive(&event("task_started", Some("turn-1"), Some(START)));
    work.extend(derive.derive(&record(json!({"type":"response_item","payload":{
        "type":"function_call","id":"native-tool","call_id":"call","name":"functions.exec_command",
        "arguments":"{\"cmd\":\"cargo check\"}"
    }}))));
    work.extend(
        derive.derive(&record(json!({"type":"response_item","payload":{
            "type":"function_call_output","call_id":"call","output":"PRIVATE_RESULT_BODY"
        }}))),
    );
    work.extend(derive.derive(&codex_body("answer")));
    work.extend(derive.derive(&event(
        "task_started",
        Some("turn-1"),
        Some("2026-10-19T00:00:50.000Z"),
    )));
    work.extend(derive.derive(&event(
        "task_complete",
        Some("turn-1"),
        Some("2026-10-19T00:01:02.000Z"),
    )));
    assert!(
        derive
            .derive(&event("task_complete", Some("turn-1"), Some(END)))
            .is_empty()
    );
    let posted = pieces(&work);
    assert_eq!(
        posted
            .iter()
            .map(|piece| piece.payload.as_str())
            .collect::<Vec<_>>(),
        vec!["-# ▸ Bash  cargo check  ·", "codex answer", "-# ✅ 1분 2초"]
    );
    assert_eq!(marks(&work).len(), 1);
    assert_eq!(marks(&work)[0].native_key, "native-tool#turn");
    assert_eq!(summaries(&work).len(), 1);
    assert_eq!(
        summaries(&work)[0].unit_key.native_key,
        "native-tool#turn_end"
    );
    assert_eq!(summaries(&work)[0].unit_key.kind, UnitKind::Tool);
    assert!(!format!("{work:?}").contains("PRIVATE_RESULT_BODY"));
}

#[test]
fn codex_named_abort_closes_only_its_matching_turn_and_never_duplicates() {
    let mut derive = enabled(ShadowProvider::Codex, inputs(None));
    derive.derive(&event("task_started", Some("ours"), Some(START)));
    derive.derive(&codex_body("answer"));
    for kind in ["turn_aborted", "task_complete"] {
        assert!(summaries(&derive.derive(&event(kind, Some("foreign"), Some(END)))).is_empty());
    }
    let work = derive.derive(&event(
        "turn_aborted",
        Some("ours"),
        Some("2026-10-19T00:00:05.000Z"),
    ));
    assert_eq!(summaries(&work).len(), 1);
    assert_eq!(summaries(&work)[0].payload, "-# ⏹ 중단 · 5초");
    assert!(
        derive
            .derive(&event("task_complete", Some("ours"), Some(END)))
            .is_empty()
    );
    assert!(
        derive
            .derive(&event("turn_aborted", Some("ours"), Some(END)))
            .is_empty()
    );
}

#[test]
fn codex_unnamed_abort_does_not_close_or_cancel_the_active_turn() {
    let mut derive = enabled(ShadowProvider::Codex, inputs(None));
    derive.derive(&event("task_started", Some("ours"), None));
    derive.derive(&codex_body("answer"));
    assert!(derive.derive(&event("turn_aborted", None, None)).is_empty());
    let work = derive.derive(&event("task_complete", Some("ours"), None));
    assert_eq!(summaries(&work).len(), 1);
    assert_eq!(summaries(&work)[0].payload, "-# ✅ 완료");
}

#[test]
fn codex_native_error_is_failed_but_a_named_abort_still_takes_precedence() {
    for (terminal, expected) in [
        ("task_complete", "-# ⚠️ 오류 · 5초"),
        ("turn_aborted", "-# ⏹ 중단 · 5초"),
    ] {
        let mut derive = enabled(ShadowProvider::Codex, inputs(None));
        derive.derive(&event("task_started", Some("ours"), Some(START)));
        derive.derive(&codex_body("answer"));
        derive.derive(&record(json!({"type":"event_msg","payload":{"type":"error","message":"PRIVATE_PROVIDER_ERROR"}})));
        let work = derive.derive(&event(
            terminal,
            Some("ours"),
            Some("2026-10-19T00:00:05.000Z"),
        ));
        assert_eq!(summaries(&work).len(), 1);
        assert_eq!(summaries(&work)[0].payload, expected);
        assert!(
            !summaries(&work)[0]
                .payload
                .contains("PRIVATE_PROVIDER_ERROR")
        );
    }
}
