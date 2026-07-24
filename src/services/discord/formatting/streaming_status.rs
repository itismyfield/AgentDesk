use super::*;

pub(in crate::services::discord) fn floor_char_boundary(s: &str, index: usize) -> usize {
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

pub(super) fn char_count(s: &str) -> usize {
    s.chars().count()
}

pub(super) fn byte_index_at_char_limit(s: &str, max_chars: usize) -> usize {
    if max_chars == 0 {
        0
    } else {
        s.char_indices()
            .nth(max_chars)
            .map(|(idx, _)| idx)
            .unwrap_or(s.len())
    }
}

pub(in crate::services::discord) fn streaming_split_boundary(
    text: &str,
    max_len: usize,
) -> Option<usize> {
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
        .or_else(|| super::super::semantic_boundaries::semantic_sentence_split_boundary(window))
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
pub(in crate::services::discord) struct StreamingRolloverPlan {
    pub(in crate::services::discord) display_snapshot: String,
    pub(in crate::services::discord) frozen_chunk: String,
    pub(in crate::services::discord) split_at: usize,
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

pub(in crate::services::discord) fn plan_streaming_rollover(
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

pub(in crate::services::discord) fn build_streaming_placeholder_text(
    current_portion: &str,
    status_block: &str,
) -> String {
    if current_portion.is_empty() {
        clamp_placeholder_status_block(status_block)
    } else {
        build_streaming_placeholder_snapshot(current_portion, status_block)
    }
}

/// Remove ephemeral placeholder lines (e.g. "⏳ 대기 중...") from the final
/// delivered response.  These lines are useful during streaming but should not
/// persist in the channel.
pub(super) fn strip_placeholder_lines(s: &str) -> String {
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
pub(in crate::services::discord) fn text_ends_with_streaming_footer(text: &str) -> bool {
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
pub(in crate::services::discord) fn finalize_stale_streaming_footer(
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

pub(in crate::services::discord) fn is_streaming_placeholder_status_line(line: &str) -> bool {
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
pub(in crate::services::discord) fn format_for_discord(s: &str) -> String {
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

/// Determine the raw tool status string for Discord status display.
/// Shared by turn_bridge and tmux watcher to avoid duplicating fallback logic.
pub(in crate::services::discord) fn resolve_raw_tool_status<'a>(
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
pub(in crate::services::discord) fn finalize_in_progress_tool_status(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("⚙ ") {
        format!("⚠ {rest}")
    } else if let Some(rest) = line.strip_prefix("⚙") {
        format!("⚠{rest}")
    } else {
        line.to_string()
    }
}

pub(in crate::services::discord) fn preserve_previous_tool_status(
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
pub(in crate::services::discord) fn humanize_tool_status(tool_line: &str) -> String {
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
pub(in crate::services::discord) enum MonitorHandoffReason {
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
pub(in crate::services::discord) enum MonitorHandoffStatus<'a> {
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
pub(in crate::services::discord) fn build_monitor_handoff_placeholder(
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
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn build_monitor_handoff_placeholder_with_context(
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
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn build_monitor_handoff_placeholder_with_live_events(
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
pub(in crate::services::discord) enum LongRunningCloseTrigger {
    MonitorLike,
    BackgroundDispatch,
}

pub(in crate::services::discord) fn classify_long_running_tool(
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
pub(in crate::services::discord) fn build_placeholder_status_block(
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

pub(in crate::services::discord) fn build_processing_status_block(indicator: &str) -> String {
    format!("{indicator} 계속 처리 중")
}

pub(in crate::services::discord) fn build_status_panel_streaming_edit_text(
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
