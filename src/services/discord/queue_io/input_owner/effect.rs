//! The owner's effect for one reserved attempt: its provider leaf runs inside the owner scope and
//! the attempt settles from what the leaf proved, never from a resend.
#![cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;

use crate::services::claude_tui::busy_inject::{self, Outcome, WriteFailure};
use crate::services::claude_tui::owner_input;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

/// One attempt's input as the actor prepared it; the frame may carry several sources.
pub(crate) struct PreparedPayload {
    pub(crate) frame: String,
    pub(crate) sources: Vec<String>,
    pub(crate) attempt: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Completion {
    /// Content and Enter went, whether or not the transcript receipt was observed.
    Submitted,
    /// No key reached the pane for a reason a later look may clear; the original waits at the
    /// front. Only the leaf answers this: `run` reports it as a NotSent once the owner holds it.
    Held(String),
    /// No key reached the pane and the owner holds the proof; the original stays in custody.
    NotSent(String),
    /// C-s went but no content: a person's draft may be parked.
    Failed(String),
    /// Content may have reached the pane; it is never sent again.
    WriteUncertain(String),
}

/// The refusal of a pane showing a modal: the original is held at the front, not retained.
pub(crate) const MODAL: &str = "Modal";

/// A refusal before any key, held at the front when a later look may clear it and retained when
/// none can.
pub(crate) fn refused(reason: String, waits: bool) -> Completion {
    owner_input::vetoed(&reason);
    match waits {
        true => Completion::Held(reason),
        false => Completion::NotSent(reason),
    }
}

/// Whether a later look may clear a Claude veto; an input the entry refuses never submits.
pub(crate) fn claude_waits(veto: busy_inject::Veto) -> bool {
    use busy_inject::Veto::*;
    match veto {
        InvalidInput => false,
        LockContended
        | PaneUnavailable
        | Modal
        | NotBusy
        | TranscriptUnavailable
        | LoadFailed
        | ScreenUnread
        | CopyMode => true,
    }
}

/// Whether a later look may clear a Codex veto; an input the entry refuses never submits.
pub(crate) fn codex_waits(veto: crate::services::codex_tui::busy_inject::Veto) -> bool {
    use crate::services::codex_tui::busy_inject::Veto::*;
    match veto {
        InvalidInput => false,
        LockContended
        | PaneUnavailable
        | Modal
        | ComposerUnread
        | DraftNavigationUnproven
        | DraftSelectionUnproven
        | DraftCommandUnproven
        | DraftPasteTransformUnproven
        | DraftPlaceholderUnproven
        | DraftNotFullyVisible
        | DraftImagePositionUnproven
        | NotSteerable
        | NotBusy
        | TurnUnknown
        | TurnChanged
        | TranscriptUnavailable
        | LoadFailed
        | LedgerFull
        | AmbiguousIdenticalPending => true,
    }
}

/// A refusal before any key, marked so a caller never takes it for a write that may have landed.
pub(crate) fn unsent(error: String) -> String {
    match error.starts_with("NotSent(") {
        true => error,
        false => format!("NotSent({error:?})"),
    }
}

/// Whether a failed attempt surely put no content in the pane; anything else may have.
pub(crate) fn surely_unsent(error: &str) -> bool {
    error.starts_with("NotSent(") || error.starts_with("Failed(")
}

pub(crate) type Leaf = Box<dyn FnOnce(String) -> Result<Completion, String> + Send>;
pub(crate) type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Runs `leaf` on the reserved head and settles it: Submitted delivers, a proven NotSent returns
/// to custody or, under a modal, to the front, anything else leaves the head Unknown.
pub(crate) async fn run(
    mailbox: &ChannelMailboxHandle,
    clock: Clock,
    leaf: Leaf,
) -> Result<Completion, String> {
    // Only a panicked effect may have written; every other refusal here comes before the leaf.
    let (identity, outcome) = (mailbox.v2_cold_observed(clock(), clock.clone(), leaf).await)
        .map_err(|e| {
            if e.starts_with("cold effect ") {
                e
            } else {
                unsent(e)
            }
        })?;
    let completion = outcome.result.unwrap_or_else(Completion::WriteUncertain);
    if let Some(proof) = outcome.not_sent {
        let held = matches!(&completion, Completion::Held(_));
        mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("V2 not adopted")?;
                match held {
                    true => owner.hold_not_sent(&parse(identity)?, proof),
                    false => owner.retain_not_sent(&parse(identity)?, proof),
                }
            })
            .await
            .map_err(unsent)?;
        return Ok(match completion {
            Completion::Held(reason) | Completion::NotSent(reason) => Completion::NotSent(reason),
            other => Completion::NotSent(format!("{other:?}")),
        });
    }
    let delivered = matches!(completion, Completion::Submitted);
    let now = clock();
    mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            owner.settle(&parse(identity)?, delivered, now)
        })
        .await?;
    Ok(match completion {
        Completion::Held(reason) | Completion::NotSent(reason) => {
            Completion::WriteUncertain(format!("unproven: {reason}"))
        }
        other => other,
    })
}

