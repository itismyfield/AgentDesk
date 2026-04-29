use regex::Regex;
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
    /// Tool use started
    ToolUse { name: String, input: String },
    /// Tool execution result
    ToolResult { content: String, is_error: bool },
    /// Provider thinking/reasoning progress marker. Raw reasoning payloads must stay redacted.
    Thinking { summary: Option<String> },
    /// Background task notification
    TaskNotification {
        task_id: String,
        status: String,
        summary: String,
        kind: TaskNotificationKind,
    },
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
    /// Statusline info extracted from result/assistant events
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
    /// tmux session is ready for background monitoring (first turn completed)
    TmuxReady {
        output_path: String,
        input_fifo_path: String,
        tmux_session_name: String,
        last_offset: u64,
    },
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

/// Cached regex pattern for session ID validation.
pub(crate) fn session_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9_-]+$").expect("Invalid session ID regex pattern"))
}

/// Validate session ID format (alphanumeric, dashes, underscores only).
/// Max length reduced to 64 characters for security.
pub(crate) fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty() && session_id.len() <= 64 && session_id_regex().is_match(session_id)
}

#[cfg(test)]
mod tests {
    use super::{is_valid_session_id, session_id_regex};

    #[test]
    fn test_session_id_valid() {
        assert!(is_valid_session_id("abc123"));
        assert!(is_valid_session_id("session-1"));
        assert!(is_valid_session_id("session_2"));
        assert!(is_valid_session_id("ABC-XYZ_123"));
        assert!(is_valid_session_id("a"));
    }

    #[test]
    fn test_session_id_empty_rejected() {
        assert!(!is_valid_session_id(""));
    }

    #[test]
    fn test_session_id_too_long_rejected() {
        let max_len = "a".repeat(64);
        assert!(is_valid_session_id(&max_len));

        let too_long = "a".repeat(65);
        assert!(!is_valid_session_id(&too_long));
    }

    #[test]
    fn test_session_id_special_chars_rejected() {
        assert!(!is_valid_session_id("session;rm -rf"));
        assert!(!is_valid_session_id("session'OR'1=1"));
        assert!(!is_valid_session_id("session`cmd`"));
        assert!(!is_valid_session_id("session$(cmd)"));
        assert!(!is_valid_session_id("session\nline2"));
        assert!(!is_valid_session_id("session\0null"));
        assert!(!is_valid_session_id("path/traversal"));
        assert!(!is_valid_session_id("session with space"));
        assert!(!is_valid_session_id("session.dot"));
        assert!(!is_valid_session_id("session@email"));
    }

    #[test]
    fn test_session_id_unicode_rejected() {
        assert!(!is_valid_session_id("세션아이디"));
        assert!(!is_valid_session_id("session_日本語"));
        assert!(!is_valid_session_id("émoji🎉"));
    }

    #[test]
    fn test_session_id_regex_caching() {
        let regex1 = session_id_regex();
        let regex2 = session_id_regex();
        assert!(std::ptr::eq(regex1, regex2));
    }
}
