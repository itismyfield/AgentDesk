use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateMessage, EditMessage, MessageId};
use std::collections::HashSet;
use std::sync::Arc;

use super::{DISCORD_MSG_LIMIT, SharedData, rate_limit_wait};
use crate::services::provider::ProviderKind;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, super::Data, Error>;

/// All available tools with (name, description, is_destructive)
pub(super) const ALL_TOOLS: &[(&str, &str, bool)] = &[
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
        "AskUserQuestion",
        "Ask the user a question (interactive)",
        false,
    ),
    ("EnterPlanMode", "Enter planning mode (interactive)", false),
    ("ExitPlanMode", "Exit planning mode (interactive)", false),
];

/// Tool info: (description, is_destructive)
pub(super) fn tool_info(name: &str) -> (&'static str, bool) {
    ALL_TOOLS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, desc, destr)| (*desc, *destr))
        .unwrap_or(("Custom tool", false))
}

/// Map a user-provided tool name onto its canonical Claude Code tool name.
pub(super) fn canonical_tool_name(name: &str) -> Option<&'static str> {
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
pub(super) fn normalize_allowed_tools<I, S>(tools: I) -> Vec<String>
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

/// Format a risk badge for display
pub(super) fn risk_badge(destructive: bool) -> &'static str {
    if destructive { "⚠️" } else { "" }
}

/// Claude Code built-in slash commands
pub(super) const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("clear", "Clear conversation context and start fresh"),
    ("compact", "Compact conversation to reduce context"),
    ("context", "Visualize current context usage"),
    ("cost", "Show token usage and cost for this session"),
    ("diff", "View uncommitted changes and per-turn diffs"),
    ("doctor", "Check Claude Code health and configuration"),
    ("export", "Export conversation to file"),
    ("fast", "Toggle fast output mode"),
    ("files", "List all files currently in context"),
    ("fork", "Create a fork of the current conversation"),
    ("init", "Initialize project with CLAUDE.md guide"),
    ("memory", "Edit CLAUDE.md memory files"),
    ("model", "Switch AI model"),
    ("permissions", "View and manage tool permissions"),
    ("plan", "Enable plan mode or view current plan"),
    ("pr-comments", "View PR comments for current branch"),
    ("rename", "Rename the current conversation"),
    ("review", "Code review for uncommitted changes"),
    ("skills", "List available skills"),
    ("stats", "Show usage statistics"),
    ("status", "Show session status and git info"),
    ("todos", "List current todo items"),
    ("usage", "Show plan usage limits"),
];

/// Extract a description from a skill .md file.
/// Priority: 1) frontmatter `description:` field  2) first meaningful text line
pub(super) fn extract_skill_description(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();

    // Check for YAML frontmatter (starts with ---)
    if lines.first().map(|l| l.trim()) == Some("---") {
        // Find closing ---
        for (i, line) in lines.iter().enumerate().skip(1) {
            let trimmed = line.trim();
            if trimmed == "---" {
                // Look for description: inside frontmatter
                for fm_line in &lines[1..i] {
                    let fm_trimmed = fm_line.trim();
                    if let Some(desc) = fm_trimmed.strip_prefix("description:") {
                        let desc = desc.trim();
                        if !desc.is_empty() {
                            return desc.chars().take(80).collect();
                        }
                    }
                }
                // No description in frontmatter, use first line after frontmatter
                for after_line in &lines[(i + 1)..] {
                    let t = after_line.trim().trim_start_matches('#').trim();
                    if !t.is_empty() {
                        return t.chars().take(80).collect();
                    }
                }
                break;
            }
        }
    }

    // No frontmatter: skip heading lines like "# 역할", use first non-heading meaningful line
    let mut found_heading = false;
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            found_heading = true;
            continue;
        }
        // Use this line as description
        return trimmed.chars().take(80).collect();
    }

    // Fallback: if only heading exists, use heading text
    if found_heading {
        for line in &lines {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                let t = trimmed.trim_start_matches('#').trim();
                if !t.is_empty() {
                    return t.chars().take(80).collect();
                }
            }
        }
    }

    "Custom skill".to_string()
}

