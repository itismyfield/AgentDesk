//! Real Claude TUI pane captures through the tmux `!stop` entry.

use super::super::stop_host::tests::{Fixture, generating_turn, run};
use super::*;
use ClaudeTuiInterruptPhase::{ActiveGeneration, Ambiguous, PromptReady, UserSubmitted};

macro_rules! capture {
    ($name:literal) => {
        include_str!(concat!(
            "../../../../../tests/fixtures/tui_input/claude-2.1.293-",
            $name,
            ".txt"
        ))
    };
}

const MANUAL_FOOTER: &str = "⏸ manual mode on";
const BYPASS_FOOTER: &str = "⏵⏵ bypass permissions on";

const STREAMING_FOOTER: &str =
    include_str!("../../../../../tests/fixtures/tui_input/claude-2.1.311-c2-busy.txt");
const SHELL_FOOTER: &str =
    include_str!("../../../../../tests/fixtures/tui_input/claude-2.1.311-b2-busy.txt");

fn stop_decision(pane: &str) -> ClaudeStopDeliveryDecision {
    let (ready, active, draft) = claude_tui_stop_pane_evidence(pane);
    decide_claimed_claude_stop_delivery(
        ClaudeTurnInterruptDelivery::TuiEscape,
        classify_tui_interrupt_phase(Some(TuiTurnState::Streaming), ready || draft, active),
    )
}

#[test]
fn claude_tui_stop_delivers_escape_for_streaming_footer_without_spinner() {
    use crate::services::tmux_common as screen;
    assert_eq!(
        stop_decision(STREAMING_FOOTER),
        ClaudeStopDeliveryDecision::Deliver(ClaudeTurnInterruptDelivery::TuiEscape)
    );
    assert!(screen::tmux_capture_indicates_claude_tui_busy(
        STREAMING_FOOTER
    ));
    assert!(!screen::tmux_capture_indicates_claude_tui_ready_for_input(
        STREAMING_FOOTER
    ));
    let draft = STREAMING_FOOTER.replace("❯", "❯ queued follow-up");
    assert!(screen::tmux_capture_indicates_claude_tui_prompt_draft(
        &draft
    ));
    assert!(!screen::tmux_capture_indicates_claude_tui_idle_suggestion(
        &draft
    ));
    assert!(crate::services::tui_input::actor::gate::submission_busy(
        crate::services::tui_o::shadow::ShadowProvider::Claude,
        &draft
    ));
    assert_eq!(
        stop_decision(&draft),
        ClaudeStopDeliveryDecision::SkipAmbiguous
    );
}

#[test]
fn claude_tui_stop_delivers_escape_for_shell_interrupt_manage_footer() {
    for pane in [
        SHELL_FOOTER.to_owned(),
        SHELL_FOOTER.replace("1 shell", "2 shells"),
    ] {
        assert_eq!(
            stop_decision(&pane),
            ClaudeStopDeliveryDecision::Deliver(ClaudeTurnInterruptDelivery::TuiEscape)
        );
    }
}

#[test]
fn claude_tui_stop_rejects_quoted_interrupt_footer_above_composer() {
    let footer = STREAMING_FOOTER.lines().last().unwrap();
    let idle = STREAMING_FOOTER.replace(footer, "  ⏵⏵ bypass permissions on");
    for pane in [
        format!("⏺ The footer says esc to interrupt.\n{idle}"),
        format!("{footer}\n{idle}"),
        idle.replace("❯", &format!("❯ quote\n{footer}")),
        footer.to_owned(),
    ] {
        assert!(!crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(&pane));
        assert!(!crate::services::tui_input::actor::gate::submission_busy(
            crate::services::tui_o::shadow::ShadowProvider::Claude,
            &pane
        ));
        assert_eq!(
            stop_decision(&pane),
            ClaudeStopDeliveryDecision::SkipAmbiguous
        );
    }
}

