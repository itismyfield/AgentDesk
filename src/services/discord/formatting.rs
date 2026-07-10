use poise::serenity_prelude as serenity;
use regex::Regex;
use serenity::{ChannelId, CreateAttachment, MessageId};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use super::{
    DISCORD_MSG_LIMIT, SharedData,
    placeholder_cleanup::{PlaceholderCleanupOutcome, classify_delete_error},
    rate_limit_wait,
    response_sanitizer::subagent_notification_card,
};
use crate::utils::format::tail_with_ellipsis;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, super::Data, Error>;
const STREAMING_PLACEHOLDER_MARGIN: usize = 10;
const THINKING_STATUS_MAX_BYTES: usize = 600;
const TOOL_STATUS_MAX_BYTES: usize = 300;
/// Invisible marker appended to newly-rendered placeholder cards so probes can
/// distinguish status surfaces from delivered answers that happen to start
/// with the same handoff header text.
pub(super) const PLACEHOLDER_PROBE_MARKER: &str = "\u{2063}\u{2062}\u{2063}\u{2062}";

fn watcher_send_failure_message(
    class: super::replace_outcome_policy::WatcherSendFailureClass,
    message: impl std::fmt::Display,
) -> String {
    super::replace_outcome_policy::watcher_send_failure_classified_message(class, message)
}

pub(super) use super::reaction_lifecycle::is_real_discord_message_id;
#[cfg(test)]
pub(super) use super::reaction_lifecycle::reaction_target_channel_for_shared;

// This mutex serializes both the process-local rollback map and every sidecar
// mutation for the same protocol: atomic writes, clears, empty-marker writes,
// and claim-side empty-marker GC. Keeping the fs operation under the same lock
// prevents a claimer from deleting a freshly-written durable debt.
static REPLACE_CONTINUATION_ROLLBACKS: LazyLock<
    Mutex<HashMap<(u64, u64), ReplaceContinuationRollback>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
static REPLACE_CONTINUATION_ROLLBACK_FORCED_REMOVE_FAILURES: LazyLock<Mutex<HashSet<(u64, u64)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[path = "formatting/long_send_rollback.rs"]
mod long_send_rollback;

use self::long_send_rollback::delete_rollback_channel_message;
pub(in crate::services::discord) use self::long_send_rollback::send_long_message_raw_with_reference_rollback;
pub(in crate::services::discord) use self::long_send_rollback::send_long_message_raw_with_rollback;

#[cfg(test)]
pub(in crate::services::discord) mod rollback_transport_test_hook {
    use super::*;

    type SendHook = Box<
        dyn Fn(ChannelId, &str, Option<(ChannelId, MessageId)>) -> Option<Result<MessageId, String>>
            + Send
            + Sync,
    >;
    type DeleteHook = Box<dyn Fn(ChannelId, MessageId) -> Option<Result<(), String>> + Send + Sync>;

    static SEND_HOOK: LazyLock<Mutex<Option<SendHook>>> = LazyLock::new(|| Mutex::new(None));
    static DELETE_HOOK: LazyLock<Mutex<Option<DeleteHook>>> = LazyLock::new(|| Mutex::new(None));

