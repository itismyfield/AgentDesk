//! Busy input's host boundary; the owner keeps framing, notices and settlement.
mod herdr;
mod tmux;
pub(crate) use tmux::{cause, loads};

use std::path::PathBuf;
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
                let adapter = tmux::Adapter {
                    pane: busy_inject::Pane::with_program(&session, program),
                    timing,
                    declared,
                    cancel,
                };
                (Box::new(move |request| adapter.submit(request)), None)
            }
            InputTarget::Herdr(target) => {
                let adapter = herdr::Adapter { target, writer };
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
