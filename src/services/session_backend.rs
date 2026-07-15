//! Shared session/process backend utilities for AI provider wrappers.
//!
//! This module owns:
//! - direct child-process wrapper spawning (`ProcessBackend`)
//! - the shared in-memory process session registry
//! - normalized output-file tailing/parsing for wrapper JSONL streams

use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::{
    StreamMessage, TaskNotificationKind, status_events_from_workflow_json,
};
use crate::services::process::ProcessIdentity;
use crate::services::provider::{CancelToken, ReadOutputResult, SessionProbe};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOutputFailure {
    pub error: String,
    pub last_offset: u64,
}

/// Configuration for creating a new session.
pub struct SessionConfig {
    /// Unique session name (used for temp file naming)
    pub session_name: String,
    /// Working directory for the AI provider
    pub working_dir: String,
    /// Path to the agentdesk binary (for spawning wrapper)
    pub agentdesk_exe: String,
    /// Output JSONL file path
    pub output_path: String,
    /// Prompt file path
    pub prompt_path: String,
    /// Wrapper subcommand (e.g., tmux-wrapper, codex-tmux-wrapper, qwen-tmux-wrapper)
    pub wrapper_subcommand: String,
    /// Provider-specific wrapper args (e.g., --codex-bin, -- claude ...)
    pub wrapper_args: Vec<String>,
    /// Environment variables to set
    pub env_vars: Vec<(String, String)>,
}

/// Handle to a running session, returned by create_session.
pub enum SessionHandle {
    Process {
        child_stdin: Arc<Mutex<Option<ChildStdin>>>,
        child: Arc<Mutex<Option<Child>>>,
        pid: u32,
    },
    #[cfg(test)]
    TestProcess {
        pid: u32,
        alive: Arc<std::sync::atomic::AtomicBool>,
    },
}

struct ProcessSessionEntry {
    handle: SessionHandle,
    identity: ProcessIdentity,
    active_turns: usize,
}

#[derive(Default)]
struct ProcessSessionRegistry {
    handles: HashMap<String, ProcessSessionEntry>,
    stopped: HashSet<String>,
    stopped_order: VecDeque<String>,
}

const MAX_STOPPED_PROCESS_SESSIONS: usize = 1024;

impl ProcessSessionRegistry {
    fn insert(&mut self, session_name: String, handle: SessionHandle) {
        self.insert_with_active_turns(session_name, handle, 0);
    }

    fn insert_with_active_turns(
        &mut self,
        session_name: String,
        handle: SessionHandle,
        active_turns: usize,
    ) {
        self.remove_stopped_marker(&session_name);
        let identity = ProcessIdentity::capture(handle.pid());
        self.handles.insert(
            session_name,
            ProcessSessionEntry {
                handle,
                identity,
                active_turns,
            },
        );
    }

    fn mark_stopped(&mut self, session_name: String) {
        if self.stopped.insert(session_name.clone()) {
            self.stopped_order.push_back(session_name);
            self.prune_stopped_markers();
        }
    }

    fn remove_stopped_marker(&mut self, session_name: &str) {
        self.stopped.remove(session_name);
    }

    fn prune_stopped_markers(&mut self) {
        while self.stopped.len() > MAX_STOPPED_PROCESS_SESSIONS {
            let Some(candidate) = self.stopped_order.pop_front() else {
                break;
            };
            self.stopped.remove(&candidate);
        }
    }
}

/// Backend for managing AI provider sessions.
pub trait SessionBackend: Send + Sync {
    /// Create a new session. Returns a handle for subsequent operations.
    fn create_session(&self, config: &SessionConfig) -> Result<SessionHandle, String>;

    /// Send a follow-up message to an existing session (stream-json formatted).
    fn send_input(&self, handle: &SessionHandle, message: &str) -> Result<(), String>;

    /// Check if the session process is still running.
    fn is_alive(&self, handle: &SessionHandle) -> bool;
}

// ─── ProcessBackend ───────────────────────────────────────────────────────────

pub struct ProcessBackend;

impl ProcessBackend {
    pub fn new() -> Self {
        Self
    }

