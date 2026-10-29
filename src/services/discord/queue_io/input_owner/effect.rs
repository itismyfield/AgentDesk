//! The owner's effect for one reserved attempt: its provider leaf runs inside the owner scope and
//! the attempt settles from what the leaf proved, never from a resend.
#![cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;

use crate::services::claude_tui::busy_inject::{self, Outcome, Receipt, WriteFailure};
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
    /// No key reached the pane and the owner holds the proof; the original stays in custody.
    NotSent(String),
    /// C-s went but no content: a person's draft may be parked.
    Failed(String),
    /// Content may have reached the pane; it is never sent again.
    WriteUncertain(String),
}

pub(crate) type Leaf = Box<dyn FnOnce(String) -> Result<Completion, String> + Send>;
pub(crate) type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Runs `leaf` on the reserved head and settles it: Submitted delivers, a proven NotSent returns
/// to custody, anything else leaves the head Unknown.
pub(crate) async fn run(
    mailbox: &ChannelMailboxHandle,
    clock: Clock,
    leaf: Leaf,
) -> Result<Completion, String> {
    let (identity, outcome) = mailbox
        .v2_cold_observed(clock(), clock.clone(), leaf)
        .await?;
    let completion = outcome.result.unwrap_or_else(Completion::WriteUncertain);
    if let Some(proof) = outcome.not_sent {
        mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("V2 not adopted")?;
                owner.retain_not_sent(&parse(identity)?, proof)
            })
            .await?;
        return Ok(match completion {
            Completion::NotSent(reason) => Completion::NotSent(reason),
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
        Completion::NotSent(reason) => Completion::WriteUncertain(format!("unproven: {reason}")),
        other => other,
    })
}

/// The owner's own types, read back from the identity it handed out.
pub(super) fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// A busy Claude tmux pane: main's busy transaction and its one confirm, mapped as main maps them.
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
}

impl BusyClaude {
    pub(crate) fn leaf(self) -> Leaf {
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
            let inject = || {
                let report = busy_inject::inject_report(
                    &self.pane,
                    &request,
                    Some(self.channel),
                    self.timing,
                );
                let receipt = busy_inject::confirm(&self.pane, &request, &report, self.timing);
                (report.outcome, receipt)
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(inject));
            let alert = |detail: &str| {
                self.unconfirmed(detail);
                detail.to_string()
            };
            Ok(match outcome {
                Ok((Outcome::NotSent(veto), _)) => {
                    let reason = format!("{veto:?}");
                    owner_input::vetoed(&reason);
                    Completion::NotSent(reason)
                }
                // Paste and Enter went: a receipt neither window saw is only observed, and the
                // attempt is delivered without a deadline or retention.
                Ok((Outcome::Injected, Some(Receipt::Missing { .. }))) => {
                    self.unconfirmed("not_observed");
                    Completion::Submitted
                }
                Ok((Outcome::Injected, _)) => Completion::Submitted,
                Ok((Outcome::Failed(WriteFailure::Stash), _)) => {
                    Completion::Failed(alert("stash_failed"))
                }
                Ok((Outcome::Failed(WriteFailure::Paste), _)) => {
                    Completion::WriteUncertain(alert("paste_failed"))
                }
                Ok((Outcome::Failed(WriteFailure::Enter), _)) => {
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

/// An idle Claude tmux pane: main's warm submission, whose stash the owner scope admits.
pub(crate) fn idle_claude(session: String) -> Leaf {
    use crate::services::claude_tui::guarded_outcome::SubmissionError;
    Box::new(move |frame| {
        match crate::services::claude_tui::input::send_warm_followup_owned(&session, &frame, None) {
            Ok(()) => Ok(Completion::Submitted),
            Err(SubmissionError::Held(hold)) => Ok(Completion::NotSent(format!("held: {hold:?}"))),
            Err(error) => Ok(Completion::WriteUncertain(format!("{error:?}"))),
        }
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
            Err(SubmissionError::Held(hold)) => Completion::NotSent(format!("held: {hold:?}")),
            Err(error) => Completion::WriteUncertain(format!("{error:?}")),
        })
    })
}

/// The first prompt of a Claude tmux execution the launch just created; `ticket` keeps what the
/// provider's observer reads next.
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
pub(crate) use codex::{BusyCodex, BusyCodexHerdr, CodexInput};
pub(crate) use herdr::BusyClaudeHerdr;

#[cfg(test)]
#[path = "effect_tests.rs"]
mod tests;
