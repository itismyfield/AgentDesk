use std::path::Path;

use serde_json::{Value, json};

use super::{Verdict, judge};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::binding_reader::source_id_for;

/// The captured model turn, one record per line, without a session_meta of its own.
fn captured() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_busy_inject");
    let text = std::fs::read_to_string(dir.join("model_turn.jsonl")).unwrap();
    text.lines().map(str::to_owned).collect()
}

fn meta(id: &str) -> String {
    json!({"type":"session_meta","payload":{"id":id,"cwd":"/tmp","source":"cli"}}).to_string()
}

struct Rollout {
    _dir: tempfile::TempDir,
    source: SourceId,
}

/// A rollout of `lines` written as given, then `tail` with no newline added.
fn rollout(lines: &[String], tail: &str) -> Rollout {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-takeover.jsonl");
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, text + tail).unwrap();
    let source = source_id_for("parent", &path).unwrap();
    Rollout { _dir: dir, source }
}

/// The session's own header followed by `lines`.
fn after_meta(lines: &[&String]) -> Vec<String> {
    std::iter::once(meta("parent"))
        .chain(lines.iter().map(|line| (*line).clone()))
        .collect()
}

fn edited(line: &str, edit: impl FnOnce(&mut Value)) -> String {
    let mut value: Value = serde_json::from_str(line).unwrap();
    edit(&mut value);
    value.to_string()
}

fn eligible_prompt_end(lines: &[String]) -> Option<u64> {
    match judge(&rollout(lines, "").source) {
        Verdict::Eligible(prefix) => prefix.prompt_end,
        other => panic!("expected eligible, got {other:?}"),
    }
}

/// The captured order task_started, turn_context, prompt, then its mirror and reasoning, stays
/// eligible with the prompt boundary named; the turn's first announced or sealed output ends it.
#[test]
fn a_captured_first_turn_stays_eligible_until_its_first_output() {
    let c = captured();
    let prompt_end =
        (meta("parent").len() + c[..4].iter().map(|l| l.len() + 1).sum::<usize>() + 1) as u64;
    assert_eq!(
        eligible_prompt_end(&after_meta(&[&c[0], &c[1], &c[2]])),
        None
    );
    let through_prompt = after_meta(&c[..7].iter().collect::<Vec<_>>());
    assert_eq!(eligible_prompt_end(&through_prompt), Some(prompt_end));
    let text = c[3].as_str();
    let text: Value = serde_json::from_str(text).unwrap();
    let text = text["payload"]["content"][0]["text"].clone();
    let user_message = json!({"type":"event_msg","payload":{"type":"user_message","message":text}});
    let mirrored = after_meta(&[&c[1], &c[3], &user_message.to_string()]);
    assert!(eligible_prompt_end(&mirrored).is_some());

    // The captured assistant body carries no payload.id, so it is an output without identity.
    let keyed = edited(&c[8], |body| body["payload"]["id"] = "msg_1".into());
    let outputs = [
        (&c[7], "an announced output"),
        (&c[8], "an output without identity"),
        (&keyed, "an output unit"),
    ];
    for (output, why) in outputs {
        let mut lines = through_prompt.clone();
        lines.push(output.clone());
        let verdict = judge(&rollout(&lines, "").source);
        assert!(
            matches!(&verdict, Verdict::Refuse(detail) if detail.contains(why)),
            "{verdict:?}"
        );
    }
}

/// Codex writes developer instructions and an environment_context user item before the prompt;
/// they do not make the real prompt a second one, but such an item after it does.
#[test]
fn the_environment_codex_writes_before_the_prompt_is_not_a_prompt() {
    let c = captured();
    let developer = json!({"type":"response_item","payload":{"type":"message","role":"developer",
        "content":[{"type":"input_text","text":"<permissions instructions>"}]}});
    let environment = json!({"type":"response_item","payload":{"type":"message","role":"user",
        "content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/tmp</cwd>\n</environment_context>"}]}});
    let world = json!({"type":"world_state","payload":{"full":true}});
    let (developer, environment, world) = (
        developer.to_string(),
        environment.to_string(),
        world.to_string(),
    );
    let real = [&c[1], &developer, &environment, &world, &c[2], &c[3], &c[4]];
    assert!(eligible_prompt_end(&after_meta(&real)).is_some());
    let late = after_meta(&[&c[1], &c[2], &c[3], &environment]);
    let verdict = judge(&rollout(&late, "").source);
    assert!(
        matches!(&verdict, Verdict::Refuse(detail) if detail.contains("a second user prompt")),
        "{verdict:?}"
    );
}

/// The AGENTS.md instructions Codex writes with its environment as one user record before the
/// prompt are not that prompt either.
#[test]
fn the_agents_md_record_codex_writes_before_the_prompt_is_not_a_prompt() {
    let c = captured();
    let agents = json!({"type":"response_item","payload":{"type":"message","role":"user",
        "content":[
            {"type":"input_text","text":"# AGENTS.md instructions\n\n<INSTRUCTIONS>\ndummy rule\n</INSTRUCTIONS>"},
            {"type":"input_text","text":"<environment_context>\n  <cwd>/tmp</cwd>\n</environment_context>"}
        ],
        "internal_chat_message_metadata_passthrough":{
            "content_item_kinds":["agents_md.instructions","environments.environment_context"]
        }}})
    .to_string();
    let lines = after_meta(&[&c[1], &agents, &c[2], &c[3], &c[4]]);
    let prompt_end = lines[..5].iter().map(|line| line.len() as u64 + 1).sum();
    assert_eq!(eligible_prompt_end(&lines), Some(prompt_end));
}

