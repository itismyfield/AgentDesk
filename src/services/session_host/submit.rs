//! Typed input effects retain the original transport evidence for existing callers.
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use super::{HerdrTarget, HostError, HostKey, HostKind, HostMutation, HostRefusal};
use crate::services::tui_input::bounded_tmux::BoundedTmuxError;

#[derive(Clone, Copy, Debug)]
pub enum InputSource {
    Inherited,
    Control,
    #[cfg(unix)]
    LegacyWrapper,
}

#[derive(Clone, Copy)]
pub(super) enum TmuxInputMode<'a> {
    Platform,
    Submission,
    Actor { program: &'a Path, budget: Duration },
    Busy { program: &'a Path, target: &'a str },
}

pub(super) enum Target<'a> {
    Tmux {
        session: &'a str,
        mode: TmuxInputMode<'a>,
    },
    Buffer(TmuxInputMode<'a>),
    Herdr(&'a HerdrTarget),
    #[cfg(unix)]
    Wrapper {
        path: &'a str,
        nonblocking_open: bool,
    },
}

/// Only adapters construct tmux authorities; they consume the original identity and owner
/// checks at the original write boundary. A Herdr authority carries its pinned execution gate.
pub struct AuthorizedHostTarget<'a> {
    pub(super) target: Target<'a>,
    pub(super) source: InputSource,
}

impl<'a> AuthorizedHostTarget<'a> {
    pub(crate) fn herdr(target: &'a HerdrTarget) -> Self {
        Self {
            target: Target::Herdr(target),
            source: InputSource::Inherited,
        }
    }
}

#[derive(Clone, Copy)]
pub enum PinnedInput<'a> {
    Paste { buffer: &'a str },
    Enter,
}

