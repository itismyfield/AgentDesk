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
// composer; submitted, idle, draft, finished and already-interrupted panes receive none.
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
    let rows: [(&str, &str, Transcript, ClaudeTuiInterruptPhase, usize); 14] = [
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
            UserSubmitted,
            0,
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
