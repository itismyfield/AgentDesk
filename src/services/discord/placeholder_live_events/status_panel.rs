use crate::services::agent_protocol::{StatusEvent, StatusTodoItem, SubagentSummary};
use crate::services::provider::ProviderKind;

use super::common::{
    EVENT_LINE_MAX_CHARS, STATUS_PANEL_MAX_CHARS, STATUS_PANEL_SUBAGENT_LIMIT,
    STATUS_PANEL_TASK_LIMIT, STATUS_PANEL_TODO_LIMIT, STATUS_PANEL_WORKFLOW_LIMIT,
    escape_status_panel_markdown, normalize_summary, sanitized_tool_name, truncate_chars,
    truncate_chars_with_marker,
};
use super::completion_footer::compact_live_panel_terminal_lines;
use super::context_panel::{ContextPanelSnapshot, render_context_panel_line};
use super::session_panel::{SessionPanelSnapshot, render_session_panel_line};
use super::status_events::{is_schedule_wakeup_tool, parse_eta_secs};
use super::subagent_summary::render_subagent_done_summary;
use super::task_panel::{
    TaskPanelSnapshot, TaskToolSlot, finish_background_task_tool_slot,
    force_abort_stuck_background_task_slots, render_task_panel_line, render_task_tool_slot,
    take_slot_ordinal, task_tool_slot_is_unfinished_background, upsert_background_task_tool_slot,
    upsert_task_tool_slot,
};
use super::workflow_panel::{
    WorkflowAgentSlot, WorkflowSlot, render_workflow_slot, trim_workflow_slot, trim_workflows,
    upsert_workflow_agent, upsert_workflow_phase, workflow_status_label,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubagentSlot {
    subagent_type: String,
    pub(super) desc: String,
    recent: Option<String>,
    pub(super) finished: Option<bool>,
    /// #3084: Task tool-use id that opened this slot, so `SubagentEnd` closes the
    /// exact slot among parallels instead of the first unfinished one.
    pub(super) tool_use_id: Option<String>,
    /// #3086: TUI-parity accounting from the finishing `SubagentEnd`; drives the
    /// `Done (N tools · M tokens · Xs)` summary on the render line.
    summary: Option<SubagentSummary>,
    /// `true` when launched with `run_in_background`: an ack-only `SubagentEnd`
    /// must NOT mark it ✓ (only a genuine completion finalizes it).
    background: bool,
    /// #3391: monotonic, never-reused per-entry slot id (mirrors
    /// `TaskToolSlot::ordinal`) backing slot-identity subagent eviction.
    ordinal: u64,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum DerivedStatus {
    #[default]
    Running,
    MonitorWait,
    ScheduleWakeup(Option<u64>),
    TerminalDeliveryPending,
    TerminalDeliveryUnconfirmed,
    Completed {
        kind: CompletedKind,
    },
    ToolRunning {
        name: String,
        summary: Option<String>,
    },
    SubagentRunning {
        desc: String,
    },
    WorkflowRunning {
        label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletedKind {
    Foreground,
    Background,
}

impl CompletedKind {
    fn from_background(background: bool) -> Self {
        if background {
            Self::Background
        } else {
            Self::Foreground
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct StatusPanelState {
    pub(super) status: DerivedStatus,
    pub(super) session: Option<SessionPanelSnapshot>,
    pub(super) task: Option<TaskPanelSnapshot>,
    pub(super) context: Option<ContextPanelSnapshot>,
    todos: Vec<StatusTodoItem>,
    pub(super) tasks: Vec<TaskToolSlot>,
    pub(super) subagents: Vec<SubagentSlot>,
    pub(super) workflows: Vec<WorkflowSlot>,
    next_slot_ordinal: u64, // #3391: advancing, never-reused task/subagent ordinals.
    // #3477 item 3: instant the turn entered `Completed` (None until then); vs the
    // store's `last_recent_event_at` it gates the late-batch 🖥️ Recent freshness.
    pub(super) completed_at: Option<std::time::Instant>,
    // #3811: intake-set original-request user_msg_id; drives the `요청:` deeplink
    // (`None` for headless/synthetic/voice/id-0 — no real Discord message).
    pub(super) request_user_msg_id: Option<u64>,
}

impl StatusPanelState {
    /// #3087: on a true session boundary (provider session id delta), clears the
    /// per-session content slots and resets status to `Running`, PRESERVING
    /// context/token usage + session snapshots and the ordinal counter.
    pub(super) fn reset_session_content(&mut self) {
        self.status = DerivedStatus::Running;
        self.todos.clear();
        self.tasks.clear();
        self.subagents.clear();
        self.workflows.clear();
        self.completed_at = None; // #3477 item 3: drop the stale freshness gate.
        self.request_user_msg_id = None; // #3811: new session = new request context.
    }

    pub(super) fn reset_turn_content_preserving_unfinished_footer_residuals(&mut self) -> bool {
        // #3473: turn-boundary reconciliation — force a TTL-expired stuck
        // background task to `aborted` BEFORE the retain filter so it is dropped
        // here instead of sitting ⏳ forever.
        force_abort_stuck_background_task_slots(&mut self.tasks, std::time::Instant::now());
        let tasks = self
            .tasks
            .iter()
            .filter(|slot| task_tool_slot_is_unfinished_background(slot))
            .cloned()
            .collect::<Vec<_>>();
        let subagents = self
            .subagents
            .iter()
            .filter(|slot| slot.is_unfinished_background())
            .cloned()
            .collect::<Vec<_>>();
        let has_residuals = !tasks.is_empty() || !subagents.is_empty();
        *self = StatusPanelState {
            tasks,
            subagents,
            // #3391: carry the counter so a residual ordinal is never reissued.
            next_slot_ordinal: self.next_slot_ordinal,
            request_user_msg_id: self.request_user_msg_id, // #3811: survive turn reset
            ..StatusPanelState::default()
        };
        has_residuals
    }

    pub(super) fn apply(&mut self, event: StatusEvent) {
        match event {
            StatusEvent::ToolStart { name, args_summary } => {
                if is_schedule_wakeup_tool(&name) {
                    self.status =
                        DerivedStatus::ScheduleWakeup(parse_eta_secs(args_summary.as_deref()));
                } else {
                    self.status = DerivedStatus::ToolRunning {
                        name,
                        summary: args_summary,
                    };
                }
            }
            StatusEvent::ToolEnd { success: _ } => {
                self.status = DerivedStatus::Running;
            }
            StatusEvent::SubagentStart {
                subagent_type,
                desc,
                tool_use_id,
                background,
            } => {
                // #3920: keep "was a real value provided?" BEFORE defaulting, so a
                // background-promotion re-affirmation (an async launch ack carries
                // no desc/type) never overwrites the launching slot's real
                // description with the `subagent`/`Task` placeholders.
                let provided_desc = desc.filter(|value| !value.trim().is_empty());
                let provided_type = subagent_type.filter(|value| !value.trim().is_empty());
                // A background `SubagentStart` re-affirms (and #3920: PROMOTES) the
                // still-running slot for this tool-use id. Matching ANY unfinished
                // slot — not only an already-background one — lets an async/
                // `run_in_background` Agent launch (whose async-ness is known only
                // from the launch-ack `toolUseResult`, not the tool INPUT) flip its
                // foreground-looking slot to a background subagent. That keeps it
                // alive across turn-boundary resets like a Bash `run_in_background`
                // task, instead of being dropped a turn later (#3920).
                if background
                    && let Some(id) = tool_use_id.as_deref().filter(|id| !id.trim().is_empty())
                    && let Some(slot) = self.subagents.iter_mut().rev().find(|slot| {
                        slot.finished.is_none() && slot.tool_use_id.as_deref() == Some(id)
                    })
                {
                    slot.background = true;
                    if let Some(subagent_type) = provided_type {
                        slot.subagent_type = subagent_type;
                    }
                    if let Some(desc) = provided_desc {
                        slot.desc = desc;
                    }
                    let running_desc = slot.desc.clone();
                    self.status = DerivedStatus::SubagentRunning { desc: running_desc };
                    return;
                }
                let desc = provided_desc.unwrap_or_else(|| "subagent".to_string());
                let subagent_type = provided_type.unwrap_or_else(|| "Task".to_string());
                let ordinal = take_slot_ordinal(&mut self.next_slot_ordinal);
                self.subagents.push(SubagentSlot {
                    subagent_type,
                    desc: desc.clone(),
                    recent: None,
                    finished: None,
                    tool_use_id,
                    summary: None,
                    background,
                    ordinal,
                });
                self.status = DerivedStatus::SubagentRunning { desc };
                trim_subagents(&mut self.subagents);
            }
            StatusEvent::SubagentEvent { summary } => {
                if let Some(slot) = self
                    .subagents
                    .iter_mut()
                    .rev()
                    .find(|slot| slot.finished.is_none())
                {
                    slot.recent = Some(normalize_summary(&summary));
                    self.status = DerivedStatus::SubagentRunning {
                        desc: slot.desc.clone(),
                    };
                }
            }
            StatusEvent::SubagentActivity {
                tool_use_id,
                summary,
            } => self.set_subagent_activity(tool_use_id, summary),
            StatusEvent::SubagentEnd {
                success,
                tool_use_id,
                summary,
                ack_only,
            } => {
                // #3084: close the id-matching slot (pairs a long-running subagent
                // to its own result among parallels); else first-unfinished.
                let id = tool_use_id.as_deref();
                let matched = id.and_then(|id| {
                    self.subagents.iter().rposition(|slot| {
                        slot.finished.is_none() && slot.tool_use_id.as_deref() == Some(id)
                    })
                });
                // #3086 P1 / #3359: a summary/id-bearing ack-only end is safe only
                // on an exact id match; id-less legacy acks close only id-less slots.
                let has_summary = summary.as_ref().is_some_and(|s| !s.is_empty());
                let target = match matched {
                    Some(index) => Some(index),
                    None if id.is_some() => None,
                    None if ack_only => self
                        .subagents
                        .iter()
                        .rposition(|slot| slot.finished.is_none() && slot.tool_use_id.is_none()),
                    None if has_summary && id.is_some() => None,
                    None => self
                        .subagents
                        .iter()
                        .rposition(|slot| slot.finished.is_none()),
                };
                let slot = target.map(|index| &mut self.subagents[index]);
                if let Some(slot) = slot {
                    // A background ack-only end is just a launch ack (slot keeps
                    // running); a genuine/foreground end still closes it.
                    let finalize = !(ack_only && slot.background);
                    if finalize {
                        slot.finished = Some(success);
                    }
                    // #3086: attach Done summary only when accounting present.
                    if let Some(summary) = summary.filter(|summary| !summary.is_empty()) {
                        slot.summary = Some(summary);
                    }
                }
                self.status = DerivedStatus::Running;
            }
            StatusEvent::TaskToolUpdate {
                name,
                task_id,
                summary,
                status,
            } => {
                upsert_task_tool_slot(
                    &mut self.tasks,
                    &mut self.next_slot_ordinal,
                    name,
                    task_id,
                    summary,
                    status,
                );
            }
            StatusEvent::BackgroundTaskStart {
                name,
                summary,
                tool_use_id,
            } => {
                upsert_background_task_tool_slot(
                    &mut self.tasks,
                    &mut self.next_slot_ordinal,
                    name,
                    summary,
                    tool_use_id,
                );
            }
            StatusEvent::BackgroundTaskEnd {
                tool_use_id,
                success,
            } => {
                finish_background_task_tool_slot(&mut self.tasks, &tool_use_id, success);
            }
            StatusEvent::TodoUpdate { items } => {
                self.todos = items
                    .into_iter()
                    .filter(|item| !item.content.trim().is_empty())
                    .take(STATUS_PANEL_TODO_LIMIT)
                    .collect();
            }
            StatusEvent::MonitorWait => {
                self.status = DerivedStatus::MonitorWait;
            }
            StatusEvent::ScheduleWakeup { eta_secs } => {
                self.status = DerivedStatus::ScheduleWakeup(eta_secs);
            }
            StatusEvent::WorkflowStart { task_id, name } => {
                let label = {
                    let slot = self.workflow_slot_mut(task_id.clone());
                    if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
                        slot.name = Some(normalize_summary(&name));
                    }
                    trim_workflow_slot(slot);
                    workflow_status_label(slot)
                };
                self.status = DerivedStatus::WorkflowRunning { label };
                trim_workflows(&mut self.workflows);
            }
            StatusEvent::WorkflowPhase {
                task_id,
                index,
                title,
            } => {
                let label = {
                    let slot = self.workflow_slot_mut(task_id);
                    upsert_workflow_phase(&mut slot.phases, index, title);
                    trim_workflow_slot(slot);
                    workflow_status_label(slot)
                };
                self.status = DerivedStatus::WorkflowRunning { label };
                trim_workflows(&mut self.workflows);
            }
            StatusEvent::WorkflowAgent {
                task_id,
                index,
                label,
                phase_index,
                phase_title,
                state,
            } => {
                let label = {
                    let slot = self.workflow_slot_mut(task_id);
                    upsert_workflow_agent(
                        &mut slot.agents,
                        WorkflowAgentSlot {
                            index,
                            label,
                            phase_index,
                            phase_title,
                            state,
                        },
                    );
                    trim_workflow_slot(slot);
                    workflow_status_label(slot)
                };
                self.status = DerivedStatus::WorkflowRunning { label };
                trim_workflows(&mut self.workflows);
            }
            StatusEvent::WorkflowLog { task_id, summary } => {
                {
                    let slot = self.workflow_slot_mut(task_id);
                    let summary = normalize_summary(&summary);
                    if !summary.is_empty() {
                        slot.recent = Some(summary);
                    }
                    trim_workflow_slot(slot);
                }
                trim_workflows(&mut self.workflows);
            }
            StatusEvent::WorkflowEnd {
                task_id,
                success,
                summary,
            } => {
                {
                    let slot = self.workflow_slot_mut(task_id);
                    if let Some(summary) = summary.filter(|value| !value.trim().is_empty()) {
                        slot.recent = Some(normalize_summary(&summary));
                    }
                    slot.finished = Some(success);
                    trim_workflow_slot(slot);
                }
                trim_workflows(&mut self.workflows);
                if matches!(self.status, DerivedStatus::WorkflowRunning { .. }) {
                    self.status = DerivedStatus::Running;
                }
            }
            StatusEvent::TurnCompleted { background } => {
                self.status = DerivedStatus::Completed {
                    kind: CompletedKind::from_background(background),
                };
                // #3477 item 3: stamp the late-batch freshness gate.
                self.completed_at = Some(std::time::Instant::now());
            }
            StatusEvent::Heartbeat => {
                if matches!(self.status, DerivedStatus::Running) {
                    self.status = DerivedStatus::Running;
                }
            }
        }
    }

    /// #3204/#3198: routes a running subagent's live step onto its slot's recent
    /// line (UNFINISHED id-matching slot; id-bearing no-match dropped).
    fn set_subagent_activity(&mut self, tool_use_id: Option<String>, summary: String) {
        let id = tool_use_id.as_deref();
        let target = self.subagents.iter_mut().rev().find(|slot| {
            slot.finished.is_none() && (id.is_none() || slot.tool_use_id.as_deref() == id)
        });
        if let Some(slot) = target {
            let summary = normalize_summary(&summary);
            if !summary.trim().is_empty() {
                slot.recent = Some(summary);
            }
        }
    }

    fn workflow_slot_mut(&mut self, task_id: Option<String>) -> &mut WorkflowSlot {
        let index = task_id
            .as_deref()
            .and_then(|task_id| {
                self.workflows
                    .iter()
                    .position(|slot| slot.task_id.as_deref() == Some(task_id))
            })
            .or_else(|| (task_id.is_none() && self.workflows.len() == 1).then_some(0));
        if let Some(index) = index {
            return &mut self.workflows[index];
        }
        self.workflows.push(WorkflowSlot {
            task_id,
            name: None,
            phases: Vec::new(),
            agents: Vec::new(),
            recent: None,
            finished: None,
        });
        self.workflows.last_mut().expect("workflow just pushed")
    }
}

pub(super) fn render_status_panel(
    snapshot: StatusPanelState,
    provider: &ProviderKind,
    // #3983 item 2: precomputed `마지막 업데이트 : … / 턴 시작 : …` time line (line 2).
    time_line: String,
    // #3983 item 3: precomputed `턴 트리거:` deeplink, appended as the LAST footer
    // line (or `None` for headless/synthetic/id-0 turns with no real user message).
    turn_trigger_line: Option<String>,
) -> String {
    let header_status = if matches!(provider, ProviderKind::Codex)
        && matches!(snapshot.status, DerivedStatus::SubagentRunning { .. })
    {
        DerivedStatus::Running
    } else {
        snapshot.status.clone()
    };
    // #3983: line 1 = derived-status ACTIVITY label, line 2 = relative TIME line
    // (both built in the colocated `freshness` module — status_panel.rs is at the
    // namespace cap). The pre-#3983 confidence line + `진행 중 — provider` header is
    // retired (item 2); the request anchor no longer prepends here (item 3, see the
    // trailing `턴 트리거:` push below).
    let mut sections = vec![
        super::freshness::render_activity_line(&header_status),
        time_line,
    ];

    if let Some(session) = snapshot.session.as_ref() {
        sections.push(render_session_panel_line(session, provider));
    }

    if let Some(task) = snapshot.task.as_ref() {
        sections.push(render_task_panel_line(task));
    }

    if let Some(context_line) = snapshot
        .context
        .as_ref()
        .and_then(|context| render_context_panel_line(context, provider))
    {
        sections.push(context_line);
    }

    if !snapshot.todos.is_empty() {
        let lines = snapshot
            .todos
            .iter()
            .take(STATUS_PANEL_TODO_LIMIT)
            .map(|item| {
                let content = escape_status_panel_markdown(&normalize_summary(&item.content));
                format!(
                    "- {} {}",
                    item.status.checkbox_marker(),
                    truncate_chars(&content, 110)
                )
            })
            .collect::<Vec<_>>();
        sections.push(format!("Plan\n{}", lines.join("\n")));
    }

    // #3983 item 5a: the compact 🖥️ Recent + host block is removed from the footer
    // (the terminal echo is retired from the status panel entirely).
    if !snapshot.tasks.is_empty() {
        let lines = snapshot
            .tasks
            .iter()
            .rev()
            .take(STATUS_PANEL_TASK_LIMIT)
            .map(render_task_tool_slot)
            .collect::<Vec<_>>();
        let lines = compact_live_panel_terminal_lines(&lines).map_or(lines, |(out, _)| out); // #3404 cap
        sections.push(format!("Tasks\n{}", lines.join("\n")));
    }

    if !matches!(provider, ProviderKind::Codex) && !snapshot.subagents.is_empty() {
        let lines = snapshot
            .subagents
            .iter()
            .rev()
            .take(STATUS_PANEL_SUBAGENT_LIMIT)
            .map(render_subagent_slot)
            .collect::<Vec<_>>();
        let lines = compact_live_panel_terminal_lines(&lines).map_or(lines, |(out, _)| out); // #3404 cap
        sections.push(format!("Subagents\n{}", lines.join("\n")));
    }

    if !matches!(provider, ProviderKind::Codex) && !snapshot.workflows.is_empty() {
        let lines = snapshot
            .workflows
            .iter()
            .rev()
            .take(STATUS_PANEL_WORKFLOW_LIMIT)
            .flat_map(render_workflow_slot)
            .collect::<Vec<_>>();
        if !lines.is_empty() {
            sections.push(format!("Workflow\n{}", lines.join("\n")));
        }
    }

    // #3983 item 3: the `턴 트리거:` original-request deeplink is the LAST footer
    // line (it previously prepended above the header). Absent for headless /
    // synthetic / id-0 turns that carry no real Discord user message.
    if let Some(trigger) = turn_trigger_line.filter(|line| !line.trim().is_empty()) {
        sections.push(trigger);
    }

    truncate_status_panel_sections(sections)
}

fn join_status_panel_sections(sections: &[String]) -> String {
    sections.join("\n\n")
}

/// #3394: section-wise degradation. A char cut of the JOINED panel chops a
/// trailing fenced section's ``` (rendered as literal text), so on overflow DROP
/// whole trailing sections; a lone overflowing section is fence-safe-truncated and
/// `repair_fence_parity` re-balances every return path.
pub(super) fn truncate_status_panel_sections(mut sections: Vec<String>) -> String {
    use crate::services::discord::single_message_panel::repair_fence_parity;
    while sections.len() > 1
        && join_status_panel_sections(&sections).chars().count() > STATUS_PANEL_MAX_CHARS
    {
        sections.pop();
    }
    let joined = join_status_panel_sections(&sections);
    if joined.chars().count() <= STATUS_PANEL_MAX_CHARS {
        return repair_fence_parity(&joined);
    }
    repair_fence_parity(&truncate_chars(&joined, STATUS_PANEL_MAX_CHARS))
}

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

impl SubagentSlot {
    fn is_unfinished_background(&self) -> bool {
        self.background && self.finished.is_none()
    }

    pub(super) fn is_terminal(&self) -> bool {
        self.finished.is_some() // #3391: terminal (✓/✗) once `finished` is set.
    }

    /// #3391: the ✓/✗ this slot renders (`None` while unfinished); single source for both render and the footer honesty gate.
    pub(super) fn terminal_marker(&self) -> Option<&'static str> {
        self.finished.map(|ok| if ok { "✓" } else { "✗" })
    }

    // #3391: eviction identity — launching `tool_use_id`, else `ordinal`.
    pub(super) fn identity(&self) -> super::completion_footer::SlotKey {
        use super::completion_footer::SlotKey;
        match self.tool_use_id.as_deref() {
            Some(id) => SlotKey::ToolUseId(id.to_string()),
            None => SlotKey::Ordinal(self.ordinal),
        }
    }
}

fn sanitize_label(raw: &str) -> String {
    sanitized_tool_name(raw).unwrap_or_else(|| "Task".to_string())
}

fn trim_subagents(slots: &mut Vec<SubagentSlot>) {
    if slots.len() > STATUS_PANEL_SUBAGENT_LIMIT {
        let excess = slots.len() - STATUS_PANEL_SUBAGENT_LIMIT;
        slots.drain(0..excess);
    }
}
