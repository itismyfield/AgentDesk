use std::io::Write;

use serde_json::{Value, json};

use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::file_identity;

fn bind(path: &std::path::Path, provider: ShadowProvider) -> SourceBinding {
    let (dev, ino) = file_identity(&std::fs::metadata(path).unwrap());
    let session_id = "parent".into();
    let source = SourceId {
        session_id,
        path: path.into(),
        dev,
        ino,
    };
    SourceBinding {
        channel_id: 61,
        provider,
        source,
    }
}

fn lines(path: &std::path::Path, lines: &[String]) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

fn header(root: &std::path::Path) -> String {
    json!({"type":"session_meta","payload":{"id":"parent","cwd":root,"source":"cli","originator":"codex-tui"}}).to_string()
}

fn event(kind: &str, id: &str) -> String {
    json!({"type":"event_msg","payload":{"type":kind,"turn_id":id}}).to_string()
}

/// A JSON record whose output block O cannot type; the reader may resume after it.
fn blocked() -> String {
    json!({"type":"response_item","payload":{"type":"unknown_item","id":"x"}}).to_string()
}

fn assistant(id: &str) -> String {
    json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":id,"content":[{"type":"output_text","text":"done"}]}}).to_string()
}

/// A record cut short; it could have been any record, an opener included.
fn broken(kind: &str, id: &str) -> String {
    let full = event(kind, id);
    full[..full.len() - 3].to_string()
}

fn open_turn(id: &str) -> TurnState {
    TurnState::Open {
        native_turn_id: Some(id.into()),
    }
}

/// Reads to the end in `budget`-byte polls, resuming past each typed `Blocked` record.
fn replay(binding: &SourceBinding, budget: u64) -> Result<(TurnState, bool), String> {
    let len = std::fs::metadata(&binding.source.path).unwrap().len();
    let mut facts = InputFacts::open(binding.clone())?;
    loop {
        match facts.poll(budget) {
            Ok(fact) if fact.through == len => return Ok((fact.state, facts.awaiting_boundary())),
            Ok(_) => {}
            Err(error) => {
                let resume = facts.resume_point().cloned().ok_or(error)?;
                facts = InputFacts::resume(binding.clone(), &resume)?;
            }
        }
    }
}

/// Every split of the same bytes, one batch or one byte per poll, reaches the same state.
fn replay_all(binding: &SourceBinding) -> Result<(TurnState, bool), String> {
    let whole = replay(binding, u64::MAX);
    assert_eq!(replay(binding, 1), whole);
    whole
}

/// A typed Blocked record keeps the turn it interrupted: a foreign abort cannot close it, and
/// its own closer or a new opener ends the wait.
#[test]
fn resumed_reader_keeps_the_carried_turn_until_its_own_boundary() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    lines(&path, &[header(root.path())]);
    let binding = bind(&path, ShadowProvider::Codex);
    lines(
        &path,
        &[
            event("task_started", "A"),
            blocked(),
            event("turn_aborted", "B"),
        ],
    );
    assert_eq!(replay_all(&binding), Ok((TurnState::Unknown, true)));
    lines(&path, &[event("turn_aborted", "A")]);
    assert_eq!(replay_all(&binding), Ok((TurnState::Idle, false)));
    // A turn opened after the first boundary is the one a second Blocked record carries.
    lines(
        &path,
        &[
            event("task_started", "C"),
            blocked(),
            event("turn_aborted", "B"),
        ],
    );
    assert_eq!(replay_all(&binding), Ok((TurnState::Unknown, true)));
    lines(&path, &[event("task_complete", "C")]);
    assert_eq!(replay_all(&binding), Ok((TurnState::Idle, false)));
    lines(&path, &[blocked(), event("task_started", "D")]);
    assert_eq!(replay_all(&binding), Ok((open_turn("D"), false)));
}

/// A second Blocked record before any boundary still carries the first turn.
#[test]
fn a_repeated_blocked_record_keeps_the_first_carried_turn() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    lines(&path, &[header(root.path())]);
    let binding = bind(&path, ShadowProvider::Codex);
    let records = [event("task_started", "A"), blocked(), blocked()];
    lines(&path, &records);
    let mut facts = InputFacts::open(binding.clone()).unwrap();
    assert!(facts.poll(u64::MAX).is_err());
    let first = facts.resume_point().cloned().unwrap();
    assert_eq!(first.carried, open_turn("A"));
    let mut resumed = InputFacts::resume(binding.clone(), &first).unwrap();
    assert!(resumed.poll(u64::MAX).is_err());
    let second = resumed.resume_point().cloned().unwrap();
    assert!(second.after > first.after);
    assert_eq!(second.carried, open_turn("A"));
    lines(&path, &[event("turn_aborted", "B")]);
    assert_eq!(replay_all(&binding), Ok((TurnState::Unknown, true)));
    lines(&path, &[event("task_complete", "A")]);
    assert_eq!(replay_all(&binding), Ok((TurnState::Idle, false)));
}

/// Claude's strict end names no turn, so it closes whatever was carried; a later prompt opens.
#[test]
fn an_unnamed_close_ends_the_wait_and_a_prompt_after_it_opens() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    let unsupported = json!({"type":"assistant","uuid":"x","apiBlockIndex":0,
        "message":{"id":"m","content":[{"type":"image"}]}});
    let end = json!({"type":"system","subtype":"turn_duration"});
    let prompt = json!({"type":"user","uuid":"next","message":{"content":"again"}});
    let first = json!({"type":"user","uuid":"a","message":{"content":"go"}});
    lines(&path, &[first.to_string(), unsupported.to_string()]);
    let binding = bind(&path, ShadowProvider::Claude);
    assert_eq!(replay_all(&binding), Ok((TurnState::Unknown, true)));
    lines(&path, &[end.to_string()]);
    assert_eq!(replay_all(&binding), Ok((TurnState::Idle, false)));
    lines(
        &path,
        &[unsupported.to_string(), end.to_string(), prompt.to_string()],
    );
    assert_eq!(replay_all(&binding), Ok((open_turn("next"), false)));
}