    pub(in crate::services::discord) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            *SEND_HOOK.lock().unwrap_or_else(|error| error.into_inner()) = None;
            *DELETE_HOOK
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
        }
    }

    pub(in crate::services::discord) fn install(send: SendHook, delete: DeleteHook) -> Guard {
        *SEND_HOOK.lock().unwrap_or_else(|error| error.into_inner()) = Some(send);
        *DELETE_HOOK
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(delete);
        Guard
    }

    pub(super) fn send(
        channel_id: ChannelId,
        content: &str,
        reference: Option<(ChannelId, MessageId)>,
    ) -> Option<Result<MessageId, Error>> {
        SEND_HOOK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(|hook| {
                hook(channel_id, content, reference).map(|result| result.map_err(Into::into))
            })
    }

    pub(super) fn delete(
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> Option<Result<(), Error>> {
        DELETE_HOOK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(|hook| hook(channel_id, message_id).map(|result| result.map_err(Into::into)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReplaceContinuationRollback {
    message_ids: Vec<u64>,
    claimed: bool,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct PersistedReplaceContinuationRollback {
    message_ids: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistReplaceContinuationRollbackOutcome {
    Recorded,
    Removed,
    ClearedMarkerWritten,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReplaceContinuationRollbackClaim {
    None,
    Owner(Vec<u64>),
    InProgress(Vec<u64>),
}

fn remove_replace_continuation_rollback_file(
    key: (u64, u64),
    path: &PathBuf,
) -> std::io::Result<()> {
    #[cfg(test)]
    {
        if REPLACE_CONTINUATION_ROLLBACK_FORCED_REMOVE_FAILURES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key)
        {
            return Err(std::io::Error::other("forced rollback remove failure"));
        }
    }
    fs::remove_file(path)
}

#[cfg(test)]
fn force_next_replace_continuation_rollback_remove_failure(key: (u64, u64)) {
    REPLACE_CONTINUATION_ROLLBACK_FORCED_REMOVE_FAILURES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(key);
}

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

/// Inline footer appended to a summary when a long message is delivered as a
/// `.txt` attachment via `/api/discord/send`.
const ATTACHMENT_FOOTER_PREFIX: &str = "📎 전문은 첨부 파일 참고";

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
pub(super) fn risk_badge(destructive: bool) -> &'static str {
    if destructive { "⚠️" } else { "" }
}

/// Claude Code built-in slash commands
pub(super) const BUILTIN_SKILLS: &[(&str, &str)] = &[
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

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn byte_index_at_char_limit(s: &str, max_chars: usize) -> usize {
    if max_chars == 0 {
        0
    } else {
        s.char_indices()
            .nth(max_chars)
            .map(|(idx, _)| idx)
            .unwrap_or(s.len())
    }
}

pub(super) fn streaming_split_boundary(text: &str, max_len: usize) -> Option<usize> {
    if max_len == 0 || char_count(text) <= max_len {
        return None;
    }

    let safe_end = byte_index_at_char_limit(text, max_len);
    if safe_end == 0 {
        return None;
    }

    let window = &text[..safe_end];
    let paragraph_split = window.rfind("\n\n").map(|idx| idx + 2);
    let newline_split = window.rfind('\n').map(|idx| idx + 1);
    let whitespace_split = window
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(idx, ch)| idx + ch.len_utf8());

    let preferred = paragraph_split
        .or(newline_split)
        .or_else(|| super::semantic_boundaries::semantic_sentence_split_boundary(window))
        .or(whitespace_split)
        .unwrap_or(safe_end);
    let preferred_chars = char_count(&text[..preferred]);
    let split_at = if preferred_chars < max_len / 2 {
        safe_end
    } else {
        preferred
    };

    Some(floor_char_boundary(text, split_at))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StreamingRolloverPlan {
    pub(super) display_snapshot: String,
    pub(super) frozen_chunk: String,
    pub(super) split_at: usize,
}

fn build_streaming_placeholder_snapshot(current_portion: &str, status_block: &str) -> String {
    let status_block = clamp_placeholder_status_block(status_block);
    let footer = format!("\n\n{status_block}");
    let body_budget = DISCORD_MSG_LIMIT
        .saturating_sub(char_count(&footer) + STREAMING_PLACEHOLDER_MARGIN)
        .max(1);
    let normalized = normalize_empty_lines(current_portion);
    let body = tail_with_ellipsis(&normalized, body_budget);
    format!("{}{}", body, footer)
}

pub(super) fn plan_streaming_rollover(
    current_portion: &str,
    status_block: &str,
) -> Option<StreamingRolloverPlan> {
    if current_portion.is_empty() {
        return None;
    }

    let status_block = clamp_placeholder_status_block(status_block);
    let footer = format!("\n\n{status_block}");
    let body_budget = DISCORD_MSG_LIMIT
        .saturating_sub(char_count(&footer) + STREAMING_PLACEHOLDER_MARGIN)
        .max(1);
    let split_at = streaming_split_boundary(current_portion, body_budget)?;

    Some(StreamingRolloverPlan {
        display_snapshot: build_streaming_placeholder_snapshot(current_portion, &status_block),
        frozen_chunk: current_portion[..split_at].to_string(),
        split_at,
    })
}

pub(super) fn build_streaming_placeholder_text(
    current_portion: &str,
    status_block: &str,
) -> String {
    if current_portion.is_empty() {
        clamp_placeholder_status_block(status_block)
    } else {
        build_streaming_placeholder_snapshot(current_portion, status_block)
    }
}

/// Truncate a string to max_len bytes at a safe UTF-8 and line boundary
/// Make a string safe to embed inside a Discord triple-backtick code fence.
///
/// If the input contains a literal "```" sequence, it would prematurely close
/// the surrounding fence and let the rest leak out as Markdown. Insert a
/// zero-width space (U+200B) between the second and third backtick so the
/// rendered output stays inside the fence; the user sees the same backticks
/// visually but Discord no longer treats it as a fence terminator.
pub(super) fn escape_for_code_fence(s: &str) -> String {
    if s.contains("```") {
        s.replace("```", "``\u{200B}`")
    } else {
        s.to_string()
    }
}

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

/// Remove Codex CLI tool-call marker lines from response text.
///
/// Status panel v2 surfaces tool progress separately, so final/streaming body
/// content should not keep `[Bash] ...` style marker lines. Lines inside code
/// fences are preserved.
pub(super) fn strip_codex_tool_log_lines(s: &str) -> String {
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
pub(super) fn format_for_discord_with_provider(
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
pub(super) fn format_for_discord_with_status_panel(
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

#[cfg(test)]
mod status_panel_v2_formatter_tests {
    use super::{
        MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder,
        build_monitor_handoff_placeholder_with_live_events, build_placeholder_status_block,
        build_status_panel_streaming_edit_text, build_streaming_placeholder_text,
        format_for_discord, format_for_discord_with_provider, format_for_discord_with_status_panel,
        plan_streaming_rollover,
    };
    use crate::services::provider::ProviderKind;

    const LIVENESS_FOOTER: &str = "⠸ 계속 처리 중";

    #[test]
    fn plan_streaming_rollover_strips_liveness_footer_from_frozen_chunk_s0() {
        let footer = format!("\n\n{LIVENESS_FOOTER}");
        let body_budget = super::DISCORD_MSG_LIMIT
            .saturating_sub(super::char_count(&footer) + super::STREAMING_PLACEHOLDER_MARGIN)
            .max(1);
        let current_portion = "x".repeat(body_budget + 64);

        let plan = plan_streaming_rollover(&current_portion, LIVENESS_FOOTER)
            .expect("current portion should roll over once footer budget is reserved");

        assert_eq!(super::char_count(&plan.frozen_chunk), body_budget);
        assert_eq!(plan.frozen_chunk, &current_portion[..plan.split_at]);
        assert!(!plan.frozen_chunk.contains(LIVENESS_FOOTER));
        assert!(plan.display_snapshot.ends_with(&footer));
    }

    #[test]
    fn rollover_seed_starts_as_liveness_footer_only_s0() {
        let seed = build_streaming_placeholder_text("", LIVENESS_FOOTER);

        assert_eq!(seed, LIVENESS_FOOTER);
    }

    #[test]
    fn plan_streaming_rollover_reserves_footer_length_before_2000_char_limit_s0() {
        let footer = format!("\n\n{LIVENESS_FOOTER}");
        let body_budget = super::DISCORD_MSG_LIMIT
            .saturating_sub(super::char_count(&footer) + super::STREAMING_PLACEHOLDER_MARGIN)
            .max(1);
        let current_portion = "x".repeat(body_budget + 1);
        assert!(super::char_count(&current_portion) < super::DISCORD_MSG_LIMIT);

        let plan = plan_streaming_rollover(&current_portion, LIVENESS_FOOTER)
            .expect("body fits raw Discord limit but not the reserved footer budget");

        assert_eq!(plan.split_at, body_budget);
        assert!(super::char_count(&plan.display_snapshot) <= super::DISCORD_MSG_LIMIT);
        assert!(plan.display_snapshot.ends_with(&footer));
    }

    #[test]
    fn no_rollover_body_and_footer_under_limit_stays_single_message_s0() {
        let current_portion = "short streamed body";
        let rendered = build_streaming_placeholder_text(current_portion, LIVENESS_FOOTER);

        assert!(plan_streaming_rollover(current_portion, LIVENESS_FOOTER).is_none());
        assert_eq!(rendered, format!("{current_portion}\n\n{LIVENESS_FOOTER}"));
        assert!(super::char_count(&rendered) < super::DISCORD_MSG_LIMIT);
    }

    #[test]
    fn empty_body_with_near_limit_footer_stays_footer_only_s0() {
        let oversized_footer = "⠸".repeat(super::DISCORD_MSG_LIMIT);
        let rendered = build_streaming_placeholder_text("", &oversized_footer);

        assert!(plan_streaming_rollover("", &oversized_footer).is_none());
        assert!(super::char_count(&rendered) <= super::DISCORD_MSG_LIMIT);
        assert!(rendered.starts_with('⠸'));
        assert!(!rendered.contains("\n\n"));
    }

    #[test]
    fn single_message_panel_s0_streaming_footer_present_and_final_body_absent() {
        let streamed = build_status_panel_streaming_edit_text(
            "Final answer",
            LIVENESS_FOOTER,
            &ProviderKind::Codex,
        );
        assert_eq!(streamed, "Final answer\n\n⠸ 계속 처리 중");

        let finalized = format_for_discord_with_status_panel(&streamed, &ProviderKind::Codex);
        assert_eq!(finalized, "Final answer");
    }

    #[test]
    fn monitor_handoff_active_keeps_processing_tail_last() {
        let text = build_monitor_handoff_placeholder_with_live_events(
            MonitorHandoffStatus::Active,
            MonitorHandoffReason::AsyncDispatch,
            1_700_000_000,
            Some("⚙ Bash: cargo build"),
            None,
            None,
            None,
            None,
            None,
            Some("```text\n[Bash] cargo build\n```"),
        );

        assert!(text.contains("```text\n[Bash] cargo build\n```"));
        assert!(text.ends_with("⠋ 계속 처리 중 · 시작 <t:1700000000:R>"));
    }

    #[test]
    fn monitor_handoff_terminal_states_drop_processing_tail() {
        let text = build_monitor_handoff_placeholder(
            MonitorHandoffStatus::Completed,
            MonitorHandoffReason::AsyncDispatch,
            1_700_000_000,
            None,
            None,
        );

        assert!(text.starts_with("✅ **응답 완료**\n"));
        assert!(!text.contains("계속 처리 중"));
    }

    #[test]
    fn status_panel_disabled_codex_formatter_keeps_legacy_tool_markers() {
        let input = "[Bash] /bin/zsh -lc \"ls\"\nkeep";
        let output = format_for_discord_with_provider(input, &ProviderKind::Codex);
        assert_eq!(output, "⚙️ Bash\nkeep");
    }

    #[test]
    fn format_for_discord_does_not_insert_blank_line_before_header() {
        let input = "previous line\n## Heading\nfollowing line";
        let output = format_for_discord(input);
        assert_eq!(output, "previous line\n**Heading**\nfollowing line");
    }

    #[test]
    fn format_for_discord_does_not_insert_blank_line_before_list() {
        let input = "lead-in paragraph\n- first item\n- second item\ntrailing line";
        let output = format_for_discord(input);
        assert_eq!(
            output,
            "lead-in paragraph\n- first item\n- second item\ntrailing line"
        );
    }

    #[test]
    fn format_for_discord_preserves_explicit_blank_line_when_agent_provides_one() {
        let input = "first paragraph\n\nsecond paragraph";
        let output = format_for_discord(input);
        assert_eq!(output, "first paragraph\n\nsecond paragraph");
    }

    #[test]
    fn format_for_discord_collapses_codex_quad_newline_to_single_blank_3475() {
        // #3475: codex/adk-cdx bodies arrive with 4-newline paragraph gaps
        // (agent_message chunks each carry their own \n\n after #3431/#3468).
        // The final relayed body must collapse to at most one blank visual line,
        // through the codex-specific formatting entry points.
        let input = "질문은 코드 기준으로 확인.\n\n\n\n검색상 파일과.\n\n\n\n여기서 분기가 하나.";
        let expected = "질문은 코드 기준으로 확인.\n\n검색상 파일과.\n\n여기서 분기가 하나.";

        assert_eq!(format_for_discord(input), expected);
        assert_eq!(
            format_for_discord_with_provider(input, &ProviderKind::Codex),
            expected
        );
        assert_eq!(
            format_for_discord_with_status_panel(input, &ProviderKind::Codex),
            expected
        );
        // No 3+ newline run survives in the relayed body.
        assert!(!format_for_discord(input).contains("\n\n\n"));
    }

    #[test]
    fn format_for_discord_with_provider_hides_raw_subagent_notification() {
        let input = r#"<subagent_notification>
{"agent_path":"/tmp/private-agent","status":{"completed":"Read-only review complete.\n\n1. Check relay path."}}
</subagent_notification>"#;

        let output = format_for_discord_with_provider(input, &ProviderKind::Codex);

        assert!(output.contains("Subagent completed"));
        assert!(output.contains("Read-only review complete."));
        assert!(output.contains("1. Check relay path."));
        assert!(!output.contains("<subagent_notification>"));
        assert!(!output.contains("agent_path"));
        assert!(!output.contains("/tmp/private-agent"));
        assert!(!output.contains("{\""));
    }

    #[test]
    fn format_for_discord_sanitizes_subagent_after_tui_chrome_strip() {
        let input = "No response requested.\n<subagent_notification>{\"agent_path\":\"/tmp/private-agent\",\"status\":{\"completed\":\"Read-only review complete.\"}}</subagent_notification>";

        let output = format_for_discord_with_provider(input, &ProviderKind::Codex);
        assert!(output.contains("Subagent completed"));
        assert!(output.contains("Read-only review complete."));
        assert!(!output.contains("No response requested."));
        assert!(!output.contains("<subagent_notification>"));
        assert!(!output.contains("agent_path"));
        assert!(!output.contains("/tmp/private-agent"));

        let status_panel_output = format_for_discord_with_status_panel(input, &ProviderKind::Codex);
        assert!(status_panel_output.contains("Subagent completed"));
        assert!(!status_panel_output.contains("<subagent_notification>"));
        assert!(!status_panel_output.contains("agent_path"));
    }

    #[test]
    fn format_for_discord_sanitizes_provider_reuse_user_prefixed_subagent_3777() {
        let input = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
[User: 0hbujang (ID: 343742347365974026)] \
<subagent_notification>{\"agent_path\":\"/tmp/private-agent\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";

        let output = format_for_discord_with_provider(input, &ProviderKind::Codex);
        assert!(output.contains("Subagent completed"));
        assert!(output.contains("Review complete."));
        assert!(!output.contains("[Provider Session Reuse]"));
        assert!(!output.contains("[User:"));
        assert!(!output.contains("<subagent_notification>"));
        assert!(!output.contains("agent_path"));
        assert!(!output.contains("/tmp/private-agent"));
    }

    #[test]
    fn format_for_discord_sanitizes_provider_reuse_chrome_then_user_subagent_3818() {
        let input = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
No response requested.\n\
[User: 0hbujang (ID: 343742347365974026)] \
<subagent_notification>{\"agent_path\":\"/tmp/private-agent\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";

        let output = format_for_discord_with_provider(input, &ProviderKind::Codex);
        assert!(output.contains("Subagent completed"));
        assert!(output.contains("Review complete."));
        assert!(!output.contains("[Provider Session Reuse]"));
        assert!(!output.contains("No response requested."));
        assert!(!output.contains("[User:"));
        assert!(!output.contains("<subagent_notification>"));
        assert!(!output.contains("agent_path"));
        assert!(!output.contains("/tmp/private-agent"));

        let status_panel_output = format_for_discord_with_status_panel(input, &ProviderKind::Codex);
        assert!(status_panel_output.contains("Subagent completed"));
        assert!(!status_panel_output.contains("<subagent_notification>"));
        assert!(!status_panel_output.contains("[User:"));
    }

    #[test]
    fn placeholder_status_block_summarizes_subagent_notification_3818() {
        let input = r#"<subagent_notification>
{"agent_path":"/tmp/private-agent","status":{"completed":"Review complete.\n\nVERDICT: CLEAN"}}
</subagent_notification>"#;

        let from_full_response = build_placeholder_status_block("⠙", None, None, input);
        assert!(from_full_response.contains("Subagent completed"));
        assert!(!from_full_response.contains("<subagent_notification>"));
        assert!(!from_full_response.contains("agent_path"));
        assert!(!from_full_response.contains("/tmp/private-agent"));

        let from_current_tool = build_placeholder_status_block("⠙", None, Some(input), "");
        assert!(from_current_tool.contains("Subagent completed"));
        assert!(!from_current_tool.contains("<subagent_notification>"));
        assert!(!from_current_tool.contains("agent_path"));
        assert!(!from_current_tool.contains("/tmp/private-agent"));
    }

    #[test]
    fn format_for_discord_preserves_blank_lines_inside_code_block_3475() {
        // #3475 acceptance: the blank-line collapse must NOT touch code block
        // contents, so relayed code/tool output keeps its intentional spacing.
        let input = "before\n```text\nline1\n\n\n\nline2\n```\nafter";
        let output = format_for_discord(input);
        assert_eq!(output, input);
        // Prose around the fence still collapses normally.
        let mixed = "p1\n\n\n\n```\ncode\n\n\n\nmore\n```\n\n\n\np2";
        let mixed_out = format_for_discord(mixed);
        assert_eq!(mixed_out, "p1\n\n```\ncode\n\n\n\nmore\n```\n\np2");
    }

    #[test]
    fn format_for_discord_removes_trailing_streaming_status_footer() {
        let input = "Final answer\n\n⠋ Processing...";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, "Final answer");
    }

    #[test]
    fn format_for_discord_removes_trailing_korean_processing_footer() {
        let input = "Final answer\n\n⠋ 계속 처리 중";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, "Final answer");
    }

    #[test]
    fn finalize_stale_streaming_footer_strips_completed_body() {
        // #3104: a turn that streamed then returned idle leaves the last edit
        // text ending in `⠏ 계속 처리 중`; finalize must strip it.
        let last_edit = "E2E answer\n- did the work\n\n⠏ 계속 처리 중";
        let finalized = super::finalize_stale_streaming_footer(last_edit, &ProviderKind::Claude);
        assert_eq!(finalized.as_deref(), Some("E2E answer\n- did the work"));
    }

    #[test]
    fn finalize_stale_streaming_footer_leaves_streaming_body_untouched() {
        // A genuinely-still-streaming body (no trailing footer) is left as-is so
        // the reconciliation pass never clears a live footer prematurely.
        let still_streaming = "Partial answer so far";
        assert_eq!(
            super::finalize_stale_streaming_footer(still_streaming, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn finalize_stale_streaming_footer_skips_footer_only_body() {
        // Footer-only placeholder (no real content) must NOT be edited to blank;
        // the caller's delete/replace path owns that case.
        let footer_only = "⠏ 계속 처리 중";
        assert_eq!(
            super::finalize_stale_streaming_footer(footer_only, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn text_ends_with_streaming_footer_detects_korean_footer() {
        assert!(super::text_ends_with_streaming_footer(
            "Answer\n\n⠏ 계속 처리 중"
        ));
        assert!(!super::text_ends_with_streaming_footer(
            "Answer\n\nmore text"
        ));
    }

    #[test]
    fn format_for_discord_removes_leading_tui_no_response_chrome() {
        let input = "No response requested.\n\nFinal answer";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, "Final answer");
    }

    #[test]
    fn format_for_discord_preserves_legitimate_no_response_sentence() {
        let input = "No response requested. But here is the explanation.";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, input);
    }

    #[test]
    fn format_for_discord_keeps_non_trailing_spinner_text() {
        let input = "⠋ Processing...\nFinal answer";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, input);
    }

    #[test]
    fn format_for_discord_removes_stacked_streaming_status_footers() {
        let input = "Final answer\n\n⠋ Processing...\n⠙ Working...";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, "Final answer");
    }

    #[test]
    fn format_for_discord_removes_placeholder_waiting_before_streaming_footer() {
        let input = "Final answer\n⏳ 대기 중...\n\n⠋ Processing...";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, "Final answer");
    }

    #[test]
    fn format_for_discord_keeps_trailing_spinner_without_known_status_shape() {
        let input = "Final answer\n\n⠋ note";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, input);
    }

    #[test]
    fn flag_off_formatter_preserves_trailing_merged_footer_shaped_user_text() {
        let input = "Final answer\n\n⠋ 진행 중 — user-authored line";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);

        assert_eq!(output, input);
        assert_eq!(
            super::finalize_stale_streaming_footer(input, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn format_for_discord_removes_ascii_spinner_status_footer() {
        let input = "Final answer\n\n| Processing...";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, "Final answer");
    }

    #[test]
    fn format_for_discord_keeps_trailing_ascii_bullet_status_text() {
        let input = "Final answer\n- Working on the backend now";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, input);
    }

    #[test]
    fn format_for_discord_keeps_trailing_ascii_table_row() {
        let input = "Final answer\n| Processing fee | 3% |";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, input);
    }

    #[test]
    fn format_for_discord_preserves_trailing_blank_without_footer() {
        let input = "Final answer\n\n";
        let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
        assert_eq!(output, "Final answer");
    }

    // #3089 A0 — characterization of the chunker + streaming-rollover splitter
    // (design §5 A0 item 1: split_message chunk boundaries; item 4: streaming
    // rollover split algorithm). Value-exact pins so any change to chunk
    // boundaries/ordering, the Discord limit cliff, or the rollover
    // split point fails BEFORE the #3089 controller cutover. Nested inside this
    // `#[cfg(test)] mod` block => ZERO production LoC under the ratchet
    // (formatting.rs baseline 2802 stays unchanged).
    mod a0_characterization_tests {
        use super::super::super::semantic_boundaries::{
            message_split_boundary, semantic_sentence_split_boundary,
        };
        use super::super::{
            DISCORD_MSG_LIMIT, char_count, long_message_reply_builders, plan_streaming_rollover,
            split_message, streaming_split_boundary,
        };

        // -------------------------------------------------------------------
        // split_message — the single chunker (design §5 A0 item 1)
        // -------------------------------------------------------------------

        #[test]
        fn a0_split_message_keeps_short_body_as_a_single_verbatim_chunk() {
            let body = "hello world\nsecond line";
            let chunks = split_message(body);
            assert_eq!(chunks.len(), 1, "short body must stay one chunk");
            assert_eq!(chunks[0], body, "the single chunk must be byte-identical");
        }

        #[test]
        fn a0_split_message_effective_limit_is_msg_limit_minus_ten_outside_code_block() {
            let effective_limit = DISCORD_MSG_LIMIT - 10; // 1990
            assert_eq!(effective_limit, 1990, "pins the 2000-10 effective limit");

            let exactly_at_limit = "a".repeat(effective_limit);
            let chunks = split_message(&exactly_at_limit);
            assert_eq!(
                chunks.len(),
                1,
                "a body of exactly effective_limit chars stays a single chunk"
            );

            let one_over = "a".repeat(effective_limit + 1);
            let chunks = split_message(&one_over);
            assert_eq!(
                chunks.len(),
                2,
                "one char over the effective limit splits into two chunks"
            );
        }

        #[test]
        fn a0_split_message_keeps_700_korean_chars_as_one_chunk_issue_4214() {
            let body = "가".repeat(700);
            assert!(
                body.len() > DISCORD_MSG_LIMIT,
                "UTF-8 byte length reproduces the old premature split condition"
            );
            assert_eq!(char_count(&body), 700);

            let chunks = split_message(&body);
            assert_eq!(chunks.len(), 1, "700 Korean chars fit one Discord message");
            assert_eq!(chunks[0], body);

            let replies = long_message_reply_builders(&body);
            assert_eq!(
                replies.len(),
                1,
                "reply builders must not enter the delayed multi-chunk path"
            );
            assert_eq!(
                replies[0].content.as_deref(),
                Some(body.as_str()),
                "single reply preserves the original Korean body"
            );
        }

        #[test]
        fn a0_split_message_bounds_long_korean_chunks_by_character_count_issue_4214() {
            let body = "한".repeat(DISCORD_MSG_LIMIT + 25);
            let chunks = split_message(&body);

            assert!(chunks.len() >= 2, "over-limit Korean body splits");
            assert!(
                chunks.iter().all(|chunk| !chunk.is_empty()),
                "no empty chunk may be emitted for Korean splits"
            );
            assert!(
                chunks
                    .iter()
                    .all(|chunk| char_count(chunk) <= DISCORD_MSG_LIMIT),
                "every emitted chunk must fit Discord's character limit"
            );
            assert_eq!(
                chunks.concat(),
                body,
                "Korean chunks reassemble without losing or corrupting code points"
            );
        }

        #[test]
        fn a0_split_message_hard_splits_newline_free_body_at_the_effective_limit() {
            let body = "a".repeat(2500);
            let chunks = split_message(&body);
            assert_eq!(chunks.len(), 2);
            assert_eq!(
                char_count(&chunks[0]),
                1990,
                "hard split at effective_limit"
            );
            assert_eq!(char_count(&chunks[1]), 2500 - 1990, "remainder length");
            // Order + completeness: concatenation reproduces the input.
            assert_eq!(format!("{}{}", chunks[0], chunks[1]), body);
        }

        #[test]
        fn a0_split_message_prefers_last_newline_and_strips_the_boundary_newline() {
            let head = "a".repeat(1000);
            let tail = "b".repeat(1500);
            let body = format!("{head}\n{tail}"); // newline at byte 1000, within 1990
            let chunks = split_message(&body);
            assert_eq!(chunks.len(), 2);
            assert_eq!(
                chunks[0], head,
                "first chunk ends at the last newline (excl.)"
            );
            assert_eq!(
                chunks[1], tail,
                "the boundary newline is stripped from the next chunk head"
            );
        }

        #[test]
        fn a0_split_message_uses_semantic_sentence_boundary_when_no_newline_exists() {
            let head = format!("{}확인합니다.", "a".repeat(1480));
            let tail = format!("`NullRHI`{}", "b".repeat(1000));
            let body = format!("{head}{tail}");
            let chunks = split_message(&body);

            assert_eq!(chunks.len(), 2);
            assert_eq!(
                chunks[0], head,
                "newline-free prose should split at a sentence boundary before hard-splitting"
            );
            assert_eq!(chunks[1], tail);
            assert_eq!(format!("{}{}", chunks[0], chunks[1]), body);
        }

        #[test]
        fn a0_split_message_leading_newline_does_not_emit_an_empty_chunk() {
            // Issue #1043 guard.
            let body = format!("\n{}", "a".repeat(2200));
            let chunks = split_message(&body);
            assert!(
                chunks.iter().all(|c| !c.is_empty()),
                "no empty chunk may be emitted (#1043)"
            );
            assert!(chunks.len() >= 2, "the long body still splits");
        }

        #[test]
        fn a0_split_message_reopens_code_fence_across_a_chunk_boundary() {
            let mut body = String::from("```rust\n");
            body.push_str(&"x".repeat(2100)); // forces a split while the fence is open
            let chunks = split_message(&body);
            assert!(chunks.len() >= 2, "long fenced body splits");
            assert!(
                chunks[0].ends_with("\n```"),
                "first chunk closes the open fence: {:?}",
                &chunks[0][chunks[0].len().saturating_sub(8)..]
            );
            assert!(
                chunks[1].starts_with("```rust\n"),
                "next chunk re-opens the fence with the same language tag"
            );
        }

        #[test]
        fn a0_long_message_reply_builders_split_without_continuation_markers() {
            let body = "a".repeat(2500);
            let replies = long_message_reply_builders(&body);
            assert_eq!(replies.len(), 2);
            let first = replies[0].content.as_ref().expect("first content");
            let second = replies[1].content.as_ref().expect("second content");

            // Continuation markers ([n/m]) were removed per operator request:
            // the relay must not prepend chunk-index prefixes.
            assert!(!first.starts_with('['));
            assert!(!second.starts_with('['));
            assert!(char_count(first) <= DISCORD_MSG_LIMIT);
            assert!(char_count(second) <= DISCORD_MSG_LIMIT);
        }

        // -------------------------------------------------------------------
        // streaming_split_boundary — rollover boundary primitive (§5 A0 item 4)
        // -------------------------------------------------------------------

        #[test]
        fn a0_streaming_split_boundary_is_none_when_text_fits() {
            assert_eq!(streaming_split_boundary("short", 100), None);
            assert_eq!(streaming_split_boundary("anything", 0), None);
        }

        #[test]
        fn a0_streaming_split_boundary_prefers_paragraph_then_newline_then_whitespace() {
            // Preference: "\n\n" (+2) > "\n" (+1) > whitespace > hard safe_end.
            // safe_end = 50; each break is past safe_end/2 (=25) so the
            // "early break => hard split" guard does not fire and the preferred
            // boundary is used (the index INCLUDES the delimiter).

            // Paragraph break at byte 30 ("\n\n") => split at 30 + 2 = 32.
            let para = format!("{}\n\n{}", "x".repeat(30), "c".repeat(100));
            assert_eq!(
                streaming_split_boundary(&para, 50),
                Some(32),
                "splits just after the paragraph break"
            );

            // Single newline at byte 30 => split at 30 + 1 = 31.
            let nl = format!("{}\n{}", "x".repeat(30), "e".repeat(100));
            assert_eq!(
                streaming_split_boundary(&nl, 50),
                Some(31),
                "splits just after the single newline"
            );

            // Whitespace (space) at byte 30, no newline => split at 30 + 1 = 31.
            let ws = format!("{} {}", "x".repeat(30), "f".repeat(100));
            assert_eq!(
                streaming_split_boundary(&ws, 50),
                Some(31),
                "splits just after the last whitespace"
            );
        }

        #[test]
        fn a0_streaming_split_boundary_paragraph_beats_a_later_single_newline() {
            // MIXED delimiters that DISPROVE priority (codex Medium 4): a
            // paragraph break at byte 26 ("\n\n" => 26 + 2 = 28) precedes a LATER
            // single newline at byte 40 (=> 41), both inside safe_end = 50 and
            // both past safe_end/2 = 25. Production prefers paragraph
            // (`paragraph_split.or(newline_split)`), so the split is 28. If that
            // `.or` were reordered to newline-first, the later newline would win
            // and the split would be 41 — a DIFFERENT value, so this pins the
            // paragraph > single-newline priority, not just the position.
            let body = format!(
                "{}\n\n{}\n{}",
                "x".repeat(26),
                "y".repeat(12),
                "z".repeat(100)
            );
            assert_eq!(
                streaming_split_boundary(&body, 50),
                Some(28),
                "paragraph break wins over a later single newline"
            );
        }

        #[test]
        fn a0_streaming_split_boundary_single_newline_beats_a_later_space() {
            // MIXED delimiters: a single newline at byte 30 (=> 31) precedes a
            // LATER space at byte 42 (=> 43), no paragraph break, both past
            // safe_end/2. Production prefers newline over whitespace
            // (`newline_split.or(whitespace_split)`), so the split is 31. If the
            // chain were reordered to whitespace-first, the later space would win
            // (43) — a DIFFERENT value, pinning the single-newline > whitespace
            // priority.
            let body = format!("{}\n{} {}", "x".repeat(30), "y".repeat(11), "w".repeat(100));
            assert_eq!(
                streaming_split_boundary(&body, 50),
                Some(31),
                "single newline wins over a later space"
            );
        }

        #[test]
        fn a0_streaming_split_boundary_sentence_beats_a_later_space() {
            let head = format!("{}확인합니다.", "x".repeat(20));
            let body = format!("{}{} {}", head, "y".repeat(5), "z".repeat(100));
            assert_eq!(
                streaming_split_boundary(&body, 50),
                Some(head.len()),
                "sentence boundary wins over a later whitespace split"
            );
        }

        #[test]
        fn a0_semantic_sentence_split_boundary_skips_markdown_continuations_and_code_fences() {
            assert_eq!(
                semantic_sentence_split_boundary("확인합니다.`NullRHI`"),
                Some("확인합니다.".len()),
                "inline-code follow-up after Korean sentence is a readable split point"
            );
            assert_eq!(
                semantic_sentence_split_boundary("Use `foo.bar` in config"),
                None,
                "inline code punctuation is not a sentence split point"
            );
            let code_window = "println!(\"done.\"); keep streaming inside fence";
            assert_eq!(
                message_split_boundary(code_window, code_window.len(), true),
                (code_window.len(), "hard"),
                "already-open code fences must not use semantic sentence splits"
            );
            assert_eq!(
                semantic_sentence_split_boundary("- item. more text"),
                None,
                "list items keep their existing markdown continuation behavior"
            );
            assert_eq!(
                semantic_sentence_split_boundary("| Col | value."),
                None,
                "table-like lines keep their existing markdown continuation behavior"
            );
            assert_eq!(
                semantic_sentence_split_boundary("version 1.2"),
                None,
                "decimal points are not sentence boundaries"
            );
            assert_eq!(
                semantic_sentence_split_boundary("config.yaml"),
                None,
                "single-token file extensions are not sentence boundaries"
            );
            assert_eq!(
                semantic_sentence_split_boundary("```text\nDone.\n```"),
                None,
                "code-fence content must not be sentence-split"
            );
        }

        #[test]
        fn a0_streaming_split_boundary_hard_splits_when_break_is_in_first_half() {
            // "preferred < safe_end / 2 => use safe_end": an early break is
            // rejected in favor of a hard split.
            let body = format!("ab\n{}", "g".repeat(100));
            assert_eq!(
                streaming_split_boundary(&body, 50),
                Some(50),
                "an early break is rejected; hard-split at safe_end"
            );
        }

        // -------------------------------------------------------------------
        // plan_streaming_rollover — the rollover plan (§5 A0 item 4)
        //
        // Both turn_bridge and tmux_watcher call THIS single function, so
        // "same input => same output" is the duplication-free behavior to lock.
        // -------------------------------------------------------------------

        #[test]
        fn a0_plan_streaming_rollover_reserves_footer_and_margin_before_the_2000_cliff() {
            // body_budget = 2000 - ((2 + char_count(status)) + 10). For "STATUS":
            // footer "\n\nSTATUS" = 8; body_budget = 2000 - 18 = 1982.
            let status = "STATUS";
            let body = "Z".repeat(2500);
            let plan = plan_streaming_rollover(&body, status).expect("a long body must roll over");
            assert_eq!(
                plan.split_at, 1982,
                "rollover split point pins the footer+margin reservation"
            );
            assert_eq!(
                plan.frozen_chunk,
                "Z".repeat(1982),
                "frozen chunk is body[..split_at]"
            );
            assert_eq!(char_count(&plan.frozen_chunk), plan.split_at);
        }

        #[test]
        fn a0_plan_streaming_rollover_is_none_for_empty_or_short_body() {
            assert_eq!(plan_streaming_rollover("", "STATUS"), None);
            assert_eq!(plan_streaming_rollover("short body", "STATUS"), None);
        }

        #[test]
        fn a0_plan_streaming_rollover_is_deterministic_for_both_caller_surfaces() {
            // Identical (body, status) must yield byte-identical plans on every
            // call, so a future per-surface re-derivation is caught.
            let body = "line one\nline two\n".repeat(200); // > body_budget, newlines
            let a = plan_streaming_rollover(&body, "STATUS").expect("rolls over");
            let b = plan_streaming_rollover(&body, "STATUS").expect("rolls over");
            assert_eq!(a, b, "same input must produce an identical rollover plan");
            assert_eq!(a.frozen_chunk, body[..a.split_at]);
        }
    }
}

/// Remove ephemeral placeholder lines (e.g. "⏳ 대기 중...") from the final
/// delivered response.  These lines are useful during streaming but should not
/// persist in the channel.
fn strip_placeholder_lines(s: &str) -> String {
    let mut lines = Vec::new();
    for line in s.lines() {
        let t = line.trim();
        if t.starts_with("⏳") && t.contains("대기") {
            continue;
        }
        lines.push(line);
    }
    strip_trailing_streaming_status_footer(&mut lines);
    lines.join("\n")
}

fn strip_trailing_streaming_status_footer(lines: &mut Vec<&str>) {
    loop {
        let Some(last_nonblank) = lines.iter().rposition(|line| !line.trim().is_empty()) else {
            break;
        };
        if !is_streaming_placeholder_status_line(lines[last_nonblank].trim()) {
            break;
        }
        lines.truncate(last_nonblank);
    }
}

/// True when `text`'s last non-blank line is a transient streaming footer
/// (e.g. `⠏ 계속 처리 중`). Used by the terminal/idle reconciliation pass to
/// detect a message that still advertises "still processing" after the turn
/// has actually finished, without re-running the full formatter.
pub(super) fn text_ends_with_streaming_footer(text: &str) -> bool {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| is_streaming_placeholder_status_line(line.trim()))
}

/// #3104: terminal/idle reconciliation. Given the last text the bridge/watcher
/// edited onto the visible response message, return the footer-stripped final
/// body that should replace it — but ONLY when the message still ends with a
/// transient `계속 처리 중` (still processing) streaming footer.
///
/// Returns `None` when the message does not end with a streaming footer (so a
/// genuinely-still-streaming or already-finalized body is left untouched), or
/// when stripping the footer would leave no visible content (the caller should
/// then delete/replace via its own empty-body path rather than edit to blank).
pub(super) fn finalize_stale_streaming_footer(
    last_edit_text: &str,
    provider: &crate::services::provider::ProviderKind,
) -> Option<String> {
    if !text_ends_with_streaming_footer(last_edit_text) {
        return None;
    }
    let cleaned = format_for_discord_with_provider(last_edit_text, provider);
    if cleaned.trim().is_empty() {
        return None;
    }
    if cleaned == last_edit_text {
        return None;
    }
    Some(cleaned)
}

pub(super) fn is_streaming_placeholder_status_line(line: &str) -> bool {
    const SPINNER_FRAMES: &[char] = &[
        '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '|', '/', '-', '\\', '◐', '◓', '◑', '◒',
        '⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷',
    ];
    let mut chars = line.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let braille_spinner = ('\u{2800}'..='\u{28ff}').contains(&first);
    if !(SPINNER_FRAMES.contains(&first) || braille_spinner)
        || !chars.next().is_some_and(char::is_whitespace)
    {
        return false;
    }
    let status = chars.as_str().trim();
    let ascii_spinner = matches!(first, '|' | '/' | '-' | '\\');
    if ascii_spinner {
        return matches!(
            status,
            "Processing..."
                | "Processing…"
                | "Thinking..."
                | "Thinking…"
                | "Generating..."
                | "Generating…"
                | "Working..."
                | "Working…"
        );
    }
    status.starts_with("Processing")
        || status.starts_with("Thinking")
        || status.starts_with("Generating")
        || status.starts_with("Working")
        || status.starts_with("계속 처리 중")
        || status.starts_with("응답")
        || status.starts_with("처리")
        || status.starts_with('⚙')
        || status.starts_with('⚠')
        || status.starts_with('⏱')
        || status.starts_with('💭')
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

        // Convert # headers to **bold** (Discord ignores them); keep the agent's spacing as-is.
        if let Some(rest) = trimmed.strip_prefix("### ") {
            lines.push(format!("**{}**", rest));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            lines.push(format!("**{}**", rest));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            lines.push(format!("**{}**", rest));
            continue;
        }

        // List items pass through verbatim; surrounding blank lines collapse below.
        lines.push(line.to_string());
    }

    // Collapse consecutive blank lines (max 1); ``` code-block contents stay verbatim (#3475).
    let mut result = String::with_capacity(s.len());
    let mut prev_was_empty = false;
    let mut in_code_block = false;
    for line in &lines {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
        }
        let is_empty = !in_code_block && line.trim().is_empty();
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
    if char_count(text) <= DISCORD_MSG_LIMIT {
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_ctx",
            chunk_index = 0usize,
            byte_len = text.len(),
            total_chunks = 1usize,
            "discord send single"
        );
        ctx.say(text).await?;
        return Ok(());
    }

    let chunks = split_message(text);
    let total = chunks.len();
    for (i, chunk) in chunks.iter().enumerate() {
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_ctx",
            chunk_index = i,
            byte_len = chunk.len(),
            total_chunks = total,
            "discord send chunk"
        );
        if i == 0 {
            ctx.say(chunk).await?;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            ctx.channel_id().say(ctx.serenity_context(), chunk).await?;
        }
    }

    Ok(())
}

pub(in crate::services::discord) fn long_message_reply_builders(
    text: &str,
) -> Vec<poise::CreateReply> {
    if char_count(text) <= DISCORD_MSG_LIMIT {
        return vec![poise::CreateReply::default().content(text.to_string())];
    }

    split_message(text)
        .into_iter()
        .map(|chunk| poise::CreateReply::default().content(chunk))
        .collect()
}

/// Send a long command response through poise's reply abstraction.
///
/// In slash-command contexts, poise maps the first call to an interaction
/// response and later calls to interaction followups. That avoids direct
/// channel sends from slash command handlers while preserving chunking.
pub(in crate::services::discord) async fn send_long_message_reply_ctx(
    ctx: Context<'_>,
    text: &str,
) -> Result<(), Error> {
    let replies = long_message_reply_builders(text);
    let total = replies.len();

    for (i, reply) in replies.into_iter().enumerate() {
        let byte_len = reply.content.as_ref().map_or(0, String::len);
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_reply_ctx",
            chunk_index = i,
            byte_len,
            total_chunks = total,
            "discord command reply chunk"
        );
        if i > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
        ctx.send(reply).await?;
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
    send_long_message_raw_with_reference(http, channel_id, text, shared, None).await
}

/// Send a long message using raw HTTP, replying to `reference` for the first
/// Discord message when available.
pub(in crate::services::discord) async fn send_long_message_raw_with_reference(
    http: &serenity::Http,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
    reference: Option<(ChannelId, MessageId)>,
) -> Result<(), Error> {
    send_long_message_raw_with_reference_returning_message_ids(
        http, channel_id, text, shared, reference,
    )
    .await
    .map(|_| ())
}

/// Send a long message using raw HTTP and return every created Discord message id.
pub(in crate::services::discord) async fn send_long_message_raw_with_reference_returning_message_ids(
    http: &serenity::Http,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
    reference: Option<(ChannelId, MessageId)>,
) -> Result<Vec<MessageId>, Error> {
    let payload_byte_len = text.len();
    if char_count(text) <= DISCORD_MSG_LIMIT {
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_raw",
            channel_id = channel_id.get(),
            payload_byte_len,
            chunk_index = 0usize,
            byte_len = payload_byte_len,
            total_chunks = 1usize,
            "discord send single"
        );
        rate_limit_wait(shared, channel_id).await;
        match send_channel_message_with_optional_reference(http, channel_id, text, reference).await
        {
            Ok(message) => {
                tracing::debug!(
                    target: "discord::chunker",
                    path = "send_long_message_raw",
                    channel_id = channel_id.get(),
                    payload_byte_len,
                    last_chunk = true,
                    outcome = "ok",
                    "discord send single done"
                );
                return Ok(vec![message.id]);
            }
            Err(err) => {
                tracing::warn!(
                    target: "discord::chunker",
                    path = "send_long_message_raw",
                    channel_id = channel_id.get(),
                    payload_byte_len,
                    last_chunk = true,
                    outcome = "err",
                    error = %err,
                    "discord send single failed (issue #1043)"
                );
                return Err(err.into());
            }
        }
    }

    let chunks = split_message(text);
    let total = chunks.len();
    // #3082 part B: hold the per-channel answer-flush barrier for the whole
    // multi-chunk send so a queued-turn notice POST cannot interleave between
    // chunks. The guard clears the gate on every exit path (Ok, `?`, panic).
    let _answer_flush_guard =
        (total > 1).then(|| shared.answer_flush_barrier.begin_flush(channel_id));
    tracing::debug!(
        target: "discord::chunker",
        path = "send_long_message_raw",
        channel_id = channel_id.get(),
        payload_byte_len,
        total_chunks = total,
        "discord send begin"
    );
    let mut sent_message_ids = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i + 1 == total;
        tracing::debug!(
            target: "discord::chunker",
            path = "send_long_message_raw",
            channel_id = channel_id.get(),
            chunk_index = i,
            byte_len = chunk.len(),
            total_chunks = total,
            is_last_chunk = is_last,
            "discord send chunk"
        );
        rate_limit_wait(shared, channel_id).await;
        let chunk_reference = if i == 0 { reference.clone() } else { None };
        let send_result =
            send_channel_message_with_optional_reference(http, channel_id, chunk, chunk_reference)
                .await;
        match send_result {
            Ok(message) => {
                // #3082 P1-2: chunk landed — keep the answer-flush barrier's
                // inactivity window fresh so a long answer never trips the
                // queued-card wait while it is still making progress.
                shared.answer_flush_barrier.note_progress(channel_id);
                shared
                    .tmux_relay_coord(channel_id)
                    .note_relay_progress_heartbeat(chrono::Utc::now().timestamp_millis());
                sent_message_ids.push(message.id);
                if is_last {
                    tracing::debug!(
                        target: "discord::chunker",
                        path = "send_long_message_raw",
                        channel_id = channel_id.get(),
                        chunk_index = i,
                        total_chunks = total,
                        last_chunk = true,
                        outcome = "ok",
                        "discord send last chunk ok"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    target: "discord::chunker",
                    path = "send_long_message_raw",
                    channel_id = channel_id.get(),
                    chunk_index = i,
                    total_chunks = total,
                    last_chunk = is_last,
                    outcome = "err",
                    error = %err,
                    "discord send chunk failed (issue #1043 — tail may be missing)"
                );
                return Err(err.into());
            }
        }
    }

    Ok(sent_message_ids)
}

async fn send_channel_message_with_optional_reference(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    reference: Option<(ChannelId, MessageId)>,
) -> serenity::Result<serenity::Message> {
    let Some((reference_channel_id, reference_message_id)) = reference else {
        return super::http::send_channel_message(http, channel_id, content).await;
    };
    match super::http::send_channel_message_with_reference(
        http,
        channel_id,
        content,
        reference_channel_id,
        reference_message_id,
    )
    .await
    {
        Ok(message) => Ok(message),
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                reference_channel_id = reference_channel_id.get(),
                reference_message_id = reference_message_id.get(),
                error = %error,
                "discord referenced send failed; falling back to plain message"
            );
            super::http::send_channel_message(http, channel_id, content).await
        }
    }
}

/// Replace an existing Discord message with the first chunk, then send the remaining chunks.
pub(super) async fn replace_long_message_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    replace_long_message_outcome_to_result(
        // #3805 P1: this wrapper discards the last-chunk anchor (footer re-anchor
        // is watcher-only); pass a throwaway sink.
        replace_long_message_raw_with_outcome(
            http, channel_id, message_id, text, shared, &mut None,
        )
        .await?,
    )
}

