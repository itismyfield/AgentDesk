#![cfg(unix)]
use super::super::*;
use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view};
use crate::services::tui_turn_state::idle_compaction_tests::{compacted_tail, end_turn, tail};
use crate::services::tui_turn_state::{TuiTurnState, observe_claude_jsonl_turn_state};

fn append_completion(r: &Rig, at: DateTime<Utc>) {
    let mut reply = end_turn();
    reply["timestamp"] = json!(at);
    r.append(reply);
    for record in tail() {
        r.append(record);
    }
}

fn publish(r: &Rig, kind: HookEventKind, at: DateTime<Utc>, continuing: bool) {
    note_hook(
        "claude",
        &kind,
        &r.session,
        &json!({"session_id":r.ctx.expected_native_session_id,"transcript_path":r.path}),
        Some(&r.ctx),
        Some(at),
        continuing,
    );
}

fn held_without_keys(r: &Rig, tmux: &ScriptedTmux) {
    assert!(!r.ready(), "the earlier reply cannot settle this work");
    super::assert_evidence_pending(&super::send_with_budget(r), tmux);
}

#[test]
fn stop_hook_context_before_its_user_record_holds_the_actual_preparation_path() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    append_completion(&r, Utc::now());
    r.append(
        json!({"type":"attachment","attachment":{"type":"hook_success",
        "hookEvent":"Stop","hookName":"fixture hook","stdout":"{\"suppressOutput\":true}"}}),
    );
    assert!(r.ready(), "a quiet Stop success stays neutral");
    r.append(json!({"type":"attachment","attachment":{"type":"hook_success",
        "hookEvent":"Stop","hookName":"fixture hook","exitCode":0,"stderr":"",
        "stdout":"{\"hookSpecificOutput\":{\"hookEventName\":\"Stop\",\"additionalContext\":\"continue the answer\"}}"}}));
    let result = super::send_with_budget(&r);
    eprintln!(
        "Stop context before opener: result={result:?}, writes={:?}",
        tmux.sends()
    );
    super::assert_evidence_pending(&result, &tmux);
    assert_ne!(observe_claude_jsonl_turn_state(&r.path), TuiTurnState::Idle);
    r.append(
        json!({"type":"attachment","attachment":{"type":"hook_additional_context",
        "hookEvent":"Stop","content":"continue the answer"}}),
    );
    held_without_keys(&r, &tmux);
    r.append(json!({"type":"user","isMeta":true,"timestamp":Utc::now(),
        "message":{"role":"user","content":"continue the answer"}}));
    held_without_keys(&r, &tmux);
    r.assistant("continuing the answer");
    held_without_keys(&r, &tmux);
    append_completion(&r, Utc::now());
    assert_eq!(super::send_with_budget(&r), Ok(()));
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
}

#[test]
fn queued_receipts_then_user_prompt_submit_fence_the_previous_idle_reply() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    append_completion(&r, Utc::now());
    for receipt in [
        json!({"type":"queue-operation","operation":"enqueue","content":"queued input"}),
        json!({"type":"attachment","attachment":{"type":"queued_command",
            "commandMode":"prompt","origin":{"kind":"human"},"humanTurn":true,
            "prompt":"queued input"}}),
        json!({"type":"queue-operation","operation":"remove","content":"queued input",
            "reason":"absorbed_mid_turn"}),
        json!({"type":"queue-operation","operation":"dequeue"}),
    ] {
        r.append(receipt);
        assert!(r.ready(), "queue bookkeeping alone opens no native turn");
    }
    let reservation = reserve(observe(&r.session, Some(&r.path)).unwrap()).unwrap();
    assert!(reservation.current());
    publish(&r, HookEventKind::UserPromptSubmit, Utc::now(), false);
    assert!(
        !reservation.current(),
        "UserPromptSubmit invalidates a reservation even before the transcript grows"
    );
    reservation.not_sent();
    held_without_keys(&r, &tmux);

    r.append(json!({"type":"user","timestamp":Utc::now(),
        "message":{"role":"user","content":"queued input"}}));
    held_without_keys(&r, &tmux);
    r.assistant("working on the queued input");
    held_without_keys(&r, &tmux);
    append_completion(&r, Utc::now());
    assert!(
        r.ready(),
        "this submit's own completed reply clears its floor"
    );
    assert_eq!(super::send_with_budget(&r), Ok(()));
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
}

