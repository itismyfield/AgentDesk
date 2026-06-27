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
    subagent_notification_card::sanitize_start_anchored_subagent_notification(&stripped)
        .unwrap_or(stripped)
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
