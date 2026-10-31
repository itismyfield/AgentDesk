use std::sync::OnceLock;

/// Byte budget for the COMPLETION footer task section (#3089 S3) — the final
/// turn message keeps a compact task summary. The LIVE streaming panel uses the
/// larger `SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES` below.
pub(in crate::services::discord) const SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES: usize = 600;
/// #3497: byte budget for the LIVE single-message footer panel BODY (Recent +
/// Tasks/Subagents/context usage; excludes the spinner/status header). The 1400-byte
/// cap includes Discord's per-line subtext prefixes. Normal Recent output is compact
/// after #3806, but the footer clamp still handles legacy or
/// debug-shaped fenced Recent sections as whole blocks. The relay body auto-uses
/// the remaining message space (`DISCORD_MSG_LIMIT − footer_len − margin`) and
/// rolls over when longer, so commentary is preserved across messages rather
/// than starving the terminal panel.
pub(in crate::services::discord) const SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES: usize = 1400;
pub(in crate::services::discord) const SINGLE_MESSAGE_PANEL_SPINNER_FRAMES: &[&str] =
    &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
// Residual background agents can legitimately run for about an hour, but a
// crashed agent must not keep editing the terminal message forever.
pub(in crate::services::discord) const COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS: i64 = 3600;
pub(in crate::services::discord) const COMPLETION_FOOTER_MAX_CONSECUTIVE_EDIT_FAILURES: u8 = 3;
pub(in crate::services::discord) const COMPLETION_FOOTER_IDLE_EXPIRED_INDICATOR: &str = "…";

pub(in crate::services::discord) fn single_message_panel_spinner_frame(
    index: usize,
) -> &'static str {
    SINGLE_MESSAGE_PANEL_SPINNER_FRAMES[index % SINGLE_MESSAGE_PANEL_SPINNER_FRAMES.len()]
}

pub(in crate::services::discord) fn enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let enabled = configured_enabled();
        let state = if enabled { "enabled" } else { "disabled" };
        tracing::info!("  ✓ single_message_panel: {state}");
        enabled
    })
}

/// Default ON; standard false tokens disable the live panel.
fn configured_enabled() -> bool {
    crate::utils::env_flag("AGENTDESK_SINGLE_MESSAGE_PANEL", true)
}

// Footer writes are retired; callers keep the flag arguments until the legacy panel path goes.
pub(in crate::services::discord) fn footer_mode_enabled(
    _single_message_panel_enabled: bool,
    _status_panel_v2_enabled: bool,
) -> bool {
    false
}

// Separate-panel writes are retired as well; the same legacy removal drops this gate.
pub(in crate::services::discord) fn separate_status_panel_enabled_for_flags(
    _single_message_panel_enabled: bool,
    _status_panel_v2_enabled: bool,
) -> bool {
    false
}

pub(in crate::services::discord) fn live_events_dirty_should_force_status_update(
    live_events_dirty: bool,
    single_message_panel_footer_mode: bool,
) -> bool {
    live_events_dirty && !single_message_panel_footer_mode
}

pub(in crate::services::discord) fn compose_footer_status_block(
    indicator: &str,
    panel_text: &str,
) -> String {
    let spinner = super::formatting::build_processing_status_block(indicator);
    let panel_text = panel_text.trim();
    let status_block = if panel_text.is_empty() {
        spinner
    } else if let Some(status_block) = compose_merged_footer_status_block(indicator, panel_text) {
        status_block
    } else {
        spinner
    };
    clamp_footer_status_block(status_block)
}

/// Render terminal relay context as Discord subtext. Every non-empty line is
/// explicitly prefixed because Discord does not carry a multiline subtext span.
pub(in crate::services::discord) fn completion_footer_subtext(block: &str) -> String {
    crate::services::terminal_status_formatting::format_subtext_block(block)
}

pub(in crate::services::discord) fn compose_completion_footer_text(
    body: &str,
    completion_block: Option<&str>,
) -> String {
    compose_completion_footer_text_tracked(body, completion_block, &mut 0)
}

