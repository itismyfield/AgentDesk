//! Busy input's host boundary; the owner keeps framing, notices and settlement.
mod herdr;
mod tmux;
pub(crate) use herdr::observe as observe_herdr;
pub(crate) use tmux::{cause, loads};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{InputRefusal, InputTarget};
use crate::services::claude_tui::busy_inject::{self, Request, Screen, Veto};
use crate::services::provider::CancelToken;
use crate::services::session_host::HerdrTarget;

pub(crate) type BusyProbe = Box<dyn FnMut() -> Screen + Send>;

pub(crate) enum BusyOutcome {
    NotSent {
        reason: String,
        waits: bool,
        probe: Option<BusyProbe>,
    },
    Submitted,
    Failed(&'static str),
    Uncertain(&'static str),
    Stopped,
}

pub(crate) struct BusyReport {
    pub(crate) outcome: BusyOutcome,
    pub(crate) draft_unproven: bool,
}

impl BusyReport {
    fn outcome(outcome: BusyOutcome) -> Self {
        Self {
            outcome,
            draft_unproven: false,
        }
    }

    fn not_sent(veto: Veto, probe: Option<BusyProbe>) -> Self {
        Self::outcome(BusyOutcome::NotSent {
            reason: format!("{veto:?}"),
            waits: veto != Veto::InvalidInput,
            probe,
        })
    }

    fn refused(reason: String) -> Self {
        Self::outcome(BusyOutcome::NotSent {
            reason,
            waits: false,
            probe: None,
        })
    }
}

type BusySubmit = Box<dyn Fn(&Request<'_>) -> BusyReport + Send>;

/// One already-resolved host's busy transaction, ending at Enter without a resend.
pub(crate) struct BusyOperation {
    submit: BusySubmit,
    host_tag: Option<&'static str>,
}

impl BusyOperation {
    pub(crate) fn for_session(
        session: &str,
        herdr: Option<(HerdrTarget, String)>,
        program: PathBuf,
        timing: &'static busy_inject::Timing,
        declared: bool,
        cancel: Arc<CancelToken>,
    ) -> Self {
        let (target, writer) = herdr.map_or_else(
            || (InputTarget::Tmux(session.into()), None),
            |(target, writer)| (InputTarget::Herdr(target), Some(writer)),
        );
        Self::new(target, program, timing, declared, cancel, writer)
    }

    pub(crate) fn new(
        target: InputTarget,
        program: PathBuf,
        timing: &'static busy_inject::Timing,
        declared: bool,
        cancel: Arc<CancelToken>,
        writer: Option<String>,
    ) -> Self {
        let (submit, host_tag): (BusySubmit, Option<&'static str>) = match target {
            InputTarget::Tmux(session) => {
                let pane = match busy_inject::Pane::with_program(&session, program) {
                    Ok(pane) => pane,
                    Err(reason) => {
                        return Self {
                            submit: Box::new(move |_| BusyReport::refused(reason.clone())),
                            host_tag: None,
                        };
                    }
                };
                let adapter = tmux::Adapter {
                    pane,
                    timing,
                    declared,
                    cancel,
                };
                (Box::new(move |request| adapter.submit(request)), None)
            }
            InputTarget::Herdr(target) => {
                let adapter = herdr::Adapter {
                    target,
                    writer,
                    observation: None,
                };
                (
                    Box::new(move |request| adapter.submit(request)),
                    Some("herdr"),
                )
            }
            InputTarget::Refused(refusal) => {
                let adapter = Refused(refusal);
                (Box::new(move |request| adapter.submit(request)), None)
            }
        };
        Self { submit, host_tag }
    }

    pub(crate) fn submit(&self, request: &Request<'_>) -> BusyReport {
        #[cfg(test)]
        observe(request.nonce);
        (self.submit)(request)
    }

    pub(crate) fn host_tag(&self) -> Option<&'static str> {
        self.host_tag
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BusyState {
    Legacy,
    Idle,
    Held,
    Busy,
}

/// The immutable target read for this front and the source its later key must still name.
#[derive(Clone)]
pub(crate) struct BusyObservation {
    context: Option<Arc<herdr::Context>>,
    state: BusyState,
    source: Option<herdr::Source>,
}

impl BusyObservation {
    pub(crate) fn only(state: BusyState) -> Self {
        Self {
            context: None,
            state,
            source: None,
        }
    }

    pub(crate) fn state(&self) -> BusyState {
        self.state
    }

    pub(crate) fn transcript(&self) -> Option<&Path> {
        self.source.as_ref().map(|source| source.id.path.as_path())
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.source
            .as_ref()
            .map(|source| source.id.session_id.as_str())
    }

    pub(crate) fn target(&self) -> Option<serde_json::Value> {
        self.context
            .as_ref()
            .map(|context| context.attempt_target.clone())
    }

    pub(crate) fn source(&self) -> Option<serde_json::Value> {
        self.source.as_ref().map(|source| {
            serde_json::json!({
                "kind":"existing", "session_id":source.id.session_id,
                "transcript":source.id.path, "file":[source.id.dev, source.id.ino],
                "offset":source.offset,
            })
        })
    }

    pub(crate) fn herdr(&self) -> Option<(HerdrTarget, String)> {
        self.context
            .as_ref()
            .map(|context| (context.target.clone(), context.writer.clone()))
    }

    pub(crate) fn probe(&self) -> BusyProbe {
        let context = self.context.clone();
        Box::new(
            move || match context.as_ref().map(|context| context.read()) {
                Some(read) if read.state == BusyState::Busy || read.idle_verified => Screen::Ready,
                _ => Screen::Unknown,
            },
        )
    }

    pub(crate) fn prepare_busy(
        &self,
        program: PathBuf,
        timing: &'static busy_inject::Timing,
        declared: bool,
        cancel: Arc<CancelToken>,
    ) -> BusyOperation {
        let Some(context) = self.context.as_ref() else {
            return BusyOperation::new(
                InputTarget::Refused(InputRefusal::Unknown),
                program,
                timing,
                declared,
                cancel,
                None,
            );
        };
        let adapter = herdr::Adapter {
            target: context.target.clone(),
            writer: Some(context.writer.clone()),
            observation: Some(self.clone()),
        };
        BusyOperation {
            submit: Box::new(move |request| adapter.submit(request)),
            host_tag: Some("herdr"),
        }
    }

    fn admits(&self, request: &Request<'_>) -> bool {
        let Some(expected) = self
            .source
            .as_ref()
            .filter(|source| source.id.path == request.transcript)
        else {
            return false;
        };
        let Some(context) = self.context.as_ref() else {
            return false;
        };
        if request.session != context.logical {
            return false;
        }
        let read = context.read();
        matches!(read.state, BusyState::Busy | BusyState::Idle)
            && read.source.is_some_and(|source| source.id == expected.id)
    }

    fn held(&self) -> BusyReport {
        BusyReport::outcome(BusyOutcome::NotSent {
            reason: "busy_source_unavailable".into(),
            waits: true,
            probe: Some(self.probe()),
        })
    }
}

struct Refused(InputRefusal);

impl Refused {
    fn submit(&self, _request: &Request<'_>) -> BusyReport {
        BusyReport::refused(format!("input_target_refused: {:?}", self.0))
    }
}

#[cfg(test)]
static OBSERVED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
    std::sync::LazyLock::new(Default::default);

#[cfg(test)]
fn observe(nonce: &str) {
    let mut observed = OBSERVED.lock().unwrap_or_else(|error| error.into_inner());
    *observed.entry(nonce.into()).or_default() += 1;
}

#[cfg(test)]
pub(crate) fn observed(nonce: &str) -> usize {
    OBSERVED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(nonce)
        .copied()
        .unwrap_or_default()
}