pub(super) fn replace_long_message_outcome_to_result(
    outcome: ReplaceLongMessageOutcome,
) -> Result<(), Error> {
    match outcome {
        ReplaceLongMessageOutcome::EditedOriginal => Ok(()),
        ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. } => Ok(()),
        ReplaceLongMessageOutcome::PartialContinuationFailure { error, .. } => Err(error.into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReplaceLongMessageOutcome {
    EditedOriginal,
    SentFallbackAfterEditFailure {
        edit_error: String,
        /// First fallback fresh-send message; recovery records it after a stale-anchor edit miss.
        replacement_anchor: Option<MessageId>,
    },
    PartialContinuationFailure {
        sent_chunks: usize,
        total_chunks: usize,
        failed_chunk_index: usize,
        sent_continuation_message_ids: Vec<u64>,
        cleanup_errors: Vec<String>,
        error: String,
    },
}

/// #3805 P1: the LAST continuation chunk produced by a fully-successful
/// multi-chunk `replace_long_message_raw_with_outcome` (`EditedOriginal` where
/// the body split into 2+ chunks). Carries BOTH the tail chunk's message id
/// (the highest snowflake — #3717 latest-wins) AND its exact text so a caller
/// that appends a completion footer can re-anchor onto the tail chunk instead of
/// stranding the footer on the edited chunk 0. The text MUST be the tail chunk's
/// OWN text: the footer edit rewrites the target message with `strip(text) +
/// footer`, so registering the full body would clobber the tail chunk with the
/// entire answer. Reported via a `&mut Option` out-param (the enum stays a unit
/// variant — ~20 `matches!` commit/delivered sites depend on that) and left
/// `None` for single-chunk answers, where chunk 0 already IS the tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReplaceLastChunkAnchor {
    pub(super) msg_id: u64,
    pub(super) text: String,
}

/// #3805 P1: pick the completion-footer terminal target (message id + text) for
/// the watcher's in-place edit arm (`replace_long_message_raw_with_outcome`
/// `EditedOriginal`). When the answer split into multiple chunks the tail
/// continuation is the durable anchor (highest snowflake — #3717 latest-wins),
/// and its edit text MUST be the tail chunk's OWN text: the completion edit
/// rewrites the target with `strip(text) + footer`, so passing the full body
/// would overwrite the tail chunk with the entire answer (§4 regression). For a
/// single-chunk answer there is no continuation anchor, so the edited chunk-0 id
/// plus the full relay text are the target (identical there — no regression).
pub(super) fn watcher_completion_footer_anchor<'a>(
    last_chunk_anchor: Option<&'a ReplaceLastChunkAnchor>,
    edited_chunk0_msg_id: MessageId,
    full_relay_text: &'a str,
) -> (MessageId, &'a str) {
    match last_chunk_anchor {
        Some(anchor) => (MessageId::new(anchor.msg_id), anchor.text.as_str()),
        None => (edited_chunk0_msg_id, full_relay_text),
    }
}

