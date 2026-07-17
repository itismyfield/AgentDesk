use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::services::tmux_diagnostics::clear_tmux_exit_reason;

const CLAUDE_TUI_READY_SCAN_LINES: usize = 12;
const CLAUDE_TUI_ACTIVE_SCAN_LINES: usize = 24;
const CLAUDE_TUI_DRAFT_SCAN_LINES: usize = 36;
/// Recent non-empty pane lines scanned for the MCP-authentication-required
/// cold-boot banner. The warning renders just above the composer on a fresh
/// boot, so a modest tail window captures it reliably while keeping older
/// scrollback that merely mentions "authentication" from false-positiving.
const CLAUDE_TUI_MCP_AUTH_SCAN_LINES: usize = 16;
const CLAUDE_TUI_READY_BANNER: &str = "Ready for input (type message + Enter)";
const CLAUDE_TUI_PROMPT_MARKER: &str = "\u{276f}";

fn trim_prompt_line(line: &str) -> &str {
    line.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}')
}

pub(crate) fn tmux_line_is_claude_tui_ready_prompt(line: &str) -> bool {
    trim_prompt_line(line) == CLAUDE_TUI_PROMPT_MARKER
}

/// Conservatively identify the currently editable, completely empty Claude
/// composer. Unlike the general readiness predicate this remains true while a
/// prior turn is busy: busy-turn steering needs the empty bottom composer, not
/// an idle transcript. The bottom-most prompt marker must be exactly `❯`; a
/// historical prompt, ghost text, or editable draft never qualifies.
pub(crate) fn tmux_capture_indicates_claude_tui_exact_empty_composer(capture: &str) -> bool {
    capture
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(CLAUDE_TUI_DRAFT_SCAN_LINES)
        .find(|line| trim_prompt_line(line).starts_with(CLAUDE_TUI_PROMPT_MARKER))
        .is_some_and(|line| tmux_line_is_claude_tui_ready_prompt(line))
}

/// Strict modal veto for machine steering. Normal busy chrome (including
/// `esc to interrupt` and the persistent `bypass permissions` footer) is not a
/// modal. Permission approval cards, startup/resume selectors, and generic
/// confirmation selectors are rejected before any machine key is sent.
pub(crate) fn tmux_capture_indicates_claude_tui_interactive_modal(capture: &str) -> bool {
    if tmux_capture_indicates_claude_tui_selector_open(capture) {
        return true;
    }
    let recent = capture
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(CLAUDE_TUI_DRAFT_SCAN_LINES)
        .map(trim_prompt_line)
        .collect::<Vec<_>>();
    let lower = recent
        .iter()
        .map(|line| line.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let has_confirm_footer = lower.iter().any(|line| {
        line.contains("enter to confirm")
            || line.contains("esc to cancel")
            || (line.contains("enter") && line.contains("select"))
    });
    let has_permission_choices = lower.iter().any(|line| line.contains("allow"))
        && lower
            .iter()
            .any(|line| line.contains("deny") || line.contains("reject"));
    has_confirm_footer || has_permission_choices
}

fn tmux_line_is_claude_tui_prompt_draft(line: &str) -> bool {
    let Some(rest) = trim_prompt_line(line).strip_prefix(CLAUDE_TUI_PROMPT_MARKER) else {
        return false;
    };
    let rest = rest.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    // AgentDesk injects submitted Discord turns as lines like
    // `❯ [User: name (ID: ...)] ...`. Those are pane history, not an active
    // composer draft, so do not block the transcript-idle readiness fallback.
    let discord_submitted_prompt = rest
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("[User:"));
    !rest.is_empty() && !discord_submitted_prompt
}

fn tmux_lines_after_claude_prompt_show_completed_history(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let line = trim_prompt_line(line);
        let nonzero_tool_summary =
            line.contains("Tools:") && line.contains(" done") && !line.contains("Tools: 0 done");
        line.starts_with('⏺')
            || line.starts_with("✻ ")
            || line.contains("Baked for")
            || line.contains("Brewed for")
            || line.contains("Crunched for")
            || line.contains("Cogitated for")
            || nonzero_tool_summary
    })
}

/// #3924: genuine ASSISTANT RESPONSE output after a prompt line — the
/// `⏺`/`✻ <verb>` response/thinking markers, but NOT the `Tools: N done` footer.
///
/// This is `..._show_completed_history` minus its `nonzero_tool_summary` clause.
/// A STRANDED follow-up draft renders the PREVIOUS (finished) turn's idle footer
/// directly below it — including that turn's `Tools: N done` — so the broad
/// completed-history check (which counts `Tools: N>0 done`) would treat the idle
/// footer as "this prompt produced output" and hide the stranded draft. Keying
/// stranded-detection on actual response glyphs avoids that: a dropped-Enter
/// draft has none below it; a genuinely-submitted prompt does.
fn tmux_lines_after_claude_prompt_show_response_output(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let line = trim_prompt_line(line);
        line.starts_with('⏺')
            || line.starts_with("✻ ")
            || line.contains("Baked for")
            || line.contains("Brewed for")
            || line.contains("Crunched for")
            || line.contains("Cogitated for")
    })
}

fn tmux_lines_after_claude_prompt_show_idle_suggestion_chrome(lines: &[&str]) -> bool {
    // POST-FINISH idle ghost chrome ONLY (see
    // `tmux_capture_indicates_claude_tui_actively_streaming`). A `Tools: 0 done`
    // footer is deliberately NOT treated as busy here: a turn that finished having
    // run zero tools also prints it, and suppressing that broke idle/draft
    // detection for 0-tool turns (#3524). The freshly-submitted-vs-idle guard
    // (a just-submitted prompt must not read as READY) lives in the
    // `ready_for_input` caller via `..._show_freshly_submitted_footer` (#3463).
    let busy = lines.iter().any(|line| {
        let trimmed = trim_prompt_line(line);
        let lower = trimmed.to_ascii_lowercase();
        line_has_claude_tui_interrupt_chrome(trimmed)
            || lower.contains("processing")
            || lower.contains("thinking")
            || lower.contains("running")
    });
    if busy {
        return false;
    }
    let separator = lines.iter().any(|line| {
        trim_prompt_line(line)
            .chars()
            .filter(|ch| *ch == '─')
            .count()
            >= 8
    });
    let idle_footer = lines.iter().any(|line| {
        let line = trim_prompt_line(line);
        // `Tools: 0 done` means a turn has just started (no tools run yet) — a
        // running, not idle, signal — so it must NOT count as idle chrome (it
        // previously let a freshly-submitted running prompt read as ready, #3051).
        // A completed-work footer (`Tools: N>0 done`) or the permission-mode
        // banner are the genuine idle markers; mirrors the `!Tools: 0 done` guard
        // in `..._show_completed_history`.
        line.contains("bypass permissions")
            || (line.contains("Tools:")
                && line.contains(" done")
                && !line.contains("Tools: 0 done"))
    });
    separator && idle_footer
}

/// #3463/#3524: a just-submitted prompt's footer shows `Tools: 0 done` (no tools
/// run yet) while output has not begun. For READINESS this is a RUNNING signal —
/// a follow-up must not inject into it — but it is NOT a post-finish idle signal
/// (a turn that finished having run zero tools also prints `Tools: 0 done`), so
/// this guard lives only in the `ready_for_input` caller, never in the shared
/// idle-suggestion chrome detector (which by design reports post-finish ghost).
fn tmux_lines_after_claude_prompt_show_freshly_submitted_footer(lines: &[&str]) -> bool {
    lines
        .iter()
        .any(|line| trim_prompt_line(line).contains("Tools: 0 done"))
}

pub(crate) fn tmux_capture_indicates_claude_tui_ready_for_input(capture: &str) -> bool {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_ACTIVE_SCAN_LINES);
    let recent_forward = &non_empty[start..];
    let recent = recent_forward.iter().rev().copied().collect::<Vec<_>>();

    if recent.iter().any(|l| l.contains(CLAUDE_TUI_READY_BANNER)) {
        return true;
    }
    // `recent` is reverse-ordered for bottom-up prompt lookup below. Restore
    // screen order inside the active-work helper before evaluating wrapped
    // spinner head → interrupt-tail adjacency. This predicate is only a prompt-
    // marker producer; every production readiness acceptance also runs the full
    // forward capture through `snapshot_allows_prompt_readiness`, whose shared
    // busy classifier vetoes partial spinner frames this lightweight scan cannot
    // reconstruct by itself.
    if tmux_recent_lines_show_claude_tui_active_work(&recent) {
        return false;
    }

    if recent
        .iter()
        .take(CLAUDE_TUI_READY_SCAN_LINES)
        .any(|l| tmux_line_is_claude_tui_ready_prompt(l))
    {
        return true;
    }

    // #3463/#3524: if the BOTTOM-most prompt is a just-submitted, still-running
    // turn (footer shows `Tools: 0 done` with no produced output after it), the
    // pane is NOT ready — even when an older completed prompt sits higher in the
    // scrollback. Checked GLOBALLY on the latest prompt so the `.any` scan below
    // cannot flip readiness via an earlier historical prompt whose own
    // `after_prompt` happens to contain completed output (codex #3524). A
    // bypass-permissions banner alone would otherwise satisfy idle chrome and let
    // a follow-up inject into a turn that has not produced output (#3463).
    // Empty-composer ready panes are already returned above; a finished 0-tool
    // turn (idle suggestion) is intentionally not-ready here but is still
    // reported by `tmux_capture_indicates_claude_tui_idle_suggestion`.
    if let Some(last_prompt_idx) = recent_forward
        .iter()
        .rposition(|line| trim_prompt_line(line).starts_with(CLAUDE_TUI_PROMPT_MARKER))
    {
        let tail = &recent_forward[last_prompt_idx + 1..];
        if tmux_lines_after_claude_prompt_show_freshly_submitted_footer(tail)
            && !tmux_lines_after_claude_prompt_show_completed_history(tail)
        {
            return false;
        }
    }

    recent_forward
        .iter()
        .enumerate()
        .rev()
        .take(CLAUDE_TUI_READY_SCAN_LINES)
        .any(|(index, line)| {
            if !tmux_line_is_claude_tui_prompt_draft(line) {
                return false;
            }
            let after_prompt = &recent_forward[index + 1..];
            tmux_lines_after_claude_prompt_show_completed_history(after_prompt)
                || tmux_lines_after_claude_prompt_show_idle_suggestion_chrome(after_prompt)
        })
}

