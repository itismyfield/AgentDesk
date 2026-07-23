//! Structured Discord card rendering for Claude/Codex TUI
//! `<task-notification>` auto-turn events (#3075/#4055).
//!
//! Background: Claude Code / Codex inject a `<task-notification>` XML-ish block
//! into their own TUI session when a subagent / background task / dynamic
//! workflow reaches a terminal state. AgentDesk observes that injected text via
//! the SSH-direct TUI prompt relay. Before #3075 the text was mirrored verbatim
//! as a `터미널에 직접 주입된 입력` code block — a noisy machine event masquerading
//! as human input.
//!
//! This module turns a `<task-notification>` payload into a compact, scannable
//! card. `task_notification_delivery` dedupes repeated completions and edits the
//! durable message rather than posting N cards. Parsing stays deliberately
//! defensive: the payload is XML-ish with embedded free-form Markdown/JSON, so
//! only stable structured fields are extracted and generated prose is never
//! used as a control signal.
//!
//! Durable card identity/delivery now lives in `task_notification_delivery`;
//! this module owns parsing, sanitization, and rendering only.

use serde_json::Value;

/// Preview budget for a free-form Markdown `result` body rendered into a card.
/// Long subagent reports are truncated to keep the card scannable on mobile; the
/// full payload remains available via the existing output/log path.
const RESULT_PREVIEW_CHARS: usize = 1400;

/// Number of leading non-blank `result` lines surfaced as the card body preview
/// for a free-form Markdown completion report.
const RESULT_PREVIEW_LINES: usize = 10;

const DISCORD_MESSAGE_LIMIT_CHARS: usize = super::DISCORD_MSG_LIMIT;
const RESULT_PREVIEW_TRUNCATED_MARKER: &str = "… (truncated)";

/// Structured fields extracted from a `<task-notification>` payload (#3075).
///
/// All fields are optional/defensive: a real payload is XML-ish with embedded
/// content and the exact tag set varies (some repeats omit `<tool-use-id>`,
/// `<output-file>` is hidden by default, `result` may be Markdown or JSON).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TaskNotification {
    pub task_id: Option<String>,
    /// The launching tool's `tool_use_id` (e.g. `toolu_…`). Hyphenated child tag
    /// in live payloads; the panel slot key the #3393 terminal bridge pairs on.
    /// Some repeats / lost-process notifications omit it (then no slot flips).
    pub tool_use_id: Option<String>,
    pub status: Option<String>,
    pub summary: Option<String>,
    pub result: Option<String>,
    pub usage: Option<String>,
    pub duration: Option<String>,
    pub tool_uses: Option<String>,
    /// #3169: the workflow/agent `<usage>` block nests `<subagent-tokens>`,
    /// `<agent-count>`, `<duration-ms>` tags. Parsed individually so the footer
    /// renders the tool count and human-readable duration instead of
    /// leaking the raw nested XML of `usage`.
    pub subagent_tokens: Option<String>,
    pub agent_count: Option<String>,
    pub duration_ms: Option<String>,
}

/// Parse a `<task-notification>` payload into its structured fields (#3075).
///
/// Defensive XML-ish extraction: we read the inner text of the first occurrence
/// of each known tag, accepting both hyphen and underscore spellings of the
/// id/use tags (`task-id`/`task_id`, `tool-use-id`/`tool_use_id`,
/// `output-file`/`output_file`) because the observed payloads use both. Terminal
/// control sequences a TUI injector may prepend are stripped first. Unknown tags
/// and attributes are ignored. Returns the parsed fields; callers decide how to
/// render. `output-file` is intentionally NOT surfaced (hidden by default per
/// the issue) so it is not stored here.
pub(super) fn parse_task_notification(raw: &str) -> TaskNotification {
    let text = strip_terminal_controls(raw);
    let trimmed = text.trim();
    // #3169 (codex R3/R5): the real `<usage>`/`<duration>` are top-level children
    // that follow BOTH the `<summary>` and `<result>` prose fields. Restrict the
    // search to the tail after the LATER of the two real prose-field closes (max,
    // not first-found) so a literal `</result>`/`</summary>` or `<usage>…</usage>`
    // written inside the prose cannot be mistaken for the real block.
    let usage_search_from = trimmed
        .rfind("</result>")
        .into_iter()
        .chain(trimmed.rfind("</summary>"))
        .max()
        .unwrap_or(0);
    let usage = extract_tag(&trimmed[usage_search_from..], &["usage"]);
    // #3169 (codex): parse the nested usage sub-fields from WITHIN the already-
    // extracted `<usage>` block ONLY. Scanning the whole payload would let a
    // literal `<subagent_tokens>`/`<agent_count>`/`<duration_ms>` appearing inside
    // `<summary>`/`<result>` prose poison the footer metadata.
    let usage_inner = usage.as_deref().unwrap_or("");
    TaskNotification {
        task_id: extract_tag(trimmed, &["task-id", "task_id"]),
        // #3393: the launching tool's id is a distinct top-level child tag (both
        // hyphen and underscore spellings observed); scoped to the whole payload
        // like task-id, never the <usage> tail.
        tool_use_id: extract_tag(trimmed, &["tool-use-id", "tool_use_id"]),
        status: extract_tag(trimmed, &["status"]),
        summary: extract_tag(trimmed, &["summary"]),
        result: extract_tag(trimmed, &["result"]),
        // #3169 (codex R4): the footer-fallback `<duration>` is also a top-level
        // metadata child after the prose fields — scope it to the same tail so a
        // prose-literal `<duration>` can't poison the footer for legacy payloads.
        duration: extract_tag(&trimmed[usage_search_from..], &["duration"]),
        // #3169 (codex R2): scope the tool-use COUNT to the <usage> block too, so a
        // prose-literal `<tool_uses>` before the real usage cannot poison the
        // footer count (the count is always nested inside <usage>). `<tool-use-id>`
        // is a distinct top-level tag and is unaffected.
        tool_uses: extract_tag(usage_inner, &["tool-uses", "tool_uses", "tool-use-count"]),
        subagent_tokens: extract_tag(usage_inner, &["subagent-tokens", "subagent_tokens"]),
        agent_count: extract_tag(usage_inner, &["agent-count", "agent_count"]),
        duration_ms: extract_tag(usage_inner, &["duration-ms", "duration_ms"]),
        usage,
    }
}