    pub(crate) fn create_session_with_command_env(
        &self,
        config: &SessionConfig,
        apply_command_env: impl FnOnce(&mut Command),
    ) -> Result<SessionHandle, String> {
        // 1. Ensure output file exists (empty)
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.output_path)
            .map_err(|e| format!("Failed to create output file: {}", e))?;

        // 2. Build wrapper command args
        let mut args = vec![
            config.wrapper_subcommand.clone(),
            "--output-file".to_string(),
            config.output_path.clone(),
            "--input-fifo".to_string(),
            // Pipe mode doesn't use a FIFO, but the wrapper CLI still requires
            // this arg.  Use a path under the runtime temp dir so cleanup's
            // remove_file() can never hit a real user file.
            {
                #[cfg(unix)]
                {
                    crate::services::tmux_common::session_temp_path(
                        &config.session_name,
                        "unused-fifo",
                    )
                }
                #[cfg(not(unix))]
                {
                    let tmp = std::env::temp_dir()
                        .join(format!("agentdesk-{}-unused-fifo", config.session_name));
                    tmp.display().to_string()
                }
            },
            "--prompt-file".to_string(),
            config.prompt_path.clone(),
            "--cwd".to_string(),
            config.working_dir.clone(),
            "--input-mode".to_string(),
            "pipe".to_string(),
        ];
        args.extend(config.wrapper_args.clone());

        // 3. Spawn wrapper directly as child process.
        // Create a new process group so kill_pid_tree(-pid) can clean up
        // the entire subtree (wrapper + Claude/Codex child) on cancel.
        let mut cmd = Command::new(&config.agentdesk_exe);
        cmd.args(&args)
            .envs(config.env_vars.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::null()) // wrapper writes to file, not stdout
            .stderr(Stdio::inherit()); // show wrapper logs
        apply_command_env(&mut cmd);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0); // new process group = wrapper PID
        }

        #[cfg(not(unix))]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
            // CREATE_NO_WINDOW gives the wrapper a hidden console that children
            // inherit. Without this, every cmd.exe spawned by Claude/Codex
            // creates its own *visible* console window when the parent process
            // has no console (e.g. running as a Windows service via NSSM).
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn wrapper process: {}", e))?;

        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture child stdin".to_string())?;

        Ok(SessionHandle::Process {
            child_stdin: Arc::new(Mutex::new(Some(stdin))),
            child: Arc::new(Mutex::new(Some(child))),
            pid,
        })
    }
}

impl SessionBackend for ProcessBackend {
    fn create_session(&self, config: &SessionConfig) -> Result<SessionHandle, String> {
        self.create_session_with_command_env(config, |_| {})
    }

    fn send_input(&self, handle: &SessionHandle, message: &str) -> Result<(), String> {
        match handle {
            SessionHandle::Process { child_stdin, .. } => {
                let mut guard = child_stdin
                    .lock()
                    .map_err(|e| format!("stdin lock poisoned: {}", e))?;
                if let Some(ref mut stdin) = *guard {
                    writeln!(stdin, "{}", message)
                        .map_err(|e| format!("Failed to write to child stdin: {}", e))?;
                    stdin
                        .flush()
                        .map_err(|e| format!("Failed to flush child stdin: {}", e))?;
                    Ok(())
                } else {
                    Err("Child stdin already closed".to_string())
                }
            }
            #[cfg(test)]
            SessionHandle::TestProcess { alive, .. } => {
                if alive.load(std::sync::atomic::Ordering::Relaxed) {
                    Ok(())
                } else {
                    Err("test process is stopped".to_string())
                }
            }
        }
    }

    fn is_alive(&self, handle: &SessionHandle) -> bool {
        match handle {
            SessionHandle::Process { child, .. } => {
                let mut guard = match child.lock() {
                    Ok(g) => g,
                    Err(_) => return false,
                };
                if let Some(ref mut c) = *guard {
                    matches!(c.try_wait(), Ok(None))
                } else {
                    false
                }
            }
            #[cfg(test)]
            SessionHandle::TestProcess { alive, .. } => {
                alive.load(std::sync::atomic::Ordering::Relaxed)
            }
        }
    }
}