pub(in crate::services::discord) fn compose_completion_footer_text_tracked(
    body: &str,
    completion_block: Option<&str>,
    surviving_prefix: &mut usize,
) -> String {
    *surviving_prefix = 0;
    if let Some(completion_block) = completion_block
        && let Some((completion_without_warning, warning)) =
            super::turn_end_wip_warning::split_merged_turn_end_wip_warning(completion_block)
    {
        let completion_without_warning =
            (!completion_without_warning.trim().is_empty()).then_some(completion_without_warning);
        let warning = completion_footer_subtext(&warning);
        let separator_units = super::formatting::discord_message_units("\n\n");
        // Subtracting a byte budget from a UTF-16-unit limit is conservative because UTF-8 bytes
        // are never fewer than UTF-16 units. It reserves that many rendered non-warning units but
        // not a whole source section: `completion_footer_subtext` adds three units per non-empty line.
        let warning_budget = super::DISCORD_MSG_LIMIT.saturating_sub(
            SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES
                .saturating_add(6)
                .saturating_add(separator_units),
        );
        let warning =
            super::turn_end_wip_warning::bounded_turn_end_wip_warning(&warning, warning_budget);
        let content_limit = super::DISCORD_MSG_LIMIT.saturating_sub(
            super::formatting::discord_message_units(&warning).saturating_add(separator_units),
        );
        let composed = compose_completion_footer_text_without_warning_with_limit(
            body,
            completion_without_warning.as_deref(),
            content_limit,
            true,
            surviving_prefix,
        );
        return if composed.trim().is_empty() {
            warning
        } else {
            format!("{composed}\n\n{warning}")
        };
    }
    compose_completion_footer_text_without_warning_with_limit(
        body,
        completion_block,
        super::DISCORD_MSG_LIMIT,
        false,
        surviving_prefix,
    )
}

fn compose_completion_footer_text_without_warning_with_limit(
    body: &str,
    completion_block: Option<&str>,
    message_limit: usize,
    clamp_unadorned_body: bool,
    surviving_prefix: &mut usize,
) -> String {
    *surviving_prefix = 0;
    let body = body.trim_end();
    let Some(block) = completion_block
        .map(str::trim)
        .filter(|block| !block.is_empty())
    else {
        return if clamp_unadorned_body {
            let safe_end =
                super::formatting::byte_index_at_discord_message_units(body, message_limit);
            repair_fence_parity(&body[..safe_end])
        } else {
            body.to_string()
        };
    };
    let block = completion_footer_subtext(block);

    *surviving_prefix = block.len();
    let max_block_units = message_limit.saturating_sub(6);
    let block = if max_block_units == 0 {
        *surviving_prefix = 0;
        String::new()
    } else if super::formatting::discord_message_units(&block) <= max_block_units {
        repair_fence_parity(&block)
    } else {
        let ellipsis = "…";
        let body_budget =
            max_block_units.saturating_sub(super::formatting::discord_message_units(ellipsis));
        let safe_end = super::formatting::byte_index_at_discord_message_units(&block, body_budget);
        *surviving_prefix = safe_end;
        repair_fence_parity(&format!("{}{}", &block[..safe_end], ellipsis))
    };
    *surviving_prefix = (*surviving_prefix).min(block.len());
    if block.is_empty() {
        let safe_end = super::formatting::byte_index_at_discord_message_units(body, message_limit);
        return repair_fence_parity(&body[..safe_end]);
    }
    if body.is_empty() {
        return block;
    }

    let suffix = format!("\n\n{block}");
    let max_units = message_limit.saturating_sub(super::formatting::discord_message_units(&suffix));
    let base = if super::formatting::discord_message_units(body) > max_units {
        let safe_end = super::formatting::byte_index_at_discord_message_units(body, max_units);
        &body[..safe_end]
    } else {
        body
    }
    .trim_end();
    // #3394 round 2 (boundary-scoped repair): repair the BODY alone, THEN append
    // the footer block — never run `repair_fence_parity` over the combined string.
    // `repair_fence_parity` drops the last dangling opener THROUGH end-of-string,
    // so a body whose fence was chopped by the Discord-limit trim above would, on
    // the combined `{base}{suffix}`, take the appended footer down with it.
    //
    // #3391: body-only repair prevents `repair_fence_parity` from consuming the rendered footer.
    // Its ids were computed earlier, so the inline block clamp can still cut a reported mark (#5348).
    //
    // Append the rendered footer to the separately repaired body. The assertion
    // below checks the renderer's current even fence parity; the runtime branch
    // repairs the suffix separately if that assumption stops holding, without
    // reaching back across the body/footer boundary.
    let base = repair_fence_parity(base);
    let combined = format!("{base}{suffix}");
    debug_assert_eq!(
        combined.matches("```").count() % 2,
        0,
        "boundary-scoped repair left odd combined fence parity: {combined:?}"
    );
    // Runtime backstop: if the footer block ever did carry an odd fence (a future
    // change to the completion-footer composition), repair the footer SEPARATELY so
    // each part is balanced — never run a combined repair that could reach back
    // across the boundary and delete the body's tail or the footer's marks. The
    // already-repaired `base` stays balanced; balanced base + balanced footer =
    // balanced combined. With today's fence-free footer this branch is unreachable,
    // which the debug_assert above enforces in test builds.
    if combined.matches("```").count() % 2 != 0 {
        let repaired_suffix = repair_fence_parity(&suffix);
        return format!("{base}{repaired_suffix}");
    }
    combined
}

