//! Session backend for managing AI provider (Claude/Codex) processes.
//!
//! `ProcessBackend`: Runs wrapper as a direct child process with stdin pipe (cross-platform).
//! Simpler but sessions die with dcserver. Recovery via `claude --resume`.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

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
    /// Provider-specific wrapper args (e.g., --codex-bin, -- claude ...)
    pub wrapper_args: Vec<String>,
    /// Whether this is a codex session (uses codex-tmux-wrapper subcommand)
    pub is_codex: bool,
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
}

impl SessionBackend for ProcessBackend {
    fn create_session(&self, config: &SessionConfig) -> Result<SessionHandle, String> {
        // 1. Ensure output file exists (empty)
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.output_path)
            .map_err(|e| format!("Failed to create output file: {}", e))?;

        // 2. Build wrapper command args
        let wrapper_flag = if config.is_codex {
            "codex-tmux-wrapper"
        } else {
            "tmux-wrapper"
        };

        let mut args = vec![
            wrapper_flag.to_string(),
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
        }
    }
}
