//! Dormant channel input actor: gate, inject and confirm the oldest open ledger row.

use std::collections::BTreeSet;
use std::io;
use std::time::{Duration, Instant};

use super::attempt::{Disposition, Tracking};
use super::ledger::Ledger;
use super::rows::{AttemptEvidence, DoneReason, Entry, HeldReason, Row, RowState, Rows};
use crate::services::tui_o::shadow::capture::SourceCapture;

use crate::services::tui_o::shadow::SourceBinding;
use crate::services::tui_o::writer::input_facts::{ChannelFact, TurnState};

pub mod gate;
pub mod pane;
pub(crate) mod token;
pub(crate) mod witness;

use gate::{PaneVerdict, judge_pane};
use pane::{Pane, SendOutcome};

/// Without a user record or transcript activity this long after Enter, the input was not accepted.
pub const ACCEPT_WINDOW: Duration = Duration::from_secs(15);
/// An idle channel whose pane stays unready this long holds the input for a person.
pub const READY_WINDOW: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Idle,
    Wait(&'static str),
    /// The head row waits for a person or a later stage; younger rows stay behind it.
    Blocked(u64, RowState),
    Moved(u64, RowState),
}

struct Attempt {
    key: u64,
    entered_at: Instant,
    activity: bool,
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
    pub(crate) fn entered_at(&self) -> Instant {
        self.attempt.as_ref().unwrap().entered_at
    }

    #[cfg(test)]
    pub(crate) fn pane(&self) -> &P {
        &self.pane
    }

