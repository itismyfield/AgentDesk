//! Rows are the ones Claude Code 2.1.325 wrote for a real `/model sonnet` (2026-11-14, session
//! ddaf32fb rows 6–7): the command row, then its output row.

use super::super::observed_prompt_decision::relay_observed_prompt_injected_prompt_decision;
use super::*;
use crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id;

const COMMAND_ROW: &str = r#"{"parentUuid":"8839f051-a055-4206-ba85-8f9e5cdc4676","isSidechain":false,"promptId":"96041549-30bd-4bc4-a807-f7fe446f60fc","type":"user","message":{"role":"user","content":"<command-name>/model</command-name>\n            <command-message>model</command-message>\n            <command-args>sonnet</command-args>"},"uuid":"6739634e-87ce-4901-8f0b-b95c3d85f505","timestamp":"2026-11-13T23:25:49.376Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/i8142-probe2/proj","sessionId":"ddaf32fb-dd8a-4336-b124-5683936cd3c4","version":"2.1.325","gitBranch":"HEAD"}"#;
const STDOUT_ROW: &str = r#"{"parentUuid":"6739634e-87ce-4901-8f0b-b95c3d85f505","isSidechain":false,"promptId":"96041549-30bd-4bc4-a807-f7fe446f60fc","type":"user","message":{"role":"user","content":"<local-command-stdout>Set model to `Sonnet 5.5` and saved as your default for new sessions</local-command-stdout>"},"uuid":"a3fdf681-fa83-47d9-9eeb-9e7c25449be1","timestamp":"2026-11-13T23:25:49.376Z","userType":"external","entrypoint":"sdk-cli","cwd":"/tmp/i8142-probe2/proj","sessionId":"ddaf32fb-dd8a-4336-b124-5683936cd3c4","version":"2.1.325","gitBranch":"HEAD"}"#;

const STDOUT_BODY: &str = "Set model to `Sonnet 5.5` and saved as your default for new sessions";

const STDOUT_PROMPT: &str = "<local-command-stdout>Set model to `Sonnet 5.5` and saved as your default for new sessions</local-command-stdout>";

/// One transcript row through the scanner's extraction and the relay's decision.
fn observed_control(row: &str) -> (String, String) {
    let row: serde_json::Value = serde_json::from_str(row).expect("transcript row");
    let (prompt, _) =
        extract_claude_transcript_user_prompt_with_entry_id(&row).expect("observable user row");
    let decision = relay_observed_prompt_injected_prompt_decision(&prompt);
    let control = decision
        .local_only_control
        .expect("local-only slash control");
    (control.kind, prompt)
}

fn note_for_row(tmux: &str, row: &str) -> String {
    let (kind, prompt) = observed_control(row);
    local_slash_control_note(tmux, &kind, &prompt)
}

/// The note every local-only row had before this issue.
fn kind_only_note(tmux: &str, kind: &str, prompt: &str) -> String {
    format_slash_command_control_note(tmux, kind, prompt)
}

#[test]
fn real_rows_are_the_command_and_its_output() {
    let (kind, prompt) = observed_control(COMMAND_ROW);
    assert_eq!(kind, "/model");
    assert!(prompt.starts_with("<command-name>/model</command-name>"));
    assert_eq!(
        observed_control(STDOUT_ROW),
        (
            "local-command-stdout".to_string(),
            STDOUT_PROMPT.to_string()
        )
    );
    let unpaired = kind_only_note("t", "local-command-stdout", STDOUT_PROMPT);
    assert!(
        unpaired.starts_with("-# ⚙️ 머신 슬래시 명령 (tmux: t)"),
        "{unpaired}"
    );
}

#[test]
fn output_row_is_shown_under_the_command_that_printed_it() {
    let tmux = "AgentDesk-claude-i8142-slash-paired";
    let (command_kind, command_prompt) = observed_control(COMMAND_ROW);

    assert_eq!(
        note_for_row(tmux, COMMAND_ROW),
        kind_only_note(tmux, &command_kind, &command_prompt)
    );
    assert_eq!(
        note_for_row(tmux, STDOUT_ROW),
        format!("-# ⚙️ /model\n```\n{STDOUT_BODY}\n```")
    );
    // The name is used once: a second output row has no command row before it.
    assert_eq!(
        note_for_row(tmux, STDOUT_ROW),
        kind_only_note(tmux, "local-command-stdout", STDOUT_PROMPT)
    );
}

#[test]
fn output_row_without_a_command_row_keeps_the_kind_only_note() {
    let tmux = "AgentDesk-claude-i8142-slash-unpaired";
    let unpaired = kind_only_note(tmux, "local-command-stdout", STDOUT_PROMPT);
    assert_eq!(note_for_row(tmux, STDOUT_ROW), unpaired);

    // Another session's command row names nothing here.
    note_for_row("AgentDesk-claude-i8142-slash-other", COMMAND_ROW);
    assert_eq!(note_for_row(tmux, STDOUT_ROW), unpaired);
}

#[test]
fn multi_line_output_keeps_its_lines_and_cannot_close_the_block() {
    let tmux = "AgentDesk-claude-i8142-slash-lines";
    local_slash_control_note(tmux, "/status", "<command-name>/status</command-name>");
    assert_eq!(
        local_slash_control_note(
            tmux,
            "local-command-stdout",
            "<local-command-stdout>\nmodel: sonnet\n  effort: low\n@everyone ```\n</local-command-stdout>",
        ),
        crate::domain::message_split::bounded_raw_code_block_notice(
            "-# ⚙️ /status",
            "model: sonnet\n  effort: low\n@everyone ```"
        )
    );
}

#[test]
fn command_name_that_is_not_a_plain_token_or_an_empty_output_keeps_the_kind_only_note() {
    let tmux = "AgentDesk-claude-i8142-slash-odd";
    let done = "<local-command-stdout>done</local-command-stdout>";
    for odd in ["/my_plugin_cmd", "/a`b", "/"] {
        local_slash_control_note(tmux, odd, "<command-name>x</command-name>");
        assert_eq!(
            local_slash_control_note(tmux, "local-command-stdout", done),
            kind_only_note(tmux, "local-command-stdout", done),
            "{odd}"
        );
    }
    let empty = "<local-command-stdout>  </local-command-stdout>";
    local_slash_control_note(tmux, "/model", "<command-name>/model</command-name>");
    assert_eq!(
        local_slash_control_note(tmux, "local-command-stdout", empty),
        kind_only_note(tmux, "local-command-stdout", empty)
    );
}