/// A user row whose tool result cannot be typed drops the prompt after it; nothing it held opens.
#[test]
fn a_mixed_user_row_with_an_untyped_tool_result_stays_unknown() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    let end = json!({"type":"system","subtype":"turn_duration"});
    let mixed = json!({"type":"user","uuid":"mixed","message":{"content":[
        {"type":"tool_result","content":"no id"},{"type":"text","text":"new prompt"}]}});
    lines(&path, &[end.to_string(), mixed.to_string()]);
    let binding = bind(&path, ShadowProvider::Claude);
    let mut facts = InputFacts::open(binding.clone()).unwrap();
    assert!(facts.poll(u64::MAX).is_err());
    assert_eq!(facts.resume_point().unwrap().carried, TurnState::Idle);
    assert_eq!(replay_all(&binding), Ok((TurnState::Unknown, true)));
}

/// A cut-short record may hide an opener, so it halts every reader with no resume point: on the
/// first read, on a reread from the start, and after an earlier turn closed cleanly.
#[test]
fn a_broken_json_record_halts_without_a_resume_point() {
    let sequences = [
        vec![
            broken("task_started", "A"),
            event("turn_aborted", "B"),
            assistant("A"),
        ],
        vec![
            event("task_started", "A"),
            broken("task_started", "C"),
            event("task_complete", "A"),
            assistant("C"),
        ],
    ];
    for (n, sequence) in sequences.iter().enumerate() {
        for earlier in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("parent.jsonl");
            lines(&path, &[header(root.path())]);
            if earlier {
                lines(
                    &path,
                    &[event("task_started", "Z"), event("task_complete", "Z")],
                );
            }
            let binding = bind(&path, ShadowProvider::Codex);
            let mut first = InputFacts::open(binding.clone()).unwrap();
            if earlier {
                assert_eq!(first.poll(u64::MAX).unwrap().state, TurnState::Idle);
            }
            lines(&path, sequence);
            let len = std::fs::metadata(&path).unwrap().len();
            assert!(
                first.poll(u64::MAX).is_err(),
                "sequence {n} earlier={earlier}"
            );
            assert_eq!(first.resume_point(), None);
            for budget in [u64::MAX, 1] {
                let mut reread = InputFacts::open(binding.clone()).unwrap();
                let halted = loop {
                    match reread.poll(budget) {
                        Ok(fact) => assert!(fact.through < len, "sequence {n}"),
                        Err(error) => break error,
                    }
                };
                assert!(!halted.is_empty());
                assert_eq!(reread.resume_point(), None);
            }
        }
    }
}

/// Only a row that does not itself open a turn, or a record with no fact, leaves no evidence.
#[test]
fn turn_evidence_counts_every_fact_but_a_row_that_opens_no_turn() {
    let root = tempfile::tempdir().unwrap();
    let summary = json!({"type":"summary","summary":"earlier"});
    let meta = json!({"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>"}});
    let anonymous = json!({"type":"assistant","apiBlockIndex":0,
        "message":{"id":"m","content":[{"type":"text","text":"hi"}]}});
    let end = json!({"type":"system","subtype":"turn_duration"});
    let result = json!({"type":"user","uuid":"r","message":{"content":[
        {"type":"tool_result","tool_use_id":"t","content":"ok"}]}});
    let prompt = json!({"type":"user","uuid":"p","message":{"content":"go"}});
    let cases: [(&str, Vec<&Value>, bool, TurnState); 6] = [
        ("metadata", vec![&summary], false, TurnState::Unknown),
        (
            "meta prompt",
            vec![&summary, &meta],
            false,
            TurnState::Unknown,
        ),
        (
            "anonymous assistant",
            vec![&anonymous],
            true,
            TurnState::Unknown,
        ),
        ("strict end", vec![&end], true, TurnState::Idle),
        ("tool result", vec![&result], true, TurnState::Unknown),
        (
            "native prompt",
            vec![&summary, &prompt],
            true,
            open_turn("p"),
        ),
    ];
    for (name, records, evidence, state) in cases {
        let path = root.path().join(format!("{name}.jsonl"));
        let records: Vec<String> = records.iter().map(|r| r.to_string()).collect();
        lines(&path, &records);
        let mut facts = InputFacts::open(bind(&path, ShadowProvider::Claude)).unwrap();
        let read = facts.poll(u64::MAX).unwrap().state;
        assert_eq!(
            (read, facts.saw_turn_evidence()),
            (state, evidence),
            "{name}"
        );
    }
    // Codex records user input as a row that opens no turn; only its task start is evidence.
    let path = root.path().join("codex.jsonl");
    let user = json!({"type":"response_item","payload":{"type":"message","role":"user",
        "content":[{"type":"input_text","text":"hi"}]}});
    lines(&path, &[header(root.path()), user.to_string()]);
    let mut facts = InputFacts::open(bind(&path, ShadowProvider::Codex)).unwrap();
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Unknown);
    assert!(!facts.saw_turn_evidence());
    lines(&path, &[event("task_started", "A")]);
    assert_eq!(facts.poll(u64::MAX).unwrap().state, open_turn("A"));
    assert!(facts.saw_turn_evidence());
}