pub(in crate::services::discord) fn finalize_streaming_footer_with_completion(
    last_edit_text: &str,
    provider: &super::ProviderKind,
    completion_block: Option<&str>,
) -> Option<String> {
    let cleaned = completion_footer_base_body(last_edit_text, provider);
    let finalized = compose_completion_footer_text(&cleaned, completion_block);
    if finalized.trim().is_empty() {
        None
    } else if finalized == last_edit_text {
        None
    } else {
        Some(finalized)
    }
}

pub(in crate::services::discord) fn completion_footer_base_body(
    text: &str,
    provider: &super::ProviderKind,
) -> String {
    strip_streaming_footer(text, provider).unwrap_or_else(|| text.trim_end().to_string())
}

fn compose_merged_footer_status_block(indicator: &str, panel_text: &str) -> Option<String> {
    let (header_line, panel_body) = panel_text.split_once('\n').unwrap_or((panel_text, ""));
    let header = merged_footer_header_line(indicator, header_line)?;
    let panel_body = completion_footer_subtext(panel_body);
    let panel_body = clamp_footer_panel_text(&panel_body);
    if panel_body.trim().is_empty() {
        Some(header)
    } else {
        Some(format!("{header}\n{panel_body}"))
    }
}

fn merged_footer_header_line(indicator: &str, header_line: &str) -> Option<String> {
    // A detached live panel is reclaimable; its header reused on an answer is not.
    let header_line = header_line.trim_end();
    let header_line = header_line
        .strip_suffix(super::formatting::PLACEHOLDER_PROBE_MARKER)
        .unwrap_or(header_line);
    let header = strip_panel_header_status_marker(header_line)?;
    if header.is_empty() {
        None
    } else {
        Some(format!("-# {indicator} {header}"))
    }
}

fn strip_panel_header_status_marker(header_line: &str) -> Option<&str> {
    let header_line = header_line
        .trim()
        .strip_prefix("-# ")
        .unwrap_or_else(|| header_line.trim());
    if header_line.is_empty() {
        return None;
    }

    let mut chars = header_line.char_indices();
    let (_, first) = chars.next()?;
    let rest_start = chars
        .next()
        .map(|(idx, _)| idx)
        .unwrap_or(header_line.len());
    if is_panel_header_status_marker(first) {
        Some(header_line[rest_start..].trim_start())
    } else {
        Some(header_line)
    }
}

fn is_panel_header_status_marker(marker: char) -> bool {
    // #3983: `🟡` is the new stale activity marker (`🟡 응답 지연 · 조사 권장`), swapped
    // for the spinner like every other leading status emoji.
    matches!(
        marker,
        '🟢' | '💤' | '⏰' | '✅' | '🔧' | '🧵' | '🧬' | '🟡'
    )
}

fn clamp_footer_panel_text(panel_text: &str) -> String {
    if panel_text.len() <= SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES {
        return panel_text.to_string();
    }

    const TRUNCATION_MARKER: &str = "-# …";
    if SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES <= TRUNCATION_MARKER.len() {
        let safe_end = super::formatting::floor_char_boundary(
            TRUNCATION_MARKER,
            SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES,
        );
        return TRUNCATION_MARKER[..safe_end].to_string();
    }

    let lines: Vec<&str> = panel_text.lines().collect();
    for keep_count in (1..=lines.len()).rev() {
        let prefix = lines[..keep_count].join("\n");
        let candidate = format!("{prefix}\n{TRUNCATION_MARKER}");
        if candidate.len() > SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES {
            continue;
        }
        // #3495: keep the fenced 🖥️ Recent block atomic. A line-wise cut can
        // sever it — keeping the header (and maybe a ```text opener) while
        // dropping the body + closing ```. `repair_fence_parity` later strips
        // the dangling opener but NOT the header above it, leaving a Recent
        // header with an empty terminal body. So drop the whole incomplete
        // fenced section here: if the first DROPPED line is a ``` opener, the
        // kept tail is a bare section header — drop it too; and never return a
        // prefix whose ``` parity is odd (an unclosed fence). Each rejection
        // falls back to a shorter prefix.
        let mut kept = keep_count;
        while kept > 0
            && lines
                .get(kept)
                .is_some_and(|line| strip_subtext_prefix(line.trim_start()).starts_with("```"))
        {
            kept -= 1;
        }
        let prefix = lines[..kept].join("\n");
        if prefix.matches("```").count() % 2 != 0 {
            continue;
        }
        if prefix.is_empty() {
            return TRUNCATION_MARKER.to_string();
        }
        return format!("{prefix}\n{TRUNCATION_MARKER}");
    }

    let first_line = lines.first().copied().unwrap_or_default();
    let first_line_budget = SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES
        .saturating_sub(TRUNCATION_MARKER.len())
        .saturating_sub(1);
    let safe_end = super::formatting::floor_char_boundary(first_line, first_line_budget);
    if safe_end == 0 {
        TRUNCATION_MARKER.to_string()
    } else {
        format!("{}\n{TRUNCATION_MARKER}", &first_line[..safe_end])
    }
}

