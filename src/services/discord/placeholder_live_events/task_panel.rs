use super::common::{
    EVENT_LINE_MAX_CHARS, TASK_PANEL_LINE_MAX_CHARS, escape_status_panel_markdown,
    first_content_line, sanitized_tool_name, truncate_chars, truncate_chars_with_marker,
};

const DISPATCH_ID_SHORT_LEN: usize = 8;
const TASK_PANEL_TITLE_MAX_CHARS: usize = 60;

/// #3473: max age a background task footer slot may sit "in progress" before the
/// turn-boundary reconciliation force-aborts it. A slot whose terminal
/// notification never arrives (dcserver restart / `/compact` rebinds tool ids)
/// would otherwise render its spinner forever. 30 minutes comfortably exceeds any
/// legitimate background job's silent stretch while still bounding the stuck-slot
/// lifetime to one turn boundary past the TTL.
pub(super) const STUCK_BACKGROUND_TASK_TTL: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskPanelSnapshot {
    pub(super) dispatch_id: String,
    pub(super) card_id: Option<String>,
    pub(super) dispatch_type: Option<String>,
    pub(super) owner_instance_id: Option<String>,
    pub(super) card_title: Option<String>,
    pub(super) dispatch_title: Option<String>,
    pub(super) github_issue_number: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub(in crate::services::discord) struct TaskPanelInfo<'a> {
    pub dispatch_id: &'a str,
    pub card_id: Option<&'a str>,
    pub dispatch_type: Option<&'a str>,
    pub owner_instance_id: Option<&'a str>,
    pub card_title: Option<&'a str>,
    pub dispatch_title: Option<&'a str>,
    pub github_issue_number: Option<i64>,
}

pub(super) fn clean_task_panel_value(raw: &str) -> String {
    first_content_line(raw)
}

