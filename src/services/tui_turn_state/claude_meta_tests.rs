use super::*;

const PROMPT: &str = r#"{"type":"user","uuid":"u1","timestamp":"2026-10-22T02:30:59.000Z","permissionMode":"bypassPermissions","message":{"role":"user","content":"run the 90 second sleep probe"}}"#;
const ATTACHMENT: &str = r#"{"type":"attachment","uuid":"at1","timestamp":"2026-10-22T02:30:59.100Z","attachment":{"type":"todo_reminder"}}"#;
const TOOL_USE: &str = r#"{"type":"assistant","uuid":"a1","timestamp":"2026-10-22T02:31:02.007Z","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"python3 -c \"import time; time.sleep(90)\""}}],"stop_reason":"tool_use"}}"#;
const TOOL_RESULT: &str = r#"{"type":"user","uuid":"u2","timestamp":"2026-10-22T02:32:32.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}]}}"#;
const TEXT: &str = r#"{"type":"assistant","uuid":"a2","message":{"id":"msg_2","role":"assistant","content":[{"type":"text","text":"Starting the probe."}],"stop_reason":null}}"#;
const END_TURN: &str = r#"{"type":"assistant","uuid":"a3","message":{"id":"msg_3","role":"assistant","content":[{"type":"text","text":"DONE"}],"stop_reason":"end_turn"}}"#;
const STOP_HOOK: &str = r#"{"type":"system","subtype":"stop_hook_summary","uuid":"s1"}"#;
const TURN_DURATION: &str =
    r#"{"type":"system","subtype":"turn_duration","uuid":"s2","durationMs":93000}"#;
const RESULT: &str = r#"{"type":"result","result":"done","session_id":"s-prev"}"#;
const INTERRUPTED: &str = r#"{"type":"user","uuid":"u3","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
const LOCAL_COMMAND: &str = r#"{"type":"user","uuid":"u4","message":{"role":"user","content":"<local-command-stdout>Set model to Sonnet</local-command-stdout>"}}"#;
const PERMISSION_MODE: &str =
    r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s1"}"#;
const TORN_PERMISSION_MODE: &str = r#"{"type":"permission-mode","permissionMode":"bypassPer"#;
/// The bundle Claude appended after the probe's tool call, `permission-mode` the only marker.
const META: [&str; 5] = [
    r#"{"type":"last-prompt","lastPrompt":"run the 90 second sleep probe","sessionId":"s1"}"#,
    r#"{"type":"ai-title","aiTitle":"Sleep probe","sessionId":"s1"}"#,
    r#"{"type":"mode","mode":"normal","sessionId":"s1"}"#,
    PERMISSION_MODE,
    r#"{"type":"atis-latch","sessionId":"s1"}"#,
];
const META_WITHOUT_MARKER: [&str; 4] = [META[0], META[1], META[2], META[4]];

fn transcript(parts: &[&[&str]]) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), parts.concat().join("\n")).unwrap();
    file
}

fn read(file: &tempfile::NamedTempFile) -> (TuiTurnState, Option<TuiReadyState>) {
    let ready = jsonl_ready_for_input(
        &ProviderKind::Claude,
        Some(RuntimeHandoffKind::ClaudeTui),
        file.path(),
        None,
    );
    (observe_claude_jsonl_turn_state(file.path()), ready)
}

// A tool call still running reads as Streaming and busy through the session records Claude
// appends after it, wherever the marker sits and even when it is torn.
#[test]
fn a_running_tool_reads_as_streaming_past_trailing_session_metadata() {
    let cases: [(&str, &[&[&str]]); 5] = [
        ("probe order", &[&[PROMPT, ATTACHMENT, TOOL_USE], &META]),
        (
            "marker and latch",
            &[&[PROMPT, TOOL_USE, PERMISSION_MODE, META[4]]],
        ),
        ("bundle twice", &[&[PROMPT, TOOL_USE], &META, &META]),
        (
            "torn marker last",
            &[
                &[PROMPT, TOOL_USE],
                &META_WITHOUT_MARKER,
                &[TORN_PERMISSION_MODE],
            ],
        ),
        (
            "metadata before the reply",
            &[&[PROMPT], &META, &[TOOL_USE]],
        ),
    ];
    for (name, parts) in cases {
        let file = transcript(parts);
        assert_eq!(
            read(&file),
            (TuiTurnState::Streaming, Some(TuiReadyState::Busy)),
            "{name}"
        );
    }
}

// Past the marker nothing but a pending tool call is taken: a finished, interrupted or unstarted
// turn and a reply awaiting its stop hook stay Unknown, and the strict readers still see them done.
#[test]
fn session_metadata_never_uncovers_an_idle_or_submitted_turn() {
    let cases: [(&str, &[&[&str]], TuiTurnState, bool); 12] = [
        (
            "stop hook and duration",
            &[
                &[
                    PROMPT,
                    TOOL_USE,
                    TOOL_RESULT,
                    END_TURN,
                    STOP_HOOK,
                    TURN_DURATION,
                ],
                &META,
            ],
            TuiTurnState::Unknown,
            true,
        ),
        (
            "result",
            &[&[TOOL_USE, RESULT], &META],
            TuiTurnState::Unknown,
            true,
        ),
        (
            "torn marker after result",
            &[&[TOOL_USE, RESULT, TORN_PERMISSION_MODE]],
            TuiTurnState::Unknown,
            true,
        ),
        (
            "interrupt marker",
            &[&[PROMPT, TOOL_USE, INTERRUPTED], &META],
            TuiTurnState::Unknown,
            false,
        ),
        (
            "finished without marker",
            &[
                &[PROMPT, END_TURN, STOP_HOOK, TURN_DURATION],
                &META_WITHOUT_MARKER,
            ],
            TuiTurnState::Idle,
            true,
        ),
        (
            "reply before its stop hook",
            &[&[PROMPT, TOOL_USE, TOOL_RESULT, END_TURN], &META],
            TuiTurnState::Unknown,
            false,
        ),
        (
            "text block",
            &[&[PROMPT, TEXT], &META],
            TuiTurnState::Unknown,
            false,
        ),
        (
            "tool result",
            &[&[PROMPT, TOOL_USE, TOOL_RESULT], &META],
            TuiTurnState::Unknown,
            false,
        ),
        (
            "local command",
            &[&[TOOL_USE, RESULT, LOCAL_COMMAND, PERMISSION_MODE]],
            TuiTurnState::Unknown,
            false,
        ),
        (
            "new prompt",
            &[&[TOOL_USE, RESULT, PROMPT], &META],
            TuiTurnState::Unknown,
            false,
        ),
        (
            "marker alone",
            &[&[PERMISSION_MODE]],
            TuiTurnState::Unknown,
            false,
        ),
        ("metadata alone", &[&META], TuiTurnState::Unknown, false),
    ];
    for (name, parts, expected, done) in cases {
        let file = transcript(parts);
        let state = observe_claude_jsonl_turn_state(file.path());
        assert_eq!(state, expected, "{name}");
        if done {
            let len = std::fs::metadata(file.path()).unwrap().len();
            let strict = jsonl_ready_for_input(
                &ProviderKind::Claude,
                Some(RuntimeHandoffKind::ClaudeTui),
                file.path(),
                Some(len / 2),
            );
            assert_eq!(strict, Some(TuiReadyState::Ready), "{name}");
            assert!(
                jsonl_completion_scan_idle(&ProviderKind::Claude, file.path()),
                "{name}"
            );
        }
    }
    let empty = transcript(&[]);
    assert_eq!(
        observe_claude_jsonl_turn_state(empty.path()),
        TuiTurnState::Idle
    );
}
