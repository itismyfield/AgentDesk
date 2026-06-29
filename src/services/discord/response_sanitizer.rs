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
    let WrapperWalk {
        out, redacted_any, ..
    } = redact_wrapper_markup_walk(input, false, false);
    if redacted_any {
        tracing::warn!(
            "redacted leaked tool-call wrapper markup from outbound Discord relay (#3883)"
        );
    }
    trim_blank_edges(out)
}

/// Redact leaked tool-call wrapper markup from a single STREAMING frozen chunk
/// (#3883). The streaming rollover path freezes contiguous slices of the raw
/// accumulated response straight to Discord, bypassing the non-streaming relay
/// sanitizer. A `<invoke>` opener can land in chunk N while its `</invoke>`
/// closer only arrives in chunk N+k, with all-interior chunks in between that
/// contain no tags at all — so per-chunk redaction cannot see across chunks.
///
/// We close that gap statefully: `already_sent_prefix` is everything already
/// frozen for this message (`full_response[..response_sent_offset]`). Replaying
/// the walk over it yields the fence / leaked-block regime in force at the
/// chunk boundary, which seeds the chunk's own walk. Thus once an unterminated
/// opener has been seen, subsequent frozen chunks stay suppressed until the
/// matching closer, and a normal chunk after the block resumes untouched.
///
/// The frozen chunk's byte length (and therefore the caller's
/// `response_sent_offset` bookkeeping) is derived from the RAW slice, never
/// from this redacted text — we only change what is displayed, not the
/// streaming offsets. When nothing is redacted the raw chunk is returned
/// byte-for-byte. When a chunk redacts down to nothing (a pure-interior leak
/// chunk) we emit a zero-width space so the Discord edit stays non-empty while
/// showing nothing.
pub(crate) fn redact_streaming_frozen_chunk(chunk: &str, already_sent_prefix: &str) -> String {
    let prefix_regime = redact_wrapper_markup_walk(already_sent_prefix, false, false);
    let WrapperWalk {
        out, redacted_any, ..
    } = redact_wrapper_markup_walk(
        chunk,
        prefix_regime.in_code_block,
        prefix_regime.dropping_block,
    );
    if !redacted_any {
        // Common path: no leak in this chunk — preserve it exactly so the
        // streaming bytes/whitespace are unchanged.
        return chunk.to_string();
    }
    tracing::warn!("redacted leaked tool-call wrapper markup from streaming frozen chunk (#3883)");
    let redacted = out.join("\n");
    if redacted.trim().is_empty() {
        // Whole chunk was interior leak: show nothing, but keep the Discord
        // edit non-empty (an empty edit is rejected by the API).
        "\u{200B}".to_string()
    } else {
        redacted
    }
}

struct WrapperWalk {
    out: Vec<String>,
    in_code_block: bool,
    dropping_block: bool,
    redacted_any: bool,
}

/// Shared line-walk behind both the non-streaming relay redactor and the
/// streaming frozen-chunk redactor (#3883). It starts from the given
/// `in_code_block` / `dropping_block` regime — both `false` for a standalone
/// message — and returns the surviving lines plus the regime at end-of-input so
/// a streaming caller can carry state across frozen-chunk boundaries.
fn redact_wrapper_markup_walk(
    input: &str,
    mut in_code_block: bool,
    mut dropping_block: bool,
) -> WrapperWalk {
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
    let mut redacted_any = false;

    for line in input.lines() {
        // Finding #1: while dropping a leaked block, block-drop takes
        // precedence over fence preservation. A ```fenced``` block that lives
        // INSIDE the leaked `<invoke>…</invoke>` must be dropped with it rather
        // than escaping through the fence branch, so this check runs BEFORE the
        // fence toggle. We deliberately do not toggle `in_code_block` while
        // dropping — fences inside the leak are part of the dropped content.
        if dropping_block {
            // Drop lines until (and including) the one that closes the block,
            // then resume. If no closer ever arrives we drop to EOF — this is
            // the cross-chunk streaming tail where the opener lived earlier.
            redacted_any = true;
            if BLOCK_CLOSER_RE.is_match(line) {
                dropping_block = false;
            }
            continue;
        }

        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            out.push(line.to_string());
            continue;
        }

        if in_code_block {
            // Preserve fenced content verbatim (acceptance #2).
            out.push(line.to_string());
            continue;
        }

        // Finding #3: preserve the line only if EVERY wrapper match on it sits
        // inside a *matched* inline code span (CommonMark: a run of N backticks
        // opens, the next run of exactly N closes). Otherwise redact from the
        // first genuinely-leaked match. This preserves valid multi-/double-
        // backtick spans, refuses to corrupt them, and still redacts a real
        // leak that merely has stray/unmatched backticks before it.
        let first_leak = WRAPPER_RE
            .find_iter(line)
            .find(|m| !is_inside_inline_code_span(line, m.start(), m.end()));
        let Some(m) = first_leak else {
            out.push(line.to_string());
            continue;
        };

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

    WrapperWalk {
        out,
        in_code_block,
        dropping_block,
        redacted_any,
    }
}

