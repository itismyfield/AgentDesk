use poise::serenity_prelude::ChannelId;

use crate::services::discord::single_message_panel::SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES;
use crate::services::provider::ProviderKind;

use super::common::{
    EVENT_LINE_MAX_CHARS, STATUS_PANEL_SUBAGENT_LIMIT, STATUS_PANEL_TASK_LIMIT, truncate_chars,
    truncate_chars_with_marker,
};
use super::context_panel::render_context_panel_line;
use super::status_panel::{StatusPanelState, SubagentSlot, render_subagent_slot};
use super::task_panel::{
    TaskToolSlot, render_task_tool_slot, task_tool_slot_identity, task_tool_slot_is_terminal,
    task_tool_terminal_marker,
};

/// #3391: stable per-slot handle (NOT the rendered line). `ToolUseId`/`TaskId`
/// carry the slot's primary external id; `Ordinal` is the never-reused internal
/// counter every slot also has. Compared for equality during eviction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum SlotKey {
    ToolUseId(String),
    TaskId(String),
    Ordinal(u64),
}

/// #3391: identity of a terminal slot whose mark was INCLUDED in a delivered
/// block, tagged by which list it lives in so eviction targets the right Vec
/// (a task and a subagent could share the same ordinal counter value only
/// across lists; the tag keeps them disjoint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum TerminalSlotId {
    Task(SlotKey),
    Subagent(SlotKey),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct CompletionFooterRender {
    pub(in crate::services::discord) block: Option<String>,
    pub(in crate::services::discord) has_unfinished_entries: bool,
    /// #3391: identities of EXACTLY the terminal task/subagent slots whose
    /// lines were included in `block` post-clamp. After the Discord edit
    /// containing `block` is CONFIRMED delivered, pass these to
    /// `evict_delivered_terminal_footer_tasks` so the next render drops them.
    /// Slot identity (not the line string) is used so two slots rendering the
    /// IDENTICAL terminal line stay distinct: a clamped-out duplicate keeps its
    /// (undelivered) mark, and a slot that turned terminal between render and
    /// ack — whose line happens to match another delivered line — is never
    /// evicted on a mark it never showed (Finding 1).
    pub(in crate::services::discord) delivered_terminal_ids: Vec<TerminalSlotId>,
}

// #3391: delivery-ack surface colocated with the render below — eviction drops
// exactly the slot identities `render_completion_footer` reported as delivered.
impl super::PlaceholderLiveEvents {
    /// Drops task/subagent slots whose terminal mark (✓/✗) was confirmed
    /// delivered in a completion-footer render, addressing them by SLOT
    /// IDENTITY rather than the rendered line. Call only after the Discord
    /// edit/send returned Ok — a failed edit retries the terminal mark on the
    /// next render. A slot is dropped only if its identity is in the delivered
    /// set AND it is STILL terminal at evict time; ordinals are unique and never
    /// reused, so a non-terminal slot can never carry a recycled id that aliases
    /// an evicted one, and in-flight slots are never dropped.
    pub(in crate::services::discord) fn evict_delivered_terminal_footer_tasks(
        &self,
        channel_id: ChannelId,
        delivered_terminal_ids: &[TerminalSlotId],
    ) {
        if delivered_terminal_ids.is_empty() {
            return;
        }
        let Some(entry) = self.status_by_channel.get(&channel_id) else {
            return;
        };
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.tasks.retain(|slot| {
            !task_tool_slot_is_terminal(slot)
                || !delivered_terminal_ids
                    .contains(&TerminalSlotId::Task(task_tool_slot_identity(slot)))
        });
        guard.subagents.retain(|slot| {
            !slot.is_terminal()
                || !delivered_terminal_ids.contains(&TerminalSlotId::Subagent(slot.identity()))
        });
    }
}

/// #3391: one emitted footer line plus the identity of the terminal slot it
/// belongs to (`None` for headers and non-terminal lines). The clamp keeps a
/// contiguous prefix of these, so survival is decided by POSITION — never by
/// line-string matching, which collides on duplicate terminal lines.
struct EmittedLine {
    text: String,
    terminal_id: Option<TerminalSlotId>,
}