/// Replace an existing Discord message and report whether the original
/// placeholder was actually edited. If the edit fails but the fallback send
/// succeeds, wrapper callers treat delivery as committed, while callers that
/// own placeholder lifecycle can still use this outcomeful variant to delete
/// or terminal-edit the stale original.
pub(super) async fn replace_long_message_raw_with_outcome(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
    // #3805 P1: on the fully-successful multi-chunk edit path this is set to the
    // tail continuation chunk (id + text) so a footer-appending caller can
    // re-anchor onto it; left untouched (caller-initialised `None`) on every
    // other path (single-chunk, edit-failure fallback, partial failure).
    last_chunk_anchor: &mut Option<ReplaceLastChunkAnchor>,
) -> Result<ReplaceLongMessageOutcome, Error> {
    let payload_byte_len = text.len();
    let chunks = split_message(text);
    let total = chunks.len();
    let Some(first_chunk) = chunks.first() else {
        tracing::debug!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            payload_byte_len,
            total_chunks = 0usize,
            "discord replace: no chunks"
        );
        return Ok(ReplaceLongMessageOutcome::EditedOriginal);
    };
    let rollback_key = replace_continuation_rollback_key(channel_id, message_id);
    match claim_replace_continuation_rollback(rollback_key) {
        ReplaceContinuationRollbackClaim::None => {}
        ReplaceContinuationRollbackClaim::InProgress(pending_ids) => {
            return Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                sent_chunks: 0,
                total_chunks: total,
                failed_chunk_index: 0,
                sent_continuation_message_ids: pending_ids,
                cleanup_errors: Vec::new(),
                error: watcher_send_failure_message(
                    super::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete,
                    "previous continuation cleanup in progress",
                ),
            });
        }
        ReplaceContinuationRollbackClaim::Owner(pending_ids) => {
            let cleanup =
                cleanup_replace_continuations_after_failure(http, channel_id, &pending_ids, shared)
                    .await;
            if cleanup.failed_message_ids.is_empty() {
                if let Err(error) = clear_replace_continuation_rollback(rollback_key) {
                    unclaim_replace_continuation_rollback(rollback_key);
                    let mut cleanup_errors = cleanup.errors;
                    cleanup_errors.push(error.clone());
                    return Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                        sent_chunks: 0,
                        total_chunks: total,
                        failed_chunk_index: 0,
                        sent_continuation_message_ids: pending_ids,
                        cleanup_errors,
                        error: watcher_send_failure_message(
                            super::replace_outcome_policy::WatcherSendFailureClass::Transient,
                            format!(
                                "previous continuation rollback state was not cleared: {error}"
                            ),
                        ),
                    });
                }
            } else {
                let mut cleanup_errors = cleanup.errors;
                if let Err(error) = record_replace_continuation_rollback(
                    rollback_key,
                    cleanup.failed_message_ids.clone(),
                ) {
                    record_replace_continuation_rollback_memory_only(
                        rollback_key,
                        cleanup.failed_message_ids.clone(),
                    );
                    cleanup_errors.push(error.clone());
                }
                return Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks: 0,
                    total_chunks: total,
                    failed_chunk_index: 0,
                    sent_continuation_message_ids: pending_ids,
                    cleanup_errors,
                    error: watcher_send_failure_message(
                        super::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete,
                        "previous continuation cleanup incomplete",
                    ),
                });
            }
        }
    }

    // #3082 part B (codex P1-1): the edit/replace path is ALSO a multi-chunk
    // send (chunk 0 edited, continuations sent). Hold the same per-channel
    // answer-flush barrier across the whole edit+continuation send so a
    // queued-turn "📬" notice POST cannot interleave between this answer's
    // chunks. Acquired BEFORE the first edit await and held (RAII) across every
    // continuation send and every cleanup/error return below — the guard clears
    // the gate on every exit path (Ok, early `return`, `?`, panic-unwind). The
    // fallback `send_long_message_raw_with_rollback` acquires its own guard, so
    // we intentionally do NOT also hold one there (no double-count needed).
    let _answer_flush_guard =
        (total > 1).then(|| shared.answer_flush_barrier.begin_flush(channel_id));

    tracing::debug!(
        target: "discord::chunker",
        path = "replace_long_message_raw",
        channel_id = channel_id.get(),
        message_id = message_id.get(),
        payload_byte_len,
        chunk_index = 0usize,
        byte_len = first_chunk.len(),
        total_chunks = total,
        is_last_chunk = total == 1,
        "discord edit first chunk"
    );
    rate_limit_wait(shared, channel_id).await;
    let edit_result =
        super::http::edit_channel_message(http, channel_id, message_id, first_chunk).await;

    if let Err(e) = edit_result {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ replace_long_message_raw edit failed for channel {} msg {}: {e}",
            channel_id.get(),
            message_id.get()
        );
        tracing::warn!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            message_id = message_id.get(),
            payload_byte_len,
            chunk_index = 0usize,
            total_chunks = total,
            outcome = "edit_failed_falling_back_to_send",
            error = %e,
            "discord first-chunk edit failed; falling back to send_long_message_raw (issue #1043)"
        );
        let edit_error = e.to_string();
        let replacement_message_ids =
            send_long_message_raw_with_rollback(http, channel_id, message_id, text, shared).await?;
        return Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
            edit_error,
            replacement_anchor: replacement_message_ids.first().copied(),
        });
    }

    // #3082 P1-2 residual: the FIRST edited chunk also delivers answer payload
    // while the multi-chunk barrier guard is held. Mirror the continuation loop
    // (and the two send loops) by bumping the answer-flush barrier's inactivity
    // window here too, so a queued-card waiter's inactivity grace cannot
    // spuriously expire between this first edit and the first continuation send.
    // Only on the multi-chunk path (guard active) — single-chunk edits hold no
    // guard and have no continuation to race against.
    if total > 1 {
        shared.answer_flush_barrier.note_progress(channel_id);
    }

    if total == 1 {
        tracing::debug!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            message_id = message_id.get(),
            payload_byte_len,
            chunk_index = 0usize,
            total_chunks = total,
            last_chunk = true,
            outcome = "ok",
            "discord edit single-chunk ok"
        );
    }

    let mut sent_continuation_message_ids = Vec::new();
    for (offset, chunk) in chunks.iter().skip(1).enumerate() {
        let i = offset + 1;
        let is_last = i + 1 == total;
        tracing::debug!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            chunk_index = i,
            byte_len = chunk.len(),
            total_chunks = total,
            is_last_chunk = is_last,
            "discord send continuation chunk"
        );
        rate_limit_wait(shared, channel_id).await;
        match super::http::send_channel_message(http, channel_id, chunk).await {
            Ok(message) => {
                // #3082 P1-2: this chunk landed — reset the answer-flush
                // barrier's inactivity window so a long edit/replace answer
                // that keeps making progress never trips the queued-card wait.
                shared.answer_flush_barrier.note_progress(channel_id);
                sent_continuation_message_ids.push(message.id.get());
                if let Err(error) = record_replace_continuation_rollback(
                    rollback_key,
                    sent_continuation_message_ids.clone(),
                ) {
                    tracing::warn!(
                        target: "discord::chunker",
                        path = "replace_long_message_raw",
                        channel_id = channel_id.get(),
                        chunk_index = i,
                        total_chunks = total,
                        error = %error,
                        "discord replace continuation sent but rollback state was not durable; deleting sent continuations before retry"
                    );
                    let cleanup_errors = cleanup_replace_continuations_after_failure(
                        http,
                        channel_id,
                        &sent_continuation_message_ids,
                        shared,
                    )
                    .await;
                    let mut errors = cleanup_errors.errors;
                    errors.push(error.clone());
                    if cleanup_errors.failed_message_ids.is_empty() {
                        if let Err(clear_error) = clear_replace_continuation_rollback(rollback_key)
                        {
                            errors.push(clear_error);
                        }
                    } else if let Err(record_error) = record_replace_continuation_rollback(
                        rollback_key,
                        cleanup_errors.failed_message_ids.clone(),
                    ) {
                        record_replace_continuation_rollback_memory_only(
                            rollback_key,
                            cleanup_errors.failed_message_ids.clone(),
                        );
                        errors.push(record_error);
                    }
                    let class = if cleanup_errors.failed_message_ids.is_empty() {
                        super::replace_outcome_policy::WatcherSendFailureClass::Transient
                    } else {
                        super::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete
                    };
                    return Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                        sent_chunks: i + 1,
                        total_chunks: total,
                        failed_chunk_index: i,
                        sent_continuation_message_ids: sent_continuation_message_ids.clone(),
                        cleanup_errors: errors,
                        error: watcher_send_failure_message(class, error),
                    });
                }
                if is_last {
                    tracing::debug!(
                        target: "discord::chunker",
                        path = "replace_long_message_raw",
                        channel_id = channel_id.get(),
                        chunk_index = i,
                        total_chunks = total,
                        last_chunk = true,
                        outcome = "ok",
                        "discord replace last chunk ok"
                    );
                }
            }
            Err(err) => {
                let failure_class =
                    super::replace_outcome_policy::classify_watcher_send_failure(&err);
                let error = err.to_string();
                tracing::warn!(
                    target: "discord::chunker",
                    path = "replace_long_message_raw",
                    channel_id = channel_id.get(),
                    chunk_index = i,
                    total_chunks = total,
                    last_chunk = is_last,
                    outcome = "err",
                    error = %error,
                    "discord replace continuation failed; deleting sent continuations before retry"
                );
                let cleanup_errors = cleanup_replace_continuations_after_failure(
                    http,
                    channel_id,
                    &sent_continuation_message_ids,
                    shared,
                )
                .await;
                if cleanup_errors.failed_message_ids.is_empty() {
                    if let Err(error) = clear_replace_continuation_rollback(rollback_key) {
                        unclaim_replace_continuation_rollback(rollback_key);
                        let mut errors = cleanup_errors.errors;
                        errors.push(error.clone());
                        return Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                            sent_chunks: i,
                            total_chunks: total,
                            failed_chunk_index: i,
                            sent_continuation_message_ids: sent_continuation_message_ids.clone(),
                            cleanup_errors: errors,
                            error: watcher_send_failure_message(failure_class, error),
                        });
                    }
                } else {
                    let mut errors = cleanup_errors.errors;
                    if let Err(record_error) = record_replace_continuation_rollback(
                        rollback_key,
                        cleanup_errors.failed_message_ids.clone(),
                    ) {
                        record_replace_continuation_rollback_memory_only(
                            rollback_key,
                            cleanup_errors.failed_message_ids.clone(),
                        );
                        errors.push(record_error);
                    }
                    return Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                        sent_chunks: i,
                        total_chunks: total,
                        failed_chunk_index: i,
                        sent_continuation_message_ids: sent_continuation_message_ids.clone(),
                        cleanup_errors: errors,
                        error: watcher_send_failure_message(
                            super::replace_outcome_policy::WatcherSendFailureClass::RollbackIncomplete,
                            error,
                        ),
                    });
                }
                return Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks: i,
                    total_chunks: total,
                    failed_chunk_index: i,
                    sent_continuation_message_ids: sent_continuation_message_ids.clone(),
                    cleanup_errors: cleanup_errors.errors,
                    error: watcher_send_failure_message(failure_class, error),
                });
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    if !sent_continuation_message_ids.is_empty()
        && let Err(error) = clear_replace_continuation_rollback(rollback_key)
    {
        clear_replace_continuation_rollback_memory_only(rollback_key);
        tracing::warn!(
            target: "discord::chunker",
            path = "replace_long_message_raw",
            channel_id = channel_id.get(),
            message_id = message_id.get(),
            error = %error,
            "discord replace delivered all chunks but rollback state cleanup failed"
        );
    }
    // #3805 P1: fully-successful edit+continuations. When continuations were
    // sent, hand back the TAIL chunk (id + its own text) so the watcher footer
    // can re-anchor onto it (highest snowflake, #3717 latest-wins). Empty
    // continuations ⇒ single-chunk answer ⇒ leave `None` (chunk 0 is the tail).
    *last_chunk_anchor =
        sent_continuation_message_ids
            .last()
            .copied()
            .map(|msg_id| ReplaceLastChunkAnchor {
                msg_id,
                text: chunks.last().cloned().unwrap_or_default(),
            });
    Ok(ReplaceLongMessageOutcome::EditedOriginal)
}

