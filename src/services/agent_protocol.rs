use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;

/// Reference list of known Claude Code tools.
///
/// No longer used for CLI gating — the Claude CLI is invoked without `--allowed-tools`
/// so that newly released tools (e.g. `Monitor`) are exposed automatically. This list
/// remains as the baseline for session-level defaults and legacy migration fallbacks.
/// Update when Anthropic ships new tools so user-facing configuration stays accurate.
pub const DEFAULT_ALLOWED_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "Task",
    "TaskOutput",
    "TaskStop",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Skill",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
    "Monitor",
    "BashOutput",
    "KillBash",
    "SlashCommand",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
];

/// Streaming message types for provider responses consumed by Discord orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskNotificationKind {
    Subagent,
    Background,
    MonitorAutoTurn,
}

impl TaskNotificationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
            Self::Background => "background",
            Self::MonitorAutoTurn => "monitor_auto_turn",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "subagent" => Some(Self::Subagent),
            "background" => Some(Self::Background),
            "monitor_auto_turn" => Some(Self::MonitorAutoTurn),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeHandoffKind {
    LegacyTmuxWrapper,
    ClaudeTui,
    CodexTui,
    ProcessBackend,
    ClaudeEAdapter,
}

impl RuntimeHandoffKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LegacyTmuxWrapper => "legacy_tmux_wrapper",
            Self::ClaudeTui => "claude_tui",
            Self::CodexTui => "codex_tui",
            Self::ProcessBackend => "process_backend",
            Self::ClaudeEAdapter => "claude_e_adapter",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim() {
            "legacy_tmux_wrapper" => Some(Self::LegacyTmuxWrapper),
            "claude_tui" => Some(Self::ClaudeTui),
            "codex_tui" => Some(Self::CodexTui),
            "process_backend" => Some(Self::ProcessBackend),
            "claude_e_adapter" => Some(Self::ClaudeEAdapter),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::LegacyTmuxWrapper => "legacy tmux wrapper",
            Self::ClaudeTui => "Claude TUI",
            Self::CodexTui => "Codex TUI",
            Self::ProcessBackend => "ProcessBackend",
            Self::ClaudeEAdapter => "claude-e adapter",
        }
    }

    pub fn requires_input_fifo(self) -> bool {
        matches!(self, Self::LegacyTmuxWrapper)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeHandoff {
    LegacyTmuxWrapper {
        output_path: String,
        input_fifo_path: String,
        tmux_session_name: String,
        last_offset: u64,
    },
    ClaudeTui {
        transcript_path: String,
        tmux_session_name: String,
        last_offset: u64,
    },
    CodexTui {
        rollout_path: String,
        thread_id: Option<String>,
        tmux_session_name: String,
        last_offset: u64,
    },
    ProcessBackend {
        output_path: String,
        session_name: String,
        last_offset: u64,
    },
    /// `claude-e` per-turn adapter. Output is a stream-json file written by
    /// the wrapper for the current turn; `session_name` is the logical
    /// AgentDesk session label (Discord channel + provider session id).
    /// `last_offset` is the byte offset into `output_path` consumed so far.
    ClaudeEAdapter {
        output_path: String,
        session_name: String,
        last_offset: u64,
    },
}

#[derive(Debug, Clone)]
pub enum StreamMessage {
    /// Initialization - contains session_id
    Init {
        session_id: String,
        raw_session_id: Option<String>,
    },
    /// Provider started a fresh retry attempt after discarding stale session state
    RetryBoundary,
    /// Text response chunk
    Text { content: String },
    /// Tool use started.
    ///
    /// `tool_use_id` is the provider-assigned identifier for this tool
    /// invocation (Anthropic `id`, etc.). It lets consumers pair a
    /// `ToolResult` back to the exact `ToolUse` instead of relying on FIFO
    /// ordering, which mis-pairs when a long-running tool (e.g. a Task
    /// subagent) returns after intervening short foreground tools. Backends
    /// that cannot surface an id leave this `None`, in which case consumers
    /// fall back to FIFO pairing.
    ToolUse {
        name: String,
        input: String,
        tool_use_id: Option<String>,
    },
    /// Tool execution result.
    ///
    /// `tool_use_id` mirrors the originating [`StreamMessage::ToolUse`] id so
    /// the result can be matched to its tool use precisely. `None` when the
    /// backend does not provide one.
    ToolResult {
        content: String,
        is_error: bool,
        tool_use_id: Option<String>,
    },
    /// Provider thinking/reasoning progress marker. Raw reasoning payloads must stay redacted.
    Thinking { summary: Option<String> },
    /// Background task notification
    TaskNotification {
        task_id: String,
        /// Originating tool-use id when the provider/harness surfaces one (for
        /// example Claude Code background Bash `<tool-use-id>` notifications).
        /// `None` means consumers must not guess an owner.
        tool_use_id: Option<String>,
        status: String,
        summary: String,
        kind: TaskNotificationKind,
    },
    /// Provider-normalized status-panel events observed in raw stream JSON.
    StatusEvents { events: Vec<StatusEvent> },
    /// Completion
    Done {
        result: String,
        session_id: Option<String>,
    },
    /// Error
    Error {
        message: String,
        #[allow(dead_code)]
        stdout: String,
        stderr: String,
        #[allow(dead_code)]
        exit_code: Option<i32>,
    },
    /// Statusline info extracted from result/assistant events.
    ///
    /// Token fields are snapshots of the latest provider usage known for the
    /// turn. For context-window occupancy, consumers must count cache-create
    /// and cache-read input tokens with raw input tokens, and must not add
    /// output tokens.
    StatusUpdate {
        model: Option<String>,
        cost_usd: Option<f64>,
        total_cost_usd: Option<f64>,
        #[allow(dead_code)]
        duration_ms: Option<u64>,
        #[allow(dead_code)]
        num_turns: Option<u32>,
        input_tokens: Option<u64>,
        cache_create_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    /// Complete per-assistant-record Claude usage for live compact steering.
    ///
    /// This is intentionally separate from [`Self::StatusUpdate`]: status updates
    /// remain terminal/panel/analytics telemetry, while active snapshots preserve
    /// the current record's model and explicit zero cache fields.
    ActiveUsageSnapshot {
        model: Option<String>,
        input_tokens: u64,
        cache_create_tokens: u64,
        cache_read_tokens: u64,
    },
    /// tmux session is ready for background monitoring or watcher-owned relay.
    TmuxReady {
        output_path: String,
        input_fifo_path: String,
        tmux_session_name: String,
        last_offset: u64,
    },
    /// Provider runtime is ready for bridge handoff without overloading
    /// wrapper-only fields such as the tmux input FIFO.
    RuntimeReady { handoff: RuntimeHandoff },
    /// ProcessBackend session completed first turn (no tmux watcher needed)
    ProcessReady {
        output_path: String,
        session_name: String,
        last_offset: u64,
    },
    /// Latest read offset in a growing tmux output file
    OutputOffset { offset: u64 },
}

impl StreamMessage {
    pub(crate) fn redacted_thinking() -> Self {
        Self::Thinking { summary: None }
    }
}

/// Final accounting for a finished subagent, mirroring the Claude TUI's
/// `Done (N tool uses · M tokens · Xs)` summary. Reconstructed from the parent
/// transcript's Task `toolUseResult` (`totalToolUseCount` / `totalTokens` /
/// `totalDurationMs`) and/or the per-subagent `subagents/agent-<id>.jsonl`
/// rollout (#3086). Each field is optional so a partial/older transcript that
/// surfaces only some of the values still renders what it can.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubagentSummary {
    /// Number of tool invocations the subagent issued (`totalToolUseCount`).
    pub tool_count: Option<u64>,
    /// Total tokens attributed to the subagent (`totalTokens`).
    pub tokens: Option<u64>,
    /// Wall-clock duration in seconds (`totalDurationMs` / 1000).
    pub duration_secs: Option<u64>,
}

impl SubagentSummary {
    /// `true` when no field carries a value, so callers can skip emitting an
    /// empty `Done (...)` summary line.
    pub fn is_empty(&self) -> bool {
        self.tool_count.is_none() && self.tokens.is_none() && self.duration_secs.is_none()
    }
}

/// Provider-normalized status events consumed by Discord status-panel rendering.
/// The panel code should not depend on provider-specific JSONL shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusEvent {
    ToolStart {
        name: String,
        args_summary: Option<String>,
    },
    ToolEnd {
        success: bool,
    },
    SubagentStart {
        subagent_type: Option<String>,
        desc: Option<String>,
        agent_id: Option<String>,
        /// Originating Task tool-use id, used to pair the eventual
        /// `SubagentEnd` to the exact slot rather than the first unfinished
        /// one (which mis-attributes across parallel subagents). `None` when
        /// the backend cannot surface an id.
        tool_use_id: Option<String>,
        /// `true` when the Task/Agent was launched with `run_in_background`.
        /// A background dispatch's immediate `tool_result` is only a launch
        /// ack — the subagent keeps running and outlives the launching turn —
        /// so the panel must NOT mark it ✓ (completed) on that ack-only
        /// `SubagentEnd`; only a genuine completion (summary-bearing end or a
        /// terminal task_notification) finalizes it (#3041 / status panel).
        background: bool,
    },
    SubagentEvent {
        summary: String,
    },
    /// Live activity from a still-running subagent (especially a
    /// `run_in_background` one), attributed to its slot via the nested record's
    /// top-level `parent_tool_use_id` — the launching Task's tool-use id stored
    /// on [`StatusEvent::SubagentStart::tool_use_id`]. Carries the subagent's
    /// latest activity line (current tool / short summary) so a long background
    /// task is not an opaque "running". The panel updates the slot's recent line
    /// on each new activity and NEVER resurrects a finished slot (#3198 ack-only
    /// / background-finalize semantics are preserved). `tool_use_id` is the
    /// parent Task id; `None` falls back to the last unfinished slot.
    SubagentActivity {
        tool_use_id: Option<String>,
        summary: String,
    },
    SubagentEnd {
        success: bool,
        agent_id: Option<String>,
        desc: Option<String>,
        /// Tool-use id of the Task whose result closed this subagent. Matched
        /// against [`StatusEvent::SubagentStart::tool_use_id`]; `None` falls
        /// back to closing the first unfinished slot.
        tool_use_id: Option<String>,
        /// TUI-parity accounting (tool count / tokens · duration) reconstructed
        /// from the Task `toolUseResult` and/or `subagents/*.jsonl` (#3086).
        /// `None` when no summary fields were recoverable.
        summary: Option<SubagentSummary>,
        /// `true` when this end is only the immediate `tool_result` launch ack
        /// for a Task (it always fires when the Task tool returns, even for a
        /// `run_in_background` dispatch whose subagent keeps running). The panel
        /// uses this to avoid marking a still-running background subagent ✓: an
        /// ack-only end finalizes a foreground slot but NOT a background slot.
        /// A genuine completion (summary-bearing end, or a terminal
        /// task_notification) sets this `false` and always finalizes.
        ack_only: bool,
    },
    TaskToolUpdate {
        name: String,
        task_id: Option<String>,
        summary: Option<String>,
        status: Option<String>,
    },
    BackgroundTaskStart {
        name: String,
        summary: String,
        tool_use_id: String,
    },
    BackgroundTaskEnd {
        tool_use_id: String,
        success: bool,
    },
    TodoUpdate {
        items: Vec<StatusTodoItem>,
    },
    MonitorWait,
    ScheduleWakeup {
        eta_secs: Option<u64>,
    },
    WorkflowStart {
        task_id: Option<String>,
        name: Option<String>,
    },
    WorkflowPhase {
        task_id: Option<String>,
        index: u64,
        title: String,
    },
    WorkflowAgent {
        task_id: Option<String>,
        index: u64,
        label: String,
        phase_index: Option<u64>,
        phase_title: Option<String>,
        state: String,
    },
    WorkflowLog {
        task_id: Option<String>,
        summary: String,
    },
    WorkflowEnd {
        task_id: Option<String>,
        success: bool,
        summary: Option<String>,
    },
    TurnCompleted {
        background: bool,
        background_agent_pending: bool,
    },
    Heartbeat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusTodoItem {
    pub content: String,
    pub status: StatusTodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl StatusTodoStatus {
    pub fn from_provider_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "completed" | "done" | "success" => Self::Completed,
            "in_progress" | "in-progress" | "active" | "running" => Self::InProgress,
            "cancelled" | "canceled" | "cancelled_by_user" => Self::Cancelled,
            _ => Self::Pending,
        }
    }

    pub fn checkbox_marker(self) -> &'static str {
        match self {
            Self::Completed => "[x]",
            Self::Cancelled => "[-]",
            Self::Pending | Self::InProgress => "[ ]",
        }
    }
}