impl SessionHandle {
    pub fn pid(&self) -> u32 {
        match self {
            SessionHandle::Process { pid, .. } => *pid,
            #[cfg(test)]
            SessionHandle::TestProcess { pid, .. } => *pid,
        }
    }
}

static PROCESS_SESSIONS: LazyLock<Mutex<ProcessSessionRegistry>> =
    LazyLock::new(|| Mutex::new(ProcessSessionRegistry::default()));

fn process_sessions() -> MutexGuard<'static, ProcessSessionRegistry> {
    PROCESS_SESSIONS.lock().unwrap_or_else(|error| {
        tracing::warn!("Recovered poisoned PROCESS_SESSIONS mutex; continuing with inner state");
        error.into_inner()
    })
}

pub struct ProcessSessionActiveTurnGuard {
    session_name: String,
}

impl Drop for ProcessSessionActiveTurnGuard {
    fn drop(&mut self) {
        let mut registry = process_sessions();
        if let Some(entry) = registry.handles.get_mut(&self.session_name) {
            entry.active_turns = entry.active_turns.saturating_sub(1);
        }
    }
}

pub fn mark_process_session_active_turn(
    session_name: impl Into<String>,
) -> ProcessSessionActiveTurnGuard {
    let session_name = session_name.into();
    let mut registry = process_sessions();
    if let Some(entry) = registry.handles.get_mut(&session_name) {
        entry.active_turns = entry.active_turns.saturating_add(1);
    }
    drop(registry);
    ProcessSessionActiveTurnGuard { session_name }
}

pub fn insert_process_session(session_name: impl Into<String>, handle: SessionHandle) {
    process_sessions().insert(session_name.into(), handle);
}

pub fn insert_process_session_and_mark_active_turn(
    session_name: impl Into<String>,
    handle: SessionHandle,
) -> ProcessSessionActiveTurnGuard {
    let session_name = session_name.into();
    let mut registry = process_sessions();
    registry.insert_with_active_turns(session_name.clone(), handle, 1);
    drop(registry);
    ProcessSessionActiveTurnGuard { session_name }
}

pub fn remove_process_session(session_name: &str) -> Option<SessionHandle> {
    process_sessions()
        .handles
        .remove(session_name)
        .map(|entry| entry.handle)
}

pub fn terminate_process_session(session_name: &str) -> bool {
    let Some(handle) = remove_process_session(session_name) else {
        return false;
    };
    terminate_process_handle(handle);
    true
}

pub fn terminate_process_session_before_tmux(session_name: &str) -> bool {
    let mut registry = process_sessions();
    if registry.stopped.contains(session_name) {
        let handle = registry
            .handles
            .remove(session_name)
            .map(|entry| entry.handle);
        drop(registry);
        if let Some(handle) = handle {
            terminate_process_handle(handle);
            return true;
        }
        return false;
    }

    let Some(entry) = registry.handles.get(session_name) else {
        return false;
    };
    if entry.active_turns > 0 && ProcessBackend::new().is_alive(&entry.handle) {
        tracing::warn!(
            session_name = session_name,
            wrapper_pid = entry.handle.pid(),
            "skipping ProcessBackend cleanup before tmux because wrapper is hosting an active turn"
        );
        return false;
    }

    let Some(entry) = registry.handles.remove(session_name) else {
        return false;
    };
    drop(registry);
    terminate_process_handle(entry.handle);
    true
}

pub fn process_session_was_stopped(session_name: &str) -> bool {
    process_sessions().stopped.contains(session_name)
}

#[cfg(test)]
fn mark_process_session_stopped(session_name: impl Into<String>) {
    process_sessions().mark_stopped(session_name.into());
}

pub fn mark_process_sessions_stopped_by_pid(pid: u32) -> Vec<String> {
    let mut registry = process_sessions();
    let session_names = registry
        .handles
        .iter()
        .filter_map(|(session_name, entry)| {
            if entry.handle.pid() != pid {
                return None;
            }
            if !entry.identity.matches(pid) {
                tracing::warn!(
                    "process backend stopped marker skipped: session={} pid={} reason=identity_mismatch",
                    session_name,
                    pid
                );
                return None;
            }
            Some(session_name.clone())
        })
        .collect::<Vec<_>>();

    let mut stopped_handles = Vec::new();
    for session_name in &session_names {
        if let Some(entry) = registry.handles.remove(session_name) {
            stopped_handles.push(entry.handle);
        }
        registry.mark_stopped(session_name.clone());
    }
    drop(registry);

    for handle in stopped_handles {
        reap_stopped_process_handle(handle);
    }

    session_names
}