impl TaskNotification {
    /// #3393: classify a parsed notification into the `task_notification_kind`
    /// vocabulary the live-panel status bridge consumes
    /// (`status_events_from_task_notification_with_tool_use_id`). Live payloads
    /// carry NO `kind=` attribute, so the kind is read from the `<summary>`
    /// prefix Claude Code emits:
    ///   - `Background command "…"`  → `background` (a background Bash → its
    ///     terminal status flips a `BackgroundTaskEnd` slot, keyed by tool-use-id).
    ///   - `Dynamic workflow "…"`    → `workflow`.
    ///   - `Agent "…"` / `Background agent "…"` / anything else → `subagent`
    ///     (a `SubagentEnd`; the default is the safe one — it only finalizes a
    ///     slot whose tool-use-id matches, so an unknown prefix never mis-fires).
    pub(super) fn kind(&self) -> &'static str {
        let summary = self.summary.as_deref().unwrap_or("").trim_start();
        if summary.starts_with("Background command") {
            "background"
        } else if summary.starts_with("Dynamic workflow") {
            "workflow"
        } else {
            "subagent"
        }
    }
}

/// #3169: format a raw millisecond duration as a human-readable string
/// (`504983` → `8m 25s`, `45000` → `45s`, `900` → `900ms`).
/// Returns `None` if the input is not a parseable non-negative integer.
fn format_duration_ms(raw: &str) -> Option<String> {
    let ms: u64 = raw.trim().parse().ok()?;
    Some(if ms < 1_000 {
        format!("{ms}ms")
    } else {
        let secs = ms / 1_000;
        if secs < 60 {
            format!("{secs}s")
        } else {
            let (m, s) = (secs / 60, secs % 60);
            if s == 0 {
                format!("{m}m")
            } else {
                format!("{m}m {s}s")
            }
        }
    })
}

/// Extract the inner text of the first `<name>…</name>` for any of `names`.
///
/// Tolerates attributes on the open tag (`<name attr="x">`). Returns the trimmed
/// inner text, or `None` if absent/empty. Used only for the stable structured
/// fields; never for prose pattern-matching.
fn extract_tag(haystack: &str, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = extract_one_tag(haystack, name) {
            return Some(value);
        }
    }
    None
}

fn extract_one_tag(haystack: &str, name: &str) -> Option<String> {
    let open_prefix = format!("<{name}");
    let close = format!("</{name}>");
    let open_at = haystack.find(&open_prefix)?;
    // Find the '>' that closes the (possibly attributed) open tag.
    let after_prefix = open_at + open_prefix.len();
    let rest = &haystack[after_prefix..];
    // The char right after the tag name must be '>' or whitespace (so `<status>`
    // is not matched by a hypothetical `<status-bar>` request). If it is `>` the
    // tag has no attributes; otherwise scan to the next `>`.
    let first = rest.chars().next()?;
    if first != '>' && !first.is_whitespace() {
        return None;
    }
    let gt_rel = rest.find('>')?;
    let inner_start = after_prefix + gt_rel + 1;
    let inner_end_rel = haystack[inner_start..].find(&close)?;
    let inner = &haystack[inner_start..inner_start + inner_end_rel];
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Render a `<task-notification>` payload as a compact Discord card (#3075).
///
/// `update_count` is the number of times this task-id has been seen (1 on the
/// first card). When > 1 the header notes `updated Nx` and the body shows the
/// LATEST result preview. A JSON `result` is summarized by top-level shape
/// (`results`/`reviews`/`plans`) rather than dumped raw; a free-form Markdown
/// `result` is truncated to a short preview with any PR URLs surfaced.
pub(super) fn format_task_notification_card(note: &TaskNotification, update_count: u64) -> String {
    let status = note.status.as_deref().unwrap_or("");
    let icon = status_icon(status);
    let status_label = if status.is_empty() {
        "Task event".to_string()
    } else {
        format!("Task {status}")
    };
    let mut header = format!("{icon} {status_label}");
    if update_count > 1 {
        header.push_str(&format!(" · updated {update_count}x"));
    }

    let mut lines = vec![header];

    if let Some(summary) = note.summary.as_deref().filter(|s| !s.is_empty()) {
        // #3477 item 1: preserve the summary's newlines so multi-line task
        // summaries stay readable (Discord renders multi-line bold fine). Still
        // escapes the ``` fence hazard. The footer/preview slots below keep using
        // `sanitize_oneline` so they remain compact single-line cells.
        // #4338: decode one layer of harness XML-escaping first, once, so a literal
        // `&`/`<`/`>` in the summary renders as itself rather than `&amp;`/`&lt;`.
        lines.push(format!(
            "**{}**",
            sanitize_multiline(&decode_entities_once(summary))
        ));
    }

    if let Some(result) = note.result.as_deref().filter(|s| !s.is_empty()) {
        // #4338: decode ONE layer of harness XML-escaping up front so the preview,
        // the JSON-shape summary, and PR-URL extraction all operate on the same
        // human-facing text (`&amp;provider` → `&provider`). Doing it once here
        // (not per sub-step) keeps the card a pure, idempotent function of the
        // payload — a streaming edit re-renders byte-identical.
        let result = decode_entities_once(result);
        let body = render_result_preview(&result, update_count > 1);
        if !body.is_empty() {
            lines.push(String::new());
            lines.push(body);
        }
        for url in extract_pr_urls(&result) {
            lines.push(format!("🔗 {url}"));
        }
    }

    // Footer: short task id + usage/duration metadata, all from structured
    // fields (never generated prose). `output-file` is intentionally omitted.
    let mut footer_parts: Vec<String> = Vec::new();
    if let Some(task_id) = note.task_id.as_deref().filter(|s| !s.is_empty()) {
        footer_parts.push(format!("task {}", short_task_id(task_id)));
    }
    // #3169: render the structured usage sub-fields human-readably. Multi-agent
    // count, then tool count, then `Xm Ys` duration. The token total is omitted. The
    // raw `usage` block is only surfaced as a fallback when it carries NO nested
    // tags (`<`) — the workflow/agent payload nests `<subagent-tokens>` etc.,
    // which we parse above, so we must not dump that XML into the footer.
    if let Some(agents) = note
        .agent_count
        .as_deref()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 1)
    {
        footer_parts.push(format!("{agents} agents"));
    }
    if let Some(tool_uses) = note.tool_uses.as_deref().filter(|s| !s.is_empty()) {
        footer_parts.push(format!("{} tools", sanitize_oneline(tool_uses)));
    }
    // Duration: prefer the millisecond field (human-formatted); fall back to a
    // pre-formatted `<duration>` string if that is what the payload carried.
    if let Some(dur) = note
        .duration_ms
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(format_duration_ms)
    {
        footer_parts.push(format!("⏱ {dur}"));
    } else if let Some(duration) = note.duration.as_deref().filter(|s| !s.is_empty()) {
        footer_parts.push(format!("⏱ {}", sanitize_oneline(duration)));
    }
    // Fallback: a plain (non-XML) usage string, only when no structured
    // sub-field was parsed and it is not itself nested XML.
    if note.subagent_tokens.is_none() && note.agent_count.is_none() {
        if let Some(usage) = note
            .usage
            .as_deref()
            .filter(|s| !s.is_empty() && !s.contains('<'))
            .filter(|s| {
                let normalized = s.to_ascii_lowercase();
                !normalized.contains(" tok") && !normalized.contains("token")
            })
        {
            footer_parts.push(sanitize_oneline(usage));
        }
    }
    if !footer_parts.is_empty() {
        lines.push(String::new());
        lines.push(format!("-# {}", footer_parts.join(" · ")));
    }

    clamp_discord_message_content(&lines.join("\n"))
}

fn status_icon(status: &str) -> &'static str {
    match status.trim().to_ascii_lowercase().as_str() {
        "completed" | "complete" | "done" | "success" | "succeeded" => "✅",
        "failed" | "error" | "errored" => "❌",
        "cancelled" | "canceled" | "stopped" => "🛑",
        "" => "📋",
        _ => "📋",
    }
}

