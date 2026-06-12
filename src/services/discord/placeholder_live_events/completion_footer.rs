use poise::serenity_prelude::ChannelId;

use crate::services::discord::single_message_panel::SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES;
use crate::services::provider::ProviderKind;

use super::common::{
    EVENT_LINE_MAX_CHARS, STATUS_PANEL_SUBAGENT_LIMIT, STATUS_PANEL_TASK_LIMIT, truncate_chars,
};
use super::context_panel::render_context_panel_line;
use super::status_panel::{StatusPanelState, SubagentSlot, render_subagent_slot};
use super::task_panel::{TaskToolSlot, render_task_tool_slot, task_tool_terminal_marker};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct CompletionFooterRender {
    pub(in crate::services::discord) block: Option<String>,
    pub(in crate::services::discord) has_unfinished_entries: bool,
    /// #3391: task lines in `block` carrying a terminal mark (✓/✗) that
    /// survived the S3 clamp. After the Discord edit containing `block` is
    /// CONFIRMED delivered, pass these to
    /// `evict_delivered_terminal_footer_tasks` so the next render drops them.
    pub(in crate::services::discord) terminal_task_lines: Vec<String>,
}

// #3391: delivery-ack surface colocated with the render below — eviction must
// reproduce exactly the per-slot lines `render_completion_footer` produced.
impl super::PlaceholderLiveEvents {
    /// Drops task slots whose terminal mark (✓/✗) was confirmed delivered in a
    /// completion-footer render. Call only after the Discord edit/send returned
    /// Ok — a failed edit retries the terminal mark on the next render. A slot
    /// is matched by its recomputed render line (terminal lines are
    /// indicator-free), so a slot that mutated since the delivered render keeps
    /// rendering and evicts on the next confirmed delivery; in-flight slots
    /// never match.
    pub(in crate::services::discord) fn evict_delivered_terminal_footer_tasks(
        &self,
        channel_id: ChannelId,
        delivered_terminal_task_lines: &[String],
    ) {
        if delivered_terminal_task_lines.is_empty() {
            return;
        }
        let Some(entry) = self.status_by_channel.get(&channel_id) else {
            return;
        };
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.tasks.retain(|slot| {
            let (line, unfinished) = render_completion_task_tool_slot(slot, "");
            unfinished || !delivered_terminal_task_lines.contains(&line)
        });
    }
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

    let mut task_sections: Vec<String> = Vec::new();
    let mut has_unfinished_entries = false;
    let mut terminal_task_lines: Vec<String> = Vec::new();

    if !snapshot.tasks.is_empty() {
        let mut task_unfinished = false;
        let lines = snapshot
            .tasks
            .iter()
            .rev()
            .take(STATUS_PANEL_TASK_LIMIT)
            .map(|slot| {
                let (line, unfinished) = render_completion_task_tool_slot(slot, indicator);
                task_unfinished |= unfinished;
                if !unfinished {
                    terminal_task_lines.push(line.clone());
                }
                line
            })
            .collect::<Vec<_>>();
        has_unfinished_entries |= task_unfinished;
        task_sections.push(format!("Tasks\n{}", lines.join("\n")));
    }

    if !matches!(provider, ProviderKind::Codex) && !snapshot.subagents.is_empty() {
        let mut subagent_unfinished = false;
        let lines = snapshot
            .subagents
            .iter()
            .rev()
            .take(STATUS_PANEL_SUBAGENT_LIMIT)
            .map(|slot| {
                subagent_unfinished |= slot.finished.is_none();
                render_completion_subagent_slot(slot, indicator)
            })
            .collect::<Vec<_>>();
        has_unfinished_entries |= subagent_unfinished;
        task_sections.push(format!("Subagents\n{}", lines.join("\n")));
    }

    if !task_sections.is_empty() {
        // #3089 completion footer: keep the Context line outside the S3 budget
        // so usage never disappears because a task section is noisy. The same
        // 600-byte cap applies to the combined task/subagent section.
        let clamped = clamp_completion_task_section(&task_sections.join("\n\n"));
        // #3391: a terminal mark counts as rendered only if its full line
        // survived the clamp. Tasks render first, so the task lines are the
        // prefix of `clamped` up to the first blank separator line.
        terminal_task_lines.retain(|line| {
            clamped
                .lines()
                .take_while(|l| !l.is_empty())
                .any(|l| l == line)
        });
        sections.push(clamped);
    }

    CompletionFooterRender {
        block: (!sections.is_empty()).then(|| sections.join("\n\n")),
        has_unfinished_entries,
        terminal_task_lines,
    }
}

fn render_completion_task_tool_slot(slot: &TaskToolSlot, indicator: &str) -> (String, bool) {
    let (marker, unfinished) = completion_task_marker(slot.status.as_deref(), indicator);
    let base = render_task_tool_slot(slot);
    let line = if slot.background && task_tool_terminal_marker(slot.status.as_deref()).is_some() {
        base
    } else if marker.is_empty() {
        base
    } else {
        format!("{base} {marker}")
    };
    (truncate_chars(&line, EVENT_LINE_MAX_CHARS), unfinished)
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

fn clamp_completion_task_section(task_section: &str) -> String {
    if task_section.len() <= SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES {
        return task_section.to_string();
    }

    const TRUNCATION_MARKER: &str = "…";
    let lines: Vec<&str> = task_section.lines().collect();
    for keep_count in (1..=lines.len()).rev() {
        let prefix = lines[..keep_count].join("\n");
        let candidate = format!("{prefix}\n{TRUNCATION_MARKER}");
        if candidate.len() <= SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES {
            return candidate;
        }
    }

    let first_line = lines.first().copied().unwrap_or_default();
    let first_line_budget = SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES
        .saturating_sub(TRUNCATION_MARKER.len())
        .saturating_sub(1);
    let safe_end =
        crate::services::discord::formatting::floor_char_boundary(first_line, first_line_budget);
    if safe_end == 0 {
        TRUNCATION_MARKER.to_string()
    } else {
        format!("{}\n{TRUNCATION_MARKER}", &first_line[..safe_end])
    }
}
