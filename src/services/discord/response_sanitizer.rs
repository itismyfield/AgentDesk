//! Outbound response sanitizer for AgentDesk-owned hidden context.

#[path = "subagent_notification_card.rs"]
pub(in crate::services::discord) mod subagent_notification_card;

const TUI_IDLE_RESPONSE_CHROME_PREFIXES: &[&str] = &[
    "No response requested.",
    "Continue from where you left off.",
];

const HIDDEN_HEADERS: &[&str] = &[
    "[Authoritative Instructions]",
    "[Tool Policy]",
    "[Shared Agent Rules]",
    "[Channel Role Binding]",
    "[ADK API Usage]",
    "[Agent Performance",
    "[Peer Agent Directory]",
    "[Proactive Memory Guidance]",
    "[Queued Turn Rules]",
    "[User Request]",
];

const HIDDEN_LINE_PREFIXES: &[&str] = &[
    "You are chatting with a user through Discord.",
    "Discord context:",
    "Channel participants:",
    "Current working directory:",
    "When your work produces a file the user would want",
    "This delivers the file directly to the user's Discord channel.",
    "Do NOT tell the user to use /down",
    "When referencing files in your text,",
    "Discord formatting rules:",
    "This Discord channel does not support interactive prompts.",
    "Message author prefix:",
    "Reply context:",
    "These instructions are authoritative for this turn.",
];

pub(crate) fn sanitize_hidden_context(input: &str) -> String {
    if let Some(card) =
        subagent_notification_card::sanitize_start_anchored_subagent_notification(input)
    {
        return card;
    }

    let mut out = Vec::new();
    let mut in_code_block = false;
    let mut dropping_block = false;
    let mut saw_blank_in_block = false;

    for line in input.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            if !dropping_block {
                out.push(line.to_string());
            }
            continue;
        }

        if in_code_block {
            out.push(line.to_string());
            continue;
        }

        let trimmed = line.trim();
        if is_hidden_header(trimmed) {
            dropping_block = true;
            saw_blank_in_block = false;
            continue;
        }

        if dropping_block {
            if trimmed.is_empty() {
                saw_blank_in_block = true;
                continue;
            }
            if saw_blank_in_block
                && !is_hidden_line(trimmed)
                && !looks_like_hidden_continuation(trimmed)
            {
                dropping_block = false;
                saw_blank_in_block = false;
            } else {
                continue;
            }
        }

        out.push(line.to_string());
    }

    trim_blank_edges(out)
}

pub(crate) fn sanitize_hidden_context_and_strip_chrome(input: &str) -> String {
    let sanitized = sanitize_hidden_context(input);
    let stripped = strip_leading_tui_response_chrome(&sanitized);
    let redacted = redact_tool_call_wrapper_markup(&stripped);
    subagent_notification_card::sanitize_start_anchored_subagent_notification(&redacted)
        .unwrap_or(redacted)
}