/// #3107: inflight-INDEPENDENT "the pane is in an active TUI turn" signal.
///
/// A multi-step agentic Claude TUI turn can lose its dcserver inflight mid-turn
/// (a momentary idle observation between tool calls trips the completion gate,
/// commits, and clears inflight) while the pane keeps producing assistant
/// output. Once inflight is gone every later batch is treated as ownerless and
/// suppressed (`should_skip_streaming_placeholder_without_inflight` /
/// `should_suppress_post_terminal_output_without_inflight`), so the live turn
/// goes dark even though the watcher is still alive.
///
/// This predicate gives the suppression/reclaim paths a way to tell a genuinely
/// finished turn (returned to ready-for-input, or showing idle-suggestion
/// chrome — the real post-finish ghost noise we DO want to suppress) apart from
/// a live turn that merely lost its inflight.
///
/// #3107 codex re-review (P2#1): the original definition was
/// `!ready_for_input && !idle_suggestion`, i.e. it treated the *absence* of
/// idle markers as "streaming". That false-positived on every pane that is
/// neither idle-marked nor busy: a scrolled pane, an error screen, a
/// non-Claude-TUI pane, or a generic prompt-waiting pane all read as
/// "streaming" → spurious un-suppress + re-acquire + reclaim-block. We now
/// require a POSITIVE Claude-TUI busy signal, not merely the absence of idle
/// chrome. `true` means: the pane IS a Claude TUI showing an active/busy
/// indicator AND is not ready-for-input ⇒ a live turn that lost its inflight.
/// Anything ambiguous (blank / error / scrolled / non-Claude / generic prompt)
/// biases to `false` (keep suppressing) — the safe direction.
pub(crate) fn tmux_capture_indicates_claude_tui_actively_streaming(capture: &str) -> bool {
    if capture.trim().is_empty() {
        return false;
    }
    if tmux_capture_indicates_claude_tui_ready_for_input(capture) {
        return false;
    }
    if tmux_capture_indicates_claude_tui_idle_suggestion(capture) {
        return false;
    }
    // Positive busy signal required (bias to FALSE/suppress when ambiguous).
    tmux_capture_indicates_claude_tui_busy(capture)
}

/// #3107 codex re-review (P2#1, F2): a POSITIVE "Claude TUI is mid-response
/// right now" signal that requires Claude-TUI-SPECIFIC CHROME, not generic
/// words. The previous implementation accepted any recent line containing the
/// bare substrings `processing` / `thinking` / `running`. Those words routinely
/// appear in ASSISTANT BODY TEXT (e.g. the model writing "the test is
/// running…") and in non-Claude program output, so a finished or even
/// non-Claude pane could read as "actively streaming" → wrongly un-suppress /
/// re-acquire / block reclaim.
///
/// The reliable in-progress markers the Claude TUI actually RENDERS are:
///   1. the `esc to interrupt` footer — the strongest, unambiguous signal; it
///      only renders while a turn is in flight; and
///   2. structured spinner chrome — a leading spinner glyph, a compact status
///      phrase, and an ellipsis, optionally followed by duration/token/interrupt
///      chrome. This admits multi-word and earliest truncated frames without a
///      fixed verb allowlist while excluding ordinary prose and balanced fenced
///      examples.
/// Plus the explicit `⏺ Running command / Searching for / Reading / Editing …`
/// active-work markers via `tmux_recent_lines_show_claude_tui_active_work`.
///
/// Bare `processing`/`thinking`/`running` NOT anchored to spinner structure or
/// the `esc to interrupt` footer are DROPPED. Anything that is not a
/// recognizable Claude-TUI in-progress frame biases to FALSE.
pub(crate) fn tmux_capture_indicates_claude_tui_busy(capture: &str) -> bool {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    if non_empty.is_empty() {
        return false;
    }
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_ACTIVE_SCAN_LINES);
    let recent = &non_empty[start..];
    tmux_recent_lines_show_claude_tui_interrupt_chrome(recent)
        || tmux_capture_indicates_claude_tui_structured_spinner(capture)
        || tmux_recent_forward_lines_show_claude_tui_active_work(recent)
}

/// A conservative live-turn signal for Claude TUI spinner chrome. The status
/// line must have a spinner glyph, a compact status phrase, and an ellipsis.
/// Parenthesized duration/interrupt chrome is a strong positive signal, while an
/// early suffix-free frame must use a known work-status shape or phrase.
/// Only candidates inside a fence pair that is balanced in this capture are
/// excluded. An unmatched fence can be a closing fence whose opener scrolled
/// away, so it must not hide later live status chrome.
pub(crate) fn tmux_capture_indicates_claude_tui_structured_spinner(capture: &str) -> bool {
    let lines = capture.lines().collect::<Vec<_>>();
    let recent_start = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .rev()
        .nth(CLAUDE_TUI_ACTIVE_SCAN_LINES.saturating_sub(1))
        .map(|(index, _)| index)
        .unwrap_or(0);
    let balanced_fences = claude_tui_balanced_markdown_fence_ranges(&lines);
    lines.iter().enumerate().any(|(index, line)| {
        index >= recent_start
            && !balanced_fences
                .iter()
                .any(|(open, close)| index > *open && index < *close)
            && tmux_line_is_claude_tui_structured_spinner(trim_prompt_line(line))
    })
}

/// #3521: `true` when the Claude TUI pane shows a BACKGROUND AGENT still pending — the
/// `✻ Waiting for N background agent(s) to finish` footer, or a fresh `Backgrounded agent`
/// spawn line. Distinct from `tmux_capture_indicates_claude_tui_busy`: a detached background
/// agent leaves the FOREGROUND turn JSONL-idle (no `esc to interrupt`, no spinner) while it
/// keeps running, so the completion gate must treat this as not-yet-idle to keep the live
/// footer/turn alive — otherwise the turn finalizes and the panel vanishes mid-run (#3521).
/// Markers are TUI chrome (`waiting for` + `background agent`, or `backgrounded agent`),
/// anchored tightly so assistant body text that merely mentions a "background agent" (e.g.
/// the voice handoff line) does NOT trip a false keep-alive.
pub(crate) fn tmux_capture_indicates_claude_tui_background_agent_pending(capture: &str) -> bool {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    if non_empty.is_empty() {
        return false;
    }
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_ACTIVE_SCAN_LINES);
    non_empty[start..].iter().any(|line| {
        let lower = line.to_ascii_lowercase();
        (lower.contains("waiting for") && lower.contains("background agent"))
            || lower.contains("backgrounded agent")
    })
}

/// Shared producer for the Claude TUI background-agent pending bit.
///
/// Capture failures and dead sessions return `false`: treating an unknown pane
/// as not-pending keeps completion unblocked, and the footer TTL/watchdog bounds
/// the damage from a wrong false.
pub(crate) fn sniff_background_agent_pending(tmux_session_name: &str) -> bool {
    crate::services::platform::tmux::capture_pane(tmux_session_name, 0)
        .map(|pane| tmux_capture_indicates_claude_tui_background_agent_pending(&pane))
        .unwrap_or(false)
}

/// `true` when `line` is a Claude TUI spinner progress footer: a leading spinner
/// glyph (the rotating set the TUI cycles through) directly followed by a work
/// verb. Anchoring the verb to the spinner glyph is what distinguishes the TUI
/// chrome from the same verb appearing in assistant body text.
fn tmux_line_is_claude_tui_spinner_progress(line: &str) -> bool {
    let line = line.trim_start();
    let mut chars = line.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_claude_tui_spinner_glyph(first) {
        return false;
    }
    // The remainder after the glyph (and its following space) must lead with a
    // work verb the TUI uses for the streaming footer. Completed-work summaries
    // (`✻ Churned for 4m 56s`, `✻ Worked for 2s`) use a past-tense "<verb> for
    // <duration>" shape and must NOT count as in-progress.
    let rest = chars.as_str().trim_start();
    let lower = rest.to_ascii_lowercase();
    if lower.contains(" for ") && !lower.contains("esc to interrupt") {
        return false;
    }
    const WORK_VERBS: [&str; 7] = [
        "actioning",
        "musing",
        "thinking",
        "processing",
        "running",
        "crunching",
        "churning",
    ];
    if !WORK_VERBS.iter().any(|verb| lower.starts_with(verb)) {
        return false;
    }
    // #3107 codex re-review (F2): the leading glyph + work verb alone is NOT
    // enough — a plain assistant sentence that happens to begin with a spinner
    // glyph and a verb (e.g. `· Thinking through the problem and running the
    // tests`) would otherwise read as the streaming footer. The REAL Claude TUI
    // spinner line ALWAYS carries a status SUFFIX — it renders like
    // `✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)`. Require at least
    // one of those status markers so assistant prose can't trip it:
    //   - the literal `esc to interrupt`, OR
    //   - a parenthesized TUI status group containing a duration (`<N>s` /
    //     `<N>m`), a `tokens` count, and/or the `·` separator the TUI uses.
    if line_has_claude_tui_interrupt_chrome(line) {
        return true;
    }
    line_has_claude_tui_spinner_status_group(line)
}

fn is_claude_tui_spinner_glyph(glyph: char) -> bool {
    const SPINNER_GLYPHS: [char; 8] = ['·', '✢', '✳', '✶', '✻', '✽', '✦', '∗'];
    SPINNER_GLYPHS.contains(&glyph)
}

fn line_has_claude_tui_interrupt_chrome(line: &str) -> bool {
    let line = trim_prompt_line(line);
    let lower = line.to_ascii_lowercase();
    let Some(interrupt_start) = lower.find("esc to interrupt") else {
        return false;
    };
    let after_interrupt = &line[interrupt_start + "esc to interrupt".len()..];
    // Complete footer chrome ends at its closing parenthesis. Any trailing
    // non-whitespace turns the line into assistant prose quoting the footer.
    if after_interrupt.trim() != ")" {
        return false;
    }
    let before_interrupt = &line[..interrupt_start];
    let has_status_group = before_interrupt.starts_with('(')
        && !before_interrupt[1..].contains(')')
        && before_interrupt[1..].contains('·');
    let spinner_prefix = line.chars().next().is_some_and(is_claude_tui_spinner_glyph);
    has_status_group || (spinner_prefix && lower[..interrupt_start].contains('…'))
}

fn lines_reconstruct_claude_tui_interrupt_chrome(first: &str, second: &str) -> bool {
    let mut combined = String::with_capacity(first.len() + second.len());
    combined.push_str(trim_prompt_line(first));
    combined.push_str(trim_prompt_line(second));
    line_has_claude_tui_interrupt_chrome(&combined)
}

fn tmux_recent_lines_show_claude_tui_interrupt_chrome(lines: &[&str]) -> bool {
    lines.iter().enumerate().any(|(index, line)| {
        line_has_claude_tui_interrupt_chrome(line)
            || index.checked_sub(1).is_some_and(|previous_index| {
                lines_reconstruct_claude_tui_interrupt_chrome(lines[previous_index], line)
            })
    })
}

fn claude_tui_markdown_fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = trim_prompt_line(line).trim_start_matches(|character: char| {
        matches!(character, '│' | '┃' | '┆' | '┊' | '▏' | '▕')
    });
    let trimmed = trimmed.trim_start();
    let marker = trimmed.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let length = trimmed
        .chars()
        .take_while(|character| *character == marker)
        .count();
    (length >= 3).then_some((marker, length))
}

