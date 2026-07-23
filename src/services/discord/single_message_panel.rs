use std::sync::OnceLock;

/// Byte budget for the COMPLETION footer task section (#3089 S3) — the final
/// turn message keeps a compact task summary. The LIVE streaming panel uses the
/// larger `SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES` below.
pub(in crate::services::discord) const SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES: usize = 600;
/// #3497: byte budget for the LIVE single-message footer panel BODY (Recent +
/// Tasks/Subagents/Context; excludes the spinner/status header). Normal Recent
/// output is compact after #3806, but the footer clamp still handles legacy or
/// debug-shaped fenced Recent sections as whole blocks. The relay body auto-uses
/// the remaining message space (`DISCORD_MSG_LIMIT − footer_len − margin`) and
/// rolls over when longer, so commentary is preserved across messages rather
/// than starving the terminal panel.
pub(in crate::services::discord) const SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES: usize = 1200;
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
        let raw = std::env::var("AGENTDESK_SINGLE_MESSAGE_PANEL").ok();
        let enabled = parse_single_message_panel_flag(raw.as_deref());
        let state = if enabled { "enabled" } else { "disabled" };
        tracing::info!("  ✓ single_message_panel: {state}");
        enabled
    })
}

fn parse_single_message_panel_flag(raw: Option<&str>) -> bool {
    // #3560: default-ON (opt-out). The rollout gate previously short-circuited a
    // missing env var to `false`, so any environment without an explicit
    // `AGENTDESK_SINGLE_MESSAGE_PANEL=1` silently fell back to the legacy panel.
    // Mirror the original explicit truthy-allowlist as an explicit disable-list:
    // only the documented opt-out tokens "0" / case-insensitive "false" disable
    // the feature. Everything else — unset, empty, or garbage — stays enabled so
    // a default-ON feature is never quietly switched off by an empty env value.
    match raw.map(str::trim) {
        None => true,
        Some(value) => value != "0" && !value.eq_ignore_ascii_case("false"),
    }
}

pub(in crate::services::discord) fn footer_mode_enabled(
    single_message_panel_enabled: bool,
    status_panel_v2_enabled: bool,
) -> bool {
    single_message_panel_enabled && status_panel_v2_enabled
}

pub(in crate::services::discord) fn separate_status_panel_enabled_for_flags(
    single_message_panel_enabled: bool,
    status_panel_v2_enabled: bool,
) -> bool {
    status_panel_v2_enabled
        && !footer_mode_enabled(single_message_panel_enabled, status_panel_v2_enabled)
}