/// The owner's own types, read back from the identity it handed out.
pub(super) fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// A busy Claude tmux pane: main's busy transaction, any visible draft parked, ending at Enter.
pub(crate) struct BusyClaude {
    pub(crate) pane: busy_inject::Pane,
    pub(crate) session: String,
    pub(crate) transcript: PathBuf,
    pub(crate) channel: u64,
    pub(crate) source: String,
    pub(crate) author: String,
    pub(crate) name: String,
    /// The attempt's nonce, which names the input in the transcript.
    pub(crate) nonce: String,
    /// The original body; the prepared frame must be exactly main's framing of it.
    pub(crate) text: String,
    pub(crate) timing: &'static busy_inject::Timing,
    /// A reader set up before the paste answers it, so the relay leaves its turn alone.
    pub(crate) declared: bool,
    /// Tells the original once that a draft beside the pasted input may not be kept.
    pub(crate) draft_notice: Option<DraftNotice>,
    /// The turn's token, whose first-key gate the paste consumes.
    pub(crate) cancel: Arc<crate::services::provider::CancelToken>,
}

/// Raises the one notice an original owes when its paste met a draft C-s could not park.
pub(crate) type DraftNotice = Box<dyn FnOnce() + Send>;

impl BusyClaude {
    pub(crate) fn leaf(mut self) -> Leaf {
        let notice = self.draft_notice.take();
        Box::new(move |frame| {
            let (source, author, name) = (&self.source, &self.author, &self.name);
            if busy_inject::frame(source, author, name, &self.nonce, &self.text) != frame {
                owner_input::vetoed("frame_mismatch");
                return Ok(Completion::NotSent("frame_mismatch".into()));
            }
            let request = busy_inject::Request {
                session: &self.session,
                transcript: &self.transcript,
                source: &self.source,
                author: &self.author,
                name: &self.name,
                nonce: &self.nonce,
                text: &self.text,
            };
            // Enter ends the transaction: the receipt is only recorded off this thread.
            let inject = || {
                let declared = self.declared;
                let sink = crate::services::claude_tui::guarded_outcome::with_receipt_sink;
                sink(Some(&self.cancel), || {
                    let report =
                        busy_inject::inject_owned(&self.pane, &request, self.timing, declared);
                    busy_inject::collect_receipt(&request, &report, self.timing);
                    (report.outcome, report.draft_unproven)
                })
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(inject));
            // A stop that took the gate before the first key left nothing sent and nothing to resend.
            if matches!(outcome, Ok((Outcome::NotSent(_), _)))
                && self.cancel.native_stop.receipt_sink().stopped_before_key()
            {
                owner_input::stop_took_gate();
                return Ok(Completion::WriteUncertain("stopped_before_key".into()));
            }
            let outcome = outcome.map(|(outcome, unproven)| {
                if unproven && let Some(notice) = notice {
                    tracing::info!(channel_id = self.channel, nonce = %self.nonce, "owned paste met an unparked draft");
                    notice();
                }
                outcome
            });
            let alert = |detail: &str| {
                self.unconfirmed(detail);
                detail.to_string()
            };
            Ok(match outcome {
                Ok(Outcome::NotSent(veto)) => {
                    use busy_inject::Veto::{
                        CopyMode, LoadFailed, Modal, ScreenUnread, TranscriptUnavailable,
                    };
                    match veto {
                        Modal | ScreenUnread | CopyMode => {
                            arm_screen(self.channel, self.pane.clone())
                        }
                        // A buffer that failed to load is a cause a read sees; a frame ledger refusal
                        // is not, so that front waits for the busy turn's end wake.
                        LoadFailed if owner_input::buffer_failed() => {
                            arm_cause(self.channel, self.pane.clone(), loads(self.pane.clone()))
                        }
                        TranscriptUnavailable => {
                            let transcript = self.transcript.clone();
                            arm_cause(self.channel, self.pane.clone(), move || transcript.exists());
                        }
                        _ => {}
                    }
                    refused(format!("{veto:?}"), claude_waits(veto))
                }
                Ok(Outcome::Injected) => Completion::Submitted,
                Ok(Outcome::Failed(WriteFailure::Stash)) => {
                    Completion::Failed(alert("stash_failed"))
                }
                Ok(Outcome::Failed(WriteFailure::Paste)) => {
                    Completion::WriteUncertain(alert("paste_failed"))
                }
                Ok(Outcome::Failed(WriteFailure::Enter)) => {
                    Completion::WriteUncertain(alert("enter_failed"))
                }
                Err(_) => Completion::WriteUncertain(alert("executor_failed")),
            })
        })
    }