/// Build the system-prompt skill notice using one-line descriptions only.
pub(super) fn format_skills_notice(provider: &ProviderKind, skills: &[(String, String)]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let header = match provider {
        ProviderKind::Claude => "Available skills (invoke via the Skill tool):",
        ProviderKind::Codex => "Available local Codex skills (use them by name when relevant):",
        ProviderKind::Gemini => "Available local Gemini skills (use them by name when relevant):",
        ProviderKind::Qwen => "Available local Qwen skills (use them by name when relevant):",
        ProviderKind::Unsupported(_) => return String::new(),
    };

    let list = skills
        .iter()
        .map(|(name, desc)| format!("  - /{}: {}", name, desc))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "\n\n{header}\n\
         The entries below are descriptions only, not the full skill body.\n\
         If a skill is relevant or explicitly requested, load that skill's `SKILL.md` before acting.\n\
         Read files under `references/` only when the `SKILL.md` points to them or you need extra detail.\n\
         {list}"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        build_placeholder_status_block, canonical_tool_name, convert_markdown_tables,
        filter_codex_tool_logs, format_skills_notice, normalize_allowed_tools,
        preserve_previous_tool_status,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn test_canonical_tool_name_is_case_insensitive() {
        assert_eq!(canonical_tool_name("webfetch"), Some("WebFetch"));
        assert_eq!(canonical_tool_name("WEBSEARCH"), Some("WebSearch"));
        assert_eq!(
            canonical_tool_name("AskUserQuestion"),
            Some("AskUserQuestion")
        );
        assert_eq!(
            canonical_tool_name("askuserquestion"),
            Some("AskUserQuestion")
        );
    }

    #[test]
    fn test_normalize_allowed_tools_discards_unknown_and_dedupes() {
        let normalized = normalize_allowed_tools([
            "webfetch",
            "WebFetch",
            "BASH",
            "unknown-tool",
            "askuserquestion",
        ]);

        assert_eq!(
            normalized,
            vec![
                "WebFetch".to_string(),
                "Bash".to_string(),
                "AskUserQuestion".to_string()
            ]
        );
    }

    #[test]
    fn test_convert_markdown_table_to_list() {
        let input = "Before\n\n| Name | Role | Status |\n|------|------|--------|\n| Alice | Dev | Active |\n| Bob | QA | On Leave |\n\nAfter";
        let result = convert_markdown_tables(input);
        assert!(result.contains("- **Name**: Alice, **Role**: Dev, **Status**: Active"));
        assert!(result.contains("- **Name**: Bob, **Role**: QA, **Status**: On Leave"));
        assert!(result.contains("Before"));
        assert!(result.contains("After"));
        assert!(!result.contains("|---"));
    }

    #[test]
    fn test_table_inside_code_block_untouched() {
        let input = "```\n| A | B |\n|---|---|\n| 1 | 2 |\n```";
        let result = convert_markdown_tables(input);
        assert!(result.contains("| A | B |"));
        assert!(result.contains("| 1 | 2 |"));
    }

    #[test]
    fn test_no_table_passthrough() {
        let input = "Just some text\n- list item\n- another";
        let result = convert_markdown_tables(input);
        assert_eq!(result, input);
    }

    // ── P0 tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_canonical_tool_name_case_insensitive() {
        assert_eq!(canonical_tool_name("bash"), Some("Bash"));
        assert_eq!(canonical_tool_name("BASH"), Some("Bash"));
        assert_eq!(canonical_tool_name("Bash"), Some("Bash"));
    }

    #[test]
    fn test_canonical_tool_name_unknown_none() {
        assert_eq!(canonical_tool_name("nonexistent-tool"), None);
        assert_eq!(canonical_tool_name(""), None);
        assert_eq!(canonical_tool_name("FooBar"), None);
    }

    #[test]
    fn test_normalize_allowed_tools_dedupes() {
        let result = normalize_allowed_tools(["Bash", "bash", "BASH"]);
        assert_eq!(result, vec!["Bash".to_string()]);
    }

    #[test]
    fn test_normalize_allowed_tools_discards_unknown() {
        let result = normalize_allowed_tools(["Bash", "unknown-tool", "Read"]);
        assert_eq!(result, vec!["Bash".to_string(), "Read".to_string()]);
        assert!(!result.iter().any(|t| t == "unknown-tool"));
    }

    #[test]
    fn test_extract_skill_description_from_frontmatter() {
        use super::extract_skill_description;

        let content =
            "---\ndescription: Build and deploy the project\n---\n# Deploy\nSome body text";
        assert_eq!(
            extract_skill_description(content),
            "Build and deploy the project"
        );
    }

    #[test]
    fn test_extract_skill_description_no_frontmatter() {
        use super::extract_skill_description;

        let content = "# My Skill\nThis is the body of the skill.";
        // No frontmatter → falls back to first non-heading line
        assert_eq!(
            extract_skill_description(content),
            "This is the body of the skill."
        );
    }

    #[test]
    fn test_format_skills_notice_adds_progressive_disclosure_guidance() {
        let notice = format_skills_notice(
            &ProviderKind::Codex,
            &[(
                "deploy".to_string(),
                "Build and deploy the project".to_string(),
            )],
        );

        assert!(notice.contains("Available local Codex skills"));
        assert!(notice.contains("/deploy: Build and deploy the project"));
        assert!(notice.contains("descriptions only"));
        assert!(notice.contains("`SKILL.md`"));
        assert!(notice.contains("`references/`"));
    }

    #[test]
    fn test_format_skills_notice_mentions_qwen_local_skills() {
        let notice = format_skills_notice(
            &ProviderKind::Qwen,
            &[("deploy".to_string(), "Ship the current branch".to_string())],
        );

        assert!(notice.contains("Available local Qwen skills"));
        assert!(notice.contains("`SKILL.md`"));
        assert!(notice.contains("/deploy: Ship the current branch"));
    }

    #[test]
    fn test_split_message_short_passthrough() {
        use super::split_message;

        let short = "Hello, world!";
        let chunks = split_message(short);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], short);
    }

    #[test]
    fn test_split_message_long_produces_multiple_chunks() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Create a message longer than the Discord limit
        let long_msg: String = "A".repeat(DISCORD_MSG_LIMIT + 500);
        let chunks = split_message(&long_msg);
        assert!(chunks.len() >= 2);
        // Each chunk should be within the limit (with some overhead tolerance)
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT + 50);
        }
    }

    // ── filter_codex_tool_logs tests ─────────────────────────────────────

    #[test]
    fn test_filter_codex_tool_logs_basic() {
        let input = "[Bash] /bin/zsh -lc \"ls -la\"\nHere is the result.\n[Read] /path/to/file\nThe file contains...";
        let output = filter_codex_tool_logs(input);
        assert!(output.contains("⚙\u{fe0f} Bash"));
        assert!(output.contains("Here is the result."));
        assert!(output.contains("⚙\u{fe0f} Read"));
        assert!(output.contains("The file contains..."));
        assert!(!output.contains("/bin/zsh"));
        assert!(!output.contains("/path/to/file"));
    }

    #[test]
    fn test_filter_codex_tool_logs_preserves_code_blocks() {
        let input = "```\n[Bash] should not be filtered\n```\n[Bash] should be filtered";
        let output = filter_codex_tool_logs(input);
        assert!(output.contains("[Bash] should not be filtered"));
        assert!(output.contains("⚙\u{fe0f} Bash"));
    }

    #[test]
    fn test_filter_codex_tool_logs_no_tool_lines() {
        let input = "Hello world\nNo tools here";
        let output = filter_codex_tool_logs(input);
        assert_eq!(output, input);
    }

    #[test]
    fn test_filter_codex_tool_logs_consecutive_same_tool() {
        let input = "[Bash] ls\n[Bash] pwd\n[Bash] cat foo\nDone";
        let output = filter_codex_tool_logs(input);
        assert_eq!(output.matches("⚙\u{fe0f} Bash").count(), 3);
        assert!(output.contains("Done"));
    }

    #[test]
    fn test_filter_codex_tool_logs_tool_name_only() {
        let input = "[Glob]\nResults here";
        let output = filter_codex_tool_logs(input);
        assert!(output.contains("⚙\u{fe0f} Glob"));
        assert!(output.contains("Results here"));
    }

    #[test]
    fn test_filter_codex_tool_logs_leading_whitespace() {
        let input = "  [Edit] some/file.rs\nDone";
        let output = filter_codex_tool_logs(input);
        assert!(output.contains("⚙\u{fe0f} Edit"));
        assert!(output.contains("Done"));
    }

    #[test]
    fn test_filter_codex_tool_logs_ignores_non_tool_brackets() {
        let input = "[Summary] final answer\n[Stopped]\n[HTTP2] note\n[Note] something";
        let output = filter_codex_tool_logs(input);
        assert_eq!(
            output, input,
            "Non-tool bracketed lines must not be filtered"
        );
    }

    #[test]
    fn test_filter_codex_tool_logs_task_family() {
        let input =
            "[Task] worker\n[TaskCreate] issue\n[TaskGet] 123\n[TaskUpdate] 123\n[TaskList]\nDone";
        let output = filter_codex_tool_logs(input);
        assert!(output.contains("⚙\u{fe0f} Task\n"), "Task must be filtered");
        assert!(
            output.contains("⚙\u{fe0f} TaskCreate"),
            "TaskCreate must be filtered"
        );
        assert!(
            output.contains("⚙\u{fe0f} TaskGet"),
            "TaskGet must be filtered"
        );
        assert!(
            output.contains("⚙\u{fe0f} TaskUpdate"),
            "TaskUpdate must be filtered"
        );
        assert!(
            output.contains("⚙\u{fe0f} TaskList"),
            "TaskList must be filtered"
        );
        assert!(output.contains("Done"));
    }

    #[test]
    fn test_preserve_previous_tool_status_promotes_distinct_completed_tool() {
        let mut prev = None;
        preserve_previous_tool_status(
            &mut prev,
            Some("✓ Read: src/config.rs"),
            Some("⚙ Bash: cargo build"),
        );
        assert_eq!(prev.as_deref(), Some("✓ Read: src/config.rs"));
    }

    #[test]
    fn test_preserve_previous_tool_status_ignores_same_tool_transition() {
        let mut prev = None;
        preserve_previous_tool_status(
            &mut prev,
            Some("⚙ Bash: cargo build"),
            Some("✓ Bash: cargo build"),
        );
        assert_eq!(prev, None);
    }

    #[test]
    fn test_build_placeholder_status_block_uses_two_line_trail_only_without_narration() {
        let two_line = build_placeholder_status_block(
            "⠋",
            Some("✓ Read: src/config.rs"),
            Some("⚙ Bash: cargo build"),
            "",
            false,
        );
        assert_eq!(two_line, "✓ Read: src/config.rs\n⠋ ⚙ Bash: cargo build");

        let narrated = build_placeholder_status_block(
            "⠋",
            Some("✓ Read: src/config.rs"),
            Some("⚙ Bash: cargo build"),
            "",
            true,
        );
        assert_eq!(narrated, "⠋ ⚙ Bash: cargo build");
    }
}