pub(super) fn render_completion_footer(
    snapshot: StatusPanelState,
    provider: &ProviderKind,
    indicator: &str,
) -> CompletionFooterRender {
    let mut sections: Vec<String> = Vec::new();
    if let Some(context_line) = snapshot
        .context
        .as_ref()
        .and_then(|context| render_context_panel_line(context, provider))
    {
        sections.push(context_line);
    }

    // Flat ordered list of emitted task/subagent lines (incl. section headers
    // and the blank separator) carrying each terminal slot's identity. The
    // clamp below keeps a prefix of these, so a terminal line counts as
    // delivered iff its position survives.
    let mut emitted: Vec<EmittedLine> = Vec::new();
    let mut has_unfinished_entries = false;

    if !snapshot.tasks.is_empty() {
        emitted.push(EmittedLine::header("Tasks"));
        let mut task_unfinished = false;
        for slot in snapshot.tasks.iter().rev().take(STATUS_PANEL_TASK_LIMIT) {
            let (line, unfinished) = render_completion_task_tool_slot(slot, indicator);
            task_unfinished |= unfinished;
            // #3391 honesty guarantee: only report a delivered identity if the
            // FINAL rendered line actually ends with this slot's terminal mark.
            // Fix 1 reserves the marker width, so a terminal line ALWAYS ends
            // with its ✓/✗ — debug_assert that invariant, but keep the runtime
            // `line_shows_marker` gate so a future render regression can never
            // evict a slot on a mark the user never saw.
            let marker = task_tool_terminal_marker(slot.status.as_deref());
            let terminal_id = (!unfinished)
                .then(|| {
                    debug_assert!(
                        line_shows_marker(&line, marker),
                        "terminal task line dropped its mark: {line:?}"
                    );
                    line_shows_marker(&line, marker)
                        .then(|| TerminalSlotId::Task(task_tool_slot_identity(slot)))
                })
                .flatten();
            emitted.push(EmittedLine {
                text: line,
                terminal_id,
            });
        }
        has_unfinished_entries |= task_unfinished;
    }

    if !matches!(provider, ProviderKind::Codex) && !snapshot.subagents.is_empty() {
        if !emitted.is_empty() {
            emitted.push(EmittedLine::blank());
        }
        emitted.push(EmittedLine::header("Subagents"));
        let mut subagent_unfinished = false;
        for slot in snapshot
            .subagents
            .iter()
            .rev()
            .take(STATUS_PANEL_SUBAGENT_LIMIT)
        {
            subagent_unfinished |= !slot.is_terminal();
            let line = render_completion_subagent_slot(slot, indicator);
            // #3391 honesty guarantee (mirrors the task loop): a finished
            // subagent's line always ends with its ✓/✗ thanks to fix 1 —
            // debug_assert it, but gate the delivered id on the runtime check.
            let marker = slot.terminal_marker();
            let terminal_id = slot
                .is_terminal()
                .then(|| {
                    debug_assert!(
                        line_shows_marker(&line, marker),
                        "terminal subagent line dropped its mark: {line:?}"
                    );
                    line_shows_marker(&line, marker)
                        .then(|| TerminalSlotId::Subagent(slot.identity()))
                })
                .flatten();
            emitted.push(EmittedLine {
                text: line,
                terminal_id,
            });
        }
        has_unfinished_entries |= subagent_unfinished;
    }

    if !emitted.is_empty() {
        // #3089 completion footer: keep the Context line outside the S3 budget
        // so usage never disappears because a task section is noisy. The same
        // 600-byte cap applies to the combined task/subagent section.
        let section = emitted
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let (clamped, kept_count) = clamp_completion_task_section(&section);
        // #3391: a terminal mark counts as delivered iff its emitted line
        // survived the clamp by POSITION. `kept_count` is the number of leading
        // emitted lines retained (the rest are replaced by the `…` marker), so
        // exactly the first `kept_count` identities are delivered.
        let delivered_terminal_ids = emitted
            .iter()
            .take(kept_count)
            .filter_map(|line| line.terminal_id.clone())
            .collect::<Vec<_>>();
        sections.push(clamped);
        return CompletionFooterRender {
            block: Some(sections.join("\n\n")),
            has_unfinished_entries,
            delivered_terminal_ids,
        };
    }

    CompletionFooterRender {
        block: (!sections.is_empty()).then(|| sections.join("\n\n")),
        has_unfinished_entries,
        delivered_terminal_ids: Vec::new(),
    }
}

impl EmittedLine {
    fn header(name: &str) -> Self {
        Self {
            text: name.to_string(),
            terminal_id: None,
        }
    }

    fn blank() -> Self {
        Self {
            text: String::new(),
            terminal_id: None,
        }
    }
}

/// #3391: true iff `line` ends with this slot's terminal mark (`marker` is the
/// ✓/✗ the slot maps to, or `None` if it is not terminal). Used as the
/// delivered-id honesty gate: a slot's identity is reported as delivered only
/// when the user can actually SEE its mark on the rendered line. Fix 1 keeps
/// this true for every terminal slot; the runtime check is the safety net.
fn line_shows_marker(line: &str, marker: Option<&str>) -> bool {
    match marker {
        Some(marker) => line.ends_with(marker),
        None => false,
    }
}

