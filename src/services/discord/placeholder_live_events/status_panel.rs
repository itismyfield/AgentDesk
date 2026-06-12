use crate::services::agent_protocol::{StatusEvent, StatusTodoItem, SubagentSummary};
use crate::services::provider::ProviderKind;

use super::common::{
    EVENT_LINE_MAX_CHARS, STATUS_PANEL_MAX_CHARS, STATUS_PANEL_SUBAGENT_LIMIT,
    STATUS_PANEL_TASK_LIMIT, STATUS_PANEL_TODO_LIMIT, STATUS_PANEL_WORKFLOW_LIMIT,
    escape_status_panel_markdown, normalize_summary, sanitized_tool_name, tool_prefix,
    truncate_chars,
};
use super::context_panel::{ContextPanelSnapshot, render_context_panel_line};
use super::session_panel::{SessionPanelSnapshot, render_session_panel_line};
use super::status_events::{is_schedule_wakeup_tool, parse_eta_secs};
use super::subagent_summary::render_subagent_done_summary;
use super::task_panel::{
    TaskPanelSnapshot, TaskToolSlot, finish_background_task_tool_slot, render_task_panel_line,
    render_task_tool_slot, take_slot_ordinal, task_tool_slot_is_unfinished_background,
    upsert_background_task_tool_slot, upsert_task_tool_slot,
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
    tool_use_id: Option<String>,
    /// #3086: TUI-parity accounting from the finishing `SubagentEnd`; drives the
    /// `Done (N tools · M tokens · Xs)` summary on the render line.
    summary: Option<SubagentSummary>,
    /// `true` when launched with `run_in_background`. Its immediate Task result
    /// is only a launch ack (the slot keeps running), so an ack-only
    /// `SubagentEnd` must NOT mark it ✓ — only a genuine completion finalizes it.
    background: bool,
    /// #3391: monotonic, never-reused per-entry slot id (mirrors
    /// `TaskToolSlot::ordinal`) backing slot-identity subagent eviction.
    ordinal: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DerivedStatus {
    Running,
    MonitorWait,
    ScheduleWakeup(Option<u64>),
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

impl Default for DerivedStatus {
    fn default() -> Self {
        Self::Running
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
    // #3391: only-advancing counter minting never-reused task/subagent ordinals.
    next_slot_ordinal: u64,
}

impl StatusPanelState {
    /// #3087: on a true session boundary (provider session id delta), clears the
    /// per-session content slots (subagents/tasks/todos/workflows) and resets the
    /// derived status to `Running`, while PRESERVING the context/token usage and
    /// session-panel snapshots. The ordinal counter is preserved (never cleared).
    pub(super) fn reset_session_content(&mut self) {
        self.status = DerivedStatus::Running;
        self.todos.clear();
        self.tasks.clear();
        self.subagents.clear();
        self.workflows.clear();
    }

    pub(super) fn reset_turn_content_preserving_unfinished_footer_residuals(&mut self) -> bool {
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
                let desc = desc
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "subagent".to_string());
                let subagent_type = subagent_type
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "Task".to_string());
                if background
                    && let Some(id) = tool_use_id.as_deref().filter(|id| !id.trim().is_empty())
                    && let Some(slot) = self.subagents.iter_mut().rev().find(|slot| {
                        slot.background
                            && slot.finished.is_none()
                            && slot.tool_use_id.as_deref() == Some(id)
                    })
                {
                    slot.subagent_type = subagent_type;
                    slot.desc = desc.clone();
                    self.status = DerivedStatus::SubagentRunning { desc };
                    return;
                }
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
                // #3084: close the slot whose Task tool-use id matches the
                // result (pairs a long-running subagent to its own result among
                // parallels); fall back to the first unfinished slot only when
                // no id is available or matches.
                let id = tool_use_id.as_deref();
                let matched = id.and_then(|id| {
                    self.subagents.iter().rposition(|slot| {
                        slot.finished.is_none() && slot.tool_use_id.as_deref() == Some(id)
                    })
                });
                // #3086 P1 / #3359: a summary-bearing OR id-bearing ack-only end
                // is safe only on an exact id match — never fall back to the
                // last-unfinished slot (would mis-mark an unrelated running
                // subagent, or prematurely ✓ a still-running background dispatch).
                // Id-less legacy acks may close only id-less slots.
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
                    // A background ack-only end is just a launch ack (the slot
                    // keeps running), so skip finalizing it; a genuine completion
                    // (`ack_only == false`) and any foreground end still close it.
                    let finalize = !(ack_only && slot.background);
                    if finalize {
                        slot.finished = Some(success);
                    }
                    // #3086: attach the TUI-parity Done summary, only when the
                    // event carries accounting (don't wipe a richer one).
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
            }
            StatusEvent::Heartbeat => {
                if matches!(self.status, DerivedStatus::Running) {
                    self.status = DerivedStatus::Running;
                }
            }
        }
    }

    /// #3204/#3198: routes a running subagent's live step onto its slot's recent
    /// line. Prefers the UNFINISHED slot whose Task id matches; an id-bearing
    /// activity matching no slot is dropped (never mis-routed/resurrected).
    fn set_subagent_activity(&mut self, tool_use_id: Option<String>, summary: String) {
        let id = tool_use_id.as_deref();
        let target =
            match id {
                Some(id) => self.subagents.iter_mut().rev().find(|slot| {
                    slot.finished.is_none() && slot.tool_use_id.as_deref() == Some(id)
                }),
                None => self
                    .subagents
                    .iter_mut()
                    .rev()
                    .find(|slot| slot.finished.is_none()),
            };
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
    live_block: Option<String>,
    provider: &ProviderKind,
    started_at_unix: i64,
    _heartbeat_at_unix: i64,
) -> String {
    let header_status = if matches!(provider, ProviderKind::Codex)
        && matches!(snapshot.status, DerivedStatus::SubagentRunning { .. })
    {
        DerivedStatus::Running
    } else {
        snapshot.status.clone()
    };
    let mut sections = vec![format!(
        "{} — {} (<t:{started_at_unix}:R>)",
        render_derived_status(&header_status),
        provider.as_str()
    )];

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

    if !snapshot.tasks.is_empty() {
        let lines = snapshot
            .tasks
            .iter()
            .rev()
            .take(STATUS_PANEL_TASK_LIMIT)
            .map(render_task_tool_slot)
            .collect::<Vec<_>>();
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

    let cluster_config = &crate::config::load_graceful().cluster;
    let cluster_enabled = cluster_config.enabled;
    let local_instance_id = cluster_config.instance_id.clone();
    let recent_header = render_recent_section_header(
        snapshot.task.as_ref(),
        cluster_enabled,
        local_instance_id.as_deref(),
    );
    let recent_section = if matches!(header_status, DerivedStatus::Completed { .. }) {
        None
    } else {
        live_block
            .filter(|block| !block.trim().is_empty())
            .map(|block| format!("{recent_header}\n{block}"))
    };
    if let Some(recent) = recent_section.as_ref() {
        let mut with_recent = sections.clone();
        with_recent.push(recent.clone());
        let joined = join_status_panel_sections(&with_recent);
        if joined.chars().count() <= STATUS_PANEL_MAX_CHARS {
            return joined;
        }
    }

    truncate_status_panel_sections(sections)
}

fn join_status_panel_sections(sections: &[String]) -> String {
    sections.join("\n\n")
}

pub(super) fn truncate_status_panel_sections(sections: Vec<String>) -> String {
    let joined = join_status_panel_sections(&sections);
    if joined.chars().count() <= STATUS_PANEL_MAX_CHARS {
        return joined;
    }
    truncate_chars(&joined, STATUS_PANEL_MAX_CHARS)
}

pub(super) fn render_recent_section_header(
    task: Option<&TaskPanelSnapshot>,
    cluster_enabled: bool,
    local_instance_id: Option<&str>,
) -> String {
    if !cluster_enabled {
        return "🖥️ Recent".to_string();
    }
    let dispatch_owner = task
        .and_then(|task| task.owner_instance_id.as_deref())
        .map(str::trim)
        .filter(|owner| !owner.is_empty());
    let owner = dispatch_owner.or_else(|| {
        local_instance_id
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
    });
    match owner {
        Some(owner) => format!("🖥️ Recent ({})", escape_status_panel_markdown(owner)),
        None => "🖥️ Recent".to_string(),
    }
}

fn render_derived_status(status: &DerivedStatus) -> String {
    match status {
        DerivedStatus::Running => "🟢 진행 중".to_string(),
        DerivedStatus::MonitorWait => "💤 monitor 대기".to_string(),
        DerivedStatus::ScheduleWakeup(Some(eta_secs)) => {
            format!("⏰ scheduled wakeup ({eta_secs}s 후)")
        }
        DerivedStatus::ScheduleWakeup(None) => "⏰ scheduled wakeup".to_string(),
        DerivedStatus::Completed {
            kind: CompletedKind::Background,
        } => "✅ **백그라운드 완료**".to_string(),
        DerivedStatus::Completed {
            kind: CompletedKind::Foreground,
        } => "✅ **응답 완료**".to_string(),
        DerivedStatus::ToolRunning { name, summary } => {
            let mut rendered = tool_prefix(name);
            if let Some(summary) = summary.as_deref().filter(|value| !value.trim().is_empty()) {
                rendered.push(' ');
                rendered.push_str(&escape_status_panel_markdown(&normalize_summary(summary)));
            }
            format!("🔧 도구 실행 중 ({})", truncate_chars(&rendered, 140))
        }
        DerivedStatus::SubagentRunning { desc } => {
            let desc = escape_status_panel_markdown(desc);
            format!("🧵 subagent 실행 중 ({})", truncate_chars(&desc, 120))
        }
        DerivedStatus::WorkflowRunning { label } => {
            let label = escape_status_panel_markdown(label);
            format!("🧬 workflow 실행 중 ({})", truncate_chars(&label, 120))
        }
    }
}

pub(super) fn render_subagent_slot(slot: &SubagentSlot) -> String {
    let marker = match slot.finished {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "",
    };
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
    // #3086: append the TUI-parity Done summary on finished slots that carry
    // accounting (`Done (N tools · M tokens · Xs)`).
    if let Some(summary) = slot
        .summary
        .as_ref()
        .filter(|_| matches!(slot.finished, Some(true)))
        .filter(|summary| !summary.is_empty())
    {
        if let Some(done) = render_subagent_done_summary(summary) {
            line.push_str(" — ");
            line.push_str(&done);
        }
    }
    if !marker.is_empty() {
        line.push(' ');
        line.push_str(marker);
    }
    truncate_chars(&line, EVENT_LINE_MAX_CHARS)
}

impl SubagentSlot {
    fn is_unfinished_background(&self) -> bool {
        self.background && self.finished.is_none()
    }

    pub(super) fn is_terminal(&self) -> bool {
        self.finished.is_some() // #3391: terminal (✓/✗) once `finished` is set.
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