pub(super) fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        s.len()
    } else {
        let mut i = index;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}

/// Truncate a string to max_len bytes at a safe UTF-8 and line boundary
pub(super) fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let safe_end = floor_char_boundary(s, max_len);
    let truncated = &s[..safe_end];
    if let Some(pos) = truncated.rfind('\n') {
        truncated[..pos].to_string()
    } else {
        truncated.to_string()
    }
}

/// Normalize consecutive empty lines to maximum of one
pub(super) fn normalize_empty_lines(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_was_empty = false;

    for line in s.lines() {
        let is_empty = line.is_empty();
        if is_empty {
            if !prev_was_empty {
                result.push('\n');
            }
            prev_was_empty = true;
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line);
            prev_was_empty = false;
        }
    }

    result
}

/// Shorten a file path for display: replace home dir with ~ and show only last 2 components
pub(super) fn shorten_path(path: &str) -> String {
    let home = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    let shortened = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    // If path has many components, show .../<last2>
    let parts: Vec<&str> = shortened.split('/').collect();
    if parts.len() > 4 {
        format!(".../{}", parts[parts.len() - 2..].join("/"))
    } else {
        shortened
    }
}

/// Format tool input JSON into a human-readable summary (without tool name prefix).
/// The caller adds the tool name, so this returns only the detail part.
pub(super) fn format_tool_input(name: &str, input: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return truncate_str(input, 200).to_string();
    };

    match name {
        "Bash" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if !desc.is_empty() {
                format!("{}: `{}`", desc, truncate_str(cmd, 150))
            } else {
                format!("`{}`", truncate_str(cmd, 200))
            }
        }
        "Read" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            shorten_path(fp).to_string()
        }
        "Write" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let content = v.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let lines = content.lines().count();
            if lines > 0 {
                format!("{} ({} lines)", shorten_path(fp), lines)
            } else {
                shorten_path(fp).to_string()
            }
        }
        "Edit" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let replace_all = v
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if replace_all {
                format!("{} (replace all)", shorten_path(fp))
            } else {
                shorten_path(fp).to_string()
            }
        }
        "Glob" => {
            let pattern = v.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                format!("{} in {}", pattern, shorten_path(path))
            } else {
                pattern.to_string()
            }
        }
        "Grep" => {
            let pattern = v.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let output_mode = v.get("output_mode").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                if !output_mode.is_empty() {
                    format!(
                        "\"{}\" in {} ({})",
                        pattern,
                        shorten_path(path),
                        output_mode
                    )
                } else {
                    format!("\"{}\" in {}", pattern, shorten_path(path))
                }
            } else {
                format!("\"{}\"", pattern)
            }
        }
        "NotebookEdit" => {
            let nb_path = v
                .get("notebook_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let cell_id = v.get("cell_id").and_then(|v| v.as_str()).unwrap_or("");
            if !cell_id.is_empty() {
                format!("{} ({})", shorten_path(nb_path), cell_id)
            } else {
                shorten_path(nb_path).to_string()
            }
        }
        "WebSearch" => {
            let query = v.get("query").and_then(|v| v.as_str()).unwrap_or("");
            query.to_string()
        }
        "WebFetch" => {
            let url = v.get("url").and_then(|v| v.as_str()).unwrap_or("");
            url.to_string()
        }
        "Task" | "Agent" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let subagent_type = v
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !subagent_type.is_empty() {
                format!("[{}] {}", subagent_type, desc)
            } else {
                desc.to_string()
            }
        }
        "TaskOutput" => {
            let task_id = v.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            task_id.to_string()
        }
        "TaskStop" => {
            let task_id = v.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            task_id.to_string()
        }
        "TodoWrite" => {
            if let Some(todos) = v.get("todos").and_then(|v| v.as_array()) {
                let pending = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("pending"))
                    .count();
                let in_progress = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("in_progress"))
                    .count();
                let completed = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("completed"))
                    .count();
                format!(
                    "Todo: {} pending, {} in progress, {} completed",
                    pending, in_progress, completed
                )
            } else {
                "Update todos".to_string()
            }
        }
        "Skill" => {
            let skill = v.get("skill").and_then(|v| v.as_str()).unwrap_or("");
            skill.to_string()
        }
        "AskUserQuestion" => {
            if let Some(questions) = v.get("questions").and_then(|v| v.as_array()) {
                if let Some(q) = questions.first() {
                    let question = q.get("question").and_then(|v| v.as_str()).unwrap_or("");
                    truncate_str(question, 200)
                } else {
                    "Ask user question".to_string()
                }
            } else {
                "Ask user question".to_string()
            }
        }
        "ExitPlanMode" => "Exit plan mode".to_string(),
        "EnterPlanMode" => "Enter plan mode".to_string(),
        "TaskCreate" => {
            let subject = v.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            subject.to_string()
        }
        "TaskUpdate" => {
            let task_id = v.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            let status = v.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if !status.is_empty() {
                format!("{}: {}", task_id, status)
            } else {
                task_id.to_string()
            }
        }
        "TaskGet" => {
            let task_id = v.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            task_id.to_string()
        }
        "TaskList" => String::new(),
        _ => {
            // MCP tools: try to extract a meaningful detail
            if name.starts_with("mcp__") {
                // Show the short tool name (last segment after __)
                let short_name = name.rsplit("__").next().unwrap_or(name);
                truncate_str(&format!("{}: {}", short_name, input), 200).to_string()
            } else {
                truncate_str(input, 200).to_string()
            }
        }
    }
}

