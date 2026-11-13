//! The existing tmux transaction and the cause its held watch reads.
use std::sync::Arc;

use super::{BusyOutcome, BusyProbe, BusyReport};
use crate::services::claude_tui::busy_inject::{
    self, Outcome, Request, Screen, Veto, WriteFailure,
};
use crate::services::claude_tui::{guarded_outcome, owner_input};
use crate::services::provider::CancelToken;

pub(super) struct Adapter {
    pub(super) pane: busy_inject::Pane,
    pub(super) timing: &'static busy_inject::Timing,
    pub(super) declared: bool,
    pub(super) cancel: Arc<CancelToken>,
}

impl Adapter {
    pub(super) fn submit(&self, request: &Request<'_>) -> BusyReport {
        let report = guarded_outcome::with_receipt_sink(Some(&self.cancel), || {
            let report = busy_inject::inject_owned(&self.pane, request, self.timing, self.declared);
            busy_inject::collect_receipt(request, &report, self.timing);
            report
        });
        if matches!(report.outcome, Outcome::NotSent(_))
            && self.cancel.native_stop.receipt_sink().stopped_before_key()
        {
            return BusyReport::outcome(BusyOutcome::Stopped);
        }
        let outcome = match report.outcome {
            Outcome::NotSent(veto) => {
                let probe = self.probe(veto, request);
                let mut stopped = BusyReport::not_sent(veto, probe);
                stopped.draft_unproven = report.draft_unproven;
                return stopped;
            }
            Outcome::Injected => BusyOutcome::Submitted,
            Outcome::Failed(WriteFailure::Stash) => BusyOutcome::Failed("stash_failed"),
            Outcome::Failed(WriteFailure::Paste) => BusyOutcome::Uncertain("paste_failed"),
            Outcome::Failed(WriteFailure::Enter) => BusyOutcome::Uncertain("enter_failed"),
        };
        BusyReport {
            outcome,
            draft_unproven: report.draft_unproven,
        }
    }
}

impl Adapter {
    fn probe(&self, veto: Veto, request: &Request<'_>) -> Option<BusyProbe> {
        let pane = self.pane.clone();
        match veto {
            Veto::Modal | Veto::ScreenUnread | Veto::CopyMode => {
                Some(Box::new(move || busy_inject::screen_submits(&pane)))
            }
            Veto::LoadFailed if owner_input::buffer_failed() => {
                Some(cause(pane.clone(), loads(pane)))
            }
            Veto::TranscriptUnavailable => {
                let transcript = request.transcript.to_path_buf();
                Some(cause(pane, move || transcript.exists()))
            }
            _ => None,
        }
    }
}

pub(crate) fn cause(
    pane: busy_inject::Pane,
    mut clear: impl FnMut() -> bool + Send + 'static,
) -> BusyProbe {
    Box::new(move || match clear() {
        true => Screen::Ready,
        false => pane.lost(),
    })
}

pub(crate) fn loads(pane: busy_inject::Pane) -> impl FnMut() -> bool + Send + 'static {
    move || {
        let Ok(file) = tempfile::NamedTempFile::new() else {
            return false;
        };
        let (path, buffer) = (file.path().to_string_lossy(), "agentdesk-owner-load-probe");
        let loaded = std::fs::write(file.path(), "probe").is_ok()
            && pane.ok(&["load-buffer", "-b", buffer, &path]).is_some();
        if loaded {
            let _ = pane.ok(&["delete-buffer", "-b", buffer]);
        }
        loaded
    }
}