// A draft's continuation rows sit two columns in, so a border and footer typed there are text;
// the composer's own border and the idle footer under it decide.
#[test]
fn claude_tui_busy_ignores_indented_border_and_footer_inside_the_draft() {
    use crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy as busy;
    let footer = STREAMING_FOOTER.lines().last().unwrap();
    let idle = STREAMING_FOOTER.replace(footer, "  ⏵⏵ bypass permissions on");
    let draft = idle.replace(
        "❯",
        "❯ draft\n  ──────────\n  ⏵⏵ bypass permissions on · esc to interrupt",
    );
    assert!(!busy(&draft));
    assert!(!crate::services::tui_input::actor::gate::submission_busy(
        crate::services::tui_o::shadow::ShadowProvider::Claude,
        &draft
    ));
    assert_eq!(
        stop_decision(&draft),
        ClaudeStopDeliveryDecision::SkipAmbiguous
    );
    assert!(busy(STREAMING_FOOTER));
    assert!(busy(SHELL_FOOTER));
}

#[derive(Clone, Copy, Debug)]
enum Transcript {
    Submitted,
    Streaming,
    Idle,
}

fn write_transcript(path: &std::path::Path, transcript: Transcript) {
    let user = serde_json::json!({"type": "user", "message": {"role": "user", "content": "go"}});
    let assistant = serde_json::json!({"type": "assistant", "message": {"content": []}});
    let done = serde_json::json!({"type": "system", "subtype": "turn_duration"});
    let lines = match transcript {
        Transcript::Submitted => vec![user],
        Transcript::Streaming => vec![user, assistant],
        Transcript::Idle => vec![user, assistant, done],
    };
    let body = lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    std::fs::write(path, body).unwrap();
}

// One Escape reaches a Claude turn whose pane shows spinner chrome over the painted empty
// composer, before its first reply and over an ended tail too; a submitted prompt without that
// chrome, idle, draft, finished and already-interrupted panes receive none.
#[test]
fn claude_tui_stop_escapes_only_a_pane_showing_live_turn_chrome() {
    let fx = Fixture::new();
    let prose = capture!("done").replace(
        "⏺ DONE",
        "⏺ DONE. The ping kept running while processing; thinking it over, nothing is running now.",
    );
    let foreground = capture!("tool-running")
        .lines()
        .filter(|line| !line.contains("Tomfoolering"))
        .collect::<Vec<_>>()
        .join("\n");
    let old_tool_done = format!("{foreground}\n⏺ DONE\n❯\n{MANUAL_FOOTER}");
    let old_tool_interrupted = format!(
        "{foreground}\n  ⎿ Interrupted · What should Claude do instead?\n❯\n{MANUAL_FOOTER}"
    );
    let rows: [(&str, &str, Transcript, ClaudeTuiInterruptPhase, usize); 15] = [
        (
            "ended-tail",
            capture!("thinking"),
            Transcript::Idle,
            ActiveGeneration,
            1,
        ),
        (
            "foreground-no-spinner",
            &foreground,
            Transcript::Streaming,
            ActiveGeneration,
            1,
        ),
        (
            "historical-tool-done",
            &old_tool_done,
            Transcript::Streaming,
            Ambiguous,
            0,
        ),
        (
            "historical-tool-interrupted",
            &old_tool_interrupted,
            Transcript::Streaming,
            Ambiguous,
            0,
        ),
        (
            "tool-running",
            capture!("tool-running"),
            Transcript::Streaming,
            ActiveGeneration,
            1,
        ),
        (
            "thinking",
            capture!("thinking"),
            Transcript::Streaming,
            ActiveGeneration,
            1,
        ),
        (
            "pre-generation",
            capture!("pre-generation"),
            Transcript::Submitted,
            ActiveGeneration,
            1,
        ),
        (
            "submitted",
            capture!("submitted"),
            Transcript::Submitted,
            UserSubmitted,
            0,
        ),
        ("idle", capture!("idle"), Transcript::Idle, PromptReady, 0),
        ("draft", capture!("draft"), Transcript::Idle, PromptReady, 0),
        (
            "stale-draft",
            capture!("draft"),
            Transcript::Streaming,
            Ambiguous,
            0,
        ),
        (
            "busy-draft",
            capture!("busy-draft"),
            Transcript::Streaming,
            Ambiguous,
            0,
        ),
        (
            "done",
            capture!("done"),
            Transcript::Streaming,
            Ambiguous,
            0,
        ),
        (
            "interrupted",
            capture!("interrupted"),
            Transcript::Streaming,
            Ambiguous,
            0,
        ),
        (
            "busy-words-prose",
            &prose,
            Transcript::Streaming,
            Ambiguous,
            0,
        ),
    ];
    run(async {
        for (footer, chrome) in [("manual", MANUAL_FOOTER), ("bypass", BYPASS_FOOTER)] {
            for (name, capture, transcript, phase, escapes) in rows {
                let pane = capture.replace(MANUAL_FOOTER, chrome);
                assert!(pane.contains(chrome), "{name}: footer chrome missing");
                let session = format!("AgentDesk-claude-i6686-{footer}-{name}");
                let (token, path) = generating_turn(&fx, &session);
                write_transcript(&path, transcript);
                fx.pane(&pane);
                let evidence = claude_tui_stop_pane_evidence(&pane);
                let (ready, active, draft) = evidence;
                let observed = classify_tui_interrupt_phase(
                    crate::services::tui_turn_state::runtime_binding_turn_state(
                        &ProviderKind::Claude,
                        &crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
                            &session,
                        )
                        .unwrap(),
                    ),
                    ready || draft,
                    active,
                );
                let _ = fx.take_calls();
                let outcome = interrupt_claude_native_turn(
                    &token,
                    session.clone(),
                    "!stop",
                    ClaudeStopTurnIdentity::capture(path.to_str().unwrap()).unwrap(),
                )
                .await;
                let sent = fx
                    .take_calls()
                    .iter()
                    .filter(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
                    .count();
                assert_eq!(sent, escapes, "{footer}/{name}: evidence={evidence:?}");
                assert_eq!(outcome.sent_keys, escapes == 1, "{footer}/{name}");
                assert_eq!(observed, phase, "{footer}/{name}: evidence={evidence:?}");
                crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
            }
        }
    });
}

