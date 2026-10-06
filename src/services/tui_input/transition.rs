//! Dormant move and handback execution; activation supplies the boot-time intake fences.

use std::collections::BTreeSet;
use std::io;
use std::time::Duration;

use serde_json::Value;

use super::blob::BlobPin;
use super::handover::{
    Composer, EnqueueOutcome, Handback, MoveEvidence, MoveSource, handback_after_enqueue,
    handback_plan, move_disposition,
};
use super::ledger::{Ledger, LedgerLease};
use super::rows::{Entry, Owner, Row, Rows};

pub struct Input {
    pub key: u64,
    pub payload: Value,
    pub source: MoveSource,
    pub pins: Vec<BlobPin>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeletePhase {
    Dispatch,
    Queue,
    Accessories,
    Row,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Legacy,
    Ledger,
    Held,
}

// Evidence and enqueue remain owned by the existing transcript and Legacy adapters.
pub trait Host {
    fn collect(&mut self, ledger: &Ledger) -> io::Result<Vec<Input>>;
    fn evidence(&mut self, input: &Input) -> io::Result<MoveEvidence>;
    fn pin_input(&mut self, _ledger: &Ledger, _input: &mut Input) -> io::Result<()> {
        Ok(())
    }
    fn delete(&mut self, phase: DeletePhase) -> io::Result<()>;
    fn start_actor(&mut self) -> io::Result<()>;
    fn reconcile(&mut self, key: u64, row: &Row) -> io::Result<(bool, Composer)>;
    fn enqueue(&mut self, key: u64, row: &Row) -> io::Result<EnqueueOutcome>;
    fn notice(&mut self, key: Option<u64>, reason: &'static str) -> io::Result<()>;
}

#[derive(Clone, Copy)]
enum Phase {
    Stage,
    Commit,
    Delete(usize),
    Actor,
    Finished(Outcome),
}

pub struct Move {
    inputs: Vec<Input>,
    first: u64,
    phase: Phase,
    stage_retry: bool,
    refused_stage: bool,
    notice_sent: bool,
}

const DELETIONS: [DeletePhase; 4] = [
    DeletePhase::Dispatch,
    DeletePhase::Queue,
    DeletePhase::Accessories,
    DeletePhase::Row,
];

impl Move {
    pub fn prepare(lease: &mut LedgerLease, host: &mut impl Host) -> io::Result<Self> {
        let ledger = lease.get()?;
        let rows = ledger.rows()?;
        let inputs = match host.collect(ledger) {
            Ok(inputs) => inputs,
            Err(error) => {
                if rows.ledger_owned() || rows.boundary_since(1) {
                    return Err(error);
                }
                host.notice(None, "tui_o:turn_mode_refused")?;
                return Ok(Self::new(Vec::new(), 0, Phase::Finished(Outcome::Legacy)));
            }
        };
        let mut ids = BTreeSet::new();
        if inputs
            .iter()
            .any(|input| input.key == 0 || !ids.insert(input.key))
        {
            return Err(io::Error::other("move population has invalid primary ids"));
        }
        // An unbound key's Legacy copy is its only content, so no step may delete it.
        let unbound: Vec<u64> = (inputs.iter().map(|i| i.key))
            .filter(|key| rows.unbound().contains(key))
            .collect();
        if !unbound.is_empty() {
            for key in unbound {
                host.notice(Some(key), "move_unbound")?;
            }
            return Err(io::Error::other("move population names unbound keys"));
        }
        let claimed = rows.ledger_owned()
            || inputs.iter().any(|i| rows.owner(i.key) != Owner::Legacy)
            || (inputs.is_empty() && rows.boundary_since(1));
        if claimed && inputs.iter().any(|i| rows.owner(i.key) == Owner::Legacy) {
            return Err(io::Error::other(
                "uncommitted input appeared alongside committed inputs",
            ));
        }
        if claimed
            && inputs.iter().any(|i| {
                rows.row(i.key).is_some_and(|row| {
                    let original = row.input.get("legacy_input").unwrap_or(&row.input);
                    !row.state.is_terminal()
                        && if i.source == MoveSource::TurnRow {
                            // A retired marker held richer metadata than the surviving row wire format.
                            [
                                "author_id",
                                "message_id",
                                "text",
                                "source_message_ids",
                                "queued_generation",
                                "reply_context",
                                "has_reply_boundary",
                                "merge_consecutive",
                                "pending_uploads",
                                "voice_announcement",
                            ]
                            .iter()
                            .any(|field| {
                                let captured = &i.payload[*field];
                                let saved = &original[*field];
                                captured != saved
                                    && !(saved.is_null()
                                        && (captured == &Value::Bool(false)
                                            || captured == &serde_json::json!([])
                                            || captured == &Value::from(0)))
                            })
                        } else {
                            original != &i.payload
                        }
                })
            })
        {
            return Err(io::Error::other(
                "committed input differs from captured Legacy source",
            ));
        }
        let first = rows
            .folded_seq()
            .checked_add(1)
            .ok_or_else(|| io::Error::other("sequence exhausted"))?;
        Ok(Self::new(
            inputs,
            first,
            if claimed {
                Phase::Delete(0)
            } else {
                Phase::Stage
            },
        ))
    }