/// Convert markdown tables to Discord-friendly list format.
/// Each data row becomes a bullet with "Header: Value" pairs.
fn convert_markdown_tables(input: &str) -> String {
    let raw_lines: Vec<&str> = input.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mut in_code = false;

    while i < raw_lines.len() {
        let line = raw_lines[i];
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            out.push(line.to_string());
            i += 1;
            continue;
        }
        if in_code {
            out.push(line.to_string());
            i += 1;
            continue;
        }

        // Detect table: header row + separator row
        if line.contains('|') && i + 1 < raw_lines.len() && is_table_separator(raw_lines[i + 1]) {
            let headers = parse_table_cells(line);
            if headers.len() >= 2 {
                i += 2; // skip header + separator
                while i < raw_lines.len() && raw_lines[i].contains('|') {
                    let cells = parse_table_cells(raw_lines[i]);
                    let pairs: Vec<String> = headers
                        .iter()
                        .zip(cells.iter())
                        .filter(|(h, v)| !h.is_empty() || !v.is_empty())
                        .map(|(h, v)| format!("**{}**: {}", h, v))
                        .collect();
                    if !pairs.is_empty() {
                        out.push(format!("- {}", pairs.join(", ")));
                    }
                    i += 1;
                }
                continue;
            }
        }

        out.push(line.to_string());
        i += 1;
    }
    out.join("\n")
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|')
        && trimmed
            .chars()
            .all(|c| c == '|' || c == '-' || c == ':' || c == ' ')
}