pub(super) fn render_task_panel_line(task: &TaskPanelSnapshot) -> String {
    let short_id = short_dispatch_id(&task.dispatch_id);
    let title = task
        .card_title
        .as_deref()
        .or(task.dispatch_title.as_deref())
        .map(|value| truncate_chars(value, TASK_PANEL_TITLE_MAX_CHARS));

    let mut parts: Vec<String> = Vec::new();
    parts.push("Task     ".to_string());

    if let Some(dispatch_type) = task.dispatch_type.as_deref() {
        parts.push(escape_status_panel_markdown(dispatch_type));
    }

    match (task.github_issue_number, title.as_deref()) {
        (Some(issue_number), Some(title)) => {
            parts.push(format!(
                "gh#{issue_number} \"{}\"",
                escape_status_panel_markdown(title)
            ));
            parts.push(format!("dsp #{}", escape_status_panel_markdown(&short_id)));
        }
        (Some(issue_number), None) => {
            parts.push(format!("gh#{issue_number}"));
            parts.push(format!("dsp #{}", escape_status_panel_markdown(&short_id)));
        }
        (None, Some(title)) => {
            parts.push(format!("\"{}\"", escape_status_panel_markdown(title)));
            parts.push(format!("#{}", escape_status_panel_markdown(&short_id)));
        }
        (None, None) => {
            parts.push(format!(
                "dispatch #{}",
                escape_status_panel_markdown(&task.dispatch_id)
            ));
            if let Some(card_id) = task.card_id.as_deref() {
                parts.push(format!("card #{}", escape_status_panel_markdown(card_id)));
            }
        }
    }

    let header = parts.remove(0);
    let body = parts.join(" · ");
    let line = if body.is_empty() {
        header.trim_end().to_string()
    } else {
        format!("{header} {body}")
    };
    truncate_chars(&line, TASK_PANEL_LINE_MAX_CHARS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskToolSlot {
    pub(super) name: String,
    pub(super) task_id: Option<String>,
    pub(super) summary: Option<String>,
    pub(super) status: Option<String>,
    pub(super) tool_use_id: Option<String>,
    pub(super) background: bool,
    /// #3391: monotonic per-channel-entry slot id assigned at creation and never
    /// reused. Backs slot-identity eviction so two slots that render the same
    /// terminal line stay distinct (string-identity eviction collided them).
    pub(super) ordinal: u64,
    /// #3473: monotonic creation instant, set at upsert. The turn-boundary
    /// reconciliation force-aborts a background slot older than
    /// `STUCK_BACKGROUND_TASK_TTL` whose terminal notification never arrived.
    pub(super) created_at: std::time::Instant,
}

pub(super) fn clean_task_tool_value(raw: impl AsRef<str>) -> Option<String> {
    let value = first_content_line(raw.as_ref());
    (!value.is_empty()).then_some(value)
}

pub(super) fn upsert_task_tool_slot(
    slots: &mut Vec<TaskToolSlot>,
    next_ordinal: &mut u64,
    name: String,
    task_id: Option<String>,
    summary: Option<String>,
    status: Option<String>,
) {
    let task_id = task_id.and_then(clean_task_tool_value);
    let summary = summary.and_then(clean_task_tool_value);
    let status = status.and_then(clean_task_tool_value);
    if let Some(task_id_value) = task_id.as_deref()
        && let Some(slot) = slots
            .iter_mut()
            .rev()
            .find(|slot| slot.task_id.as_deref() == Some(task_id_value))
    {
        slot.name = name;
        if summary.is_some() {
            slot.summary = summary;
        }
        if status.is_some() {
            slot.status = status;
        }
        return;
    }

    slots.push(TaskToolSlot {
        name,
        task_id,
        summary,
        status,
        tool_use_id: None,
        background: false,
        ordinal: take_slot_ordinal(next_ordinal),
        created_at: std::time::Instant::now(),
    });
    trim_task_tool_slots(slots);
}

pub(super) fn upsert_background_task_tool_slot(
    slots: &mut Vec<TaskToolSlot>,
    next_ordinal: &mut u64,
    name: String,
    summary: String,
    tool_use_id: String,
) {
    let Some(tool_use_id) = clean_task_tool_value(tool_use_id) else {
        return;
    };
    let summary = clean_task_tool_value(summary).unwrap_or_else(|| "Bash".to_string());
    if let Some(slot) = slots
        .iter_mut()
        .rev()
        .find(|slot| slot.background && slot.tool_use_id.as_deref() == Some(&tool_use_id))
    {
        slot.name = name;
        slot.summary = Some(summary);
        return;
    }

    slots.push(TaskToolSlot {
        name,
        task_id: None,
        summary: Some(summary),
        status: None,
        tool_use_id: Some(tool_use_id),
        background: true,
        ordinal: take_slot_ordinal(next_ordinal),
        created_at: std::time::Instant::now(),
    });
    trim_task_tool_slots(slots);
}

/// #3391: hands out the next never-reused per-channel-entry slot ordinal. The
/// counter only ever advances, so a trimmed/evicted slot's ordinal cannot be
/// minted again within the same channel entry.
pub(super) fn take_slot_ordinal(next_ordinal: &mut u64) -> u64 {
    let ordinal = *next_ordinal;
    *next_ordinal = next_ordinal.saturating_add(1);
    ordinal
}

pub(super) fn finish_background_task_tool_slot(
    slots: &mut [TaskToolSlot],
    tool_use_id: &str,
    success: bool,
) {
    let Some(tool_use_id) = clean_task_tool_value(tool_use_id) else {
        return;
    };
    if let Some(slot) = slots
        .iter_mut()
        .rev()
        .find(|slot| slot.background && slot.tool_use_id.as_deref() == Some(&tool_use_id))
    {
        slot.status = Some(if success { "completed" } else { "failed" }.to_string());
    }
}

pub(super) fn render_task_tool_slot(slot: &TaskToolSlot) -> String {
    let label = sanitized_tool_name(&slot.name).unwrap_or_else(|| "Task".to_string());
    let mut detail_parts = Vec::new();
    if let Some(task_id) = slot.task_id.as_deref() {
        detail_parts.push(escape_status_panel_markdown(task_id));
    }
    if let Some(summary) = slot.summary.as_deref() {
        if slot.task_id.as_deref() != Some(summary) && summary != label {
            detail_parts.push(escape_status_panel_markdown(summary));
        }
    }
    if !slot.background
        && let Some(status) = slot.status.as_deref()
    {
        detail_parts.push(escape_status_panel_markdown(status));
    }

    let line = if detail_parts.is_empty() {
        format!("└ {label}")
    } else {
        format!("└ {label} {}", detail_parts.join(" · "))
    };
    // #3391: reserve marker width then append, so a terminal background slot's
    // line always ENDS WITH its ✓/✗ even when the description is long enough
    // that a post-append truncation would have swallowed the mark. Non-terminal
    // lines keep their plain char truncation.
    if slot.background
        && let Some(marker) = task_tool_terminal_marker(slot.status.as_deref())
    {
        truncate_chars_with_marker(&line, marker, EVENT_LINE_MAX_CHARS)
    } else {
        truncate_chars(&line, EVENT_LINE_MAX_CHARS)
    }
}

pub(super) fn task_tool_terminal_marker(status: Option<&str>) -> Option<&'static str> {
    let status = status.map(str::trim).filter(|value| !value.is_empty())?;
    let normalized = status.to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "completed" | "complete" | "done" | "success" | "succeeded" | "ok"
    ) || normalized.contains("complete")
        || normalized.contains("success")
        || normalized.contains("done")
    {
        Some("✓")
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
        Some("✗")
    } else {
        None
    }
}

pub(super) fn task_tool_slot_is_unfinished_background(slot: &TaskToolSlot) -> bool {
    slot.background && task_tool_terminal_marker(slot.status.as_deref()).is_none()
}