/// CommonMark-style check: does the byte range `[start, end)` of `line` fall
/// inside a *matched* inline code span? A code span opens with a run of N
/// backticks and closes with the next run of exactly N backticks; an unmatched
/// run is literal text. Used so genuinely inline-code-quoted tag mentions are
/// preserved while stray or mismatched backticks before a real leak do not
/// shield it from redaction (#3883 finding #3).
fn is_inside_inline_code_span(line: &str, start: usize, end: usize) -> bool {
    let bytes = line.as_bytes();

    // Collect backtick runs as (byte_offset, run_len).
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run_start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            runs.push((run_start, i - run_start));
        } else {
            i += 1;
        }
    }

    // Pair each opener with the next run of EXACTLY equal length; the content
    // between them is a code span. An opener with no equal-length closer is
    // literal, so we advance and try the next run as a potential opener.
    let mut idx = 0;
    while idx < runs.len() {
        let (open_off, open_len) = runs[idx];
        if let Some(rel) = runs[idx + 1..].iter().position(|&(_, len)| len == open_len) {
            let closer_idx = idx + 1 + rel;
            let content_start = open_off + open_len;
            let content_end = runs[closer_idx].0;
            if start >= content_start && end <= content_end {
                return true;
            }
            idx = closer_idx + 1;
        } else {
            idx += 1;
        }
    }
    false
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

    // Finding #1: a fenced code block that lives INSIDE a leaked wrapper block
    // must be dropped with the block, not escape through fence preservation.
    #[test]
    fn drops_fenced_code_block_nested_inside_leaked_wrapper_block() {
        let input = "Done.\n<invoke name=\"Agent\">\n<parameter name=\"prompt\">\n```rust\nlet secret = \"leak\";\n```\n</parameter>\n</invoke>\nVisible tail.";
        let output = sanitize_hidden_context_and_strip_chrome(input);
        assert!(output.contains("Done."), "prose prefix dropped: {output:?}");
        assert!(
            output.contains("Visible tail."),
            "post-block tail dropped: {output:?}"
        );
        assert!(
            !output.contains("secret"),
            "fenced secret inside leaked block escaped: {output:?}"
        );
        assert!(
            !output.contains("```"),
            "fence inside leaked block escaped: {output:?}"
        );
        assert!(!output.contains("<invoke"), "invoke tag leaked: {output:?}");
    }

    // Finding #2: streaming frozen chunks split a `<invoke>…</invoke>` across an
    // opener-only chunk, an all-interior chunk (no tags), and a closer chunk;
    // none of the leak may reach output and a following normal chunk must be
    // byte-for-byte untouched.
    #[test]
    fn streaming_suppresses_leak_split_across_opener_interior_closer_chunks() {
        use super::redact_streaming_frozen_chunk;

        let opener =
            "Working on it.\n<invoke name=\"Agent\">\n<parameter name=\"prompt\">step one ";
        let interior = "step two more leaked prompt body with no tags at all ";
        let closer = "final words</parameter>\n</invoke>\nHere is your answer.";
        let normal = "\nAnd a normal follow-up line.";

        // Chunk 1: opener-only, no prior prefix.
        let out1 = redact_streaming_frozen_chunk(opener, "");
        assert!(
            out1.contains("Working on it."),
            "prose prefix lost: {out1:?}"
        );
        assert!(!out1.contains("<invoke"), "opener leaked: {out1:?}");
        assert!(!out1.contains("<parameter"), "parameter leaked: {out1:?}");
        assert!(
            !out1.contains("step one"),
            "leaked body escaped opener chunk: {out1:?}"
        );

        // Chunk 2: all-interior, no tags. Prefix = opener.
        let out2 = redact_streaming_frozen_chunk(interior, opener);
        assert!(
            !out2.contains("leaked prompt body") && !out2.contains("step two"),
            "interior chunk escaped: {out2:?}"
        );

        // Chunk 3: closer + visible tail. Prefix = opener + interior.
        let prefix3 = format!("{opener}{interior}");
        let out3 = redact_streaming_frozen_chunk(closer, &prefix3);
        assert!(
            !out3.contains("final words"),
            "pre-closer leak escaped: {out3:?}"
        );
        assert!(!out3.contains("</invoke>"), "closer tag leaked: {out3:?}");
        assert!(
            out3.contains("Here is your answer."),
            "post-block visible tail lost: {out3:?}"
        );

        // Chunk 4: a normal chunk fully after the block — untouched.
        let prefix4 = format!("{opener}{interior}{closer}");
        let out4 = redact_streaming_frozen_chunk(normal, &prefix4);
        assert_eq!(
            out4, normal,
            "normal post-block chunk was altered: {out4:?}"
        );
    }

    // Finding #3: a valid double-backtick span containing a single-backtick tag
    // mention survives byte-for-byte, while a genuine leak that merely has a
    // stray unmatched backtick before it is still redacted.
    #[test]
    fn preserves_double_backtick_span_but_redacts_leak_with_stray_backtick_prefix() {
        let span = "``Use `<invoke name=\"x\">` literally``";
        assert_eq!(
            sanitize_hidden_context_and_strip_chrome(span),
            span,
            "valid multi-backtick span corrupted"
        );

        let leak = "Heads up: ` then <invoke name=\"Agent\">\n<parameter name=\"prompt\">do the thing</parameter>\n</invoke>";
        let out = sanitize_hidden_context_and_strip_chrome(leak);
        assert!(out.contains("Heads up:"), "prose prefix lost: {out:?}");
        assert!(
            !out.contains("<invoke"),
            "leak with stray backtick escaped: {out:?}"
        );
        assert!(!out.contains("<parameter"), "parameter leaked: {out:?}");
        assert!(
            !out.contains("do the thing"),
            "leaked body escaped: {out:?}"
        );
    }
}