fn claude_tui_balanced_markdown_fence_ranges(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut open_fence = None;
    for (index, line) in lines.iter().enumerate() {
        let Some(marker) = claude_tui_markdown_fence_marker(line) else {
            continue;
        };
        match open_fence {
            Some((open_index, open_char, open_len))
                if marker.0 == open_char && marker.1 >= open_len =>
            {
                ranges.push((open_index, index));
                open_fence = None;
            }
            None => open_fence = Some((index, marker.0, marker.1)),
            _ => {}
        }
    }
    ranges
}

fn tmux_line_is_claude_tui_structured_spinner(line: &str) -> bool {
    let line = line.trim_start();
    let mut chars = line.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_claude_tui_spinner_glyph(first) {
        return false;
    }

    let rest = chars.as_str().trim_start();
    let (status, suffix) = if let Some(status_end) = rest.find('…') {
        (&rest[..status_end], &rest[status_end + '…'.len_utf8()..])
    } else if let Some(status_end) = rest.find("...") {
        (&rest[..status_end], &rest[status_end + 3..])
    } else {
        return false;
    };
    let status = status.trim();
    if !claude_tui_spinner_status_phrase_is_compact(status) {
        return false;
    }
    if !claude_tui_spinner_status_is_known(status) {
        return false;
    }
    let suffix = suffix.trim_start();
    if suffix.is_empty() {
        return true;
    }
    if !suffix.starts_with('(') {
        return false;
    }
    line_has_claude_tui_spinner_status_fragment(suffix)
}

fn claude_tui_spinner_status_phrase_is_compact(status: &str) -> bool {
    !status.is_empty()
        && status.chars().count() <= 48
        && status.split_whitespace().count() <= 5
        && status.chars().all(|character| {
            character.is_alphanumeric()
                || matches!(character, ' ' | '-' | '_' | '/' | ':' | '\'' | '’')
        })
}

fn claude_tui_spinner_status_is_known(status: &str) -> bool {
    const SPECIAL_STATUSES: [&str; 3] = [
        "Beboppin'",
        "Compacting conversation",
        "Mapping distant galaxies",
    ];
    if SPECIAL_STATUSES
        .iter()
        .any(|candidate| status.eq_ignore_ascii_case(candidate))
    {
        return true;
    }
    !status.chars().any(char::is_whitespace) && status.to_ascii_lowercase().ends_with("ing")
}

fn line_has_claude_tui_spinner_status_fragment(line: &str) -> bool {
    let Some(open) = line.find('(') else {
        return false;
    };
    let after_open = &line[open + 1..];
    let (group, closed) = if let Some(close) = after_open.find(')') {
        if !after_open[close + 1..].trim().is_empty() {
            return false;
        }
        (&after_open[..close], true)
    } else {
        (after_open, false)
    };
    let lower = group.to_ascii_lowercase();
    let has_status = lower.contains("esc to interrupt")
        || lower.contains("tokens")
        || group.contains('·')
        || group
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(is_claude_tui_duration_token);
    has_status && (closed || !group.trim().is_empty())
}

fn is_claude_tui_duration_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 2
        && matches!(bytes[bytes.len() - 1], b's' | b'm')
        && bytes[..bytes.len() - 1]
            .iter()
            .all(|byte| byte.is_ascii_digit())
}

/// `true` when `line` contains the parenthesized status group the Claude TUI
/// spinner footer renders next to the work verb, e.g.
/// `(12s · ↑ 1.2k tokens · esc to interrupt)`. The group must carry at least one
/// of: a duration token (`<N>s` / `<N>m`), a `tokens` count, or the interior `·`
/// separator the TUI draws between status fields. A bare parenthetical in
/// assistant prose (no such marker) does NOT qualify.
fn line_has_claude_tui_spinner_status_group(line: &str) -> bool {
    let Some(open) = line.find('(') else {
        return false;
    };
    let after_open = &line[open + 1..];
    let Some(close_rel) = after_open.find(')') else {
        return false;
    };
    if !after_open[close_rel + 1..].trim().is_empty() {
        return false;
    }
    let group = &after_open[..close_rel];
    let lower = group.to_ascii_lowercase();
    if lower.contains("esc to interrupt") || lower.contains("tokens") || group.contains('·') {
        return true;
    }
    // A standalone duration token such as `12s` / `4m` inside the group.
    group
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(is_claude_tui_duration_token)
}

pub(crate) fn tmux_capture_indicates_claude_tui_prompt_draft(capture: &str) -> bool {
    tmux_capture_claude_tui_prompt_draft_backspace_budget(capture).is_some()
}

pub(crate) fn tmux_capture_indicates_claude_tui_idle_suggestion(capture: &str) -> bool {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_DRAFT_SCAN_LINES);
    let recent = &non_empty[start..];
    recent
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, line)| {
            if !trim_prompt_line(line).starts_with(CLAUDE_TUI_PROMPT_MARKER) {
                return None;
            }
            if !tmux_line_is_claude_tui_prompt_draft(line) {
                return Some(false);
            }
            let after_prompt = &recent[index + 1..];
            if tmux_lines_after_claude_prompt_show_completed_history(after_prompt) {
                return Some(false);
            }
            Some(tmux_lines_after_claude_prompt_show_idle_suggestion_chrome(
                after_prompt,
            ))
        })
        .unwrap_or(false)
}

/// True for the Claude Code cold-boot banner reporting one or more MCP servers
/// still need authentication, e.g. `⚠ 1 MCP server needs authentication · run
/// /mcp`. The banner paints on the post-launch welcome screen (commonly when a
/// remote SSE server failed to authenticate), which still renders normal
/// composer chrome — so `..._ready_for_input` reads it as READY even though
/// Claude Code silently drops every submission until `/mcp` is run. The
/// claude_tui readiness gate uses this to refuse that false-ready and fail fast
/// with an actionable reason instead of blind-waiting/retrying (#3889).
pub(crate) fn tmux_capture_indicates_claude_tui_mcp_auth_required(capture: &str) -> bool {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    let start = non_empty
        .len()
        .saturating_sub(CLAUDE_TUI_MCP_AUTH_SCAN_LINES);
    non_empty[start..]
        .iter()
        .any(|line| line_is_mcp_auth_required_warning(line))
}

/// One pane line is the MCP-auth-needed warning only when it is Claude Code's
/// system warning banner — **anchored to the `⚠` glyph** (`⚠ N MCP server(s)
/// need authentication · run /mcp`) AND naming MCP + authentication + (`need` |
/// `run /mcp`). The `⚠` anchor is load-bearing: assistant/tool transcript output
/// (lines beginning `⏺`/`✻`, or continuation prose) can say "The MCP server needs
/// authentication; run /mcp ..." above a perfectly ready composer, so token
/// presence alone must NOT match — only the chrome glyph the system banner
/// carries does (#3889 Codex review [1]).
fn line_is_mcp_auth_required_warning(line: &str) -> bool {
    let trimmed = trim_prompt_line(line);
    // Anchor to the system warning banner glyph (U+26A0). `starts_with` matches
    // regardless of a trailing emoji variation selector (`⚠️`, U+FE0F).
    if !trimmed.starts_with('\u{26a0}') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("mcp")
        && lower.contains("authentic")
        && (lower.contains("need") || lower.contains("run /mcp"))
}

fn tmux_recent_lines_show_claude_tui_active_work(lines_reverse_ordered: &[&str]) -> bool {
    let forward = lines_reverse_ordered
        .iter()
        .rev()
        .copied()
        .collect::<Vec<_>>();
    // Prompt-marker detection intentionally consumes only decisive in-flight
    // chrome. A suffix-free early spinner can coexist briefly with an already
    // painted empty composer, so it remains marker-ready here; the final
    // readiness boundary independently applies the full structured-spinner busy
    // veto to the same capture.
    tmux_recent_lines_show_claude_tui_interrupt_chrome(&forward)
        || tmux_recent_forward_lines_show_claude_tui_active_work_without_spinner(&forward)
}

fn tmux_recent_forward_lines_show_claude_tui_active_work(lines: &[&str]) -> bool {
    lines
        .iter()
        .any(|line| tmux_line_is_claude_tui_structured_spinner(trim_prompt_line(line)))
        || tmux_recent_forward_lines_show_claude_tui_active_work_without_spinner(lines)
}

fn tmux_recent_forward_lines_show_claude_tui_active_work_without_spinner(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let line = trim_prompt_line(line);
        let lower = line.to_ascii_lowercase();
        lower.contains("current work")
            // NOTE: neither the footer context-usage bar (`🤖 Model │ ██░░ │ NN%`)
            // nor the completed-thinking summary line (`✻ Churned for 4m 56s`) is a
            // running signal — both render in IDLE/ready states too. #3051 keyed
            // active-work on the `██` run, which flipped a ready prompt with >20%
            // context usage to not-ready; the running vs. idle distinction is
            // instead carried by the footer (`Tools: 0 done` = freshly-started, no
            // tools yet) handled in `..._show_idle_suggestion_chrome`, plus the
            // explicit interrupt/status chrome above.
            || (line.starts_with('⏺')
                && ((line.contains("Running ") && line.contains("command"))
                    || line.contains("Searching for ")
                    || line.contains("Reading ")
                    || line.contains("Editing ")))
    })
}

pub(crate) fn tmux_capture_claude_tui_prompt_draft_backspace_budget(
    capture: &str,
) -> Option<usize> {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_DRAFT_SCAN_LINES);
    let recent = &non_empty[start..];
    recent
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, line)| {
            if !trim_prompt_line(line).starts_with(CLAUDE_TUI_PROMPT_MARKER) {
                return None;
            }
            let after_prompt = &recent[index + 1..];
            if !tmux_line_is_claude_tui_prompt_draft(line) {
                // A `❯ [User: …] …` line is normally submitted Discord history
                // (its Enter landed, so it is pane scrollback, not a composer
                // draft). But #3924: the SAME shape can be a STRANDED follow-up
                // whose submit Enter was DROPPED — it then sits in the composer
                // below a finished-turn block under ONLY idle-suggestion chrome.
                // Pure capture text cannot finally separate the two; recognizing
                // the stranded SHAPE here lets the recovery net's authoritative
                // JSONL transcript cross-check (Idle/Unknown vs running) decide.
                return Some(claude_tui_stranded_followup_draft_backspace_budget(
                    line,
                    after_prompt,
                ));
            }
            // Claude keeps submitted prompt lines in the pane history. If the
            // prompt line is followed by rendered assistant/completion output,
            // it is historical text, not an editable composer draft.
            if tmux_lines_after_claude_prompt_show_completed_history(after_prompt)
                || tmux_lines_after_claude_prompt_show_idle_suggestion_chrome(after_prompt)
            {
                return Some(None);
            }
            Some(claude_tui_prompt_draft_backspace_budget_from_line(line))
        })
        .unwrap_or(None)
}