pub(in crate::services::discord) fn separate_status_panel_enabled(
    status_panel_v2_enabled: bool,
) -> bool {
    separate_status_panel_enabled_for_flags(enabled(), status_panel_v2_enabled)
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
    block
        .lines()
        .filter(|line| line.trim() != "\u{2063}")
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("-# {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(in crate::services::discord) fn compose_completion_footer_text(
    body: &str,
    completion_block: Option<&str>,
) -> String {
    let body = body.trim_end();
    let Some(block) = completion_block
        .map(str::trim)
        .filter(|block| !block.is_empty())
    else {
        return body.to_string();
    };
    let block = completion_footer_subtext(block);
    if body.is_empty() {
        return clamp_footer_status_block(block);
    }

    let suffix = format!("\n\n{block}");
    let max_len = super::DISCORD_MSG_LIMIT.saturating_sub(suffix.len());
    let base = if body.len() > max_len {
        let safe_end = super::formatting::floor_char_boundary(body, max_len);
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
    // #3391 delivered-ID honesty invariant: `delivered_terminal_ids` for this
    // footer were already computed from the footer block (completion_footer.rs:202)
    // and are evicted once this edit returns Ok. If repair ate the footer, the
    // ✓/✗ marks would vanish from the delivered text yet their slots would still
    // be evicted — reporting marks the user never saw. Repairing only the body
    // keeps every footer mark in the delivered text, so whenever this edit
    // succeeds every reported terminal slot's mark was actually present.
    //
    // The footer block is fence-balanced by construction: `render_completion_footer`
    // emits only the Context line plus Tasks/Subagents slot lines (no fenced Recent
    // block lives here), so it carries zero ``` runs (an even count). balanced base
    // + balanced footer = balanced combined.
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
    let panel_body = clamp_footer_panel_text(panel_body);
    if panel_body.trim().is_empty() {
        Some(header)
    } else {
        Some(format!("{header}\n{panel_body}"))
    }
}

fn merged_footer_header_line(indicator: &str, header_line: &str) -> Option<String> {
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

    const TRUNCATION_MARKER: &str = "…";
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
                .is_some_and(|line| line.trim_start().starts_with("```"))
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
    // `clamp_footer_panel_text` body trim and this Discord-limit trim — either of
    // which can chop the Recent block's closing ```). Re-balance fence parity on
    // EVERY return path so Discord never renders a dangling fence as literal text.
    let max_bytes = super::DISCORD_MSG_LIMIT.saturating_sub(6);
    let clamped = if status_block.len() <= max_bytes {
        status_block
    } else {
        let ellipsis = "…";
        let body_budget = max_bytes.saturating_sub(ellipsis.len());
        if body_budget == 0 {
            ellipsis.to_string()
        } else {
            let safe_end = super::formatting::floor_char_boundary(&status_block, body_budget);
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
/// `└ ` slot line, or `Context   ` summary), the run must contain a braille
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
    // bare `└ ` / `Context   ` tail without a header is not a panel footer, and a
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
/// or a `Context   ` summary. Used only by the tail-anchored reclaim split.
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
            line.starts_with("Context   ")
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
    strip_subtext_prefix(line).starts_with("Context   ")
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
mod tests {
    use super::super::DISCORD_MSG_LIMIT;
    use super::super::ProviderKind;
    use super::super::footer_view_reconciler as footer_registry;
    use crate::services::agent_protocol::StatusEvent;
    use poise::serenity_prelude::{ChannelId, MessageId};

    fn panel_portion(status_block: &str) -> &str {
        status_block
            .split_once('\n')
            .map(|(_, panel)| panel)
            .unwrap_or("")
    }

    fn footer_header(status_block: &str) -> &str {
        status_block.lines().next().unwrap_or("")
    }

    fn push_unfinished_subagent(channel_id: ChannelId) -> std::sync::Arc<super::super::SharedData> {
        let shared = super::super::make_shared_data_for_tests();
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::SubagentStart {
                subagent_type: Some("reviewer".to_string()),
                desc: Some("Long background job".to_string()),
                agent_id: None,
                tool_use_id: Some(format!("tool-{}", channel_id.get())),
                background: true,
            },
        );
        shared
    }

    fn push_unfinished_background_task(
        channel_id: ChannelId,
    ) -> std::sync::Arc<super::super::SharedData> {
        let shared = super::super::make_shared_data_for_tests();
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::BackgroundTaskStart {
                name: "Bash".to_string(),
                summary: "Run background codex".to_string(),
                tool_use_id: format!("tool-{}", channel_id.get()),
            },
        );
        shared
    }

    fn push_finished_and_running_background_tasks(
        channel_id: ChannelId,
    ) -> std::sync::Arc<super::super::SharedData> {
        let shared = super::super::make_shared_data_for_tests();
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::BackgroundTaskStart {
                name: "Bash".to_string(),
                summary: "Finished job".to_string(),
                tool_use_id: format!("tool-done-{}", channel_id.get()),
            },
        );
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::BackgroundTaskEnd {
                tool_use_id: format!("tool-done-{}", channel_id.get()),
                success: true,
            },
        );
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::BackgroundTaskStart {
                name: "Bash".to_string(),
                summary: "Running job".to_string(),
                tool_use_id: format!("tool-run-{}", channel_id.get()),
            },
        );
        shared
    }

    #[test]
    fn single_message_panel_flag_defaults_on_when_unset() {
        // #3560: missing env var means default-ON (opt-out), not the legacy OFF.
        assert!(super::parse_single_message_panel_flag(None));
    }

    #[test]
    fn single_message_panel_flag_treats_non_optout_values_as_enabled() {
        // #3560: only "0" / "false" disable the gate. Everything else — including
        // an empty string or garbage — keeps the default-ON feature enabled so it
        // is never quietly switched off by an unintended env value.
        for raw in [
            "1", "true", "TRUE", "TrUe", " true ", "", "yes", "on", "garbage",
        ] {
            assert!(
                super::parse_single_message_panel_flag(Some(raw)),
                "{raw:?} should enable the flag"
            );
        }
    }

    #[test]
    fn single_message_panel_flag_rejects_documented_optout_values() {
        // #3560: the disable-list mirrors the original truthy-allowlist — only the
        // documented opt-out tokens "0" and case-insensitive "false" turn it off.
        for raw in ["0", "false", "FALSE"] {
            assert!(
                !super::parse_single_message_panel_flag(Some(raw)),
                "{raw:?} should disable the flag"
            );
        }
    }

    #[test]
    fn footer_mode_requires_both_flags() {
        assert!(super::footer_mode_enabled(true, true));
        assert!(!super::footer_mode_enabled(true, false));
        assert!(!super::footer_mode_enabled(false, true));
    }

    #[test]
    fn footer_status_block_keeps_spinner_first() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let block = super::compose_footer_status_block("⠸", panel);

        assert!(block.starts_with("-# ⠸ 진행 중 — Claude (<t:1700000000:R>)"));
        assert!(!footer_header(&block).contains('🟢'));
        assert!(!block.contains("계속 처리 중"));
        assert!(block.contains("\n\nSubagents\n└ review inspect"));
    }

    #[test]
    fn footer_status_block_empty_panel_falls_back_to_processing_line() {
        assert_eq!(
            super::compose_footer_status_block("⠸", ""),
            "⠸ 계속 처리 중"
        );
        assert_eq!(
            super::compose_footer_status_block("⠸", " \n\t "),
            "⠸ 계속 처리 중"
        );
    }

    #[test]
    fn footer_panel_under_budget_is_unchanged_s3() {
        let panel = "Header\n\nTools\n└ cargo test";
        let block = super::compose_footer_status_block("⠸", panel);

        assert_eq!(block, "-# ⠸ Header\n\nTools\n└ cargo test");
        assert!(!panel_portion(&block).ends_with("\n…"));
    }

    #[test]
    fn footer_panel_over_budget_excludes_merged_header_from_budget_s3() {
        let huge_panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n{}\n{}\n{}",
            "a".repeat(590),
            "b".repeat(590),
            "c".repeat(590)
        );
        let block = super::compose_footer_status_block("⠸", &huge_panel);
        let (header, panel) = block
            .split_once('\n')
            .expect("over-budget panel should keep merged header and panel body");

        assert_eq!(header, "-# ⠸ 진행 중 — Claude (<t:1700000000:R>)");
        assert!(panel.len() <= super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES);
        assert!(panel.ends_with("\n…") || panel == "…");
        assert!(block.len() > super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES);
    }

    #[test]
    fn streaming_footer_semantic_compare_ignores_spinner_tick_only_3717() {
        let previous =
            "visible body\n\n⠋ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let next =
            "visible body\n\n⠙ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";

        assert!(super::same_streaming_footer_except_spinner_tick(
            previous, next
        ));
    }

    #[test]
    fn streaming_footer_semantic_compare_keeps_body_and_panel_changes_3717() {
        let previous =
            "visible body\n\n⠋ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let body_changed = "visible body plus output\n\n⠙ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let panel_changed =
            "visible body\n\n⠙ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review done";
        let completed =
            "visible body\n\n✅ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";

        assert!(!super::same_streaming_footer_except_spinner_tick(
            previous,
            body_changed
        ));
        assert!(!super::same_streaming_footer_except_spinner_tick(
            previous,
            panel_changed
        ));
        assert!(!super::same_streaming_footer_except_spinner_tick(
            previous, completed
        ));
    }

    /// #3394: the footer finalization sink must re-balance fence parity. A
    /// legacy/debug-shaped fenced Recent block chopped by the body clamp must
    /// not emit a dangling ``` for Discord to render as literal text.
    #[test]
    fn footer_status_block_repairs_dangling_fence_after_clamp_3394() {
        let panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n🖥️ Recent\n```text\n{}\n```",
            "echo hello\n".repeat(120)
        );
        let block = super::compose_footer_status_block("⠸", &panel);

        let fences = block.matches("```").count();
        assert_eq!(
            fences % 2,
            0,
            "footer leaked an unterminated fence: {block}"
        );
        assert!(!block.contains("```text") || fences >= 2);
    }

    /// #3495: when the panel clamp severs a legacy/debug-shaped fenced 🖥️ Recent
    /// block, the WHOLE section (header + fence + body) is dropped — Discord
    /// must never see a bare `🖥️ Recent` header with no terminal body (the
    /// intermittent "터미널 칸이 사라짐" symptom). When the block fits, it is
    /// preserved intact.
    #[test]
    fn footer_panel_clamp_drops_severed_recent_section_3495() {
        // Small leading line so the cut lands inside the Recent section: the
        // header (+ opener) would survive a naive clamp but its body cannot.
        let severed = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\nstreaming line\n🖥️ Recent (mac-book-release)\n```text\n{}\n```",
            "echo hello\n".repeat(130)
        );
        let block = super::compose_footer_status_block("⠸", &severed);
        assert!(
            !block.contains("🖥️ Recent"),
            "severed Recent header left without a terminal body: {block}"
        );
        assert_eq!(
            block.matches("```").count() % 2,
            0,
            "footer leaked an unterminated fence: {block}"
        );
        assert!(block.len() <= super::super::DISCORD_MSG_LIMIT);

        // Control: a Recent block that fits the budget is preserved intact.
        let fits =
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n🖥️ Recent (host)\n```text\necho hi\n```";
        let kept = super::compose_footer_status_block("⠸", fits);
        assert!(
            kept.contains("🖥️ Recent"),
            "fitting Recent block was dropped: {kept}"
        );
        assert!(
            kept.contains("echo hi"),
            "fitting Recent body was dropped: {kept}"
        );
        assert_eq!(
            kept.matches("```").count() % 2,
            0,
            "fitting block unbalanced: {kept}"
        );
    }

    /// #3497: the raised 1200B panel budget renders a full ~1000B 🖥️ Recent
    /// terminal block that the prior 600B budget would have truncated/dropped.
    #[test]
    fn footer_panel_budget_shows_recent_block_over_legacy_600_3497() {
        let recent_body = "L cargo build output line\n".repeat(40); // ~1040B
        assert!(recent_body.len() > 600 && recent_body.len() < 1100);
        let panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n🖥️ Recent (host)\n```text\n{recent_body}```"
        );
        let block = super::compose_footer_status_block("⠸", &panel);
        // The full fenced block survives (not severed by the clamp) under 1200B,
        // whereas a 600B budget would have dropped the whole Recent section (#3495).
        assert!(
            block.contains("🖥️ Recent"),
            "Recent header dropped under 1200B budget: {block}"
        );
        assert!(
            block.contains("L cargo build output line"),
            "Recent terminal body truncated under 1200B budget: {block}"
        );
        assert_eq!(
            block.matches("```").count() % 2,
            0,
            "unbalanced fence: {block}"
        );
        assert!(block.len() <= super::super::DISCORD_MSG_LIMIT);
    }

    /// #3394 round 2 (P1): when the response BODY carries an unterminated code
    /// fence (the Discord-limit trim cut inside a body fence), the boundary-scoped
    /// repair must drop only the body's dangling opener and KEEP the appended
    /// completion footer — its ✓ marks must survive. On HEAD the combined repair
    /// deleted from the body opener through the footer, taking the ✓ with it while
    /// #3391 still evicted the slot, breaking delivered-ID honesty.
    #[test]
    fn compose_completion_footer_repair_scoped_to_body_keeps_footer_marks_3394() {
        let body = "Here is some output:\n\n```text\nchopped streamed body";
        let footer = "Context   📦 154.6k / 1.0M tokens (15%)\n\nTasks\n└ Bash Finished job ✓";
        let composed = super::compose_completion_footer_text(body, Some(footer));

        // The footer block (and its terminal ✓) survives composition.
        assert!(
            composed.contains("Bash Finished job ✓"),
            "footer ✓ was deleted by combined fence repair: {composed}"
        );
        assert!(composed.contains("Context   📦"));
        assert!(composed.contains("Tasks"));
        // The body's dangling opener is dropped (boundary-scoped repair).
        assert!(
            !composed.contains("```text"),
            "body dangling fence leaked: {composed}"
        );
        assert!(composed.contains("Here is some output:"));
        // Combined parity is even.
        assert_eq!(
            composed.matches("```").count() % 2,
            0,
            "composed text has odd fence parity: {composed}"
        );
    }

    /// #3394 round 2 (P1): the registered-refresh composition site
    /// (`completion_footer_edit_for_registered_target_at`, ~258) shares the
    /// `compose_completion_footer_text` pattern. A registered body carrying an
    /// unterminated fence must still emit the rendered footer's terminal ✓ in the
    /// delivered edit text, with even combined parity — so the #3391 eviction that
    /// follows a successful edit only drops marks the user actually saw.
    #[test]
    fn registered_refresh_repair_scoped_to_body_keeps_footer_marks_3394() {
        let channel_id = ChannelId::new(3_394_201);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_finished_and_running_background_tasks(channel_id);
        let body_with_dangling_fence = "Streaming reply\n\n```text\ncut off mid fence";
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_394_301),
            &ProviderKind::Claude,
            1_800_000_000,
            body_with_dangling_fence,
            None,
            true,
        );

        let edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_001,
        )
        .expect("registered refresh should render the terminal mark");

        assert!(
            edit.text.contains("Bash Finished job ✓"),
            "registered-refresh footer ✓ was deleted by fence repair: {}",
            edit.text
        );
        assert!(edit.text.contains("Bash Running job ⠸"));
        assert!(
            !edit.text.contains("```text"),
            "body dangling fence leaked: {}",
            edit.text
        );
        assert_eq!(
            edit.text.matches("```").count() % 2,
            0,
            "registered-refresh composed text has odd fence parity: {}",
            edit.text
        );
        // #3391: the delivered ✓ slot identity is reported for this edit, and it is
        // genuinely present in the delivered text above.
        assert!(!edit.delivered_terminal_ids.is_empty());
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    /// #3394 round 2: a body with a BALANCED fence and a footer compose without
    /// any repair touching the body's closed fence — parity stays even and both
    /// the fenced body and the footer survive intact.
    #[test]
    fn completion_footer_subtext_prefixes_each_nonempty_line_4080() {
        assert_eq!(
            super::completion_footer_subtext(
                "Context   📦 10 / 100 tokens\n\nTasks\n└ Bash Done ✓\n⏱ 2m 34s"
            ),
            "-# Context   📦 10 / 100 tokens\n\n-# Tasks\n-# └ Bash Done ✓\n-# ⏱ 2m 34s"
        );
    }

    #[test]
    fn subtext_completion_footer_is_stripped_at_terminal_reconciliation_4080() {
        let text = "Final answer\n\n-# Context   📦 10 / 100 tokens\n\n-# Tasks\n-# └ Bash Done ✓\n-# ⏱ 2m 34s";
        assert_eq!(
            super::strip_streaming_footer(text, &ProviderKind::Claude),
            Some("Final answer".to_string())
        );
    }

    #[test]
    fn compose_completion_footer_keeps_balanced_body_fence_intact_3394() {
        let body = "intro\n\n```text\nclosed body\n```\noutro";
        let footer = "Context   📦 1.0k / 1.0M tokens (1%)\n\nTasks\n└ Bash Done ✓";
        let composed = super::compose_completion_footer_text(body, Some(footer));

        assert!(composed.contains("```text\nclosed body\n```"));
        assert!(composed.contains("Bash Done ✓"));
        assert_eq!(composed.matches("```").count() % 2, 0);
    }

    #[test]
    fn footer_panel_truncates_on_line_boundaries_s3() {
        let second = "a".repeat(500);
        let third = "b".repeat(500);
        let fourth = "c".repeat(500);
        let panel =
            format!("🟢 진행 중 — Claude (<t:1700000000:R>)\n\n{second}\n{third}\n{fourth}");
        let block = super::compose_footer_status_block("⠸", &panel);
        let truncated_lines: Vec<&str> = panel_portion(&block).lines().collect();

        assert_eq!(
            truncated_lines,
            vec!["", second.as_str(), third.as_str(), "…"]
        );
        assert!(!panel_portion(&block).contains(fourth.as_str()));
        assert!(panel_portion(&block).len() <= super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES);
    }

    #[test]
    fn footer_panel_byte_clamps_first_line_on_char_boundary_s3() {
        let panel_body_first_line = "가🙂".repeat(200);
        let panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n{panel_body_first_line}\nSubagents\n└ reviewer inspect"
        );
        let block = super::compose_footer_status_block("⠸", &panel);
        let panel = panel_portion(&block);
        let panel_lines: Vec<&str> = panel.lines().collect();

        assert!(std::str::from_utf8(panel.as_bytes()).is_ok());
        assert!(panel.len() <= super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES);
        assert_eq!(panel_lines.last().copied(), Some("…"));
        assert_eq!(panel_lines.len(), 2);
        assert!(!panel.contains("Subagents"));
    }

    #[test]
    fn footer_rollover_reservation_is_bound_by_panel_budget_s3() {
        const STREAMING_PLACEHOLDER_MARGIN_CHARS: usize = 10;

        let huge_panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nTools\n{}",
            "└ cargo test --lib single_message_panel ".repeat(120)
        );
        let status_block = super::compose_footer_status_block("⠸", &huge_panel);
        let merged_header = footer_header(&status_block);
        let max_footer_chars = 2
            + merged_header.chars().count()
            + 1
            + super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES;
        let footer = format!("\n\n{status_block}");
        let footer_chars = footer.chars().count();
        let expected_body_budget = DISCORD_MSG_LIMIT
            .saturating_sub(footer_chars + STREAMING_PLACEHOLDER_MARGIN_CHARS)
            .max(1);
        let minimum_body_budget = DISCORD_MSG_LIMIT
            .saturating_sub(max_footer_chars + STREAMING_PLACEHOLDER_MARGIN_CHARS)
            .max(1);
        let current_portion = "x".repeat(expected_body_budget + 1);
        let plan =
            super::super::formatting::plan_streaming_rollover(&current_portion, &status_block)
                .expect("body should roll over after reserving the bounded footer");

        assert!(footer_chars <= max_footer_chars);
        assert_eq!(plan.split_at, expected_body_budget);
        assert!(plan.split_at >= minimum_body_budget);
        assert!(plan.display_snapshot.ends_with(&footer));
    }

    #[test]
    fn footer_rollover_seed_carries_merged_header_but_frozen_chunk_does_not() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nTools\n└ cargo test --lib single_message_panel";
        let status_block = super::compose_footer_status_block("⠸", panel);
        let current_portion = "streamed body ".repeat(220);
        let plan =
            super::super::formatting::plan_streaming_rollover(&current_portion, &status_block)
                .expect("body should roll over after reserving the footer");
        let seed = super::super::formatting::build_streaming_placeholder_text("", &status_block);

        assert!(seed.starts_with("-# ⠸ 진행 중 — Claude (<t:1700000000:R>)"));
        assert!(seed.contains("Tools\n└ cargo test --lib single_message_panel"));
        assert!(!plan.frozen_chunk.contains("진행 중 — Claude"));
        assert!(!plan.frozen_chunk.contains("Tools\n└ cargo test"));
        assert!(
            plan.display_snapshot
                .ends_with(&format!("\n\n{status_block}"))
        );
    }

    #[test]
    fn terminal_footer_strip_removes_panel_block() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let rendered = format!(
            "Final answer\n\n{}",
            super::compose_footer_status_block("⠸", panel)
        );
        let finalized = super::finalize_streaming_footer(&rendered, &ProviderKind::Claude)
            .expect("panel footer should strip at terminal reconciliation");

        assert_eq!(finalized, "Final answer");
        assert!(!finalized.contains("계속 처리 중"));
        assert!(!finalized.contains("진행 중 — Claude"));
        assert!(!finalized.contains("Subagents"));
    }

    #[test]
    fn terminal_footer_strip_preserves_body_text_that_mentions_running_status() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let body = "Final answer\n\n본문에 진행 중 문구가 있어도 유지";
        let rendered = format!(
            "{body}\n\n{}",
            super::compose_footer_status_block("⠸", panel)
        );
        let finalized = super::finalize_streaming_footer(&rendered, &ProviderKind::Claude)
            .expect("merged footer should strip at terminal reconciliation");

        assert_eq!(finalized, body);
    }

    #[test]
    fn footer_mode_strip_preserves_spinner_prefixed_user_body_without_panel_timestamp() {
        let body = "Final answer\n\n⠋ 진행 중 — user-authored line";

        assert_eq!(
            super::strip_streaming_footer(body, &ProviderKind::Claude),
            None
        );
        assert_eq!(
            super::finalize_streaming_footer(body, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn footer_only_body_strips_to_empty_for_cleanup_callers() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let rendered = super::compose_footer_status_block("⠸", panel);

        assert_eq!(
            super::strip_streaming_footer(&rendered, &ProviderKind::Claude),
            Some(String::new())
        );
        assert_eq!(
            super::finalize_streaming_footer(&rendered, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn subtext_footer_only_completion_surface_is_exposed_4080() {
        let footer = super::completion_footer_subtext(
            "Context   📦 10 / 100 tokens\n\nTasks\n└ Bash Done ✓\n⏱ 2m 34s",
        );

        assert!(super::streaming_footer_only_surface_was_exposed(
            &footer,
            &ProviderKind::Claude
        ));
        assert_eq!(
            super::strip_streaming_footer(&footer, &ProviderKind::Claude),
            Some(String::new())
        );
    }

    #[test]
    fn footer_only_surface_recognizes_split_fixed_kst_time_lines() {
        let panel = "🟢 진행 중\n턴 트리거: https://discord.com/channels/1/2/3\n턴 시작 : 11-15 07:13:20 (<t:1700000000:R>)\n마지막 업데이트 : 11-15 07:18:20 (<t:1700000300:R>)";
        let rendered = super::compose_footer_status_block("⠸", panel);

        assert!(super::streaming_footer_only_surface_was_exposed(
            &rendered,
            &ProviderKind::Claude
        ));
    }

    #[test]
    fn footer_only_surface_recognizes_legacy_combined_time_line() {
        let panel = "🟢 진행 중\n\n마지막 업데이트 : 11-15 07:18:20 (<t:1700000300:R>) / 턴 시작 : 11-15 07:13:20 (<t:1700000000:R>)";
        let rendered = super::compose_footer_status_block("⠸", panel);

        assert!(super::streaming_footer_only_surface_was_exposed(
            &rendered,
            &ProviderKind::Claude
        ));
    }

    #[test]
    fn terminal_footer_replacement_keeps_completion_context_block() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let rendered = format!(
            "Final answer\n\n{}",
            super::compose_footer_status_block("⠸", panel)
        );
        let completion = "Context   📦 154.6k / 1.0M tokens (15%) · auto-compact 60%";

        let finalized = super::finalize_streaming_footer_with_completion(
            &rendered,
            &ProviderKind::Claude,
            Some(completion),
        )
        .expect("streaming footer should be replaced by completion block");

        assert_eq!(finalized, format!("Final answer\n\n-# {completion}"));
        assert!(!finalized.contains("진행 중 — Claude"));
        assert!(!finalized.contains("Subagents"));
    }

    #[test]
    fn completion_footer_strip_supports_suppression_exposure_test() {
        let completion = "Context   📦 154.6k / 1.0M tokens (15%) · auto-compact 60%\n\nSubagents\n└ bgworker Long job ✓";

        assert_eq!(
            super::strip_streaming_footer(completion, &ProviderKind::Claude),
            Some(String::new())
        );
        assert_eq!(
            super::strip_streaming_footer(
                &format!("visible assistant body\n\n{completion}"),
                &ProviderKind::Claude,
            ),
            Some("visible assistant body".to_string())
        );
    }

    #[test]
    fn completion_footer_strip_preserves_bare_user_section_heading_without_slot_evidence() {
        let body = "visible assistant body\n\nSubagents\n- user-authored note";

        assert_eq!(
            super::strip_streaming_footer(body, &ProviderKind::Claude),
            None
        );
        assert_eq!(
            super::finalize_streaming_footer(body, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn completion_footer_strip_still_removes_real_slot_section() {
        let body = "visible assistant body\n\nSubagents\n└ bgworker Long job ✓";

        assert_eq!(
            super::strip_streaming_footer(body, &ProviderKind::Claude),
            Some("visible assistant body".to_string())
        );
    }

    #[test]
    fn completion_footer_strip_removes_frozen_supersede_shape() {
        let completion = "Context   📦 154.6k / 1.0M tokens (15%) · auto-compact 60%\n\nSubagents\n└ bgworker Long job …";

        assert_eq!(
            super::strip_streaming_footer(
                &format!("visible assistant body\n\n{completion}"),
                &ProviderKind::Claude,
            ),
            Some("visible assistant body".to_string())
        );
    }

    #[test]
    fn registering_new_target_supersedes_old_footer_once_and_keeps_snapshot() {
        let channel_id = ChannelId::new(3_089_021);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        assert!(
            shared
                .ui
                .placeholder_live_events
                .set_context_panel_usage(channel_id, None, 154_600, 0, 0, 1_000_000, 60,)
        );
        let old_block = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
            .block
            .expect("old footer block");
        assert!(old_block.contains('⠸'));
        assert!(old_block.contains("Context   "));
        assert!(old_block.contains("Subagents\n└ "));

        assert_eq!(
            footer_registry::register_completion_footer_target(
                channel_id,
                MessageId::new(3_089_121),
                &ProviderKind::Claude,
                1_800_000_000,
                "Old answer",
                Some(&old_block),
                true,
            ),
            None
        );
        let new_block = old_block.replace('⠸', "⠼");
        let supersede = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_122),
            &ProviderKind::Claude,
            1_800_000_010,
            "New answer",
            Some(&new_block),
            true,
        )
        .expect("new target should supersede old target");

        assert_eq!(supersede.message_id, MessageId::new(3_089_121));
        assert!(supersede.remove_after_edit);
        assert!(supersede.text.starts_with("Old answer\n\n-# Context   "));
        assert!(supersede.text.contains("-# Subagents\n-# └ "));
        assert!(supersede.text.contains('…'));
        assert!(!supersede.text.contains('⠸'));
        assert!(!supersede.text.contains('⠼'));

        let latest = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠋",
            1_800_000_011,
        )
        .expect("new target should be the only registered target");
        assert_eq!(latest.message_id, MessageId::new(3_089_122));
        assert!(!latest.text.contains("Old answer"));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn carried_entry_notification_updates_latest_target_not_superseded_text() {
        let channel_id = ChannelId::new(3_089_022);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = super::super::make_shared_data_for_tests();
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::BackgroundTaskStart {
                name: "Bash".to_string(),
                summary: "Carried bash".to_string(),
                tool_use_id: "toolu_latest_bash".to_string(),
            },
        );
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::SubagentStart {
                subagent_type: Some("bgworker".to_string()),
                desc: Some("Carried agent".to_string()),
                agent_id: None,
                tool_use_id: Some("toolu_latest_agent".to_string()),
                background: true,
            },
        );
        let old_block = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
            .block
            .expect("old footer block");
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_221),
            &ProviderKind::Claude,
            1_800_000_000,
            "Old answer",
            Some(&old_block),
            true,
        );
        shared
            .ui
            .placeholder_live_events
            .clear_channel_preserving_footer_residuals(channel_id);
        let carried_block = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &ProviderKind::Claude, "⠼")
            .block
            .expect("carried footer block");
        let supersede = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_222),
            &ProviderKind::Claude,
            1_800_000_010,
            "New answer",
            Some(&carried_block),
            true,
        )
        .expect("new target should supersede old target");
        assert_eq!(supersede.message_id, MessageId::new(3_089_221));
        assert!(supersede.text.contains("Carried bash …"));
        assert!(supersede.text.contains("Carried agent …"));
        assert!(!supersede.text.contains('✓'));

        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::BackgroundTaskEnd {
                tool_use_id: "toolu_latest_bash".to_string(),
                success: true,
            },
        );
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::SubagentEnd {
                success: true,
                agent_id: None,
                desc: None,
                tool_use_id: Some("toolu_latest_agent".to_string()),
                summary: None,
                ack_only: false,
            },
        );
        let latest = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠋",
            1_800_000_011,
        )
        .expect("latest target should receive finalization");
        assert_eq!(latest.message_id, MessageId::new(3_089_222));
        assert!(latest.text.contains("Carried bash ✓"));
        assert!(latest.text.contains("Carried agent ✓"));
        assert!(!supersede.text.contains('✓'));
        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &latest,
            true,
        );
        assert!(!footer_registry::completion_footer_has_registered_target(
            channel_id
        ));
    }

    #[test]
    fn completion_footer_terminal_mark_renders_once_then_next_edit_drops_it() {
        let channel_id = ChannelId::new(3_391_101);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_finished_and_running_background_tasks(channel_id);
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_391_201),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        let edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_001,
        )
        .expect("first refresh should render the terminal mark");
        assert!(edit.text.contains("Bash Finished job ✓"));
        assert!(edit.text.contains("Bash Running job ⠸"));
        assert!(!edit.remove_after_edit);

        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &edit,
            true,
        );

        let next = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠼",
            1_800_000_002,
        )
        .expect("running entry keeps the target registered");
        assert!(!next.text.contains("Finished job"));
        assert!(next.text.contains("Bash Running job ⠼"));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn completion_footer_failed_delivery_retries_terminal_mark() {
        let channel_id = ChannelId::new(3_391_102);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_finished_and_running_background_tasks(channel_id);
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_391_202),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );
        let edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_001,
        )
        .expect("first refresh should render the terminal mark");

        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &edit,
            false,
        );

        let retry = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠼",
            1_800_000_002,
        )
        .expect("failed edit keeps the target registered");
        assert!(retry.text.contains("Bash Finished job ✓"));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn completion_footer_failed_final_edit_keeps_target_then_retry_commits_4025() {
        let channel_id = ChannelId::new(4_025_001);
        let message_id = MessageId::new(4_025_101);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_background_task(channel_id);
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            message_id,
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::BackgroundTaskEnd {
                tool_use_id: format!("tool-{}", channel_id.get()),
                success: true,
            },
        );

        let failed_edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_001,
        )
        .expect("final terminal refresh should target the registered message");
        assert_eq!(failed_edit.message_id, message_id);
        assert!(failed_edit.text.contains("Bash Run background codex ✓"));
        assert!(failed_edit.remove_after_edit);

        assert!(
            footer_registry::completion_footer_record_edit_result_for_edit(
                shared.as_ref(),
                channel_id,
                &failed_edit,
                false,
            )
        );
        assert!(
            footer_registry::completion_footer_has_registered_target(channel_id),
            "failed final edit must retain its retry target"
        );
        assert_eq!(
            footer_registry::completion_footer_registered_failure_count(channel_id),
            Some(1),
            "failed final edit must count toward the existing retry cap"
        );
        let retained = shared.ui.placeholder_live_events.render_completion_footer(
            channel_id,
            &ProviderKind::Claude,
            "⠼",
        );
        assert!(
            retained
                .block
                .as_deref()
                .is_some_and(|block| block.contains("Bash Run background codex ✓")),
            "failed delivery must not evict the terminal slot"
        );

        let retry = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠼",
            1_800_000_002,
        )
        .expect("failed final edit should remain available for retry");
        assert_eq!(retry.message_id, failed_edit.message_id);
        assert_eq!(retry.text, failed_edit.text);
        assert!(retry.remove_after_edit);

        assert!(
            footer_registry::completion_footer_record_edit_result_for_edit(
                shared.as_ref(),
                channel_id,
                &retry,
                true,
            )
        );
        assert!(!footer_registry::completion_footer_has_registered_target(
            channel_id
        ));
        assert_eq!(
            shared
                .ui
                .placeholder_live_events
                .render_completion_footer(channel_id, &ProviderKind::Claude, "⠴")
                .block,
            None,
            "terminal slot must be evicted only after successful recorded delivery"
        );
    }

    #[test]
    fn completion_footer_evicts_all_terminal_marks_delivered_by_one_edit() {
        let channel_id = ChannelId::new(3_391_103);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_finished_and_running_background_tasks(channel_id);
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::BackgroundTaskStart {
                name: "Bash".to_string(),
                summary: "Failed sweep".to_string(),
                tool_use_id: "tool-fail-3391103".to_string(),
            },
        );
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::BackgroundTaskEnd {
                tool_use_id: "tool-fail-3391103".to_string(),
                success: false,
            },
        );
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_391_203),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );
        let edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_001,
        )
        .expect("first refresh should render both terminal marks");
        assert!(edit.text.contains("Bash Finished job ✓"));
        assert!(edit.text.contains("Bash Failed sweep ✗"));

        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &edit,
            true,
        );

        let next = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠼",
            1_800_000_002,
        )
        .expect("running entry keeps the target registered");
        assert!(!next.text.contains("Finished job"));
        assert!(!next.text.contains("Failed sweep"));
        assert!(next.text.contains("Bash Running job ⠼"));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn migration_does_not_carry_delivered_terminal_marks_to_new_target() {
        let channel_id = ChannelId::new(3_391_104);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_finished_and_running_background_tasks(channel_id);
        let old_block = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
            .block
            .expect("old footer block");
        assert_eq!(
            footer_registry::register_completion_footer_target(
                channel_id,
                MessageId::new(3_391_204),
                &ProviderKind::Claude,
                1_800_000_000,
                "Old answer",
                Some(&old_block),
                true,
            ),
            None
        );
        let edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_001,
        )
        .expect("old target refresh");
        assert!(edit.text.contains("Bash Finished job ✓"));
        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &edit,
            true,
        );

        // #3386 migration: the channel footer moves to a newer message. The new
        // footer must not carry the already-delivered terminal mark, while the
        // frozen snapshot of the superseded message keeps it (that delivered
        // render IS "the once").
        let new_block = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &ProviderKind::Claude, "⠼")
            .block
            .expect("running entry still renders");
        assert!(!new_block.contains("Finished job"));
        let supersede = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_391_205),
            &ProviderKind::Claude,
            1_800_000_010,
            "New answer",
            Some(&new_block),
            true,
        )
        .expect("new target should supersede old target");
        assert_eq!(supersede.message_id, MessageId::new(3_391_204));
        assert!(supersede.text.contains("Bash Finished job ✓"));
        assert!(supersede.text.contains("Bash Running job …"));

        let latest = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠋",
            1_800_000_011,
        )
        .expect("new target refresh");
        assert_eq!(latest.message_id, MessageId::new(3_391_205));
        assert!(!latest.text.contains("Finished job"));
        assert!(latest.text.contains("Bash Running job ⠋"));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn ttl_freezes_carried_entries_on_latest_target() {
        let channel_id = ChannelId::new(3_089_023);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_background_task(channel_id);
        shared
            .ui
            .placeholder_live_events
            .clear_channel_preserving_footer_residuals(channel_id);
        let carried_block = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
            .block
            .expect("carried footer block");
        let now = 1_800_000_000;
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_123),
            &ProviderKind::Claude,
            now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS - 1,
            "Latest answer",
            Some(&carried_block),
            true,
        );

        let edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠼",
            now,
        )
        .expect("carried latest target should receive TTL freeze edit");

        assert_eq!(edit.message_id, MessageId::new(3_089_123));
        assert!(edit.remove_after_edit);
        assert!(
            edit.text
                .contains("-# Tasks\n-# └ Bash Run background codex …")
        );
        assert!(!edit.text.contains('⠼'));
        assert!(!edit.text.contains('✓'));
        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &edit,
            true,
        );
        assert!(!footer_registry::completion_footer_has_registered_target(
            channel_id
        ));
    }

    #[test]
    fn completion_footer_ttl_freezes_unfinished_entries_then_forgets_target() {
        let channel_id = ChannelId::new(3_089_001);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let now = 1_800_000_000;
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_101),
            &ProviderKind::Claude,
            now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS - 1,
            "Final answer",
            None,
            true,
        );

        let edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            now,
        )
        .expect("expired unfinished footer should render one freeze edit");

        assert!(edit.remove_after_edit);
        assert!(edit.text.contains("-# Subagents\n-# └"));
        assert!(edit.text.contains('…'));
        assert!(!edit.text.contains('⠸'));
        assert!(!edit.text.contains('✓'));

        footer_registry::completion_footer_record_edit_result(
            channel_id,
            edit.remove_after_edit,
            true,
        );
        assert!(!footer_registry::completion_footer_has_registered_target(
            channel_id
        ));
    }

    #[test]
    fn completion_footer_ttl_freezes_unfinished_background_bash_task() {
        let channel_id = ChannelId::new(3_089_011);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_background_task(channel_id);
        let now = 1_800_000_000;
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_111),
            &ProviderKind::Claude,
            now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS - 1,
            "Final answer",
            None,
            true,
        );

        let edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            now,
        )
        .expect("expired unfinished background Bash footer should render one freeze edit");

        assert!(edit.remove_after_edit);
        assert!(
            edit.text
                .contains("-# Tasks\n-# └ Bash Run background codex")
        );
        assert!(edit.text.contains('…'));
        assert!(!edit.text.contains('⠸'));
        assert!(!edit.text.contains('✓'));

        footer_registry::completion_footer_record_edit_result(
            channel_id,
            edit.remove_after_edit,
            true,
        );
        assert!(!footer_registry::completion_footer_has_registered_target(
            channel_id
        ));
    }

    #[test]
    fn completion_footer_below_ttl_keeps_animating_registered_target() {
        let channel_id = ChannelId::new(3_089_002);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let now = 1_800_000_000;
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_102),
            &ProviderKind::Claude,
            now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS + 1,
            "Final answer",
            None,
            true,
        );

        let edit = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            now,
        )
        .expect("non-expired unfinished footer should render an animated edit");

        assert!(!edit.remove_after_edit);
        assert!(edit.text.contains("-# Subagents\n-# └"));
        assert!(edit.text.contains('⠸'));

        footer_registry::completion_footer_record_edit_result(
            channel_id,
            edit.remove_after_edit,
            true,
        );
        assert!(footer_registry::completion_footer_has_registered_target(
            channel_id
        ));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn completion_footer_skips_refresh_when_committed_text_is_unchanged_3717() {
        let channel_id = ChannelId::new(3_717_001);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let now = 1_800_000_000;
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_717_101),
            &ProviderKind::Claude,
            now,
            "Final answer",
            None,
            true,
        );

        let first = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            now + 1,
        )
        .expect("first refresh should edit the footer");
        assert!(first.text.contains('⠸'));
        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &first,
            true,
        );

        assert_eq!(
            footer_registry::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                now + 2,
            ),
            None,
            "same rendered footer text must not issue a redundant Discord edit"
        );

        let next = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠼",
            now + 3,
        )
        .expect("changed spinner frame still edits the stable target");
        assert_eq!(next.message_id, MessageId::new(3_717_101));
        assert!(next.text.contains('⠼'));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn duplicate_same_message_registration_preserves_committed_noop_state_3717() {
        let channel_id = ChannelId::new(3_717_002);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let msg_id = MessageId::new(3_717_102);
        let now = 1_800_000_000;
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            msg_id,
            &ProviderKind::Claude,
            now,
            "Final answer",
            None,
            true,
        );
        let first = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            now + 1,
        )
        .expect("first refresh should edit the footer");
        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &first,
            true,
        );

        assert_eq!(
            footer_registry::register_completion_footer_target(
                channel_id,
                msg_id,
                &ProviderKind::Claude,
                now + 2,
                "Final answer",
                first.completion_block.as_deref(),
                true,
            ),
            None,
            "same message id is a duplicate registration, not a supersede"
        );
        assert_eq!(
            footer_registry::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                now + 3,
            ),
            None,
            "duplicate registration must not lose the committed-text dedupe state"
        );

        let changed = footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠼",
            now + 4,
        )
        .expect("changed content still edits the original stable target");
        assert_eq!(changed.message_id, msg_id);
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn stale_owner_cannot_refresh_newer_footer_target_3717() {
        let channel_id = ChannelId::new(3_717_003);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let old_owner = footer_registry::CompletionFooterOwner::new(10, 1_800_000_000);
        let new_owner = footer_registry::CompletionFooterOwner::new(11, 1_800_000_010);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_103),
            new_owner,
            &ProviderKind::Claude,
            new_owner.started_at_unix,
            "New answer",
            None,
            true,
        );

        assert_eq!(
            footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
                shared.as_ref(),
                channel_id,
                Some(old_owner),
                "⠸",
                1_800_000_011,
            ),
            None,
            "older owner must not edit the newer registered footer target"
        );

        let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(new_owner),
            "⠸",
            1_800_000_012,
        )
        .expect("registered owner should refresh its target");
        assert_eq!(edit.message_id, MessageId::new(3_717_103));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn stale_owner_cannot_register_over_newer_footer_target_3717() {
        let channel_id = ChannelId::new(3_717_007);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let old_owner = footer_registry::CompletionFooterOwner::new(50, 1_800_000_000);
        let new_owner = footer_registry::CompletionFooterOwner::new(51, 1_800_000_010);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_108),
            new_owner,
            &ProviderKind::Claude,
            new_owner.started_at_unix,
            "New answer",
            None,
            true,
        );

        assert_eq!(
            footer_registry::register_completion_footer_target_for_owner(
                channel_id,
                MessageId::new(3_717_109),
                old_owner,
                &ProviderKind::Claude,
                old_owner.started_at_unix,
                "Old answer",
                None,
                true,
            ),
            None,
            "older completion must not replace the newer registered footer target"
        );

        let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(new_owner),
            "⠸",
            1_800_000_011,
        )
        .expect("newer target should remain registered after stale register attempt");
        assert_eq!(edit.message_id, MessageId::new(3_717_108));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn stale_edit_snapshot_is_not_current_after_target_changes_3717() {
        let channel_id = ChannelId::new(3_717_009);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let old_owner = footer_registry::CompletionFooterOwner::new(70, 1_800_000_000);
        let new_owner = footer_registry::CompletionFooterOwner::new(71, 1_800_000_010);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_112),
            old_owner,
            &ProviderKind::Claude,
            old_owner.started_at_unix,
            "Old answer",
            None,
            true,
        );
        let old_edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(old_owner),
            "⠸",
            1_800_000_001,
        )
        .expect("old owner should initially receive an edit");
        assert!(footer_registry::completion_footer_edit_still_registered(
            channel_id, &old_edit
        ));

        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_113),
            new_owner,
            &ProviderKind::Claude,
            new_owner.started_at_unix,
            "New answer",
            None,
            true,
        );

        assert!(
            !footer_registry::completion_footer_edit_still_registered(channel_id, &old_edit),
            "refresh callers must skip an edit snapshot after another target registers"
        );
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn stale_owner_cannot_clear_newer_footer_target_3717() {
        let channel_id = ChannelId::new(3_717_008);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let old_owner = footer_registry::CompletionFooterOwner::new(60, 1_800_000_000);
        let new_owner = footer_registry::CompletionFooterOwner::new(61, 1_800_000_010);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_110),
            new_owner,
            &ProviderKind::Claude,
            new_owner.started_at_unix,
            "New answer",
            None,
            true,
        );

        assert_eq!(
            footer_registry::register_completion_footer_target_for_owner(
                channel_id,
                MessageId::new(3_717_111),
                old_owner,
                &ProviderKind::Claude,
                old_owner.started_at_unix,
                "Old answer",
                None,
                false,
            ),
            None,
            "older no-residual completion must not clear the newer footer target"
        );

        let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(new_owner),
            "⠸",
            1_800_000_011,
        )
        .expect("newer target should remain registered after stale clear attempt");
        assert_eq!(edit.message_id, MessageId::new(3_717_110));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn stale_edit_result_cannot_remove_newer_footer_target_3717() {
        let channel_id = ChannelId::new(3_717_004);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let old_owner = footer_registry::CompletionFooterOwner::new(20, 1_800_000_000);
        let new_owner = footer_registry::CompletionFooterOwner::new(21, 1_800_000_010);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_104),
            old_owner,
            &ProviderKind::Claude,
            old_owner.started_at_unix,
            "Old answer",
            None,
            true,
        );
        let old_edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(old_owner),
            "⠸",
            1_800_000_001,
        )
        .expect("old owner should initially receive an edit");
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_105),
            new_owner,
            &ProviderKind::Claude,
            new_owner.started_at_unix,
            "New answer",
            None,
            true,
        );

        let mut stale_remove = old_edit.clone();
        stale_remove.remove_after_edit = true;
        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &stale_remove,
            true,
        );

        let new_edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(new_owner),
            "⠼",
            1_800_000_011,
        )
        .expect("stale edit result must not remove the newer target");
        assert_eq!(new_edit.message_id, MessageId::new(3_717_105));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn older_turn_start_cannot_supersede_newer_footer_target_3717() {
        let channel_id = ChannelId::new(3_717_005);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let old_owner = footer_registry::CompletionFooterOwner::new(30, 1_800_000_000);
        let new_owner = footer_registry::CompletionFooterOwner::new(31, 1_800_000_010);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_106),
            new_owner,
            &ProviderKind::Claude,
            new_owner.started_at_unix,
            "New answer",
            None,
            true,
        );

        assert_eq!(
            footer_registry::completion_footer_supersede_registered_target_for_owner(
                channel_id,
                Some(old_owner)
            ),
            None,
            "older turn-start cleanup must not supersede the newer footer target"
        );
        assert!(
            footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
                shared.as_ref(),
                channel_id,
                Some(new_owner),
                "⠸",
                1_800_000_011,
            )
            .is_some(),
            "newer target should remain registered after stale supersede attempt"
        );
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn same_second_lower_snowflake_cannot_supersede_higher_footer_owner_3717() {
        let channel_id = ChannelId::new(3_717_006);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let old_owner = footer_registry::CompletionFooterOwner::new(40, 1_800_000_000);
        let new_owner = footer_registry::CompletionFooterOwner::new(41, 1_800_000_000);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_107),
            new_owner,
            &ProviderKind::Claude,
            new_owner.started_at_unix,
            "New answer",
            None,
            true,
        );

        assert_eq!(
            footer_registry::completion_footer_supersede_registered_target_for_owner(
                channel_id,
                Some(old_owner)
            ),
            None,
            "same-second stale owner with lower Discord snowflake must not supersede"
        );
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn same_second_unknown_owner_does_not_block_known_owner_3717() {
        let channel_id = ChannelId::new(3_717_010);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let unknown_owner = footer_registry::CompletionFooterOwner::new(0, 1_800_000_000);
        let known_owner = footer_registry::CompletionFooterOwner::new(81, 1_800_000_000);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_114),
            unknown_owner,
            &ProviderKind::Claude,
            unknown_owner.started_at_unix,
            "Unknown owner answer",
            None,
            true,
        );

        assert!(
            footer_registry::register_completion_footer_target_for_owner(
                channel_id,
                MessageId::new(3_717_115),
                known_owner,
                &ProviderKind::Claude,
                known_owner.started_at_unix,
                "Known owner answer",
                None,
                true,
            )
            .is_some(),
            "same-second user_msg_id=0 owner must not be treated as provably newer"
        );
        let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(known_owner),
            "⠸",
            1_800_000_001,
        )
        .expect("known owner should replace same-second unknown target");
        assert_eq!(edit.message_id, MessageId::new(3_717_115));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn same_second_unknown_owner_cannot_register_over_known_owner_3717() {
        let channel_id = ChannelId::new(3_717_011);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let known_owner = footer_registry::CompletionFooterOwner::new(81, 1_800_000_000);
        let unknown_owner = footer_registry::CompletionFooterOwner::new(0, 1_800_000_000);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_116),
            known_owner,
            &ProviderKind::Claude,
            known_owner.started_at_unix,
            "Known owner answer",
            None,
            true,
        );

        assert_eq!(
            footer_registry::register_completion_footer_target_for_owner(
                channel_id,
                MessageId::new(3_717_117),
                unknown_owner,
                &ProviderKind::Claude,
                unknown_owner.started_at_unix,
                "Unknown owner answer",
                None,
                true,
            ),
            None,
            "same-second user_msg_id=0 owner must not replace a known owner"
        );
        let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(known_owner),
            "⠸",
            1_800_000_001,
        )
        .expect("known owner should remain registered");
        assert_eq!(edit.message_id, MessageId::new(3_717_116));
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn same_second_unknown_owner_cannot_supersede_known_owner_3717() {
        let channel_id = ChannelId::new(3_717_012);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let known_owner = footer_registry::CompletionFooterOwner::new(82, 1_800_000_000);
        let unknown_owner = footer_registry::CompletionFooterOwner::new(0, 1_800_000_000);
        let _ = footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_118),
            known_owner,
            &ProviderKind::Claude,
            known_owner.started_at_unix,
            "Known owner answer",
            None,
            true,
        );

        assert_eq!(
            footer_registry::completion_footer_supersede_registered_target_for_owner(
                channel_id,
                Some(unknown_owner)
            ),
            None,
            "same-second user_msg_id=0 owner must not supersede a known owner"
        );
        assert!(
            footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
                shared.as_ref(),
                channel_id,
                Some(known_owner),
                "⠸",
                1_800_000_001,
            )
            .is_some(),
            "known owner should remain registered after unknown supersede attempt"
        );
        footer_registry::completion_footer_forget_registered_target(channel_id);
    }

    #[test]
    fn completion_footer_consecutive_edit_failures_evict_registered_target() {
        let channel_id = ChannelId::new(3_089_003);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_103),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        for expected_failures in 1..super::COMPLETION_FOOTER_MAX_CONSECUTIVE_EDIT_FAILURES {
            footer_registry::completion_footer_record_edit_result(channel_id, false, false);
            assert_eq!(
                footer_registry::completion_footer_registered_failure_count(channel_id),
                Some(expected_failures)
            );
            assert!(footer_registry::completion_footer_has_registered_target(
                channel_id
            ));
        }

        footer_registry::completion_footer_record_edit_result(channel_id, false, false);
        assert!(!footer_registry::completion_footer_has_registered_target(
            channel_id
        ));
        assert_eq!(
            footer_registry::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            ),
            None
        );
    }

    #[test]
    fn completion_footer_forget_registered_target_suppresses_future_edits() {
        let channel_id = ChannelId::new(3_089_004);
        footer_registry::completion_footer_forget_registered_target(channel_id);
        let shared = push_unfinished_subagent(channel_id);
        let _ = footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_104),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        footer_registry::completion_footer_forget_registered_target(channel_id);

        assert_eq!(
            footer_registry::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            ),
            None
        );
    }

    #[test]
    fn footer_status_block_stays_within_discord_limit() {
        let huge_panel = format!(
            "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n{}",
            "└ reviewer ".repeat(1_000)
        );
        let status_block = super::compose_footer_status_block("⠸", &huge_panel);
        let rendered =
            super::super::formatting::build_streaming_placeholder_text("body", &status_block);

        assert!(rendered.len() <= DISCORD_MSG_LIMIT);
        assert!(rendered.contains("\n\n"));
    }

    // ---- codex round-1 High-b: tail-anchored reclaim detection ----

    #[test]
    fn reclaim_tail_footer_matches_when_footer_is_last_section() {
        // A genuine frozen panel: prose body then a Tasks footer with a spinner
        // as the message's final section. Must reclaim, body preserved verbatim.
        let body = "여기 실제 응답 본문입니다.\n\n두 번째 문단도 보존되어야 합니다.";
        let frozen = format!("{body}\n\nTasks\n└ Bash 백그라운드 실행 ⠧");
        assert!(super::text_has_frozen_spinner_footer(&frozen));
        let out = super::reclaim_finalize_text(&frozen, &ProviderKind::Claude)
            .expect("real tail footer must reclaim");
        assert!(out.starts_with(body), "body must be preserved verbatim");
        assert!(out.contains(super::STARTUP_RECLAIM_STOP_MARKER));
        assert!(!out.contains('⠧'), "spinner must be gone");
    }

    #[test]
    fn reclaim_tail_footer_ignores_mid_body_footer_quote() {
        // This channel posts bot messages that QUOTE a footer/Tasks example in
        // the middle of ordinary prose. The quote is followed by real prose, so
        // the trailing run never reaches it: the message must NOT be reclaimed
        // and the body must never be truncated.
        let quoted = "여기 패널 예시를 인용합니다:\n\nTasks\n└ Bash 실행 중 ⠧\n\n그리고 이것은 인용 뒤에 오는 실제 본문 산문입니다.";
        assert!(
            !super::text_has_frozen_spinner_footer(quoted),
            "mid-body footer quote must not be detected as a frozen tail footer"
        );
        assert_eq!(
            super::reclaim_finalize_text(quoted, &ProviderKind::Claude),
            None,
            "mid-body footer quote must never be stripped (no body truncation)"
        );
    }

    #[test]
    fn reclaim_tail_footer_ignores_mid_body_streaming_status_quote() {
        // Same risk for a streaming status line quoted mid-body.
        let quoted =
            "상태 라인 예시:\n\n⠧ 진행 중 — claude (<t:1700000000:R>)\n\n뒤따르는 실제 본문 산문.";
        assert!(!super::text_has_frozen_spinner_footer(quoted));
        assert_eq!(
            super::reclaim_finalize_text(quoted, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn reclaim_tail_footer_ignores_completed_tail_footer() {
        // A completed tail footer (terminal ✓ marks, no live spinner) must not
        // be reclaimed even though it IS the message's last section.
        let done = "완료된 응답 본문.\n\nTasks\n└ Bash 백그라운드 실행 ✓";
        assert!(!super::text_has_frozen_spinner_footer(done));
        assert_eq!(
            super::reclaim_finalize_text(done, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn reclaim_tail_footer_matches_clamped_frozen_tail_with_truncation_marker() {
        // codex round-2 Medium: when the frozen footer overran the 600-byte
        // budget, `clamp_completion_task_section` keeps a leading prefix and
        // appends a bare `…` truncation-marker line. The frozen spinner still
        // rides a KEPT slot line, but the marker is the message's last line. The
        // tail walk must pass the marker through and reclaim the clamped tail
        // (previously it bailed on the marker and returned None), stripping BOTH
        // the footer and the marker while preserving the body verbatim.
        let body = "여기 실제 응답 본문입니다.\n\n두 번째 문단도 보존되어야 합니다.";
        // Exact clamp shape: kept prefix (header + frozen slot) then `…` line,
        // mirroring `format!("{prefix}\n{TRUNCATION_MARKER}")`.
        let clamped_footer = "Tasks\n└ Bash 백그라운드 실행 … ⠧\n…";
        let frozen = format!("{body}\n\n{clamped_footer}");
        assert!(
            super::text_has_frozen_spinner_footer(&frozen),
            "clamped frozen tail (footer prefix + `…` marker) must be detected"
        );
        let out = super::reclaim_finalize_text(&frozen, &ProviderKind::Claude)
            .expect("clamped frozen tail must reclaim");
        assert!(out.starts_with(body), "body must be preserved verbatim");
        assert!(out.contains(super::STARTUP_RECLAIM_STOP_MARKER));
        assert!(!out.contains('⠧'), "frozen spinner must be gone");
        assert!(
            !out.contains('…'),
            "trailing truncation marker must be stripped with the footer"
        );
    }

    #[test]
    fn reclaim_tail_footer_matches_frozen_panel_when_body_ends_with_bare_marker() {
        // codex round-3 Medium: the body's own last line is a standalone `…` just
        // above the footer (NOT a clamp marker — the clamp marker only ever sits
        // BELOW the footer). The two-phase walk must still reclaim the real frozen
        // footer below; the body `…` stays in the preserved body. Previously the
        // marker passed through transparently, made `…` the run's first line, and
        // `opens_footer` failed → the frozen panel was missed.
        let body = "본문이 여기서 끝납니다.\n…";
        let frozen = format!("{body}\n\nTasks\n└ Bash 실행 중 ⠧");
        assert!(
            super::text_has_frozen_spinner_footer(&frozen),
            "frozen footer must be detected even when the body ends with a bare `…`"
        );
        let out = super::reclaim_finalize_text(&frozen, &ProviderKind::Claude)
            .expect("frozen footer below a body `…` must reclaim");
        assert!(
            out.starts_with(body),
            "body (including its `…` line) preserved"
        );
        assert!(out.contains('…'), "the body's own `…` line must survive");
        assert!(!out.contains('⠧'), "frozen spinner must be gone");
        assert!(out.contains(super::STARTUP_RECLAIM_STOP_MARKER));
    }

    #[test]
    fn reclaim_tail_footer_matches_clamped_streaming_status_with_marker() {
        // The byte clamp `clamp_footer_panel_text` appends the same `…` marker.
        // A frozen merged/streaming status line (leading-spinner shape) followed
        // by the marker must also reclaim.
        let body = "스트리밍 본문 텍스트.";
        let frozen = format!("{body}\n\n⠧ 진행 중 — claude (<t:1700000000:R>)\n…");
        assert!(super::text_has_frozen_spinner_footer(&frozen));
        let out = super::reclaim_finalize_text(&frozen, &ProviderKind::Claude)
            .expect("clamped frozen streaming tail must reclaim");
        assert!(out.starts_with(body), "body preserved verbatim");
        assert!(!out.contains('⠧'));
        assert!(!out.contains('…'), "marker stripped with footer");
    }

    #[test]
    fn reclaim_tail_footer_ignores_bare_truncation_marker_without_footer() {
        // A bare `…` tail with ordinary prose above it (no footer run) must NOT
        // be reclaimed: the marker passes the walk but the run's first line is
        // prose, which fails `opens_footer`. The body is never touched.
        let prose = "그냥 평범한 본문 산문입니다. 패널 푸터가 없습니다.\n…";
        assert!(
            !super::text_has_frozen_spinner_footer(prose),
            "a lone `…` after prose must not anchor a footer"
        );
        assert_eq!(
            super::reclaim_finalize_text(prose, &ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn reclaim_tail_footer_ignores_mid_body_quote_with_trailing_marker() {
        // codex round-2 Medium High-b re-confirmation: a footer QUOTED mid-body
        // (followed by real prose) must STILL not be reclaimed even though a `…`
        // marker now passes the walk transparently. The trailing prose breaks the
        // walk before it ever reaches the quoted footer, so the marker allowance
        // cannot revive a mid-body quote match.
        let quoted = "여기 패널 예시를 인용합니다:\n\nTasks\n└ Bash 실행 중 ⠧\n…\n\n그리고 이것은 인용 뒤에 오는 실제 본문 산문입니다.";
        assert!(
            !super::text_has_frozen_spinner_footer(quoted),
            "mid-body footer quote (with a marker) must not be a frozen tail footer"
        );
        assert_eq!(
            super::reclaim_finalize_text(quoted, &ProviderKind::Claude),
            None,
            "mid-body footer quote must never be stripped"
        );
    }
}