pub fn process_session_pid(session_name: &str) -> Option<u32> {
    let registry = process_sessions();
    if registry.stopped.contains(session_name) {
        return None;
    }
    registry
        .handles
        .get(session_name)
        .map(|entry| entry.handle.pid())
}

pub fn process_session_is_alive(session_name: &str) -> bool {
    let registry = process_sessions();
    if registry.stopped.contains(session_name) {
        return false;
    }
    registry
        .handles
        .get(session_name)
        .map(|entry| ProcessBackend::new().is_alive(&entry.handle))
        .unwrap_or(false)
}

pub fn process_session_available_for_followup(session_name: &str) -> bool {
    if process_session_was_stopped(session_name) {
        if let Some(handle) = remove_process_session(session_name) {
            terminate_process_handle(handle);
        }
        return false;
    }
    process_session_is_alive(session_name)
}

pub fn send_process_session_input(session_name: &str, message: &str) -> Result<(), String> {
    let registry = process_sessions();
    if registry.stopped.contains(session_name) {
        return Err(format!("Process session {session_name} was stopped"));
    }
    let handle = registry
        .handles
        .get(session_name)
        .ok_or_else(|| format!("No process handle found for session {}", session_name))?;
    ProcessBackend::new().send_input(&handle.handle, message)
}

pub fn process_session_probe(session_name: &str) -> SessionProbe {
    let session_name = session_name.to_string();
    SessionProbe::process(move || process_session_is_alive(&session_name))
}