fn parse_table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim().trim_matches('|');
    trimmed
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// Build tool-name regex alternation from ALL_TOOLS plus extra names
/// that appear in logs but aren't in the interactive tool list.
fn tool_name_pattern() -> String {
    let mut names: Vec<&str> = ALL_TOOLS.iter().map(|(name, _, _)| *name).collect();
    for extra in &["Agent", "LSP"] {
        if !names.contains(extra) {
            names.push(extra);
        }
    }
    names.join("|")
}

/// Filter Codex CLI tool-call log lines from response text.
/// Replaces `[Bash] command...` -> `⚙️ Bash`, etc.
/// Only lines matching known tool names are replaced; all other text is
/// preserved verbatim. Lines inside code blocks (``` ... ```) are NOT filtered.
pub(super) fn filter_codex_tool_logs(s: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    static TOOL_RE: LazyLock<Regex> = LazyLock::new(|| {
        let names = tool_name_pattern();
        Regex::new(&format!(r"^\s*\[({names})\](\s.*)?$")).unwrap()
    });

    let mut result = Vec::new();
    let mut in_code_block = false;

    for line in s.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            result.push(line.to_string());
            continue;
        }
        if in_code_block {
            result.push(line.to_string());
            continue;
        }

        if let Some(caps) = TOOL_RE.captures(line) {
            let tool_name = &caps[1];
            result.push(format!("⚙\u{fe0f} {tool_name}"));
        } else {
            result.push(line.to_string());
        }
    }

    result.join("\n")
}

