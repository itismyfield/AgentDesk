//! Dormant channel input actor: gate, inject and confirm the oldest open ledger row.

use std::io;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::ledger::Ledger;
use super::rows::{DoneReason, Entry, HeldReason, Row, RowState, Rows};
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::identity::{RecordFact, classify};
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource, SourceBinding};
use crate::services::tui_o::writer::input_facts::{ChannelFact, TurnState};
use crate::services::tui_prompt_dedupe::prompts_match;

pub mod gate;
pub mod pane;

use gate::{PaneVerdict, judge_pane};
use pane::{Pane, SendOutcome};

/// Without a user record or transcript activity this long after Enter, the input was not accepted.
pub const ACCEPT_WINDOW: Duration = Duration::from_secs(15);
/// An idle channel whose pane stays unready this long holds the input for a person.
pub const READY_WINDOW: Duration = Duration::from_secs(120);
const CONFIRM_READ_BYTES: u64 = 1024 * 1024;
const CONFIRM_POLLS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Idle,
    Wait(&'static str),
    /// The head row waits for a person or a later stage; younger rows stay behind it.
    Blocked(u64, RowState),
    Moved(u64, RowState),
}

// The transcript cursor opened just before the paste; only records after it can confirm.
struct Attempt {
    key: u64,
    capture: SourceCapture,
    entered_at: Instant,
    activity: bool,
    record_end: Option<u64>,
}

pub struct InputActor<P> {
    binding: SourceBinding,
    pane: P,
    attempt: Option<Attempt>,
    unready_since: Option<Instant>,
}

