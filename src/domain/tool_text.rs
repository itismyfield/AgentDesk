use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

pub(crate) fn redact_sensitive_for_placeholder(input: &str) -> String {
    static OPENAI_KEY_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"sk-[A-Za-z0-9][A-Za-z0-9_-]{8,}").expect("valid key regex"));
    static BEARER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bBearer\s+\S+").expect("valid bearer token regex"));
    static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b").expect("valid email regex")
    });

    // Keep the compact renderer's token-aware Bearer behavior (so command
    // details after the token remain visible), while sharing the common
    // whole-value Cookie/Set-Cookie contract with logs and recent output.
    let redacted = crate::utils::redact::redact_cookie_headers(input, "***");
    let redacted = OPENAI_KEY_RE.replace_all(&redacted, "***");
    let redacted = BEARER_RE.replace_all(&redacted, "Bearer ***");
    let redacted = crate::utils::redact::redact_assignments(&redacted);
    EMAIL_RE.replace_all(&redacted, "***@***").into_owned()
}

/// All available tools with (name, description, is_destructive)
pub(crate) const ALL_TOOLS: &[(&str, &str, bool)] = &[
    ("Bash", "Execute shell commands", true),
    ("Read", "Read file contents from the filesystem", false),
    ("Edit", "Perform find-and-replace edits in files", true),
    ("Write", "Create or overwrite files", true),
    ("Glob", "Find files by name pattern", false),
    ("Grep", "Search file contents with regex", false),
    (
        "Task",
        "Launch autonomous sub-agents for complex tasks",
        true,
    ),
    ("TaskOutput", "Retrieve output from background tasks", false),
    ("TaskStop", "Stop a running background task", false),
    ("WebFetch", "Fetch and process web page content", true),
    (
        "WebSearch",
        "Search the web for up-to-date information",
        true,
    ),
    ("NotebookEdit", "Edit Jupyter notebook cells", true),
    ("Skill", "Invoke slash-command skills", false),
    (
        "TaskCreate",
        "Create a structured task in the task list",
        false,
    ),
    ("TaskGet", "Retrieve task details by ID", false),
    ("TaskUpdate", "Update task status or details", false),
    ("TaskList", "List all tasks and their status", false),
    (
        "Monitor",
        "Stream events from a background task or shell",
        false,
    ),
    (
        "BashOutput",
        "Read incremental output from a background shell",
        false,
    ),
    ("KillBash", "Terminate a running background shell", true),
    ("SlashCommand", "Invoke a Claude Code slash command", false),
    (
        "AskUserQuestion",
        "Ask the user a question (interactive)",
        false,
    ),
    ("EnterPlanMode", "Enter planning mode (interactive)", false),
    ("ExitPlanMode", "Exit planning mode (interactive)", false),
];

/// Map a user-provided tool name onto its canonical Claude Code tool name.
pub(crate) fn canonical_tool_name(name: &str) -> Option<&'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }

    ALL_TOOLS
        .iter()
        .find(|(tool_name, _, _)| tool_name.eq_ignore_ascii_case(trimmed))
        .map(|(tool_name, _, _)| *tool_name)
}

/// Canonicalize, dedupe, and discard unknown tool names while preserving input order.
pub(crate) fn normalize_allowed_tools<I, S>(tools: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();

    for tool in tools {
        let Some(canonical) = canonical_tool_name(tool.as_ref()) else {
            continue;
        };
        if seen.insert(canonical) {
            normalized.push(canonical.to_string());
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use super::{normalize_allowed_tools, redact_sensitive_for_placeholder};

    #[test]
    fn placeholder_redaction_preserves_exact_display_bytes() {
        for (input, expected) in [
            (
                "sk-abcdefghij Bearer fixture-token echo safe alice@example.com",
                "*** Bearer *** echo safe ***@***",
            ),
            ("Bearer sk-abcdefghij", "Bearer ***"),
            ("sk-abcdefghij@example.com", "***@example.com"),
            (
                "Cookie: session=fixture; access=two\nSet-Cookie: refresh=three;HttpOnly\nnext",
                "Cookie: ***\nSet-Cookie: ***\nnext",
            ),
            (
                "password = \"fixture words\"\nAPI_KEY='fixture words'\n{\"client_secret\": \"fixture words\"}",
                "password = ***\nAPI_KEY=***\n{\"client_secret\": ***}",
            ),
            (
                "api_key_id=public-id\n한글 🙂 safe text\n",
                "api_key_id=public-id\n한글 🙂 safe text\n",
            ),
        ] {
            assert_eq!(
                redact_sensitive_for_placeholder(input),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn allowed_tools_golden_preserves_normalization_and_input_order() {
        assert_eq!(
            normalize_allowed_tools([
                " read ",
                "BASH",
                "read",
                "",
                "unknown",
                "  ",
                " taskoutput\n",
                "bash",
                "ASKUSERQUESTION",
                "write",
                " ReadFile ",
                "Write",
            ]),
            ["Read", "Bash", "TaskOutput", "AskUserQuestion", "Write"]
        );
        assert!(normalize_allowed_tools(["", "  ", "unknown", "ReadFile"]).is_empty());
        assert_eq!(
            normalize_allowed_tools([
                "Bash",
                "Read",
                "Edit",
                "Write",
                "Glob",
                "Grep",
                "Task",
                "TaskOutput",
                "TaskStop",
                "WebFetch",
                "WebSearch",
                "NotebookEdit",
                "Skill",
                "TaskCreate",
                "TaskGet",
                "TaskUpdate",
                "TaskList",
                "Monitor",
                "BashOutput",
                "KillBash",
                "SlashCommand",
                "AskUserQuestion",
                "EnterPlanMode",
                "ExitPlanMode",
            ]),
            [
                "Bash",
                "Read",
                "Edit",
                "Write",
                "Glob",
                "Grep",
                "Task",
                "TaskOutput",
                "TaskStop",
                "WebFetch",
                "WebSearch",
                "NotebookEdit",
                "Skill",
                "TaskCreate",
                "TaskGet",
                "TaskUpdate",
                "TaskList",
                "Monitor",
                "BashOutput",
                "KillBash",
                "SlashCommand",
                "AskUserQuestion",
                "EnterPlanMode",
                "ExitPlanMode",
            ]
        );
    }
}