pub enum SubmitPayload<'a> {
    Literal(&'a str),
    BracketedText(&'a str),
    LoadText {
        buffer: &'a str,
        text: &'a str,
    },
    LoadFile {
        buffer: &'a str,
        path: &'a str,
    },
    Paste {
        buffer: &'a str,
        delete: bool,
    },
    Keys(&'a [HostKey]),
    KeyNames(&'a [&'a str]),
    Pinned {
        input: PinnedInput<'a>,
        codex: bool,
    },
    #[cfg(unix)]
    WrapperFrame(WrapperFrame<'a>),
}

#[cfg(unix)]
pub enum WrapperFrame<'a> {
    Encoded(&'a str),
    Line(&'a dyn std::fmt::Display),
}

pub enum KeyOutcome {
    Applied,
    Held,
    Failed,
}

enum Evidence {
    Output(Result<Output, String>),
    Bounded(Result<Output, BoundedTmuxError>),
    Herdr(Result<HostMutation, HostError>),
    Key(KeyOutcome),
    #[cfg(unix)]
    Wrapper(Result<(), String>),
}

pub struct SubmitOutcome {
    pub(crate) mutation: HostMutation,
    evidence: Evidence,
}

#[cfg(test)]
thread_local! { static SUBMITS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
pub(crate) struct SubmitSpy(Option<usize>);

#[cfg(test)]
impl SubmitSpy {
    pub(crate) fn new() -> Self {
        Self(SUBMITS.replace(Some(0)))
    }
    pub(crate) fn calls(&self) -> usize {
        SUBMITS.get().unwrap()
    }
}

#[cfg(test)]
impl Drop for SubmitSpy {
    fn drop(&mut self) {
        SUBMITS.set(self.0);
    }
}

fn uncertain(detail: impl Into<String>) -> HostMutation {
    HostMutation::Indeterminate(detail.into())
}

impl SubmitOutcome {
    pub(super) fn refused(message: &str) -> Self {
        Self {
            mutation: HostMutation::Refused(HostRefusal::Precondition(message.into())),
            evidence: Evidence::Output(Err(message.into())),
        }
    }

    pub(super) fn output(result: Result<Output, String>) -> Self {
        let mutation = match &result {
            Ok(output) if output.status.success() => HostMutation::Confirmed,
            Ok(output) => uncertain(String::from_utf8_lossy(&output.stderr)),
            Err(error) => uncertain(error.clone()),
        };
        Self {
            mutation,
            evidence: Evidence::Output(result),
        }
    }

    pub(super) fn bounded(result: Result<Output, BoundedTmuxError>) -> Self {
        let mutation = match &result {
            Ok(output) if output.status.success() => HostMutation::Confirmed,
            Ok(output) => uncertain(String::from_utf8_lossy(&output.stderr)),
            Err(error) if !error.may_have_effect() => {
                HostMutation::Refused(HostRefusal::Precondition(error.to_string()))
            }
            Err(error) => uncertain(error.to_string()),
        };
        Self {
            mutation,
            evidence: Evidence::Bounded(result),
        }
    }

    fn herdr(result: Result<HostMutation, HostError>) -> Self {
        let mutation = match &result {
            Ok(mutation) => mutation.clone(),
            Err(error) => uncertain(format!("{error:?}")),
        };
        Self {
            mutation,
            evidence: Evidence::Herdr(result),
        }
    }

    pub(super) fn key(key: KeyOutcome) -> Self {
        let mutation = match key {
            KeyOutcome::Applied => HostMutation::Confirmed,
            KeyOutcome::Held => {
                HostMutation::Refused(HostRefusal::Precondition("pinned_key_held".into()))
            }
            KeyOutcome::Failed => uncertain("pinned_key_failed"),
        };
        Self {
            mutation,
            evidence: Evidence::Key(key),
        }
    }

    #[cfg(unix)]
    pub(super) fn wrapper(mutation: HostMutation, result: Result<(), String>) -> Self {
        Self {
            mutation,
            evidence: Evidence::Wrapper(result),
        }
    }

    pub fn into_output(self) -> Result<Output, String> {
        match self.evidence {
            Evidence::Output(output) => output,
            Evidence::Bounded(output) => output.map_err(|error| error.to_string()),
            _ => Err("host submit evidence does not contain command output".into()),
        }
    }

    pub(crate) fn into_bounded(self) -> Result<Output, BoundedTmuxError> {
        match self.evidence {
            Evidence::Bounded(output) => output,
            _ => Err(BoundedTmuxError::Spawn(std::io::Error::other(
                "host submit evidence is not bounded",
            ))),
        }
    }

    pub(crate) fn into_herdr(self) -> Result<HostMutation, HostError> {
        match self.evidence {
            Evidence::Herdr(result) => result,
            _ => Err(HostError::Protocol(
                "host submit evidence is not Herdr".into(),
            )),
        }
    }

    pub(crate) fn into_key(self) -> KeyOutcome {
        match self.evidence {
            Evidence::Key(key) => key,
            _ => KeyOutcome::Failed,
        }
    }

    #[cfg(unix)]
    pub fn into_wrapper(self) -> Result<(), String> {
        match self.evidence {
            Evidence::Wrapper(result) => result,
            _ => Err("host submit evidence is not a wrapper frame".into()),
        }
    }
}

pub(super) fn submit(
    kind: HostKind,
    target: AuthorizedHostTarget<'_>,
    payload: SubmitPayload<'_>,
) -> SubmitOutcome {
    #[cfg(test)]
    SUBMITS.set(SUBMITS.get().map(|calls| calls + 1));
    let source = target.source;
    let owner = crate::services::claude_tui::owner_input::original_identity();
    let result = match target.target {
        Target::Herdr(target) if kind == HostKind::Herdr => SubmitOutcome::herdr(match payload {
            SubmitPayload::Literal(text) => target.send_text(text),
            SubmitPayload::BracketedText(text) => target.send_paste(text),
            SubmitPayload::Keys(keys) => target.send_keys(keys),
            _ => Ok(HostMutation::Refused(HostRefusal::Precondition(
                "unsupported Herdr submit payload".into(),
            ))),
        }),
        target if kind == HostKind::Tmux => super::tmux_host::input::submit(target, payload),
        _ => SubmitOutcome::refused("host submit target kind mismatch"),
    };
    tracing::debug!(?source, ?owner, mutation = ?result.mutation, "host input effect");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_timeout_keeps_uncertainty_and_original_cleanup_evidence() {
        let outcome = SubmitOutcome::bounded(Err(BoundedTmuxError::Timeout {
            killed: true,
            reaped: false,
        }));
        assert!(matches!(outcome.mutation, HostMutation::Indeterminate(_)));
        assert!(matches!(
            outcome.into_bounded(),
            Err(BoundedTmuxError::Timeout {
                killed: true,
                reaped: false
            })
        ));
    }

    #[test]
    fn confirmed_output_keeps_stdout_and_stderr_for_the_existing_caller() {
        let output = Output {
            status: Default::default(),
            stdout: b"original reply".to_vec(),
            stderr: b"original diagnostic".to_vec(),
        };
        let outcome = SubmitOutcome::output(Ok(output));
        assert_eq!(outcome.mutation, HostMutation::Confirmed);
        let retained = outcome.into_output().unwrap();
        assert_eq!(retained.stdout, b"original reply");
        assert_eq!(retained.stderr, b"original diagnostic");
        assert!(retained.status.success());
    }
}