    fn new(inputs: Vec<Input>, first: u64, phase: Phase) -> Self {
        Self {
            inputs,
            first,
            phase,
            stage_retry: false,
            refused_stage: false,
            notice_sent: false,
        }
    }

    /// True when this move would stage a new population rather than finish a committed one.
    pub fn is_fresh(&self) -> bool {
        matches!(self.phase, Phase::Stage)
    }

    fn committed(&self, rows: &Rows) -> bool {
        rows.boundary_since(self.first)
            && self
                .inputs
                .iter()
                .all(|i| rows.row(i.key).is_some_and(|r| r.since_seq >= self.first))
    }

    pub fn advance(&mut self, lease: &mut LedgerLease, host: &mut impl Host) -> Outcome {
        match self.try_advance(lease, host) {
            Ok(outcome) => outcome,
            Err(_) => {
                // Judge the next step only from a fresh read of the durable ledger.
                lease.needs_reopen = true;
                if !self.notice_sent && host.notice(None, "tui_o:turn_transition_held").is_ok() {
                    self.notice_sent = true;
                }
                Outcome::Held
            }
        }
    }

    fn try_advance(
        &mut self,
        lease: &mut LedgerLease,
        host: &mut impl Host,
    ) -> io::Result<Outcome> {
        if let Phase::Finished(outcome) = self.phase {
            return Ok(outcome);
        }
        let ledger = lease.get()?;
        let rows = ledger.rows()?;
        if self.committed(&rows) && matches!(self.phase, Phase::Stage | Phase::Commit) {
            self.phase = Phase::Delete(0);
        }
        if self.refused_stage && matches!(self.phase, Phase::Stage) {
            host.notice(None, "tui_o:turn_mode_refused")?;
            self.phase = Phase::Finished(Outcome::Legacy);
            return Ok(Outcome::Legacy);
        }
        if matches!(self.phase, Phase::Stage) {
            let staged = rows.staged_since(self.first);
            for input in &mut self.inputs {
                if staged.contains(&input.key) {
                    continue;
                }
                let mut state = move_disposition(input.source, host.evidence(input)?);
                if state == super::rows::RowState::Running
                    && serde_json::from_value::<super::rows::AttemptEvidence>(
                        input.payload["move_attempt"].clone(),
                    )
                    .ok()
                    .is_none_or(|attempt| attempt.record_end.is_none())
                {
                    state = super::rows::RowState::Held(super::rows::HeldReason::Ambiguous);
                }
                if !state.is_terminal() {
                    host.pin_input(ledger, input)?;
                }
                if matches!(
                    state,
                    super::rows::RowState::Held(super::rows::HeldReason::Ambiguous)
                        | super::rows::RowState::Done(super::rows::DoneReason::AcceptedBeforeMove)
                ) {
                    host.notice(Some(input.key), "move_input_requires_attention")?;
                }
                if let Err(error) = ledger.append_entry(
                    &Entry::Staged {
                        key: input.key,
                        input: input.payload.clone(),
                        state,
                    },
                    &input.pins,
                ) {
                    self.refused_stage = self.stage_retry;
                    self.stage_retry = true;
                    return Err(error);
                }
            }
            self.phase = Phase::Commit;
            #[cfg(test)]
            if mutant("early_delete") {
                host.delete(DeletePhase::Dispatch)?;
            }
            let entry = Entry::MoveCommitted {
                first_staged_seq: self.first,
                ids: self.inputs.iter().map(|i| i.key).collect(),
            };
            match ledger.append_entry(&entry, &[]) {
                Ok(_) => self.phase = Phase::Delete(0),
                Err(error) => return Err(error),
            }
        } else if matches!(self.phase, Phase::Commit) {
            // The previous append was uncertain. A successful reopen proves absence.
            host.notice(None, "tui_o:turn_mode_refused")?;
            self.phase = Phase::Finished(Outcome::Legacy);
            return Ok(Outcome::Legacy);
        }
        while let Phase::Delete(index) = self.phase {
            #[cfg(test)]
            let index_for_effect = if mutant("delete_order") && index < 2 {
                1 - index
            } else {
                index
            };
            #[cfg(not(test))]
            let index_for_effect = index;
            host.delete(DELETIONS[index_for_effect])?;
            self.phase = if index == 3 {
                Phase::Actor
            } else {
                Phase::Delete(index + 1)
            };
        }
        if matches!(self.phase, Phase::Actor) {
            host.start_actor()?;
            self.phase = Phase::Finished(Outcome::Ledger);
        }
        Ok(Outcome::Ledger)
    }