fn clamp_footer_status_block(status_block: String) -> String {
    // #3394: this is the universal live-panel finalization sink (every
    // `compose_footer_status_block` call lands here, after the upstream 600-byte
    // `clamp_footer_panel_text` body trim and this Discord-unit trim — either
    // can chop the Recent block's closing ```). Re-balance fence parity on EVERY
    // return path so Discord never renders a dangling fence as literal text.
    let max_units = super::DISCORD_MSG_LIMIT.saturating_sub(6);
    let clamped = if super::formatting::discord_message_units(&status_block) <= max_units {
        status_block
    } else {
        let ellipsis = "…";
        let body_budget =
            max_units.saturating_sub(super::formatting::discord_message_units(ellipsis));
        if body_budget == 0 {
            ellipsis.to_string()
        } else {
            let safe_end =
                super::formatting::byte_index_at_discord_message_units(&status_block, body_budget);
            format!("{}{}", &status_block[..safe_end], ellipsis)
        }
    };
    repair_fence_parity(&clamped)
}

/// #3394: shared triple-backtick fence-parity backstop. Discord renders an
/// UNTERMINATED code fence as literal text (the reported bug: a blind char cut
/// chopped the closing ``` of the trailing fenced section). After ANY truncation
/// of panel text, re-balance by counting ``` runs: if the count is ODD, the last
/// opener is dangling, so REMOVE that opener and everything after it.
///
/// Removal is chosen over appending a closing fence because the truncated tail is
/// already incomplete content — re-closing it would resurrect a tiny orphan code
/// block of cut-off text; dropping it keeps the output clean. Under Discord
/// semantics there is no fence nesting (a ``` inside a fenced block CLOSES it), so
/// plain parity counting is correct. Lives here, the panel-text finalization
/// layer; `status_panel.rs` calls it via the full crate path after its own
/// section-wise truncation.
pub(in crate::services::discord) fn repair_fence_parity(text: &str) -> String {
    let mut count = 0usize;
    let mut last_open: Option<usize> = None;
    for (idx, _) in text.match_indices("```") {
        // Every other fence (1st, 3rd, ...) is an opener; track the latest one.
        if count % 2 == 0 {
            last_open = Some(idx);
        }
        count += 1;
    }
    if count % 2 == 0 {
        return text.to_string();
    }
    // Odd count: drop the dangling opener and its trailing content; trim the now
    // exposed tail so no blank line is left where the fence used to start.
    match last_open {
        Some(cut) => text[..cut].trim_end().to_string(),
        None => text.to_string(),
    }
}

