//! Owned Codex delivery on either host, with physical refusals kept apart from uncertain writes.
use std::path::PathBuf;

use super::{Completion, Leaf, codex_waits, refused};
use crate::services::claude_tui::busy_inject::Screen;
use crate::services::claude_tui::{busy_inject, owner_input};
use crate::services::codex_tui::busy_inject as codex_inject;
use crate::services::discord::health::CodexLedger;
use crate::services::session_host::HerdrTarget;

/// One input as main's Codex busy transaction takes it, whichever host the pane runs on.
pub(crate) struct CodexInput {
    /// The logical session the history and the legacy ledger name.
    pub(crate) session: String,
    pub(crate) rollout: PathBuf,
    /// An observed model turn for diagnostics; empty when the readable rollout has none.
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

    /// `inject` on main's request and ledger, its outcome or panic mapped as `run_codex` maps it;
    /// `probe` reads the pane once a modal refused it.
    fn run(
        &self,
        inject: impl FnOnce(
            &codex_inject::Request<'_>,
            &dyn codex_inject::Ledger,
        ) -> codex_inject::Outcome,
        probe: impl FnMut() -> Screen + Send + 'static,
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
        let ledger = OwnedLedger {
            ledger,
            input: self,
        };
        let inject = || inject(&request, &ledger);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(inject));
        if let Ok(codex_inject::Outcome::NotSent(codex_inject::Veto::Modal)) = outcome {
            super::super::held_watch::arm(self.channel, Box::new(probe));
        }
        map(outcome, |detail| {
            unconfirmed(self.channel, &self.session, &self.nonce, detail)
        })
    }
}

struct OwnedLedger<'a> {
    ledger: CodexLedger<'a>,
    input: &'a CodexInput,
}
impl codex_inject::Ledger for OwnedLedger<'_> {
    fn register(&self) -> bool {
        self.ledger.register()
    }
    fn withdraw(&self) {
        self.ledger.withdraw();
    }
    fn message_id(&self) -> Option<u64> {
        self.ledger.message_id()
    }
    fn admitted(&self) {
        self.ledger.admitted();
    }
    fn draft_delivered(&self) {
        crate::services::codex_tui::input::notify_owned_draft(self.input.channel);
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
            let probe = composer(self.pane.clone());
            Ok(self.input.run(
                |request, ledger| codex_inject::inject(pane, request, ledger, timing),
                probe,
            ))
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
            let target = self.target.clone();
            let probe = move || match target.capture_ansi(-80) {
                Some(capture) if codex_inject::screen::delivery_veto(&capture).is_none() => {
                    Screen::Ready
                }
                Some(_) => Screen::Blocked,
                None => Screen::Unknown,
            };
            Ok(self
                .input
                .run(|request, ledger| busy(&self.target, request, ledger), probe))
        })
    }
}

/// The owner's own read of a Codex composer: ready once a capture admits a paste.
fn composer(pane: busy_inject::Pane) -> impl FnMut() -> Screen + Send + 'static {
    move || match codex_inject::owned_composer_capture(&pane) {
        Ok(_) => Screen::Ready,
        Err(codex_inject::Veto::PaneUnavailable) => pane.lost(),
        Err(_) => Screen::Blocked,
    }
}

/// An idle Codex tmux pane: one pre-key read of the composer, then the owner's pinned paste, settle
/// and Enter; nothing is read after a key, and a paste of unknown effect is never resent.
pub(crate) fn idle_codex(pane: busy_inject::Pane, session: String, channel: u64) -> Leaf {
    use crate::services::claude_tui::busy_inject::stash::Key;
    use crate::services::codex_tui::input::{
        admit_owned_codex_prompt, try_with_composer_mutation_lock,
    };
    use codex_inject::Veto;
    let vetoed = |veto: Veto| refused(format!("{veto:?}"), codex_waits(veto));
    Box::new(move |frame| {
        let send = || {
            let before = match admit_owned_codex_prompt(&session, &frame) {
                Ok((_, before)) => Some(before),
                Err(Ok(Veto::Modal)) => {
                    super::super::held_watch::arm(channel, Box::new(composer(pane.clone())));
                    return vetoed(Veto::Modal);
                }
                Err(Ok(veto)) => return vetoed(veto),
                Err(Err(prompt)) => return refused(prompt, false),
            };
            let buffer = format!("agentdesk-owner-idle-{session}");
            let loaded = tempfile::NamedTempFile::new().ok().filter(|file| {
                let path = file.path().to_string_lossy().into_owned();
                std::fs::write(&path, &frame).is_ok()
                    && pane.ok(&["load-buffer", "-b", &buffer, &path]).is_some()
            });
            if loaded.is_none() {
                super::arm_cause(channel, super::loads(pane.clone()));
                return vetoed(Veto::LoadFailed);
            }
            if owner_input::ready_payload().is_err() || owner_input::start_payload().is_err() {
                let _ = pane.ok(&["delete-buffer", "-b", &buffer]);
                return refused("owner_refused".into(), false);
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
            match codex_inject::key(&pane, paste) {
                Key::Applied => {}
                Key::Held => {
                    owner_input::first_key_held("pane_moved");
                    return Completion::NotSent("pane_moved".into());
                }
                Key::Failed => return Completion::WriteUncertain("paste_failed".into()),
            }
            std::thread::sleep(codex_inject::TIMING.settle);
            match codex_inject::key(&pane, |target: &str| {
                ["send-keys", "-t", target, "Enter"].map(String::from)
            }) {
                Key::Applied => {
                    if before
                        .as_deref()
                        .is_some_and(codex_inject::screen::draft_may_be_present)
                        && let Some((_, context)) =
                            crate::services::claude_tui::submission_fence::idle_stash_scope()
                        && context.target == session
                    {
                        crate::services::codex_tui::input::notify_owned_draft(context.channel);
                    }
                    Completion::Submitted
                }
                _ => Completion::WriteUncertain("enter_failed".into()),
            }
        };
        Ok(
            try_with_composer_mutation_lock(&session, send).unwrap_or_else(|| {
                let held = session.clone();
                super::arm_cause(channel, move || {
                    try_with_composer_mutation_lock(&held, || ()).is_some()
                });
                vetoed(Veto::LockContended)
            }),
        )
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
        Ok(Outcome::NotSent(veto)) => refused(format!("{veto:?}"), codex_waits(veto)),
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
