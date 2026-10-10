use super::*;
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};

const IDLE: &str = include_str!("../../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt");
const BUSY: &str = "✳ Architecting…";

#[derive(Clone, Copy, Debug)]
enum Entry {
    Fresh,
    Followup,
    ProvenWarm,
    Steering,
}

const ENTRIES: [Entry; 4] = [
    Entry::Fresh,
    Entry::Followup,
    Entry::ProvenWarm,
    Entry::Steering,
];

fn submit_text(entry: Entry, session: &str, prompt: &str) -> Result<(), String> {
    // Hosted dispatch supplies a token; its polling path stays on the thread owning this spy.
    let token = CancelToken::new();
    let token = Some(&token);
    match entry {
        Entry::Fresh => send_fresh_prompt(session, prompt, token),
        Entry::Followup => send_followup_prompt(session, prompt, token),
        Entry::Steering => inject_steering_prompt(session, prompt),
        Entry::ProvenWarm => {
            let transcript = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(
                transcript.path(),
                "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
            )
            .unwrap();
            send_followup_prompt_or_idle_transcript(session, prompt, token, transcript.path())
        }
    }
}

#[test]
fn claude_default_entries_deliver_lines_beyond_the_composer_rows_unchanged() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    // At 80x24 each needs 15 to 22 typed rows yet stays under the 800-char paste fold.
    let lines = [
        "가".repeat(533),
        "가".repeat(800),
        format!("{}가", "가 ".repeat(399)),
    ];
    for prompt in lines {
        for entry in ENTRIES {
            let session = format!("claude-r5-default-{}", uuid::Uuid::new_v4());
            let spy = SpyGuard::install(SpyState {
                pane_size: Some((80, 24)),
                captures: std::iter::repeat_n(Some(IDLE.to_string()), 16).collect(),
                captures_after_send: Some(std::iter::repeat_n(Some(BUSY.to_string()), 4).collect()),
                ..SpyState::default()
            });
            let result = submit_text(entry, &session, &prompt);
            let calls = spy.calls();
            let count = |name: &str| calls.iter().filter(|c| *c == name).count();
            assert_eq!(result, Ok(()), "{entry:?}: {calls:?}");
            assert_eq!(
                count(&format!("literal:{prompt}")),
                1,
                "{entry:?}: {calls:?}"
            );
            assert_eq!(count("keys:Enter"), 1, "{entry:?}: {calls:?}");
            assert!(
                !calls
                    .iter()
                    .any(|c| c.starts_with("load:") || c.starts_with("paste:")),
                "{entry:?}: {calls:?}"
            );
        }
    }
}
