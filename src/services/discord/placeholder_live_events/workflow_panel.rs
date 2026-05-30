use super::common::{
    EVENT_LINE_MAX_CHARS, STATUS_PANEL_WORKFLOW_AGENT_LIMIT, STATUS_PANEL_WORKFLOW_LIMIT,
    STATUS_PANEL_WORKFLOW_PHASE_LIMIT, escape_status_panel_markdown, normalize_summary,
    truncate_chars,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkflowSlot {
    pub(super) task_id: Option<String>,
    pub(super) name: Option<String>,
    pub(super) phases: Vec<WorkflowPhaseSlot>,
    pub(super) agents: Vec<WorkflowAgentSlot>,
    pub(super) recent: Option<String>,
    pub(super) finished: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkflowPhaseSlot {
    pub(super) index: u64,
    pub(super) title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkflowAgentSlot {
    pub(super) index: u64,
    pub(super) label: String,
    pub(super) phase_index: Option<u64>,
    pub(super) phase_title: Option<String>,
    pub(super) state: String,
}

pub(super) fn upsert_workflow_phase(
    phases: &mut Vec<WorkflowPhaseSlot>,
    index: u64,
    title: String,
) {
    let title = normalize_summary(&title);
    if title.is_empty() {
        return;
    }
    if let Some(phase) = phases.iter_mut().find(|phase| phase.index == index) {
        phase.title = title;
    } else {
        phases.push(WorkflowPhaseSlot { index, title });
        phases.sort_by_key(|phase| phase.index);
    }
}

pub(super) fn upsert_workflow_agent(
    agents: &mut Vec<WorkflowAgentSlot>,
    mut next: WorkflowAgentSlot,
) {
    next.label = normalize_summary(&next.label);
    next.phase_title = next
        .phase_title
        .map(|title| normalize_summary(&title))
        .filter(|title| !title.is_empty());
    next.state = normalize_summary(&next.state);
    if next.label.is_empty() {
        return;
    }
    if let Some(agent) = agents.iter_mut().find(|agent| {
        agent.index == next.index
            && agent.phase_index == next.phase_index
            && agent.label == next.label
    }) {
        *agent = next;
    } else {
        agents.push(next);
        agents.sort_by_key(|agent| (agent.phase_index.unwrap_or(u64::MAX), agent.index));
    }
}

pub(super) fn workflow_status_label(slot: &WorkflowSlot) -> String {
    slot.agents
        .iter()
        .rev()
        .find(|agent| !workflow_agent_is_terminal(&agent.state))
        .map(workflow_agent_label)
        .or_else(|| slot.agents.last().map(workflow_agent_label))
        .or_else(|| slot.name.clone())
        .unwrap_or_else(|| "workflow".to_string())
}

fn workflow_agent_label(agent: &WorkflowAgentSlot) -> String {
    agent
        .phase_title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|phase| format!("{phase}: {}", agent.label))
        .unwrap_or_else(|| agent.label.clone())
}

pub(super) fn render_workflow_slot(slot: &WorkflowSlot) -> Vec<String> {
    let mut lines = Vec::new();
    let marker = workflow_finished_marker(slot.finished);
    let name = slot
        .name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("workflow");
    let mut header = format!("└ {}", escape_status_panel_markdown(name));
    if let Some(recent) = slot
        .recent
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        header.push_str(" — ");
        header.push_str(&escape_status_panel_markdown(&normalize_summary(recent)));
    }
    if !marker.is_empty() {
        header.push(' ');
        header.push_str(marker);
    }
    lines.push(truncate_chars(&header, EVENT_LINE_MAX_CHARS));

    for agent in slot.agents.iter().take(STATUS_PANEL_WORKFLOW_AGENT_LIMIT) {
        let mut line = String::from("  ");
        line.push_str(&escape_status_panel_markdown(&workflow_agent_label(agent)));
        let marker = workflow_agent_marker(&agent.state);
        if !marker.is_empty() {
            line.push(' ');
            line.push_str(marker);
        }
        lines.push(truncate_chars(&line, EVENT_LINE_MAX_CHARS));
    }

    if slot.agents.is_empty() {
        for phase in slot.phases.iter().take(STATUS_PANEL_WORKFLOW_PHASE_LIMIT) {
            let line = format!("  {}", escape_status_panel_markdown(&phase.title));
            lines.push(truncate_chars(&line, EVENT_LINE_MAX_CHARS));
        }
    }

    lines
}

fn workflow_finished_marker(finished: Option<bool>) -> &'static str {
    match finished {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "",
    }
}

fn workflow_agent_marker(state: &str) -> &'static str {
    let state = state.trim().to_ascii_lowercase();
    if matches!(
        state.as_str(),
        "done" | "completed" | "success" | "succeeded"
    ) {
        "✓"
    } else if matches!(
        state.as_str(),
        "failed" | "error" | "aborted" | "cancelled" | "canceled" | "stopped"
    ) {
        "✗"
    } else if matches!(state.as_str(), "progress" | "running" | "active") {
        "…"
    } else {
        ""
    }
}

fn workflow_agent_is_terminal(state: &str) -> bool {
    matches!(workflow_agent_marker(state), "✓" | "✗")
}

pub(super) fn trim_workflows(slots: &mut Vec<WorkflowSlot>) {
    if slots.len() > STATUS_PANEL_WORKFLOW_LIMIT {
        let excess = slots.len() - STATUS_PANEL_WORKFLOW_LIMIT;
        slots.drain(0..excess);
    }
}

pub(super) fn trim_workflow_slot(slot: &mut WorkflowSlot) {
    if slot.phases.len() > STATUS_PANEL_WORKFLOW_PHASE_LIMIT {
        let excess = slot.phases.len() - STATUS_PANEL_WORKFLOW_PHASE_LIMIT;
        slot.phases.drain(0..excess);
    }
    if slot.agents.len() > STATUS_PANEL_WORKFLOW_AGENT_LIMIT {
        let excess = slot.agents.len() - STATUS_PANEL_WORKFLOW_AGENT_LIMIT;
        slot.agents.drain(0..excess);
    }
}
