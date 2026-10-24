use super::{class, safe_tool_summary};
use serde_json::json;

fn bash(input: serde_json::Value) -> Option<String> {
    safe_tool_summary("Bash", &input.to_string(), None)
}

#[test]
fn unknown_names_are_bounded_and_arguments_stay_hidden() {
    for name in [
        "ToolSearch",
        "CronDelete",
        "CronCreate",
        "task-runner",
        "custom_tool-v2.run:one",
    ] {
        assert_eq!(class(name), name);
        assert!(safe_tool_summary(name, r#"{"description":"ARGUMENT_SENTINEL"}"#, None).is_none());
    }
    assert_eq!(class(&"N".repeat(41)), "N".repeat(40));
    assert_eq!(class("functions.ToolSearch"), "ToolSearch");
    assert_eq!(
        class(&format!("functions.{}", "N".repeat(41))),
        "N".repeat(40)
    );
}

#[test]
fn unknown_names_reject_unsafe_bytes_even_after_the_visible_prefix() {
    for name in [
        "ToolSearch @everyone",
        "@here",
        "https://example.test",
        "`command`",
        "two words",
        "line\nnext",
        "tab\tname",
        "control\u{7f}",
        "도구",
        "ghp_ShortLeak9",
        "x_ghp_ShortLeak9",
        "functions.x_ghp_ShortLeak9",
        "",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([format!("{}@", "N".repeat(40))])
    {
        assert_eq!(class(&name), "Tool", "unsafe name: {name:?}");
    }
}

#[test]
fn bash_description_precedes_command_and_uses_task_sanitization() {
    let description = "Inspect @everyone `source` [progress]\nSECOND LINE";
    let input = json!({"description":description,"command":"echo COMMAND_SENTINEL"});
    assert_eq!(
        bash(input.clone()),
        safe_tool_summary("Task", &input.to_string(), None)
    );
    assert!(!bash(input).unwrap().contains("COMMAND_SENTINEL"));
    assert_eq!(
        bash(json!({"command":"cargo test"})).as_deref(),
        Some("cargo test")
    );
}

#[test]
fn bash_description_rejects_credentials_without_command_fallback() {
    for description in [
        "TOKEN=ShortLeak9",
        "Bearer ShortLeak9",
        "mysql status",
        "inspect ghp_ShortLeak9",
    ] {
        assert!(
            bash(json!({"description":description,"command":"cargo test"})).is_none(),
            "{description}"
        );
    }
}

#[test]
fn shell_assignment_values_are_masked_as_whole_words() {
    for (command, expected) in [
        ("A=1 B=2 cmd", "A=*** B=*** cmd"),
        ("(A=ShortValue9 cmd)", "(A=*** cmd)"),
        ("{A=ShortValue9 cmd; }", "{A=*** cmd; }"),
        ("FOO+=ShortValue9 cmd", "FOO+=*** cmd"),
        ("MODE='head tail' cmd", "MODE=*** cmd"),
        ("MODE='head'\"tail\" cmd", "MODE=*** cmd"),
        ("export X=ShortValue9", "export X=***"),
        ("D=ShortValue9; cargo test", "D=***; cargo test"),
        ("A=\"$(pwd) tail\" cmd", "A=*** cmd"),
    ] {
        let output = bash(json!({"command":command})).unwrap();
        assert_eq!(output.replace('\\', ""), expected, "{command}");
    }
}

#[test]
fn shell_credentials_still_omit_the_summary() {
    for command in [
        "echo ShortLeak9 | docker login --password-stdin",
        "echo ghp_ShortLeak9",
        "TOKEN=ShortLeak9 cargo test",
        "DB_PASS=ShortLeak9 client",
        "MY_PASS+=ShortLeak9 client",
        "(MY_PASS=ShortLeak9 client)",
        "ARR=(head tail) cmd",
        "ARR+=(head tail) cmd",
        "export SERVICE_AUTH=ShortLeak9",
    ] {
        assert!(bash(json!({"command":command})).is_none(), "{command}");
    }
}