pub(in crate::services::discord) fn finalize_streaming_footer(
    last_edit_text: &str,
    provider: &super::ProviderKind,
) -> Option<String> {
    let cleaned = strip_streaming_footer(last_edit_text, provider)?;
    if cleaned.trim().is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Stop marker appended to a panel whose live spinner footer is reclaimed by
/// the #3412 startup sweep. The original body is preserved verbatim; only the
/// animated footer/Tasks block is replaced by this single static line so the
/// user can tell the turn was cut short by a restart rather than left hanging
/// on a frozen spinner forever.
pub(in crate::services::discord) const STARTUP_RECLAIM_STOP_MARKER: &str = "⏹ (재시작으로 중단됨)";

/// #3412: true when `text` still carries a LIVE animated footer that a prior
/// generation left frozen — either a single-message streaming status line
/// (진행 중 / 도구 실행 중 / …) or a completion-footer Tasks/Subagents block
/// whose slot lines still end in a braille spinner glyph rather than a terminal
/// ✓/✗/… mark. A panel that already finalized (only terminal marks remain) is
/// NOT reclaimable and returns false, so the sweep never re-touches a completed
/// panel.
///
/// codex round-1 High-b: detection is anchored to the message TAIL. The footer
/// must be the message's final run of footer-shaped lines, and the spinner glyph
/// must sit on a line within that tail run. A footer/Tasks block quoted in the
/// middle of an ordinary bot message (this very channel posts such quotes) is
/// followed by non-footer prose, so the trailing run never reaches it and the
/// body is left intact. The general live-panel path keeps using the broad
/// `strip_streaming_footer` matcher — this narrowing is reclaim-only.
pub(in crate::services::discord) fn text_has_frozen_spinner_footer(text: &str) -> bool {
    tail_frozen_footer_split(text).is_some()
}

/// codex round-1 High-b core: split `text` into `(body, footer)` only when the
/// message ends with a frozen-spinner footer. The footer is the maximal trailing
/// run of footer-shaped lines (streaming status line, `Tasks`/`Subagents` header,
/// `└ ` slot line, or icon-led context summary), the run must contain a braille
/// spinner glyph, and its first line must open a footer (a status line or section
/// header) so an unrelated `└ ` quote alone cannot trigger it. `body` is every
/// line before the run, joined verbatim. Returns `None` when the tail is ordinary
/// prose (a mid-body footer quote ends up here), so the reclaim edit is skipped.
fn tail_frozen_footer_split(text: &str) -> Option<(String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    // Walk up from the last line, collecting the trailing run of footer-shaped
    // lines. Blank lines inside the run are tolerated (the `\n\n` before the
    // footer / between sections) but a non-blank, non-footer line ends the run.
    //
    // codex round-2 Medium: when `clamp_completion_task_section` (and the byte
    // clamp `clamp_footer_panel_text`) trims a frozen footer over the 600-byte
    // budget, it appends a bare truncation-marker line (`…`, U+2026 on its own
    // line) BELOW the kept footer prefix. That marker is NOT footer-shaped, so a
    // naive walk ended on the last line and the clamped frozen tail
    // (`Tasks\n└ Bash … ⠧\n…`) was never reclaimed.
    //
    // codex round-3 Medium: the clamp marker is ALWAYS at the very tail (below the
    // footer). Letting a marker pass through anywhere in the walk wrongly skipped
    // a real frozen footer when the BODY ended with a standalone `…` line just
    // above the footer. So the walk is two phases: (1) consume trailing clamp
    // markers / blanks at the very bottom, then (2) consume the contiguous footer
    // run (footer-shaped + inter-section blanks) — a marker reached in phase 2
    // sits above the footer, is body content, and closes the run.
    let mut start = lines.len();
    // Phase 1 — trailing clamp markers / blank lines below the footer.
    while start > 0 {
        let line = lines[start - 1];
        if line.trim().is_empty() || line_is_truncation_marker(line) {
            start -= 1;
        } else {
            break;
        }
    }
    // Phase 2 — the footer run itself.
    while start > 0 {
        let line = lines[start - 1];
        if line_is_footer_shaped(line) || line.trim().is_empty() {
            start -= 1;
        } else {
            break;
        }
    }
    if start >= lines.len() {
        return None;
    }
    // Trim leading blank lines of the run so the footer opens on real content.
    while start < lines.len() && lines[start].trim().is_empty() {
        start += 1;
    }
    if start >= lines.len() {
        return None;
    }
    let footer_lines = &lines[start..];
    // First footer line must open a footer (status line or section header); a
    // bare `└ ` / context-summary tail without a header is not a panel footer, and a
    // bare truncation marker without a footer above it is not one either. The
    // first non-blank line is therefore required to be a real footer opener, NOT a
    // marker — so a lone `…` tail can never anchor a (nonexistent) footer.
    let first = footer_lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim())?;
    let opens_footer = is_single_message_footer_status_line(first)
        || completion_footer_section_header(first)
        || completion_footer_context_line(first);
    if !opens_footer {
        return None;
    }
    // Frozen ⇔ a braille spinner glyph still rides a footer line. A completion-
    // footer slot carries the spinner as a TRAILING glyph (`└ … ⠧`); a streaming/
    // merged status line carries it as a LEADING glyph (`⠧ 진행 중 …`). A finished
    // footer carries only ✓/✗/… terminal marks (or a non-spinner `**응답 완료**`
    // status line) and matches neither, so a completed panel is never reclaimed.
    let frozen = footer_lines
        .iter()
        .any(|line| line_ends_with_spinner_glyph(line) || line_starts_with_spinner_glyph(line));
    if !frozen {
        return None;
    }
    let body: String = lines[..start].join("\n");
    let footer: String = footer_lines.join("\n");
    Some((body, footer))
}

