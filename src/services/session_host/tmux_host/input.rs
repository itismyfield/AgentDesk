use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use super::{TmuxHost, tmux_key_name};
use crate::services::platform::{binary_resolver::runtime_command, tmux};
use crate::services::session_host::InteractiveSessionHost;
use crate::services::session_host::submit::{
    AuthorizedHostTarget, InputSource, KeyOutcome, PinnedInput, SubmitOutcome, SubmitPayload,
    Target, TmuxInputMode,
};
use crate::services::tui_input::bounded_tmux::{
    BoundedTmuxError, run_bounded_tmux, run_with_budget,
};
use crate::services::tui_input::writer_lease;

impl TmuxHost {
    pub(crate) fn capture_draft_with(
        &self,
        program: impl AsRef<std::ffi::OsStr>,
        session: &str,
        timeout: Duration,
    ) -> Option<String> {
        use crate::services::process::{configure_child_process_group, wait_with_output_timeout};
        let mut command = runtime_command(program).ok()?;
        let args = ["capture-pane", "-p", "-e", "-t", session, "-S", "-80"];
        tmux::input_command(&mut command, &args).ok()?;
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        configure_child_process_group(&mut command);
        let child = command.spawn().ok()?;
        let output = wait_with_output_timeout(child, timeout, "tmux capture-pane -e").ok()?;
        let text = || String::from_utf8_lossy(&output.stdout).into_owned();
        output.status.success().then(text)
    }

    pub(crate) fn first_key(&self) -> bool {
        crate::services::claude_tui::guarded_outcome::first_key()
    }

    pub(crate) fn run_submission(&self, args: &[&str]) -> Result<Output, String> {
        run_submission(args)
    }
    pub(crate) fn run_actor(
        &self,
        program: &Path,
        session: &str,
        budget: Duration,
        args: &[&str],
    ) -> Result<Output, BoundedTmuxError> {
        run_actor(program, session, budget, args)
    }
    pub(crate) fn run_busy(
        &self,
        program: &Path,
        target: &str,
        args: &[&str],
    ) -> Result<Output, BoundedTmuxError> {
        run_busy(program, target, args)
    }
    fn submit_tmux(
        &self,
        session: &str,
        mode: TmuxInputMode<'_>,
        source: InputSource,
        payload: SubmitPayload<'_>,
    ) -> SubmitOutcome {
        self.submit(
            AuthorizedHostTarget {
                target: Target::Tmux { session, mode },
                source,
            },
            payload,
        )
    }

    pub(crate) fn platform_input(
        &self,
        session: &str,
        source: InputSource,
        payload: SubmitPayload<'_>,
    ) -> SubmitOutcome {
        self.submit_tmux(session, TmuxInputMode::Platform, source, payload)
    }

    pub(crate) fn load_buffer(&self, buffer: &str, text: &str) -> Result<Output, String> {
        self.submit(
            AuthorizedHostTarget {
                target: Target::Buffer(TmuxInputMode::Platform),
                source: InputSource::Inherited,
            },
            SubmitPayload::LoadText { buffer, text },
        )
        .into_output()
    }

    pub(crate) fn submission_input(
        &self,
        session: &str,
        payload: SubmitPayload<'_>,
    ) -> SubmitOutcome {
        self.submit_tmux(
            session,
            TmuxInputMode::Submission,
            InputSource::Inherited,
            payload,
        )
    }

    pub(crate) fn actor_input(
        &self,
        program: &Path,
        session: &str,
        budget: Duration,
        payload: SubmitPayload<'_>,
    ) -> SubmitOutcome {
        self.submit_tmux(
            session,
            TmuxInputMode::Actor { program, budget },
            InputSource::Inherited,
            payload,
        )
    }

    pub(crate) fn busy_input(
        &self,
        program: &Path,
        target: &str,
        payload: SubmitPayload<'_>,
    ) -> SubmitOutcome {
        let session = target.trim_start_matches('=').trim_end_matches(':');
        self.submit_tmux(
            session,
            TmuxInputMode::Busy { program, target },
            InputSource::Inherited,
            payload,
        )
    }
}

