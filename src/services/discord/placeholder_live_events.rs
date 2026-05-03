use std::collections::VecDeque;
use std::sync::Mutex;

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use super::formatting::{canonical_tool_name, format_tool_input, redact_sensitive_for_placeholder};
use crate::db::prompt_manifests::PromptManifest;
use crate::services::agent_protocol::{StatusEvent, StatusTodoItem, StatusTodoStatus};
use crate::services::provider::ProviderKind;

const CHANNEL_EVENT_CAPACITY: usize = 20;
const EVENT_RENDER_LIMIT: usize = 5;
const EVENT_LINE_MAX_CHARS: usize = 100;
const EVENT_BLOCK_MAX_CHARS: usize = 1500;
const STATUS_PANEL_MAX_CHARS: usize = 4096;
const STATUS_PANEL_TODO_LIMIT: usize = 8;
const STATUS_PANEL_SUBAGENT_LIMIT: usize = 6;
const SESSION_PANEL_LINE_MAX_CHARS: usize = 100;
const TASK_PANEL_LINE_MAX_CHARS: usize = 140;
const CONTEXT_PANEL_LINE_MAX_CHARS: usize = 120;
const PROMPT_PANEL_LINE_MAX_CHARS: usize = 120;
const PROMPT_PANEL_SKIPPED_REASON_MAX_CHARS: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecentPlaceholderEvent {
    prefix: String,
    summary: String,
}

impl RecentPlaceholderEvent {
    pub(super) fn tool_use(name: &str, input: &str) -> Option<Self> {
        let summary = format_tool_input(name, input);
        let summary = if summary.trim().is_empty() {
            first_content_line(input)
        } else {
            summary
        };
        Self::new(tool_prefix(name), summary)
    }

    pub(super) fn tool_error(content: &str) -> Option<Self> {
        Self::new("[tool error]", content)
    }

    pub(super) fn task_notification(kind: &str, status: &str, summary: &str) -> Option<Self> {
        let prefix = match kind {
            "monitor_auto_turn" => "[Monitor]",
            "subagent" => "[Task]",
            "background" => "[background]",
            _ => "[system]",
        };
        let mut detail = first_content_line(summary);
        let status = status.trim();
        if !status.is_empty() {
            detail = if detail.is_empty() {
                status.to_string()
            } else {
                format!("{status}: {detail}")
            };
        }
        Self::new(prefix, detail)
    }

    fn new(prefix: impl Into<String>, summary: impl AsRef<str>) -> Option<Self> {
        let summary = normalize_summary(summary.as_ref());
        if summary.is_empty() {
            return None;
        }
        Some(Self {
            prefix: prefix.into(),
            summary,
        })
    }

    fn render_line(&self) -> String {
        let raw = format!("{} {}", self.prefix, self.summary);
        let sanitized = sanitize_for_code_fence(raw.trim());
        truncate_chars(&sanitized, EVENT_LINE_MAX_CHARS)
    }
}

fn sanitize_for_code_fence(raw: &str) -> String {
    raw.replace('`', "")
}

#[derive(Debug, Default)]
pub(super) struct PlaceholderLiveEvents {
    by_channel: dashmap::DashMap<ChannelId, Mutex<VecDeque<RecentPlaceholderEvent>>>,
    status_by_channel: dashmap::DashMap<ChannelId, Mutex<StatusPanelState>>,
}

impl PlaceholderLiveEvents {
    pub(super) fn clear_channel(&self, channel_id: ChannelId) {
        self.by_channel.remove(&channel_id);
        self.status_by_channel.remove(&channel_id);
    }

