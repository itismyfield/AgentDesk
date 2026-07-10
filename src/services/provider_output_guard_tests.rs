use super::provider::ProviderKind;
use super::provider_output_guard::{
    BLOCKED_PROVIDER_OUTPUT_BODY, HELD_PROVIDER_OUTPUT_BODY, ProviderOutputKind,
    ProviderOutputVerdict, inspect_provider_output, inspect_provider_streaming_output,
    safe_blocked_body, safe_held_body,
};

const INCIDENT_TASK_LEAK_4371: &str = r#"다음 Wave 스카우트를 진행합니다.
user[SYSTEM NOTIFICATION - NOT USER INPUT]
This is an automated background-task event, NOT a message from the user.
<task-notification>
<task-id>b3zsyu13i</task-id>
<tool-use-id>toolu_01CmQ5J8p9AWTawCSCLxNVSm</tool-use-id>
<output-file>/private/tmp/claude-501/tasks/b3zsyu13i.output</output-file>
<status>completed</status>
</task-notification>"#;

const INCIDENT_TOOL_WRAPPER_4371: &str = r#"검증을 완료했습니다.
</parameter>
</invoke>"#;

#[test]
fn invariant_4371_system_notification_with_private_fields_is_blocked() {
    let verdict = inspect_provider_output(&ProviderKind::Claude, INCIDENT_TASK_LEAK_4371);
    assert_eq!(
        verdict,
        ProviderOutputVerdict::Blocked {
            kind: ProviderOutputKind::ClaudeSystemNotification,
        }
    );
    let ProviderOutputVerdict::Blocked { kind } = verdict else {
        panic!("incident fixture must be blocked");
    };
    let safe = safe_blocked_body(kind);
    assert_eq!(safe, BLOCKED_PROVIDER_OUTPUT_BODY);
    for secret in [
        "[SYSTEM NOTIFICATION",
        "<task-notification>",
        "<task-id>",
        "<tool-use-id>",
        "<output-file>",
        "/private/tmp/",
        "b3zsyu13i",
    ] {
        assert!(!safe.contains(secret), "safe body leaked {secret}");
    }
}

#[test]
fn invariant_4371_tool_wrapper_terminal_compound_is_blocked() {
    assert_eq!(
        inspect_provider_output(&ProviderKind::Claude, INCIDENT_TOOL_WRAPPER_4371),
        ProviderOutputVerdict::Blocked {
            kind: ProviderOutputKind::ClaudeToolWrapper,
        }
    );
    assert_eq!(
        inspect_provider_output(
            &ProviderKind::Claude,
            "prose <invoke name=\"Bash\"> then <parameter name=\"cmd\">"
        ),
        ProviderOutputVerdict::Blocked {
            kind: ProviderOutputKind::ClaudeToolWrapper,
        }
    );
}

#[test]
fn invariant_4371_compound_matching_does_not_broaden_to_lone_markers() {
    for explanation in [
        "The literal <task-notification> tag names a structured event.",
        "The banner [SYSTEM NOTIFICATION - NOT USER INPUT] is discussed here.",
        "A malformed report might contain <invoke name= without parameters.",
        "A lone </parameter> closer is insufficient evidence by itself.",
    ] {
        assert_eq!(
            inspect_provider_output(&ProviderKind::Claude, explanation),
            ProviderOutputVerdict::Clean,
            "legitimate explanation was over-blocked: {explanation}"
        );
    }
}

#[test]
fn invariant_4371_markdown_code_is_excluded_from_detection() {
    for example in [
        format!("```text\n{INCIDENT_TASK_LEAK_4371}\n```"),
        format!("Use `{}` only as an example.", "</parameter></invoke>"),
        format!("~~~xml\n{INCIDENT_TOOL_WRAPPER_4371}\n~~~"),
    ] {
        assert_eq!(
            inspect_provider_output(&ProviderKind::Claude, &example),
            ProviderOutputVerdict::Clean,
            "code example was over-blocked"
        );
    }
}

#[test]
fn invariant_4371_claude_fingerprints_do_not_apply_to_other_providers() {
    for provider in [
        ProviderKind::Codex,
        ProviderKind::Gemini,
        ProviderKind::Qwen,
        ProviderKind::OpenCode,
    ] {
        assert_eq!(
            inspect_provider_output(&provider, INCIDENT_TASK_LEAK_4371),
            ProviderOutputVerdict::Clean,
            "wrong-provider output was suppressed: {provider:?}"
        );
    }
}

#[test]
fn invariant_4371_streaming_partial_anchor_is_held_fail_closed() {
    for partial in [
        "answer [SYSTEM NOTIF",
        "answer <task-notif",
        "answer </param",
        "answer <",
    ] {
        let ProviderOutputVerdict::Hold { kind } =
            inspect_provider_streaming_output(&ProviderKind::Claude, partial)
        else {
            panic!("partial control marker was not held: {partial}");
        };
        assert_eq!(kind, ProviderOutputKind::PartialControlMarker);
        assert_eq!(safe_held_body(kind), HELD_PROVIDER_OUTPUT_BODY);
    }
}

#[test]
fn invariant_4371_streaming_holds_a_lone_complete_anchor_but_terminal_allows_it() {
    let explanation = "The literal <task-notification> tag is being explained.";
    assert_eq!(
        inspect_provider_output(&ProviderKind::Claude, explanation),
        ProviderOutputVerdict::Clean
    );
    assert_eq!(
        inspect_provider_streaming_output(&ProviderKind::Claude, explanation),
        ProviderOutputVerdict::Hold {
            kind: ProviderOutputKind::PartialControlMarker,
        }
    );
}