/// #3924: budget to clear a STRANDED Discord follow-up draft, or `None` when the
/// `❯ [User: …] …` line is genuine submitted history rather than a dropped-Enter
/// draft.
///
/// The recovery-net false-negative this guards against: a follow-up whose submit
/// Enter was dropped leaves `❯ [User: …] <text>` editable in the composer,
/// directly below a finished previous-turn block, surrounded by idle-suggestion
/// chrome — visually identical to a post-finish idle ghost. The bare `[User:]`
/// exclusion in `tmux_line_is_claude_tui_prompt_draft` (which keeps SUBMITTED
/// history from blocking readiness) misclassifies that stranded draft as
/// no-draft, so the recovery net never fires and the turn is killed at 120s.
///
/// Capture text alone CANNOT separate a stranded draft from a freshly-submitted
/// still-running turn: a `Tools: 0 done` footer renders for BOTH a just-started
/// turn AND a FINISHED 0-tool turn, so it is not a usable running signal (#3924
/// codex re-review — keying the guard on it re-introduced the false-negative for
/// a stranded draft below a finished 0-tool turn). This is therefore a
/// CONSERVATIVE SHAPE gate only — it fires for a `[User:]` line that
/// (1) sits under idle-suggestion chrome (separator + idle footer), and
/// (2) has produced NO assistant RESPONSE output (`⏺`/`✻`) below it.
/// It deliberately does NOT decide running-vs-stranded from the footer. The
/// recovery net (`claude_tui_followup_stranded_prompt_draft_state`) makes that
/// call from the AUTHORITATIVE JSONL transcript turn-state: Idle/Unknown (no
/// in-progress turn) ⇒ stranded, recover; a running/in-progress turn ⇒ NOT
/// recovered. The submit-confirmation Enter-retry path is independently gated by
/// `..._ready_for_input` (false on any `Tools: 0 done` pane via its own
/// freshly-submitted guard), so promoting this shape to a draft cannot
/// double-submit a live turn there either.
fn claude_tui_stranded_followup_draft_backspace_budget(
    line: &str,
    after_prompt: &[&str],
) -> Option<usize> {
    let rest = trim_prompt_line(line)
        .strip_prefix(CLAUDE_TUI_PROMPT_MARKER)?
        .trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    // Only AgentDesk-injected `[User: …]` text is recoverable this way; an empty
    // composer or a non-injected suggestion ghost is handled by the normal path.
    if !rest
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("[User:"))
    {
        return None;
    }
    // A genuine stranded draft sits under ONLY idle-suggestion chrome. (That
    // chrome detector already returns false when a live busy/spinner marker —
    // `esc to interrupt`/processing/thinking/running — is present, so a visibly
    // streaming turn is excluded here without depending on the tool footer.)
    if !tmux_lines_after_claude_prompt_show_idle_suggestion_chrome(after_prompt) {
        return None;
    }
    // Assistant RESPONSE output after the line means it actually submitted AND
    // produced output (pane history), not a dropped-Enter draft. NOTE: key on
    // response glyphs (`..._show_response_output`), NOT the broad completed-
    // history check — the `Tools: N done` count in the idle footer below a
    // stranded draft belongs to the PREVIOUS finished turn, and a finished
    // 0-tool prior turn's `Tools: 0 done` must NOT hide the draft.
    if tmux_lines_after_claude_prompt_show_response_output(after_prompt) {
        return None;
    }
    // Budget covers the whole injected line (`[User: …] <text>`) plus a margin so
    // the clear erases the entire stranded draft.
    Some(rest.chars().count().saturating_add(4).min(512))
}

pub(crate) fn claude_tui_prompt_draft_backspace_budget_from_line(line: &str) -> Option<usize> {
    let rest = trim_prompt_line(line)
        .strip_prefix(CLAUDE_TUI_PROMPT_MARKER)?
        .trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    if rest.is_empty()
        || rest
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("[User:"))
    {
        return None;
    }
    Some(rest.chars().count().saturating_add(4).min(512))
}

pub(crate) fn tmux_capture_indicates_generic_ready_banner(capture: &str) -> bool {
    capture
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(CLAUDE_TUI_READY_SCAN_LINES)
        .any(|l| l.contains(CLAUDE_TUI_READY_BANNER))
}

/// Detect whether the interactive Claude TUI `/effort` slider overlay is still
/// open in the captured pane.
///
/// Claude Code 2.1.x renders `/effort` as a *horizontal slider*, not a
/// box-drawing radio list: the open overlay carries BOTH an `Effort` heading
/// and a `←/→ to adjust` (left/right arrow) instructional footer. When the
/// overlay is dismissed (Enter confirms the selection) both disappear and the
/// pane returns to the normal composer chrome.
///
/// We require BOTH signals to co-occur in the recent capture so that stale
/// scrollback — e.g. a prior conversation or code snippet that merely mentions
/// `←/→ to adjust` or the word "effort" — cannot be mistaken for a live
/// overlay. Requiring the pair is the load-bearing guard against false
/// "selector still open" failures.
///
/// This is the post-submit validation for `/effort` passthrough: if this
/// returns true after we drive the slider, the selection did NOT confirm and
/// the pane is stranded on the overlay.
pub(crate) fn tmux_capture_indicates_claude_tui_selector_open(capture: &str) -> bool {
    let non_empty = capture
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>();
    let start = non_empty.len().saturating_sub(CLAUDE_TUI_DRAFT_SCAN_LINES);
    let recent = &non_empty[start..];

    let has_footer = recent.iter().any(|line| line_is_slider_adjust_footer(line));
    let has_heading = recent
        .iter()
        .any(|line| line_is_effort_slider_heading(line));
    has_footer && has_heading
}

/// True for the slider's instructional footer, e.g. `←/→ to adjust` or
/// `← / → to adjust` (Claude renders the arrow glyphs `←`/`→` paired with the
/// word "adjust"). We accept either arrow glyph plus the "adjust" keyword so a
/// minor copy/spacing change does not silently disable the detector.
fn line_is_slider_adjust_footer(line: &str) -> bool {
    let lower = trim_prompt_line(line).to_lowercase();
    (lower.contains('←') || lower.contains('→')) && lower.contains("adjust")
}

/// True for the `/effort` slider heading line — the overlay labels the control
/// with the word "effort". Required alongside the adjust footer so a stray
/// scrollback line containing only one of the two signals is not read as a
/// live overlay.
fn line_is_effort_slider_heading(line: &str) -> bool {
    trim_prompt_line(line).to_lowercase().contains("effort")
}

/// Format a tmux session name as an exact-match target.
///
/// tmux `-t` flags perform prefix matching by default: `-t foo` matches
/// both `foo` and `foo-bar`.  Prefixing with `=` forces exact matching,
/// preventing the wrong session from being targeted when session names
/// share a common prefix (e.g. main vs thread sessions).
pub fn tmux_exact_target(session_name: &str) -> String {
    format!("={}", session_name)
}

/// Subdirectory under the runtime root where session temp files live.
const SESSIONS_SUBDIR: &str = "runtime/sessions";
pub(crate) const CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT: &str = "claude-tui-settings.json";
pub(crate) const CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT: &str = "claude-tui.sh";
pub(crate) const CODEX_TUI_HOME_TEMP_EXT: &str = "codex-tui-home";
pub(crate) const CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT: &str = "codex-tui-rollout.json";
pub(crate) const TMUX_DEAD_MARKER_TEMP_EXT: &str = "pane_dead";
pub(crate) const TMUX_RUNTIME_KIND_TEMP_EXT: &str = "runtime-kind";

/// Returns the persistent AgentDesk sessions directory, if a runtime root
/// is configured. This is the new canonical location for session temp files
/// (jsonl, input FIFO, owner markers, prompt, etc.).
///
/// Returns None when `runtime_root()` is unavailable (rare; only during
/// very early bootstrap or broken environments). Callers should fall back
/// to `std::env::temp_dir()` in that case — see `agentdesk_temp_dir()`.
pub fn persistent_sessions_dir() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| root.join(SESSIONS_SUBDIR))
}

/// Get the platform-appropriate directory for AgentDesk session runtime files.
///
/// Prefers the persistent path under `runtime_root()/runtime/sessions/` so
/// that session jsonl/FIFO/owner markers survive across dcserver restarts
/// (see issue #892). Falls back to `std::env::temp_dir()` only when a
/// runtime root is not available.
pub fn agentdesk_temp_dir() -> String {
    match persistent_sessions_dir() {
        Some(dir) => {
            // Best-effort lazy create so early callers (tests, one-off tools)
            // don't fail before the dcserver startup bootstrap runs. The
            // startup code also calls `ensure_sessions_dir_on_startup()` so
            // wrappers spawned after boot write into the right place.
            let _ = ensure_sessions_dir_inner(&dir);
            dir.display().to_string()
        }
        None => std::env::temp_dir().display().to_string(),
    }
}

fn ensure_sessions_dir_inner(dir: &PathBuf) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(dir) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 != 0o700 {
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(dir, perms);
            }
        }
    }
    Ok(())
}

/// Startup hook: create the persistent sessions directory (0o700) so that
/// wrappers spawned after dcserver boot write into the canonical location.
/// Idempotent; safe to call multiple times.
pub fn ensure_sessions_dir_on_startup() -> Result<(), String> {
    let Some(dir) = persistent_sessions_dir() else {
        return Ok(()); // nothing to do when no runtime_root
    };
    ensure_sessions_dir_inner(&dir)
        .map_err(|e| format!("Failed to create sessions dir '{}': {}", dir.display(), e))
}

fn host_temp_namespace() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn session_temp_prefix(session_name: &str) -> String {
    let host = host_temp_namespace();
    let mut hasher = Sha256::new();
    hasher.update(current_tmux_owner_marker().as_bytes());
    hasher.update(b"|");
    hasher.update(host.as_bytes());
    let digest = hasher.finalize();
    let runtime_hash = format!("{:x}", digest);
    format!(
        "agentdesk-{}-{}-{}",
        &runtime_hash[..12],
        host,
        session_name
    )
}

/// Build a path for an AgentDesk runtime temp file in the **canonical**
/// (persistent) location.
///
/// Example: `session_temp_path("mySession", "jsonl")`
///   → `~/.adk/release/runtime/sessions/agentdesk-<runtime>-<host>-mySession.jsonl`
pub fn session_temp_path(session_name: &str, extension: &str) -> String {
    format!(
        "{}/{}.{}",
        agentdesk_temp_dir(),
        session_temp_prefix(session_name),
        extension
    )
}

/// Canonical marker written by tmux pane/session hooks when a session's pane
/// exits. Watchers treat this as an explicit "tmux died" wake-up; the legacy
/// liveness probe remains as a hook-miss safety net.
pub fn session_dead_marker_path(session_name: &str) -> String {
    session_temp_path(session_name, TMUX_DEAD_MARKER_TEMP_EXT)
}