/// codex round-2 Medium: true when `line` is the bare truncation marker the
/// footer clamps emit on their own line when a section overruns the 600-byte
/// budget — `clamp_completion_task_section` and `clamp_footer_panel_text` both
/// append `format!("{prefix}\n{…}")`, so the marker is the literal `…` (a single
/// U+2026 HORIZONTAL ELLIPSIS) on its own line. Matched on the trimmed line so a
/// frozen-but-clamped footer tail (`… ⠧\n…`) is still reclaimable; the tail walk
/// passes such a marker through transparently but never lets it *open* a footer.
fn line_is_truncation_marker(line: &str) -> bool {
    line.trim() == "…"
}

/// True when `line` is one of the footer-section shapes the panel renders: a
/// streaming/merged status line, a `Tasks`/`Subagents` header, a `└ ` slot line,
/// or an icon-led context summary. Used only by the tail-anchored reclaim split.
fn line_is_footer_shaped(line: &str) -> bool {
    let trimmed = strip_subtext_prefix(line.trim());
    is_single_message_footer_status_line(trimmed)
        || completion_footer_section_header(trimmed)
        || completion_footer_context_line(trimmed)
        || trimmed.starts_with("└ ")
}

fn strip_subtext_prefix(line: &str) -> &str {
    line.strip_prefix("-# ").unwrap_or(line)
}

fn line_ends_with_spinner_glyph(line: &str) -> bool {
    line.trim_end().chars().next_back().is_some_and(|c| {
        SINGLE_MESSAGE_PANEL_SPINNER_FRAMES
            .iter()
            .any(|f| f.starts_with(c))
    })
}

/// True when the first non-whitespace char of `line` is a braille spinner frame
/// — the live shape of a streaming/merged footer status line (`⠧ 진행 중 …`).
fn line_starts_with_spinner_glyph(line: &str) -> bool {
    line.trim_start().chars().next().is_some_and(|c| {
        SINGLE_MESSAGE_PANEL_SPINNER_FRAMES
            .iter()
            .any(|f| f.starts_with(c))
    })
}

/// #3412: produce the one-shot finalize edit for a frozen panel. Strips the
/// tail-anchored animated footer/Tasks block (codex round-1 High-b), preserves
/// the body verbatim, appends a single `STARTUP_RECLAIM_STOP_MARKER` line, and
/// re-balances ``` fences (#3394) so a body that ended mid code block stays
/// renderable. Returns `None` when there is no frozen TAIL footer to reclaim (so
/// the caller can skip the edit) — the body itself is NEVER mutated, and a
/// mid-body footer quote never matches.
pub(in crate::services::discord) fn reclaim_finalize_text(
    last_edit_text: &str,
    provider: &super::ProviderKind,
) -> Option<String> {
    if !text_has_frozen_spinner_footer(last_edit_text) {
        return None; // No frozen TAIL footer (or a mid-body quote) — skip.
    }
    let (body, _footer) = tail_frozen_footer_split(last_edit_text)?;
    let _ = provider; // body is preserved verbatim; provider reserved for parity.
    let body = body.trim_end();
    let repaired = repair_fence_parity(body);
    let stop = STARTUP_RECLAIM_STOP_MARKER;
    let finalized = if repaired.trim().is_empty() {
        stop.to_string()
    } else {
        format!("{repaired}\n\n{stop}")
    };
    if finalized == last_edit_text {
        None
    } else {
        Some(finalized)
    }
}

pub(in crate::services::discord) fn strip_streaming_footer(
    last_edit_text: &str,
    provider: &super::ProviderKind,
) -> Option<String> {
    if footer_starts_with_spinner(last_edit_text) {
        return Some(String::new());
    }

    if completion_footer_starts(last_edit_text) {
        return Some(String::new());
    }

    if let Some(cleaned) = strip_completion_footer(last_edit_text, provider) {
        return Some(cleaned);
    }

    if let Some((body, _footer)) = split_footer(last_edit_text) {
        let cleaned = super::formatting::format_for_discord_with_status_panel(body, provider);
        return if cleaned == last_edit_text {
            None
        } else {
            Some(cleaned)
        };
    }

    if let Some(finalized) =
        super::formatting::finalize_stale_streaming_footer(last_edit_text, provider)
    {
        return Some(finalized);
    }

    None
}