#[test]
fn continuing_stop_keeps_idle_metadata_and_late_terminals_fenced_until_a_new_reply() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let r = Rig::new();
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    append_completion(&r, Utc::now());
    assert!(r.ready());
    let continued_at = Utc::now();
    publish(&r, HookEventKind::Stop, continued_at, true);
    held_without_keys(&r, &tmux);
    r.append(json!({"type":"attachment","timestamp":continued_at,
        "attachment":{"type":"hook_success","hookEvent":"Stop","hookName":"fixture hook",
            "command":"node fixture-stop.mjs","exitCode":0,"stderr":"",
            "stdout":"{\"hookSpecificOutput\":{\"hookEventName\":\"Stop\",\"additionalContext\":\"continue the answer\"}}"}}));
    held_without_keys(&r, &tmux);
    r.append(json!({"type":"system","subtype":"stop_hook_summary",
        "timestamp":continued_at,"preventedContinuation":true,
        "hookAdditionalContext":["continue the answer"]}));
    held_without_keys(&r, &tmux);
    r.terminal();
    held_without_keys(&r, &tmux);
    for record in tail() {
        r.append(record);
        held_without_keys(&r, &tmux);
    }
    r.append(
        json!({"type":"assistant","timestamp":continued_at + chrono::Duration::milliseconds(1),
        "message":{"role":"assistant","stop_reason":null,
            "content":[{"type":"text","text":"continuing the answer"}]}}),
    );
    held_without_keys(&r, &tmux);
    append_completion(&r, continued_at + chrono::Duration::milliseconds(2));
    assert!(
        r.ready(),
        "a reply after the continuing Stop clears its own fence"
    );
    assert_eq!(super::send_with_budget(&r), Ok(()));
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
}

#[test]
fn each_compaction_attachment_and_notice_preserves_only_an_already_completed_reply() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let closed = Rig::new();
    closed.append(end_turn());
    assert!(
        !closed.ready(),
        "a bare reply still awaits completion evidence"
    );
    let compacting = Rig::new();
    compacting.append(end_turn());
    compacting.append(json!({"type":"system","subtype":"compact_boundary",
        "compactMetadata":{"trigger":"auto"}}));
    compacting.append(json!({"type":"user","isCompactSummary":true,
        "message":{"role":"user","content":"summary in progress"}}));
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    let mut notified = false;
    for record in tail() {
        closed.append(record.clone());
        compacting.append(record.clone());
        assert_eq!(
            observe_claude_jsonl_turn_state(&closed.path),
            TuiTurnState::Idle,
            "known metadata preserves the completed reply: {record}"
        );
        assert!(
            closed.ready(),
            "the same canonical completion proves readiness"
        );
        // Only the idle-compaction notice closes the compaction; until then it holds.
        notified |= record["subtype"] == "informational";
        if notified {
            assert_eq!(
                observe_claude_jsonl_turn_state(&compacting.path),
                TuiTurnState::Idle
            );
            assert!(
                compacting.ready(),
                "a closed compaction is not activity: {record}"
            );
            continue;
        }
        assert_ne!(
            observe_claude_jsonl_turn_state(&compacting.path),
            TuiTurnState::Idle,
            "an attachment cannot finish a compaction no notice closed: {record}"
        );
        held_without_keys(&compacting, &tmux);
    }
    let hard = Rig::new();
    hard.append(end_turn());
    hard.terminal();
    compacted_tail()
        .into_iter()
        .for_each(|record| hard.append(record));
    assert!(hard.ready(), "a hard terminal survives a closed compaction");
    append_completion(&compacting, Utc::now());
    assert_eq!(super::send_with_budget(&compacting), Ok(()));
    assert_eq!(tmux.sends(), ["load", "paste", "Enter"]);
}

#[test]
fn a_compaction_no_idle_notice_closed_or_that_work_follows_holds_both_readings() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let unclosed = |trigger: &str| {
        let mut records = compacted_tail();
        records.retain(|record| record["subtype"] != "informational");
        records[0]["compactMetadata"]["trigger"] = json!(trigger);
        records
    };
    let mut cases = vec![unclosed("auto"), unclosed("manual")];
    for work in [
        json!({"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"working"}]}}),
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool","content":"done"}]}}),
        json!({"type":"user","message":{"content":"next prompt"}}),
    ] {
        cases.push([compacted_tail(), vec![work]].concat());
    }
    for records in cases {
        let r = Rig::new();
        r.append(end_turn());
        records.iter().for_each(|record| r.append(record.clone()));
        assert!(!r.ready(), "{records:?}");
        let state = observe_claude_jsonl_turn_state(&r.path);
        assert_ne!(state, TuiTurnState::Idle, "{records:?}");
    }
}