/// Build a path to the *legacy* `/tmp/`-based location for a session temp
/// file. Wrappers spawned before the migration hold open fds to these files;
/// readers must be able to still find them during the migration window.
pub fn legacy_tmp_session_path(session_name: &str, extension: &str) -> String {
    format!(
        "{}/{}.{}",
        std::env::temp_dir().display(),
        session_temp_prefix(session_name),
        extension
    )
}

/// Resolve whichever location actually holds the session temp file.
/// Prefers the new persistent path when both exist. Returns `None` when
/// neither location has the file. Used by read-side code (e.g. the
/// `session_usable` check and the watcher skip-on-missing-output file)
/// so they accept either location during the migration window.
pub fn resolve_session_temp_path(session_name: &str, extension: &str) -> Option<String> {
    let new_path = session_temp_path(session_name, extension);
    if std::path::Path::new(&new_path).exists() {
        return Some(new_path);
    }
    let legacy = legacy_tmp_session_path(session_name, extension);
    if std::path::Path::new(&legacy).exists() {
        return Some(legacy);
    }
    None
}

/// Delete all known session temp files for the given tmux session.
/// Idempotent — missing files are not errors. Hits both the new persistent
/// location and the legacy `/tmp/` location so cleanup is total regardless
/// of where the wrapper originally wrote.
pub fn cleanup_session_temp_files(session_name: &str) {
    // All extensions we ever allocate under the session prefix.
    const EXTS: &[&str] = &[
        "jsonl",
        "input",
        "prompt",
        "owner",
        "sh",
        "generation",
        // #3087: the per-spawn status-panel instance nonce. Must be swept on
        // teardown like the other session temp files — otherwise a respawn whose
        // fresh nonce write fails (logged, non-fatal) would leave the PRIOR
        // spawn's nonce readable, yielding the same instance key as the old
        // spawn and suppressing the panel reset on a genuinely new session.
        // (Mirrors `SPAWN_NONCE_SUFFIX` in discord::tmux_session_files.)
        "spawn_nonce",
        "exit_reason",
        TMUX_RUNTIME_KIND_TEMP_EXT,
        TMUX_DEAD_MARKER_TEMP_EXT,
        CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT,
        CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    ];
    for ext in EXTS {
        let _ = std::fs::remove_file(session_temp_path(session_name, ext));
        let _ = std::fs::remove_file(legacy_tmp_session_path(session_name, ext));
    }
    let _ = std::fs::remove_dir_all(session_temp_path(session_name, CODEX_TUI_HOME_TEMP_EXT));
    let _ = std::fs::remove_dir_all(legacy_tmp_session_path(
        session_name,
        CODEX_TUI_HOME_TEMP_EXT,
    ));
}

/// Get the current AgentDesk runtime root marker for tmux session ownership.
pub fn current_tmux_owner_marker() -> String {
    crate::config::runtime_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".adk/release".to_string())
}

/// Path to the owner marker file for a tmux session.
pub fn tmux_owner_path(tmux_session_name: &str) -> String {
    session_temp_path(tmux_session_name, "owner")
}

/// Write the owner marker file so this runtime claims the tmux session.
pub fn write_tmux_owner_marker(tmux_session_name: &str) -> Result<(), String> {
    clear_tmux_exit_reason(tmux_session_name);
    let owner_path = tmux_owner_path(tmux_session_name);
    std::fs::write(&owner_path, current_tmux_owner_marker())
        .map_err(|e| format!("Failed to write tmux owner marker: {}", e))
}

pub(crate) fn write_tmux_runtime_kind_marker(
    tmux_session_name: &str,
    runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind,
) -> Result<(), String> {
    let path = session_temp_path(tmux_session_name, TMUX_RUNTIME_KIND_TEMP_EXT);
    std::fs::write(&path, runtime_kind.as_str())
        .map_err(|e| format!("Failed to write tmux runtime kind marker: {}", e))
}

pub(crate) fn resolve_tmux_runtime_kind_marker(
    tmux_session_name: &str,
) -> Option<crate::services::agent_protocol::RuntimeHandoffKind> {
    let path = resolve_session_temp_path(tmux_session_name, TMUX_RUNTIME_KIND_TEMP_EXT)?;
    let raw = std::fs::read_to_string(path).ok()?;
    crate::services::agent_protocol::RuntimeHandoffKind::from_str(&raw)
}

/// Append-only JSONL writer that reopens the path when external rotation
/// replaces the file behind the path with a different inode.
#[derive(Debug)]
pub struct RotatingJsonlWriter {
    path: PathBuf,
    file: File,
}

impl RotatingJsonlWriter {
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let file = open_jsonl_append_file(&path)?;
        Ok(Self { path, file })
    }

    pub fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.reopen_if_path_replaced()?;
        writeln!(self.file, "{}", line)?;
        self.file.flush()
    }

    pub fn sync_all(&mut self) -> std::io::Result<()> {
        self.file.sync_all()
    }
    fn reopen_if_path_replaced(&mut self) -> std::io::Result<()> {
        if path_points_to_different_file(&self.file, &self.path)? {
            self.file = open_jsonl_append_file(&self.path)?;
        }
        Ok(())
    }
}