pub(in crate::services::discord) fn same_streaming_footer_except_spinner_tick(
    previous: &str,
    next: &str,
) -> bool {
    if previous == next {
        return true;
    }
    let Some((previous_body, previous_footer)) = streaming_footer_split(previous) else {
        return false;
    };
    let Some((next_body, next_footer)) = streaming_footer_split(next) else {
        return false;
    };
    previous_body == next_body
        && normalize_footer_spinner_header(previous_footer)
            == normalize_footer_spinner_header(next_footer)
}

pub(in crate::services::discord) fn streaming_footer_text_changed(
    footer_mode: bool,
    previous: &str,
    next: &str,
) -> bool {
    previous != next && !(footer_mode && same_streaming_footer_except_spinner_tick(previous, next))
}

pub(in crate::services::discord) fn streaming_footer_only_surface_was_exposed(
    text: &str,
    provider: &super::ProviderKind,
) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && text_has_single_message_footer_surface(trimmed)
        && strip_streaming_footer(text, provider).is_some_and(|cleaned| cleaned.trim().is_empty())
}

pub(in crate::services::discord) fn strip_placeholder_terminal_status(
    text: &str,
    provider: &super::ProviderKind,
) -> String {
    let text = strip_streaming_footer(text, provider).unwrap_or_else(|| text.to_string());
    strip_inprogress_indicators(&text)
}

