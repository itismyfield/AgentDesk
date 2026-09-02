use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;
use uuid::Uuid;

mod fresh_session;
#[allow(clippy::too_many_arguments)]
mod session_lifecycle;

pub(crate) use fresh_session::{
    build_stream_exec_args, normalize_resume_strategy, normalize_resume_strategy_for_turn,
};
use fresh_session::{should_preserve_live_reused_provider_session, validated_resume_session_id};
use session_lifecycle::execute_streaming_local_process;
#[cfg(unix)]
use session_lifecycle::execute_streaming_local_tmux;

use crate::services::agent_protocol::{StreamMessage, is_valid_session_id};
use crate::services::claude;
use crate::services::discord::restart_report::{
    RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
};
use crate::services::process::{kill_child_tree, shell_escape};
use crate::services::provider::{
    CancelToken, FollowupResult, ProviderKind, ReadOutputResult, SessionProbe, cancel_requested,
    register_child_pid, tmux_followup_fallback_after_read_error,
};
use crate::services::provider_runtime::{
    LineStreamEvent, SharedAllowedToolKind, resolve_shared_allowed_tool_compat,
    spawn_line_stream_reader,
};
use crate::services::remote::RemoteProfile;
use crate::services::session_backend::{
    ReadOutputFailure, StreamLineState, insert_process_session, process_session_is_alive,
    process_session_probe, process_stream_line, remove_process_session, send_process_session_input,
    terminate_process_session,
};
#[cfg(unix)]
use crate::services::tmux_common::{tmux_owner_path, write_tmux_owner_marker};
#[cfg(unix)]
use crate::services::tmux_diagnostics::{
    record_tmux_exit_reason, should_recreate_session_after_followup_fifo_error,
    tmux_session_exists, tmux_session_has_live_pane,
};

#[cfg(unix)]
pub(super) fn stamp_qwen_spawn_markers(tmux_session_name: &str) {
    // Keep the provider-specific spawn marker contract at the Qwen facade while
    // the Unix lifecycle implementation owns the surrounding process flow.
    if let Err(e) = crate::services::discord::stamp_spawn_markers(tmux_session_name) {
        tracing::warn!("failed to write spawn nonce for {tmux_session_name}: {e}");
    }
}

const QWEN_CANCELLED_MESSAGE: &str = "Qwen request cancelled";
const QWEN_SESSION_DEAD_MESSAGE: &str = "Qwen stream ended without a terminal result";
pub(crate) const QWEN_STREAM_POLL_TIMEOUT: Duration = Duration::from_secs(5);
// Allow up to 240 s for the first token: NVIDIA-backed large-context requests and upstream
// rate-limit backoffs can legitimately exceed the normal interactive response budget.
pub(crate) const QWEN_STREAM_STARTUP_WATCHDOG: Duration = Duration::from_secs(240);
// Allow up to 120 s of silence after progress has been seen: covers long-running tool calls
// (e.g. cargo build, test suites) where the model is waiting for a tool result between turns.
pub(crate) const QWEN_STREAM_IDLE_WATCHDOG: Duration = Duration::from_secs(120);
pub(crate) const QWEN_MAX_SESSION_RETRIES: usize = 1;
const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";
pub(crate) const QWEN_CODE_SYSTEM_SETTINGS_ENV: &str = "QWEN_CODE_SYSTEM_SETTINGS_PATH";
pub(crate) const QWEN_SUPPORTED_ALLOWED_TOOLS: &[&str] = &[
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
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct QwenStreamWatchdog {
    poll_timeout: Duration,
    startup_watchdog: Duration,
    idle_watchdog: Duration,
    startup_silent_for: Duration,
    idle_silent_for: Duration,
}

impl Default for QwenStreamWatchdog {
    fn default() -> Self {
        Self::new(
            QWEN_STREAM_POLL_TIMEOUT,
            QWEN_STREAM_STARTUP_WATCHDOG,
            QWEN_STREAM_IDLE_WATCHDOG,
        )
    }
}

impl QwenStreamWatchdog {
    pub(crate) const fn new(
        poll_timeout: Duration,
        startup_watchdog: Duration,
        idle_watchdog: Duration,
    ) -> Self {
        Self {
            poll_timeout,
            startup_watchdog,
            idle_watchdog,
            startup_silent_for: Duration::ZERO,
            idle_silent_for: Duration::ZERO,
        }
    }

    pub(crate) const fn poll_timeout(&self) -> Duration {
        self.poll_timeout
    }

    // Called on every received line, not just meaningful ones.  Any stream activity resets both
    // accumulators so a session that is producing non-content output (init handshake, system
    // events) does not get prematurely retried.  The startup-vs-idle threshold selection is made
    // by `on_timeout` based on `meaningful_progress_seen`, not here.
    pub(crate) fn observe_line(&mut self) {
        self.startup_silent_for = Duration::ZERO;
        self.idle_silent_for = Duration::ZERO;
    }

    pub(crate) fn on_timeout(&mut self, meaningful_progress_seen: bool) -> Option<String> {
        if !meaningful_progress_seen {
            self.startup_silent_for += self.poll_timeout;
            if self.startup_silent_for >= self.startup_watchdog {
                return Some(self.startup_retry_message());
            }
            return None;
        }

        self.idle_silent_for += self.poll_timeout;
        if self.idle_silent_for >= self.idle_watchdog {
            return Some(self.idle_retry_message());
        }
        None
    }

    pub(crate) fn startup_retry_message(&self) -> String {
        format!(
            "Qwen stream produced no output for {} seconds before first progress",
            self.startup_watchdog.as_secs()
        )
    }

    pub(crate) fn idle_retry_message(&self) -> String {
        format!(
            "Qwen stream produced no output for {} seconds after progress",
            self.idle_watchdog.as_secs()
        )
    }
}

#[derive(Debug)]
pub(crate) struct QwenSystemSettingsOverride {
    path: PathBuf,
}

impl QwenSystemSettingsOverride {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for QwenSystemSettingsOverride {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

type QwenStreamEvent = LineStreamEvent;

#[derive(Clone, Debug)]
pub(crate) enum QwenResumeStrategy {
    Fresh,
    Continue,
    Resume(String),
}

#[derive(Debug)]