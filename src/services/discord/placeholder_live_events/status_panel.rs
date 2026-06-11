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
    TaskPanelSnapshot, TaskToolSlot, clean_task_tool_value, render_task_panel_line,
    render_task_tool_slot,
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
    finished: Option<bool>,
    /// Task tool-use id that opened this slot. Lets `SubagentEnd` close the
    /// exact slot it belongs to (#3084) instead of the first unfinished one,
    /// which mis-attributes completion across parallel subagents.
    tool_use_id: Option<String>,
    /// TUI-parity accounting (tool count / tokens / duration) populated from the
    /// finishing `SubagentEnd` (#3086). Drives the `Done (N tools · M tokens ·
    /// Xs)` summary on the slot's render line.
    summary: Option<SubagentSummary>,
    /// `true` when this subagent was launched with `run_in_background`. Such a
    /// subagent's immediate Task `tool_result` is only a launch ack and the
    /// subagent keeps running (often outliving the launching turn), so an
    /// ack-only `SubagentEnd` must NOT mark it ✓ — only a genuine completion
    /// (summary-bearing end or a terminal task_notification) finalizes it.
    background: bool,
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
}

impl StatusPanelState {
    /// Clears the content slots that accumulate within a single provider
    /// session (subagents/tasks/todos/workflows) and resets the derived status
    /// back to `Running`, while PRESERVING the context/token usage snapshot and
    /// the session panel snapshot itself. Invoked on a true session boundary
    /// (a provider session id delta) so a freshly started session does not
    /// inherit the previous session's stale subagent/task list (#3087).
    pub(super) fn reset_session_content(&mut self) {
        self.status = DerivedStatus::Running;
        self.todos.clear();
        self.tasks.clear();
        self.subagents.clear();
        self.workflows.clear();
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
                self.subagents.push(SubagentSlot {
                    subagent_type,
                    desc: desc.clone(),
                    recent: None,
                    finished: None,
                    tool_use_id,
                    summary: None,
                    background,
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
                // #3084: prefer closing the slot whose Task tool-use id matches
                // the result. This pairs a long-running subagent to its own
                // result even when shorter foreground tools resolved in
                // between, and attributes completion to the correct slot among
                // parallel subagents. Fall back to the first unfinished slot
                // only when no id is available or no slot matches (e.g.
                // backends that cannot surface a tool-use id).
                let id = tool_use_id.as_deref();
                let matched = id.and_then(|id| {
                    self.subagents.iter().rposition(|slot| {
                        slot.finished.is_none() && slot.tool_use_id.as_deref() == Some(id)
                    })
                });
                // #3086 P1: a summary-bearing end carries accounting for ONE
                // specific subagent (identified by its `tool_use_id`). If that
                // id does not match a tracked slot, the end MUST NOT fall back
                // to the last-unfinished slot — doing so would mark an unrelated
                // running subagent Done with the wrong summary. Drop the unmatched
                // summary-bearing end entirely. A plain (no-summary) end keeps the
                // legacy fallback so #3084 id-less backends still close a slot.
                let has_summary = summary.as_ref().is_some_and(|s| !s.is_empty());
                let target = match matched {
                    Some(index) => Some(index),
                    None if has_summary && id.is_some() => None,
                    None => self
                        .subagents
                        .iter()
                        .rposition(|slot| slot.finished.is_none()),
                };
                let slot = target.map(|index| &mut self.subagents[index]);
                if let Some(slot) = slot {
                    // A background subagent's ack-only end is just a launch ack
                    // (it keeps running, often past the launching turn), so
                    // finalizing here would render a premature ✓. Skip it for
                    // background slots on ack-only ends; a genuine completion
                    // (`ack_only == false`) still closes it. Foreground finalizes
                    // on the ack as before.
                    let finalize = !(ack_only && slot.background);
                    if finalize {
                        slot.finished = Some(success);
                    }
                    // #3086: attach the TUI-parity Done summary to the closing
                    // slot. Only overwrite when the event actually carries
                    // accounting, so an id-less terminal notification does not
                    // wipe a richer summary already present on the slot.
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
                self.upsert_task_tool(name, task_id, summary, status);
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

    /// Routes a still-running subagent's live step (#3204) onto its slot's
    /// recent line. Prefers the UNFINISHED slot whose Task id matches the
    /// nested record's `parent_tool_use_id`; an id-bearing activity that matches
    /// no slot is dropped (never mis-routed). Only unfinished slots are touched,
    /// so a finished/background-finalized slot is never resurrected (#3198). The
    /// panel header is left unchanged — the subagent is already on its own line.
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

    fn upsert_task_tool(
        &mut self,
        name: String,
        task_id: Option<String>,
        summary: Option<String>,
        status: Option<String>,
    ) {
        let task_id = task_id.and_then(clean_task_tool_value);
        let summary = summary.and_then(clean_task_tool_value);
        let status = status.and_then(clean_task_tool_value);
        if let Some(task_id_value) = task_id.as_deref()
            && let Some(slot) = self
                .tasks
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

        self.tasks.push(TaskToolSlot {
            name,
            task_id,
            summary,
            status,
        });
        trim_tasks(&mut self.tasks);
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

fn render_subagent_slot(slot: &SubagentSlot) -> String {
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

fn sanitize_label(raw: &str) -> String {
    sanitized_tool_name(raw).unwrap_or_else(|| "Task".to_string())
}

fn trim_subagents(slots: &mut Vec<SubagentSlot>) {
    if slots.len() > STATUS_PANEL_SUBAGENT_LIMIT {
        let excess = slots.len() - STATUS_PANEL_SUBAGENT_LIMIT;
        slots.drain(0..excess);
    }
}

fn trim_tasks(slots: &mut Vec<TaskToolSlot>) {
    if slots.len() > STATUS_PANEL_TASK_LIMIT {
        let excess = slots.len() - STATUS_PANEL_TASK_LIMIT;
        slots.drain(0..excess);
    }
}