    // Bounded work per boot; exhausted retries stay held for the next boot.
    // Test-only: advance does blocking IO, so production retries it from a blocking worker.
    #[cfg(test)]
    pub async fn retry(
        &mut self,
        lease: &mut LedgerLease,
        host: &mut impl Host,
        attempts: u32,
    ) -> Outcome {
        for attempt in 0..attempts.min(8) {
            let outcome = self.advance(lease, host);
            if outcome != Outcome::Held {
                return outcome;
            }
            if attempt + 1 < attempts.min(8) {
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
        Outcome::Held
    }
}

pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(
        5u64.saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX))
            .min(300),
    )
}

pub fn handback(lease: &mut LedgerLease, host: &mut impl Host) -> io::Result<Outcome> {
    let result = return_rows(lease, host);
    lease.needs_reopen |= result.is_err();
    result
}

fn return_rows(lease: &mut LedgerLease, host: &mut impl Host) -> io::Result<Outcome> {
    let ledger = lease.get()?;
    let rows = ledger.rows()?;
    if !rows.unbound().is_empty() {
        // No row backs an unbound key, so only a notice naming each one can explain the hold.
        for key in rows.unbound() {
            host.notice(Some(*key), "handback_unbound")?;
        }
        return Ok(Outcome::Held);
    }
    let mut open: Vec<_> = rows.open_rows().collect();
    if open.iter().any(|(_, row)| row.received_seq.is_none()) {
        host.notice(None, "handback_order_unavailable")?;
        return Ok(Outcome::Held);
    }
    open.sort_by_key(|(_, row)| row.received_seq);
    #[cfg(test)]
    if mutant("handback_order") {
        open.reverse();
    }
    let mut held = false;
    for (key, row) in open {
        let (accepted, composer) = host.reconcile(key, row)?;
        let closed = match handback_plan(row.state, accepted, composer) {
            Handback::Enqueue => {
                let outcome = host.enqueue(key, row).unwrap_or(EnqueueOutcome::Rejected);
                let closed = handback_after_enqueue(outcome);
                if closed.is_none() {
                    held = true;
                    host.notice(Some(key), "handback_enqueue_rejected")?;
                }
                closed
            }
            Handback::Close(state) => Some(state),
            Handback::NoticeThenClose(state) => {
                held = !state.is_terminal();
                host.notice(Some(key), "handback_ambiguous")?;
                Some(state)
            }
            Handback::Settled => None,
        };
        if let Some(state) = closed {
            ledger.append_entry(
                &Entry::Transition {
                    key,
                    state,
                    attempt: None,
                },
                &[],
            )?;
        }
        // Do not append later inputs ahead of a rejected earlier input on restart.
        if held {
            break;
        }
    }
    Ok(if held { Outcome::Held } else { Outcome::Legacy })
}

#[cfg(test)]
pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("ADK_TEST_INPUT_TRANSITION_MUTANT").is_ok_and(|value| value == name)
}