/// #3473: at a turn boundary, force any background task slot that is still
/// unfinished AND older than `STUCK_BACKGROUND_TASK_TTL` to a terminal `aborted`
/// status. Its terminal notification never arrived (dcserver restart / `/compact`
/// rebinds tool ids), so it would otherwise render ⏳ forever; marking it terminal
/// makes it render ✗ and become eligible for delivered-terminal eviction on the
/// next ack cycle. Returns the number of slots reconciled (for observability).
/// `now` is injected so the reconciliation is unit-testable.
pub(super) fn force_abort_stuck_background_task_slots(
    slots: &mut [TaskToolSlot],
    now: std::time::Instant,
) -> usize {
    let mut aborted = 0usize;
    for slot in slots.iter_mut() {
        if task_tool_slot_is_unfinished_background(slot)
            && now.saturating_duration_since(slot.created_at) >= STUCK_BACKGROUND_TASK_TTL
        {
            slot.status = Some("aborted".to_string());
            aborted += 1;
        }
    }
    aborted
}

/// #3391: a task slot carries a terminal mark (✓/✗) iff its status maps to one.
/// Matches the `unfinished == false` branch of `completion_task_marker`, so a
/// slot is "still terminal at evict time" exactly when this holds.
pub(super) fn task_tool_slot_is_terminal(slot: &TaskToolSlot) -> bool {
    task_tool_terminal_marker(slot.status.as_deref()).is_some()
}

/// #4093: a task slot is "in progress" — the only kind the LIVE Tasks panel now
/// renders — iff it is NOT terminal (carries no ✓/✗ mark). Terminal slots
/// (completed / failed) are hidden immediately so finished work no longer masks
/// active work until it falls out of the 10-slot window.
///
/// `status == None` is treated as IN PROGRESS, not "done": a freshly-created
/// foreground task (e.g. `TaskCreate`) carries no status until its first update,
/// and a background (bash) slot keeps `status == None` for its whole running
/// life until the terminal notification sets `completed`/`failed`. Excluding
/// `None` would hide brand-new and long-running tasks mid-flight, so the filter
/// keys on terminal-ness alone.
///
/// This gates the LIVE panel only. The completion footer deliberately still
/// renders terminal slots — its ✓/✗ turn-end result summary and the #3391
/// delivered-terminal eviction both depend on completed rows being emitted — so
/// it must not use this predicate.
pub(super) fn task_tool_slot_is_in_progress(slot: &TaskToolSlot) -> bool {
    !task_tool_slot_is_terminal(slot)
}

/// #4093: renders the LIVE status panel's `Tasks` section for `tasks`, or `None`
/// when nothing should render. Only in-progress slots are shown (completed /
/// failed rows are hidden so they can never mask active work), newest first,
/// capped at `STATUS_PANEL_TASK_LIMIT` over the FILTERED set. Returns `None` when
/// no in-progress task survives so the caller emits no dangling `Tasks` header.
/// Colocated here (not in `status_panel.rs`) so task-slot rendering concerns live
/// with the task-slot model; the completion footer keeps its own terminal-aware
/// task rendering.
///
/// #4093 후속 (#4367): the pre-existing #3404 live terminal-slot compaction call
/// is removed. `compact_live_panel_terminal_lines` classifies a line as terminal
/// by TEXT (`ends_with('✓'|'✗')`); once this section is filtered to in-progress
/// slots, no genuine terminal line can reach it, so its only possible matches are
/// FALSE POSITIVES — a running slot whose desc/recent text happens to end with a
/// ✓/✗ glyph — which would have wrongly hidden in-progress rows behind a
/// `… (+N completed)` summary (the #4367 bug inverted). Terminals are hidden
/// outright now, so capping how many terminal rows render is moot.
pub(super) fn render_live_tasks_section(tasks: &[TaskToolSlot]) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }
    let lines = tasks
        .iter()
        .rev()
        .filter(|slot| task_tool_slot_is_in_progress(slot))
        .take(super::common::STATUS_PANEL_TASK_LIMIT)
        .map(render_task_tool_slot)
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| format!("Tasks\n{}", lines.join("\n")))
}

/// #3391: stable slot identity for delivered-terminal eviction. Background
/// tasks key on their `tool_use_id`, Task-tool slots on their `task_id`, and
/// any slot lacking both falls back to the never-reused `ordinal`. The ordinal
/// alone is unique within a channel entry, so the id/task_id preference only
/// reflects the slot's primary handle without weakening uniqueness.
pub(super) fn task_tool_slot_identity(slot: &TaskToolSlot) -> super::completion_footer::SlotKey {
    use super::completion_footer::SlotKey;
    if let Some(tool_use_id) = slot.tool_use_id.as_deref() {
        SlotKey::ToolUseId(tool_use_id.to_string())
    } else if let Some(task_id) = slot.task_id.as_deref() {
        SlotKey::TaskId(task_id.to_string())
    } else {
        SlotKey::Ordinal(slot.ordinal)
    }
}

fn short_dispatch_id(dispatch_id: &str) -> String {
    dispatch_id.chars().take(DISPATCH_ID_SHORT_LEN).collect()
}

fn trim_task_tool_slots(slots: &mut Vec<TaskToolSlot>) {
    if slots.len() > super::common::STATUS_PANEL_TASK_LIMIT {
        let excess = slots.len() - super::common::STATUS_PANEL_TASK_LIMIT;
        slots.drain(0..excess);
    }
}