/// Every shape that may follow Legacy output, another turn or input, or an unjudgeable record
/// refuses the rollout; a torn tail or a missing header waits for the same rollout.
#[test]
fn takeover_prefix_refuses_output_other_turns_and_unmatched_input() {
    let c = captured();
    let (opener, prompt, mirror) = (&c[1], &c[3], &c[4]);
    let keyed = edited(&c[8], |body| body["payload"]["id"] = "msg_1".into());
    let aborted = json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t9"}});
    let other_turn = edited(mirror, |m| m["payload"]["turn_id"] = "other".into());
    let unnamed = edited(mirror, |m| m["payload"]["turn_id"] = Value::Null);
    let aborted = aborted.to_string();
    let malformed = "{\"type\":\"event_msg\"".to_string();
    let second_meta = meta("parent");
    let cases: [(&str, Vec<&String>); 14] = [
        ("an output unit", vec![opener, prompt, &keyed]),
        ("an announced output", vec![opener, prompt, &c[7]]),
        ("an output without identity", vec![opener, &c[8]]),
        ("a turn end", vec![opener, prompt, &c[19]]),
        ("a turn end", vec![opener, &aborted]),
        ("a second turn opener", vec![opener, prompt, opener]),
        ("a second user prompt", vec![opener, prompt, &c[11]]),
        ("before any prompt", vec![opener, mirror]),
        ("is not the first prompt", vec![opener, prompt, &c[12]]),
        ("a second user input", vec![opener, prompt, mirror, mirror]),
        ("no matching turn", vec![opener, prompt, &other_turn]),
        ("no matching turn", vec![opener, prompt, &unnamed]),
        ("malformed record", vec![opener, &malformed]),
        ("a second session_meta", vec![opener, &second_meta]),
    ];
    for (why, lines) in cases {
        let verdict = judge(&rollout(&after_meta(&lines), "").source);
        assert!(
            matches!(&verdict, Verdict::Refuse(detail) if detail.contains(why)),
            "{why}: {verdict:?}"
        );
    }
    let headless = judge(&rollout(&[opener.clone(), meta("parent")], "").source);
    assert!(matches!(headless, Verdict::Refuse(d) if d.contains("precedes the session_meta")));
    let foreign = judge(&rollout(&[meta("another")], "").source);
    assert!(matches!(foreign, Verdict::Refuse(d) if d.contains("identity mismatch")));

    let torn = judge(&rollout(&after_meta(&[opener, prompt]), "{\"type\":").source);
    assert!(matches!(torn, Verdict::Wait(d) if d.contains("inside a record")));
    assert!(matches!(judge(&rollout(&[], "").source), Verdict::Wait(_)));
    let replaced = rollout(&after_meta(&[opener]), "");
    let stale = SourceId {
        ino: replaced.source.ino + 1,
        ..replaced.source.clone()
    };
    assert!(matches!(judge(&stale), Verdict::Refuse(d) if d.contains("replaced")));
}

const CAPTURED_SESSION: &str = "01a0e15a-0000-7000-8000-000000000001";

/// A TUI rollout as Codex 0.147 wrote it from its session_meta to the first prompt's mirror,
/// with identifiers, paths and texts replaced.
fn captured_prefix() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_busy_inject");
    let text = std::fs::read_to_string(dir.join("takeover_prefix.jsonl")).unwrap();
    text.lines().map(str::to_owned).collect()
}

fn captured_rollout(lines: &[String]) -> Rollout {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-captured.jsonl");
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, text).unwrap();
    let source = source_id_for(CAPTURED_SESSION, &path).unwrap();
    Rollout { _dir: dir, source }
}

fn with_text(line: &str, text: &str) -> String {
    edited(line, |record| match record["payload"]["type"].as_str() {
        Some("item_completed") => record["payload"]["item"]["content"][0]["text"] = text.into(),
        _ => record["payload"]["content"][0]["text"] = text.into(),
    })
}

/// Every record boundary of the captured prefix is eligible, the prompt boundary named once the
/// prompt is written; its environment item is not that prompt.
#[test]
fn a_captured_rollout_is_eligible_through_its_first_prompt_mirror() {
    let lines = captured_prefix();
    assert_eq!(lines.len(), 10);
    for end in 1..=lines.len() {
        let verdict = judge(&captured_rollout(&lines[..end]).source);
        let Verdict::Eligible(prefix) = verdict else {
            panic!("{end} records: {verdict:?}");
        };
        assert_eq!(prefix.prompt_end.is_some(), end > 8, "{end} records");
    }
}

/// An environment block cut short or followed by text is the prompt, so the real one is a second;
/// one written after the prompt is a second prompt, and a mirror of other text matches nothing.
#[test]
fn a_captured_rollout_refuses_what_only_resembles_its_environment_or_mirror() {
    let lines = captured_prefix();
    let (environment, prompt, mirror) = (5, 8, 9);
    let mut partial = lines.clone();
    partial[environment] = with_text(&lines[environment], "<environment_context>\n  <cwd>/work");
    let mut outside = lines.clone();
    let trailed = "<environment_context>\n</environment_context>\nAlso list the files.";
    outside[environment] = with_text(&lines[environment], trailed);
    let mut late = lines.clone();
    let moved = late.remove(environment);
    late.insert(prompt, moved);
    let mut mismatched = lines.clone();
    mismatched[mirror] = with_text(&lines[mirror], "Summarize another rollout.");
    let cases = [
        ("a second user prompt", partial),
        ("a second user prompt", outside),
        ("a second user prompt", late),
        ("is not the first prompt", mismatched),
    ];
    for (why, lines) in cases {
        let verdict = judge(&captured_rollout(&lines).source);
        assert!(
            matches!(&verdict, Verdict::Refuse(detail) if detail.contains(why)),
            "{why}: {verdict:?}"
        );
    }
}