/// Redact leaked tool-call/control wrapper markup that a provider can emit as
/// plain assistant text when a tool call is malformed or streamed mid-parse
/// (#3883): raw `<invoke …>`, `<parameter …>`, `<function_calls>`,
/// `<function_results>` (with an optional `antml:` namespace) and their close
/// tags. Mirrors the code-fence walk used by [`sanitize_hidden_context`] so
/// fenced examples and inline-backtick quotes that legitimately mention these
/// tags are preserved (acceptance #2).
///
/// The allowlist is deliberately NARROW — only the four wrapper tag names
/// above. It must NOT behave like the issue's broad `<[a-z_]+>` proposal, which
/// would clobber legitimate prose such as `Vec<T>`, `a < b`, HTML examples, or
/// autolinks like `<https://example>`.
fn redact_tool_call_wrapper_markup(input: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    // Any wrapper tag, open or close form.
    static WRAPPER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)</?(?:antml:)?(?:invoke|parameter|function_calls|function_results)\b[^>]*>"#,
        )
        .unwrap()
    });
    // Closers that terminate a whole leaked block (a bare `</parameter>` does
    // not — its enclosing `<invoke>`/`<function_calls>` block continues).
    static BLOCK_CLOSER_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)</(?:antml:)?(?:invoke|function_calls|function_results)\s*>"#).unwrap()
    });

    let mut out: Vec<String> = Vec::new();
    let mut in_code_block = false;
    let mut dropping_block = false;
    let mut redacted_any = false;

    for line in input.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            // A fence line is never wrapper markup; reaching one also ends any
            // in-progress unfenced redaction region.
            dropping_block = false;
            out.push(line.to_string());
            continue;
        }

        if in_code_block {
            // Preserve fenced content verbatim (acceptance #2).
            out.push(line.to_string());
            continue;
        }

        if dropping_block {
            // Inside a leaked wrapper block: drop lines until (and including)
            // the one that closes it, then resume. If no closer ever arrives
            // we drop to EOF — this is the cross-message streaming tail where
            // the opener lived in a previous chunk.
            redacted_any = true;
            if BLOCK_CLOSER_RE.is_match(line) {
                dropping_block = false;
            }
            continue;
        }

        let Some(m) = WRAPPER_RE.find(line) else {
            out.push(line.to_string());
            continue;
        };

        // Inline-backtick guard: an odd number of backticks before the match
        // means it sits inside an inline code span, i.e. the line is
        // legitimately quoting the tag — preserve it untouched (acceptance #2).
        let backticks_before = line[..m.start()].bytes().filter(|&b| b == b'`').count();
        if backticks_before % 2 == 1 {
            out.push(line.to_string());
            continue;
        }

        redacted_any = true;
        let prefix = line[..m.start()].trim_end();
        if !prefix.is_empty() {
            // Mid-line wrapper after prose: keep the prose prefix, drop the
            // wrapper remainder.
            out.push(prefix.to_string());
        }
        // The block continues onto following lines unless this same line
        // already carried its block closer (e.g. a self-contained
        // `<invoke …>…</invoke>` or a stray `</invoke>`).
        if !BLOCK_CLOSER_RE.is_match(line) {
            dropping_block = true;
        }
    }

    if redacted_any {
        tracing::warn!(
            "redacted leaked tool-call wrapper markup from outbound Discord relay (#3883)"
        );
    }

    trim_blank_edges(out)
}

/// Remove leading Claude/Codex TUI housekeeping text that can be emitted by
/// resume/meta prompts before the real assistant body. Preserve legitimate
/// prose like "No response requested. But ..." where the phrase is part of
/// the answer rather than a standalone chrome chunk.
pub(crate) fn strip_leading_tui_response_chrome(input: &str) -> String {
    let mut stripped = input;
    let mut changed = false;
    loop {
        let trimmed = stripped.trim_start();
        if let Some(prefix) = TUI_IDLE_RESPONSE_CHROME_PREFIXES
            .iter()
            .find(|prefix| leading_tui_chrome_prefix_matches(trimmed, prefix))
        {
            changed = true;
            stripped = &trimmed[prefix.len()..];
            continue;
        }
        return if changed {
            trimmed.to_string()
        } else {
            input.to_string()
        };
    }
}

fn leading_tui_chrome_prefix_matches(trimmed: &str, prefix: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix(prefix) else {
        return false;
    };
    rest.is_empty()
        || rest.starts_with('\n')
        || rest.starts_with('\r')
        || rest.chars().next().is_some_and(|ch| !ch.is_whitespace())
}