/// Apply Codex tool-log filter (if provider is Codex) then format for Discord.
pub(super) fn format_for_discord_with_provider(
    s: &str,
    provider: &crate::services::provider::ProviderKind,
) -> String {
    let filtered;
    let input = if matches!(provider, crate::services::provider::ProviderKind::Codex) {
        filtered = filter_codex_tool_logs(s);
        &filtered
    } else {
        s
    };
    format_for_discord(input)
}

/// Mechanical formatting for Discord readability.
/// Converts markdown headers to bold, ensures spacing around lists, etc.
pub(super) fn format_for_discord(s: &str) -> String {
    // Pre-process: convert markdown tables to bullet lists
    let s = convert_markdown_tables(s);
    let mut lines: Vec<String> = Vec::new();
    let mut in_code_block = false;

    for line in s.lines() {
        // Don't touch anything inside code blocks
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(line.to_string());
            continue;
        }
        if in_code_block {
            lines.push(line.to_string());
            continue;
        }

        let trimmed = line.trim_start();

        // Convert # headers to **bold** (Discord doesn't render headers in bot messages)
        if let Some(rest) = trimmed.strip_prefix("### ") {
            if let Some(prev) = lines.last() {
                if !prev.trim().is_empty() {
                    lines.push(String::new());
                }
            }
            lines.push(format!("**{}**", rest));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            if let Some(prev) = lines.last() {
                if !prev.trim().is_empty() {
                    lines.push(String::new());
                }
            }
            lines.push(format!("**{}**", rest));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            if let Some(prev) = lines.last() {
                if !prev.trim().is_empty() {
                    lines.push(String::new());
                }
            }
            lines.push(format!("**{}**", rest));
            continue;
        }

        // Ensure blank line before the first item of a list block
        let is_list_item = trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || (trimmed.len() > 2
                && trimmed.as_bytes()[0].is_ascii_digit()
                && trimmed.contains(". "));

        if is_list_item {
            if let Some(prev) = lines.last() {
                let prev_trimmed = prev.trim();
                let prev_is_list = prev_trimmed.starts_with("- ")
                    || prev_trimmed.starts_with("* ")
                    || (prev_trimmed.len() > 2
                        && prev_trimmed.as_bytes()[0].is_ascii_digit()
                        && prev_trimmed.contains(". "));
                if !prev_trimmed.is_empty() && !prev_is_list {
                    lines.push(String::new());
                }
            }
        }

        lines.push(line.to_string());
    }

    // Collapse consecutive blank lines (max 1)
    let mut result = String::with_capacity(s.len());
    let mut prev_was_empty = false;
    for line in &lines {
        let is_empty = line.trim().is_empty();
        if is_empty {
            if !prev_was_empty && !result.is_empty() {
                result.push('\n');
            }
            prev_was_empty = true;
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line);
            prev_was_empty = false;
        }
    }

    result
}