pub(in crate::services::session_host) fn submit(
    target: Target<'_>,
    payload: SubmitPayload<'_>,
) -> SubmitOutcome {
    let (session, mode) = match target {
        Target::Tmux { session, mode } => (session, mode),
        Target::Buffer(mode) => ("", mode),
        #[cfg(unix)]
        Target::Wrapper {
            path,
            nonblocking_open,
        } => {
            return match payload {
                SubmitPayload::WrapperFrame(frame) => {
                    super::fifo::submit(path, frame, nonblocking_open)
                }
                _ => SubmitOutcome::output(Err("wrapper submit requires a frame".into())),
            };
        }
        _ => return SubmitOutcome::output(Err("not a tmux submit target".into())),
    };
    let target = match mode {
        TmuxInputMode::Busy { target, .. } => target.to_owned(),
        _ => format!("={session}:"),
    };
    match payload {
        SubmitPayload::BracketedText(_) => {
            SubmitOutcome::output(Err("bracketed text requires a Herdr target".into()))
        }
        SubmitPayload::Pinned { input, codex } => pinned(mode, session, &target, input, codex),
        SubmitPayload::LoadText { buffer, text } => {
            if matches!(mode, TmuxInputMode::Platform) {
                return SubmitOutcome::output(tmux::load_buffer(buffer, text));
            }
            let loaded = || {
                let mut file = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
                file.write_all(text.as_bytes())
                    .and_then(|()| file.flush())
                    .map_err(|e| e.to_string())?;
                command(
                    mode,
                    session,
                    &target,
                    &["load-buffer", "-b", buffer, &file.path().to_string_lossy()],
                )
                .into_output()
            };
            SubmitOutcome::output(loaded())
        }
        SubmitPayload::LoadFile { buffer, path } => {
            command(mode, session, &target, &["load-buffer", "-b", buffer, path])
        }
        SubmitPayload::Literal(text) => match mode {
            TmuxInputMode::Platform => {
                SubmitOutcome::output(writer_lease::content("tmux", session, || {
                    tmux::send_literal(session, text)
                }))
            }
            TmuxInputMode::Submission => SubmitOutcome::output((|| {
                tmux::session_identity::require_managed(session)?;
                writer_lease::content("tmux", session, || {
                    run_submission(&["send-keys", "-t", &target, "-l", "--", text])
                })
            })()),
            _ => SubmitOutcome::output(Err("literal payload unsupported for this runner".into())),
        },
        SubmitPayload::Paste { buffer, delete } => {
            if matches!(mode, TmuxInputMode::Platform) {
                return SubmitOutcome::output(writer_lease::content("tmux", session, || {
                    tmux::paste_buffer(session, buffer, delete)
                }));
            }
            let mut args = vec!["paste-buffer", "-p", "-r"];
            if delete {
                args.push("-d");
            }
            args.extend(["-b", buffer, "-t", &target]);
            if matches!(mode, TmuxInputMode::Submission) {
                return SubmitOutcome::output((|| {
                    tmux::session_identity::require_managed(session)?;
                    writer_lease::content("tmux", session, || run_submission(&args))
                })());
            }
            command(mode, session, &target, &args)
        }
        SubmitPayload::Keys(keys) => {
            let names: Vec<_> = keys.iter().copied().map(tmux_key_name).collect();
            keys_command(mode, session, &target, &names)
        }
        SubmitPayload::KeyNames(keys) => keys_command(mode, session, &target, keys),
        #[cfg(unix)]
        SubmitPayload::WrapperFrame(_) => {
            SubmitOutcome::output(Err("frame requires a wrapper target".into()))
        }
    }
}

fn keys_command(
    mode: TmuxInputMode<'_>,
    session: &str,
    target: &str,
    keys: &[&str],
) -> SubmitOutcome {
    if matches!(mode, TmuxInputMode::Platform) {
        return SubmitOutcome::output(tmux::send_keys(session, keys));
    }
    let mut args = vec!["send-keys", "-t", target];
    args.extend(keys);
    if matches!(mode, TmuxInputMode::Submission) {
        return SubmitOutcome::output(
            tmux::session_identity::require_managed(session).and_then(|()| run_submission(&args)),
        );
    }
    command(mode, session, target, &args)
}

fn command(mode: TmuxInputMode<'_>, session: &str, target: &str, args: &[&str]) -> SubmitOutcome {
    match mode {
        TmuxInputMode::Submission => SubmitOutcome::output(run_submission(args)),
        TmuxInputMode::Actor { program, budget } => {
            SubmitOutcome::bounded(run_actor(program, session, budget, args))
        }
        TmuxInputMode::Busy { program, .. } => {
            SubmitOutcome::bounded(run_busy(program, target, args))
        }
        TmuxInputMode::Platform => {
            SubmitOutcome::output(Err("unsupported platform command payload".into()))
        }
    }
}

