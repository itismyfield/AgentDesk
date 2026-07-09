//! #4367: LIVE status-panel `Subagents` section rendering, split out of
//! `status_panel.rs` to keep that file within the placeholder_live_events
//! namespace size cap (mirrors what `task_panel.rs` does for the Tasks section
//! after #4093). Owns the subagent-slot render helper and the in-progress-only
//! live filter; the `SubagentSlot` model and its lifecycle state machine stay in
//! `status_panel.rs`, and the completion footer keeps its own terminal-aware
//! subagent rendering.

use super::common::{
    EVENT_LINE_MAX_CHARS, STATUS_PANEL_SUBAGENT_LIMIT, escape_status_panel_markdown,
    normalize_summary, sanitized_tool_name, truncate_chars, truncate_chars_with_marker,
};
use super::status_panel::SubagentSlot;
use super::subagent_summary::render_subagent_done_summary;

pub(super) fn render_subagent_slot(slot: &SubagentSlot) -> String {
    let mut line = format!(
        "└ {} {}",
        sanitize_label(&slot.subagent_type),
        escape_status_panel_markdown(&normalize_summary(&slot.desc))
    );
    if let Some(recent) = slot
        .recent
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        line.push_str(" — ");
        line.push_str(&escape_status_panel_markdown(&normalize_summary(recent)));
    }
    // #3086: append the TUI-parity Done summary on finished slots with accounting.
    if let Some(summary) = slot
        .summary
        .as_ref()
        .filter(|_| matches!(slot.finished, Some(true)))
        .filter(|summary| !summary.is_empty())
        && let Some(done) = render_subagent_done_summary(summary)
    {
        line.push_str(" — ");
        line.push_str(&done);
    }
    // #3391: reserve marker width so a finished line always ENDS WITH its ✓/✗.
    match slot.terminal_marker() {
        Some(marker) => truncate_chars_with_marker(&line, marker, EVENT_LINE_MAX_CHARS),
        None => truncate_chars(&line, EVENT_LINE_MAX_CHARS),
    }
}

/// #4367: a subagent slot is "in progress" — the only kind the LIVE Subagents
/// panel now renders — iff it is NOT terminal (carries no ✓/✗). A finished slot
/// (completed / failed) is hidden immediately so finished subagents no longer
/// mask active ones until they fall out of the 10-slot window (the exact #4367
/// symptom: two already-completed relay-audit subagents kept showing).
///
/// `finished == None` is treated as IN PROGRESS, not "done" — the direct analogue
/// of #4093's `status == None` reasoning for tasks. A freshly-created subagent
/// (`SubagentStart`) carries `finished == None` until its `SubagentEnd`, and a
/// `run_in_background` subagent keeps `finished == None` across an ack-only end
/// for its whole running life; only a genuine completion sets `finished =
/// Some(_)`. Keying on terminal-ness (`SubagentSlot::is_terminal`) alone
/// therefore keeps brand-new and long-running subagents visible mid-flight.
///
/// This gates the LIVE panel only. The completion footer deliberately still
/// renders terminal slots — its ✓/✗ turn-end result summary and the #3391
/// delivered-terminal eviction both depend on finished rows being emitted — so it
/// must not use this predicate.
pub(super) fn subagent_slot_is_in_progress(slot: &SubagentSlot) -> bool {
    !slot.is_terminal()
}

/// #4367: renders the LIVE status panel's `Subagents` section for `subagents`, or
/// `None` when nothing should render. Only in-progress slots are shown (completed
/// / failed rows are hidden so they can never mask active work), newest first,
/// capped at `STATUS_PANEL_SUBAGENT_LIMIT` over the FILTERED set. Returns `None`
/// when no in-progress subagent survives so the caller emits no dangling
/// `Subagents` header. The Codex-provider suppression stays with the caller (Codex
/// never renders subagents). Colocated here (mirroring
/// `task_panel::render_live_tasks_section`) so subagent-slot rendering lives with
/// the subagent-slot render helper.
///
/// No #3404 live terminal-slot compaction runs here (nor in the Tasks section
/// after #4093 후속). `compact_live_panel_terminal_lines` classified a line as
/// terminal by TEXT (`ends_with('✓'|'✗')`); once this section is filtered to
/// in-progress slots, no genuine terminal line can reach it, so its only possible
/// matches would be FALSE POSITIVES — a running subagent whose desc/recent text
/// happens to end with a ✓/✗ glyph — which would wrongly hide in-progress rows
/// behind a `… (+N completed)` summary (the #4367 bug inverted). Terminals are
/// hidden outright now, so capping how many terminal rows render is moot.
pub(super) fn render_live_subagents_section(subagents: &[SubagentSlot]) -> Option<String> {
    if subagents.is_empty() {
        return None;
    }
    let lines = subagents
        .iter()
        .rev()
        .filter(|slot| subagent_slot_is_in_progress(slot))
        .take(STATUS_PANEL_SUBAGENT_LIMIT)
        .map(render_subagent_slot)
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| format!("Subagents\n{}", lines.join("\n")))
}

fn sanitize_label(raw: &str) -> String {
    sanitized_tool_name(raw).unwrap_or_else(|| "Task".to_string())
}