// A token stop whose pane alone decides shares the draft and modal veto of the other stops.
#[test]
fn claude_tui_token_stop_spares_a_draft_and_a_modal_before_the_first_reply() {
    let fx = Fixture::new();
    let draft = STREAMING_FOOTER.replace("❯", "❯ queued follow-up");
    let modal = format!("  1. Allow once\n  2. Deny\n{STREAMING_FOOTER}");
    run(async {
        for (name, pane) in [("draft", &draft), ("modal", &modal)] {
            let session = format!("AgentDesk-claude-token-veto-{name}");
            let (token, path) = generating_turn(&fx, &session);
            write_transcript(&path, Transcript::Submitted);
            fx.pane(pane);
            let stop =
                interrupt_claude_turn_session_preserving(&token, Some(session.clone()), "!stop");
            assert!(!stop.await.sent_keys, "{name}");
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
        }
    });
}

// A session stop reads only the transcript O bound: once that file is replaced nothing is sent,
// whatever the pane shows.
#[test]
fn claude_tui_session_stop_refuses_a_replaced_source_under_a_live_pane() {
    use crate::services::tui_o::shadow::{SourceId, capture::file_identity};
    let fx = Fixture::new();
    fx.pane(STREAMING_FOOTER);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("parent.jsonl");
    write_transcript(&path, Transcript::Streaming);
    let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
    run(async {
        for (replaced, sent) in [(0, true), (1, false)] {
            let (session_id, path, ino) = ("parent".into(), path.clone(), ino + replaced);
            let source = SourceId {
                session_id,
                path,
                dev,
                ino,
            };
            let (channel_id, provider) = (1, ShadowProvider::Claude);
            let source = SourceBinding {
                channel_id,
                provider,
                source,
            };
            let session = "AgentDesk-claude-owned-stop".to_owned();
            let stop = interrupt_owned_claude_session(session, "!stop", Arc::new(|| true), source);
            assert_eq!(stop.await.sent_keys, sent, "replaced={replaced}");
        }
    });
}

#[test]
fn claude_tui_session_stop_keeps_cacheless_foreground_prompt_ready() {
    let fx = Fixture::new();
    let pane = capture!("tool-running")
        .lines()
        .filter(|line| !line.contains("Tomfoolering"))
        .collect::<Vec<_>>()
        .join("\n");
    let session = "AgentDesk-claude-cacheless-session-stop";
    fx.pane(&pane);
    assert!(
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session).is_none()
    );
    let (ready, active, _) = claude_tui_stop_pane_evidence(&pane);
    assert_eq!(
        classify_tui_interrupt_phase(None, ready, active),
        PromptReady
    );
    run(async {
        let outcome = interrupt_claude_session(
            None,
            Some(session.into()),
            "!stop",
            Some(Arc::new(|| true)),
            None,
        )
        .await;
        assert!(
            !outcome.sent_keys,
            "the existing session entry keeps its PromptReady policy"
        );
        assert!(
            !fx.take_calls()
                .iter()
                .any(|call| call.starts_with("send-keys") && call.ends_with("Escape"))
        );
    });
}