impl<P: Pane> InputActor<P> {
    pub fn new(binding: SourceBinding, pane: P) -> Self {
        Self {
            binding,
            pane,
            attempt: None,
            unready_since: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn pane(&self) -> &P {
        &self.pane
    }

    /// One decision for the head row; every effect follows its durable intent record.
    pub async fn step(
        &mut self,
        ledger: &mut Ledger,
        fact: Option<&ChannelFact>,
        now: Instant,
    ) -> io::Result<Step> {
        let rows = ledger.rows()?;
        let Some((key, row)) = head(&rows) else {
            self.attempt = None;
            return Ok(Step::Idle);
        };
        if self
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.key != key)
        {
            self.attempt = None;
        }
        // A fact read from another transcript says nothing about this channel's pane.
        let fact = fact
            .filter(|fact| fact.binding == self.binding)
            .map(|fact| (&fact.state, fact.through));
        match row.state {
            RowState::Received | RowState::Ready => self.offer(ledger, key, &row, fact, now).await,
            RowState::AwaitTurn if self.attempt.is_some() => {
                self.confirm(ledger, key, &row, fact, now)
            }
            RowState::Running => self.finish(ledger, key, fact),
            // This process holds no paste anchor, so the effect cannot be judged; never re-inject.
            RowState::Injecting | RowState::AwaitTurn => {
                set(ledger, key, RowState::Held(HeldReason::Ambiguous))
            }
            state => Ok(Step::Blocked(key, state)),
        }
    }

    async fn offer(
        &mut self,
        ledger: &mut Ledger,
        key: u64,
        row: &Row,
        fact: Option<(&TurnState, u64)>,
        now: Instant,
    ) -> io::Result<Step> {
        if !matches!(fact, Some((TurnState::Idle, _))) {
            self.unready_since = None;
            return Ok(Step::Wait("turn_not_idle"));
        }
        let Some(text) = row.input.get("text").and_then(Value::as_str) else {
            return set(ledger, key, RowState::Held(HeldReason::NotReady));
        };
        let verdict = match self.pane.capture().await {
            Ok(capture) => judge_pane(self.binding.provider, &capture),
            Err(_) => PaneVerdict::NotReady,
        };
        match verdict {
            PaneVerdict::Modal => return set(ledger, key, RowState::Held(HeldReason::Modal)),
            PaneVerdict::NotReady => {
                let since = *self.unready_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= READY_WINDOW {
                    self.unready_since = None;
                    return set(ledger, key, RowState::Held(HeldReason::NotReady));
                }
                return Ok(Step::Wait("pane_not_ready"));
            }
            PaneVerdict::Ready => self.unready_since = None,
        }
        // Without a transcript cursor the paste could never be confirmed, so nothing is sent.
        let source = self.binding.source.clone();
        let Ok(capture) = std::fs::metadata(&source.path)
            .and_then(|meta| SourceCapture::open(source, meta.len()))
        else {
            return Ok(Step::Wait("transcript_unavailable"));
        };
        set(ledger, key, RowState::Injecting)?;
        let state = match self.pane.submit(text).await {
            SendOutcome::Sent => RowState::AwaitTurn,
            SendOutcome::NotSent(_) => RowState::Ready,
            SendOutcome::Indeterminate(_) => RowState::Held(HeldReason::Ambiguous),
            SendOutcome::Refused(_) => RowState::Held(HeldReason::NotReady),
        };
        let step = set(ledger, key, state)?;
        if state == RowState::AwaitTurn {
            self.attempt = Some(Attempt {
                key,
                capture,
                entered_at: now,
                activity: false,
                record_end: None,
            });
        }
        Ok(step)
    }

    fn confirm(
        &mut self,
        ledger: &mut Ledger,
        key: u64,
        row: &Row,
        fact: Option<(&TurnState, u64)>,
        now: Instant,
    ) -> io::Result<Step> {
        let provider = self.binding.provider;
        let attempt = self.attempt.as_mut().expect("confirm requires an attempt");
        let expected = row.input.get("text").and_then(Value::as_str).unwrap_or("");
        match find_user_record(&mut attempt.capture, provider, expected) {
            Err(_) => return set(ledger, key, RowState::Held(HeldReason::Ambiguous)),
            Ok(Some(end)) => {
                attempt.record_end = Some(end);
                return set(ledger, key, RowState::Running);
            }
            Ok(None) => {}
        }
        match fact {
            Some((TurnState::Open { .. }, _)) => attempt.activity = true,
            // A turn opened and closed after Enter without our record: it was someone else's.
            Some((TurnState::Idle, _)) if attempt.activity => {
                return set(ledger, key, RowState::Unaccepted);
            }
            _ => {}
        }
        if !attempt.activity && now.saturating_duration_since(attempt.entered_at) >= ACCEPT_WINDOW {
            return set(ledger, key, RowState::Unaccepted);
        }
        Ok(Step::Wait("awaiting_user_record"))
    }

    fn finish(
        &mut self,
        ledger: &mut Ledger,
        key: u64,
        fact: Option<(&TurnState, u64)>,
    ) -> io::Result<Step> {
        let record_end = self.attempt.as_ref().and_then(|attempt| attempt.record_end);
        match fact {
            // Only an Idle read past our own record closes the turn it opened.
            Some((TurnState::Idle, through)) if record_end.is_none_or(|end| through >= end) => {
                self.attempt = None;
                set(ledger, key, RowState::Done(DoneReason::Completed))
            }
            _ => Ok(Step::Wait("turn_open")),
        }
    }
}

// Rows keep arrival order: the activation seq, then the Discord message id.
fn head(rows: &Rows) -> Option<(u64, Row)> {
    rows.open_rows()
        .min_by_key(|(key, row)| (row.since_seq, *key))
        .map(|(key, row)| (key, row.clone()))
}

fn set(ledger: &mut Ledger, key: u64, state: RowState) -> io::Result<Step> {
    ledger.append_entry(&Entry::Transition { key, state }, &[])?;
    Ok(Step::Moved(key, state))
}

fn find_user_record(
    capture: &mut SourceCapture,
    provider: crate::services::tui_o::shadow::ShadowProvider,
    expected: &str,
) -> Result<Option<u64>, String> {
    for _ in 0..CONFIRM_POLLS {
        let batch = match capture.poll(CONFIRM_READ_BYTES) {
            CaptureOutcome::Batch(batch) => batch,
            CaptureOutcome::Anomaly(anomaly) => return Err(anomaly.detail),
        };
        if batch.records.is_empty() {
            return Ok(None);
        }
        for record in batch.records {
            let Ok(value) = serde_json::from_slice::<Value>(&record.line) else {
                continue;
            };
            if value.get("isSidechain") == Some(&Value::Bool(true)) {
                continue;
            }
            let matched = classify(provider, &value).into_iter().any(|fact| {
                matches!(fact, RecordFact::Prompt(_, text) if prompts_match(expected, &text))
            });
            if matched {
                return Ok(Some(record.end));
            }
        }
    }
    Ok(None)
}
