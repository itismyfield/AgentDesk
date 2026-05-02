use poise::serenity_prelude as serenity;
use regex::Regex;
use serenity::{ChannelId, CreateAttachment, MessageId};
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use super::{DISCORD_MSG_LIMIT, SharedData, rate_limit_wait};
use crate::utils::format::tail_with_ellipsis_bytes;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, super::Data, Error>;
const STREAMING_PLACEHOLDER_MARGIN: usize = 10;
const UTF8_ELLIPSIS_EXTRA_BYTES: usize = "…".len().saturating_sub(1);
const THINKING_STATUS_MAX_BYTES: usize = 600;
const TOOL_STATUS_MAX_BYTES: usize = 300;

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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        LongRunningCloseTrigger, MonitorHandoffReason, MonitorHandoffStatus,
        ReplaceLongMessageOutcome, build_monitor_handoff_placeholder,
        build_monitor_handoff_placeholder_with_context, build_placeholder_status_block,
        build_processing_status_block, canonical_tool_name, classify_long_running_tool,
        convert_markdown_tables, escape_for_code_fence, filter_codex_tool_logs,
        finalize_in_progress_tool_status, format_for_discord_with_provider,
        normalize_allowed_tools, preserve_previous_tool_status,
        replace_long_message_outcome_to_result, strip_codex_tool_log_lines,
    };

    #[test]
    fn escape_for_code_fence_passes_through_when_no_triple_backtick() {
        assert_eq!(escape_for_code_fence("plain text"), "plain text");
        assert_eq!(escape_for_code_fence("`one` `two`"), "`one` `two`");
        assert_eq!(
            escape_for_code_fence("``two backticks``"),
            "``two backticks``"
        );
    }

    #[test]
    fn escape_for_code_fence_breaks_triple_backtick_fences() {
        // Without escaping, "```" inside a fenced block would close the fence
        // prematurely. We split it with a zero-width space so the user still
        // sees three backticks but Discord stops treating it as a terminator.
        let zwsp = "\u{200B}";
        assert_eq!(
            escape_for_code_fence("before ``` after"),
            format!("before ``{zwsp}` after"),
        );
        // Multiple occurrences are all escaped.
        assert_eq!(
            escape_for_code_fence("a``` b ```c"),
            format!("a``{zwsp}` b ``{zwsp}`c"),
        );
    }

    #[test]
    fn replace_long_message_wrapper_treats_fallback_send_as_delivery_success() {
        assert!(
            replace_long_message_outcome_to_result(ReplaceLongMessageOutcome::EditedOriginal)
                .is_ok()
        );

        let result = replace_long_message_outcome_to_result(
            ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "HTTP 403 Forbidden".to_string(),
            },
        );
        assert!(
            result.is_ok(),
            "fallback send committed visible delivery, so recovery callers can finalize"
        );
    }

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

    // --- #1043 regression tests: chunker boundary behaviour ----------------------
    //
    // These tests freeze the current behaviour of `split_message` at 2000-char
    // boundaries to prevent regressions where the tail of long agent messages
    // (e.g. option-blocks A/B/C) disappears from Discord. They also cover the
    // "message ends mid-code-fence / mid-list" shapes that triggered #1043.

    fn reassemble(chunks: &[String]) -> String {
        // The chunker may insert ```lang\n ... \n``` wrappers around continuation
        // chunks when an unclosed code fence spans a chunk boundary, plus it may
        // drop a single `\n` separator when the break landed on a newline. For the
        // non-code-block tests we just strip those artefacts back out.
        let mut out = String::new();
        for chunk in chunks {
            if !out.is_empty() && !out.ends_with('\n') {
                // Chunker breaks at '\n' and strips the newline; re-insert it so
                // that assembled content matches the original by-line.
                out.push('\n');
            }
            out.push_str(chunk);
        }
        out
    }

    #[test]
    fn test_split_message_1990_char_single_chunk_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // 1990 chars fits comfortably inside the 2000-char limit (even after the
        // chunker's internal 10-byte safety margin).
        let body: String = "a".repeat(1990);
        assert!(body.len() < DISCORD_MSG_LIMIT);

        let chunks = split_message(&body);
        assert_eq!(chunks.len(), 1, "1990-char input must not split");
        assert_eq!(chunks[0], body, "single-chunk content must be preserved");
        assert!(chunks[0].len() <= DISCORD_MSG_LIMIT);
    }

    #[test]
    fn test_split_message_exact_2000_char_no_drop_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Exactly at the Discord limit. Because the chunker reserves 10 bytes of
        // headroom (to avoid 2001-byte overruns from UTF-8/fence-closing), we
        // expect 2 chunks; the invariant is that NO content is dropped and each
        // emitted chunk stays within the Discord limit.
        let body: String = "b".repeat(DISCORD_MSG_LIMIT);
        let chunks = split_message(&body);

        assert!(
            !chunks.is_empty(),
            "must emit at least one chunk (last-chunk-drop guard)"
        );
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                chunk.len() <= DISCORD_MSG_LIMIT,
                "chunk {i} exceeds Discord 2000-byte limit: {}",
                chunk.len()
            );
        }
        let total_bytes: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(
            total_bytes,
            body.len(),
            "no bytes lost at the exact-2000 boundary"
        );
    }

    #[test]
    fn test_split_message_2010_char_boundary_last_chunk_preserved_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Just past the limit — this is the classic off-by-one zone where the
        // final short tail (e.g. 10 extra chars of an "A/B/C 선택지" block) could
        // be silently dropped.
        let body: String = "c".repeat(DISCORD_MSG_LIMIT + 10);
        let chunks = split_message(&body);

        assert!(chunks.len() >= 2, "2010-char input must split");
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
        }
        // The trailing tail must survive as its own chunk and must not be empty.
        let last = chunks.last().expect("at least one chunk");
        assert!(!last.is_empty(), "final chunk must not be empty");

        // Concatenated content equals the original (no pure-ascii fence wrappers
        // are inserted when there are no code fences in the source).
        let joined: String = chunks.concat();
        assert_eq!(joined, body, "no bytes lost near the 2000-char boundary");
    }

    #[test]
    fn test_split_message_input_ending_with_fence_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Agent emits a big prose block, then a closing ``` on the last line.
        // Ensure the trailing fence survives in the final chunk and is not
        // swallowed by the boundary logic.
        let mut body = String::new();
        body.push_str("```rust\n");
        body.push_str(&"let x = 1;\n".repeat(250)); // ~2750 bytes, forces split
        body.push_str("```");

        let chunks = split_message(&body);
        assert!(chunks.len() >= 2, "must split");
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
        }
        // Final chunk must end with the closing fence from the source.
        let last = chunks.last().expect("at least one chunk");
        assert!(
            last.trim_end().ends_with("```"),
            "trailing ``` must not be dropped; final chunk ends with: {:?}",
            &last[last.len().saturating_sub(40)..]
        );
    }

    #[test]
    fn test_split_message_multiple_fences_keeps_each_chunk_balanced_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Two separate fenced blocks separated by prose, large enough to force
        // mid-fence splits. Every emitted chunk must have a balanced number of
        // ``` markers so Discord doesn't render half the message as a code block
        // and silently truncate option tails.
        let mut body = String::new();
        body.push_str("intro line\n");
        body.push_str("```python\n");
        body.push_str(&"print('hello world')\n".repeat(60));
        body.push_str("```\n");
        body.push_str("middle prose\n");
        body.push_str("```bash\n");
        body.push_str(&"echo 'x'\n".repeat(200));
        body.push_str("```\n");
        body.push_str("outro line — option A/B/C");

        let chunks = split_message(&body);
        assert!(chunks.len() >= 2);
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                chunk.len() <= DISCORD_MSG_LIMIT,
                "chunk {i} too big: {}",
                chunk.len()
            );
            let fence_count = chunk.matches("```").count();
            assert!(
                fence_count % 2 == 0,
                "chunk {i} has unbalanced ``` fences ({fence_count}); Discord would render this as an open code block and hide the tail"
            );
        }
        // The trailing "option A/B/C" sentinel MUST appear in the last chunk.
        let last = chunks.last().unwrap();
        assert!(
            last.contains("option A/B/C"),
            "final option block disappeared: {last:?}"
        );
    }

    #[test]
    fn test_split_message_ending_mid_bullet_list_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Simulates an agent reply that overflows the first chunk and ends with
        // a short bullet-list tail (the exact shape #1043 reported missing).
        let filler: String = "filler line\n".repeat(180); // ~2160 bytes
        let tail = "\n- 선택지 A: 계속\n- 선택지 B: 중단\n- 선택지 C: 보류";
        let body = format!("{filler}{tail}");

        let chunks = split_message(&body);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
        }

        // The combined chunk content must still contain every option bullet.
        let joined = reassemble(&chunks);
        for opt in ["선택지 A", "선택지 B", "선택지 C"] {
            assert!(
                joined.contains(opt),
                "{opt} was dropped; reassembled tail = {:?}",
                &joined[joined.len().saturating_sub(120)..]
            );
        }
    }

    #[test]
    fn test_split_message_last_chunk_is_short_tail_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Long body that spills a tiny (<30 byte) tail into the final chunk.
        // Verifies the final-chunk emission path is taken and no drop occurs.
        let body: String = "d".repeat(DISCORD_MSG_LIMIT + 25);
        let chunks = split_message(&body);

        assert!(chunks.len() >= 2);
        let last = chunks.last().unwrap();
        assert!(!last.is_empty(), "short tail must not be dropped");
        assert!(last.len() <= DISCORD_MSG_LIMIT);
        // All 'd's combined must equal the original length (no off-by-one).
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, body.len());
    }

    // ── #1043 chunker tail-rendering battery ─────────────────────────────────
    //
    // Reproduces the empty-chunk root cause that made `send_long_message_raw`
    // and `replace_long_message_raw` short-circuit before the trailing chunks
    // hit Discord (the user-visible "선택지 A/B/C 끝부분이 사라짐" symptom).
    // Also locks in DoD coverage for the boundary shapes the issue called out:
    // 1990–2010 char window, fenced-fence-close at boundary, list/heading
    // markers at boundary, multi-byte (한글, emoji) at boundary.

    /// Convenience: every emitted chunk must be non-empty and fit within the
    /// Discord byte limit. An empty chunk is the exact failure mode that
    /// silently dropped trailing content in #1043 (Discord 400-rejected the
    /// payload, aborting the send loop before the tail was delivered).
    fn assert_no_empty_chunks(chunks: &[String]) {
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                !chunk.is_empty(),
                "chunk {i} is empty — Discord would reject this with HTTP 400 \
                 and short-circuit the send loop, dropping the tail (issue #1043)"
            );
        }
    }

    #[test]
    fn test_split_message_leading_newline_no_empty_chunk_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // ROOT CAUSE REPRODUCER (issue #1043): a leading `\n` followed by a
        // long stretch (>= 1990 bytes) with no other newlines made the chunker
        // emit a 0-byte chunk[0]. Discord rejects empty content with HTTP 400,
        // which short-circuited send_long_message_raw → the trailing 선택지
        // A/B/C block never reached the channel.
        let mut body = String::new();
        body.push('\n');
        body.push_str(&"X".repeat(2500));
        body.push_str("\n선택지 A: 계속\n선택지 B: 중단\n선택지 C: 보류");

        let chunks = split_message(&body);
        assert_no_empty_chunks(&chunks);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
        }
        let last = chunks.last().expect("chunks");
        assert!(
            last.contains("선택지 C: 보류"),
            "trailing 선택지 C block must survive (issue #1043 — was dropped \
             when chunk[0] was emitted empty); last chunk = {:?}",
            &last[last.len().saturating_sub(80)..]
        );
    }

    #[test]
    fn test_split_message_only_newline_at_index_zero_falls_back_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Stress: build an input where the only `\n` in the first 1990-byte
        // window is at byte 0. Without the #1043 fix `rfind('\n')` returned
        // `Some(0)`, raw_chunk was "", and the empty chunk poisoned the send.
        let mut body = String::new();
        body.push('\n');
        body.push_str(&"a".repeat(DISCORD_MSG_LIMIT * 2));
        let chunks = split_message(&body);
        assert_no_empty_chunks(&chunks);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
        }
        // The total emitted bytes must be at least the body length minus the
        // single leading-newline separator the chunker is allowed to drop
        // (it acts as a chunk delimiter, like in the multi-chunk newline path).
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert!(
            total + 1 >= body.len(),
            "lost more than the leading separator newline: emitted {total} of {}",
            body.len()
        );
    }

    #[test]
    fn test_split_message_2005_char_boundary_tail_survives_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // 2005 bytes — the exact "near 2000-char-per-message boundary" zone
        // from the bug report. The 5-byte tail must come back as a real chunk.
        let body: String = "y".repeat(2005);
        let chunks = split_message(&body);
        assert_no_empty_chunks(&chunks);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
        }
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, body.len(), "no bytes lost at 2005-char boundary");
    }

    #[test]
    fn test_split_message_fenced_close_at_boundary_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Fenced code block whose closing ``` lands ~at the 2000-byte boundary,
        // followed by the visible A/B/C tail. The tail must reach a non-empty
        // final chunk and every chunk must keep ``` fences balanced (else
        // Discord renders the rest as code and visually "eats" the tail).
        let mut body = String::new();
        body.push_str("```rust\n");
        body.push_str(&"let _x = 1;\n".repeat(160)); // ~1920 bytes inside fence
        body.push_str("```\n");
        body.push_str("선택지 A — 계속\n선택지 B — 중단\n선택지 C — 보류");

        let chunks = split_message(&body);
        assert_no_empty_chunks(&chunks);
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT, "chunk {i} too big");
            let fence_count = chunk.matches("```").count();
            assert!(
                fence_count % 2 == 0,
                "chunk {i} has unbalanced fences ({fence_count}); Discord \
                 would render the tail as code and hide the option block"
            );
        }
        let last = chunks.last().unwrap();
        assert!(
            last.contains("선택지 C — 보류"),
            "trailing option C disappeared near fenced-close boundary"
        );
    }

    #[test]
    fn test_split_message_list_marker_at_boundary_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Content fills almost a full chunk and then continues straight into a
        // bullet list. The list markers themselves must not be eaten and the
        // last bullet must survive in a non-empty chunk.
        let filler: String = "filler text line that fills the chunk\n".repeat(52); // ~1976b
        let body = format!("{filler}- 선택지 A: 계속\n- 선택지 B: 중단\n- 선택지 C: 보류");
        let chunks = split_message(&body);
        assert_no_empty_chunks(&chunks);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
        }
        let joined = reassemble(&chunks);
        for opt in ["- 선택지 A", "- 선택지 B", "- 선택지 C"] {
            assert!(
                joined.contains(opt),
                "{opt} dropped near list-marker boundary"
            );
        }
    }

    #[test]
    fn test_split_message_heading_at_boundary_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // A `# 마지막 섹션` heading lands exactly at the chunk boundary, with
        // its body in the next chunk. The heading text and the body's tail
        // both have to survive without the chunker swallowing either.
        let mut body = String::new();
        body.push_str(&"a".repeat(1985));
        body.push_str("\n# 마지막 섹션\n");
        body.push_str("선택지 A — 계속");

        let chunks = split_message(&body);
        assert_no_empty_chunks(&chunks);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
        }
        let joined = reassemble(&chunks);
        assert!(
            joined.contains("# 마지막 섹션"),
            "heading at boundary was dropped"
        );
        assert!(
            joined.contains("선택지 A — 계속"),
            "section body after heading was dropped"
        );
    }

    #[test]
    fn test_split_message_multibyte_hangul_at_boundary_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Korean text composed of 3-byte chars such that the chunk boundary
        // lands inside one of them — floor_char_boundary must pull back to a
        // valid char start, and no chunk may be empty or unbalanced.
        let body: String = "한글입니다 ".repeat(160); // 16 bytes * 160 ≈ 2560 bytes
        assert!(body.len() > DISCORD_MSG_LIMIT);
        let chunks = split_message(&body);
        assert_no_empty_chunks(&chunks);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
            assert!(
                chunk.is_char_boundary(0) && chunk.is_char_boundary(chunk.len()),
                "chunk straddles a multi-byte char boundary: {:?}",
                &chunk[..chunk.len().min(20)]
            );
        }
        // Joined ≥ original minus the boundary newlines the chunker drops as
        // separators (here zero, since the input has no '\n').
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, body.len());
    }

    #[test]
    fn test_split_message_emoji_at_boundary_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // 4-byte emoji repeated until past the limit. Every chunk must start
        // and end on a UTF-8 boundary and the tail emoji must survive.
        let body: String = "🙂".repeat(550); // 4 * 550 = 2200 bytes
        let chunks = split_message(&body);
        assert_no_empty_chunks(&chunks);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
            assert!(chunk.is_char_boundary(0));
            assert!(chunk.is_char_boundary(chunk.len()));
        }
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, body.len(), "no emoji bytes dropped");
        let last = chunks.last().unwrap();
        assert!(last.ends_with("🙂"), "trailing emoji was dropped");
    }

    #[test]
    fn test_split_message_emoji_followed_by_option_block_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Long emoji prefix with no newlines, then a tail bullet block — the
        // exact "leading non-newline → tail eaten" pathology #1043 reported,
        // but with multi-byte content so the empty-chunk-skip path also has
        // to cope with char-boundary pullback.
        let mut body = String::new();
        body.push('\n');
        body.push_str(&"🙂".repeat(520)); // 2080 bytes, no newlines
        body.push_str("\n- 선택지 A\n- 선택지 B\n- 선택지 C");

        let chunks = split_message(&body);
        assert_no_empty_chunks(&chunks);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MSG_LIMIT);
            assert!(chunk.is_char_boundary(0));
            assert!(chunk.is_char_boundary(chunk.len()));
        }
        let last = chunks.last().unwrap();
        assert!(
            last.contains("- 선택지 C"),
            "trailing 선택지 C block dropped; last chunk = {:?}",
            &last[last.len().saturating_sub(80)..]
        );
    }

    #[test]
    fn test_split_message_window_boundary_1990_to_2010_sweep_issue1043() {
        use super::{DISCORD_MSG_LIMIT, split_message};

        // Sweep the 1990–2010 byte zone and assert: no empty chunks, all
        // chunks within the limit, no bytes lost. This is the "near-2000-byte
        // boundary" failure window the bug report called out.
        for n in 1990..=2010 {
            let body: String = "z".repeat(n);
            let chunks = split_message(&body);
            assert_no_empty_chunks(&chunks);
            for chunk in &chunks {
                assert!(
                    chunk.len() <= DISCORD_MSG_LIMIT,
                    "n={n}: chunk overruns DISCORD_MSG_LIMIT"
                );
            }
            let total: usize = chunks.iter().map(|c| c.len()).sum();
            assert_eq!(total, n, "n={n}: bytes lost on boundary sweep");
        }
    }

    #[test]
    fn test_build_long_message_attachment_without_summary_uses_generic_notice() {
        use super::{DISCORD_MSG_LIMIT, build_long_message_attachment};

        let long: String = "A".repeat(DISCORD_MSG_LIMIT + 5000);
        let (inline, _attachment) = build_long_message_attachment(&long, None);

        assert!(inline.len() <= DISCORD_MSG_LIMIT);
        assert!(inline.contains("전문을 파일로 첨부"));
        // Must not embed a raw prefix of the source content.
        assert!(
            !inline.contains("AAAAAAAAAA"),
            "inline should not leak body bytes: {inline}"
        );
    }

    #[test]
    fn test_build_long_message_attachment_with_summary_uses_summary() {
        use super::{DISCORD_MSG_LIMIT, build_long_message_attachment};

        let body: String = "A".repeat(DISCORD_MSG_LIMIT + 5000);
        let summary = "# AI 통합 브리핑\n- OpenAI: codex CLI 0.122.0-alpha 릴리스\n- Anthropic: Claude 4.7 1M context";
        let (inline, _attachment) = build_long_message_attachment(&body, Some(summary));

        assert!(inline.len() <= DISCORD_MSG_LIMIT);
        assert!(inline.starts_with("# AI 통합 브리핑"));
        assert!(inline.contains("전문은 첨부 파일 참고"));
    }

    #[test]
    fn test_build_long_message_attachment_empty_summary_treated_as_none() {
        use super::build_long_message_attachment;

        let body: String = "A".repeat(5000);
        let (inline, _attachment) = build_long_message_attachment(&body, Some("   \n  "));

        assert!(inline.contains("전문을 파일로 첨부"));
    }

    #[test]
    fn test_build_long_message_attachment_oversized_summary_falls_back() {
        use super::{DISCORD_MSG_LIMIT, build_long_message_attachment};

        let body: String = "A".repeat(DISCORD_MSG_LIMIT + 1000);
        let huge_summary: String = "S".repeat(DISCORD_MSG_LIMIT + 100);
        let (inline, _attachment) = build_long_message_attachment(&body, Some(&huge_summary));

        assert!(inline.len() <= DISCORD_MSG_LIMIT);
        assert!(inline.contains("전문을 파일로 첨부"));
        assert!(!inline.contains("SSSSSSSSSS"));
    }

    #[test]
    fn test_build_long_message_attachment_utf8_safe_boundary() {
        use super::{DISCORD_MSG_LIMIT, build_long_message_attachment};

        // Multi-byte summary that sits near the limit.
        let text: String = "한글🙂".repeat(1500);
        let summary: String = "요약 ".repeat(400);
        assert!(text.len() > DISCORD_MSG_LIMIT);

        let (inline, _attachment) = build_long_message_attachment(&text, Some(&summary));
        assert!(inline.is_char_boundary(inline.len()));
        assert!(inline.len() <= DISCORD_MSG_LIMIT);
    }

    #[test]
    fn test_streaming_split_boundary_prefers_paragraph_breaks() {
        use super::streaming_split_boundary;

        let text = "alpha line\n\nbeta section continues";
        let split_at = streaming_split_boundary(text, 14).unwrap();

        assert_eq!(&text[..split_at], "alpha line\n\n");
    }

    #[test]
    fn test_streaming_split_boundary_falls_back_to_word_boundary() {
        use super::streaming_split_boundary;

        let text = "alpha beta gamma";
        let split_at = streaming_split_boundary(text, 12).unwrap();

        assert_eq!(&text[..split_at], "alpha beta ");
    }

    #[test]
    fn test_plan_streaming_rollover_keeps_raw_frozen_chunk() {
        use super::plan_streaming_rollover;

        let current_portion = format!("{}\n\n\n{}", "a".repeat(1500), "b".repeat(700));
        let plan = plan_streaming_rollover(&current_portion, "⏳ status").unwrap();

        assert_eq!(plan.frozen_chunk, current_portion[..plan.split_at]);
        assert!(plan.frozen_chunk.contains("\n\n\n"));
    }

    #[test]
    fn test_plan_streaming_rollover_can_freeze_chunk_above_safe_outbound_limit() {
        use super::{DISCORD_MSG_LIMIT, plan_streaming_rollover};

        let current_portion = "x".repeat(DISCORD_MSG_LIMIT + 250);
        let plan = plan_streaming_rollover(&current_portion, "⏳ status").unwrap();

        assert!(plan.frozen_chunk.len() > 1900);
        assert!(plan.frozen_chunk.len() <= DISCORD_MSG_LIMIT);
        assert_eq!(plan.frozen_chunk, current_portion[..plan.split_at]);
    }

    #[test]
    fn test_build_streaming_placeholder_text_keeps_ascii_snapshot_behavior() {
        use super::{DISCORD_MSG_LIMIT, build_streaming_placeholder_text, normalize_empty_lines};

        let current_portion = format!("{}\n\n{}", "alpha ".repeat(260), "omega ".repeat(120));
        let status_block = "⏳ status";
        let footer = format!("\n\n{status_block}");
        let legacy_body_budget = DISCORD_MSG_LIMIT
            .saturating_sub(footer.len() + super::STREAMING_PLACEHOLDER_MARGIN)
            .max(1);
        let expected_body = crate::utils::format::tail_with_ellipsis(
            &normalize_empty_lines(&current_portion),
            legacy_body_budget,
        );

        assert_eq!(
            build_streaming_placeholder_text(&current_portion, status_block),
            format!("{expected_body}{footer}")
        );
    }

    #[test]
    fn test_build_streaming_placeholder_text_respects_utf8_byte_limit() {
        use super::{DISCORD_MSG_LIMIT, build_streaming_placeholder_text};

        let current_portion = format!("{}\n{}", "한글🙂".repeat(320), "끝".repeat(300));
        let status_block = "⏳ 상태 업데이트";
        let placeholder = build_streaming_placeholder_text(&current_portion, status_block);

        assert!(placeholder.len() <= DISCORD_MSG_LIMIT);
        assert!(placeholder.ends_with(&format!("\n\n{status_block}")));
        assert!(placeholder.starts_with('…'));
    }

    #[test]
    fn test_build_streaming_placeholder_text_respects_utf8_limit_for_status_only() {
        use super::{DISCORD_MSG_LIMIT, build_streaming_placeholder_text};

        let status_block = &format!("⏳ {}", "🙂".repeat(1200));
        let placeholder = build_streaming_placeholder_text("", status_block);

        assert!(placeholder.len() <= DISCORD_MSG_LIMIT);
        assert!(placeholder.ends_with('…'));
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
    fn test_preserve_previous_tool_status_keeps_previous_for_distinct_same_tool() {
        let mut prev = None;
        preserve_previous_tool_status(
            &mut prev,
            Some("✓ Bash: git status"),
            Some("⚙ Bash: cargo build"),
        );
        assert_eq!(prev.as_deref(), Some("✓ Bash: git status"));
    }

    #[test]
    fn test_build_placeholder_status_block_shows_only_current_tool() {
        let placeholder = build_placeholder_status_block(
            "⠋",
            Some("✓ Read: src/config.rs"),
            Some("⚙ Bash: cargo build"),
            "",
        );
        assert_eq!(placeholder, "⠋ ⚙ Bash: cargo build");
    }

    #[test]
    fn test_build_processing_status_block_uses_spinner_processing() {
        assert_eq!(build_processing_status_block("⠋"), "⠋ Processing...");
    }

    #[test]
    fn test_build_placeholder_status_block_keeps_utf8_text_within_byte_budget() {
        let placeholder = build_placeholder_status_block(
            "⠋",
            None,
            Some(&format!("💭 {}", "🙂".repeat(1200))),
            "",
        );
        assert!(placeholder.len() <= super::THINKING_STATUS_MAX_BYTES + 16);
        assert!(placeholder.ends_with('…'));
    }

    #[test]
    fn test_finalize_in_progress_tool_status_converts_running_marker() {
        assert_eq!(
            finalize_in_progress_tool_status("⚙ Bash: cargo build"),
            "⚠ Bash: cargo build"
        );
    }

    #[test]
    fn test_strip_codex_tool_log_lines_removes_markers_outside_code_blocks() {
        let input =
            "[Bash] /bin/zsh -lc \"ls\"\nkeep\n```\n[Read] keep in code\n```\n[Task] worker";
        let output = strip_codex_tool_log_lines(input);
        assert_eq!(output, "keep\n```\n[Read] keep in code\n```");
    }

    #[test]
    fn test_format_for_discord_with_provider_sanitizes_hidden_context() {
        let input =
            "[Authoritative Instructions]\nCurrent working directory: /tmp\n\nVisible answer.";
        let output = format_for_discord_with_provider(
            input,
            &crate::services::provider::ProviderKind::OpenCode,
        );
        assert_eq!(output, "Visible answer.");
    }

    #[test]
    fn test_finalize_in_progress_tool_status_converts_running_marker_no_space() {
        // Defensive: callers should produce "⚙ X" but tolerate "⚙X" too.
        assert_eq!(finalize_in_progress_tool_status("⚙Bash"), "⚠Bash");
    }

    #[test]
    fn test_finalize_in_progress_tool_status_passes_through_terminal_lines() {
        for line in &[
            "✓ Bash: cargo build",
            "✗ Read: missing.rs",
            "⚠ Bash: cargo build",
            "⏱ Bash: long_running",
            "💭 Thinking about the next step",
            "",
        ] {
            assert_eq!(
                finalize_in_progress_tool_status(line),
                *line,
                "expected pass-through for {line:?}"
            );
        }
    }

    #[test]
    fn test_implicit_terminate_promotes_orphan_tool_to_prev_status() {
        // Simulate the ToolUse → ToolUse transition where the first tool's
        // ToolResult never arrived. Callers in turn_bridge::run apply
        // `finalize_in_progress_tool_status` to current_tool_line before
        // calling preserve_previous_tool_status, which is what this test
        // exercises end-to-end at the helper level.
        let mut prev = None;
        let stale_running = "⚙ Bash: cargo build";
        let next = "⚙ Read: src/main.rs";
        let promoted = finalize_in_progress_tool_status(stale_running);
        preserve_previous_tool_status(&mut prev, Some(promoted.as_str()), Some(next));
        assert_eq!(prev.as_deref(), Some("⚠ Bash: cargo build"));
    }

    #[test]
    fn test_normal_completion_keeps_terminal_marker_in_prev_status() {
        // Sanity check that the implicit-terminate transform is a no-op when
        // ToolResult already promoted the marker to ✓ before the next
        // ToolUse arrives.
        let mut prev = None;
        let normal_completed = "✓ Bash: cargo build";
        let next = "⚙ Read: src/main.rs";
        let promoted = finalize_in_progress_tool_status(normal_completed);
        preserve_previous_tool_status(&mut prev, Some(promoted.as_str()), Some(next));
        assert_eq!(prev.as_deref(), Some("✓ Bash: cargo build"));
    }

    #[test]
    fn test_build_monitor_handoff_placeholder_active_with_tool() {
        let text = build_monitor_handoff_placeholder(
            MonitorHandoffStatus::Active,
            MonitorHandoffReason::AsyncDispatch,
            1_700_000_000,
            Some("⚙ Bash: cargo build"),
            None,
        );
        let expected = concat!(
            "🔄 **응답 처리 중**\n",
            "> **도구**: ⚙ Bash: cargo build · **사유**: 응답 스트림 전환 — watcher 이어받음\n",
            "> **시작**: <t:1700000000:R>\n",
            "완료 시 이 채널로 결과를 이어서 표시합니다.",
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn test_build_monitor_handoff_placeholder_active_with_command_field() {
        let text = build_monitor_handoff_placeholder(
            MonitorHandoffStatus::Active,
            MonitorHandoffReason::ExplicitCall,
            1_700_000_000,
            Some("Bash"),
            Some("cargo test --package agentdesk -- --nocapture"),
        );
        assert!(text.starts_with("🔄 **백그라운드 처리 중**\n"));
        assert!(text.contains("**도구**: Bash · **사유**: 백그라운드 도구 실행 중"));
        assert!(text.contains("**명령**: `cargo test --package agentdesk -- --nocapture`"));
        assert!(text.contains("<t:1700000000:R>"));
    }

    #[test]
    fn test_build_monitor_handoff_placeholder_terminal_states() {
        let completed = build_monitor_handoff_placeholder(
            MonitorHandoffStatus::Completed,
            MonitorHandoffReason::AsyncDispatch,
            1_700_000_000,
            None,
            None,
        );
        assert!(completed.starts_with("✅ **응답 완료**\n"));
        assert!(completed.contains("**도구**: —"));
        assert!(completed.contains("결과가 위에 도착했습니다."));

        let failed = build_monitor_handoff_placeholder(
            MonitorHandoffStatus::Failed {
                reason: "exit code 137",
            },
            MonitorHandoffReason::InlineTimeout,
            1_700_000_000,
            None,
            None,
        );
        assert!(failed.starts_with("❌ **응답 실패**: exit code 137\n"));
        assert!(failed.contains("**사유**: 응답 지연 — watcher 이어받음"));

        let timed_out = build_monitor_handoff_placeholder(
            MonitorHandoffStatus::TimedOut,
            MonitorHandoffReason::AsyncDispatch,
            1_700_000_000,
            None,
            None,
        );
        assert!(timed_out.starts_with("⏱ **응답 타임아웃**\n"));

        let aborted = build_monitor_handoff_placeholder(
            MonitorHandoffStatus::Aborted,
            MonitorHandoffReason::AsyncDispatch,
            1_700_000_000,
            None,
            None,
        );
        assert!(aborted.starts_with("⚠ **응답 중단**\n"));
    }

    // #1332: Queued status renders the dedicated `📬 메시지 대기 중` card
    // with the `> **사유**: 앞선 턴 진행 중` sub-line and the queued footer.
    #[test]
    fn test_build_monitor_handoff_placeholder_queued_renders_mailbox_card() {
        let text = build_monitor_handoff_placeholder_with_context(
            MonitorHandoffStatus::Queued,
            MonitorHandoffReason::Queued,
            1_700_000_000,
            // Tool/command/context are intentionally ignored for Queued so the
            // card cannot leak partial state from an earlier turn.
            Some("Bash"),
            Some("ls -la"),
            None,
            Some("⏳ context"),
            Some("user request"),
            Some("2 alive (#A 4m12s) / 0 closed"),
        );
        assert!(text.starts_with("📬 **메시지 대기 중**\n"));
        assert!(text.contains("> **사유**: 앞선 턴 진행 중"));
        assert!(!text.contains("> **도구**:"));
        assert!(!text.contains("> **명령**:"));
        assert!(!text.contains("> **요약**:"));
        assert!(!text.contains("> **요청**:"));
        assert!(!text.contains("> **진행**:"));
        assert!(text.contains("> **시작**: <t:1700000000:R>"));
        assert!(text.ends_with("현재 진행 중인 턴 완료 후 처리 시작합니다."));
    }

    #[test]
    fn test_build_monitor_handoff_placeholder_truncates_long_tool_and_command() {
        let long_tool = "⚙ Read: ".to_string() + &"x".repeat(500);
        let long_command = "y".repeat(500);
        let text = build_monitor_handoff_placeholder(
            MonitorHandoffStatus::Active,
            MonitorHandoffReason::AsyncDispatch,
            1_700_000_000,
            Some(&long_tool),
            Some(&long_command),
        );
        // Each truncated field should fit in MONITOR_HANDOFF_*_MAX_BYTES + ellipsis,
        // which is 80 + len("…") = 83 bytes. Verify indirectly by checking the
        // ellipsis is present and the overall message is well below Discord's
        // 2000-char limit.
        assert!(text.contains('…'));
        assert!(
            text.len() < 500,
            "expected truncated output, got {} bytes",
            text.len()
        );
    }

    // #1255: classifier covers the three trigger sources documented in the
    // issue body — Monitor (always), Bash{run_in_background=true}, and
    // Task/Agent{run_in_background=true}.  Foreground Bash and unknown tool
    // names must NOT trigger the placeholder card.
    #[test]
    fn test_classify_long_running_tool_monitor_always_triggers() {
        assert_eq!(
            classify_long_running_tool("Monitor", "{\"session\":\"x\"}"),
            Some((
                MonitorHandoffReason::ExplicitCall,
                LongRunningCloseTrigger::MonitorLike,
                None,
            ))
        );
        assert_eq!(
            classify_long_running_tool("monitor", "{}"),
            Some((
                MonitorHandoffReason::ExplicitCall,
                LongRunningCloseTrigger::MonitorLike,
                None,
            ))
        );
    }

    #[test]
    fn test_classify_long_running_tool_bash_background_only() {
        assert_eq!(
            classify_long_running_tool(
                "Bash",
                "{\"command\":\"sleep 999\",\"run_in_background\":true}"
            ),
            Some((
                MonitorHandoffReason::ExplicitCall,
                LongRunningCloseTrigger::BackgroundDispatch,
                None,
            ))
        );
        // Foreground Bash → no card (would otherwise spam users on every
        // ls/grep/cat).
        assert_eq!(
            classify_long_running_tool("Bash", "{\"command\":\"ls\"}"),
            None
        );
        assert_eq!(
            classify_long_running_tool("Bash", "{\"command\":\"ls\",\"run_in_background\":false}"),
            None
        );
    }

    #[test]
    fn test_classify_long_running_tool_unknown_returns_none() {
        assert_eq!(classify_long_running_tool("ZGrep", "{}"), None);
        assert_eq!(classify_long_running_tool("", "{}"), None);
    }

    #[test]
    fn test_classify_long_running_tool_task_or_agent_background() {
        assert_eq!(
            classify_long_running_tool(
                "Task",
                "{\"description\":\"x\",\"run_in_background\":true}"
            ),
            Some((
                MonitorHandoffReason::ExplicitCall,
                LongRunningCloseTrigger::BackgroundDispatch,
                Some("x".to_string()),
            ))
        );
        assert_eq!(
            classify_long_running_tool("Task", "{\"description\":\"x\"}"),
            None
        );
        // PR #1308 codex round-1 P2 regression: `Agent` is not in
        // `ALL_TOOLS` (the canonical entry is `Task`), so an unguarded
        // `canonical_tool_name(name)?` would short-circuit before reaching the
        // background-flag check. The classifier must keep treating `Agent`
        // with `run_in_background=true` as a live-turn placeholder trigger.
        assert_eq!(
            classify_long_running_tool(
                "Agent",
                "{\"description\":\"x\",\"run_in_background\":true}"
            ),
            Some((
                MonitorHandoffReason::ExplicitCall,
                LongRunningCloseTrigger::BackgroundDispatch,
                Some("x".to_string()),
            ))
        );
        assert_eq!(
            classify_long_running_tool("agent", "{\"run_in_background\":true}"),
            Some((
                MonitorHandoffReason::ExplicitCall,
                LongRunningCloseTrigger::BackgroundDispatch,
                None,
            ))
        );
        assert_eq!(
            classify_long_running_tool("Agent", "{\"description\":\"x\"}"),
            None
        );
    }

    #[test]
    fn test_build_monitor_handoff_placeholder_with_context_renders_summary_slot() {
        let text = build_monitor_handoff_placeholder_with_context(
            MonitorHandoffStatus::Active,
            MonitorHandoffReason::ExplicitCall,
            1_700_000_000,
            Some("Monitor"),
            None,
            None,
            Some("⏳ CI 통과 신호 대기"),
            Some("배포 상태 확인해줘\n두 번째 줄은 제외"),
            Some("2 alive (#A 4m12s, #B 1m05s) / 1 closed"),
        );
        assert!(text.contains("**요청**: 배포 상태 확인해줘"));
        assert!(text.contains("**진행**: 2 alive (#A 4m12s, #B 1m05s) / 1 closed"));
        assert!(text.contains("**요약**: ⏳ CI 통과 신호 대기"));
        assert!(text.contains("**도구**: Monitor"));
    }

    #[test]
    fn test_monitor_handoff_reason_detail_renders_agent_description() {
        let text = build_monitor_handoff_placeholder_with_context(
            MonitorHandoffStatus::Active,
            MonitorHandoffReason::ExplicitCall,
            1_700_000_000,
            Some("Agent"),
            Some("Branch ship-readiness audit"),
            Some("Branch ship-readiness audit"),
            None,
            None,
            None,
        );
        assert!(text.contains("**사유**: 백그라운드 도구 실행 중 (Branch ship-readiness audit)"));
        assert!(text.contains("**명령**: `Branch ship-readiness audit`"));
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

pub(super) fn streaming_split_boundary(text: &str, max_len: usize) -> Option<usize> {
    if max_len == 0 || text.len() <= max_len {
        return None;
    }

    let safe_end = floor_char_boundary(text, max_len);
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
        .or(whitespace_split)
        .unwrap_or(safe_end);
    let split_at = if preferred < safe_end / 2 {
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
        .saturating_sub(footer.len() + STREAMING_PLACEHOLDER_MARGIN)
        .saturating_add(UTF8_ELLIPSIS_EXTRA_BYTES)
        .max(1);
    let normalized = normalize_empty_lines(current_portion);
    let body = tail_with_ellipsis_bytes(&normalized, body_budget);
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
        .saturating_sub(footer.len() + STREAMING_PLACEHOLDER_MARGIN)
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
    let sanitized = super::response_sanitizer::sanitize_hidden_context(s);
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
    let sanitized = super::response_sanitizer::sanitize_hidden_context(s);
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
    use super::{format_for_discord, format_for_discord_with_provider};
    use crate::services::provider::ProviderKind;

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
}

/// Remove ephemeral placeholder lines (e.g. "⏳ 대기 중...") from the final
/// delivered response.  These lines are useful during streaming but should not
/// persist in the channel.
fn strip_placeholder_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let t = line.trim();
        if t.starts_with("⏳") && t.contains("대기") {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
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

        // Convert # headers to **bold** (Discord doesn't render headers in bot messages).
        // Do not inject blank lines around headers; preserve the agent's spacing as-is so
        // that mobile screens are not wasted by forced double line breaks.
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

        // List items are passed through verbatim. Blank lines around them, if any,
        // come from the agent and are preserved by the blank-line collapse below.
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

/// Send a long message using raw HTTP, splitting if necessary
pub(super) async fn send_long_message_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let payload_byte_len = text.len();
    if payload_byte_len <= DISCORD_MSG_LIMIT {
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
        match super::http::send_channel_message(http, channel_id, text).await {
            Ok(_) => {
                tracing::debug!(
                    target: "discord::chunker",
                    path = "send_long_message_raw",
                    channel_id = channel_id.get(),
                    payload_byte_len,
                    last_chunk = true,
                    outcome = "ok",
                    "discord send single done"
                );
                return Ok(());
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
    tracing::debug!(
        target: "discord::chunker",
        path = "send_long_message_raw",
        channel_id = channel_id.get(),
        payload_byte_len,
        total_chunks = total,
        "discord send begin"
    );
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
        let send_result = super::http::send_channel_message(http, channel_id, chunk).await;
        match send_result {
            Ok(_) => {
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
    replace_long_message_outcome_to_result(
        replace_long_message_raw_with_outcome(http, channel_id, message_id, text, shared).await?,
    )
}

fn replace_long_message_outcome_to_result(outcome: ReplaceLongMessageOutcome) -> Result<(), Error> {
    match outcome {
        ReplaceLongMessageOutcome::EditedOriginal => Ok(()),
        ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. } => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReplaceLongMessageOutcome {
    EditedOriginal,
    SentFallbackAfterEditFailure { edit_error: String },
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
        send_long_message_raw(http, channel_id, text, shared).await?;
        return Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { edit_error });
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
        let send_result = super::http::send_channel_message(http, channel_id, chunk).await;
        match send_result {
            Ok(_) => {
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
                tracing::warn!(
                    target: "discord::chunker",
                    path = "replace_long_message_raw",
                    channel_id = channel_id.get(),
                    chunk_index = i,
                    total_chunks = total,
                    last_chunk = is_last,
                    outcome = "err",
                    error = %err,
                    "discord replace continuation failed (issue #1043 — tail may be missing)"
                );
                return Err(err.into());
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    Ok(ReplaceLongMessageOutcome::EditedOriginal)
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
        // and the next ~2000 bytes contain no other newline, `rfind('\n')`
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
        let safe_end = floor_char_boundary(remaining, effective_limit);
        let (mut split_at, mut boundary_kind) = match remaining[..safe_end].rfind('\n') {
            Some(idx) => (idx, "newline"),
            None => (safe_end, "hard"),
        };
        if split_at == 0 {
            if safe_end > 0 {
                split_at = safe_end;
                boundary_kind = "hard_after_leading_newline";
            } else {
                // safe_end is also 0 (e.g. multi-byte char straddling a
                // 0-byte effective_limit). Skip one byte to guarantee
                // forward progress and never emit an empty chunk.
                let step = remaining
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| i)
                    .unwrap_or(remaining.len());
                tracing::debug!(
                    target: "discord::chunker",
                    step,
                    total_bytes,
                    "split_message advance over zero-width boundary"
                );
                remaining = &remaining[step..];
                continue;
            }
        }

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
        if summary.len() + footer.len() <= DISCORD_MSG_LIMIT {
            return format!("{summary}{footer}");
        }
    }

    format!(
        "📎 내용이 길어 전문을 파일로 첨부했습니다. ({} bytes)",
        text.len()
    )
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
        tracing::warn!(
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
        tracing::warn!(
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
    // placeholder edit safely below Discord's byte limit even for UTF-8-heavy text.
    if tool_line.starts_with("💭") {
        return truncate_for_status_bytes(tool_line, THINKING_STATUS_MAX_BYTES);
    }
    // Everything else: show the raw tool line, truncated more aggressively.
    truncate_for_status_bytes(tool_line, TOOL_STATUS_MAX_BYTES)
}

/// Reason label shown in the monitor handoff placeholder. Mirrors the issue
/// #1324 wording so users see what is happening instead of internal mechanism
/// names such as "async dispatch".
/// `Queued` (#1332) is paired with `MonitorHandoffStatus::Queued` to render
/// the mailbox-queued placeholder card (앞선 턴 진행 중).
/// `InlineTimeout` and `ExplicitCall` are exposed for downstream wiring
/// (#1113 lifecycle, #1115 sweeper) and are exercised via tests today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) enum MonitorHandoffReason {
    AsyncDispatch,
    InlineTimeout,
    ExplicitCall,
    Queued,
}

impl MonitorHandoffReason {
    fn label(self) -> &'static str {
        match self {
            Self::AsyncDispatch => "응답 스트림 전환 — watcher 이어받음",
            Self::InlineTimeout => "응답 지연 — watcher 이어받음",
            Self::ExplicitCall => "백그라운드 도구 실행 중",
            Self::Queued => "앞선 턴 진행 중",
        }
    }
}

/// Lifecycle status of a monitor handoff placeholder. Drives the leading
/// emoji/title pair shown to the user. Terminal variants (Completed / Failed
/// / TimedOut / Aborted) are exposed for downstream wiring (#1115 sweeper,
/// watcher terminal updates) and are exercised via tests today.
/// `Queued` (#1332) is the pre-active state used while a user message waits
/// for the mailbox dequeue.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(super) enum MonitorHandoffStatus<'a> {
    Queued,
    Active,
    Completed,
    Failed { reason: &'a str },
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
        MonitorHandoffStatus::Completed => "결과가 위에 도착했습니다.",
        MonitorHandoffStatus::Failed { .. } => "자세한 사유는 다음 응답을 확인해 주세요.",
        MonitorHandoffStatus::TimedOut => "타임아웃 임계를 넘어 종료되었습니다.",
        MonitorHandoffStatus::Aborted => "브릿지 또는 세션이 종료되었습니다.",
    }
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
    let raw_tool_status = resolve_raw_tool_status(current_tool_line, full_response);
    let tool_status = humanize_tool_status(raw_tool_status);
    format!("{indicator} {tool_status}")
}

pub(super) fn build_processing_status_block(indicator: &str) -> String {
    build_placeholder_status_block(indicator, None, None, "")
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

fn clamp_placeholder_status_block(status_block: &str) -> String {
    truncate_for_status_bytes(status_block, DISCORD_MSG_LIMIT)
}