/// Send a message using poise Context, splitting if necessary
pub(super) async fn send_long_message_ctx(ctx: Context<'_>, text: &str) -> Result<(), Error> {
    if text.len() <= DISCORD_MSG_LIMIT {
        ctx.say(text).await?;
        return Ok(());
    }

    let chunks = split_message(text);
    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            ctx.say(chunk).await?;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            ctx.channel_id().say(ctx.serenity_context(), chunk).await?;
        }
    }

    Ok(())
}

/// Send a long message using raw HTTP, splitting if necessary
pub(super) async fn send_long_message_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    if text.len() <= DISCORD_MSG_LIMIT {
        rate_limit_wait(shared, channel_id).await;
        channel_id
            .send_message(http, CreateMessage::new().content(text))
            .await?;
        return Ok(());
    }

    let chunks = split_message(text);
    for chunk in &chunks {
        rate_limit_wait(shared, channel_id).await;
        channel_id
            .send_message(http, CreateMessage::new().content(chunk))
            .await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    Ok(())
}

/// Replace an existing Discord message with the first chunk, then send the remaining chunks.
pub(super) async fn replace_long_message_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let chunks = split_message(text);
    let Some(first_chunk) = chunks.first() else {
        return Ok(());
    };

    rate_limit_wait(shared, channel_id).await;
    let edit_result = channel_id
        .edit_message(http, message_id, EditMessage::new().content(first_chunk))
        .await;

    if let Err(e) = edit_result {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] ⚠ replace_long_message_raw edit failed for channel {} msg {}: {e}",
            channel_id.get(),
            message_id.get()
        );
        return send_long_message_raw(http, channel_id, text, shared).await;
    }

    for chunk in chunks.iter().skip(1) {
        rate_limit_wait(shared, channel_id).await;
        channel_id
            .send_message(http, CreateMessage::new().content(chunk))
            .await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    Ok(())
}

/// Split a message into chunks that fit within Discord's 2000 char limit.
/// Handles code block boundaries correctly.
pub(super) fn split_message(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut remaining = text;
    let mut in_code_block = false;
    let mut code_block_lang = String::new();

    while !remaining.is_empty() {
        // Reserve space for code block tags we may need to add
        let tag_overhead = if in_code_block {
            // closing ``` + opening ```lang\n
            3 + 3 + code_block_lang.len() + 1
        } else {
            0
        };
        let effective_limit = DISCORD_MSG_LIMIT
            .saturating_sub(tag_overhead)
            .saturating_sub(10);

        if remaining.len() <= effective_limit {
            let mut chunk = String::new();
            if in_code_block {
                chunk.push_str("```");
                chunk.push_str(&code_block_lang);
                chunk.push('\n');
            }
            chunk.push_str(remaining);
            chunks.push(chunk);
            break;
        }

        // Find a safe split point
        let safe_end = floor_char_boundary(remaining, effective_limit);
        let split_at = remaining[..safe_end].rfind('\n').unwrap_or(safe_end);

        let (raw_chunk, rest) = remaining.split_at(split_at);

        let mut chunk = String::new();
        if in_code_block {
            chunk.push_str("```");
            chunk.push_str(&code_block_lang);
            chunk.push('\n');
        }
        chunk.push_str(raw_chunk);

        // Track code blocks across chunk boundaries
        for line in raw_chunk.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                if in_code_block {
                    in_code_block = false;
                    code_block_lang.clear();
                } else {
                    in_code_block = true;
                    code_block_lang = trimmed.strip_prefix("```").unwrap_or("").to_string();
                }
            }
        }

        // Close unclosed code block at end of chunk
        if in_code_block {
            chunk.push_str("\n```");
        }

        chunks.push(chunk);
        remaining = rest.strip_prefix('\n').unwrap_or(rest);
    }

    chunks
}