fn render_completion_task_tool_slot(slot: &TaskToolSlot, indicator: &str) -> (String, bool) {
    let (marker, unfinished) = completion_task_marker(slot.status.as_deref(), indicator);
    let base = render_task_tool_slot(slot);
    // #3391: when this slot is terminal (✓/✗, `unfinished == false`), reserve the
    // marker's width before truncating so the FINAL line always ends with its
    // mark — a long description can no longer swallow it via a second
    // post-append truncation here. Background terminal slots already carry a
    // truncation-proof mark from `render_task_tool_slot`, so reuse `base` as-is.
    // Non-terminal lines (indicator or empty marker) keep plain truncation.
    let line = if slot.background && task_tool_terminal_marker(slot.status.as_deref()).is_some() {
        truncate_chars(&base, EVENT_LINE_MAX_CHARS)
    } else if marker.is_empty() {
        truncate_chars(&base, EVENT_LINE_MAX_CHARS)
    } else if !unfinished {
        truncate_chars_with_marker(&base, marker, EVENT_LINE_MAX_CHARS)
    } else {
        truncate_chars(&format!("{base} {marker}"), EVENT_LINE_MAX_CHARS)
    };
    (line, unfinished)
}

fn completion_task_marker<'a>(status: Option<&str>, indicator: &'a str) -> (&'a str, bool) {
    let Some(status) = status.map(str::trim).filter(|value| !value.is_empty()) else {
        return (indicator, true);
    };
    let normalized = status.to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "completed" | "complete" | "done" | "success" | "succeeded" | "ok"
    ) || normalized.contains("complete")
        || normalized.contains("success")
        || normalized.contains("done")
    {
        ("✓", false)
    } else if matches!(
        normalized.as_str(),
        "failed"
            | "failure"
            | "error"
            | "errored"
            | "aborted"
            | "killed"
            | "stopped"
            | "cancelled"
            | "canceled"
    ) || normalized.contains("fail")
        || normalized.contains("error")
        || normalized.contains("abort")
        || normalized.contains("kill")
        || normalized.contains("stop")
        || normalized.contains("cancel")
    {
        ("✗", false)
    } else {
        (indicator, true)
    }
}

fn render_completion_subagent_slot(slot: &SubagentSlot, indicator: &str) -> String {
    let base = render_subagent_slot(slot);
    if slot.finished.is_none() {
        truncate_chars(&format!("{base} {indicator}"), EVENT_LINE_MAX_CHARS)
    } else {
        base
    }
}

/// #3391: returns the clamped section together with `kept_count` — the number
/// of leading lines retained verbatim. Callers map emitted-line positions
/// `< kept_count` to delivered terminal identities; positions beyond it were
/// collapsed into the `…` marker and are NOT delivered.
///
/// #3391 outer-truncation audit (fix 3): this clamp keeps WHOLE lines — it only
/// joins a leading prefix `lines[..keep_count]` and never splits a kept line, so
/// a retained terminal line's tail (its ✓/✗) survives intact. The only mid-line
/// cut is the "not even the first line fits" fallback, which returns
/// `kept_count == 0`, so nothing is reported delivered there. The single
/// downstream byte-level clamp (`single_message_panel::clamp_footer_status_block`,
/// ~1994 bytes against the Discord message ceiling) sits far above this 600-byte
/// section budget and so cannot reach a terminal line tail; the body-vs-suffix
/// split in `compose_completion_footer_text` only trims the response body, never
/// the appended completion block. No extra reservation is needed at those sites.
fn clamp_completion_task_section(task_section: &str) -> (String, usize) {
    let lines: Vec<&str> = task_section.lines().collect();
    if task_section.len() <= SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES {
        return (task_section.to_string(), lines.len());
    }

    const TRUNCATION_MARKER: &str = "…";
    for keep_count in (1..=lines.len()).rev() {
        let prefix = lines[..keep_count].join("\n");
        let candidate = format!("{prefix}\n{TRUNCATION_MARKER}");
        if candidate.len() <= SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES {
            return (candidate, keep_count);
        }
    }

    // Not even the first full line fits: it is truncated mid-line, so no whole
    // emitted line survived — `kept_count` is 0 and nothing is delivered.
    let first_line = lines.first().copied().unwrap_or_default();
    let first_line_budget = SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES
        .saturating_sub(TRUNCATION_MARKER.len())
        .saturating_sub(1);
    let safe_end =
        crate::services::discord::formatting::floor_char_boundary(first_line, first_line_budget);
    if safe_end == 0 {
        (TRUNCATION_MARKER.to_string(), 0)
    } else {
        (
            format!("{}\n{TRUNCATION_MARKER}", &first_line[..safe_end]),
            0,
        )
    }
}
