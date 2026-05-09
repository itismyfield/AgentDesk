use std::collections::VecDeque;
use std::sync::Mutex;

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use crate::services::agent_protocol::{StatusEvent, StatusTodoItem};
use crate::services::provider::ProviderKind;

mod common;
mod context_panel;
mod recent_events;
mod session_panel;
mod status_events;
mod task_panel;

#[cfg(test)]
mod tests;

use common::{
    CHANNEL_EVENT_CAPACITY, EVENT_LINE_MAX_CHARS, STATUS_PANEL_MAX_CHARS,
    STATUS_PANEL_SUBAGENT_LIMIT, STATUS_PANEL_TODO_LIMIT, escape_status_panel_markdown,
    normalize_summary, sanitized_tool_name, tool_prefix, truncate_chars,
};
use context_panel::{ContextPanelSnapshot, render_context_panel_line};
use recent_events::render_events;
use session_panel::{SessionPanelSnapshot, render_session_panel_line};
use status_events::{is_schedule_wakeup_tool, parse_eta_secs};
use task_panel::{TaskPanelSnapshot, clean_task_panel_value, render_task_panel_line};

pub(in crate::services::discord) use recent_events::RecentPlaceholderEvent;
pub(in crate::services::discord) use status_events::{
    status_events_from_task_notification, status_events_from_tool_result,
    status_events_from_tool_use,
};

pub(in crate::services::discord) use recent_events::events_from_json;
pub(in crate::services::discord) use status_events::status_events_from_json;

#[derive(Debug, Default)]
pub(in crate::services::discord) struct PlaceholderLiveEvents {
    by_channel: dashmap::DashMap<ChannelId, Mutex<VecDeque<RecentPlaceholderEvent>>>,
    status_by_channel: dashmap::DashMap<ChannelId, Mutex<StatusPanelState>>,
}

impl PlaceholderLiveEvents {
    pub(in crate::services::discord) fn clear_channel(&self, channel_id: ChannelId) {
        self.by_channel.remove(&channel_id);
        self.status_by_channel.remove(&channel_id);
    }