    fn unconfirmed(&self, detail: &str) {
        let payload = serde_json::json!({
            "detail": detail,
            "nonce": self.nonce,
            "source": self.source,
            "tmux_session": self.session,
            "owner": true,
        });
        tracing::warn!(channel_id = self.channel, %payload, "owner busy input unconfirmed");
        crate::services::observability::events::record_simple(
            "busy_inject_unconfirmed",
            Some(self.channel),
            Some("claude"),
            payload,
        );
    }
}

/// Leaves a read of the Claude pane that refused under a screen Enter may not submit on, so its
/// recovery wakes the drive.
fn arm_screen(channel: u64, pane: busy_inject::Pane) {
    super::held_watch::arm(
        channel,
        Box::new(move || busy_inject::screen_submits(&pane)),
    );
}

/// Leaves `clear`, a read of the refusal's own cause, with the held watch: its first clear look after
/// the hold wakes the drive once, even when the cause had cleared before, and a lost pane ends it.
pub(super) fn arm_cause(
    channel: u64,
    pane: busy_inject::Pane,
    mut clear: impl FnMut() -> bool + Send + 'static,
) {
    let read = move || match clear() {
        true => busy_inject::Screen::Ready,
        false => pane.lost(),
    };
    super::held_watch::arm(channel, Box::new(read));
}

/// Whether tmux loads a buffer for `pane` again; the probe's buffer is deleted and nothing pasted.
pub(super) fn loads(pane: busy_inject::Pane) -> impl FnMut() -> bool + Send + 'static {
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

/// An idle Claude tmux pane: main's warm submission, whose stash the owner scope admits.
pub(crate) fn idle_claude(
    pane: busy_inject::Pane,
    session: String,
    channel: u64,
    receipt: Arc<crate::services::provider::CancelToken>,
) -> Leaf {
    use crate::services::claude_tui::guarded_outcome::SubmissionError;
    Box::new(move |frame| {
        crate::services::claude_tui::guarded_outcome::with_receipt_sink(Some(&receipt), || {
            let sent = crate::services::claude_tui::input::send_warm_followup_owned(
                &session, &frame, None,
            );
            // A stop that took the writer's gate first left no key and nothing to send again.
            if sent.is_err() && receipt.native_stop.receipt_sink().stopped_before_key() {
                owner_input::stop_took_gate();
                return Ok(Completion::WriteUncertain("stopped_before_key".into()));
            }
            match sent {
                Ok(()) => Ok(Completion::Submitted),
                Err(SubmissionError::Held(hold)) if hold.modal => {
                    arm_screen(channel, pane.clone());
                    Ok(Completion::Held(MODAL.into()))
                }
                Err(SubmissionError::Held(hold)) => Ok(Completion::Held(format!("held: {hold:?}"))),
                Err(error) => Ok(Completion::WriteUncertain(format!("{error:?}"))),
            }
        })
    })
}

/// The first prompt a driver handed over, mapped as the cold submit ends; a Herdr pane's physical
/// writer is bound before the owner's first ready, so no fallback key is ever pinned.
pub(crate) fn created(submit: owner_input::CreatedSubmit, writer: Option<String>) -> Leaf {
    use crate::services::claude_tui::guarded_outcome::SubmissionError;
    Box::new(move |frame| {
        if let Some(Err(refused)) = writer.as_deref().map(owner_input::bind_writer) {
            owner_input::vetoed(&refused);
            return Ok(Completion::NotSent(refused));
        }
        Ok(match submit(frame) {
            Ok(()) => Completion::Submitted,
            Err(SubmissionError::Held(hold)) => Completion::Held(format!("held: {hold:?}")),
            Err(error) => Completion::WriteUncertain(format!("{error:?}")),
        })
    })
}

/// The first prompt of a Claude tmux execution the launch just created; `ticket` keeps what the
/// provider's observer reads next.
#[cfg(test)]
pub(crate) fn cold_claude(
    session: String,
    transcript: String,
    offset: u64,
    ticket: Arc<std::sync::Mutex<Option<crate::services::claude::first_input::Submitted>>>,
) -> Leaf {
    use crate::services::claude::first_input::submit_created;
    created(
        Box::new(move |frame| {
            let submitted = submit_created(&transcript, offset, &session, &frame)?;
            *ticket.lock().unwrap_or_else(|e| e.into_inner()) = Some(submitted);
            Ok(())
        }),
        None,
    )
}

mod codex;
mod herdr;
pub(crate) use codex::{BusyCodex, BusyCodexHerdr, CodexInput, idle_codex};
pub(crate) use herdr::BusyClaudeHerdr;

#[cfg(test)]
#[path = "effect_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "native_receipt_tests.rs"]
mod native_receipt_tests;
