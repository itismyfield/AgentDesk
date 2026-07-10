use super::super::{WatcherToolState, process_watcher_lines};
use crate::services::discord::formatting::format_for_discord_with_provider;
use crate::services::provider::ProviderKind;
use crate::services::provider_output_guard::BLOCKED_PROVIDER_OUTPUT_BODY;
use crate::services::session_backend::StreamLineState;

const INCIDENT_ASSISTANT_TEXT_4371: &str = r#"다음 Wave 스카우트를 진행합니다.
user[SYSTEM NOTIFICATION - NOT USER INPUT]
This is an automated background-task event, NOT a message from the user.
<task-notification>
<task-id>b3zsyu13i</task-id>
<tool-use-id>toolu_01CmQ5J8p9AWTawCSCLxNVSm</tool-use-id>
<output-file>/private/tmp/claude-501/tasks/b3zsyu13i.output</output-file>
<status>completed</status>
</task-notification>"#;

#[test]
fn invariant_4371_raw_claude_jsonl_reaches_last_mile_guard_without_leaking() {
    let mut buffer = serde_json::json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": INCIDENT_ASSISTANT_TEXT_4371}]
        }
    })
    .to_string();
    buffer.push('\n');
    let mut state = StreamLineState::new();
    let mut full_response = String::new();
    let mut tool_state = WatcherToolState::new();

    let outcome =
        process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);
    assert!(outcome.assistant_text_seen);
    assert!(full_response.contains("<output-file>"));

    let discord = format_for_discord_with_provider(&full_response, &ProviderKind::Claude);
    assert_eq!(discord, BLOCKED_PROVIDER_OUTPUT_BODY);
    for forbidden in [
        "[SYSTEM NOTIFICATION",
        "<task-notification>",
        "<task-id>",
        "<tool-use-id>",
        "<output-file>",
        "/private/tmp/",
    ] {
        assert!(!discord.contains(forbidden), "Discord leaked {forbidden}");
    }
}
