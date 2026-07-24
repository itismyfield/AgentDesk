use super::*;

pub(crate) fn redact_sensitive_for_placeholder(input: &str) -> String {
    static OPENAI_KEY_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"sk-[A-Za-z0-9][A-Za-z0-9_-]{8,}").expect("valid key regex"));
    static BEARER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bBearer\s+\S+").expect("valid bearer token regex"));
    static ASSIGNMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(password|token|api[_-]?key)=\S+")
            .expect("valid secret assignment regex")
    });
    static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b").expect("valid email regex")
    });

    let redacted = OPENAI_KEY_RE.replace_all(input, "***");
    let redacted = BEARER_RE.replace_all(&redacted, "Bearer ***");
    let redacted = ASSIGNMENT_RE.replace_all(&redacted, "${1}=***");
    EMAIL_RE.replace_all(&redacted, "***@***").into_owned()
}

/// All available tools with (name, description, is_destructive)
pub(in crate::services::discord) const ALL_TOOLS: &[(&str, &str, bool)] = &[
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

/// Tool info: (description, is_destructive)
pub(in crate::services::discord) fn tool_info(name: &str) -> (&'static str, bool) {
    ALL_TOOLS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, desc, destr)| (*desc, *destr))
        .unwrap_or(("Custom tool", false))
}

/// Map a user-provided tool name onto its canonical Claude Code tool name.
pub(in crate::services::discord) fn canonical_tool_name(name: &str) -> Option<&'static str> {
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

/// Format a risk badge for display
pub(in crate::services::discord) fn risk_badge(destructive: bool) -> &'static str {
    if destructive { "⚠️" } else { "" }
}

/// Claude Code built-in slash commands
pub(in crate::services::discord) const BUILTIN_SKILLS: &[(&str, &str)] = &[
    (
        "branch",
        "Create a branch (fork) of the current conversation",
    ),
    ("clear", "Clear conversation context and start fresh"),
    ("compact", "Compact conversation to reduce context"),
    ("context", "Visualize current context usage"),
    ("cost", "Show token usage and cost for this session"),
    ("diff", "View uncommitted changes and per-turn diffs"),
    ("doctor", "Check Claude Code health and configuration"),
    ("export", "Export conversation to file"),
    ("fast", "Toggle fast output mode"),
    ("files", "List all files currently in context"),
    (
        "fork",
        "Alias for /branch: create a branch of the current conversation",
    ),
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
pub(in crate::services::discord) fn extract_skill_description(content: &str) -> String {
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

/// Truncate a string to max_len bytes at a safe UTF-8 and line boundary
/// Make a string safe to embed inside a Discord triple-backtick code fence.
///
/// If the input contains a literal "```" sequence, it would prematurely close
/// the surrounding fence and let the rest leak out as Markdown. Insert a
/// zero-width space (U+200B) between the second and third backtick so the
/// rendered output stays inside the fence; the user sees the same backticks
/// visually but Discord no longer treats it as a fence terminator.
pub(in crate::services::discord) fn escape_for_code_fence(s: &str) -> String {
    if s.contains("```") {
        s.replace("```", "``\u{200B}`")
    } else {
        s.to_string()
    }
}

pub(in crate::services::discord) fn truncate_str(s: &str, max_len: usize) -> String {
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
pub(in crate::services::discord) fn normalize_empty_lines(s: &str) -> String {
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
pub(in crate::services::discord) fn shorten_path(path: &str) -> String {
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

/// Render parsed tool input as a COMPACT one-line JSON summary (#2847).
///
/// Tool input frequently arrives as `serde_json::to_string_pretty` output
/// (multi-line, indented) from `session_backend`. The first non-empty line of
/// that is just `{`, which downstream live-event rendering collapses to a bare
/// `[ToolSearch] {` / `[Monitor] {`. Re-serializing the already-parsed value
/// compactly removes the newlines so the fallback is always informative.
fn compact_json_fallback(v: &serde_json::Value, raw: &str) -> String {
    let compact = serde_json::to_string(v).unwrap_or_else(|_| raw.to_string());
    truncate_str(&compact, 200).to_string()
}

/// Format tool input JSON into a human-readable summary (without tool name prefix).
/// The caller adds the tool name, so this returns only the detail part.
pub(in crate::services::discord) fn format_tool_input(name: &str, input: &str) -> String {
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
        "ToolSearch" | "tool_search" | "tool_search_tool" => {
            let query = v.get("query").and_then(|v| v.as_str()).unwrap_or("");
            // ToolSearch's limit field is `max_results`; accept `limit` as an alias.
            let limit = v
                .get("max_results")
                .or_else(|| v.get("limit"))
                .and_then(|v| v.as_u64());
            if query.is_empty() {
                compact_json_fallback(&v, input)
            } else if let Some(limit) = limit {
                format!("\"{}\" (limit {})", truncate_str(query, 150), limit)
            } else {
                format!("\"{}\"", truncate_str(query, 180))
            }
        }
        "Monitor" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if !desc.is_empty() {
                if !cmd.is_empty() {
                    format!("{}: `{}`", desc, truncate_str(cmd, 150))
                } else {
                    desc.to_string()
                }
            } else if !cmd.is_empty() {
                format!("`{}`", truncate_str(cmd, 180))
            } else {
                compact_json_fallback(&v, input)
            }
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
                // Show the short tool name (last segment after __). Compact the
                // input (#2847) so pretty-printed JSON does not leak a bare
                // `<short_name>: {` line through the live-event collapse.
                let short_name = name.rsplit("__").next().unwrap_or(name);
                let compact = serde_json::to_string(&v).unwrap_or_else(|_| input.to_string());
                truncate_str(&format!("{}: {}", short_name, compact), 200).to_string()
            } else {
                compact_json_fallback(&v, input)
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
pub(in crate::services::discord) fn filter_codex_tool_logs(s: &str) -> String {
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

/// Remove Codex CLI tool-call marker lines from response text.
///
/// Status panel v2 surfaces tool progress separately, so final/streaming body
/// content should not keep `[Bash] ...` style marker lines. Lines inside code
/// fences are preserved.
pub(in crate::services::discord) fn strip_codex_tool_log_lines(s: &str) -> String {
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
        if in_code_block || !TOOL_RE.is_match(line) {
            result.push(line.to_string());
        }
    }

    result.join("\n")
}

/// Apply Codex tool-log filter (if provider is Codex) then format for Discord.
pub(in crate::services::discord) fn format_for_discord_with_provider(
    s: &str,
    provider: &crate::services::provider::ProviderKind,
) -> String {
    let sanitized = super::response_sanitizer::sanitize_provider_response(s, provider);
    let filtered;
    let input = if matches!(provider, crate::services::provider::ProviderKind::Codex) {
        filtered = filter_codex_tool_logs(&sanitized);
        &filtered
    } else {
        &sanitized
    };
    let cleaned = strip_placeholder_lines(input);
    format_for_discord(&cleaned)
}

/// Format provider output when the separate status panel is active.
pub(in crate::services::discord) fn format_for_discord_with_status_panel(
    s: &str,
    provider: &crate::services::provider::ProviderKind,
) -> String {
    let sanitized = super::response_sanitizer::sanitize_provider_response(s, provider);
    let filtered;
    let input = if matches!(provider, crate::services::provider::ProviderKind::Codex) {
        filtered = strip_codex_tool_log_lines(&sanitized);
        &filtered
    } else {
        &sanitized
    };
    let cleaned = strip_placeholder_lines(input);
    format_for_discord(&cleaned)
}
