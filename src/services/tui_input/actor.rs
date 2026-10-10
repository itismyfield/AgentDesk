//! Dormant channel input actor: durable guarded offers and exact input witnesses.

use std::collections::BTreeSet;
use std::io;
use std::time::{Duration, Instant};

use super::attempt::{AttemptMeta, Disposition, Effect, Tracking, WitnessKind, fresh_token};
use super::ledger::Ledger;
use super::rows::{AttemptEvidence, DoneReason, Entry, HeldReason, Row, RowState, Rows};
use crate::services::tui_o::shadow::capture::SourceCapture;

use crate::services::tui_o::shadow::SourceBinding;
use crate::services::tui_o::writer::input_facts::{ChannelFact, TurnState};

pub mod capability;
pub mod gate;
pub mod pane;
pub mod resume;
pub(crate) mod token;
pub(crate) mod witness;

use gate::PaneVerdict;
use pane::{Pane, SendOutcome};

/// Watchdog for a missing registered input witness after Enter.
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
    tracked: bool,
}

pub struct InputActor<P> {
    binding: SourceBinding,
    pane: P,
    attempt: Option<Attempt>,
    unready_since: Option<Instant>,
    capability: capability::CapabilitySnapshot,
}

impl<P: Pane> InputActor<P> {
    pub fn new(binding: SourceBinding, pane: P) -> Self {
        Self {
            binding,
            pane,
            attempt: None,
            unready_since: None,
            capability: capability::CapabilitySnapshot::default(),
        }
    }