    /// One decision for the head row; every effect follows its durable intent record. It does
    /// blocking tmux and file IO, so callers run it on a blocking worker.
    pub async fn step(
        &mut self,
        ledger: &mut Ledger,
        fact: Option<&ChannelFact>,
        now: Instant,
    ) -> io::Result<Step> {
        let Some((rows, foreign)) = self.observe(ledger)? else {
            return Ok(Step::Wait("witness_scan"));
        };
        let Some((key, mut row)) = head(&rows) else {
            self.attempt = None;
            return Ok(Step::Idle);
        };
        // Ready follows only NotSent, which left the pane untouched: the next attempt replaces it.
        if row.state == RowState::Ready {
            row.attempt = None;
        }
        if self
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.key != key)
        {
            self.attempt = None;
        }
        if row.received_seq.is_none() {
            return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
        }
        if row.attempt.as_ref().is_some_and(|attempt| {
            attempt.binding != self.binding
                || self.pane.binding_nonce(&self.binding).as_deref()
                    != Some(&attempt.execution_nonce)
        }) {
            return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
        }
        // A fact read from another transcript says nothing about this channel's pane.
        let fact = fact
            .filter(|fact| fact.binding == self.binding)
            .map(|fact| (&fact.state, fact.through));
        match row.state {
            // A frame this ledger never registered may be an old delivery: offer nothing new.
            RowState::Received | RowState::Ready if foreign => Ok(Step::Wait("foreign_frame")),
            RowState::Received | RowState::Ready => self.offer(ledger, key, &row, fact, now).await,
            RowState::AwaitTurn if self.attempt.is_some() => {
                self.confirm(ledger, key, &row, fact, now)
            }
            // A tracked row completes only through a matched close in `observe`.
            RowState::Running if !row.attempts.is_empty() => Ok(Step::Wait("turn_open")),
            RowState::Running => self.finish(ledger, key, &row, fact),
            // This process holds no paste anchor, so the effect cannot be judged; never re-inject.
            RowState::Injecting | RowState::AwaitTurn => self.reconcile(ledger, key, &row),
            state => Ok(Step::Blocked(key, state)),
        }
    }

    /// Records every registered witness and matched close before any decision. Nothing else runs
    /// until the read reached the source's end, and a failed append ends the step.
    fn observe(&self, ledger: &mut Ledger) -> io::Result<Option<(Rows, bool)>> {
        let rows = ledger.rows()?;
        if !rows.open_rows().any(|(_, row)| !row.attempts.is_empty()) {
            return Ok(Some((rows, false)));
        }
        let Ok(seen) = witness::scan_tracked(&self.binding, &rows) else {
            return Ok(None);
        };
        for (key, witness) in seen.witnesses {
            ledger.append_witness(key, witness)?;
        }
        let rows = ledger.rows()?;
        let state = |key| rows.row(key).map(|row| row.state);
        let mut settled = BTreeSet::new();
        for (key, aborted) in seen.closed {
            if state(key) != Some(RowState::Running) || !settled.insert(key) {
                continue;
            }
            let done = RowState::Done(DoneReason::Completed);
            if aborted {
                let tracking = Tracking {
                    disposition: Some(Disposition::Interrupted),
                    ..Tracking::default()
                };
                ledger.append_tracked(key, done, None, &tracking)?;
            } else {
                set(ledger, key, done)?;
            }
        }
        // A provider that rewrote our frame leaves an unconfirmed row's effect unknown.
        for key in seen.altered {
            if matches!(
                state(key),
                Some(RowState::Injecting | RowState::AwaitTurn | RowState::Queued)
            ) {
                set(ledger, key, RowState::Held(HeldReason::Ambiguous))?;
            }
        }
        if !seen.complete {
            return Ok(None);
        }
        Ok(Some((ledger.rows()?, seen.foreign > 0)))
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
        let Some((mut text, source_ids)) = witness::frame(key, row) else {
            return set(ledger, key, RowState::Held(HeldReason::NotReady));
        };
        if let Some(attempt) = &row.attempt {
            if attempt.source_ids != source_ids {
                return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
            }
            text = attempt.rendered_prompt.clone();
        }
        let binding = self.binding.clone();
        let unready_since = &mut self.unready_since;
        let mut entered = None;
        let result = self.pane.with_composer(|pane| {
            let verdict = pane
                .capture()
                .map(|c| judge_pane(binding.provider, &c))
                .unwrap_or(PaneVerdict::NotReady);
            match verdict {
                PaneVerdict::Modal => return set(ledger, key, RowState::Held(HeldReason::Modal)),
                PaneVerdict::NotReady => {
                    let since = *unready_since.get_or_insert(now);
                    if now.saturating_duration_since(since) >= READY_WINDOW {
                        *unready_since = None;
                        return set(ledger, key, RowState::Held(HeldReason::NotReady));
                    }
                    return Ok(Step::Wait("pane_not_ready"));
                }
                PaneVerdict::Ready => *unready_since = None,
            }
            let Some(execution_nonce) = pane.binding_nonce(&binding) else {
                return Ok(Step::Wait("execution_unknown"));
            };
            let source = binding.source.clone();
            let Ok(eof) = std::fs::metadata(&source.path).map(|meta| meta.len()) else {
                return Ok(Step::Wait("transcript_unavailable"));
            };
            if SourceCapture::open(source, eof).is_err() {
                return Ok(Step::Wait("transcript_unavailable"));
            }
            let evidence = AttemptEvidence {
                binding: binding.clone(),
                execution_nonce: execution_nonce.clone(),
                eof,
                rendered_prompt: text.clone(),
                source_ids,
                record_end: None,
                native_turn_id: None,
            };
            ledger.append_entry(
                &Entry::Transition {
                    key,
                    state: RowState::Injecting,
                    attempt: Some(evidence.clone()),
                },
                &[],
            )?;
            if pane.binding_nonce(&binding).as_deref() != Some(&execution_nonce) {
                return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
            }
            let state = match pane.submit_for_binding(&text, &binding) {
                SendOutcome::Sent => RowState::AwaitTurn,
                SendOutcome::NotSent(_) => RowState::Ready,
                SendOutcome::Indeterminate(_) => RowState::Held(HeldReason::Ambiguous),
                SendOutcome::Refused(_) => RowState::Held(HeldReason::NotReady),
            };
            entered = (state == RowState::AwaitTurn).then(Instant::now);
            set(ledger, key, state)
        });
        let Some(result) = result else {
            return Ok(Step::Wait("composer_locked"));
        };
        let step = result?;
        if step == Step::Moved(key, RowState::AwaitTurn) {
            self.attempt = Some(Attempt {
                key,
                entered_at: {
                    #[cfg(test)]
                    if super::transition::mutant("entered_start") {
                        now
                    } else {
                        entered.expect("sent attempt timestamp")
                    }
                    #[cfg(not(test))]
                    {
                        entered.expect("sent attempt timestamp")
                    }
                },
                activity: false,
            });
        }
        Ok(step)
    }

    fn reconcile(&mut self, ledger: &mut Ledger, key: u64, row: &Row) -> io::Result<Step> {
        let Some(mut evidence) = row.attempt.clone() else {
            return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
        };
        match witness::scan(&evidence, false) {
            Ok(Some((end, native))) => {
                evidence.record_end = Some(end);
                evidence.native_turn_id = native;
                ledger.append_entry(
                    &Entry::Transition {
                        key,
                        state: RowState::Running,
                        attempt: Some(evidence),
                    },
                    &[],
                )?;
                Ok(Step::Moved(key, RowState::Running))
            }
            _ => set(ledger, key, RowState::Held(HeldReason::Ambiguous)),
        }
    }

    fn confirm(
        &mut self,
        ledger: &mut Ledger,
        key: u64,
        row: &Row,
        fact: Option<(&TurnState, u64)>,
        now: Instant,
    ) -> io::Result<Step> {
        let Some(mut evidence) = row.attempt.clone() else {
            return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
        };
        match witness::scan(&evidence, false) {
            Err(_) => return set(ledger, key, RowState::Held(HeldReason::Ambiguous)),
            Ok(Some((end, native))) => {
                evidence.record_end = Some(end);
                evidence.native_turn_id = native;
                ledger.append_entry(
                    &Entry::Transition {
                        key,
                        state: RowState::Running,
                        attempt: Some(evidence),
                    },
                    &[],
                )?;
                return Ok(Step::Moved(key, RowState::Running));
            }
            Ok(None) => {}
        }
        let attempt = self.attempt.as_mut().expect("confirm requires an attempt");
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
        row: &Row,
        fact: Option<(&TurnState, u64)>,
    ) -> io::Result<Step> {
        #[cfg(test)]
        if super::transition::mutant("missing_witness")
            && matches!(fact, Some((TurnState::Idle, _)))
        {
            return set(ledger, key, RowState::Done(DoneReason::Completed));
        }
        let Some(evidence) = row.attempt.as_ref().filter(|e| e.record_end.is_some()) else {
            return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
        };
        if !matches!(fact, Some((TurnState::Idle, through)) if through >= evidence.record_end.unwrap())
        {
            return Ok(Step::Wait("turn_open"));
        }
        match witness::scan(evidence, true) {
            Ok(Some(_)) => {
                self.attempt = None;
                set(ledger, key, RowState::Done(DoneReason::Completed))
            }
            Ok(None) => Ok(Step::Wait("turn_open")),
            Err(_) => set(ledger, key, RowState::Held(HeldReason::Ambiguous)),
        }
    }
}

/// The open row with the oldest arrival.
pub(crate) fn head(rows: &Rows) -> Option<(u64, Row)> {
    #[cfg(test)]
    if super::transition::mutant("head_key") {
        return rows
            .open_rows()
            .min_by_key(|(key, _)| *key)
            .map(|(key, row)| (key, row.clone()));
    }
    rows.open_rows()
        .min_by_key(|(_, row)| row.received_seq.unwrap_or(0))
        .map(|(key, row)| (key, row.clone()))
}

fn set(ledger: &mut Ledger, key: u64, state: RowState) -> io::Result<Step> {
    ledger.append_entry(
        &Entry::Transition {
            key,
            state,
            attempt: None,
        },
        &[],
    )?;
    Ok(Step::Moved(key, state))
}
