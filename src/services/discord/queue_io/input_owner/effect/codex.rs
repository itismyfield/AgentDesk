//! A busy Codex pane: main's Codex busy transaction under the owner scope, mapped as main's
//! `run_codex` maps it, with the owner's ready and start refusals as not-sent vetoes.
use std::path::PathBuf;

use super::{Completion, Leaf};
use crate::services::claude_tui::{busy_inject, owner_input};
use crate::services::codex_tui::busy_inject as codex_inject;
use crate::services::discord::health::CodexLedger;

pub(crate) struct BusyCodex {
    pub(crate) pane: busy_inject::Pane,
    pub(crate) session: String,
    pub(crate) rollout: PathBuf,
    /// The open model turn the owner read before reserving; the paste is refused once it moves.
    pub(crate) target_turn: String,
    pub(crate) channel: u64,
    pub(crate) message: Option<u64>,
    pub(crate) source: String,
    pub(crate) author: String,
    pub(crate) name: String,
    pub(crate) nonce: String,
    pub(crate) text: String,
    pub(crate) timing: &'static codex_inject::Timing,
}

impl BusyCodex {
    pub(crate) fn leaf(self) -> Leaf {
        Box::new(move |frame| {
            let (source, author, name) = (&self.source, &self.author, &self.name);
            if busy_inject::frame(source, author, name, &self.nonce, &self.text) != frame {
                owner_input::vetoed("frame_mismatch");
                return Ok(Completion::NotSent("frame_mismatch".into()));
            }
            let request = codex_inject::Request {
                session: &self.session,
                rollout: &self.rollout,
                source,
                author,
                name,
                nonce: &self.nonce,
                text: &self.text,
                target_turn: &self.target_turn,
            };
            let ledger = CodexLedger {
                session: &self.session,
                nonce: &self.nonce,
                target_turn: &self.target_turn,
                message: self.message.map(poise::serenity_prelude::MessageId::new),
            };
            let inject = || codex_inject::inject(&self.pane, &request, &ledger, self.timing);
            Ok(map(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(inject)),
                |detail| unconfirmed(self.channel, &self.session, &self.nonce, detail),
            ))
        })
    }
}

/// Main's `run_codex` mapping: a veto proves nothing reached the pane, a paste or Enter failure
/// and a panic may have, and a local paste and Enter is the submission.
pub(crate) fn map(
    outcome: std::thread::Result<codex_inject::Outcome>,
    alert: impl Fn(&'static str) -> String,
) -> Completion {
    use codex_inject::{Outcome, Unconfirmed};
    match outcome {
        Ok(Outcome::NotSent(veto)) => {
            let reason = format!("{veto:?}");
            owner_input::vetoed(&reason);
            Completion::NotSent(reason)
        }
        Ok(Outcome::Injected) => Completion::Submitted,
        Ok(Outcome::Unconfirmed(Unconfirmed::PasteFailed)) => {
            Completion::WriteUncertain(alert("paste_failed"))
        }
        Ok(Outcome::Unconfirmed(Unconfirmed::EnterFailed)) => {
            Completion::WriteUncertain(alert("enter_failed"))
        }
        Err(_) => Completion::WriteUncertain(alert("executor_failed")),
    }
}

/// The one alert of an effect that may have reached the pane, as main records it.
pub(crate) fn unconfirmed(channel: u64, session: &str, nonce: &str, detail: &str) -> String {
    let payload = serde_json::json!({
        "detail": detail,
        "nonce": nonce,
        "tmux_session": session,
        "owner": true,
    });
    tracing::warn!(channel_id = channel, %payload, "owner busy Codex input unconfirmed");
    crate::services::observability::events::record_simple(
        "busy_inject_unconfirmed",
        Some(channel),
        Some("codex"),
        payload,
    );
    detail.into()
}