/// Add reaction using raw HTTP reference
pub(super) async fn add_reaction_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    emoji: char,
) {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    if let Err(e) = channel_id.create_reaction(http, message_id, reaction).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        eprintln!(
            "  [{ts}] ⚠ Failed to add reaction '{emoji}' to msg {message_id} in channel {channel_id}: {e}"
        );
    }
}

/// Remove reaction using raw HTTP reference
pub(super) async fn remove_reaction_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    emoji: char,
) {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    if let Err(e) = channel_id
        .delete_reaction(http, message_id, None, reaction)
        .await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        eprintln!(
            "  [{ts}] ⚠ Failed to remove reaction '{emoji}' from msg {message_id} in channel {channel_id}: {e}"
        );
    }
}

/// Determine the raw tool status string for Discord status display.
/// Shared by turn_bridge and tmux watcher to avoid duplicating fallback logic.
pub(super) fn resolve_raw_tool_status<'a>(
    current_tool_line: Option<&'a str>,
    full_response: &'a str,
) -> &'a str {
    current_tool_line
        .or_else(|| {
            full_response
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty() && l.trim().len() > 3)
                .map(|l| l.trim())
        })
        .unwrap_or("Processing...")
}

fn tool_status_identity(line: &str) -> (&str, &str) {
    let trimmed = line.trim();
    if trimmed.starts_with("💭") {
        return ("thinking", "thinking");
    }
    if let Some(stripped) = trimmed
        .strip_prefix("⚙")
        .or_else(|| trimmed.strip_prefix("✓"))
        .or_else(|| trimmed.strip_prefix("✗"))
    {
        let stripped = stripped.trim();
        let subject = stripped
            .split_once(':')
            .map(|(head, _)| head.trim())
            .unwrap_or(stripped);
        return ("tool", subject);
    }
    ("other", trimmed)
}

/// Preserve the last distinct tool/thinking status so placeholders can show a
/// short trail instead of only the newest line.
pub(super) fn preserve_previous_tool_status(
    prev_tool_status: &mut Option<String>,
    current_tool_line: Option<&str>,
    next_tool_line: Option<&str>,
) {
    let Some(current) = current_tool_line
        .map(str::trim)
        .filter(|line| !line.is_empty())
    else {
        return;
    };

    if let Some(next) = next_tool_line
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if current == next || tool_status_identity(current) == tool_status_identity(next) {
            return;
        }
    }

    if prev_tool_status.as_deref().map(str::trim) == Some(current) {
        return;
    }

    *prev_tool_status = Some(current.to_string());
}

/// Convert a technical tool status line into a human-friendly label with emoji.
pub(super) fn humanize_tool_status(tool_line: &str) -> String {
    // Thinking: show full text, cap at 600 chars (must leave room for body+footer within Discord 2000 char limit)
    if tool_line.starts_with("💭") {
        return truncate_for_status(tool_line, 600);
    }
    // Everything else: show the raw tool line, truncated at 300 chars
    truncate_for_status(tool_line, 300)
}

/// Build the spinner/status block shown in Discord placeholders.
/// When narrate_progress is disabled, include the previous status line as a
/// compact 2-line trail if both previous and current tool states are available.
pub(super) fn build_placeholder_status_block(
    indicator: &str,
    prev_tool_status: Option<&str>,
    current_tool_line: Option<&str>,
    full_response: &str,
    narrate_progress: bool,
) -> String {
    if !narrate_progress {
        if let (Some(prev), Some(current)) = (prev_tool_status, current_tool_line) {
            let prev = humanize_tool_status(prev);
            let current = humanize_tool_status(current);
            if prev != current {
                return format!("{prev}\n{indicator} {current}");
            }
        }
    }

    let raw_tool_status = resolve_raw_tool_status(current_tool_line, full_response);
    let tool_status = humanize_tool_status(raw_tool_status);
    format!("{indicator} {tool_status}")
}

fn truncate_for_status(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}
