//! A busy Claude pane on Herdr: one read before any key, the owner's ready, a visible draft parked
//! with C-s, the payload start, then one paste and one Enter on the bound physical pane.
use std::path::PathBuf;

use super::{Completion, Leaf, MODAL};
use crate::services::claude_tui::composer_lock::DraftSighting;
use crate::services::claude_tui::host_input::{InputRun, run_herdr};
use crate::services::claude_tui::input::TuiInputAction;
use crate::services::claude_tui::{busy_inject, owner_input};
use crate::services::session_host::{HerdrMutation, HerdrTarget, HostKey, HostMutation};
use crate::services::tui_input::human_frame::history;

pub(crate) struct BusyClaudeHerdr {
    pub(crate) target: HerdrTarget,
    /// The pane's physical writer `socket_path#pane_id`, as the Herdr gate names it.
    pub(crate) writer: String,
    /// The logical session the owner's history names.
    pub(crate) session: String,
    pub(crate) transcript: PathBuf,
    pub(crate) channel: u64,
    pub(crate) source: String,
    pub(crate) author: String,
    pub(crate) name: String,
    pub(crate) nonce: String,
    pub(crate) text: String,
}

impl BusyClaudeHerdr {
    pub(crate) fn leaf(self) -> Leaf {
        Box::new(move |frame| {
            let (source, author, name) = (&self.source, &self.author, &self.name);
            if busy_inject::frame(source, author, name, &self.nonce, &self.text) != frame {
                owner_input::vetoed("frame_mismatch");
                return Ok(Completion::NotSent("frame_mismatch".into()));
            }
            if let Err(refused) = owner_input::bind_writer(&self.writer) {
                owner_input::vetoed(&refused);
                return Ok(Completion::NotSent(refused));
            }
            let send = || self.send(&frame);
            Ok(std::panic::catch_unwind(std::panic::AssertUnwindSafe(send))
                .unwrap_or_else(|_| Completion::WriteUncertain(self.alert("executor_failed"))))
        })
    }

    fn send(&self, frame: &str) -> Completion {
        let not_sent = |reason: &str| {
            owner_input::vetoed(reason);
            Completion::NotSent(reason.into())
        };
        let Some(capture) = self.target.capture_ansi(-80) else {
            return not_sent("PaneUnavailable");
        };
        if busy_inject::modal(&capture) {
            return not_sent(MODAL);
        }
        let Ok(offset) = std::fs::metadata(&self.transcript).map(|meta| meta.len()) else {
            return not_sent("TranscriptUnavailable");
        };
        let who = (
            "claude",
            self.session.as_str(),
            self.nonce.as_str(),
            self.author.as_str(),
        );
        if history::register(who, &self.transcript, offset, frame).is_err() {
            return not_sent("LoadFailed");
        }
        let withdrawn = |reason: &str| {
            history::withdraw(&self.nonce, &self.transcript);
            not_sent(reason)
        };
        if owner_input::ready_payload().is_err() {
            return withdrawn("owner_refused");
        }
        // A person's visible draft is parked whichever channel the input names; an unread one,
        // or one beside an existing stash, is not.
        let park = busy_inject::draft_sighting(&capture) == DraftSighting::PersonDraft;
        if park {
            owner_input::keyed();
            let parked = (self.target.pin(HerdrMutation::Input).ok())
                .and_then(|()| self.target.send_keys(&[HostKey::CtrlS]).ok());
            if parked != Some(HostMutation::Confirmed) {
                return Completion::Failed(self.alert("stash_failed"));
            }
        }
        if owner_input::start_payload().is_err() {
            return match park {
                true => Completion::Failed(self.alert("stash_failed")),
                false => withdrawn("owner_refused"),
            };
        }
        owner_input::keyed();
        let plan = [
            TuiInputAction::PasteBuffer(frame.into()),
            TuiInputAction::Enter,
        ];
        match run_herdr(&self.target, &plan, None) {
            InputRun::Applied => {
                self.receipt(frame, offset);
                Completion::Submitted
            }
            // The gate refused before its first write: with no C-s before it, nothing went.
            InputRun::Refused(_) if !park => {
                history::withdraw(&self.nonce, &self.transcript);
                owner_input::first_key_held("PaneUnavailable");
                Completion::NotSent("PaneUnavailable".into())
            }
            InputRun::Refused(_) => Completion::Failed(self.alert("stash_failed")),
            InputRun::Indeterminate { confirmed: 0, .. } | InputRun::Cancelled { confirmed: 0 } => {
                Completion::WriteUncertain(self.alert("paste_failed"))
            }
            _ => Completion::WriteUncertain(self.alert("enter_failed")),
        }
    }

    /// Records the receipt off the effect's thread; nothing waits for it.
    fn receipt(&self, frame: &str, offset: u64) {
        let (path, nonce, text) = (
            self.transcript.clone(),
            self.nonce.clone(),
            frame.to_string(),
        );
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

    /// The one alert of a failure after a key may have reached the pane.
    fn alert(&self, detail: &str) -> String {
        let payload = serde_json::json!({
            "detail": detail,
            "nonce": self.nonce,
            "source": self.source,
            "tmux_session": self.session,
            "owner": true,
            "host": "herdr",
        });
        tracing::warn!(channel_id = self.channel, %payload, "owner busy input unconfirmed");
        crate::services::observability::events::record_simple(
            "busy_inject_unconfirmed",
            Some(self.channel),
            Some("claude"),
            payload,
        );
        detail.into()
    }
}

#[cfg(test)]
#[path = "herdr_tests.rs"]
mod tests;