/// First 8 hex-ish chars of a task id for a compact, stable footer label.
fn short_task_id(task_id: &str) -> String {
    let trimmed = task_id.trim();
    trimmed.chars().take(8).collect()
}

/// Render the `result` body: a JSON aggregate is summarized by shape; otherwise
/// a free-form Markdown body is reduced to a short preview.
fn render_result_preview(result: &str, latest: bool) -> String {
    let prefix = if latest { "Latest: " } else { "" };
    if let Some(summary) = summarize_json_result(result) {
        return format!("{prefix}{summary}");
    }
    let preview = markdown_preview(result);
    if preview.is_empty() {
        String::new()
    } else {
        blockquote_preview(&preview, prefix)
    }
}

/// If `result` parses as JSON, render a compact aggregate by top-level shape
/// (#3075 §3): `results[]` / `reviews[]` / `plans[]` counts + a short list,
/// never the raw JSON. Returns `None` for non-JSON bodies.
fn summarize_json_result(result: &str) -> Option<String> {
    let value: Value = serde_json::from_str(result.trim()).ok()?;
    let obj = value.as_object()?;
    let mut parts: Vec<String> = Vec::new();
    for (key, label) in [
        ("results", "results"),
        ("reviews", "reviews"),
        ("plans", "plans"),
    ] {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            parts.push(format!("{} {label}", arr.len()));
        }
    }
    if parts.is_empty() {
        // A JSON object/array we don't have a specific shape for: report that it
        // is a structured result without dumping it.
        let kind = if value.is_array() {
            "JSON array"
        } else {
            "JSON object"
        };
        return Some(format!("structured {kind} result (preview suppressed)"));
    }
    Some(format!("aggregate: {}", parts.join(", ")))
}

/// Short Markdown preview: first few content lines, char-capped.
fn markdown_preview(result: &str) -> String {
    let mut collected: Vec<String> = Vec::new();
    let mut counted_lines = 0usize;
    let mut collected_chars = 0usize;
    let mut state = MarkdownPreviewState::default();
    for line in result.lines() {
        let Some(line) = normalize_markdown_preview_line(line, &mut state) else {
            continue;
        };
        let counts_toward_limit = line.counts_toward_limit;
        let line = line.text;
        let line_chars = line.chars().count();
        let separator_chars = usize::from(!collected.is_empty());
        if collected_chars + separator_chars + line_chars > RESULT_PREVIEW_CHARS {
            let remaining = RESULT_PREVIEW_CHARS.saturating_sub(collected_chars + separator_chars);
            let overflow_sentinel_chars = RESULT_PREVIEW_TRUNCATED_MARKER.chars().count() + 1;
            collected.push(
                line.chars()
                    .take(remaining + overflow_sentinel_chars)
                    .collect(),
            );
            break;
        }
        collected_chars += separator_chars + line_chars;
        collected.push(line);
        if counts_toward_limit {
            counted_lines += 1;
        }
        if counted_lines >= RESULT_PREVIEW_LINES {
            break;
        }
    }
    let joined = collected.join("\n");
    truncate_preview_at_boundary(&joined, RESULT_PREVIEW_CHARS)
}

#[derive(Default)]
struct MarkdownPreviewState {
    fence: Option<MarkdownFence>,
}

#[derive(Clone, Copy)]
struct MarkdownFence {
    ch: char,
    len: usize,
    can_close: bool,
}

struct MarkdownPreviewLine {
    text: String,
    counts_toward_limit: bool,
}

fn normalize_markdown_preview_line(
    line: &str,
    state: &mut MarkdownPreviewState,
) -> Option<MarkdownPreviewLine> {
    let fence = markdown_fence_marker(line);
    let inside_fence = state.fence.is_some();
    let line = sanitize_oneline(line).trim().to_string();
    let mut is_active_fence_line = false;
    if let Some(fence) = fence {
        match state.fence {
            Some(open) if fence.can_close && fence.ch == open.ch && fence.len >= open.len => {
                state.fence = None;
                is_active_fence_line = true;
            }
            Some(_) => {}
            None => {
                state.fence = Some(fence);
                is_active_fence_line = true;
            }
        }
    }
    if line.is_empty() {
        return None;
    }
    if inside_fence || is_active_fence_line {
        return Some(MarkdownPreviewLine {
            text: line,
            counts_toward_limit: true,
        });
    }
    if is_markdown_decoration_line(&line) {
        return None;
    }
    let counts_toward_limit = !is_markdown_heading_line(&line);
    let mut text = strip_markdown_line_prefix(&line).trim().to_string();
    if text.starts_with('|') || text.ends_with('|') {
        text = text
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
    }
    if text.is_empty() || is_markdown_decoration_line(&text) {
        None
    } else {
        Some(MarkdownPreviewLine {
            text,
            counts_toward_limit,
        })
    }
}

fn markdown_fence_marker(line: &str) -> Option<MarkdownFence> {
    let trimmed = line.trim_start();
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|current| *current == ch).count();
    let can_close = trimmed[len..].trim().is_empty();
    (len >= 3).then_some(MarkdownFence { ch, len, can_close })
}