fn strip_inprogress_indicators(body: &str) -> String {
    let mut lines: Vec<&str> = body
        .lines()
        .filter(|line| !is_inprogress_indicator_line(line))
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn is_inprogress_indicator_line(line: &str) -> bool {
    line.trim_start()
        .chars()
        .next()
        .is_some_and(is_spinner_prefix_char)
}

fn is_spinner_prefix_char(ch: char) -> bool {
    matches!(
        ch,
        '⠏' | '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇'
    )
}

fn text_has_single_message_footer_surface(text: &str) -> bool {
    text.lines()
        .filter_map(|line| {
            let line = strip_subtext_prefix(line.trim());
            (!line.is_empty()).then_some(line)
        })
        .any(|line| {
            completion_footer_context_line(line)
                || line == "Tasks"
                || line == "Subagents"
                || line.starts_with("└ ")
                // #4601: the footer's split last-update line identifies the panel surface.
                || line.starts_with("마지막 업데이트 ")
                // Legacy pre-#3983 merged header shape.
                || (line.contains(" — ") && line.contains("(<t:"))
        })
}

fn streaming_footer_split(text: &str) -> Option<(&str, &str)> {
    if footer_starts_with_spinner(text) {
        return Some(("", text));
    }
    split_footer(text)
}

fn normalize_footer_spinner_header(footer: &str) -> String {
    let mut normalized = Vec::new();
    let mut header_normalized = false;
    for line in footer.lines() {
        if !header_normalized && let Some(status) = strip_footer_braille_spinner_prefix(line.trim())
        {
            normalized.push(format!("⠿ {status}"));
            header_normalized = true;
        } else {
            normalized.push(line.to_string());
        }
    }
    normalized.join("\n")
}

fn split_footer(text: &str) -> Option<(&str, &str)> {
    let mut search_end = text.len();
    while let Some(idx) = text[..search_end].rfind("\n\n") {
        let body = &text[..idx];
        let footer = &text[(idx + 2)..];
        if footer_starts_with_spinner(footer) {
            return Some((body, footer));
        }
        search_end = idx;
    }
    None
}

fn strip_completion_footer(text: &str, provider: &super::ProviderKind) -> Option<String> {
    let mut search_end = text.len();
    while let Some(idx) = text[..search_end].rfind("\n\n") {
        let body = &text[..idx];
        let footer = &text[(idx + 2)..];
        if completion_footer_starts_after_body(footer, body) {
            if completion_footer_first_line_is_section_header(footer)
                && (body_ends_with_completion_context_line(body)
                    || body_ends_with_single_message_footer_status_line(body))
            {
                search_end = idx;
                continue;
            }
            return Some(super::formatting::format_for_discord_with_status_panel(
                body, provider,
            ));
        }
        search_end = idx;
    }

    None
}

fn completion_footer_starts(footer: &str) -> bool {
    let mut lines = footer.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next().map(str::trim) else {
        return false;
    };
    completion_footer_context_line(first)
        || (completion_footer_section_header(first) && completion_footer_has_slot_shape(footer))
}

fn completion_footer_starts_after_body(footer: &str, body: &str) -> bool {
    let mut lines = footer.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next().map(str::trim) else {
        return false;
    };
    if completion_footer_context_line(first) {
        return true;
    }
    completion_footer_section_header(first)
        && (completion_footer_has_slot_shape(footer)
            || body_ends_with_completion_context_line(body)
            || body_ends_with_single_message_footer_status_line(body))
}

fn completion_footer_context_line(line: &str) -> bool {
    let line = strip_subtext_prefix(line).trim_start();
    line.starts_with("Context   ")
        || ((line.starts_with("📦 ") || line.starts_with("⚠️ ") || line.starts_with("⚠ "))
            && line.contains("auto-compact"))
}

fn completion_footer_section_header(line: &str) -> bool {
    matches!(strip_subtext_prefix(line), "Tasks" | "Subagents")
}

fn completion_footer_has_slot_shape(footer: &str) -> bool {
    footer
        .lines()
        .any(|line| strip_subtext_prefix(line).starts_with("└ "))
}

fn completion_footer_first_line_is_section_header(footer: &str) -> bool {
    footer
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .is_some_and(completion_footer_section_header)
}

fn body_ends_with_completion_context_line(body: &str) -> bool {
    body.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .is_some_and(completion_footer_context_line)
}

fn body_ends_with_single_message_footer_status_line(body: &str) -> bool {
    body.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .is_some_and(is_single_message_footer_status_line)
}

fn footer_starts_with_spinner(footer: &str) -> bool {
    let Some(first_footer_line) = footer.lines().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    is_single_message_footer_status_line(first_footer_line.trim())
}

fn is_single_message_footer_status_line(line: &str) -> bool {
    super::formatting::is_streaming_placeholder_status_line(line)
        || is_merged_footer_status_line(line)
}

fn is_merged_footer_status_line(line: &str) -> bool {
    let Some(status) = strip_footer_braille_spinner_prefix(line) else {
        return false;
    };
    // #3983: the header first line is now the BARE activity label — the provider +
    // relative-start suffix moved to the separate time line. Match those exact
    // labels so a user body line that merely opens with the same words (e.g.
    // `진행 중 — my note`) is not mistaken for the panel status line now that the
    // disambiguating `(<t:` suffix is gone.
    is_panel_activity_status_label(status)
        // Legacy pre-#3983 merged header (`<label> — <provider> (<t:..:R>)`), kept so
        // in-flight messages during a rollout keep stripping/comparing correctly.
        || (status.contains(" — ")
            && status.contains("(<t:")
            && legacy_merged_status_prefix(status))
}

/// #3983: exact-shape match for the bare footer activity labels
/// (`freshness::render_activity_line`, marker emoji already swapped for the
/// spinner). Fixed labels match exactly; parameterized labels match their
/// `<phrase> (…)` shape — never a bare prefix — so ordinary prose does not
/// false-positive without the old `(<t:` disambiguator.
fn is_panel_activity_status_label(status: &str) -> bool {
    matches!(
        status,
        "진행 중" | "monitor 대기" | "완료" | "백그라운드 완료" | "scheduled wakeup"
    ) || status.starts_with("응답 지연")
        || parenthesized_status_label(status, "scheduled wakeup")
        || parenthesized_status_label(status, "마지막 도구")
        || parenthesized_status_label(status, "도구 실행 중")
        || parenthesized_status_label(status, "subagent 실행 중")
        || parenthesized_status_label(status, "workflow 실행 중")
}

/// True when `status` is exactly `<phrase> (…)` — the shape of the parameterized
/// activity labels — so a bare `<phrase> …` prose line does not match.
fn parenthesized_status_label(status: &str, phrase: &str) -> bool {
    status
        .strip_prefix(phrase)
        .is_some_and(|rest| rest.starts_with(" (") && rest.ends_with(')'))
}

fn legacy_merged_status_prefix(status: &str) -> bool {
    status.starts_with("진행 중")
        || status.starts_with("monitor 대기")
        || status.starts_with("scheduled wakeup")
        || status.starts_with("**백그라운드 완료**")
        || status.starts_with("**응답 완료**")
        || status.starts_with("도구 실행 중")
        || status.starts_with("subagent 실행 중")
        || status.starts_with("workflow 실행 중")
}

fn strip_footer_braille_spinner_prefix(line: &str) -> Option<&str> {
    const BRAILLE_SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

    let line = strip_subtext_prefix(line.trim());
    let mut chars = line.chars();
    let first = chars.next()?;
    if !BRAILLE_SPINNER_FRAMES.contains(&first) || !chars.next().is_some_and(char::is_whitespace) {
        return None;
    }
    Some(chars.as_str().trim())
}

#[cfg(test)]
#[path = "single_message_panel_tests.rs"]
mod tests;