fn pinned(
    mode: TmuxInputMode<'_>,
    session: &str,
    target: &str,
    input: PinnedInput<'_>,
    codex: bool,
) -> SubmitOutcome {
    let args = |target: &str| -> Vec<String> {
        match input {
            PinnedInput::Paste { buffer } => {
                ["paste-buffer", "-p", "-r", "-d", "-b", buffer, "-t", target]
                    .map(String::from)
                    .into()
            }
            PinnedInput::Enter => ["send-keys", "-t", target, "Enter"]
                .map(String::from)
                .into(),
        }
    };
    let pin = crate::services::claude_tui::owner_input::pinned();
    if codex && matches!(pin, Ok(None)) {
        return pinned(mode, session, target, input, false);
    }
    let result = match pin {
        Err(_) => KeyOutcome::Held,
        Ok(None) => {
            let args = args(target);
            match command(
                mode,
                session,
                target,
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .into_output()
            {
                Ok(out) if out.status.success() => KeyOutcome::Applied,
                _ => KeyOutcome::Failed,
            }
        }
        Ok(Some((pane, condition))) => {
            let (applied, held, condition) = if codex {
                (
                    "CODEX_OWNER_KEY_APPLIED",
                    "CODEX_OWNER_KEY_HELD",
                    format!("#{{&&:{condition},#{{==:#{{pane_in_mode}},0}}}}"),
                )
            } else {
                ("PINNED_KEY_APPLIED", "PINNED_KEY_HELD", condition)
            };
            let applied_command =
                format!("{} ; display-message -p {applied}", args(&pane).join(" "));
            let held_command = format!("display-message -p {held}");
            let predicate_target =
                if !codex && crate::config::test_instance::environment::claude_login_enabled() {
                    target
                } else {
                    &pane
                };
            let args = [
                "if-shell",
                "-F",
                "-t",
                predicate_target,
                &condition,
                &applied_command,
                &held_command,
            ];
            match command(mode, session, target, &args).into_output() {
                Ok(out) if out.status.success() => {
                    match String::from_utf8_lossy(&out.stdout).trim() {
                        answer if answer == applied => KeyOutcome::Applied,
                        answer if answer == held => KeyOutcome::Held,
                        _ => KeyOutcome::Failed,
                    }
                }
                _ => KeyOutcome::Failed,
            }
        }
    };
    SubmitOutcome::key(result)
}

pub(crate) fn run_submission(args: &[&str]) -> Result<Output, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut command = runtime_command("tmux").map_err(|e| e.to_string())?;
                crate::services::platform::tmux::input_command(&mut command, args)
                    .map_err(|e| e.to_string())?;
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())?
                    .block_on(run_bounded_tmux(&mut command))
                    .map_err(|e| e.to_string())
            })
            .join()
            .unwrap_or_else(|_| Err("tmux submission worker panicked".into()))
    })
}

fn actor_command(program: &Path, args: &[&str]) -> std::io::Result<Command> {
    let mut command = runtime_command(program)?;
    tmux::input_command(&mut command, args)?;
    Ok(command)
}

pub(crate) fn run_actor(
    program: &Path,
    session: &str,
    budget: Duration,
    args: &[&str],
) -> Result<std::process::Output, BoundedTmuxError> {
    if matches!(args.first(), Some(&"send-keys" | &"paste-buffer")) {
        crate::services::platform::tmux::session_identity::require_managed_with_program(
            session, program, budget,
        )
        .map_err(|error| {
            BoundedTmuxError::Spawn(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                error,
            ))
        })?;
    }
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut command = actor_command(program, args).map_err(BoundedTmuxError::Spawn)?;
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(BoundedTmuxError::Spawn)?
                    .block_on(run_with_budget(&mut command, budget))
            })
            .join()
            .unwrap_or_else(|_| {
                Err(BoundedTmuxError::Spawn(std::io::Error::other(
                    "tmux worker panicked",
                )))
            })
    })
}

pub(crate) fn run_busy(
    program: &Path,
    target: &str,
    args: &[&str],
) -> Result<Output, BoundedTmuxError> {
    if matches!(
        args.first(),
        Some(&"send-keys" | &"paste-buffer" | &"if-shell")
    ) {
        let session = target.trim_start_matches('=').trim_end_matches(':');
        crate::services::platform::tmux::session_identity::require_managed_with_program(
            session,
            program,
            Duration::from_secs(3),
        )
        .map_err(|error| {
            BoundedTmuxError::Spawn(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                error,
            ))
        })?;
    }
    let mut command = Command::new(program);
    crate::services::platform::tmux::input_command(&mut command, args)
        .map_err(BoundedTmuxError::Spawn)?;
    crate::services::platform::binary_resolver::apply_runtime_path(&mut command);
    // A fresh thread owns its runtime, so this blocks safely from any caller.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(BoundedTmuxError::Spawn)?
                    .block_on(run_bounded_tmux(&mut command))
            })
            .join()
            .unwrap_or_else(|_| {
                Err(BoundedTmuxError::Spawn(std::io::Error::other(
                    "tmux worker panicked",
                )))
            })
    })
}
