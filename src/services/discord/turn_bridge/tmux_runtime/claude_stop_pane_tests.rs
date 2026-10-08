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
    let rows: [(&str, &str, Transcript, ClaudeTuiInterruptPhase, usize); 11] = [
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
                let outcome = interrupt_claude_turn_session_preserving(
                    &token,
                    Some(session.clone()),
                    "!stop",
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
