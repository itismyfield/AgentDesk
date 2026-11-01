use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;

/// Redacted records of a Claude 2.1.316 run under the production launch shape, keyed by case.
const FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/hook_payload/claude-2.1.316.transcript-prefix.jsonl"
));
const SESSION: &str = "00000000-0000-4000-8000-000000000316";

fn records(case: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in FIXTURE.lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        if row["case"] == case {
            bytes.extend(serde_json::to_vec(&row["record"]).unwrap());
            bytes.push(b'\n');
        }
    }
    assert!(!bytes.is_empty(), "fixture case {case}");
    bytes
}

fn transcript(dir: &tempfile::TempDir, bytes: &[u8]) -> SourceId {
    let path = dir.path().join("t.jsonl");
    std::fs::write(&path, bytes).unwrap();
    source_id_for(SESSION, &path).unwrap()
}

fn eligible(verdict: Verdict) -> Prefix {
    match verdict {
        Verdict::Eligible(prefix) => prefix,
        other => panic!("{other:?}"),
    }
}

/// A fresh session's records through its first prompt, and a cleared session's, are taken; the
/// first assistant row refuses.
#[test]
fn captured_claude_prefixes_are_eligible_until_the_first_output() {
    let dir = tempfile::tempdir().unwrap();
    for case in ["fresh_before_output", "cleared_before_output"] {
        let bytes = records(case);
        let prefix = eligible(judge(&transcript(&dir, &bytes)));
        assert_eq!(prefix.len, bytes.len() as u64, "{case}");
        assert!(prefix.prompt_end.is_some(), "{case} holds its first prompt");
    }
    let output = [
        records("fresh_before_output"),
        records("fresh_first_output"),
    ]
    .concat();
    assert!(
        matches!(judge(&transcript(&dir, &output)), Verdict::Refuse(d) if d.starts_with("output"))
    );
}

/// Before its first prompt a cleared session holds only its `/clear` echo and metadata.
#[test]
fn a_cleared_session_before_its_prompt_is_taken_without_a_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let all = records("cleared_before_output");
    let lines: Vec<&[u8]> = all.split_inclusive(|b| *b == b'\n').collect();
    let before: Vec<u8> = lines[..5].concat();
    assert_eq!(eligible(judge(&transcript(&dir, &before))).prompt_end, None);
}

/// Another session's record, a second prompt, a record naming no session and a torn line are not
/// taken; a file of sessionless records alone waits for proof of its session.
#[test]
fn claude_prefix_refuses_foreign_or_extra_input_and_waits_on_partial_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let prompted = records("fresh_before_output");
    let foreign = String::from_utf8(prompted.clone())
        .unwrap()
        .replace(SESSION, "other");
    let refused = |bytes: &[u8]| matches!(judge(&transcript(&dir, bytes)), Verdict::Refuse(_));
    assert!(refused(foreign.as_bytes()));
    assert!(refused(&[prompted.clone(), prompted.clone()].concat()));
    assert!(refused(b"{\"type\":\"mode\"}\n"));
    let torn = &prompted[..prompted.len() - 5];
    assert!(matches!(judge(&transcript(&dir, torn)), Verdict::Wait(_)));
    let sessionless = br#"{"type":"file-history-snapshot"}"#;
    let wait = judge(&transcript(&dir, &[&sessionless[..], b"\n"].concat()));
    assert!(matches!(wait, Verdict::Wait(_)));
}

/// What a tmux kill or an in-CLI `/clear` appends to the old transcript is metadata; `/exit`'s
/// command rows are not.
#[test]
fn old_transcript_suffixes_are_metadata_only_for_both_clears() {
    let dir = tempfile::tempdir().unwrap();
    let head = [
        records("fresh_before_output"),
        records("fresh_first_output"),
    ]
    .concat();
    for (case, metadata) in [
        ("suffix_after_tmux_kill", true),
        ("suffix_after_cli_clear", true),
        ("suffix_after_cli_exit", false),
    ] {
        let source = transcript(&dir, &[head.clone(), records(case)].concat());
        let tail = metadata_since(&source, head.len() as u64);
        assert_eq!(tail.is_ok(), metadata, "{case}: {tail:?}");
    }
    let source = transcript(&dir, &head);
    assert!(
        metadata_since(&source, 0).is_err(),
        "the assistant row is output"
    );
}