pub fn terminate_process_handle(handle: SessionHandle) {
    match handle {
        SessionHandle::Process {
            child_stdin, child, ..
        } => {
            if let Ok(mut stdin_guard) = child_stdin.lock() {
                let _ = stdin_guard.take();
            }
            let mut child_guard = match child.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            if let Some(mut process) = child_guard.take() {
                let _ = process.kill();
                let _ = process.wait();
            }
        }
        #[cfg(test)]
        SessionHandle::TestProcess { alive, .. } => {
            alive.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn reap_stopped_process_handle(handle: SessionHandle) {
    match handle {
        SessionHandle::Process {
            child_stdin,
            child,
            pid,
        } => {
            let _ = std::thread::Builder::new()
                .name(format!("process-session-reaper-{pid}"))
                .spawn(move || {
                    if let Ok(mut stdin_guard) = child_stdin.lock() {
                        let _ = stdin_guard.take();
                    }
                    let mut child_guard = match child.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    if let Some(mut process) = child_guard.take() {
                        let _ = process.wait();
                    }
                });
        }
        #[cfg(test)]
        SessionHandle::TestProcess { alive, .. } => {
            alive.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod process_registry_stop_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn issue_4112_stopped_process_session_by_pid_is_removed_and_guarded_from_followup() {
        let session_name = format!("process-stop-{}", uuid::Uuid::new_v4());
        let alive = Arc::new(AtomicBool::new(true));
        insert_process_session(
            session_name.clone(),
            SessionHandle::TestProcess {
                pid: 424_242,
                alive: alive.clone(),
            },
        );

        assert!(process_session_is_alive(&session_name));
        assert_eq!(process_session_pid(&session_name), Some(424_242));

        let stopped = mark_process_sessions_stopped_by_pid(424_242);

        assert_eq!(stopped, vec![session_name.clone()]);
        assert!(process_session_was_stopped(&session_name));
        assert!(!process_session_is_alive(&session_name));
        assert_eq!(process_session_pid(&session_name), None);
        assert!(send_process_session_input(&session_name, "{}").is_err());

        insert_process_session(
            session_name.clone(),
            SessionHandle::TestProcess {
                pid: 424_243,
                alive: Arc::new(AtomicBool::new(true)),
            },
        );
        assert!(!process_session_was_stopped(&session_name));
        assert!(process_session_is_alive(&session_name));

        if let Some(handle) = remove_process_session(&session_name) {
            terminate_process_handle(handle);
        }
        assert!(!process_session_is_alive(&session_name));
        assert!(!process_session_was_stopped(&session_name));
    }

    #[test]
    fn issue_4112_stopped_process_session_followup_guard_forces_fresh_session() {
        let session_name = format!("process-followup-stop-{}", uuid::Uuid::new_v4());
        let alive = Arc::new(AtomicBool::new(true));
        insert_process_session(
            session_name.clone(),
            SessionHandle::TestProcess {
                pid: 424_244,
                alive: alive.clone(),
            },
        );
        assert!(process_session_available_for_followup(&session_name));

        mark_process_session_stopped(session_name.clone());

        assert!(
            !process_session_available_for_followup(&session_name),
            "stopped pipe session must cold-start instead of accepting a follow-up"
        );
        assert!(process_session_was_stopped(&session_name));
        assert!(!alive.load(Ordering::Relaxed));
        assert!(
            crate::services::tmux_diagnostics::should_recreate_session_after_stdin_error(&format!(
                "Process session {session_name} was stopped"
            )),
            "Claude ProcessBackend follow-up maps stopped pipe sessions to cold-start recreation"
        );

        insert_process_session(
            session_name.clone(),
            SessionHandle::TestProcess {
                pid: 424_245,
                alive,
            },
        );
        if let Some(handle) = remove_process_session(&session_name) {
            terminate_process_handle(handle);
        }
    }

    #[test]
    fn issue_4113_process_session_cleanup_terminates_registered_wrapper() {
        let session_name = format!("process-tmux-return-cleanup-{}", uuid::Uuid::new_v4());
        let alive = Arc::new(AtomicBool::new(true));
        insert_process_session(
            session_name.clone(),
            SessionHandle::TestProcess {
                pid: 424_246,
                alive: alive.clone(),
            },
        );

        assert!(process_session_is_alive(&session_name));
        assert!(terminate_process_session(&session_name));

        assert!(!alive.load(Ordering::Relaxed));
        assert!(!process_session_is_alive(&session_name));
        assert!(
            !terminate_process_session(&session_name),
            "second cleanup is idempotent after registry removal"
        );
    }

    #[test]
    fn issue_4134_tmux_cleanup_terminates_idle_alive_wrapper_without_active_turn() {
        let session_name = format!("process-tmux-idle-cleanup-{}", uuid::Uuid::new_v4());
        let alive = Arc::new(AtomicBool::new(true));
        insert_process_session(
            session_name.clone(),
            SessionHandle::TestProcess {
                pid: 424_247,
                alive: alive.clone(),
            },
        );

        assert!(terminate_process_session_before_tmux(&session_name));
        assert!(!alive.load(Ordering::Relaxed));
        assert!(!process_session_is_alive(&session_name));
        assert!(
            !terminate_process_session_before_tmux(&session_name),
            "second cleanup is idempotent after registry removal"
        );
    }

    #[test]
    fn issue_4134_tmux_cleanup_skips_inflight_wrapper_without_stopped_marker() {
        let session_name = format!("process-tmux-active-skip-{}", uuid::Uuid::new_v4());
        let alive = Arc::new(AtomicBool::new(true));
        insert_process_session(
            session_name.clone(),
            SessionHandle::TestProcess {
                pid: 424_248,
                alive: alive.clone(),
            },
        );
        let active_turn = mark_process_session_active_turn(&session_name);

        assert!(!terminate_process_session_before_tmux(&session_name));
        assert!(alive.load(Ordering::Relaxed));
        assert!(process_session_is_alive(&session_name));

        drop(active_turn);
        assert!(terminate_process_session_before_tmux(&session_name));
        assert!(!alive.load(Ordering::Relaxed));
        assert!(!process_session_is_alive(&session_name));
    }

    #[test]
    fn issue_4134_insert_process_session_active_turn_skips_tmux_cleanup_until_guard_drop() {
        let session_name = format!("process-tmux-insert-active-skip-{}", uuid::Uuid::new_v4());
        let alive = Arc::new(AtomicBool::new(true));
        let active_turn = insert_process_session_and_mark_active_turn(
            session_name.clone(),
            SessionHandle::TestProcess {
                pid: 424_250,
                alive: alive.clone(),
            },
        );

        assert!(!terminate_process_session_before_tmux(&session_name));
        assert!(alive.load(Ordering::Relaxed));
        assert!(process_session_is_alive(&session_name));

        drop(active_turn);
        assert!(terminate_process_session_before_tmux(&session_name));
        assert!(!alive.load(Ordering::Relaxed));
        assert!(!process_session_is_alive(&session_name));
    }

    #[test]
    fn issue_4134_tmux_cleanup_terminates_stopped_wrapper() {
        let session_name = format!("process-tmux-stopped-cleanup-{}", uuid::Uuid::new_v4());
        let alive = Arc::new(AtomicBool::new(true));
        insert_process_session(
            session_name.clone(),
            SessionHandle::TestProcess {
                pid: 424_249,
                alive: alive.clone(),
            },
        );
        mark_process_session_stopped(session_name.clone());

        assert!(terminate_process_session_before_tmux(&session_name));
        assert!(!alive.load(Ordering::Relaxed));
        assert!(!process_session_is_alive(&session_name));
        assert!(process_session_was_stopped(&session_name));
    }
}

/// #3405: stream-line state machine cluster — `StreamLineState`/`TaskStartInfo`,
/// `process_stream_line`, and the synchronous envelope parsers — split verbatim
/// into the `stream_line` child. Re-exported below so every existing
/// `crate::services::session_backend::X` path keeps resolving unchanged.
#[path = "session_backend/stream_line.rs"]
mod stream_line;
pub use stream_line::{StreamLineState, parse_assistant_extra_tool_uses, process_stream_line};
pub(crate) use stream_line::{
    classify_task_notification_kind, emit_status_events_from_stream_json, observe_stream_context,
    parse_stream_message_with_state,
};

/// #3405: terminal-usage adoption (#3344 provenance/magnitude gate) plus the
/// analytics re-parser entry points, split verbatim into the `terminal_usage`
/// child. Re-exported below to preserve the public surface; the two children
/// reach each other (`process_stream_line` ↔ `adopt_terminal_result_usage`)
/// through these root re-exports via their `use super::*`.
#[path = "session_backend/terminal_usage.rs"]
mod terminal_usage;
pub use terminal_usage::{
    adopt_terminal_result_usage, extract_turn_analytics_from_output,
    extract_turn_analytics_from_output_range,
};

/// #3281 observability-only summary of what one transcript read forwarded to
/// the bridge (status telemetry and `Done` terminators are NOT counted):
/// `forwarded_messages == 0` on a `Completed` read means a zero-harvest
/// transcript window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadHarvestStats {
    pub forwarded_messages: u64,
    pub assistant_text_bytes: u64,
}

pub fn read_output_file_until_result(
    output_path: &str,
    start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    probe: SessionProbe,
) -> Result<ReadOutputResult, String> {
    read_output_file_until_result_tracked(output_path, start_offset, sender, cancel_token, probe)
        .map_err(|failure| failure.error)
}

pub fn read_output_file_until_result_tracked(
    output_path: &str,
    start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    probe: SessionProbe,
) -> Result<ReadOutputResult, ReadOutputFailure> {
    read_output_file_until_result_with_harvest(
        output_path,
        start_offset,
        sender,
        cancel_token,
        probe,
    )
    .map(|(result, _stats)| result)
}

/// #3281: full reader that also returns the harvest counters (truthful TUI
/// producer-exit `lines=` logging).
pub fn read_output_file_until_result_with_harvest(
    output_path: &str,
    start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    probe: SessionProbe,
) -> Result<(ReadOutputResult, ReadHarvestStats), ReadOutputFailure> {
    let mut state = StreamLineState::new();
    let SessionProbe {
        is_alive,
        is_ready_for_input,
    } = probe;
    let last_offset = Arc::new(AtomicU64::new(start_offset));
    let offset_sender = sender.clone();
    let line_sender = sender.clone();
    let synthetic_sender = sender.clone();
    let error_sender = sender.clone();
    let last_offset_for_emit = last_offset.clone();

    let result = crate::services::provider::poll_output_file_until_result(
        output_path,
        start_offset,
        cancel_token,
        &mut state,
        move || is_alive(),
        move || is_ready_for_input(),
        move |offset| {
            last_offset_for_emit.store(offset, Ordering::Relaxed);
            let _ = offset_sender.send(StreamMessage::OutputOffset { offset });
        },
        move |line, state| process_stream_line(line, &line_sender, state),
        |state| state.final_result.is_some(),
        move |state| {
            synthetic_sender
                .send(StreamMessage::Done {
                    result: String::new(),
                    session_id: state.last_session_id.clone(),
                })
                .is_ok()
        },
        move |state| {
            if let Some((message, stdout_raw)) = &state.stdout_error {
                let _ = error_sender.send(StreamMessage::Error {
                    message: message.clone(),
                    stdout: stdout_raw.clone(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
        },
    );

    let stats = ReadHarvestStats {
        forwarded_messages: state.forwarded_message_count,
        assistant_text_bytes: state.forwarded_assistant_text_bytes,
    };
    match result {
        Ok(result) => Ok((result, stats)),
        Err(error) => Err(ReadOutputFailure {
            error,
            last_offset: last_offset.load(Ordering::Relaxed),
        }),
    }
}

#[cfg(test)]
mod stream_tail_guard_tests {
    use super::*;
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::provider::{ReadOutputResult, SessionProbe};
    use std::io::Write;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn read_output_file_until_result_buffers_split_jsonl_line() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("stream.jsonl");
        let assistant_line =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello world"}]}}"#;
        let split_at = assistant_line.find("lo world").unwrap();
        let first_chunk = &assistant_line[..split_at];
        let second_chunk = format!(
            "{}\n{}\n",
            &assistant_line[split_at..],
            r#"{"type":"result","subtype":"success","result":"done","session_id":"sess-1"}"#
        );
        std::fs::write(&output_path, first_chunk).unwrap();

        let (sender, receiver) = mpsc::channel();
        let reader_path = output_path.to_string_lossy().into_owned();
        let reader = thread::spawn(move || {
            read_output_file_until_result(
                &reader_path,
                0,
                sender,
                None,
                SessionProbe::process(|| true),
            )
        });

        thread::sleep(Duration::from_millis(50));
        assert!(receiver.try_recv().is_err());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&output_path)
            .unwrap()
            .write_all(second_chunk.as_bytes())
            .unwrap();

        let result = reader.join().unwrap().unwrap();
        assert_eq!(
            result,
            ReadOutputResult::Completed {
                offset: (first_chunk.len() + second_chunk.len()) as u64
            }
        );
        let messages = receiver.try_iter().collect::<Vec<_>>();
        assert!(messages.iter().any(
            |message| matches!(message, StreamMessage::Text { content } if content == "hello world")
        ));
        assert!(messages.iter().any(
            |message| matches!(message, StreamMessage::Done { result, .. } if result == "done")
        ));
    }

    /// #3281 forensics pinned as code: the 2026-06-10 incident transcript shape
    /// (user → attachment → assistant thinking → assistant text → attachment
    /// hook record → `system{stop_hook_summary}` → trailing housekeeping). The
    /// producer MUST harvest the assistant text and the `Done` terminator via
    /// the `has_final` fast path — i.e. in the incident scenario the producer
    /// forwarded the response to the bridge; the loss was downstream.
    #[test]
    fn incident_shape_transcript_harvests_text_and_done() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("incident.jsonl");
        std::fs::write(
            &output_path,
            concat!(
                r#"{"type":"user","timestamp":"2026-06-10T01:34:29.797Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"type":"attachment","attachment":{"kind":"queued-prompt"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-06-10T01:34:35.000Z","message":{"content":[{"type":"thinking","thinking":"considering"}]}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-06-10T01:34:39.943Z","message":{"model":"claude-x","content":[{"type":"text","text":"Hello! How can I help you today?"}],"usage":{"input_tokens":10,"output_tokens":9}}}"#,
                "\n",
                r#"{"type":"attachment","attachment":{"kind":"hook_success"}}"#,
                "\n",
                r#"{"type":"system","subtype":"stop_hook_summary","timestamp":"2026-06-10T01:34:40.095Z","session_id":"297e6f2f-test"}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","durationMs":10298}"#,
                "\n",
                r#"{"type":"last-prompt","prompt":"hi"}"#,
                "\n",
                r#"{"type":"ai-title","title":"greeting"}"#,
                "\n",
                r#"{"type":"mode","mode":"default"}"#,
                "\n",
                r#"{"type":"permission-mode","mode":"default"}"#,
                "\n",
            ),
        )
        .unwrap();

        let (sender, receiver) = mpsc::channel();
        let (result, stats) = read_output_file_until_result_with_harvest(
            output_path.to_string_lossy().as_ref(),
            0,
            sender,
            None,
            SessionProbe::process(|| true),
        )
        .unwrap();

        assert!(
            matches!(result, ReadOutputResult::Completed { .. }),
            "stop_hook_summary must complete the read via has_final: {result:?}"
        );
        assert!(
            stats.forwarded_messages > 0,
            "incident-shape turn must forward harvested messages: {stats:?}"
        );
        assert!(
            stats.assistant_text_bytes > 0,
            "incident-shape turn must harvest assistant text bytes: {stats:?}"
        );
        let messages: Vec<_> = receiver.try_iter().collect();
        let text_index = messages.iter().position(|message| {
            matches!(message, StreamMessage::Text { content } if content == "Hello! How can I help you today?")
        });
        let done_index = messages
            .iter()
            .position(|message| matches!(message, StreamMessage::Done { .. }));
        let (Some(text_index), Some(done_index)) = (text_index, done_index) else {
            panic!("expected Text then Done, got: {messages:?}");
        };
        assert!(
            text_index < done_index,
            "response text must reach the bridge BEFORE the Done terminator: {messages:?}"
        );
    }

    /// #3281: a tool_use-only turn forwards messages but zero assistant text
    /// bytes — the zero-harvest health gate must therefore key on
    /// `forwarded_messages`, not on `assistant_text_bytes`.
    #[test]
    fn tool_use_only_turn_forwards_messages_without_text_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("tool-only.jsonl");
        std::fs::write(
            &output_path,
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu-1","name":"Bash","input":{"command":"ls"}}]}}"#,
                "\n",
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu-1","content":"ok"}]}}"#,
                "\n",
                r#"{"type":"result","subtype":"success","result":"","session_id":"sess-tool"}"#,
                "\n",
            ),
        )
        .unwrap();

        let (sender, _receiver) = mpsc::channel();
        let (result, stats) = read_output_file_until_result_with_harvest(
            output_path.to_string_lossy().as_ref(),
            0,
            sender,
            None,
            SessionProbe::process(|| true),
        )
        .unwrap();

        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        assert!(
            stats.forwarded_messages > 0,
            "tool_use-only turn still forwards messages: {stats:?}"
        );
        assert_eq!(
            stats.assistant_text_bytes, 0,
            "tool_use-only turn has no assistant text bytes: {stats:?}"
        );
    }

    /// #3292: a window that starts after assistant text and sees only the
    /// `stop_hook_summary` terminator must still forward `Done` to the bridge,
    /// but count as zero-harvest so the observability event can see it.
    #[test]
    fn done_only_window_forwards_done_but_counts_zero_harvest() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("done-only.jsonl");
        std::fs::write(
            &output_path,
            concat!(
                r#"{"type":"system","subtype":"stop_hook_summary","session_id":"sess-done"}"#,
                "\n",
            ),
        )
        .unwrap();

        let (sender, receiver) = mpsc::channel();
        let (result, stats) = read_output_file_until_result_with_harvest(
            output_path.to_string_lossy().as_ref(),
            0,
            sender,
            None,
            SessionProbe::process(|| true),
        )
        .unwrap();

        assert!(matches!(result, ReadOutputResult::Completed { .. }));
        assert_eq!(stats.forwarded_messages, 0);
        assert_eq!(stats.assistant_text_bytes, 0);
        let messages: Vec<_> = receiver.try_iter().collect();
        assert!(
            messages.iter().any(
                |message| matches!(message, StreamMessage::Done { session_id, .. } if session_id.as_deref() == Some("sess-done"))
            ),
            "Done still reaches the bridge: {messages:?}"
        );
    }
}