    pub(super) fn push_event(&self, channel_id: ChannelId, event: RecentPlaceholderEvent) {
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

    pub(super) fn push_many<I>(&self, channel_id: ChannelId, events: I)
    where
        I: IntoIterator<Item = RecentPlaceholderEvent>,
    {
        for event in events {
            self.push_event(channel_id, event);
        }
    }

    pub(super) fn render_block(&self, channel_id: ChannelId) -> Option<String> {
        let entry = self.by_channel.get(&channel_id)?;
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        render_events(guard.iter())
    }

    pub(super) fn push_status_event(&self, channel_id: ChannelId, event: StatusEvent) {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.apply(event);
    }

    pub(super) fn push_status_events<I>(&self, channel_id: ChannelId, events: I)
    where
        I: IntoIterator<Item = StatusEvent>,
    {
        for event in events {
            self.push_status_event(channel_id, event);
        }
    }

    pub(super) fn set_session_panel_lifecycle_event(
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

    pub(super) fn set_task_panel_info(
        &self,
        channel_id: ChannelId,
        dispatch_id: &str,
        card_id: Option<&str>,
        dispatch_type: Option<&str>,
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

    pub(super) fn set_context_panel_usage(
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

    pub(super) fn set_prompt_manifest(
        &self,
        channel_id: ChannelId,
        manifest: &PromptManifest,
    ) -> bool {
        let snapshot = PromptPanelSnapshot::from_manifest(manifest);
        self.set_prompt_panel_snapshot(channel_id, Some(snapshot))
    }

    fn set_prompt_panel_snapshot(
        &self,
        channel_id: ChannelId,
        snapshot: Option<PromptPanelSnapshot>,
    ) -> bool {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.prompt == snapshot {
            return false;
        }
        guard.prompt = snapshot;
        true
    }

    pub(super) fn render_status_panel(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionPanelKind {
    Fresh,
    Resumed,
    Fallback,
}

impl SessionPanelKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Resumed => "resumed",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmuxPanelState {
    Kept,
    New,
}

impl TmuxPanelState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Kept => "kept",
            Self::New => "new",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionPanelSnapshot {
    kind: SessionPanelKind,
    provider_session_id: Option<String>,
    tmux: Option<TmuxPanelState>,
}

impl SessionPanelSnapshot {
    fn from_lifecycle_event(kind: &str, details: &Value) -> Option<Self> {
        if !details.as_object().is_some_and(|object| !object.is_empty()) {
            return None;
        }

        let kind = session_panel_kind(kind, details)?;
        let provider_session_id = first_json_string(
            details,
            &[
                "provider_session_id",
                "providerSessionId",
                "raw_provider_session_id",
                "rawProviderSessionId",
                "session_id",
                "sessionId",
                "claude_session_id",
                "claudeSessionId",
            ],
        )
        .map(str::to_string);
        let tmux = parse_tmux_panel_state(details);

        Some(Self {
            kind,
            provider_session_id,
            tmux,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskPanelSnapshot {
    dispatch_id: String,
    card_id: Option<String>,
    dispatch_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextPanelSnapshot {
    input_tokens: u64,
    cache_create_tokens: u64,
    cache_read_tokens: u64,
    context_window_tokens: u64,
    compact_percent: u64,
}

impl ContextPanelSnapshot {
    fn usage_percent(&self) -> Option<u64> {
        if self.context_window_tokens == 0 {
            return None;
        }
        let used_tokens = self
            .input_tokens
            .saturating_add(self.cache_create_tokens)
            .saturating_add(self.cache_read_tokens);
        let percent = (u128::from(used_tokens) * 100) / u128::from(self.context_window_tokens);
        Some(percent.min(u128::from(u64::MAX)) as u64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptPanelSnapshot {
    profile: Option<String>,
    enabled_layers: Vec<String>,
    skipped_layers: Vec<SkippedLayerEntry>,
    total_input_tokens_est: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedLayerEntry {
    name: String,
    reason: Option<String>,
}

impl PromptPanelSnapshot {
    fn from_manifest(manifest: &PromptManifest) -> Self {
        let mut enabled_layers = Vec::new();
        let mut skipped_layers = Vec::new();
        for layer in &manifest.layers {
            if layer.enabled {
                enabled_layers.push(layer.layer_name.clone());
            } else {
                let reason = layer
                    .reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                skipped_layers.push(SkippedLayerEntry {
                    name: layer.layer_name.clone(),
                    reason,
                });
            }
        }
        Self {
            profile: manifest
                .profile
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            enabled_layers,
            skipped_layers,
            total_input_tokens_est: manifest.total_input_tokens_est.max(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DerivedStatus {
    Running,
    MonitorWait,
    ScheduleWakeup(Option<u64>),
    ToolRunning {
        name: String,
        summary: Option<String>,
    },
    SubagentRunning {
        desc: String,
    },
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
    prompt: Option<PromptPanelSnapshot>,
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

    if let Some(prompt) = snapshot.prompt.as_ref() {
        sections.push(render_prompt_panel_block(prompt));
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

    let recent_section = live_block
        .filter(|block| !block.trim().is_empty())
        .map(|block| format!("Recent\n{block}"));

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

fn render_session_panel_line(session: &SessionPanelSnapshot, provider: &ProviderKind) -> String {
    let mut parts = vec![format!("Lifecycle {}", session.kind.as_str())];
    if let Some(provider_session_id) = session
        .provider_session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(format!(
            "provider session {}",
            render_provider_session_label(provider, provider_session_id)
        ));
    }
    if let Some(tmux) = session.tmux {
        parts.push(format!("tmux {}", tmux.as_str()));
    }
    truncate_chars(&parts.join(" · "), SESSION_PANEL_LINE_MAX_CHARS)
}

fn render_task_panel_line(task: &TaskPanelSnapshot) -> String {
    let mut parts = vec![format!(
        "Task      dispatch #{}",
        escape_status_panel_markdown(&task.dispatch_id)
    )];
    if let Some(card_id) = task.card_id.as_deref() {
        parts.push(format!("card #{}", escape_status_panel_markdown(card_id)));
    }
    if let Some(dispatch_type) = task.dispatch_type.as_deref() {
        parts.push(escape_status_panel_markdown(dispatch_type));
    }
    truncate_chars(&parts.join(" · "), TASK_PANEL_LINE_MAX_CHARS)
}

fn render_context_panel_line(context: &ContextPanelSnapshot) -> Option<String> {
    let usage_percent = context.usage_percent()?;
    let icon = if usage_percent >= 85 {
        "⚠️"
    } else {
        "📦"
    };
    let mut line = format!(
        "Context   {icon} {usage_percent}% used · auto-compact {}%",
        context.compact_percent
    );
    if usage_percent >= 85 {
        line.push_str(" — 자동 압축 직전");
    } else if usage_percent >= 75 {
        line.push_str(" (임박)");
    }
    Some(truncate_chars(&line, CONTEXT_PANEL_LINE_MAX_CHARS))
}

fn render_prompt_panel_block(prompt: &PromptPanelSnapshot) -> String {
    let header_parts = [
        render_prompt_profile_label(prompt.profile.as_deref()),
        render_prompt_tokens(prompt.total_input_tokens_est),
    ];
    let header = truncate_chars(
        &format!("Prompt    {}", header_parts.join(" · ")),
        PROMPT_PANEL_LINE_MAX_CHARS,
    );

    let mut lines = vec![header];

    if !prompt.enabled_layers.is_empty() {
        let names = prompt
            .enabled_layers
            .iter()
            .map(|name| escape_status_panel_markdown(name))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(truncate_chars(
            &format!("- 활성 ({}): {}", prompt.enabled_layers.len(), names),
            PROMPT_PANEL_LINE_MAX_CHARS,
        ));
    }

    if !prompt.skipped_layers.is_empty() {
        let parts: Vec<String> = prompt
            .skipped_layers
            .iter()
            .map(|entry| match entry.reason.as_deref() {
                Some(reason) => format!(
                    "{} ({})",
                    escape_status_panel_markdown(&entry.name),
                    truncate_chars(
                        &escape_status_panel_markdown(reason),
                        PROMPT_PANEL_SKIPPED_REASON_MAX_CHARS
                    )
                ),
                None => escape_status_panel_markdown(&entry.name),
            })
            .collect();
        lines.push(truncate_chars(
            &format!(
                "- 스킵 ({}): {}",
                prompt.skipped_layers.len(),
                parts.join(", ")
            ),
            PROMPT_PANEL_LINE_MAX_CHARS,
        ));
    }

    lines.join("\n")
}

fn render_prompt_profile_label(profile: Option<&str>) -> String {
    let Some(profile) = profile.map(str::trim).filter(|value| !value.is_empty()) else {
        return "Unknown profile".to_string();
    };
    match profile.to_ascii_lowercase().as_str() {
        "full" => "Full profile".to_string(),
        "lite" => "Lite profile".to_string(),
        "review_lite" | "review-lite" => "Review lite profile".to_string(),
        other => {
            let label = other.replace(['_', '-'], " ");
            let mut chars = label.chars();
            let mut rendered = match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Unknown".to_string(),
            };
            if !rendered.to_ascii_lowercase().ends_with(" profile") {
                rendered.push_str(" profile");
            }
            rendered
        }
    }
}

fn render_prompt_tokens(total_input_tokens_est: i64) -> String {
    format!(
        "~{:.1}k input tokens",
        (total_input_tokens_est.max(0) as f64) / 1000.0
    )
}

fn render_provider_session_label(provider: &ProviderKind, session_id: &str) -> String {
    let abbreviated = abbreviate_provider_session_id(session_id);
    if abbreviated.contains('#') {
        abbreviated
    } else {
        format!("{}#{}", provider.as_str(), abbreviated)
    }
}

fn abbreviate_provider_session_id(session_id: &str) -> String {
    let trimmed = session_id.trim();
    let prefix: String = trimmed.chars().take(8).collect();
    if trimmed.chars().count() > 8 {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn session_panel_kind(kind: &str, details: &Value) -> Option<SessionPanelKind> {
    if is_fallback_session_details(details) {
        return Some(SessionPanelKind::Fallback);
    }
    match kind {
        "session_fresh" => Some(SessionPanelKind::Fresh),
        "session_resumed" => Some(SessionPanelKind::Resumed),
        "session_resume_failed_with_recovery" => Some(SessionPanelKind::Fallback),
        _ => None,
    }
}

fn is_fallback_session_details(details: &Value) -> bool {
    if first_json_bool(
        details,
        &[
            "fallback",
            "recovery",
            "recovery_injected",
            "recoveryInjected",
            "resume_failed",
            "resumeFailed",
        ],
    )
    .unwrap_or(false)
    {
        return true;
    }

    first_json_string(
        details,
        &[
            "strategy",
            "status",
            "reason",
            "recovery_action",
            "recoveryAction",
        ],
    )
    .is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("fallback") || value.contains("recovery") || value.contains("resume_failed")
    })
}

fn parse_tmux_panel_state(details: &Value) -> Option<TmuxPanelState> {
    if let Some(reused) = first_json_bool(
        details,
        &[
            "tmux_reused",
            "tmuxReused",
            "tmux_kept",
            "tmuxKept",
            "tmux_session_reused",
            "tmuxSessionReused",
        ],
    ) {
        return Some(if reused {
            TmuxPanelState::Kept
        } else {
            TmuxPanelState::New
        });
    }

    let status = first_json_string(details, &["tmux_status", "tmuxStatus", "tmux"])?;
    let status = status.trim().to_ascii_lowercase();
    match status.as_str() {
        "kept" | "keep" | "reused" | "reuse" | "existing" => Some(TmuxPanelState::Kept),
        "new" | "fresh" | "created" | "recreated" => Some(TmuxPanelState::New),
        _ => None,
    }
}

fn first_json_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn first_json_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_bool))
}

fn clean_task_panel_value(raw: &str) -> String {
    first_content_line(raw)
}

fn render_derived_status(status: &DerivedStatus) -> String {
    match status {
        DerivedStatus::Running => "🟢 진행 중".to_string(),
        DerivedStatus::MonitorWait => "💤 monitor 대기".to_string(),
        DerivedStatus::ScheduleWakeup(Some(eta_secs)) => {
            format!("⏰ scheduled wakeup ({eta_secs}s 후)")
        }
        DerivedStatus::ScheduleWakeup(None) => "⏰ scheduled wakeup".to_string(),
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

pub(super) fn events_from_json(value: &Value) -> Vec<RecentPlaceholderEvent> {
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "assistant" => assistant_events(value),
        "content_block_start" => content_block_start_events(value),
        "user" => user_events(value),
        "system" => system_events(value),
        "background_event" => background_event(value).into_iter().collect(),
        "result" => result_event(value).into_iter().collect(),
        _ => Vec::new(),
    }
}

pub(super) fn status_events_from_tool_use(name: &str, input: &str) -> Vec<StatusEvent> {
    let args_summary = format_tool_input(name, input)
        .trim()
        .is_empty()
        .then(|| first_content_line(input))
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let summary = format_tool_input(name, input);
            (!summary.trim().is_empty()).then_some(summary)
        })
        .map(|summary| truncate_chars(&summary, EVENT_LINE_MAX_CHARS));

    let mut events = vec![StatusEvent::ToolStart {
        name: name.to_string(),
        args_summary: args_summary.clone(),
    }];
    if is_task_tool(name) {
        let value = serde_json::from_str::<Value>(input).unwrap_or(Value::Null);
        events.push(StatusEvent::SubagentStart {
            subagent_type: value
                .get("subagent_type")
                .or_else(|| value.get("agent_type"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(name.to_string())),
            desc: subagent_description(&value).or(args_summary.clone()),
        });
    }
    if is_todo_write_tool(name) {
        let value = serde_json::from_str::<Value>(input).unwrap_or(Value::Null);
        if let Some(items) = todo_items_from_input(&value) {
            events.push(StatusEvent::TodoUpdate { items });
        }
    }
    if is_schedule_wakeup_tool(name) {
        events.push(StatusEvent::ScheduleWakeup {
            eta_secs: parse_eta_secs(input.into()),
        });
    }
    events
}

pub(super) fn status_events_from_tool_result(
    tool_name: Option<&str>,
    is_error: bool,
) -> Vec<StatusEvent> {
    let mut events = vec![StatusEvent::ToolEnd { success: !is_error }];
    if tool_name.is_some_and(is_task_tool) {
        events.push(StatusEvent::SubagentEnd { success: !is_error });
    }
    events
}

pub(super) fn status_events_from_task_notification(
    kind: &str,
    status: &str,
    summary: &str,
) -> Vec<StatusEvent> {
    let mut events = Vec::new();
    match kind {
        "monitor_auto_turn" => events.push(StatusEvent::MonitorWait),
        "subagent" => {
            let summary = first_content_line(summary);
            if !summary.is_empty() {
                events.push(StatusEvent::SubagentEvent { summary });
            }
            if task_notification_is_terminal(status) {
                events.push(StatusEvent::SubagentEnd {
                    success: !task_notification_is_error(status),
                });
            }
        }
        "background" => {
            let summary = first_content_line(summary);
            if !summary.is_empty() {
                events.push(StatusEvent::Heartbeat);
            }
        }
        _ => {}
    }
    events
}

pub(super) fn status_events_from_json(value: &Value) -> Vec<StatusEvent> {
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "assistant" => assistant_status_events(value),
        "content_block_start" => content_block_start_status_events(value),
        "user" => user_status_events(value),
        "system" => system_status_events(value),
        "background_event" => background_status_events(value),
        _ => Vec::new(),
    }
}

fn is_task_tool(name: &str) -> bool {
    matches!(
        normalize_tool_key(name).as_str(),
        "task" | "taskcreate" | "agent" | "spawnagent"
    )
}

fn is_todo_write_tool(name: &str) -> bool {
    matches!(
        normalize_tool_key(name).as_str(),
        "todowrite" | "updateplan"
    )
}

fn is_schedule_wakeup_tool(name: &str) -> bool {
    normalize_tool_key(name) == "schedulewakeup"
}

fn normalize_tool_key(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn subagent_description(value: &Value) -> Option<String> {
    [
        "description",
        "desc",
        "prompt",
        "task",
        "message",
        "request",
    ]
    .into_iter()
    .find_map(|key| value.get(key).and_then(Value::as_str))
    .map(normalize_summary)
    .filter(|summary| !summary.is_empty())
}

fn todo_items_from_input(value: &Value) -> Option<Vec<StatusTodoItem>> {
    let items = value
        .get("todos")
        .or_else(|| value.get("items"))
        .or_else(|| value.get("todo_list"))
        .and_then(Value::as_array)?;
    let parsed = items
        .iter()
        .filter_map(|item| {
            let content = item
                .get("content")
                .or_else(|| item.get("text"))
                .or_else(|| item.get("title"))
                .or_else(|| item.get("task"))
                .and_then(Value::as_str)
                .map(normalize_summary)
                .filter(|content| !content.is_empty())?;
            let status = item
                .get("status")
                .or_else(|| item.get("state"))
                .and_then(Value::as_str)
                .map(StatusTodoStatus::from_provider_str)
                .unwrap_or(StatusTodoStatus::Pending);
            Some(StatusTodoItem { content, status })
        })
        .collect::<Vec<_>>();
    (!parsed.is_empty()).then_some(parsed)
}

fn parse_eta_secs(raw: Option<&str>) -> Option<u64> {
    let value = raw?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(parsed) = value.parse::<u64>() {
        return Some(parsed);
    }
    serde_json::from_str::<Value>(value)
        .ok()
        .and_then(|json| eta_secs_from_value(&json))
        .or_else(|| {
            value
                .split(|ch: char| !ch.is_ascii_digit())
                .find(|part| !part.is_empty())
                .and_then(|part| part.parse::<u64>().ok())
        })
}

fn eta_secs_from_value(value: &Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return Some(value);
    }
    if let Some(value) = value.as_str() {
        return parse_eta_secs(Some(value));
    }
    for key in [
        "eta_secs",
        "seconds",
        "delay_secs",
        "delay_seconds",
        "duration_secs",
    ] {
        if let Some(value) = value.get(key).and_then(eta_secs_from_value) {
            return Some(value);
        }
    }
    None
}

fn task_notification_is_terminal(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "completed"
            | "done"
            | "finished"
            | "success"
            | "failed"
            | "error"
            | "aborted"
            | "cancelled"
            | "canceled"
    )
}

fn task_notification_is_error(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "failed" | "error" | "aborted" | "cancelled" | "canceled"
    )
}

fn assistant_status_events(value: &Value) -> Vec<StatusEvent> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                return Vec::new();
            }
            let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
            let input = value_to_compact_string(block.get("input").unwrap_or(&Value::Null));
            status_events_from_tool_use(name, &input)
        })
        .collect()
}

fn content_block_start_status_events(value: &Value) -> Vec<StatusEvent> {
    let Some(block) = value.get("content_block") else {
        return Vec::new();
    };
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return Vec::new();
    }
    let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
    let input = block
        .get("input")
        .map(value_to_compact_string)
        .unwrap_or_default();
    status_events_from_tool_use(name, &input)
}

fn user_status_events(value: &Value) -> Vec<StatusEvent> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                return Vec::new();
            }
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            status_events_from_tool_result(None, is_error)
        })
        .collect()
}

fn system_status_events(value: &Value) -> Vec<StatusEvent> {
    if value.get("subtype").and_then(Value::as_str) != Some("task_notification") {
        return Vec::new();
    }
    let kind = value
        .get("task_notification_kind")
        .and_then(Value::as_str)
        .unwrap_or("system");
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    let summary = value.get("summary").and_then(Value::as_str).unwrap_or("");
    status_events_from_task_notification(kind, status, summary)
}

fn background_status_events(value: &Value) -> Vec<StatusEvent> {
    let summary = value
        .get("message")
        .or_else(|| value.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if summary.trim().is_empty() {
        Vec::new()
    } else {
        vec![StatusEvent::Heartbeat]
    }
}

fn assistant_events(value: &Value) -> Vec<RecentPlaceholderEvent> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                return None;
            }
            let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
            let input = value_to_compact_string(block.get("input").unwrap_or(&Value::Null));
            RecentPlaceholderEvent::tool_use(name, &input)
        })
        .collect()
}

fn content_block_start_events(value: &Value) -> Vec<RecentPlaceholderEvent> {
    let Some(block) = value.get("content_block") else {
        return Vec::new();
    };
    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
        return Vec::new();
    }
    let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
    let input = block
        .get("input")
        .map(value_to_compact_string)
        .unwrap_or_else(|| "started".to_string());
    RecentPlaceholderEvent::tool_use(name, &input)
        .into_iter()
        .collect()
}