fn is_markdown_heading_line(line: &str) -> bool {
    let mut text = line.trim_start();
    while let Some(rest) = text.strip_prefix('>') {
        text = rest.trim_start();
    }
    let hashes = text.chars().take_while(|ch| *ch == '#').count();
    hashes > 0
        && text[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
}

fn strip_markdown_line_prefix(line: &str) -> &str {
    let mut text = line.trim_start();
    while let Some(rest) = text.strip_prefix('>') {
        text = rest.trim_start();
    }

    let hashes = text.chars().take_while(|ch| *ch == '#').count();
    if hashes > 0
        && text[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
    {
        return text[hashes..].trim_start();
    }

    if let Some(rest) = text
        .strip_prefix("- ")
        .or_else(|| text.strip_prefix("* "))
        .or_else(|| text.strip_prefix("+ "))
    {
        return rest;
    }

    let digit_count = text.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count > 0 {
        let after_digits = &text[digit_count..];
        if let Some(rest) = after_digits.strip_prefix(". ") {
            return rest;
        }
    }

    text
}

fn is_markdown_decoration_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '-' | '*' | '_' | '=' | '|' | ':' | ' '))
}

fn blockquote_preview(preview: &str, first_line_prefix: &str) -> String {
    let rendered = preview
        .lines()
        .enumerate()
        .map(|(idx, line)| {
            if idx == 0 {
                format!("> {first_line_prefix}{line}")
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    truncate_preview_at_boundary(&rendered, RESULT_PREVIEW_CHARS)
}

pub(super) fn clamp_discord_message_content(value: &str) -> String {
    truncate_preview_at_boundary(value, DISCORD_MESSAGE_LIMIT_CHARS)
}

fn truncate_preview_at_boundary(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }

    let marker_chars = RESULT_PREVIEW_TRUNCATED_MARKER.chars().count();
    if limit <= marker_chars {
        return RESULT_PREVIEW_TRUNCATED_MARKER
            .chars()
            .take(limit)
            .collect();
    }

    let content_limit = limit - marker_chars;
    let truncated = value.chars().take(content_limit).collect::<String>();
    let boundary = preview_boundary(&truncated);
    let clipped = truncated[..boundary].trim_end();
    let clipped = if clipped.is_empty() {
        truncated.trim_end()
    } else {
        clipped
    };
    format!("{clipped}{RESULT_PREVIEW_TRUNCATED_MARKER}")
}

fn preview_boundary(value: &str) -> usize {
    let tail_start = value.rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    let tail = &value[tail_start..];
    for (idx, ch) in tail.char_indices().rev() {
        if matches!(ch, '.' | '!' | '?' | '。' | '！' | '？') {
            return tail_start + idx + ch.len_utf8();
        }
    }
    if tail.chars().count() > 40 {
        if let Some((idx, _)) = tail.char_indices().rev().find(|(_, ch)| ch.is_whitespace()) {
            return tail_start + idx;
        }
    }
    for (idx, ch) in value.char_indices().rev() {
        if matches!(ch, '.' | '!' | '?' | '。' | '！' | '？') {
            return idx + ch.len_utf8();
        }
    }
    if let Some(pos) = value.rfind('\n') {
        if pos > value.len() / 2 {
            return pos;
        }
    }
    if let Some((idx, _)) = value
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
    {
        return idx;
    }
    value.len()
}

/// Extract up to a few distinct GitHub PR/issue URLs from a result body so they
/// are clickable in the card. Order-preserving, deduped, bounded.
fn extract_pr_urls(result: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for token in result.split(|c: char| {
        c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '"' | '\'')
    }) {
        let token = token.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!'));
        if (token.starts_with("https://github.com/") || token.starts_with("http://github.com/"))
            && (token.contains("/pull/") || token.contains("/issues/"))
            && !seen.iter().any(|u| u == token)
        {
            seen.push(token.to_string());
            if seen.len() >= 3 {
                break;
            }
        }
    }
    seen
}

/// Collapse a value to a single line and strip Discord-fence/markup hazards for
/// inline rendering.
///
/// NOTE: does NOT decode entities — that is a per-field concern applied once at
/// the point each prose field enters the card (#4338). This helper runs per
/// preview LINE, so decoding here would double-decode an already-decoded result.
fn sanitize_oneline(value: &str) -> String {
    value
        .replace('\r', " ")
        .replace('\n', " ")
        .replace("```", "` ` `")
}

/// #3477 item 1: like `sanitize_oneline` but PRESERVES newlines so multi-line
/// task summaries render readably (Discord renders multi-line bold fine). Still
/// neutralizes the ``` code-fence hazard and normalizes lone `\r` to `\n` so the
/// platform never sees a bare carriage return.
fn sanitize_multiline(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace("```", "` ` `")
}

/// #4338: decode EXACTLY ONE layer of XML/HTML entity escaping from a
/// task-notification prose field before it is rendered into a Discord card.
///
/// Background: Claude Code / Codex XML-escape the free-form `<summary>`/`<result>`
/// prose when they inject the `<task-notification>` envelope, so a subagent
/// report's literal `&`, `<`, `>` arrives as `&amp;`, `&lt;`, `&gt;`. Discord does
/// not decode HTML entities, so without this the entities leak as literal text
/// (`&provider` shown as `&amp;provider`). AgentDesk never re-escapes on the card
/// path — both the first post and every streaming edit rebuild the card from the
/// raw payload — so the fix is a single decode at render, the inverse of the
/// harness's single escape pass. Shared with the #3393 footer/status-panel bridge
/// (`status_events_from_task_notification_xml_for_footer_mode`), which renders
/// the same XML `<summary>` when the card is footer-suppressed — the two surfaces
/// each decode their own fresh parse of the raw payload, never each other's
/// output, so each applies exactly one layer.
///
/// We remove EXACTLY one layer: a source that was already escaped once (an agent
/// quoting an already-broken card) keeps its remaining literal `&amp;`, and a
/// re-render of the same payload is byte-identical (idempotent per payload). A
/// single left-to-right scan that never re-examines emitted output guarantees the
/// single-layer property independent of entity ordering (`&amp;lt;` → `&lt;`, not
/// `<`). Unrecognized `&…;` sequences and bare `&` pass through verbatim.
pub(super) fn decode_entities_once(input: &str) -> String {
    // Fast path: nothing to decode (also preserves exact bytes for the common
    // no-entity case).
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        // #4338 rework (codex r1): bound the `;` search to the longest decodable
        // body + 1 byte so a failed candidate costs O(1) — an unbounded
        // `after.find(';')` re-scans the remaining tail per bare `&`, going
        // quadratic on `&`-dense prose (`a && b` repeated). `;` is ASCII, so the
        // byte position is always a char boundary and the body slice below stays
        // UTF-8-safe; a window hit also implies `body.len() <= MAX_ENTITY_BODY_LEN`.
        let semi = after
            .bytes()
            .take(MAX_ENTITY_BODY_LEN + 1)
            .position(|b| b == b';');
        if let Some(semi) = semi {
            let body = &after[..semi];
            if !body.is_empty() {
                if let Some(ch) = decode_entity_body(body) {
                    out.push(ch);
                    rest = &after[semi + 1..];
                    continue;
                }
            }
        }
        // Not a recognized entity: emit the literal '&' and advance past it.
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Longest entity body we decode (between `&` and `;`). Covers the named refs
/// (`quot`/`apos`, 4 chars) and the widest numeric ref (`#x10FFFF`, 8 chars) with
/// slack for zero-padded decimals; anything longer is treated as literal text.
const MAX_ENTITY_BODY_LEN: usize = 10;

/// Decode a single entity body (the text between `&` and `;`) to its character,
/// or `None` if it is not a recognized named/numeric reference.
fn decode_entity_body(body: &str) -> Option<char> {
    match body {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let num = body.strip_prefix('#')?;
            let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                num.parse::<u32>().ok()?
            };
            char::from_u32(code)
        }
    }
}