pub(crate) fn status_events_from_workflow_json(value: &Value) -> Vec<StatusEvent> {
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "system" => workflow_system_status_events(value),
        "workflow_phase" => workflow_phase_status_event(value, workflow_task_id(value))
            .into_iter()
            .collect(),
        "workflow_agent" => workflow_agent_status_event(value, workflow_task_id(value))
            .into_iter()
            .collect(),
        "workflow_log" => workflow_log_status_event(value, workflow_task_id(value))
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn workflow_system_status_events(value: &Value) -> Vec<StatusEvent> {
    match value.get("subtype").and_then(Value::as_str).unwrap_or("") {
        "task_started" if workflow_task_started(value) => vec![StatusEvent::WorkflowStart {
            task_id: workflow_task_id(value),
            name: first_workflow_string(value, &["workflow_name", "summary", "description"]),
        }],
        "task_progress" => workflow_progress_status_events(value),
        "task_notification" if workflow_task_notification(value) => {
            vec![StatusEvent::WorkflowEnd {
                task_id: workflow_task_id(value),
                success: !workflow_status_is_error(
                    value.get("status").and_then(Value::as_str).unwrap_or(""),
                ),
                summary: first_workflow_string(value, &["summary", "description"]),
            }]
        }
        _ => Vec::new(),
    }
}

fn workflow_task_started(value: &Value) -> bool {
    value
        .get("task_type")
        .and_then(Value::as_str)
        .is_some_and(|task_type| task_type == "local_workflow")
}

fn workflow_task_notification(value: &Value) -> bool {
    if value
        .get("task_notification_kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "workflow")
        || workflow_task_started(value)
    {
        return true;
    }

    value
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|summary| {
            let lower = summary.to_ascii_lowercase();
            lower.starts_with("dynamic workflow ") || lower.starts_with("workflow ")
        })
}

fn workflow_progress_status_events(value: &Value) -> Vec<StatusEvent> {
    let task_id = workflow_task_id(value);
    value
        .get("workflow_progress")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(
            |item| match item.get("type").and_then(Value::as_str).unwrap_or("") {
                "workflow_phase" => workflow_phase_status_event(item, task_id.clone()),
                "workflow_agent" => workflow_agent_status_event(item, task_id.clone()),
                "workflow_log" => workflow_log_status_event(item, task_id.clone()),
                _ => None,
            },
        )
        .collect()
}

fn workflow_phase_status_event(value: &Value, task_id: Option<String>) -> Option<StatusEvent> {
    let index = workflow_u64(value, &["index"])?;
    let title = first_workflow_string(value, &["title", "name", "phaseTitle", "phase_title"])?;
    Some(StatusEvent::WorkflowPhase {
        task_id,
        index,
        title,
    })
}

fn workflow_agent_status_event(value: &Value, task_id: Option<String>) -> Option<StatusEvent> {
    let index = workflow_u64(value, &["index"])?;
    let label = first_workflow_string(value, &["label", "name", "agentId", "agent_id"])?;
    Some(StatusEvent::WorkflowAgent {
        task_id,
        index,
        label,
        phase_index: workflow_u64(value, &["phaseIndex", "phase_index"]),
        phase_title: first_workflow_string(value, &["phaseTitle", "phase_title"]),
        state: first_workflow_string(value, &["state", "status"])
            .unwrap_or_else(|| "progress".to_string()),
    })
}

fn workflow_log_status_event(value: &Value, task_id: Option<String>) -> Option<StatusEvent> {
    let summary = first_workflow_string(value, &["summary", "message", "text", "content", "log"])?;
    Some(StatusEvent::WorkflowLog { task_id, summary })
}

fn workflow_task_id(value: &Value) -> Option<String> {
    first_workflow_string(
        value,
        &["task_id", "taskId", "workflowRunId", "workflow_run_id"],
    )
}

fn first_workflow_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn workflow_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|raw| match raw {
            Value::Number(number) => number.as_u64().or_else(|| {
                number
                    .as_i64()
                    .filter(|value| *value >= 0)
                    .map(|value| value as u64)
            }),
            Value::String(text) => text.trim().parse::<u64>().ok(),
            _ => None,
        })
    })
}

fn workflow_status_is_error(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "failed" | "error" | "aborted" | "cancelled" | "canceled" | "stopped"
    )
}

/// Cached regex pattern for session ID validation.
pub(crate) fn session_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:-]*$").expect("Invalid session ID regex pattern")
    })
}

/// Validate session ID format for provider resume flags.
///
/// Leading dashes are rejected so a session id can never be interpreted as a
/// provider CLI flag when appended after `--resume`/`--resume-session-id`.
/// Max length reduced to 64 characters for security.
pub(crate) fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty() && session_id.len() <= 64 && session_id_regex().is_match(session_id)
}