#[derive(Debug, Default)]
struct ContinuationCleanupResult {
    failed_message_ids: Vec<u64>,
    errors: Vec<String>,
}

async fn cleanup_replace_continuations_after_failure(
    http: &serenity::Http,
    channel_id: ChannelId,
    sent_continuation_message_ids: &[u64],
    shared: &Arc<SharedData>,
) -> ContinuationCleanupResult {
    let mut result = ContinuationCleanupResult::default();
    for message_id in sent_continuation_message_ids.iter().rev().copied() {
        rate_limit_wait(shared, channel_id).await;
        if let Err(error) =
            delete_rollback_channel_message(http, channel_id, MessageId::new(message_id)).await
        {
            let detail = error.to_string();
            match classify_delete_error(&detail) {
                PlaceholderCleanupOutcome::AlreadyGone | PlaceholderCleanupOutcome::Succeeded => {
                    tracing::debug!(
                        target: "discord::chunker",
                        channel_id = channel_id.get(),
                        message_id,
                        detail = %detail,
                        "continuation cleanup delete is already committed"
                    );
                }
                PlaceholderCleanupOutcome::Failed { .. } => {
                    result.failed_message_ids.push(message_id);
                    result.errors.push(format!("{}: {}", message_id, detail));
                }
            }
        }
    }
    result
}