/// Char-bounded truncation appending `...` (ASCII) on overflow. Shared with the
/// SSH-direct / continuation formatters in `tui_prompt_relay` (#3075 consolidated
/// the duplicate helper here).
pub(super) fn truncate_chars_ascii(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

/// Strip ANSI/terminal control sequences a TUI injector may prepend before the
/// `<task-notification>` tag, preserving newlines/tabs. Shared sanitizer so the
/// relay's classifier, the SSH-direct/continuation formatters, and this card
/// parser all see the same clean text (#3075 consolidated the duplicate).
pub(super) fn strip_terminal_controls(value: &str) -> String {
    crate::services::tui_prompt_control::strip_terminal_controls(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(fields: &[(&str, &str)]) -> String {
        let mut body = String::from("<task-notification>");
        for (tag, value) in fields {
            body.push_str(&format!("<{tag}>{value}</{tag}>"));
        }
        body.push_str("</task-notification>");
        body
    }

    #[test]
    fn parses_core_fields_with_both_id_spellings() {
        let hyphen = payload(&[
            ("task-id", "aa37b21a7adafc7c0"),
            ("status", "completed"),
            ("summary", "Implement #3034 part 2"),
            ("result", "Done. See PR."),
        ]);
        let parsed = parse_task_notification(&hyphen);
        assert_eq!(parsed.task_id.as_deref(), Some("aa37b21a7adafc7c0"));
        assert_eq!(parsed.status.as_deref(), Some("completed"));
        assert_eq!(parsed.summary.as_deref(), Some("Implement #3034 part 2"));
        assert_eq!(parsed.result.as_deref(), Some("Done. See PR."));

        let underscore = payload(&[("task_id", "codex-bg-1"), ("status", "completed")]);
        assert_eq!(
            parse_task_notification(&underscore).task_id.as_deref(),
            Some("codex-bg-1")
        );
    }

    #[test]
    fn parses_tool_use_id_and_derives_kind_from_summary_prefix() {
        // #3393: real-shape background Bash notification — hyphenated tool-use-id
        // child tag, `Background command "…"` summary → `background` kind.
        let bg = "<task-notification><task-id>b5gr0v9xj</task-id>\
            <tool-use-id>toolu_01Ls2svfdnzcn9uGwA7aHjHW</tool-use-id>\
            <status>completed</status>\
            <summary>Background command \"Wait until PR 3392 CI settles\" completed (exit code 0)</summary>\
            </task-notification>";
        let parsed = parse_task_notification(bg);
        assert_eq!(
            parsed.tool_use_id.as_deref(),
            Some("toolu_01Ls2svfdnzcn9uGwA7aHjHW")
        );
        assert_eq!(parsed.kind(), "background");

        // Subagent (`Agent "…"`) and workflow (`Dynamic workflow "…"`) prefixes.
        let subagent = payload(&[
            ("tool-use-id", "toolu_018F3HtbweDDNEbi44HKAhhi"),
            ("status", "completed"),
            ("summary", "Agent \"Scout issues\" completed"),
        ]);
        let parsed = parse_task_notification(&subagent);
        assert_eq!(
            parsed.tool_use_id.as_deref(),
            Some("toolu_018F3HtbweDDNEbi44HKAhhi")
        );
        assert_eq!(parsed.kind(), "subagent");

        let workflow = payload(&[
            ("status", "completed"),
            ("summary", "Dynamic workflow \"#3277 fix\" completed"),
        ]);
        assert_eq!(parse_task_notification(&workflow).kind(), "workflow");

        // A lost-process `Background agent "…"` notification omits tool-use-id;
        // it still classifies as `subagent` (the safe default) with `None` id.
        let bg_agent = payload(&[
            ("status", "failed"),
            (
                "summary",
                "Background agent \"Rebase\" was running … did not complete.",
            ),
        ]);
        let parsed = parse_task_notification(&bg_agent);
        assert_eq!(parsed.tool_use_id, None);
        assert_eq!(parsed.kind(), "subagent");
    }

    #[test]
    fn parser_tolerates_attributes_and_terminal_controls() {
        let raw = "\u{1b}[0m<task-notification kind=\"subagent\">\
            <status>completed</status><summary>hi</summary></task-notification>";
        let parsed = parse_task_notification(raw);
        assert_eq!(parsed.status.as_deref(), Some("completed"));
        assert_eq!(parsed.summary.as_deref(), Some("hi"));
    }

    // #4338: unit-level guarantees for the entity decoder — single layer,
    // order-independent, numeric refs, and bare/unknown `&` passthrough.
    #[test]
    fn decode_entities_once_is_single_layer_and_order_independent() {
        assert_eq!(decode_entities_once("&amp;provider"), "&provider");
        assert_eq!(decode_entities_once("&lt;expr&gt;"), "<expr>");
        assert_eq!(
            decode_entities_once("&quot;q&quot; &apos;a&apos;"),
            "\"q\" 'a'"
        );
        // `&amp;lt;` is an escaped `&lt;`; one decode yields `&lt;`, NOT `<`.
        assert_eq!(decode_entities_once("&amp;lt;"), "&lt;");
        assert_eq!(decode_entities_once("&amp;amp;x"), "&amp;x");
        // Numeric (decimal + hex) references.
        assert_eq!(decode_entities_once("&#38;&#60;&#62;"), "&<>");
        assert_eq!(decode_entities_once("&#x26;&#x3c;&#X3E;"), "&<>");
        // Bare `&`, unknown entities, and entity-free text pass through untouched.
        assert_eq!(decode_entities_once("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities_once("a &nope; b"), "a &nope; b");
        assert_eq!(decode_entities_once("100% & <ok>"), "100% & <ok>");
        assert_eq!(decode_entities_once("no entities here"), "no entities here");
        // A `&` whose nearest `;` is far away with an invalid body stays literal,
        // and a real entity later in the string still decodes.
        assert_eq!(decode_entities_once("A & B; C &amp; D"), "A & B; C & D");
    }

    // #4338 rework (codex r1): the `;` search is bounded to the entity-body
    // window, so `&`-dense long prose scans linearly. This guards the bounded
    // window's CORRECTNESS (no timing assert): bare-`&` runs pass through, an
    // entity after the dense run still decodes, window-edge bodies behave, and
    // a multi-byte body slices UTF-8-safely.
    #[test]
    fn decode_entities_once_stays_correct_on_ampersand_dense_long_text() {
        let dense = "a && b &&& c & d ".repeat(20_000);
        assert_eq!(decode_entities_once(&dense), dense);
        let tail_entity = format!("{dense}&amp;end");
        assert_eq!(decode_entities_once(&tail_entity), format!("{dense}&end"));
        // A valid body at the window edge (9 chars ≤ MAX_ENTITY_BODY_LEN) decodes…
        assert_eq!(decode_entities_once("&#x0000026;"), "&");
        // …an 11-char candidate body lies beyond the window and stays literal…
        assert_eq!(decode_entities_once("&abcdefghijk;"), "&abcdefghijk;");
        // …and a multi-byte candidate body inside the window is sliced safely.
        assert_eq!(decode_entities_once("&한글;"), "&한글;");
    }

    // #4338: the completion card must render a subagent report's literal
    // `&`/`<`/`>` — XML-escaped by the harness into the `<task-notification>`
    // envelope — as the ORIGINAL characters, not as leaked entities.
    #[test]
    fn card_decodes_harness_xml_escaped_prose_once() {
        let note = parse_task_notification(&payload(&[
            ("task-id", "aa37b21a7adafc7c0"),
            ("status", "completed"),
            ("summary", "Agent \"use &lt;expr&gt;\" completed"),
            (
                "result",
                "Uses `&amp;provider` and a generic `&lt;T&gt;` bound.",
            ),
        ]));
        let card = format_task_notification_card(&note, 1);
        // Decoded original characters are present …
        assert!(
            card.contains("use <expr>"),
            "summary must decode <>: {card}"
        );
        assert!(card.contains("&provider"), "result must decode &: {card}");
        assert!(
            card.contains("<T>"),
            "result must decode generic bound: {card}"
        );
        // … and no escaped literal leaks (the visible #4338 bug).
        assert!(!card.contains("&amp;"), "must not leak &amp;: {card}");
        assert!(!card.contains("&lt;"), "must not leak &lt;: {card}");
        assert!(!card.contains("&gt;"), "must not leak &gt;: {card}");
    }

    // #4338: removing EXACTLY one layer. A source escaped twice (an agent quoting
    // an already-broken card, re-escaped by the harness) keeps its own literal
    // `&amp;` — we invert only the harness's single pass, never over-decode text
    // an agent genuinely wrote about entities.
    #[test]
    fn card_decode_removes_exactly_one_escape_layer() {
        let note = parse_task_notification(&payload(&[
            ("status", "completed"),
            (
                "result",
                "quoted `&amp;amp;provider` and `&amp;lt;expr&amp;gt;`",
            ),
        ]));
        let card = format_task_notification_card(&note, 1);
        assert!(card.contains("&amp;provider"), "one layer off: {card}");
        assert!(card.contains("&lt;expr&gt;"), "one layer off: {card}");
        assert!(
            !card.contains("&amp;amp;"),
            "must not remove two layers: {card}"
        );
    }

    // #4338: the edit path re-parses the SAME payload and re-renders; decoding one
    // layer per render keeps the card byte-identical no matter how many times a
    // streaming update re-fires — no per-edit escape accumulation.
    #[test]
    fn card_render_is_invariant_across_streaming_edits() {
        let raw = payload(&[
            ("task-id", "aa37b21a7adafc7c0"),
            ("status", "completed"),
            ("summary", "Agent \"fix &lt;T&gt;\" completed"),
            ("result", "Touches `&amp;provider`. See PR."),
        ]);
        let first = format_task_notification_card(&parse_task_notification(&raw), 1);
        let second = format_task_notification_card(&parse_task_notification(&raw), 1);
        let third = format_task_notification_card(&parse_task_notification(&raw), 1);
        assert_eq!(first, second, "post vs first edit must be identical");
        assert_eq!(second, third, "successive edits must be identical");
        assert!(
            !first.contains("&amp;provider"),
            "no leaked entity: {first}"
        );
        assert!(first.contains("&provider"), "decoded once: {first}");
    }

    // #3075 class 1: single subagent completion → card with summary title +
    // PREVIEWED result, NOT the full dump, and PR URL surfaced.
    #[test]
    fn single_subagent_completion_card_previews_not_dumps() {
        let long_result = format!(
            "First useful line about the change.\nSecond line of detail.\n{}\nopened https://github.com/itismyfield/AgentDesk/pull/3034",
            "x".repeat(5000)
        );
        let note = parse_task_notification(&payload(&[
            ("task-id", "aa37b21a7adafc7c0"),
            ("status", "completed"),
            ("summary", "Implement #3034 part 2"),
            ("result", &long_result),
            ("usage", "194k tok"),
            ("duration", "4.1s"),
        ]));
        let card = format_task_notification_card(&note, 1);
        assert!(card.contains("✅"));
        assert!(card.contains("Implement #3034 part 2"));
        assert!(card.contains("First useful line about the change."));
        // The 5000-char filler line must NOT be dumped wholesale.
        assert!(!card.contains(&"x".repeat(5000)));
        assert!(
            card.chars().count() <= DISCORD_MESSAGE_LIMIT_CHARS,
            "card should stay within Discord's message limit, got {} chars",
            card.chars().count()
        );
        assert!(
            card.lines().any(|line| line.starts_with("> ")),
            "markdown preview should be blockquoted apart from card chrome: {card}"
        );
        assert!(
            card.contains(RESULT_PREVIEW_TRUNCATED_MARKER),
            "oversized preview should use an explicit truncation marker: {card}"
        );
        // PR URL surfaced + compact footer without token usage.
        assert!(card.contains("https://github.com/itismyfield/AgentDesk/pull/3034"));
        assert!(!card.contains("194k tok"));
        assert!(card.contains("⏱ 4.1s"));
        assert!(card.contains("task aa37b21a"));
    }

    #[test]
    fn markdown_preview_normalizes_block_markers_and_fills_past_headings() {
        let result = "\
# Findings
---
## Context
> quoted setup
- first concrete fix
* second concrete fix
1. third concrete fix
| Area | Result |
| --- | --- |
| Relay | clean |
Conclusion reached after the table.";

        let preview = markdown_preview(result);
        assert!(preview.contains("Findings"));
        assert!(preview.contains("Context"));
        assert!(preview.contains("quoted setup"));
        assert!(preview.contains("first concrete fix"));
        assert!(preview.contains("second concrete fix"));
        assert!(preview.contains("third concrete fix"));
        assert!(preview.contains("Area Result"));
        assert!(preview.contains("Relay clean"));
        assert!(preview.contains("Conclusion reached after the table."));
        assert!(
            !preview.lines().any(|line| {
                line.starts_with('#')
                    || line.starts_with('>')
                    || line.starts_with("- ")
                    || line.starts_with("* ")
                    || line.starts_with("1. ")
                    || line.starts_with('|')
                    || line.ends_with('|')
            }),
            "preview should not preserve block Markdown syntax: {preview}"
        );
    }

    #[test]
    fn markdown_preview_does_not_count_heading_lines_against_content_budget() {
        let mut lines = (1..=8)
            .map(|idx| format!("# Heading {idx}"))
            .collect::<Vec<_>>();
        lines.extend((1..=11).map(|idx| format!("detail {idx}.")));
        let preview = markdown_preview(&lines.join("\n"));

        assert!(preview.contains("Heading 8"), "{preview}");
        assert!(
            preview.contains("detail 10."),
            "heading lines should not consume the 10 content-line budget: {preview}"
        );
        assert!(
            !preview.contains("detail 11."),
            "the 11th content line should still be excluded: {preview}"
        );
    }

    #[test]
    fn markdown_preview_uses_expanded_budget_and_boundary_truncation_marker() {
        let mut lines = (1..=9)
            .map(|idx| format!("line {idx}: short detail."))
            .collect::<Vec<_>>();
        lines.push(format!("line 10: {}", "detail ".repeat(250)));
        lines.push("line 11: should not be collected.".to_string());
        let result = lines.join("\n");

        let preview = markdown_preview(&result);
        assert!(
            preview.contains("line 10:"),
            "preview should include the tenth content line: {preview}"
        );
        assert!(
            !preview.contains("line 11:"),
            "preview should stop at the configured line budget: {preview}"
        );
        assert!(
            preview.chars().count() <= RESULT_PREVIEW_CHARS,
            "preview must respect char budget"
        );
        assert!(
            preview.ends_with(RESULT_PREVIEW_TRUNCATED_MARKER),
            "long preview should end with explicit marker: {preview}"
        );
    }

    #[test]
    fn task_card_clamps_many_short_headings_under_discord_limit() {
        let result = (1..=900)
            .map(|idx| format!("# H{idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let note = parse_task_notification(&payload(&[
            ("task-id", "heading-overflow"),
            ("status", "completed"),
            ("summary", "many headings"),
            ("result", &result),
        ]));
        let card = format_task_notification_card(&note, 1);

        assert!(
            card.chars().count() <= DISCORD_MESSAGE_LIMIT_CHARS,
            "card must be clamped to Discord's limit, got {} chars",
            card.chars().count()
        );
        assert!(
            card.contains(RESULT_PREVIEW_TRUNCATED_MARKER),
            "overflowing heading preview should advertise truncation: {card}"
        );
    }

    #[test]
    fn markdown_preview_preserves_fenced_block_content() {
        let result = "\
# Outside heading
```sh
# comment
- rm -rf target
| table | row |
> quoted code
```
after fence";

        let preview = markdown_preview(result);

        assert!(preview.contains("# comment"), "{preview}");
        assert!(preview.contains("- rm -rf target"), "{preview}");
        assert!(preview.contains("| table | row |"), "{preview}");
        assert!(preview.contains("> quoted code"), "{preview}");
        assert!(
            preview.lines().count() <= RESULT_PREVIEW_LINES + 1,
            "fenced block lines should still consume the content-line budget: {preview}"
        );
    }

    #[test]
    fn markdown_preview_matches_fences_by_opener_length() {
        let result = "\
````rust
fn main() {}
```rust
# comment
- item
````
# after fence";

        let preview = markdown_preview(result);

        assert!(preview.contains("# comment"), "{preview}");
        assert!(preview.contains("- item"), "{preview}");
        assert!(preview.contains("after fence"), "{preview}");
        assert!(
            !preview.contains("# after fence"),
            "content after the real 4-backtick closer should leave fence mode: {preview}"
        );
    }

    #[test]
    fn markdown_preview_matches_fences_by_opener_character() {
        let tilde_result = "\
~~~text
```rust
# tilde comment
- tilde item
~~~
- after tilde";
        let tilde_preview = markdown_preview(tilde_result);

        assert!(tilde_preview.contains("# tilde comment"), "{tilde_preview}");
        assert!(tilde_preview.contains("- tilde item"), "{tilde_preview}");
        assert!(tilde_preview.contains("after tilde"), "{tilde_preview}");
        assert!(
            !tilde_preview.contains("- after tilde"),
            "a backtick fence must not close a tilde fence: {tilde_preview}"
        );

        let backtick_result = "\
```text
~~~
# backtick comment
- backtick item
```
- after backtick";
        let backtick_preview = markdown_preview(backtick_result);

        assert!(
            backtick_preview.contains("# backtick comment"),
            "{backtick_preview}"
        );
        assert!(
            backtick_preview.contains("- backtick item"),
            "{backtick_preview}"
        );
        assert!(
            backtick_preview.contains("after backtick"),
            "{backtick_preview}"
        );
        assert!(
            !backtick_preview.contains("- after backtick"),
            "a tilde fence must not close a backtick fence: {backtick_preview}"
        );
    }

    #[test]
    fn markdown_preview_preserves_plain_three_backtick_fence_behavior() {
        let result = "\
```sh
# comment
```
# after fence";

        let preview = markdown_preview(result);

        assert!(preview.contains("# comment"), "{preview}");
        assert!(preview.contains("after fence"), "{preview}");
        assert!(
            !preview.contains("# after fence"),
            "plain 3-backtick close should still leave fence mode: {preview}"
        );
    }

    #[test]
    fn markdown_preview_preserves_issue_reference_at_line_start() {
        let preview = markdown_preview("#3034 stays linked\n# Heading");

        assert!(preview.contains("#3034 stays linked"), "{preview}");
        assert!(preview.contains("Heading"), "{preview}");
    }

    #[test]
    fn markdown_preview_bounds_large_heading_runs() {
        let result = (1..=50_000)
            .map(|idx| format!("# Heading {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = markdown_preview(&result);

        assert!(
            preview.chars().count() <= RESULT_PREVIEW_CHARS,
            "large heading-only preview must stay char-bounded"
        );
        assert!(
            preview.lines().count() < 500,
            "heading-only previews must not collect unbounded lines"
        );
    }

    // #3075 class 2: JSON aggregate result → summarized counts, NO raw JSON.
    #[test]
    fn json_aggregate_result_is_summarized_not_dumped() {
        let json = r#"{"results":[{"issue":1},{"issue":2},{"issue":3}],"reviews":[{"pr":9}]}"#;
        let note = parse_task_notification(&payload(&[
            ("task-id", "wxvufewff0000000"),
            ("status", "completed"),
            ("summary", "triage"),
            ("result", json),
        ]));
        let card = format_task_notification_card(&note, 1);
        assert!(card.contains("3 results"));
        assert!(card.contains("1 reviews"));
        // No raw JSON braces dumped.
        assert!(!card.contains("\"issue\""));
        assert!(!card.contains("[{"));
    }

    #[test]
    fn format_duration_ms_is_human_readable() {
        assert_eq!(format_duration_ms("900").as_deref(), Some("900ms"));
        assert_eq!(format_duration_ms("45000").as_deref(), Some("45s"));
        assert_eq!(format_duration_ms("504983").as_deref(), Some("8m 24s"));
        assert_eq!(format_duration_ms("120000").as_deref(), Some("2m"));
        assert_eq!(format_duration_ms("bad"), None);
    }

    #[test]
    fn task_card_footer_snapshot_uses_elapsed_icon_and_omits_tokens_4822() {
        let raw = "<task-notification><task-id>a24eb9898b9662840</task-id>\
            <status>completed</status><summary>done</summary>\
            <usage><subagent_tokens>66013</subagent_tokens><tool_uses>37</tool_uses>\
            <duration_ms>504983</duration_ms></usage></task-notification>";
        let card = format_task_notification_card(&parse_task_notification(raw), 1);

        assert_eq!(
            card,
            "✅ Task completed\n**done**\n\n-# task a24eb989 · 37 tools · ⏱ 8m 24s"
        );
    }

    #[test]
    fn footer_renders_nested_usage_readably_not_raw_xml() {
        // #3169: a real agent payload nests `<subagent_tokens>`/`<tool_uses>`/
        // `<duration_ms>` inside `<usage>`. The footer keeps the tool count and
        // human duration without leaking the raw nested XML or token usage.
        let raw = "<task-notification><task-id>a24eb9898b9662840</task-id>\
            <status>completed</status><summary>done</summary>\
            <usage><agent_count>1</agent_count><subagent_tokens>66013</subagent_tokens>\
            <tool_uses>37</tool_uses><duration_ms>504983</duration_ms></usage>\
            </task-notification>";
        let note = parse_task_notification(raw);
        let card = format_task_notification_card(&note, 0);
        assert!(
            !card.contains("<subagent_tokens>") && !card.contains("<duration_ms>"),
            "raw nested usage XML must not leak into the card: {card}"
        );
        assert!(
            !card.contains(" tok"),
            "token usage must be omitted: {card}"
        );
        assert!(
            card.contains("37 tools"),
            "tool count should render once: {card}"
        );
        assert!(
            card.contains("⏱ 8m 24s"),
            "duration should be human-readable: {card}"
        );
        // multi-agent count only shown when > 1
        assert!(
            !card.contains("1 agents"),
            "single-agent count is suppressed: {card}"
        );
    }

    #[test]
    fn footer_ignores_prose_wrapped_usage_block() {
        // #3169 (codex R3): a literal `<usage>...</usage>` written inside
        // <summary>/<result> prose must NOT be mistaken for the real top-level
        // usage block. The real block follows the prose fields.
        let raw = "<task-notification><task-id>tp</task-id><status>completed</status>\
            <summary>example: <usage><tool_uses>999</tool_uses>\
            <subagent_tokens>888888</subagent_tokens></usage></summary>\
            <result>done</result>\
            <usage><tool_uses>5</tool_uses><subagent_tokens>66013</subagent_tokens>\
            <duration_ms>504983</duration_ms></usage></task-notification>";
        let note = parse_task_notification(raw);
        assert_eq!(
            note.tool_uses.as_deref(),
            Some("5"),
            "real usage, not prose 999"
        );
        assert_eq!(note.subagent_tokens.as_deref(), Some("66013"));
        let card = format_task_notification_card(&note, 0);
        assert!(card.contains("5 tools"), "{card}");
        assert!(!card.contains(" tok"), "{card}");
        assert!(
            !card.contains("999 tools") && !card.contains("889K"),
            "footer not poisoned: {card}"
        );
    }

    #[test]
    fn footer_ignores_usage_subfield_tags_in_prose() {
        // #3169 (codex): a literal <subagent_tokens> inside <summary>/<result>
        // prose must NOT be picked up as footer metadata — only the real value
        // inside the <usage> block counts.
        let raw = "<task-notification><task-id>t9</task-id><status>completed</status>\
            <summary>note about <subagent_tokens>999999</subagent_tokens> in text</summary>\
            <result>also <duration_ms>1</duration_ms> mentioned</result>\
            <usage><subagent_tokens>66013</subagent_tokens><tool_uses>5</tool_uses>\
            <duration_ms>504983</duration_ms></usage></task-notification>";
        let note = parse_task_notification(raw);
        // The scoped parse takes the <usage> value, not the prose-literal one.
        assert_eq!(note.subagent_tokens.as_deref(), Some("66013"));
        assert_eq!(note.duration_ms.as_deref(), Some("504983"));
        let card = format_task_notification_card(&note, 0);
        assert!(
            !card.contains(" tok"),
            "token usage must be omitted: {card}"
        );
        assert!(card.contains("5 tools"), "{card}");
        assert!(card.contains("⏱ 8m 24s"), "{card}");
    }

    #[test]
    fn footer_shows_agent_count_when_multiple() {
        let raw = "<task-notification><task-id>wf123</task-id><status>completed</status>\
            <usage><agent_count>6</agent_count><subagent_tokens>1341096</subagent_tokens>\
            <tool_uses>178</tool_uses><duration_ms>1341096</duration_ms></usage>\
            </task-notification>";
        let note = parse_task_notification(raw);
        let card = format_task_notification_card(&note, 0);
        assert!(
            card.contains("6 agents"),
            "multi-agent count should show: {card}"
        );
        assert!(
            !card.contains(" tok"),
            "token usage must be omitted: {card}"
        );
        assert!(
            card.contains("178 tools"),
            "tool count should remain: {card}"
        );
        assert!(
            card.contains("⏱ 22m 21s"),
            "elapsed icon should remain: {card}"
        );
    }
}