// A transcript whose 256 KiB tail read starts inside a multi-byte character still yields its turn.
#[test]
fn claude_stop_identity_reads_a_tail_cut_inside_a_character() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    let user = serde_json::json!({"type": "user", "uuid": "u", "message": {"role": "user", "content": "go"}});
    // Three-byte characters fill the cut point, so two of the three paddings cut one in half.
    for pad in 0..3 {
        let body = format!("{}{}\n{user}\n", "x".repeat(pad), "가".repeat(100_000));
        std::fs::write(&path, body).unwrap();
        let identity = ClaudeStopTurnIdentity::capture(path.to_str().unwrap());
        assert!(identity.is_some(), "pad={pad}");
    }
}

/// A Code Mode turn as Claude Code 2.1.328 writes it: the prompt, the turn's context
/// attachments, a `REPL` call that returns at once and the progress of the tool it started.
fn code_mode_turn(context_bytes: usize) -> String {
    use serde_json::json;
    let prompt = json!({"type":"user","uuid":"turn-b","sessionId":"s1","message":{"role":"user","content":"[User: tester (ID: 1)] Run a 90s foreground Bash."}});
    let context = json!({"type":"attachment","sessionId":"s1","attachment":{"type":"deferred_tools_delta","addedLines":["x".repeat(32 * 1024)]}});
    let call = json!({"type":"assistant","sessionId":"s1","message":{"content":[{"type":"tool_use","id":"t1","name":"REPL","input":{"code":"await Bash({command: 'sleep 90'})"}}]}});
    let returned = json!({"type":"user","sessionId":"s1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"started"}]}});
    let mut rows = vec![prompt];
    rows.extend(std::iter::repeat_n(
        context,
        context_bytes.div_ceil(32 * 1024),
    ));
    rows.extend([call, returned]);
    rows.extend((1..=5).map(|s| json!({"type":"progress","sessionId":"s1","data":{"type":"bash_progress","elapsedTimeSeconds":s}})));
    rows.iter().map(|row| format!("{row}\n")).collect()
}

// #8081: a Code Mode turn whose context pushed its prompt out of the last 256 KiB, and one whose
// reply row is half written inside a character, still yield that prompt's turn.
#[test]
fn claude_stop_identity_reads_a_code_mode_turn_running_or_generating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    let head = serde_json::json!({"type":"user","uuid":"turn-a","message":{"role":"user","content":"earlier"}});
    let running = format!("{head}\n{}", code_mode_turn(300 * 1024));
    let mut generating = format!("{head}\n{}", code_mode_turn(0)).into_bytes();
    let reply =
        r#"{"type":"assistant","sessionId":"s1","message":{"content":[{"type":"text","text":"작업"#;
    generating.extend_from_slice(&reply.as_bytes()[..reply.len() - 1]);
    let mut observed = Vec::new();
    for (name, body) in [
        ("tool running", running.into_bytes()),
        ("generating", generating),
    ] {
        std::fs::write(&path, &body).unwrap();
        let identity = ClaudeStopTurnIdentity::capture(path.to_str().unwrap());
        let current = identity
            .as_ref()
            .is_some_and(ClaudeStopTurnIdentity::still_current);
        let named = identity.is_some_and(|turn| format!("{turn:?}").contains(r#"Some("turn-b")"#));
        observed.push((name, named, current));
    }
    assert_eq!(
        observed,
        [("tool running", true, true), ("generating", true, true)]
    );
}

// A transcript with no prompt row anywhere, or no file, still leaves the turn unread.
#[test]
fn claude_stop_identity_without_a_prompt_row_stays_unread() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    let turn = code_mode_turn(300 * 1024);
    let (_, rest) = turn.split_once('\n').unwrap();
    std::fs::write(&path, rest).unwrap();
    assert_eq!(
        ClaudeStopTurnIdentity::capture(path.to_str().unwrap()),
        None
    );
    let missing = dir.path().join("missing.jsonl");
    assert_eq!(
        ClaudeStopTurnIdentity::capture(missing.to_str().unwrap()),
        None
    );
}