fn replace_continuation_rollback_key(channel_id: ChannelId, message_id: MessageId) -> (u64, u64) {
    (channel_id.get(), message_id.get())
}

fn replace_continuation_rollback_root() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| {
        root.join("runtime")
            .join("discord_replace_continuation_rollbacks")
    })
}

fn replace_continuation_rollback_path(key: (u64, u64)) -> Option<PathBuf> {
    let (channel_id, message_id) = key;
    replace_continuation_rollback_root().map(|root| {
        root.join(channel_id.to_string())
            .join(format!("{message_id}.json"))
    })
}

fn load_persisted_replace_continuation_rollback(key: (u64, u64)) -> Option<Vec<u64>> {
    let path = replace_continuation_rollback_path(key)?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            // Non-UTF8 means the sidecar content is corrupt, just like a JSON
            // parse failure below. Remove it so a bad-content file cannot warn
            // forever on every future claim attempt.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to decode continuation rollback sidecar; removing corrupt sidecar and treating as no debt"
            );
            let _ = fs::remove_file(&path);
            return None;
        }
        Err(error) => {
            // Fail open WITHOUT removing the file: a read error (EIO, fd
            // exhaustion, EACCES) is transient-environment evidence, not
            // corruption evidence — removing here would permanently destroy
            // valid debt that a later claim could still read. Only the parse
            // arm below (unparseable content = a genuinely bad file) removes.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to read continuation rollback sidecar; leaving file in place and treating as no debt for this claim"
            );
            return None;
        }
    };
    let persisted: PersistedReplaceContinuationRollback = match serde_json::from_str(&content) {
        Ok(persisted) => persisted,
        Err(error) => {
            // Fail open for corrupt sidecars: runtime_store::atomic_write uses a
            // temp file, fsync, and same-dir rename, so torn files cannot be
            // self-produced. Treating corrupt data as debt would reintroduce the
            // r4 permanent send-block without a bounded probe.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to parse continuation rollback sidecar; removing corrupt sidecar and treating as no debt"
            );
            let _ = fs::remove_file(&path);
            return None;
        }
    };
    if persisted.message_ids.is_empty() {
        let _ = fs::remove_file(&path);
        return None;
    }
    (!persisted.message_ids.is_empty()).then_some(persisted.message_ids)
}

fn replace_continuation_rollback_cleared_marker() -> Result<String, String> {
    serde_json::to_string_pretty(&PersistedReplaceContinuationRollback {
        message_ids: Vec::new(),
    })
    .map_err(|error| format!("serialize cleared continuation rollback marker: {error}"))
}

fn persist_replace_continuation_rollback(
    key: (u64, u64),
    message_ids: &[u64],
) -> Result<PersistReplaceContinuationRollbackOutcome, String> {
    let Some(path) = replace_continuation_rollback_path(key) else {
        return Err("runtime root unavailable for continuation rollback".to_string());
    };
    if message_ids.is_empty() {
        match remove_replace_continuation_rollback_file(key, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let cleared_marker = replace_continuation_rollback_cleared_marker()?;
                super::runtime_store::atomic_write(&path, &cleared_marker).map_err(
                    |write_error| {
                        format!(
                            "remove continuation rollback {}: {error}; write cleared marker failed: {write_error}",
                            path.display()
                        )
                    },
                )?;
                return Ok(PersistReplaceContinuationRollbackOutcome::ClearedMarkerWritten);
            }
        }
        return Ok(PersistReplaceContinuationRollbackOutcome::Removed);
    }
    let persisted = PersistedReplaceContinuationRollback {
        message_ids: message_ids.to_vec(),
    };
    let json = serde_json::to_string_pretty(&persisted)
        .map_err(|error| format!("serialize continuation rollback: {error}"))?;
    super::runtime_store::atomic_write(&path, &json)?;
    Ok(PersistReplaceContinuationRollbackOutcome::Recorded)
}

fn claim_replace_continuation_rollback(key: (u64, u64)) -> ReplaceContinuationRollbackClaim {
    let mut rollbacks = REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let rollback = match rollbacks.get_mut(&key) {
        Some(rollback) if rollback.message_ids.is_empty() => {
            return ReplaceContinuationRollbackClaim::None;
        }
        Some(rollback) => rollback,
        None => {
            let Some(message_ids) = load_persisted_replace_continuation_rollback(key) else {
                return ReplaceContinuationRollbackClaim::None;
            };
            rollbacks.insert(
                key,
                ReplaceContinuationRollback {
                    message_ids,
                    claimed: true,
                },
            );
            return ReplaceContinuationRollbackClaim::Owner(
                rollbacks
                    .get(&key)
                    .map(|entry| entry.message_ids.clone())
                    .unwrap_or_default(),
            );
        }
    };
    if rollback.claimed {
        ReplaceContinuationRollbackClaim::InProgress(rollback.message_ids.clone())
    } else {
        rollback.claimed = true;
        ReplaceContinuationRollbackClaim::Owner(rollback.message_ids.clone())
    }
}

fn record_replace_continuation_rollback(
    key: (u64, u64),
    message_ids: Vec<u64>,
) -> Result<(), String> {
    let mut rollbacks = REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let outcome = persist_replace_continuation_rollback(key, &message_ids)?;
    if message_ids.is_empty() {
        match outcome {
            PersistReplaceContinuationRollbackOutcome::ClearedMarkerWritten => {
                rollbacks.insert(
                    key,
                    ReplaceContinuationRollback {
                        message_ids: Vec::new(),
                        claimed: false,
                    },
                );
            }
            PersistReplaceContinuationRollbackOutcome::Removed
            | PersistReplaceContinuationRollbackOutcome::Recorded => {
                rollbacks.remove(&key);
            }
        }
    } else {
        rollbacks.insert(
            key,
            ReplaceContinuationRollback {
                message_ids: message_ids.clone(),
                claimed: false,
            },
        );
    }
    Ok(())
}

fn record_replace_continuation_rollback_memory_only(key: (u64, u64), message_ids: Vec<u64>) {
    if message_ids.is_empty() {
        return;
    }
    REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            key,
            ReplaceContinuationRollback {
                message_ids,
                claimed: false,
            },
        );
}

fn clear_replace_continuation_rollback_memory_only(key: (u64, u64)) {
    REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            key,
            ReplaceContinuationRollback {
                message_ids: Vec::new(),
                claimed: false,
            },
        );
}

fn clear_replace_continuation_rollback(key: (u64, u64)) -> Result<(), String> {
    let mut rollbacks = REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    match persist_replace_continuation_rollback(key, &[])? {
        PersistReplaceContinuationRollbackOutcome::ClearedMarkerWritten => {
            rollbacks.insert(
                key,
                ReplaceContinuationRollback {
                    message_ids: Vec::new(),
                    claimed: false,
                },
            );
        }
        PersistReplaceContinuationRollbackOutcome::Removed
        | PersistReplaceContinuationRollbackOutcome::Recorded => {
            rollbacks.remove(&key);
        }
    }
    Ok(())
}

fn unclaim_replace_continuation_rollback(key: (u64, u64)) {
    if let Some(rollback) = REPLACE_CONTINUATION_ROLLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get_mut(&key)
    {
        rollback.claimed = false;
    }
}

#[cfg(test)]
mod replace_long_message_tests {
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::path::Path;

    struct RuntimeRootEnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl RuntimeRootEnvGuard {
        fn new(path: &Path) -> Self {
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self { previous }
        }
    }

    impl Drop for RuntimeRootEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    // #3805 P1: a multi-chunk answer must re-anchor the completion footer onto the
    // TAIL continuation chunk (highest snowflake, #3717 latest-wins) carrying the
    // tail chunk's OWN text — NOT chunk 0, and NOT the full body (which would
    // clobber the tail chunk once the footer edit rewrites it, §4 regression).
    #[test]
    fn completion_footer_anchor_reanchors_to_last_chunk_with_tail_text() {
        let chunk0 = MessageId::new(1000);
        let full_body = "chunk-0 body ... continuation tail body";
        let tail = super::ReplaceLastChunkAnchor {
            msg_id: 2000,
            text: "continuation tail body".to_string(),
        };

        let (target_id, target_text) =
            super::watcher_completion_footer_anchor(Some(&tail), chunk0, full_body);

        // Re-anchored to the tail chunk id (2000 > 1000: never re-anchors DOWN).
        assert_eq!(target_id, MessageId::new(2000));
        assert!(target_id > chunk0, "must re-anchor to the higher snowflake");
        // Registered text is the tail chunk's OWN text, never the full body.
        assert_eq!(target_text, "continuation tail body");
        assert_ne!(target_text, full_body);
    }

    // #3805 P1: single-chunk answers have no continuation anchor → keep chunk 0 +
    // the full relay text (identical there); the fix is a strict no-op for them.
    #[test]
    fn completion_footer_anchor_single_chunk_keeps_chunk0_and_full_text() {
        let chunk0 = MessageId::new(1000);
        let full_body = "short single-chunk answer";

        let (target_id, target_text) =
            super::watcher_completion_footer_anchor(None, chunk0, full_body);

        assert_eq!(target_id, chunk0);
        assert_eq!(target_text, full_body);
    }

    #[test]
    fn partial_continuation_failure_reports_cleanup_scope() {
        let outcome = super::ReplaceLongMessageOutcome::PartialContinuationFailure {
            sent_chunks: 2,
            total_chunks: 3,
            failed_chunk_index: 2,
            sent_continuation_message_ids: vec![9001],
            cleanup_errors: Vec::new(),
            error: "timeout".to_string(),
        };

        if let super::ReplaceLongMessageOutcome::PartialContinuationFailure {
            sent_continuation_message_ids,
            cleanup_errors,
            ..
        } = &outcome
        {
            assert_eq!(sent_continuation_message_ids, &[9001]);
            assert!(cleanup_errors.is_empty());
        } else {
            panic!("expected partial continuation failure");
        }
        assert!(super::replace_long_message_outcome_to_result(outcome).is_err());
    }