fn open_jsonl_append_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// #2442 — JSONL sentinel emitted by wrappers so the watcher /
/// recovery_engine can graduate the 2s drain quiet-period and 2s
/// ready-probe interval.
///
/// The wrapper writes one line per event directly to the session JSONL
/// using the same append-then-flush path as normal stream-json output.
/// Two flavors:
///  - `terminal_end` — emitted by `scopeguard` at wrapper exit (any exit
///    path the runtime can observe — clean exit, panic unwind). The
///    consumer treats this as a deterministic drain marker so the 2s
///    quiet-period in `recovery_engine.rs` can short-circuit. We still
///    keep the 2s fallback for SIGKILL paths that bypass scopeguard.
///  - `ready_for_input` — emitted by each wrapper immediately before/after
///    handing stdin off to the provider when the provider has signalled
///    readiness. The 2s probe-interval in `tmux.rs` short-circuits on
///    arrival; if the wrapper never writes (e.g. SIGKILL mid-turn) the
///    probe falls back to its existing cadence.
///
/// Both helpers are best-effort: a failure to write the sentinel never
/// affects the wrapper's primary work. Errors are silently dropped — the
/// 2s fallbacks on the consumer side keep behavior correct.
#[derive(Clone, Copy, Debug)]
pub enum WrapperSentinel<'a> {
    /// Wrapper is exiting. `exit` carries the runtime-derived reason
    /// string (`exit:N` / `signal:N` / `still_running`) for diagnostics.
    TerminalEnd { exit: &'a str },
    /// Provider has signalled readiness — wrapper is about to (or just
    /// did) accept further stdin. `provider` identifies the wrapper kind.
    ReadyForInput { provider: &'a str },
}

/// Public name of the JSONL `type` field for the terminal-end sentinel.
/// Exposed as a constant so consumers (recovery_engine.rs) and producers
/// (wrappers) can agree on the wire-level event name without string
/// duplication.
pub const WRAPPER_TERMINAL_END_EVENT: &str = "terminal_end";
/// Public name of the JSONL `type` field for the ready-for-input sentinel.
pub const WRAPPER_READY_FOR_INPUT_EVENT: &str = "ready_for_input";

/// Emit a sentinel line into the session JSONL. Best-effort; errors are
/// swallowed because the consumer-side fallbacks (2s drain quiet-period,
/// 2s ready-probe interval) keep behavior correct even when the sentinel
/// never lands.
pub fn emit_wrapper_sentinel(output_file: &str, sentinel: WrapperSentinel<'_>) {
    let line = match sentinel {
        WrapperSentinel::TerminalEnd { exit } => serde_json::json!({
            "type": WRAPPER_TERMINAL_END_EVENT,
            "exit": exit,
            "ts": chrono::Utc::now().to_rfc3339(),
        }),
        WrapperSentinel::ReadyForInput { provider } => serde_json::json!({
            "type": WRAPPER_READY_FOR_INPUT_EVENT,
            "provider": provider,
            "ts": chrono::Utc::now().to_rfc3339(),
        }),
    };
    let Ok(mut writer) = RotatingJsonlWriter::open(output_file) else {
        return;
    };
    let _ = writer.write_line(&line.to_string());
    let _ = writer.sync_all();
}

#[cfg(unix)]
fn path_points_to_different_file(file: &File, path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let file_meta = file.metadata()?;
    let path_meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(file_meta.dev() != path_meta.dev() || file_meta.ino() != path_meta.ino())
}

#[cfg(not(unix))]
fn path_points_to_different_file(_file: &File, _path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

// ── Rolling head-truncate for session jsonl ─────────────────────────────
//
// We cap session jsonl files at SIZE_CAP_BYTES. When they exceed the cap,
// we truncate from the head keeping ~TARGET_KEEP_BYTES worth of the most
// recent complete lines. A partial leading line after truncation is dropped
// so downstream stream-json parsers never see half of a record.

/// Soft cap at which we trigger head-truncation.
pub const JSONL_SIZE_CAP_BYTES: u64 = 20 * 1024 * 1024;
/// Target size to keep after truncation.
pub const JSONL_TARGET_KEEP_BYTES: u64 = 15 * 1024 * 1024;

/// Truncate a jsonl file from the head, keeping only complete lines totaling
/// at most `target_keep_bytes`. A leading partial line after the keep-window
/// is dropped so the first byte of the rewritten file is the first byte of a
/// complete line.
///
/// Returns `Ok(Some(new_size))` if the file was rewritten, `Ok(None)` if the
/// file is under cap or missing.
pub fn truncate_jsonl_head_safe(
    path: &str,
    size_cap_bytes: u64,
    target_keep_bytes: u64,
) -> std::io::Result<Option<u64>> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let size = meta.len();
    if size <= size_cap_bytes {
        return Ok(None);
    }

    // Figure out the byte offset we *want* to start keeping from.
    let start_offset = size.saturating_sub(target_keep_bytes);

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;
    let mut buf = Vec::with_capacity((size - start_offset) as usize);
    file.read_to_end(&mut buf)?;
    drop(file);

    // Drop any partial leading line: advance past the first newline so the
    // kept buffer begins at a line boundary. If no newline exists in buf
    // at all, we're keeping a single partial line — drop everything rather
    // than risk emitting a garbled record. (This is the rare case where
    // target_keep_bytes lands in the middle of an exceptionally huge line.)
    let keep_start = if start_offset == 0 {
        0 // no truncation needed at the head
    } else {
        match buf.iter().position(|b| *b == b'\n') {
            Some(idx) => idx + 1,
            None => buf.len(), // nothing complete to keep
        }
    };

    let kept = &buf[keep_start..];
    let new_size = kept.len() as u64;

    // Atomic-ish rewrite: write to sibling temp then rename.
    let tmp_path = format!("{}.truncate.tmp", path);
    {
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        out.write_all(kept)?;
        out.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(Some(new_size))
}

#[cfg(test)]
mod selector_overlay_tests {
    use super::*;

    #[test]
    fn selector_open_detected_for_effort_slider_footer() {
        // Claude Code 2.1.x `/effort` is a horizontal slider with a
        // `←/→ to adjust` footer while the overlay is open.
        let pane = "\
Claude Code v2.1.141

  Effort   low ─ medium ─ [high] ─ xhigh ─ max

  ←/→ to adjust · Enter to confirm · Esc to cancel";

        assert!(tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_detected_with_spaced_arrow_footer() {
        let pane = "\
  Effort
  ← / → to adjust   Enter to confirm";

        assert!(tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_false_when_only_footer_present_in_scrollback() {
        // A stale scrollback line that mentions the adjust footer but has no
        // accompanying Effort heading must not read as a live overlay.
        let pane = "\
Claude Code v2.1.141

  README: press ←/→ to adjust the carousel
❯
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_false_when_only_effort_word_present() {
        // A line that merely mentions "effort" without the adjust footer is
        // not a live slider overlay either.
        let pane = "\
Claude Code v2.1.141

⏺ I adjusted the effort estimate in the doc.
❯
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_false_for_plain_ready_prompt() {
        let pane = "\
Claude Code v2.1.141

❯
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_selector_open(pane));
    }

    #[test]
    fn selector_open_false_for_composer_draft_mentioning_adjust() {
        // A draft that merely contains the word "adjust" without the slider
        // arrow footer must not be mistaken for an open slider overlay.
        let pane = "\
Claude Code v2.1.141

❯ adjust the layout margins
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done";

        assert!(!tmux_capture_indicates_claude_tui_selector_open(pane));
    }
}

#[cfg(test)]
mod mcp_auth_required_tests {
    use super::*;

    /// The exact fresh-boot screen from #3889: a welcome box, the
    /// `⚠ N MCP server needs authentication · run /mcp` warning, and a composer
    /// that paints the usual separator + bypass-permissions footer. The composer
    /// chrome means `..._ready_for_input` reads this as READY, so the dedicated
    /// MCP-auth detector is what keeps the readiness gate from false-submitting
    /// into it.
    #[test]
    fn detects_cold_boot_mcp_auth_welcome_screen() {
        let pane = "\
╭─── Claude Code v2.1.195 ───────────────────────────╮
│            Welcome back 오부장!                    │
│   Opus 4.8 (1M context) · Claude Max               │
│   ~/.adk/release/workspaces/ch-ad                  │
╰────────────────────────────────────────────────────

 ⚠ 1 MCP server needs authentication · run /mcp

────────────────────────────────────────────────────
❯ [Pasted text #1 +59 lines]
────────────────────────────────────────────────────
  🤖 Opus(H) │ 0% │ MCP: 2 │ ⏵⏵ bypass permissions on";

        // The composer chrome makes the legacy readiness predicate read READY...
        assert!(tmux_capture_indicates_claude_tui_ready_for_input(pane));
        // ...but the MCP-auth detector flags it so the readiness gate refuses it.
        assert!(tmux_capture_indicates_claude_tui_mcp_auth_required(pane));
    }

    /// Plural copy (`N MCP servers need authentication`) must still match.
    #[test]
    fn detects_plural_servers_need_authentication() {
        let pane = "\
 ⚠ 2 MCP servers need authentication · run /mcp
────────────────────────────────────────────────────
❯
  🤖 Opus(H) │ 0% │ MCP: 3 │ ⏵⏵ bypass permissions on";

        assert!(tmux_capture_indicates_claude_tui_mcp_auth_required(pane));
    }

    /// A genuinely ready, empty composer with all MCP servers connected must NOT
    /// be flagged — the footer mentions `MCP: 2` but never "authentication".
    #[test]
    fn ignores_normal_ready_composer_with_connected_mcp() {
        let pane = "\
Claude Code v2.1.195
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 0% │ MCP: 2 │ ⏵⏵ bypass permissions on";

        assert!(tmux_capture_indicates_claude_tui_ready_for_input(pane));
        assert!(!tmux_capture_indicates_claude_tui_mcp_auth_required(pane));
    }

    /// Prose that mentions only one of the tokens (e.g. an assistant message
    /// talking about MCP, or about authentication generally) must not trip the
    /// detector — all of MCP + authentication + (need | run /mcp) are required.
    #[test]
    fn ignores_partial_token_prose() {
        let mcp_only = "⏺ I configured the MCP server list in settings.";
        assert!(!tmux_capture_indicates_claude_tui_mcp_auth_required(
            mcp_only
        ));

        let auth_only = "⏺ The API needs authentication via a bearer token.";
        assert!(!tmux_capture_indicates_claude_tui_mcp_auth_required(
            auth_only
        ));
    }

    /// Codex review #3931 [1] (over-block regression): an assistant transcript
    /// line that contains ALL the tokens (`mcp` + `authentication` + `run /mcp`)
    /// but sits as `⏺` output above a genuinely ready composer must NOT be
    /// classified as MCP-auth-blocked — only the system `⚠` warning banner is.
    #[test]
    fn ignores_assistant_prose_with_all_tokens_above_ready_composer() {
        let pane = "\
Claude Code v2.1.195
⏺ The MCP server needs authentication; run /mcp to reconnect it, then retry.
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 0% │ MCP: 2 │ ⏵⏵ bypass permissions on";

        // The composer is genuinely ready...
        assert!(tmux_capture_indicates_claude_tui_ready_for_input(pane));
        // ...and the all-token assistant prose above it must not block it.
        assert!(!tmux_capture_indicates_claude_tui_mcp_auth_required(pane));

        // The real system banner with the same tokens IS blocked — the `⚠`
        // chrome glyph is the only difference.
        let banner = "\
Claude Code v2.1.195
 ⚠ 1 MCP server needs authentication · run /mcp
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 0% │ MCP: 2 │ ⏵⏵ bypass permissions on";
        assert!(tmux_capture_indicates_claude_tui_mcp_auth_required(banner));
    }
}

#[cfg(test)]
mod sentinel_tests {
    use super::*;

    /// #2442 — round-trip the sentinel through the same code path the
    /// wrappers use, then verify the consumer-side tail-peek picks it up.
    #[test]
    fn emit_wrapper_sentinel_writes_terminal_end_line() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("session.jsonl");
        // Seed with normal output so the sentinel lands in the tail
        // window after some legit content.
        std::fs::write(&path, "{\"type\":\"assistant\",\"text\":\"hi\"}\n").unwrap();

        emit_wrapper_sentinel(
            path.to_str().unwrap(),
            WrapperSentinel::TerminalEnd { exit: "exit:0" },
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(&format!("\"type\":\"{}\"", WRAPPER_TERMINAL_END_EVENT)),
            "terminal_end sentinel must be present in the jsonl, got:\n{content}",
        );
        assert!(content.contains("\"exit\":\"exit:0\""));
    }

    /// #2442 — ready_for_input variant emits the correct provider tag so
    /// downstream consumers can attribute the readiness signal.
    #[test]
    fn emit_wrapper_sentinel_writes_ready_for_input_line() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("session.jsonl");

        emit_wrapper_sentinel(
            path.to_str().unwrap(),
            WrapperSentinel::ReadyForInput { provider: "codex" },
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(&format!("\"type\":\"{}\"", WRAPPER_READY_FOR_INPUT_EVENT)));
        assert!(content.contains("\"provider\":\"codex\""));
    }

    #[test]
    fn dead_marker_path_is_cleaned_with_session_temp_files() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_host = std::env::var_os("HOSTNAME");

        let tdir =
            std::env::temp_dir().join(format!("adk-issue-2424-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tdir);

        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", &tdir);
            std::env::set_var("HOSTNAME", "issue-2424-host");
        }

        let session = format!("issue-2424-cleanup-sess-{}", std::process::id());
        let marker_path = session_dead_marker_path(&session);
        if let Some(parent) = std::path::Path::new(&marker_path).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&marker_path, "pane-exited").unwrap();

        cleanup_session_temp_files(&session);

        assert!(
            !std::path::Path::new(&marker_path).exists(),
            "cleanup_session_temp_files must remove pane-death marker: {marker_path}"
        );

        let _ = std::fs::remove_dir_all(&tdir);
        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
        match previous_host {
            Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
    }

    #[test]
    fn claude_prompt_draft_detector_blocks_active_operator_draft() {
        let capture = "\
assistant output
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}operator is still typing
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%";

        assert!(tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_ready_prompt_rejects_active_work_chrome() {
        let capture = "\
⏺ Running 1 shell command…
· Actioning… (4m 7s · ↓ 9.4k tokens)
  ⎿  Tip: Use /btw to ask a quick side question without interrupting Claude's
     current work
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%
  CLAUDE.md: 1, MCP: 2 │ Tools: 12 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_ready_prompt_accepts_idle_empty_prompt() {
        let capture = "\
✻ Churned for 4m 56s
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%
  CLAUDE.md: 1, MCP: 2 │ Tools: 17 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_ready_prompt_accepts_submitted_prompt_with_idle_footer() {
        let capture = "\
✻ Crunched for 32s
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}claude-e 추가 채널 확장 진행해
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 5%
  CLAUDE.md: 1, MCP: 2 │ Tools: 4 done
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_ignores_submitted_discord_history_prompt() {
        let capture = "\
❯ [User: 0hbujang (ID: 343742347365974026)] 이전 턴
⏺ 처리했습니다.
✻ Baked for 2s
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_ignores_submitted_direct_history_prompt() {
        let capture = "\
❯ direct prompt typed through ssh
⏺ direct prompt typed through ssh
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_ignores_response_tail_with_tool_summary() {
        let capture = "\
❯ 계획만 적고 보류해줘
계획만 적고 보류 — 1개
  📁 claude-adk-cc-20260523-070547
  CLAUDE.md: 1, MCP: 2 │ Tools: 5 done";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert_eq!(
            tmux_capture_claude_tui_prompt_draft_backspace_budget(capture),
            None
        );
    }

    #[test]
    fn claude_prompt_draft_detector_uses_wider_window_for_history_completion() {
        let capture = "\
❯ direct prompt typed through ssh
  wrapped prompt line
  more wrapped prompt line
  filler 01
  filler 02
  filler 03
  filler 04
  filler 05
  filler 06
  filler 07
  filler 08
  filler 09
  filler 10
  filler 11
  filler 12
⏺ direct prompt typed through ssh
✻ Brewed for 2s";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_treats_running_submitted_prompt_as_not_ready() {
        let capture = "\
⏺ previous response
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
❯ direct prompt that has just been submitted
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done";

        assert!(tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn ready_for_input_rejects_freshly_submitted_prompt_with_bypass_banner() {
        // #3463/#3524: the banner-present companion to
        // `claude_prompt_draft_detector_treats_running_submitted_prompt_as_not_ready`.
        // A just-submitted prompt (footer `Tools: 0 done`, no output produced
        // yet) renders the `bypass permissions` banner, which on its own
        // satisfies idle chrome. It must STILL NOT read as ready-for-input —
        // otherwise a follow-up injects into a turn that has not produced output.
        // This is what keeps #3524's idle-suggestion relaxation from regressing
        // #3463; the freshly-submitted guard lives in `ready_for_input`, so a
        // finished 0-tool turn (see `claude_idle_suggestion_prompt_is_not_prompt_draft`)
        // is still reported as idle while this running one is not ready.
        let capture = "\
⏺ previous response
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
❯ direct prompt that has just been submitted
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn ready_for_input_rejects_fresh_submit_below_older_completed_prompt() {
        // codex #3524: the `.any` readiness scan must NOT let an OLDER historical
        // prompt — whose own `after_prompt` contains completed output — flip
        // readiness to true while the BOTTOM-most prompt is a just-submitted,
        // still-running turn (`Tools: 0 done`, no output). Otherwise the #3463
        // follow-up-injection race returns for multi-prompt panes.
        let capture = "\
❯ previous prompt
⏺ previous response
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
❯ direct prompt that has just been submitted
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ██░░░░░░░░ │ 24%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
    }

    #[test]
    fn claude_idle_suggestion_prompt_is_not_prompt_draft() {
        let capture = "\
⏺ TUI-E2E marker
✻ Worked for 2s
────────────────────────────────────────────────────────────────────────────
❯\u{00a0}좋아, 잘 동작하네
────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ░░░░░░░░░░ │ 4%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert_eq!(
            tmux_capture_claude_tui_prompt_draft_backspace_budget(capture),
            None
        );
        assert!(tmux_capture_indicates_claude_tui_idle_suggestion(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_recovers_stranded_followup_below_finished_block() {
        // #3924 (a): turn1 finished, turn2's `[User:]` follow-up Enter was
        // DROPPED, so it sits editable in the composer below the finished block
        // under idle-suggestion chrome. The bare `[User:]` exclusion previously
        // misclassified this as no-draft (idle ghost), so the recovery net never
        // fired and the turn was killed at 120s. It must now read as a DRAFT.
        let capture = "\
❯ [User: 0hbujang (ID: 343742347365974026)] previous prompt
⏺ previous response
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
❯ [User: 0hbujang (ID: 343742347365974026)] follow-up whose Enter was dropped
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%
  CLAUDE.md: 1, MCP: 2 │ Tools: 4 done
  ⏵⏵ bypass permissions on";

        assert!(tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(tmux_capture_claude_tui_prompt_draft_backspace_budget(capture).is_some());
        // A stranded draft is NOT an idle suggestion — the two readings must not
        // both be true, or downstream readiness/recovery would contradict.
        assert!(!tmux_capture_indicates_claude_tui_idle_suggestion(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_recovers_stranded_followup_below_zero_tool_block() {
        // #3924 codex re-review: the previously-MISSED shape. turn1 finished
        // having run ZERO tools — it still renders a `Tools: 0 done` footer — and
        // turn2's `[User:]` follow-up Enter was DROPPED below it. An earlier
        // attempt keyed the running-guard on `Tools: 0 done`, which a finished
        // 0-tool turn ALSO prints, so the stranded draft was hidden again. The
        // capture-side detector must now read this as a DRAFT (the recovery net's
        // JSONL transcript check, not the footer, decides running-vs-stranded).
        let capture = "\
❯ [User: 0hbujang (ID: 343742347365974026)] previous prompt
⏺ acknowledged, nothing to run
✻ Brewed for 1s
─────────────────────────────────────────────────────────────────────────────
❯ [User: 0hbujang (ID: 343742347365974026)] follow-up whose Enter was dropped
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert!(tmux_capture_claude_tui_prompt_draft_backspace_budget(capture).is_some());
        assert!(!tmux_capture_indicates_claude_tui_idle_suggestion(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_keeps_idle_ghost_below_finished_block_as_not_draft() {
        // #3924 (b): the genuine idle ghost — a finished turn left a non-injected
        // suggestion line in the composer below the finished block. It carries NO
        // `[User:]` injection marker, so it is leftover chrome, not a recoverable
        // dropped-Enter draft. It must stay NOT-a-draft / idle-suggestion.
        let capture = "\
⏺ TUI-E2E marker
✻ Worked for 2s
────────────────────────────────────────────────────────────────────────────
❯\u{00a0}좋아, 잘 동작하네
────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ░░░░░░░░░░ │ 4%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert_eq!(
            tmux_capture_claude_tui_prompt_draft_backspace_budget(capture),
            None
        );
        assert!(tmux_capture_indicates_claude_tui_idle_suggestion(capture));
    }

    #[test]
    fn claude_prompt_draft_detector_ignores_visibly_running_user_turn_with_spinner() {
        // #3924 codex re-review: a `[User:]` turn that is VISIBLY running shows a
        // live busy marker (`esc to interrupt`/spinner), which the idle-suggestion
        // chrome detector's busy guard excludes — so the capture-side detector
        // correctly reads NO draft here WITHOUT depending on the `Tools: 0 done`
        // footer (which is ambiguous between running and finished-0-tool). The
        // no-spinner `Tools: 0 done` running window that the pane CANNOT resolve
        // is instead disambiguated by the JSONL transcript in the recovery net —
        // see the claude.rs `freshly_submitted_*` recovery test.
        let capture = "\
⏺ previous response
✻ Brewed for 2s
─────────────────────────────────────────────────────────────────────────────
❯ [User: 0hbujang (ID: 343742347365974026)] follow-up that just submitted
─────────────────────────────────────────────────────────────────────────────
· Actioning… (3s · esc to interrupt)
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert_eq!(
            tmux_capture_claude_tui_prompt_draft_backspace_budget(capture),
            None
        );
    }

    #[test]
    fn claude_prompt_draft_detector_ignores_submitted_user_history_with_completed_output() {
        // #3924 guard: a `[User:]` turn that submitted AND produced output is pane
        // history, not a stranded draft. Completed-history output below the line
        // (`⏺`/`✻ Brewed`) must keep it NOT-a-draft so readiness is not blocked.
        let capture = "\
✻ Crunched for 32s
─────────────────────────────────────────────────────────────────────────────
❯ [User: 0hbujang (ID: 343742347365974026)] earlier follow-up
⏺ handled it
✻ Baked for 2s
─────────────────────────────────────────────────────────────────────────────
  CLAUDE.md: 1, MCP: 2 │ Tools: 3 done
  ⏵⏵ bypass permissions on";

        assert!(!tmux_capture_indicates_claude_tui_prompt_draft(capture));
        assert_eq!(
            tmux_capture_claude_tui_prompt_draft_backspace_budget(capture),
            None
        );
    }

    #[test]
    fn actively_streaming_detects_busy_pane_with_esc_to_interrupt() {
        // #3107: a live agentic turn that lost its inflight — the pane still
        // shows the busy/"esc to interrupt" marker and is producing.
        let capture = "\
⏺ Running 1 shell command…
· Actioning… (4m 7s · esc to interrupt)
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%";

        assert!(!tmux_capture_indicates_claude_tui_ready_for_input(capture));
        assert!(tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_rejects_ready_for_input_pane() {
        // A genuinely finished turn returned to ready-for-input: not streaming.
        let capture = "\
✻ Churned for 4m 56s
─────────────────────────────────────────────────────────────────────────────
❯\u{00a0}
─────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ █░░░░░░░░░ │ 7%
  CLAUDE.md: 1, MCP: 2 │ Tools: 17 done
  ⏵⏵ bypass permissions on";

        assert!(tmux_capture_indicates_claude_tui_ready_for_input(capture));
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_rejects_idle_suggestion_chrome() {
        // Idle-suggestion chrome is real post-finish ghost noise, not a live
        // turn — must not be treated as actively streaming.
        let capture = "\
⏺ TUI-E2E marker
✻ Worked for 2s
────────────────────────────────────────────────────────────────────────────
❯\u{00a0}좋아, 잘 동작하네
────────────────────────────────────────────────────────────────────────────
  🤖 Opus(H) │ ░░░░░░░░░░ │ 4%
  CLAUDE.md: 1, MCP: 2 │ Tools: 0 done
  ⏵⏵ bypass permissions on";

        assert!(tmux_capture_indicates_claude_tui_idle_suggestion(capture));
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_rejects_empty_capture() {
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(""));
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            "   \n  \n"
        ));
    }

    // #3107 codex re-review (P2#1): the original `!ready && !idle` definition
    // false-positived any pane that was merely not-idle as "streaming". The
    // tightened definition requires a POSITIVE busy signal, so a non-Claude /
    // error / scrolled / generic-prompt pane biases to FALSE (keep suppressing).
    #[test]
    fn actively_streaming_rejects_non_claude_pane() {
        // A plain shell prompt — not a Claude TUI at all — has no busy marker.
        let capture = "\
user@host ~/work %\u{00a0}
$ ls -la
total 0
$ ";
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_rejects_error_screen() {
        // An error/backtrace screen left in the pane is finished, not streaming.
        let capture = "\
thread 'main' panicked at src/lib.rs:42:9:
called `Result::unwrap()` on an `Err` value: Broken pipe
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
error: process didn't exit successfully (exit status: 101)";
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_rejects_scrolled_pane_without_busy_marker() {
        // A scrolled-back pane showing prior assistant output with no live
        // busy/spinner marker must not read as streaming.
        let capture = "\
⏺ Here is the summary of the changes I made earlier.
  ⎿  Edited 3 files, ran the test suite, all green.
some scrolled-back prose line
another scrolled-back prose line";
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_rejects_generic_prompt_waiting_pane() {
        // A generic prompt-waiting pane (no Claude busy chrome) is ambiguous and
        // must bias to FALSE (suppress), not be relayed as streaming.
        let capture = "\
Press any key to continue . . .
> ";
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_accepts_claude_busy_spinner_verb() {
        // A real Claude TUI mid-response with a spinner verb + active-work marker
        // (no ready/idle chrome) is the genuine "live turn lost its inflight" case.
        let capture = "\
⏺ Reading src/main.rs
· Musing… (12s · ↓ 2.1k tokens)";
        assert!(tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    // #3107 codex re-review (P2, F2): the busy classifier previously accepted any
    // recent line containing the bare substrings `running`/`processing`/`thinking`.
    // Those words appear in normal ASSISTANT BODY text, so a pane that has
    // finished but still shows such prose was mis-read as streaming. The marker
    // must be Claude-TUI chrome (spinner glyph / `esc to interrupt`), not a word.
    #[test]
    fn actively_streaming_rejects_assistant_body_with_busy_words_but_no_chrome() {
        // Assistant body text mentions "running" / "processing" / "thinking" but
        // there is NO `esc to interrupt` footer and NO spinner progress line.
        let capture = "\
⏺ I checked the build: the test suite is running in CI and the worker is
  still processing the queue while thinking through the edge cases.
some more scrolled-back assistant prose
another line of prior output";
        assert!(!tmux_capture_indicates_claude_tui_busy(capture));
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn background_agent_pending_detects_chrome_not_body_text() {
        // #3521: the `✻ Waiting for N background agent to finish` footer and the
        // `Backgrounded agent` spawn line ARE detected (keep the turn/footer alive);
        // foreground-idle panes and assistant prose merely mentioning a background
        // agent are NOT (no false keep-alive → no stuck turn).
        assert!(tmux_capture_indicates_claude_tui_background_agent_pending(
            "⏺ reading docs\n✻ Waiting for 1 background agent to finish\n❯ "
        ));
        assert!(tmux_capture_indicates_claude_tui_background_agent_pending(
            "⏺ Agent(read story)\n  ⎿  Backgrounded agent (↓ to manage · ctrl+o to expand)\n❯ "
        ));
        assert!(!tmux_capture_indicates_claude_tui_background_agent_pending(
            "⏺ done.\n❯ \n  🤖 Opus"
        ));
        assert!(!tmux_capture_indicates_claude_tui_background_agent_pending(
            "I will hand that to the background agent.\n❯ "
        ));
    }

    // #3107 F2: a real Claude TUI in-progress frame keyed only on the strongest
    // marker (`esc to interrupt`) — no spinner verb, no `⏺` active-work line —
    // must still read as streaming.
    #[test]
    fn actively_streaming_accepts_esc_to_interrupt_footer_only() {
        let capture = "\
some earlier assistant prose still on screen
(13s · ↓ 1.2k tokens · esc to interrupt)";
        // Claude renders this parenthesized footer as a standalone status line
        // while a turn is active. It is intentionally sufficient busy evidence;
        // prose with the same text embedded mid-line is rejected below.
        assert!(tmux_capture_indicates_claude_tui_busy(capture));
        assert!(tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn busy_rejects_assistant_interrupt_prose_above_idle_composer() {
        let capture = "\
⏺ Press Esc to interrupt the current Claude Code turn.
✻ Baked for 2s
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2 │ Tools: 1 done";

        assert!(!tmux_capture_indicates_claude_tui_busy(capture));
        assert!(tmux_capture_indicates_claude_tui_ready_for_input(capture));

        for parenthesized_prose in [
            "\
⏺ The footer shows (12s · esc to interrupt) while the turn is running.
✻ Baked for 2s
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2 │ Tools: 1 done",
            "\
(12s · esc to interrupt) is the footer example
✻ Baked for 2s
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2 │ Tools: 1 done",
        ] {
            assert!(!tmux_capture_indicates_claude_tui_busy(parenthesized_prose));
            assert!(tmux_capture_indicates_claude_tui_ready_for_input(
                parenthesized_prose
            ));
        }
    }

    // #3107 codex re-review (F2 PARTIAL close): a spinner-progress line keyed on
    // ONLY the leading glyph + work verb still false-positived on assistant prose
    // that happens to begin with a spinner glyph and a verb. The real Claude TUI
    // spinner footer ALWAYS carries a status SUFFIX (`esc to interrupt`, a
    // duration, a token count, and/or the `·` separator). The recognizer now
    // requires that suffix, so bare prose can no longer trip it.
    #[test]
    fn actively_streaming_rejects_glyph_verb_prose_without_status_suffix() {
        // Assistant body line: leading spinner glyph + work verb, but NO Claude
        // TUI status suffix → NOT a spinner-progress footer → NOT busy.
        let capture = "\
· Thinking through the problem and running the tests
some more scrolled-back assistant prose
another line of prior output";
        assert!(!tmux_line_is_claude_tui_spinner_progress(
            "· Thinking through the problem and running the tests"
        ));
        assert!(!tmux_capture_indicates_claude_tui_busy(capture));
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }

    #[test]
    fn actively_streaming_accepts_real_spinner_with_status_suffix() {
        // The genuine Claude TUI spinner footer: glyph + verb + parenthesized
        // status group with a duration, token count, and `esc to interrupt`.
        let line = "✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)";
        assert!(tmux_line_is_claude_tui_spinner_progress(line));
        let capture = format!("earlier assistant prose\n{line}");
        assert!(tmux_capture_indicates_claude_tui_busy(&capture));
        assert!(tmux_capture_indicates_claude_tui_actively_streaming(
            &capture
        ));
    }

    #[test]
    fn actively_streaming_accepts_spinner_with_duration_only_status() {
        // A spinner footer whose status group carries only a bare duration token
        // (no `esc to interrupt`, no `tokens`) still qualifies.
        let line = "✻ Thinking… (12s)";
        assert!(tmux_line_is_claude_tui_spinner_progress(line));
    }

    #[test]
    fn structured_spinner_accepts_unknown_and_early_status_frames() {
        let capture = "\
earlier assistant prose
✳ Architecting…
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2 │ ⏵⏵ bypass permissions on";
        assert!(!tmux_line_is_claude_tui_spinner_progress("✳ Architecting…"));
        assert!(tmux_line_is_claude_tui_structured_spinner(
            "✳ Architecting…"
        ));
        assert!(tmux_capture_indicates_claude_tui_busy(capture));
        for line in [
            "· Thinking…",
            "✳ Beboppin'…",
            "✳ Beboppin'… (12s)",
            "✳ Dilly-dallying…",
            "✦ Mapping distant galaxies…",
            "✦ Mapping distant galaxies… (12s",
            "· Compacting conversation… (30s)",
        ] {
            assert!(
                tmux_line_is_claude_tui_structured_spinner(line),
                "live early or duration spinner must be recognized: {line}"
            );
        }
        let live_early_spinner_with_stale_prompt = "\
· Thinking…
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2";
        assert!(tmux_capture_indicates_claude_tui_ready_for_input(
            live_early_spinner_with_stale_prompt
        ));
        assert!(tmux_capture_indicates_claude_tui_busy(
            live_early_spinner_with_stale_prompt
        ));
    }

    #[test]
    fn wrapped_interrupt_tail_requires_adjacent_open_spinner_head() {
        for live_wrapped_spinner in [
            "\
✳ Beboppin'… (12s · ↓ 1.2k tokens ·
esc to interrupt)
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2",
            "\
(13s · ↓ 1.2k tokens · esc to interrupt
)
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2",
            "\
(13s · ↓ 1.2k tokens · esc to interru
pt)
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2",
        ] {
            assert!(!tmux_capture_indicates_claude_tui_ready_for_input(
                live_wrapped_spinner
            ));
            assert!(tmux_capture_indicates_claude_tui_busy(live_wrapped_spinner));
        }

        let stale_isolated_wrapped_tail = "\
⏺ completed response
✻ Baked for 2s
esc to interrupt)
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2 │ Tools: 1 done";
        assert!(tmux_capture_indicates_claude_tui_ready_for_input(
            stale_isolated_wrapped_tail
        ));
        assert!(!tmux_capture_indicates_claude_tui_busy(
            stale_isolated_wrapped_tail
        ));
    }

    #[test]
    fn structured_spinner_rejects_prose_and_non_status_parentheticals() {
        assert!(!tmux_line_is_claude_tui_structured_spinner(
            "✳ Architecting the response (12s)"
        ));
        assert!(!tmux_line_is_claude_tui_structured_spinner(
            "✳ Architecting… (a fresh idea)"
        ));
        assert!(!tmux_line_is_claude_tui_structured_spinner(
            "✳ Architecting… (12s) is displayed here"
        ));
        assert!(!tmux_capture_indicates_claude_tui_busy(
            "✳ Architecting… (12s) is displayed here\n────────────────────────\n❯"
        ));
        for verb in ["Actioning", "Musing"] {
            let prose = format!(
                "· {verb}… (12s) is displayed here\n✻ Baked for 2s\n────────────────────────\n❯\n────────────────────────\n  Tools: 1 done"
            );
            assert!(!tmux_capture_indicates_claude_tui_busy(&prose));
            assert!(tmux_capture_indicates_claude_tui_ready_for_input(&prose));
        }
        assert!(!tmux_line_is_claude_tui_structured_spinner(
            "Architecting… (12s)"
        ));
        assert!(!tmux_line_is_claude_tui_structured_spinner(
            "· Thinking through the problem…"
        ));
        for line in ["✳ Done…", "✳ Done… (12s)", "· Nothin'…"] {
            assert!(!tmux_line_is_claude_tui_structured_spinner(line));
        }
        assert!(!tmux_capture_indicates_claude_tui_busy(
            "✳ Done… (12s)\n────────────────────────────────────────\n❯"
        ));
        assert!(!tmux_line_is_claude_tui_structured_spinner(
            "✳ This ordinary prose has far too many words to be compact status chrome…"
        ));
        assert!(!tmux_capture_indicates_claude_tui_structured_spinner(
            "\
Here is the exact status-line format:
```text
✳ Architecting… (12s)
```
────────────────────────────────────────────────────
❯
────────────────────────────────────────────────────
  🤖 Opus(H) │ 7% │ MCP: 2 │ ⏵⏵ bypass permissions on"
        ));
    }

    #[test]
    fn unmatched_fence_does_not_hide_later_spinner_chrome() {
        // The first fence is a closing-only tail after its opener scrolled away;
        // the second fixture leaves a dangling opener. Neither incomplete capture
        // context may suppress later live status chrome.
        for capture in [
            "```\nprior prose after a scrolled-away opener\n✦ Mapping distant galaxies… (12s)",
            "```text\n✳ Architecting…",
        ] {
            assert!(
                tmux_capture_indicates_claude_tui_structured_spinner(capture),
                "an unmatched tail fence must fail safe for later spinner chrome"
            );
        }
    }

    #[test]
    fn actively_streaming_rejects_glyph_verb_with_plain_parenthetical() {
        // Glyph + verb followed by an ordinary parenthetical with no TUI status
        // marker (no duration, no `tokens`, no `·`) must NOT qualify.
        let line = "· Thinking about the design (a fresh idea here)";
        assert!(!tmux_line_is_claude_tui_spinner_progress(line));
    }

    #[test]
    fn actively_streaming_rejects_glyph_verb_past_tense_completion() {
        // Past-tense `<verb> for <duration>` completion summary stays excluded.
        let line = "· Running for 3s";
        assert!(!tmux_line_is_claude_tui_spinner_progress(line));
        let capture = "\
· Running for 3s
some scrolled-back prose line
another scrolled-back prose line";
        assert!(!tmux_capture_indicates_claude_tui_busy(capture));
        assert!(!tmux_capture_indicates_claude_tui_actively_streaming(
            capture
        ));
    }
}
