//! Bounded tmux effects for the input actor: capture, then paste and Enter.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use super::super::bounded_tmux::{BoundedTmuxError, run_with_budget};
use super::gate::{PaneVerdict, judge_pane, own_draft};
use crate::services::tui_o::shadow::ShadowProvider;

/// Larger prompts are refused before any tmux call.
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
const CAPTURE_SCROLLBACK: &str = "-80";
const BEFORE_ENTER_SETTLE: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// No pane mutation can have happened; the input may be offered again.
    NotSent(String),
    /// Nothing more is sent: the input may already sit in the composer or be submitted.
    Indeterminate(String),
    /// The input can never be sent as is; nothing was sent.
    Refused(String),
}

pub trait Pane {
    fn capture(&mut self) -> Result<String, String>;
    fn submit(&mut self, text: &str) -> SendOutcome;
    fn with_composer<R>(&mut self, operation: impl FnOnce(&mut Self) -> R) -> Option<R> {
        Some(operation(self))
    }
    fn execution_nonce(&self) -> Option<String>;
}

pub struct TmuxPane {
    session: String,
    program: PathBuf,
    budget: Duration,
    provider: ShadowProvider,
    pre_empty: bool,
}

impl TmuxPane {
    pub fn new(session: &str) -> Self {
        Self::with_program(session, PathBuf::from("tmux"), Duration::from_secs(5))
    }

    pub(crate) fn with_program(session: &str, program: PathBuf, budget: Duration) -> Self {
        Self {
            session: session.to_string(),
            program,
            budget,
            provider: ShadowProvider::Claude,
            pre_empty: false,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command.arg("-u").args(args);
        crate::services::platform::binary_resolver::apply_runtime_path(&mut command);
        command
    }

    fn target(&self) -> String {
        format!("={}:", self.session)
    }

    fn run(&self, args: &[&str]) -> Result<std::process::Output, BoundedTmuxError> {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(BoundedTmuxError::Spawn)?
                        .block_on(run_with_budget(&mut self.command(args), self.budget))
                })
                .join()
                .unwrap_or_else(|_| {
                    Err(BoundedTmuxError::Spawn(std::io::Error::other(
                        "tmux worker panicked",
                    )))
                })
        })
    }

    pub fn for_provider(session: &str, provider: ShadowProvider) -> Self {
        let mut pane = Self::new(session);
        pane.provider = provider;
        pane
    }

    fn submit_inner(&mut self, text: &str) -> SendOutcome {
        if text.len() > MAX_PROMPT_BYTES {
            return SendOutcome::Refused(format!("prompt exceeds {MAX_PROMPT_BYTES} bytes"));
        }
        if text.trim().is_empty() {
            return SendOutcome::Refused("empty prompt".into());
        }
        // Bounded tmux closes stdin, so load-buffer reads the prompt from a file.
        let mut file = match tempfile::NamedTempFile::new() {
            Ok(file) => file,
            Err(error) => return SendOutcome::NotSent(error.to_string()),
        };
        if let Err(error) = file.write_all(text.as_bytes()).and_then(|()| file.flush()) {
            return SendOutcome::NotSent(error.to_string());
        }
        let nonce = self.execution_nonce();
        let buffer = format!("agentdesk-input-{}", uuid::Uuid::new_v4());
        let path = file.path().to_string_lossy().into_owned();
        // Loading a buffer never touches the pane, so every failure here is NotSent.
        match self.run(&["load-buffer", "-b", &buffer, &path]) {
            Ok(output) if output.status.success() => {}
            Ok(output) => return SendOutcome::NotSent(stderr_of(&output)),
            Err(error) => return SendOutcome::NotSent(error.to_string()),
        }
        let target = self.target();
        let paste = [
            "paste-buffer",
            "-p",
            "-r",
            "-d",
            "-b",
            &buffer,
            "-t",
            &target,
        ];
        match self.run(&paste) {
            Ok(output) if output.status.success() => {}
            // tmux rejects a missing pane or buffer before pasting anything.
            Ok(output) => return SendOutcome::Indeterminate(stderr_of(&output)),
            Err(error) if error.may_have_effect() => {
                return SendOutcome::Indeterminate(error.to_string());
            }
            Err(error) => return SendOutcome::NotSent(error.to_string()),
        }
        std::thread::sleep(BEFORE_ENTER_SETTLE);
        let Ok(after) = self.capture() else {
            return SendOutcome::Indeterminate("post-paste capture unavailable".into());
        };
        if self.execution_nonce() != nonce
            || !own_draft(self.provider, &after, text, self.pre_empty)
        {
            return SendOutcome::Indeterminate("own draft or modal changed".into());
        }
        // The paste landed, so any Enter failure leaves our text in the composer.
        match self.run(&["send-keys", "-t", &target, "Enter"]) {
            Ok(output) if output.status.success() => SendOutcome::Sent,
            Ok(output) => SendOutcome::Indeterminate(stderr_of(&output)),
            Err(error) => SendOutcome::Indeterminate(error.to_string()),
        }
    }
}

fn stderr_of(output: &std::process::Output) -> String {
    format!(
        "tmux exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

impl Pane for TmuxPane {
    fn capture(&mut self) -> Result<String, String> {
        let output = self
            .run(&[
                "capture-pane",
                "-p",
                "-e",
                "-t",
                &self.session,
                "-S",
                CAPTURE_SCROLLBACK,
            ])
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(stderr_of(&output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn submit(&mut self, text: &str) -> SendOutcome {
        self.submit_inner(text)
    }

    fn execution_nonce(&self) -> Option<String> {
        use crate::services::tui_prompt_dedupe::binding_context::{
            SpawnNonceMarker, observe_spawn_nonce_marker,
        };
        match observe_spawn_nonce_marker(&self.session) {
            SpawnNonceMarker::Known(n) => Some(n),
            _ => None,
        }
    }

    fn with_composer<R>(&mut self, operation: impl FnOnce(&mut Self) -> R) -> Option<R> {
        let session = self.session.clone();
        let provider = self.provider;
        let callback = || {
            self.pre_empty = self
                .capture()
                .is_ok_and(|c| judge_pane(provider, &c) == PaneVerdict::Ready);
            operation(self)
        };
        match provider {
            ShadowProvider::Claude => {
                crate::services::claude_tui::composer_lock::try_with_composer_mutation_lock(
                    &session, callback,
                )
            }
            ShadowProvider::Codex => {
                crate::services::codex_tui::input::try_with_composer_mutation_lock(
                    &session, callback,
                )
            }
        }
    }
}
