//! Outbound response sanitizer for AgentDesk-owned hidden context.

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
        if is_hidden_header(trimmed) || is_hidden_line(trimmed) {
            dropping_block = is_hidden_header(trimmed);
            saw_blank_in_block = false;
            continue;
        }

        if dropping_block {
            if trimmed.is_empty() {
                saw_blank_in_block = true;
                continue;
            }
            if saw_blank_in_block && !looks_like_hidden_continuation(trimmed) {
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
mod tests {
    use super::sanitize_hidden_context;

    #[test]
    fn removes_hidden_blocks_and_keeps_answer() {
        let input = "\
[Authoritative Instructions]
You are chatting with a user through Discord.
Current working directory: /tmp/project

Done. Updated `src/main.rs:12`.";
        assert_eq!(
            sanitize_hidden_context(input),
            "Done. Updated `src/main.rs:12`."
        );
    }

    #[test]
    fn removes_multiple_hidden_headers() {
        let input = "\
[Tool Policy]
If tools are needed, stay within this allowlist.

[Shared Agent Rules]
- raw logs must not be dumped

완료했습니다.";
        assert_eq!(sanitize_hidden_context(input), "완료했습니다.");
    }

    #[test]
    fn preserves_code_fences() {
        let input = "\
```text
[Authoritative Instructions]
Current working directory: /tmp/project
```
visible";
        assert_eq!(sanitize_hidden_context(input), input);
    }

    #[test]
    fn preserves_near_miss_user_text() {
        let input = "I found a bug in an Authoritative Instructions parser.";
        assert_eq!(sanitize_hidden_context(input), input);
    }
}
