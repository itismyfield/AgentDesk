//! A busy Codex pane: main's Codex busy transaction under the owner scope, mapped as main's
//! `run_codex` maps it, with the owner's ready and start refusals as not-sent vetoes.
use std::path::PathBuf;

use super::{Completion, Leaf};
use crate::services::claude_tui::{busy_inject, owner_input};
use crate::services::codex_tui::busy_inject as codex_inject;
use crate::services::discord::health::CodexLedger;
use crate::services::session_host::HerdrTarget;

/// One input as main's Codex busy transaction takes it, whichever host the pane runs on.
pub(crate) struct CodexInput {
    /// The logical session the history and the legacy ledger name.
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
}

impl CodexInput {
    /// A reserved frame that is not main's busy frame of this input is refused before any key.
    fn frame_refused(&self, frame: &str) -> Option<Completion> {
        let (source, author, name) = (&self.source, &self.author, &self.name);
        if busy_inject::frame(source, author, name, &self.nonce, &self.text) == frame {
            return None;
        }
        owner_input::vetoed("frame_mismatch");
        Some(Completion::NotSent("frame_mismatch".into()))
    }

    /// `inject` on main's request and ledger, its outcome or panic mapped as `run_codex` maps it.
    fn run(
        &self,
        inject: impl FnOnce(&codex_inject::Request<'_>, &CodexLedger<'_>) -> codex_inject::Outcome,
    ) -> Completion {
        let request = codex_inject::Request {
            session: &self.session,
            rollout: &self.rollout,
            source: &self.source,
            author: &self.author,
            name: &self.name,
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
        let inject = || inject(&request, &ledger);
        map(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(inject)),
            |detail| unconfirmed(self.channel, &self.session, &self.nonce, detail),
        )
    }
}

pub(crate) struct BusyCodex {
    pub(crate) pane: busy_inject::Pane,
    pub(crate) timing: &'static codex_inject::Timing,
    pub(crate) input: CodexInput,
}

impl BusyCodex {
    pub(crate) fn leaf(self) -> Leaf {
        Box::new(move |frame| {
            if let Some(refused) = self.input.frame_refused(&frame) {
                return Ok(refused);
            }
            let (pane, timing) = (&self.pane, self.timing);
            Ok(self
                .input
                .run(|request, ledger| codex_inject::inject(pane, request, ledger, timing)))
        })
    }
}

/// A busy Codex pane on Herdr: the owner's adapter on the execution's gate, held to the physical
/// pane its execution runs on.
pub(crate) struct BusyCodexHerdr {
    pub(crate) target: HerdrTarget,
    /// The pane's physical writer `socket_path#pane_id`, as the Herdr gate names it.
    pub(crate) writer: String,
    pub(crate) input: CodexInput,
}

impl BusyCodexHerdr {
    pub(crate) fn leaf(self) -> Leaf {
        Box::new(move |frame| {
            if let Some(refused) = self.input.frame_refused(&frame) {
                return Ok(refused);
            }
            // The owner's first ready pins this pane, so it is bound before the adapter runs.
            if let Err(refused) = owner_input::bind_writer(&self.writer) {
                owner_input::vetoed(&refused);
                return Ok(Completion::NotSent(refused));
            }
            let busy = crate::services::codex::herdr_turn::busy::inject;
            Ok(self
                .input
                .run(|request, ledger| busy(&self.target, request, ledger)))
        })
    }
}

/// An idle Codex tmux pane: one pre-key read of the composer, then the owner's pinned paste, settle
/// and Enter; nothing is read after a key, and a paste of unknown effect is never resent.
pub(crate) fn idle_codex(pane: busy_inject::Pane, session: String) -> Leaf {
    use crate::services::claude_tui::busy_inject::stash::{Key, key};
    use crate::services::codex_tui::input::{
        admit_codex_prompt_under_lock, try_with_composer_mutation_lock,
    };
    let refused = |reason: String| {
        owner_input::vetoed(&reason);
        Completion::NotSent(reason)
    };
    Box::new(move |frame| {
        let send = || {
            if let Err(error) = admit_codex_prompt_under_lock(&session, &frame, false) {
                return refused(format!("composer_refused: {error}"));
            }
            let buffer = format!("agentdesk-owner-idle-{session}");
            let loaded = tempfile::NamedTempFile::new().ok().filter(|file| {
                let path = file.path().to_string_lossy().into_owned();
                std::fs::write(&path, &frame).is_ok()
                    && pane.ok(&["load-buffer", "-b", &buffer, &path]).is_some()
            });
            if loaded.is_none() {
                return refused("load_failed".into());
            }
            if owner_input::ready_payload().is_err() || owner_input::start_payload().is_err() {
                let _ = pane.ok(&["delete-buffer", "-b", &buffer]);
                return refused("owner_refused".into());
            }
            owner_input::keyed();
            let paste = |target: &str| {
                [
                    "paste-buffer",
                    "-p",
                    "-r",
                    "-d",
                    "-b",
                    &buffer,
                    "-t",
                    target,
                ]
                .map(String::from)
            };
            match key(&pane, paste) {
                Key::Applied => {}
                Key::Held => {
                    owner_input::first_key_held("pane_moved");
                    return Completion::NotSent("pane_moved".into());
                }
                Key::Failed => return Completion::WriteUncertain("paste_failed".into()),
            }
            std::thread::sleep(codex_inject::TIMING.settle);
            match key(&pane, |target: &str| {
                ["send-keys", "-t", target, "Enter"].map(String::from)
            }) {
                Key::Applied => Completion::Submitted,
                _ => Completion::WriteUncertain("enter_failed".into()),
            }
        };
        Ok(try_with_composer_mutation_lock(&session, send)
            .unwrap_or_else(|| refused("lock_contended".into())))
    })
}

/// Main's `run_codex` mapping: a veto proves nothing reached the pane, a paste or Enter failure
/// and a panic may have, and a local paste and Enter is the submission.
fn map(
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
fn unconfirmed(channel: u64, session: &str, nonce: &str, detail: &str) -> String {
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

#[cfg(test)]
#[path = "codex_herdr_tests.rs"]
mod herdr_tests;