    #[test]
    fn continuation_rollback_carries_failed_cleanup_ids() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(7), MessageId::new(11));

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::None
        );

        super::record_replace_continuation_rollback(key, vec![101, 202]).expect("record rollback");
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::Owner(vec![101, 202])
        );
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::InProgress(vec![101, 202])
        );

        super::record_replace_continuation_rollback(key, Vec::new()).expect("clear by record");
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::None
        );
    }

    #[test]
    fn continuation_rollback_progress_can_be_persisted_before_cleanup() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(13), MessageId::new(29));

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
        super::record_replace_continuation_rollback(key, vec![401]).expect("record rollback");
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::Owner(vec![401])
        );

        super::record_replace_continuation_rollback(key, vec![401, 402])
            .expect("record rollback progress");
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::Owner(vec![401, 402])
        );

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
    }

    #[test]
    fn continuation_rollback_memory_only_quarantines_failed_cleanup_ids() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(31), MessageId::new(37));

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
        super::record_replace_continuation_rollback_memory_only(key, vec![701, 702]);
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::Owner(vec![701, 702])
        );

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
    }

    #[test]
    fn continuation_rollback_memory_clear_suppresses_persisted_reload_4154() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(33), MessageId::new(39));

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
        super::record_replace_continuation_rollback(key, vec![711, 712]).expect("record rollback");
        let rollback_path = super::replace_continuation_rollback_path(key).expect("rollback path");
        assert!(rollback_path.exists(), "rollback sidecar must be persisted");

        super::clear_replace_continuation_rollback_memory_only(key);
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::None
        );
        assert!(
            rollback_path.exists(),
            "memory tombstone should not require disk cleanup to succeed"
        );

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
    }

    #[test]
    fn continuation_rollback_clear_remove_failure_writes_cleared_marker_4154() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(35), MessageId::new(41));

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
        super::record_replace_continuation_rollback(key, vec![721, 722]).expect("record rollback");
        let rollback_path = super::replace_continuation_rollback_path(key).expect("rollback path");
        assert!(rollback_path.exists(), "rollback sidecar must be persisted");

        super::force_next_replace_continuation_rollback_remove_failure(key);
        super::clear_replace_continuation_rollback(key)
            .expect("clear should write a cleared marker after remove failure");
        let marker = std::fs::read_to_string(&rollback_path).expect("cleared marker");
        assert!(
            marker.contains("\"message_ids\": []"),
            "clear marker must erase delivered rollback ids"
        );

        super::REPLACE_CONTINUATION_ROLLBACKS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key);
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::None
        );
        assert!(
            !rollback_path.exists(),
            "claiming a cleared marker should best-effort remove it"
        );
    }

    #[test]
    fn continuation_rollback_successful_clear_removes_memory_entry_4154() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(37), MessageId::new(41));

        super::record_replace_continuation_rollback(key, vec![731, 732]).expect("record rollback");
        super::clear_replace_continuation_rollback(key).expect("clear rollback");

        assert!(
            !super::REPLACE_CONTINUATION_ROLLBACKS
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key(&key),
            "successful clear should leave absence, not a permanent tombstone"
        );
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::None
        );
    }

    #[test]
    fn continuation_rollback_corrupt_sidecar_warns_open_and_removes_4154() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(39), MessageId::new(45));
        let rollback_path = super::replace_continuation_rollback_path(key).expect("rollback path");
        std::fs::create_dir_all(rollback_path.parent().expect("rollback parent"))
            .expect("create rollback parent");
        std::fs::write(&rollback_path, "{not-json").expect("write corrupt sidecar");

        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::None
        );
        assert!(
            !rollback_path.exists(),
            "corrupt rollback sidecar should be removed after fail-open"
        );
    }

    #[test]
    fn continuation_rollback_non_utf8_sidecar_warns_open_and_removes_4154() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(40), MessageId::new(46));
        let rollback_path = super::replace_continuation_rollback_path(key).expect("rollback path");
        std::fs::create_dir_all(rollback_path.parent().expect("rollback parent"))
            .expect("create rollback parent");
        std::fs::write(&rollback_path, [0xff, 0xfe, 0xfd]).expect("write non-UTF8 sidecar");

        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::None
        );
        assert!(
            !rollback_path.exists(),
            "non-UTF8 rollback sidecar should be removed after fail-open"
        );
    }

    #[test]
    fn continuation_rollback_unclaim_allows_retry_after_clear_failure() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(41), MessageId::new(43));

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
        super::record_replace_continuation_rollback(key, vec![801]).expect("record rollback");
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::Owner(vec![801])
        );
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::InProgress(vec![801])
        );
        super::unclaim_replace_continuation_rollback(key);
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::Owner(vec![801])
        );

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
    }

    #[test]
    fn continuation_rollback_survives_memory_loss_until_cleared() {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = RuntimeRootEnvGuard::new(tempdir.path());
        let key = super::replace_continuation_rollback_key(ChannelId::new(17), MessageId::new(23));

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
        super::record_replace_continuation_rollback(key, vec![301, 302]).expect("record rollback");
        let rollback_path = super::replace_continuation_rollback_path(key).expect("rollback path");
        assert!(rollback_path.exists(), "rollback sidecar must be persisted");

        super::REPLACE_CONTINUATION_ROLLBACKS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key);
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::Owner(vec![301, 302])
        );
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::InProgress(vec![301, 302])
        );

        super::clear_replace_continuation_rollback(key).expect("clear rollback");
        super::REPLACE_CONTINUATION_ROLLBACKS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key);
        assert_eq!(
            super::claim_replace_continuation_rollback(key),
            super::ReplaceContinuationRollbackClaim::None
        );
        assert!(
            !rollback_path.exists(),
            "clearing rollback must remove persisted sidecar"
        );
    }
}

/// Split a message into chunks that fit within Discord's 2000 char limit.
/// Handles code block boundaries correctly. Used by stream/slash-command/recovery
/// paths where overflow is delivered as additional inline messages. The manual
/// `/api/discord/send` route uses the shared outbound API length policy instead.
///
/// Emits structured `tracing::debug!` logs at `target: "discord::chunker"` for
/// every chunk produced (chunk_index, byte_len, boundary_kind, in_code_block).
/// This is the tracepoint referenced by issue #1043 to diagnose any case where
/// the tail of a long agent message fails to reach Discord (e.g. option block
/// A/B/C sections disappearing near the 2000-char boundary).
pub(super) fn split_message(text: &str) -> Vec<String> {
    let total_bytes = text.len();
    let mut chunks = Vec::new();
    let mut remaining = text;
    let mut remaining_chars = char_count(text);
    let mut in_code_block = false;
    let mut code_block_lang = String::new();

    while !remaining.is_empty() {
        // Reserve space for code block tags we may need to add
        let tag_overhead = if in_code_block {
            // closing ``` + opening ```lang\n
            3 + 3 + char_count(&code_block_lang) + 1
        } else {
            0
        };
        let effective_limit = DISCORD_MSG_LIMIT
            .saturating_sub(tag_overhead)
            .saturating_sub(10);

        if remaining_chars <= effective_limit {
            let mut chunk = String::new();
            if in_code_block {
                chunk.push_str("```");
                chunk.push_str(&code_block_lang);
                chunk.push('\n');
            }
            chunk.push_str(remaining);
            let byte_len = chunk.len();
            let was_in_code_block = in_code_block;
            chunks.push(chunk);
            tracing::debug!(
                target: "discord::chunker",
                chunk_index = chunks.len() - 1,
                byte_len,
                boundary_kind = "final",
                in_code_block = was_in_code_block,
                total_bytes,
                "split_message emit"
            );
            break;
        }

        // Find a safe split point.
        //
        // Issue #1043 root cause #1: when the input begins with a leading `\n`
        // and the next ~2000 chars contain no other newline, `rfind('\n')`
        // returns `Some(0)`. That made `raw_chunk` empty, the chunker emitted a
        // zero-byte chunk, and Discord's REST API rejected the send with HTTP
        // 400 ("Cannot send an empty message"). The error short-circuited
        // `send_long_message_raw` / `replace_long_message_raw`, so every later
        // chunk — including the trailing A/B/C option block users were
        // reporting missing — never reached the channel.
        //
        // Fix: if a newline split would yield a zero-byte `raw_chunk`, fall
        // back to a hard split at `safe_end` (or skip the orphan newline when
        // `safe_end` is also 0 due to a multi-byte char on the boundary).
        let safe_end = byte_index_at_char_limit(remaining, effective_limit);
        let (mut split_at, mut boundary_kind) =
            super::semantic_boundaries::message_split_boundary(remaining, safe_end, in_code_block);
        if split_at == 0 {
            if safe_end > 0 {
                split_at = safe_end;
                boundary_kind = "hard_after_leading_newline";
            } else {
                // safe_end is also 0 (e.g. multi-byte char straddling a
                // 0-char effective_limit). Skip one character to guarantee
                // forward progress and never emit an empty chunk.
                let step = remaining
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| i)
                    .unwrap_or(remaining.len());
                let skipped_chars = char_count(&remaining[..step]);
                tracing::debug!(
                    target: "discord::chunker",
                    step,
                    total_bytes,
                    "split_message advance over zero-width boundary"
                );
                remaining = &remaining[step..];
                remaining_chars = remaining_chars.saturating_sub(skipped_chars);
                continue;
            }
        }

        let (raw_chunk, rest) = remaining.split_at(split_at);
        let raw_chunk_chars = char_count(raw_chunk);
        let stripped_boundary_chars = usize::from(rest.starts_with('\n'));

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

        let byte_len = chunk.len();
        let fence_was_open_at_emit = in_code_block;
        // Defensive: never emit an empty chunk to the Discord send path.
        // (split_at == 0 is handled above; this guard catches any future
        // rewrite that could regress.)
        if chunk.is_empty() {
            tracing::warn!(
                target: "discord::chunker",
                boundary_kind,
                total_bytes,
                "split_message would have emitted an empty chunk; skipping (issue #1043 guard)"
            );
            remaining_chars = remaining_chars
                .saturating_sub(raw_chunk_chars)
                .saturating_sub(stripped_boundary_chars);
            remaining = rest.strip_prefix('\n').unwrap_or(rest);
            continue;
        }
        chunks.push(chunk);
        tracing::debug!(
            target: "discord::chunker",
            chunk_index = chunks.len() - 1,
            byte_len,
            boundary_kind,
            in_code_block = fence_was_open_at_emit,
            total_bytes,
            "split_message emit"
        );
        remaining_chars = remaining_chars
            .saturating_sub(raw_chunk_chars)
            .saturating_sub(stripped_boundary_chars);
        remaining = rest.strip_prefix('\n').unwrap_or(rest);
    }

    tracing::debug!(
        target: "discord::chunker",
        total_chunks = chunks.len(),
        total_bytes,
        "split_message done"
    );

    chunks
}

/// Build an `(inline_message, attachment)` pair for content that exceeds
/// `DISCORD_MSG_LIMIT`. The attachment carries the full unmodified `text` as a
/// `.txt` file. The inline message uses `summary` when provided (so the sender
/// controls what humans see); otherwise it falls back to a short generic notice
/// pointing at the attachment instead of dumping a blind byte-prefix of `text`.
pub(super) fn build_long_message_attachment(
    text: &str,
    summary: Option<&str>,
) -> (String, CreateAttachment) {
    let filename = format!(
        "response-{}.txt",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let attachment = CreateAttachment::bytes(text.as_bytes().to_vec(), filename);
    let inline = build_attachment_inline(text, summary);
    (inline, attachment)
}

fn build_attachment_inline(text: &str, summary: Option<&str>) -> String {
    let footer = format!("\n\n{ATTACHMENT_FOOTER_PREFIX} ({} bytes)", text.len());
    let trimmed_summary = summary.and_then(|s| {
        let t = s.trim();
        (!t.is_empty()).then_some(t)
    });

    if let Some(summary) = trimmed_summary {
        if char_count(summary) + char_count(&footer) <= DISCORD_MSG_LIMIT {
            return format!("{summary}{footer}");
        }
    }

    format!(
        "📎 내용이 길어 전문을 파일로 첨부했습니다. ({} bytes)",
        text.len()
    )
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
        return ("tool", stripped);
    }
    ("other", trimmed)
}

/// Preserve the last distinct tool/thinking status in inflight state so the
/// bridge can retain prior context across stream transitions and retries.
/// Convert a still-running (`⚙`) tool status line into a terminal `⚠` form.
///
/// #1113 implicit-terminate rule: when a tool's `ToolResult` event never
/// arrives (parser error, process exit, hang, or simply because the agent
/// already moved on to the next `ToolUse` / `Thinking` event), the trailing
/// `⚙` marker is no longer accurate — the tool is not running anymore, just
/// orphaned. Convert the marker to `⚠` so the placeholder/transcript reflects
/// "terminated without an explicit result" rather than presenting a stale
/// in-progress indicator.
///
/// Lines that already carry a terminal marker (`✓`, `✗`, `⚠`, `⏱`, `💭`,
/// etc.) are returned unchanged so this can be applied unconditionally on
/// transition boundaries without risk of double-rewriting.
pub(super) fn finalize_in_progress_tool_status(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("⚙ ") {
        format!("⚠ {rest}")
    } else if let Some(rest) = line.strip_prefix("⚙") {
        format!("⚠{rest}")
    } else {
        line.to_string()
    }
}

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
    // Thinking: show more detail than tool invocations, but keep the final
    // placeholder edit compact even for UTF-8-heavy text.
    if tool_line.starts_with("💭") {
        return truncate_for_status_bytes(tool_line, THINKING_STATUS_MAX_BYTES);
    }
    // Everything else: show the raw tool line, truncated more aggressively.
    truncate_for_status_bytes(tool_line, TOOL_STATUS_MAX_BYTES)
}

/// Reason label shown in the monitor handoff placeholder. Mirrors #1324 wording
/// so users see what is happening instead of internal mechanism names such as
/// "async dispatch". `Queued` (#1332) is paired with `MonitorHandoffStatus::Queued`
/// to render the mailbox-queued placeholder card (앞선 턴 진행 중). `InlineTimeout`
/// and `ExplicitCall` are exposed for downstream wiring (#1113, #1115 sweeper).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MonitorHandoffReason {
    AsyncDispatch,
    #[allow(dead_code)] // #3034: matched in label() but not constructed; reserved for #1113 wiring
    InlineTimeout,
    ExplicitCall,
    Queued,
}

impl MonitorHandoffReason {
    fn label(self) -> &'static str {
        match self {
            Self::AsyncDispatch => "응답 스트리밍 중",
            Self::InlineTimeout => "응답 지연 — watcher 이어받음",
            Self::ExplicitCall => "백그라운드 도구 실행 중",
            Self::Queued => "앞선 턴 진행 중",
        }
    }
}

/// Lifecycle status of a monitor handoff placeholder. Drives the leading
/// emoji/title pair shown to the user. Terminal variants (Completed / Failed /
/// TimedOut / Aborted) are exposed for downstream wiring (#1115 sweeper, watcher
/// terminal updates). `Queued` (#1332) is the pre-active state used while a user
/// message waits for the mailbox dequeue.
#[derive(Debug, Clone, Copy)]
pub(super) enum MonitorHandoffStatus<'a> {
    Queued,
    Active,
    Stalled,
    Completed,
    // #3034: matched in renderers but not constructed; reserved for #1115 sweeper wiring
    #[allow(dead_code)]
    Failed {
        reason: &'a str,
    },
    TimedOut,
    Aborted,
}

const MONITOR_HANDOFF_TOOL_MAX_BYTES: usize = 80;
const MONITOR_HANDOFF_COMMAND_MAX_BYTES: usize = 80;
const MONITOR_HANDOFF_REASON_DETAIL_MAX_BYTES: usize = 80;

fn monitor_handoff_uses_background_label(reason: MonitorHandoffReason) -> bool {
    matches!(reason, MonitorHandoffReason::ExplicitCall)
}