fn user_events(value: &Value) -> Vec<RecentPlaceholderEvent> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                return None;
            }
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !is_error {
                return None;
            }
            RecentPlaceholderEvent::tool_error(&tool_result_content(block))
        })
        .collect()
}

fn system_events(value: &Value) -> Vec<RecentPlaceholderEvent> {
    if value.get("subtype").and_then(Value::as_str) != Some("task_notification") {
        return Vec::new();
    }
    let kind = value
        .get("task_notification_kind")
        .and_then(Value::as_str)
        .unwrap_or("system");
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    let summary = value.get("summary").and_then(Value::as_str).unwrap_or("");
    RecentPlaceholderEvent::task_notification(kind, status, summary)
        .into_iter()
        .collect()
}

fn background_event(value: &Value) -> Option<RecentPlaceholderEvent> {
    let summary = value
        .get("message")
        .or_else(|| value.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("");
    RecentPlaceholderEvent::task_notification("background", "", summary)
}

fn result_event(value: &Value) -> Option<RecentPlaceholderEvent> {
    let is_error = value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !is_error {
        return None;
    }
    let summary = value
        .get("errors")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .or_else(|| {
            value
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "error".to_string());
    RecentPlaceholderEvent::tool_error(&summary)
}

fn tool_result_content(block: &Value) -> String {
    if let Some(text) = block.get("content").and_then(Value::as_str) {
        return text.to_string();
    }
    block
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_events<'a>(
    events: impl DoubleEndedIterator<Item = &'a RecentPlaceholderEvent>,
) -> Option<String> {
    let mut lines = Vec::new();
    let mut used = 0usize;
    let inner_limit = EVENT_BLOCK_MAX_CHARS;
    for line in events
        .rev()
        .take(EVENT_RENDER_LIMIT)
        .map(RecentPlaceholderEvent::render_line)
    {
        let line_len = line.chars().count();
        let extra_newline = usize::from(!lines.is_empty());
        if used + extra_newline + line_len > inner_limit {
            continue;
        }
        used += extra_newline + line_len;
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

fn escape_status_panel_markdown(raw: &str) -> String {
    raw.chars()
        .flat_map(|ch| match ch {
            '\\' | '`' | '*' | '_' | '~' | '|' => ['\\', ch],
            _ => ['\0', ch],
        })
        .filter(|ch| *ch != '\0')
        .collect()
}

fn tool_prefix(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    let prefix = match lower.as_str() {
        "bash" | "bashoutput" | "killbash" | "command_execution" => Some("Bash"),
        "edit" | "multiedit" | "write" | "notebookedit" => Some("Edit"),
        "read" => Some("Read"),
        "grep" => Some("Grep"),
        "glob" => Some("Glob"),
        "monitor" => Some("Monitor"),
        "schedulewakeup" | "schedule_wakeup" => Some("ScheduleWakeup"),
        "toolsearch" | "tool_search" | "tool_search_tool" => Some("ToolSearch"),
        "task" | "agent" | "taskcreate" | "taskget" | "taskupdate" | "tasklist" => Some("Task"),
        "webfetch" => Some("WebFetch"),
        "websearch" => Some("WebSearch"),
        _ => canonical_tool_name(name),
    };
    if let Some(prefix) = prefix {
        return format!("[{prefix}]");
    }
    sanitized_tool_name(name)
        .map(|name| format!("[{name}]"))
        .unwrap_or_else(|| "[Tool]".to_string())
}

fn sanitized_tool_name(name: &str) -> Option<String> {
    let sanitized = name
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        .take(32)
        .collect::<String>();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn value_to_compact_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn normalize_summary(raw: &str) -> String {
    let redacted = redact_sensitive_for_placeholder(raw);
    let line = first_content_line(&redacted);
    truncate_chars(&line, EVENT_LINE_MAX_CHARS)
}

fn first_content_line(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(raw: &str, max_chars: usize) -> String {
    if raw.chars().count() <= max_chars {
        return raw.to_string();
    }
    let mut out = raw
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::super::formatting::{
        MonitorHandoffReason, MonitorHandoffStatus,
        build_monitor_handoff_placeholder_with_live_events,
    };
    use super::*;
    use serde_json::json;

    #[test]
    fn render_block_keeps_newest_events_under_limit() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(42);
        for idx in 0..25 {
            events.push_event(
                channel_id,
                RecentPlaceholderEvent::tool_use("Bash", &format!(r#"{{"command":"echo {idx}"}}"#))
                    .unwrap(),
            );
        }

        let block = events.render_block(channel_id).unwrap();
        assert!(!block.contains("```"));
        assert!(block.chars().count() <= EVENT_BLOCK_MAX_CHARS);
        let live_lines = block
            .lines()
            .filter(|line| line.starts_with("[Bash]"))
            .collect::<Vec<_>>();
        assert_eq!(live_lines.len(), EVENT_RENDER_LIMIT);
        assert!(!block.contains("echo 19"));
        assert!(block.contains("echo 24"));
    }

    #[test]
    fn events_from_json_redacts_and_normalizes_tool_use() {
        let events = events_from_json(&json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "name": "Bash",
                    "input": {"command": "curl -H 'Authorization: Bearer abc123' https://example.test?token=secret"}
                }]
            }
        }));

        assert_eq!(events.len(), 1);
        let line = events[0].render_line();
        assert!(line.starts_with("[Bash]"));
        assert!(line.contains("Bearer ***"));
        assert!(line.contains("token=***"));
        assert!(!line.contains("abc123"));
        assert!(!line.contains("secret"));
    }

    #[test]
    fn redact_sensitive_for_placeholder_masks_required_patterns() {
        let redacted = redact_sensitive_for_placeholder(
            "sk-abcdefghijklmnopqrstuvwxyz \
             Authorization: Bearer live-token \
             password=hunter2 token=secret api_key=key1 api-key=key2 \
             alice@example.com",
        );

        assert!(redacted.contains("***"));
        assert!(redacted.contains("Bearer ***"));
        assert!(redacted.contains("password=***"));
        assert!(redacted.contains("token=***"));
        assert!(redacted.contains("api_key=***"));
        assert!(redacted.contains("api-key=***"));
        assert!(redacted.contains("***@***"));
        assert!(!redacted.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(!redacted.contains("live-token"));
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("alice@example.com"));
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("key1"));
        assert!(!redacted.contains("key2"));
    }

    #[test]
    fn monitor_handoff_live_events_stays_under_description_limit_with_long_command() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(99);
        let long_command = format!(
            "printf '{}' && curl -H 'Authorization: Bearer secret-token' https://example.test?api_key=secret",
            "x".repeat(800)
        );
        for idx in 0..20 {
            events.push_event(
                channel_id,
                RecentPlaceholderEvent::tool_use(
                    "Bash",
                    &json!({"command": format!("{long_command}-{idx}")}).to_string(),
                )
                .unwrap(),
            );
        }

        let block = events.render_block(channel_id).unwrap();
        let live_lines = block
            .lines()
            .filter(|line| line.starts_with("[Bash]"))
            .collect::<Vec<_>>();
        assert!(!live_lines.is_empty());
        assert!(
            live_lines
                .iter()
                .all(|line| line.chars().count() <= EVENT_LINE_MAX_CHARS)
        );
        assert!(block.contains("..."));
        assert!(!block.contains("secret-token"));
        assert!(!block.contains("api_key=secret"));

        let rendered = build_monitor_handoff_placeholder_with_live_events(
            MonitorHandoffStatus::Active,
            MonitorHandoffReason::AsyncDispatch,
            1_700_000_000,
            Some(&"tool ".repeat(200)),
            Some(&long_command),
            Some(&"reason ".repeat(200)),
            Some(&"context ".repeat(200)),
            Some(&"request ".repeat(200)),
            Some(&"progress ".repeat(200)),
            Some(&block),
        );

        assert!(
            rendered.len() <= 4096,
            "monitor handoff placeholder exceeded embed description limit: {}",
            rendered.len()
        );
        assert!(rendered.contains("[Bash]"));
        assert!(!rendered.contains("```"));
    }

    #[test]
    fn events_from_json_captures_task_notification() {
        let events = events_from_json(&json!({
            "type": "system",
            "subtype": "task_notification",
            "task_notification_kind": "background",
            "status": "completed",
            "summary": "CI green"
        }));

        assert_eq!(
            events,
            vec![RecentPlaceholderEvent {
                prefix: "[background]".to_string(),
                summary: "completed: CI green".to_string()
            }]
        );
    }

    #[test]
    fn status_panel_renders_derived_tool_state_under_limit() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(77);
        events.push_status_events(
            channel_id,
            status_events_from_tool_use("Bash", &json!({"command": "cargo test"}).to_string()),
        );

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(rendered.contains("도구 실행 중"));
        assert!(rendered.contains("[Bash]"));
        assert!(rendered.chars().count() <= STATUS_PANEL_MAX_CHARS);
    }

    #[test]
    fn status_panel_renders_session_resumed_line_from_lifecycle_details() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(177);
        assert!(events.set_session_panel_lifecycle_event(
            channel_id,
            "session_resumed",
            &json!({
                "provider_session_id": "8f21abcd12345678",
                "tmux_reused": true
            }),
        ));

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(rendered.contains("Lifecycle resumed"));
        assert!(rendered.contains("provider session claude#8f21abcd…"));
        assert!(rendered.contains("tmux kept"));
    }

    #[test]
    fn status_panel_renders_session_fresh_and_fallback_distinctly() {
        let events = PlaceholderLiveEvents::default();
        let fresh_channel_id = ChannelId::new(178);
        events.set_session_panel_lifecycle_event(
            fresh_channel_id,
            "session_fresh",
            &json!({
                "reason": "first_turn",
                "provider_session_id": "fresh-session-id",
                "tmux_reused": false
            }),
        );

        let fresh =
            events.render_status_panel(fresh_channel_id, &ProviderKind::Codex, 1_700_000_000);
        assert!(fresh.contains("Lifecycle fresh"));
        assert!(fresh.contains("provider session codex#fresh-se…"));
        assert!(fresh.contains("tmux new"));

        let fallback_channel_id = ChannelId::new(179);
        events.set_session_panel_lifecycle_event(
            fallback_channel_id,
            "session_resume_failed_with_recovery",
            &json!({
                "reason": "resume_failed",
                "providerSessionId": "fallback-session-id",
                "tmuxStatus": "kept"
            }),
        );

        let fallback =
            events.render_status_panel(fallback_channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(fallback.contains("Lifecycle fallback"));
        assert!(fallback.contains("provider session claude#fallback…"));
        assert!(fallback.contains("tmux kept"));
    }

    #[test]
    fn status_panel_omits_session_line_when_lifecycle_details_are_absent() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(180);
        assert!(!events.set_session_panel_lifecycle_event(
            channel_id,
            "session_resumed",
            &json!({}),
        ));

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(!rendered.contains("Lifecycle "));
    }

    #[test]
    fn status_panel_omits_context_line_when_token_data_is_absent() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(181);

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

        assert!(!rendered.contains("Context   "));
    }

    #[test]
    fn status_panel_renders_task_line_from_dispatch_metadata() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(185);
        assert!(events.set_task_panel_info(
            channel_id,
            "dsp_123",
            Some("42"),
            Some("implementation"),
        ));

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Codex, 1_700_000_000);

        assert!(rendered.contains("Task      dispatch #dsp\\_123 · card #42 · implementation"));
    }

    #[test]
    fn status_panel_omits_task_line_without_dispatch_id() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(186);

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

        assert!(!rendered.contains("Task      "));
    }

    #[test]
    fn status_panel_renders_task_line_with_dispatch_fallback() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(187);
        assert!(events.set_task_panel_info(channel_id, "dsp_404", None, None));

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

        assert!(rendered.contains("Task      dispatch #dsp\\_404"));
        assert!(!rendered.contains("card #"));
    }

    #[test]
    fn status_panel_renders_context_usage_severity_levels() {
        let events = PlaceholderLiveEvents::default();
        let normal_channel_id = ChannelId::new(182);
        assert!(events.set_context_panel_usage(normal_channel_id, 740, 0, 0, 1000, 90));
        let normal =
            events.render_status_panel(normal_channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(normal.contains("Context   📦 74% used · auto-compact 90%"));
        assert!(!normal.contains("임박"));
        assert!(!normal.contains("자동 압축 직전"));

        let approaching_channel_id = ChannelId::new(183);
        events.set_context_panel_usage(approaching_channel_id, 700, 40, 10, 1000, 90);
        let approaching = events.render_status_panel(
            approaching_channel_id,
            &ProviderKind::Claude,
            1_700_000_000,
        );
        assert!(approaching.contains("Context   📦 75% used · auto-compact 90% (임박)"));

        let critical_channel_id = ChannelId::new(184);
        events.set_context_panel_usage(critical_channel_id, 700, 100, 50, 1000, 90);
        let critical =
            events.render_status_panel(critical_channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(critical.contains("Context   ⚠️ 85% used · auto-compact 90% — 자동 압축 직전"));
    }

    #[test]
    fn status_panel_renders_prompt_manifest_block() {
        fn layer(
            name: &str,
            enabled: bool,
            reason: Option<&str>,
        ) -> crate::db::prompt_manifests::PromptManifestLayer {
            crate::db::prompt_manifests::PromptManifestLayer {
                id: None,
                manifest_id: None,
                layer_name: name.to_string(),
                enabled,
                source: Some("test".to_string()),
                reason: reason.map(str::to_string),
                chars: 0,
                tokens_est: 0,
                content_sha256: "0".repeat(64),
                content_visibility:
                    crate::db::prompt_manifests::PromptContentVisibility::AdkProvided,
                full_content: Some(String::new()),
                redacted_preview: None,
            }
        }

        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(185);
        let manifest = PromptManifest {
            id: None,
            created_at: None,
            turn_id: "turn-185".to_string(),
            channel_id: channel_id.get().to_string(),
            dispatch_id: None,
            profile: Some("full".to_string()),
            total_input_tokens_est: 21_400,
            layer_count: 5,
            layers: vec![
                layer("role_prompt", true, None),
                layer("dispatch_contract", true, None),
                layer("current_task", true, None),
                layer("recovery_context", false, Some("no_recovery")),
                layer(
                    "memory_recall",
                    false,
                    Some("memory_backend=memento;mcp_unavailable"),
                ),
            ],
        };

        assert!(events.set_prompt_manifest(channel_id, &manifest));
        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(rendered.contains("Prompt    Full profile · ~21.4k input tokens"));
        assert!(
            rendered.contains("- 활성 (3): role\\_prompt, dispatch\\_contract, current\\_task")
        );
        assert!(rendered.contains(
            "- 스킵 (2): recovery\\_context (no\\_recovery), memory\\_recall (memory\\_backend=memento;mcp\\_unavailable)"
        ));
    }

    #[test]
    fn status_panel_omits_prompt_line_when_manifest_is_absent() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(186);

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(!rendered.contains("Prompt    "));
    }

    #[test]
    fn status_panel_tracks_todowrite_plan() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(78);
        events.push_status_events(
            channel_id,
            status_events_from_tool_use(
                "TodoWrite",
                &json!({
                    "todos": [
                        {"content": "Read issue", "status": "completed"},
                        {"content": "Implement panel", "status": "in_progress"}
                    ]
                })
                .to_string(),
            ),
        );

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(rendered.contains("Plan"));
        assert!(rendered.contains("- [x] Read issue"));
        assert!(rendered.contains("- [ ] Implement panel"));
    }

    #[test]
    fn status_panel_tracks_one_level_subagents() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(79);
        events.push_status_events(
            channel_id,
            status_events_from_tool_use(
                "Task",
                &json!({"subagent_type": "explorer", "description": "Inspect bridge"}).to_string(),
            ),
        );
        events.push_status_events(
            channel_id,
            status_events_from_task_notification("subagent", "running", "found turn bridge"),
        );
        events.push_status_events(
            channel_id,
            status_events_from_tool_result(Some("Task"), false),
        );

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
        assert!(rendered.contains("Subagents"));
        assert!(rendered.contains("explorer Inspect bridge"));
        assert!(rendered.contains("found turn bridge"));
        assert!(rendered.contains("✓"));
    }

    #[test]
    fn status_panel_hides_plan_and_subagents_for_codex() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(80);
        events.push_status_events(
            channel_id,
            status_events_from_tool_use(
                "TodoWrite",
                &json!({"todos": [{"content": "Hidden for Codex", "status": "pending"}]})
                    .to_string(),
            ),
        );
        events.push_status_events(
            channel_id,
            status_events_from_tool_use(
                "Task",
                &json!({"description": "Hidden subagent"}).to_string(),
            ),
        );

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Codex, 1_700_000_000);
        assert!(!rendered.contains("Plan"));
        assert!(!rendered.contains("Subagents"));
        assert!(!rendered.contains("Hidden for Codex"));
        assert!(!rendered.contains("Hidden subagent"));
    }

    #[test]
    fn status_events_from_json_keeps_tool_result_visibility() {
        let events = status_events_from_json(&json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "is_error": true,
                    "content": "failed"
                }]
            }
        }));

        assert_eq!(events, vec![StatusEvent::ToolEnd { success: false }]);
    }

    #[test]
    fn status_tool_result_closes_subagent_only_for_task_tools() {
        assert_eq!(
            status_events_from_tool_result(Some("Read"), false),
            vec![StatusEvent::ToolEnd { success: true }]
        );
        assert_eq!(
            status_events_from_tool_result(Some("Task"), true),
            vec![
                StatusEvent::ToolEnd { success: false },
                StatusEvent::SubagentEnd { success: false }
            ]
        );
    }
}
