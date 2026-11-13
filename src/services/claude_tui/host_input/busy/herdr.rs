//! The existing bound Herdr transaction, with no fallback to another host.
use super::{BusyOutcome, BusyReport};
use crate::services::claude_tui::busy_inject::{self, Request, Screen, Veto};
use crate::services::claude_tui::composer_lock::DraftSighting;
use crate::services::claude_tui::host_input::{InputRun, run_herdr};
use crate::services::claude_tui::input::TuiInputAction;
use crate::services::claude_tui::owner_input;
use crate::services::session_host::{HerdrMutation, HerdrTarget, HostKey, HostMutation};
use crate::services::tui_input::human_frame::history;

pub(super) struct Adapter {
    pub(super) target: HerdrTarget,
    pub(super) writer: Option<String>,
}

impl Adapter {
    pub(super) fn submit(&self, request: &Request<'_>) -> BusyReport {
        let Some(writer) = self.writer.as_deref() else {
            return BusyReport::refused("owner Herdr input without a writer".into());
        };
        if let Err(reason) = owner_input::bind_writer(writer) {
            return BusyReport::refused(reason);
        }
        let Some(capture) = self.target.capture_ansi(-80) else {
            return BusyReport::not_sent(Veto::PaneUnavailable, None);
        };
        if busy_inject::modal(&capture) {
            let target = self.target.clone();
            let probe = move || match target.capture_ansi(-80) {
                Some(capture) if !busy_inject::modal(&capture) => Screen::Ready,
                Some(_) => Screen::Blocked,
                None => Screen::Unknown,
            };
            return BusyReport::not_sent(Veto::Modal, Some(Box::new(probe)));
        }
        let Ok(offset) = std::fs::metadata(request.transcript).map(|meta| meta.len()) else {
            return BusyReport::not_sent(Veto::TranscriptUnavailable, None);
        };
        let frame = busy_inject::frame(
            request.source,
            request.author,
            request.name,
            request.nonce,
            request.text,
        );
        let who = ("claude", request.session, request.nonce, request.author);
        if history::register(who, request.transcript, offset, &frame).is_err() {
            return BusyReport::not_sent(Veto::LoadFailed, None);
        }
        let withdrawn = || {
            history::withdraw(request.nonce, request.transcript);
            BusyReport::refused("owner_refused".into())
        };
        if owner_input::ready_payload().is_err() {
            return withdrawn();
        }
        let park = busy_inject::draft_sighting(&capture) == DraftSighting::PersonDraft;
        if park {
            owner_input::keyed();
            let parked = (self.target.pin(HerdrMutation::Input).ok())
                .and_then(|()| self.target.send_keys(&[HostKey::CtrlS]).ok());
            if parked != Some(HostMutation::Confirmed) {
                return BusyReport::outcome(BusyOutcome::Failed("stash_failed"));
            }
        }
        if owner_input::start_payload().is_err() {
            return match park {
                true => BusyReport::outcome(BusyOutcome::Failed("stash_failed")),
                false => withdrawn(),
            };
        }
        owner_input::keyed();
        let plan = [
            TuiInputAction::PasteBuffer(frame.clone()),
            TuiInputAction::Enter,
        ];
        BusyReport::outcome(match run_herdr(&self.target, &plan, None) {
            InputRun::Applied => {
                receipt(request, frame, offset);
                BusyOutcome::Submitted
            }
            InputRun::Refused(_) if !park => {
                history::withdraw(request.nonce, request.transcript);
                owner_input::first_key_held("PaneUnavailable");
                return BusyReport::not_sent(Veto::PaneUnavailable, None);
            }
            InputRun::Refused(_) => BusyOutcome::Failed("stash_failed"),
            InputRun::Indeterminate { confirmed: 0, .. } | InputRun::Cancelled { confirmed: 0 } => {
                BusyOutcome::Uncertain("paste_failed")
            }
            _ => BusyOutcome::Uncertain("enter_failed"),
        })
    }
}

fn receipt(request: &Request<'_>, text: String, offset: u64) {
    let (path, nonce) = (request.transcript.to_path_buf(), request.nonce.to_string());
    let scope = history::ReceiptScope::capture();
    let timing = &busy_inject::TIMING;
    let _ = std::thread::Builder::new()
        .name("claude-receipt".into())
        .spawn(move || {
            let deadline = std::time::Instant::now() + timing.confirm_window;
            let receipt = (path.as_path(), offset, nonce.as_str(), text.as_str());
            let wait = || busy_inject::await_receipt(receipt, deadline, timing.confirm_poll);
            let result = scope.enter(wait);
            tracing::debug!(%nonce, ?result, "Claude Herdr injection receipt diagnostic");
        });
}