fn is_hidden_header(trimmed: &str) -> bool {
    HIDDEN_HEADERS
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn is_hidden_line(trimmed: &str) -> bool {
    HIDDEN_LINE_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn looks_like_hidden_continuation(trimmed: &str) -> bool {
    trimmed.starts_with('-')
        || trimmed.starts_with("* ")
        || trimmed.starts_with("##")
        || trimmed.starts_with('[')
        || trimmed.starts_with("scope:")
        || trimmed.starts_with("role:")
        || trimmed.starts_with("mission:")
        || trimmed.starts_with("workspace")
        || trimmed.starts_with("agentId")
        || trimmed.starts_with("endpoint")
        || trimmed.contains("memento")
        || trimmed.contains("AgentDesk")
        || trimmed.contains("Discord")
        || trimmed.contains("ProviderKind")
}

fn trim_blank_edges(lines: Vec<String>) -> String {
    let start = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map(|index| index + 1)
        .unwrap_or(start);
    lines[start..end].join("\n")
}

#[cfg(test)]
mod tool_call_wrapper_redaction_tests {
    use super::sanitize_hidden_context_and_strip_chrome;

    #[test]
    fn redacts_bare_leaked_invoke_block_from_issue_3883() {
        let input = "Done.\n<invoke name=\"Agent\">\n<parameter name=\"description\">x</parameter>\n<parameter name=\"prompt\">Step 2 — inspect…";
        let output = sanitize_hidden_context_and_strip_chrome(input);
        assert!(
            output.contains("Done."),
            "prose prefix preserved: {output:?}"
        );
        assert!(!output.contains("<invoke"), "invoke tag leaked: {output:?}");
        assert!(
            !output.contains("<parameter"),
            "parameter tag leaked: {output:?}"
        );
        assert!(
            !output.contains("Step 2 — inspect"),
            "leaked prompt body exposed: {output:?}"
        );
    }

    #[test]
    fn redacts_streaming_continuation_chunk_with_no_opener() {
        // Cross-message streaming tail: the `<invoke>` opener was in a prior
        // chunk, so this chunk starts mid-block on a bare `<parameter …>`.
        let input = "<parameter name=\"prompt\">Step 2 — inspect…";
        let output = sanitize_hidden_context_and_strip_chrome(input);
        assert!(
            output.trim().is_empty(),
            "continuation tail not redacted: {output:?}"
        );
    }

    #[test]
    fn preserves_fenced_code_block_mentioning_wrapper_tags() {
        let input = "Here is an example:\n```\n<invoke name=\"x\">\n```\nDone.";
        let output = sanitize_hidden_context_and_strip_chrome(input);
        assert!(
            output.contains("<invoke name=\"x\">"),
            "fenced example wrongly redacted: {output:?}"
        );
    }

    #[test]
    fn preserves_inline_backtick_quoted_wrapper_tag() {
        let input = "Use `<invoke name=\"x\">` to call the tool.";
        let output = sanitize_hidden_context_and_strip_chrome(input);
        assert_eq!(output, input, "inline-code quote wrongly altered");
    }

    #[test]
    fn truncates_line_at_midline_wrapper_keeping_prose_prefix() {
        let input = "All set. <invoke name=\"Agent\">\n<parameter name=\"prompt\">go";
        let output = sanitize_hidden_context_and_strip_chrome(input);
        assert_eq!(output, "All set.");
        assert!(!output.contains("<invoke"));
    }

    #[test]
    fn does_not_touch_generic_angle_brackets_or_autolinks() {
        // The allowlist must not behave like the issue's broad `<[a-z_]+>`.
        let input = "Vec<T> and a < b and <https://example.com> are all fine.";
        let output = sanitize_hidden_context_and_strip_chrome(input);
        assert_eq!(output, input, "over-broad redaction clobbered legit prose");
    }

    #[test]
    fn redaction_is_idempotent() {
        let input = "Done.\n<invoke name=\"Agent\">\n<parameter name=\"prompt\">leak</parameter>\n</invoke>";
        let once = sanitize_hidden_context_and_strip_chrome(input);
        let twice = sanitize_hidden_context_and_strip_chrome(&once);
        assert_eq!(
            once, twice,
            "sanitizer is not idempotent: {once:?} -> {twice:?}"
        );
    }

    #[test]
    fn existing_hidden_context_and_chrome_stripping_still_works() {
        // Hidden-header block is dropped, leading TUI chrome is stripped, and
        // surrounding prose survives — proving no regression from the redactor.
        let input = "No response requested.\nVisible answer.\n\n[Tool Policy]\nsecret policy text\n\nBack to visible prose.";
        let output = sanitize_hidden_context_and_strip_chrome(input);
        assert!(
            output.contains("Visible answer."),
            "answer dropped: {output:?}"
        );
        assert!(
            output.contains("Back to visible prose."),
            "tail dropped: {output:?}"
        );
        assert!(
            !output.contains("No response requested."),
            "TUI chrome leaked: {output:?}"
        );
        assert!(
            !output.contains("Tool Policy"),
            "hidden header leaked: {output:?}"
        );
        assert!(
            !output.contains("secret policy text"),
            "hidden body leaked: {output:?}"
        );
    }
}