    pub(in crate::services::discord) fn push_event(
        &self,
        channel_id: ChannelId,
        event: RecentPlaceholderEvent,
    ) {
        let entry = self
            .by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(VecDeque::with_capacity(CHANNEL_EVENT_CAPACITY)));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.len() >= CHANNEL_EVENT_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(event);
    }

    pub(in crate::services::discord) fn push_many<I>(&self, channel_id: ChannelId, events: I)
    where
        I: IntoIterator<Item = RecentPlaceholderEvent>,
    {
        for event in events {
            self.push_event(channel_id, event);
        }
    }

    pub(in crate::services::discord) fn render_block(
        &self,
        channel_id: ChannelId,
    ) -> Option<String> {
        let entry = self.by_channel.get(&channel_id)?;
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        render_events(guard.iter())
    }

    pub(in crate::services::discord) fn push_status_event(
        &self,
        channel_id: ChannelId,
        event: StatusEvent,
    ) {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.apply(event);
    }

    pub(in crate::services::discord) fn push_status_events<I>(
        &self,
        channel_id: ChannelId,
        events: I,
    ) where
        I: IntoIterator<Item = StatusEvent>,
    {
        for event in events {
            self.push_status_event(channel_id, event);
        }
    }

    pub(in crate::services::discord) fn set_session_panel_lifecycle_event(
        &self,
        channel_id: ChannelId,
        kind: &str,
        details: &Value,
    ) -> bool {
        let snapshot = SessionPanelSnapshot::from_lifecycle_event(kind, details);
        self.set_session_panel_snapshot(channel_id, snapshot)
    }

    fn set_session_panel_snapshot(
        &self,
        channel_id: ChannelId,
        snapshot: Option<SessionPanelSnapshot>,
    ) -> bool {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.session == snapshot {
            return false;
        }
        guard.session = snapshot;
        true
    }

    pub(in crate::services::discord) fn set_task_panel_info(
        &self,
        channel_id: ChannelId,
        dispatch_id: &str,
        card_id: Option<&str>,
        dispatch_type: Option<&str>,
        owner_instance_id: Option<&str>,
    ) -> bool {
        let dispatch_id = clean_task_panel_value(dispatch_id);
        if dispatch_id.is_empty() {
            return self.set_task_panel_snapshot(channel_id, None);
        }
        let clean_optional = |value: Option<&str>| {
            value
                .map(clean_task_panel_value)
                .filter(|value| !value.is_empty())
        };
        self.set_task_panel_snapshot(
            channel_id,
            Some(TaskPanelSnapshot {
                dispatch_id,
                card_id: clean_optional(card_id),
                dispatch_type: clean_optional(dispatch_type),
                owner_instance_id: clean_optional(owner_instance_id),
            }),
        )
    }

    fn set_task_panel_snapshot(
        &self,
        channel_id: ChannelId,
        snapshot: Option<TaskPanelSnapshot>,
    ) -> bool {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.task == snapshot {
            return false;
        }
        guard.task = snapshot;
        true
    }

    pub(in crate::services::discord) fn set_context_panel_usage(
        &self,
        channel_id: ChannelId,
        input_tokens: u64,
        cache_create_tokens: u64,
        cache_read_tokens: u64,
        context_window_tokens: u64,
        compact_percent: u64,
    ) -> bool {
        if context_window_tokens == 0 {
            return false;
        }
        self.set_context_panel_snapshot(
            channel_id,
            Some(ContextPanelSnapshot {
                input_tokens,
                cache_create_tokens,
                cache_read_tokens,
                context_window_tokens,
                compact_percent,
            }),
        )
    }

    fn set_context_panel_snapshot(
        &self,
        channel_id: ChannelId,
        snapshot: Option<ContextPanelSnapshot>,
    ) -> bool {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.context == snapshot {
            return false;
        }
        guard.context = snapshot;
        true
    }

    pub(in crate::services::discord) fn render_status_panel(
        &self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        started_at_unix: i64,
    ) -> String {
        let snapshot = self
            .status_by_channel
            .get(&channel_id)
            .map(|entry| {
                entry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            })
            .unwrap_or_default();
        render_status_panel(
            snapshot,
            self.render_block(channel_id),
            provider,
            started_at_unix,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SubagentSlot {
    subagent_type: String,
    desc: String,
    recent: Option<String>,
    finished: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DerivedStatus {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletedKind {
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
struct StatusPanelState {
    status: DerivedStatus,
    session: Option<SessionPanelSnapshot>,
    task: Option<TaskPanelSnapshot>,
    context: Option<ContextPanelSnapshot>,
    todos: Vec<StatusTodoItem>,
    subagents: Vec<SubagentSlot>,
}

impl StatusPanelState {
    fn apply(&mut self, event: StatusEvent) {
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
            StatusEvent::ToolEnd { success } => {
                if let Some(slot) = self
                    .subagents
                    .iter_mut()
                    .rev()
                    .find(|slot| slot.finished.is_none())
                {
                    slot.finished = Some(success);
                }
                self.status = DerivedStatus::Running;
            }
            StatusEvent::SubagentStart {
                subagent_type,
                desc,
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
            StatusEvent::SubagentEnd { success } => {
                if let Some(slot) = self
                    .subagents
                    .iter_mut()
                    .rev()
                    .find(|slot| slot.finished.is_none())
                {
                    slot.finished = Some(success);
                }
                self.status = DerivedStatus::Running;
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
}

fn render_status_panel(
    snapshot: StatusPanelState,
    live_block: Option<String>,
    provider: &ProviderKind,
    started_at_unix: i64,
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
        .and_then(render_context_panel_line)
    {
        sections.push(context_line);
    }

    if !matches!(provider, ProviderKind::Codex) && !snapshot.todos.is_empty() {
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

    let cluster_config = &crate::config::load_graceful().cluster;
    let cluster_enabled = cluster_config.enabled;
    let local_instance_id = cluster_config.instance_id.clone();
    let recent_header = render_recent_section_header(
        snapshot.task.as_ref(),
        cluster_enabled,
        local_instance_id.as_deref(),
    );
    let recent_section = live_block
        .filter(|block| !block.trim().is_empty())
        .map(|block| format!("{recent_header}\n{block}"));

    if let Some(recent) = recent_section.as_ref() {
        let mut with_recent = sections.clone();
        with_recent.push(recent.clone());
        let joined = with_recent.join("\n\n");
        if joined.chars().count() <= STATUS_PANEL_MAX_CHARS {
            return joined;
        }
    }

    truncate_chars(&sections.join("\n\n"), STATUS_PANEL_MAX_CHARS)
}

fn render_recent_section_header(
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