fn monitor_handoff_header(
    status: MonitorHandoffStatus<'_>,
    reason: MonitorHandoffReason,
) -> String {
    let background_label = monitor_handoff_uses_background_label(reason);
    match status {
        MonitorHandoffStatus::Queued => "📬 **메시지 대기 중**".to_string(),
        MonitorHandoffStatus::Active if background_label => "🔄 **백그라운드 처리 중**".to_string(),
        MonitorHandoffStatus::Active => "🔄 **응답 처리 중**".to_string(),
        MonitorHandoffStatus::Stalled if background_label => "⚠ **백그라운드 정체**".to_string(),
        MonitorHandoffStatus::Stalled => "⚠ **응답 정체**".to_string(),
        MonitorHandoffStatus::Completed if background_label => "✅ **백그라운드 완료**".to_string(),
        MonitorHandoffStatus::Completed => "✅ **응답 완료**".to_string(),
        MonitorHandoffStatus::Failed { reason } => {
            let trimmed = reason.trim();
            let label = if background_label {
                "백그라운드 실패"
            } else {
                "응답 실패"
            };
            if trimmed.is_empty() {
                format!("❌ **{label}**")
            } else {
                let truncated =
                    truncate_for_status_bytes(trimmed, MONITOR_HANDOFF_COMMAND_MAX_BYTES);
                format!("❌ **{label}**: {truncated}")
            }
        }
        MonitorHandoffStatus::TimedOut if background_label => {
            "⏱ **백그라운드 타임아웃**".to_string()
        }
        MonitorHandoffStatus::TimedOut => "⏱ **응답 타임아웃**".to_string(),
        MonitorHandoffStatus::Aborted if background_label => {
            "⚠ **백그라운드 중단** (모니터 연결 끊김)".to_string()
        }
        MonitorHandoffStatus::Aborted => "⚠ **응답 중단**".to_string(),
    }
}

fn monitor_handoff_footer(
    status: MonitorHandoffStatus<'_>,
    reason: MonitorHandoffReason,
) -> &'static str {
    match status {
        MonitorHandoffStatus::Queued => "현재 진행 중인 턴 완료 후 처리 시작합니다.",
        MonitorHandoffStatus::Active if monitor_handoff_uses_background_label(reason) => {
            "완료 시 이 채널로 결과 이어서 보냅니다."
        }
        MonitorHandoffStatus::Active => "완료 시 이 채널로 결과를 이어서 표시합니다.",
        MonitorHandoffStatus::Stalled => "스트림 진행이 멈춰 복구 상태를 확인 중입니다.",
        MonitorHandoffStatus::Completed => "결과가 위에 도착했습니다.",
        MonitorHandoffStatus::Failed { .. } => "자세한 사유는 다음 응답을 확인해 주세요.",
        MonitorHandoffStatus::TimedOut => "타임아웃 임계를 넘어 종료되었습니다.",
        MonitorHandoffStatus::Aborted => "브릿지 또는 세션이 종료되었습니다.",
    }
}

fn monitor_handoff_active_tail(started_at_unix: i64) -> String {
    format!("⠋ 계속 처리 중 · 시작 <t:{started_at_unix}:R>")
}

/// Build the placeholder content shown when a turn hands off to the tmux
/// watcher (or another async monitor) for completion. Layout uses Discord
/// markdown rather than a real `CreateEmbed` — Discord's PATCH semantics
/// preserve existing embeds across `EditMessage::content(...)` updates, so
/// using a true embed would require coordinated `.embeds(vec![])` clears at
/// every downstream edit/replace path. Markdown content satisfies the same
/// information-density goal while keeping watcher edit/replace paths
/// agnostic. The `<t:UNIX:R>` tag renders as a Discord-native relative
/// timestamp on the client, so we don't need server-side periodic refresh.
pub(super) fn build_monitor_handoff_placeholder(
    status: MonitorHandoffStatus<'_>,
    reason: MonitorHandoffReason,
    started_at_unix: i64,
    tool_summary: Option<&str>,
    command_summary: Option<&str>,
) -> String {
    build_monitor_handoff_placeholder_with_context(
        status,
        reason,
        started_at_unix,
        tool_summary,
        command_summary,
        None,
        None,
        None,
        None,
    )
}

const MONITOR_HANDOFF_CONTEXT_MAX_BYTES: usize = 200;
const MONITOR_HANDOFF_REQUEST_MAX_BYTES: usize = 200;
const MONITOR_HANDOFF_PROGRESS_MAX_BYTES: usize = 200;

/// Variant of `build_monitor_handoff_placeholder` that surfaces an additional
/// `context_line` slot — typically the last assistant prose line emitted just
/// before a long-running tool call (e.g. `⏳ CI 통과 신호 대기`). Issue #1255
/// requires this to give the user a "why is the agent calling this?" hint
/// without forcing them to scroll back to the streaming body.
pub(super) fn build_monitor_handoff_placeholder_with_context(
    status: MonitorHandoffStatus<'_>,
    reason: MonitorHandoffReason,
    started_at_unix: i64,
    tool_summary: Option<&str>,
    command_summary: Option<&str>,
    reason_detail: Option<&str>,
    context_line: Option<&str>,
    request_line: Option<&str>,
    progress_line: Option<&str>,
) -> String {
    build_monitor_handoff_placeholder_with_live_events(
        status,
        reason,
        started_at_unix,
        tool_summary,
        command_summary,
        reason_detail,
        context_line,
        request_line,
        progress_line,
        None,
    )
}

pub(super) fn build_monitor_handoff_placeholder_with_live_events(
    status: MonitorHandoffStatus<'_>,
    reason: MonitorHandoffReason,
    started_at_unix: i64,
    tool_summary: Option<&str>,
    command_summary: Option<&str>,
    reason_detail: Option<&str>,
    context_line: Option<&str>,
    request_line: Option<&str>,
    progress_line: Option<&str>,
    live_events_block: Option<&str>,
) -> String {
    let header = monitor_handoff_header(status, reason);
    let footer = monitor_handoff_footer(status, reason);

    let tool_field = tool_summary
        .map(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                "—".to_string()
            } else {
                truncate_for_status_bytes(trimmed, MONITOR_HANDOFF_TOOL_MAX_BYTES)
            }
        })
        .unwrap_or_else(|| "—".to_string());

    let command_line = command_summary.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_for_status_bytes(
                trimmed,
                MONITOR_HANDOFF_COMMAND_MAX_BYTES,
            ))
        }
    });

    let reason_label = reason_detail
        .and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(truncate_for_status_bytes(
                    trimmed,
                    MONITOR_HANDOFF_REASON_DETAIL_MAX_BYTES,
                ))
            }
        })
        .map(|detail| format!("{} ({detail})", reason.label()))
        .unwrap_or_else(|| reason.label().to_string());

    let context_line = context_line.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_for_status_bytes(
                trimmed,
                MONITOR_HANDOFF_CONTEXT_MAX_BYTES,
            ))
        }
    });

    let request_line = request_line.and_then(|raw| {
        let trimmed = raw.lines().next().unwrap_or("").trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_for_status_bytes(
                trimmed,
                MONITOR_HANDOFF_REQUEST_MAX_BYTES,
            ))
        }
    });

    let progress_line = progress_line.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_for_status_bytes(
                trimmed,
                MONITOR_HANDOFF_PROGRESS_MAX_BYTES,
            ))
        }
    });

    let mut lines = Vec::with_capacity(8);
    lines.push(header);
    // #1332 — the Queued card has no tool/command yet (turn has not started),
    // so collapse to a reason-only sub-line. Active/terminal cards keep the
    // dual 도구·사유 layout from #1114.
    if matches!(status, MonitorHandoffStatus::Queued) {
        let _ = (tool_field, command_line, context_line);
        lines.push(format!("> **사유**: {reason_label}"));
    } else {
        lines.push(format!(
            "> **도구**: {tool_field} · **사유**: {reason_label}",
        ));
        if let Some(request) = request_line {
            lines.push(format!("> **요청**: {request}"));
        }
        if let Some(command) = command_line {
            lines.push(format!("> **명령**: `{command}`"));
        }
        if let Some(progress) = progress_line {
            lines.push(format!("> **진행**: {progress}"));
        }
        if let Some(context) = context_line {
            lines.push(format!("> **요약**: {context}"));
        }
    }
    lines.push(format!("> **시작**: <t:{started_at_unix}:R>"));
    lines.push(footer.to_string());
    if matches!(status, MonitorHandoffStatus::Active)
        && let Some(block) = live_events_block.and_then(|raw| {
            let trimmed = raw.trim();
            (!trimmed.is_empty()).then_some(trimmed)
        })
    {
        lines.push(block.to_string());
    }
    // Push the (invisible) probe marker *before* the processing tail so the
    // Active card still ends with the "계속 처리 중" footer (#2896 regression,
    // #3051). The sweeper detects the marker via `trimmed.contains`, so its
    // position is irrelevant for detection — keeping the tail last preserves
    // the intended "tail is last" invariant.
    lines.push(PLACEHOLDER_PROBE_MARKER.to_string());
    if matches!(status, MonitorHandoffStatus::Active) {
        lines.push(monitor_handoff_active_tail(started_at_unix));
    }

    lines.join("\n")
}

/// Long-running tool classifier (#1255).
///
/// Returns `Some(MonitorHandoffReason::ExplicitCall)` when the streamed
/// `ToolUse` event refers to a tool that the issue specifies should surface
/// the live-turn placeholder card — explicitly:
///   - `Monitor` (any input — long-tail by design),
///   - `Bash` with `run_in_background=true`,
///   - `Task` / `Agent` with `run_in_background=true`.
///
/// Everything else returns `None` and is treated as a regular tool call that
/// streams its result back inline.  The tool-name comparison is
/// case-insensitive via `canonical_tool_name` so that downstream Claude code
/// providers that lower-case their tool names still trigger the placeholder.
/// Lifecycle hint paired with `MonitorHandoffReason` so the turn loop knows
/// whether `ToolResult` is the real completion signal.
///
/// - `MonitorLike`: `Monitor` tool calls deliver their final result via
///   `ToolResult`, so terminating the placeholder there is correct.
/// - `BackgroundDispatch`: `Bash`/`Task`/`Agent` with `run_in_background=true`
///   return a job/task id ack immediately; the actual work continues and is
///   read later. The placeholder must stay open until `Done` or cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LongRunningCloseTrigger {
    MonitorLike,
    BackgroundDispatch,
}

pub(super) fn classify_long_running_tool(
    name: &str,
    input: &str,
) -> Option<(
    MonitorHandoffReason,
    LongRunningCloseTrigger,
    Option<String>,
)> {
    // `Agent` is not a canonical Claude Code tool name (the canonical entry is
    // `Task`), so it would not survive `canonical_tool_name`. Match it
    // explicitly first so the Task/Agent + run_in_background path stays alive.
    let trimmed = name.trim();
    let resolved: &str = if trimmed.eq_ignore_ascii_case("Agent") {
        "Agent"
    } else {
        canonical_tool_name(trimmed)?
    };
    match resolved {
        "Monitor" => Some((
            MonitorHandoffReason::ExplicitCall,
            LongRunningCloseTrigger::MonitorLike,
            None,
        )),
        "Bash" | "Task" | "Agent" => {
            // Only escalate to the live-turn card when the call is explicitly
            // marked as background — foreground Bash/Task calls finish inline
            // and should not trigger the card.
            let v = serde_json::from_str::<serde_json::Value>(input).ok()?;
            let bg = v
                .get("run_in_background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if bg {
                let reason_detail = v
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                Some((
                    MonitorHandoffReason::ExplicitCall,
                    LongRunningCloseTrigger::BackgroundDispatch,
                    reason_detail,
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Build the spinner/status block shown in Discord placeholders.
/// Placeholder updates should surface only the currently active tool/thinking
/// line; completed prior tools remain part of the streamed body/final response.
pub(super) fn build_placeholder_status_block(
    indicator: &str,
    _prev_tool_status: Option<&str>,
    current_tool_line: Option<&str>,
    full_response: &str,
) -> String {
    let tool_status =
        subagent_notification_card::status_summary_from(current_tool_line, full_response)
            .unwrap_or_else(|| {
                humanize_tool_status(resolve_raw_tool_status(current_tool_line, full_response))
            });
    format!("{indicator} {tool_status}")
}

pub(super) fn build_processing_status_block(indicator: &str) -> String {
    format!("{indicator} 계속 처리 중")
}

pub(super) fn build_status_panel_streaming_edit_text(
    current_portion: &str,
    status_block: &str,
    provider: &crate::services::provider::ProviderKind,
) -> String {
    if current_portion.is_empty() {
        return status_block.to_string();
    }
    let formatted = format_for_discord_with_status_panel(current_portion, provider);
    build_streaming_placeholder_text(&formatted, status_block)
}

fn truncate_for_status_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    let ellipsis = "…";
    let body_budget = max_bytes.saturating_sub(ellipsis.len());
    if body_budget == 0 {
        return ellipsis.to_string();
    }

    let safe_end = floor_char_boundary(s, body_budget);
    format!("{}{}", &s[..safe_end], ellipsis)
}

fn truncate_for_status_chars(s: &str, max_chars: usize) -> String {
    let current_chars = char_count(s);
    if current_chars <= max_chars {
        return s.to_string();
    }

    let ellipsis = "…";
    let body_budget = max_chars.saturating_sub(1);
    if body_budget == 0 {
        return ellipsis.to_string();
    }

    let safe_end = byte_index_at_char_limit(s, body_budget);
    format!("{}{}", &s[..safe_end], ellipsis)
}

fn clamp_placeholder_status_block(status_block: &str) -> String {
    truncate_for_status_chars(status_block, DISCORD_MSG_LIMIT)
}