    /// A fresh attach derives a memory-only capability; ordinary construction stays disabled.
    pub fn attach(binding: SourceBinding, pane: P, evidence: capability::AttachEvidence) -> Self {
        let mut actor = Self::new(binding, pane);
        actor.capability = capability::CapabilitySnapshot::derive(evidence);
        actor
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
        let nonce = self
            .capability
            .enabled()
            .then(|| self.pane.binding_nonce(&self.binding))
            .flatten();
        let busy = self.capability.validate(&self.binding, nonce.as_deref());
        let selected = if busy {
            busy_head(&rows, &self.binding, nonce.as_deref())
        } else {
            head(&rows)
        };
        let Some((key, mut row)) = selected else {
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
        // An old durable queue waits for explicit restart proofs, never the ordinary offer path.
        let awaiting_resume = busy
            && row.state == RowState::Queued
            && has_current_q(&row)
            && row.attempts.last().is_some_and(|meta| {
                meta.source != self.binding.source
                    || Some(meta.execution_nonce.as_str()) != nonce.as_deref()
            });
        #[cfg(test)]
        let awaiting_resume =
            awaiting_resume && !super::transition::mutant("resume_old_nonce_held");
        if awaiting_resume {
            return Ok(Step::Blocked(key, row.state));
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
        let tracked = self.attempt.as_ref().is_some_and(|a| a.tracked);
        #[cfg(test)]
        let tracked = tracked
            || (super::transition::mutant("legacy_tracked_watchdog")
                && !row.attempts.is_empty()
                && self.attempt.is_some());
        match row.state {
            // A frame this ledger never registered may be an old delivery: offer nothing new.
            RowState::Received | RowState::Ready if foreign => Ok(Step::Wait("foreign_frame")),
            RowState::Received | RowState::Ready => self.offer(ledger, key, &row, fact, now, None),
            RowState::Queued if tracked && !has_current_q(&row) => {
                self.confirm_tracked(ledger, key, now)
            }
            RowState::AwaitTurn if tracked => self.confirm_tracked(ledger, key, now),
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
    fn observe(&mut self, ledger: &mut Ledger) -> io::Result<Option<(Rows, bool)>> {
        let rows = ledger.rows()?;
        if !rows.open_rows().any(|(_, row)| !row.attempts.is_empty()) {
            return Ok(Some((rows, false)));
        }
        let seen = if self.capability.enabled() {
            witness::scan_lineage(&self.binding, &rows).map_err(|error| {
                if matches!(error, witness::LineageError::CurrentIdentityChanged) {
                    invalidate(&mut self.capability, "drift_observe_keep");
                }
            })
        } else {
            witness::scan_tracked(&self.binding, &rows).map_err(|_| ())
        };
        let Ok(seen) = seen else {
            return Ok(None);
        };
        for (key, witness) in seen.witnesses {
            #[cfg(test)]
            let queued = witness.kind == WitnessKind::Queued;
            ledger.append_witness(key, witness)?;
            #[cfg(test)]
            if queued && super::transition::mutant("busy_q_running") {
                set(ledger, key, RowState::Running)?;
            }
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

    fn offer(
        &mut self,
        ledger: &mut Ledger,
        key: u64,
        row: &Row,
        fact: Option<(&TurnState, u64)>,
        now: Instant,
        restart: Option<&resume::ResumeEvidence>,
    ) -> io::Result<Step> {
        let busy = self.capability.enabled();
        if !matches!(fact, Some((TurnState::Idle, _)))
            && !(busy && matches!(fact, Some((TurnState::Open { .. }, _))))
        {
            self.unready_since = None;
            return Ok(Step::Wait("turn_not_idle"));
        }
        let Some((mut text, source_ids)) = witness::frame(key, row) else {
            return set(ledger, key, RowState::Held(HeldReason::NotReady));
        };
        if let Some(attempt) = row.attempt.as_ref().filter(|_| !busy) {
            if attempt.source_ids != source_ids {
                return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
            }
            text = attempt.rendered_prompt.clone();
        }
        let profile = token::profile(self.binding.provider);
        let tracked = busy.then(|| {
            text = pane::canonical(self.binding.provider, &text);
            let token = fresh_token();
            text = token::render(&token, &text);
            (
                token,
                token::digest(profile, &text).expect("known frame profile"),
            )
        });
        let binding = self.binding.clone();
        let capability = &mut self.capability;
        let unready_since = &mut self.unready_since;
        let mut entered = None;
        let operation = |pane: &mut P| {
            let verdict = pane
                .capture()
                .map(|c| pane::ready(binding.provider, &c, busy))
                .unwrap_or(PaneVerdict::NotReady);
            match verdict {
                PaneVerdict::Modal => return set(ledger, key, RowState::Held(HeldReason::Modal)),
                PaneVerdict::NotReady => {
                    if restart.is_some() {
                        #[cfg(test)]
                        if super::transition::mutant("resume_unready_wait") {
                            return Ok(Step::Wait("pane_not_ready"));
                        }
                        return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
                    }
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
                if busy {
                    invalidate(capability, "drift_nonce_keep");
                }
                return Ok(Step::Wait("execution_unknown"));
            };
            if busy && !capability.validate(&binding, Some(&execution_nonce)) {
                return Ok(Step::Wait("capability_changed"));
            }
            let queue_end = if let Some(restart) = restart {
                let mut fresh = restart.clone();
                fresh.current_valid &=
                    capability.validate(&binding, pane.binding_nonce(&binding).as_deref());
                fresh.exact_empty &= pane.capture().is_ok_and(|capture| {
                    pane::ready(binding.provider, &capture, busy) == PaneVerdict::Ready
                });
                let rows = ledger.rows()?;
                let seen = match witness::scan_lineage(&binding, &rows) {
                    Ok(seen) => seen,
                    Err(error) => {
                        if matches!(error, witness::LineageError::CurrentIdentityChanged) {
                            invalidate(capability, "drift_observe_keep");
                        }
                        return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
                    }
                };
                let complete = seen.complete && seen.altered.is_empty() && seen.foreign == 0;
                for (key, witness) in seen.witnesses {
                    ledger.append_witness(key, witness)?;
                }
                let rows = ledger.rows()?;
                let row = rows.row(key).expect("resume row exists");
                fresh.lineage_complete &= complete;
                fresh.all_generations_clear &= witness::stable_current(restart);
                let Some(end) = resume::decide(row, &binding, &execution_nonce, &fresh, now) else {
                    return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
                };
                Some(end)
            } else {
                None
            };
            let source = binding.source.clone();
            let Ok(eof) = std::fs::metadata(&source.path).map(|meta| meta.len()) else {
                return Ok(Step::Wait("transcript_unavailable"));
            };
            if SourceCapture::open(source.clone(), eof).is_err() {
                if witness::identity_changed(&source) {
                    invalidate(capability, "drift_offer_keep");
                }
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
            if let Some((token, digest)) = &tracked {
                let meta = AttemptMeta {
                    generation: row
                        .attempts
                        .last()
                        .map_or(1, |a| a.generation.saturating_add(1)),
                    token: token.clone(),
                    frame_digest: digest.clone(),
                    frame_profile: Some(profile.into()),
                    execution_nonce: execution_nonce.clone(),
                    source: binding.source.clone(),
                    anchor: eof,
                    effect: Effect::Intent,
                    incarnation: None,
                    queue_end: queue_end.clone(),
                };
                ledger.append_tracked(
                    key,
                    RowState::Injecting,
                    Some(evidence),
                    &Tracking {
                        attempt: Some(meta),
                        ..Tracking::default()
                    },
                )?;
            } else {
                ledger.append_entry(
                    &Entry::Transition {
                        key,
                        state: RowState::Injecting,
                        attempt: Some(evidence.clone()),
                    },
                    &[],
                )?;
            }
            if pane.binding_nonce(&binding).as_deref() != Some(&execution_nonce) {
                invalidate(capability, "drift_nonce_keep");
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
        };
        let result = if busy {
            self.pane.with_busy_composer(operation)
        } else {
            self.pane.with_composer(operation)
        };
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
                tracked: busy,
            });
        }
        Ok(step)
    }

    fn confirm_tracked(&mut self, ledger: &mut Ledger, key: u64, now: Instant) -> io::Result<Step> {
        let attempt = self
            .attempt
            .as_ref()
            .expect("tracked confirmation has Enter time");
        if now
            .checked_duration_since(attempt.entered_at)
            .is_some_and(|d| d >= ACCEPT_WINDOW)
        {
            #[cfg(test)]
            if super::transition::mutant("no_token_not_sent") {
                return set(ledger, key, RowState::Ready);
            }
            #[cfg(test)]
            if super::transition::mutant("no_token_retry") {
                let row = ledger.rows()?.row(key).expect("attempt row").clone();
                let text = &row.attempt.as_ref().expect("sent frame").rendered_prompt;
                let binding = self.binding.clone();
                let _ = self
                    .pane
                    .with_busy_composer(|pane| pane.submit_for_binding(text, &binding));
                return Ok(Step::Wait("awaiting_input_witness"));
            }
            // The durable Held transition is the Notice intent; no token never proves NotSent.
            return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
        }
        Ok(Step::Wait("awaiting_input_witness"))
    }

    /// Only a verified queue termination opens a new generation; replayed episodes send nothing.
    pub fn resume_queued(
        &mut self,
        ledger: &mut Ledger,
        key: u64,
        evidence: &resume::ResumeEvidence,
        now: Instant,
    ) -> io::Result<Step> {
        let rows = ledger.rows()?;
        let Some(row) = rows.row(key) else {
            return Ok(Step::Idle);
        };
        if row
            .attempts
            .last()
            .and_then(|a| a.queue_end.as_ref())
            .is_some_and(|end| {
                end.old_nonce == evidence.old_nonce
                    && Some(end.new_nonce.as_str())
                        == self.pane.binding_nonce(&self.binding).as_deref()
            })
        {
            return Ok(Step::Blocked(key, row.state));
        }
        let nonce = self.pane.binding_nonce(&self.binding);
        if !self.capability.validate(&self.binding, nonce.as_deref()) {
            return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
        }
        if busy_head(&rows, &self.binding, nonce.as_deref()).map(|r| r.0) != Some(key) {
            #[cfg(test)]
            if super::transition::mutant("resume_not_head_held") {
                return set(ledger, key, RowState::Held(HeldReason::Ambiguous));
            }
            return Ok(Step::Blocked(key, row.state));
        }
        self.offer(
            ledger,
            key,
            row,
            Some((&TurnState::Idle, 0)),
            now,
            Some(evidence),
        )
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

fn busy_head(rows: &Rows, binding: &SourceBinding, nonce: Option<&str>) -> Option<(u64, Row)> {
    #[cfg(test)]
    if super::transition::mutant("busy_oldest_blocker") {
        return head(rows);
    }
    let pass = |row: &Row| {
        matches!(row.state, RowState::Queued | RowState::Running)
            && row.attempts.last().is_some_and(|meta| {
                meta.source == binding.source
                    && Some(meta.execution_nonce.as_str()) == nonce
                    && row.witnesses.iter().any(|seen| {
                        seen.witness.token == meta.token
                            && (seen.witness.kind == WitnessKind::Queued
                                || seen.witness.kind.confirms_input())
                    })
            })
    };
    rows.open_rows()
        .filter(|(_, row)| !pass(row))
        .min_by_key(|(_, row)| row.received_seq.unwrap_or(0))
        .map(|(key, row)| (key, row.clone()))
        .or_else(|| head(rows))
}

fn has_current_q(row: &Row) -> bool {
    #[cfg(test)]
    if super::transition::mutant("watchdog_any_q") {
        return row
            .witnesses
            .iter()
            .any(|seen| seen.witness.kind == WitnessKind::Queued);
    }
    row.attempts.last().is_some_and(|meta| {
        row.witnesses.iter().any(|seen| {
            seen.witness.generation == meta.generation
                && seen.witness.token == meta.token
                && seen.witness.kind == WitnessKind::Queued
        })
    })
}

fn invalidate(capability: &mut capability::CapabilitySnapshot, _mutant: &str) {
    #[cfg(test)]
    if super::transition::mutant(_mutant) {
        return;
    }
    capability.invalidate();
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
