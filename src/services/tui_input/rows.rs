//! Input-ledger row semantics: the record vocabulary and a pure fold that decides who owns each input.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::attempt::{
    self, AttemptMeta, Disposition, Keep, Retention, Retire, Seen, TOMBSTONE_HORIZON_MS, Tracking,
    Witness,
};
use super::blob::BlobPin;
use super::durable::invalid;
use super::ledger::{Ledger, Record, Snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneReason {
    Completed,
    AcceptedBeforeMove,
    HandbackRunning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldReason {
    Modal,
    NotReady,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbandonReason {
    UserClear,
    Handback,
    HandbackAmbiguous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum RowState {
    Received,
    Ready,
    Injecting,
    AwaitTurn,
    /// A provider queue holds the input; no model record confirms it yet.
    Queued,
    Running,
    Unaccepted,
    Held(HeldReason),
    Done(DoneReason),
    Abandoned(AbandonReason),
}

impl RowState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done(_) | Self::Abandoned(_))
    }

    // Only a handback written after a durable Legacy enqueue returns the input to Legacy.
    fn released_to_legacy(self) -> bool {
        matches!(self, Self::Abandoned(AbandonReason::Handback))
    }

    // Revert records close the binding window of every staging attempt before them.
    fn is_handback(self) -> bool {
        matches!(
            self,
            Self::Abandoned(AbandonReason::Handback | AbandonReason::HandbackAmbiguous)
                | Self::Done(DoneReason::HandbackRunning)
        )
    }
}

// Stored through the WAL `kind`/`payload` fields; keys are primary Discord message ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum Entry {
    Received {
        key: u64,
        input: Value,
    },
    // Inert until a MoveCommitted whose window covers this seq binds it.
    Staged {
        key: u64,
        input: Value,
        state: RowState,
    },
    MoveCommitted {
        first_staged_seq: u64,
        ids: Vec<u64>,
    },
    Transition {
        key: u64,
        state: RowState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<AttemptEvidence>,
    },
}

impl Entry {
    fn encode(&self) -> io::Result<(String, Value)> {
        let Value::Object(mut tagged) = serde_json::to_value(self)? else {
            return Err(invalid("input entry did not encode as an object"));
        };
        match (tagged.remove("kind"), tagged.remove("payload")) {
            (Some(Value::String(kind)), Some(payload)) => Ok((kind, payload)),
            _ => Err(invalid("input entry did not encode kind and payload")),
        }
    }

    fn decode(record: &Record) -> io::Result<Self> {
        serde_json::from_value(json!({ "kind": record.kind, "payload": record.payload }))
            .map_err(|_| invalid("unrecognized input ledger record"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Legacy,
    Ledger,
    Settled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttemptEvidence {
    pub binding: crate::services::tui_o::shadow::SourceBinding,
    pub execution_nonce: String,
    pub eof: u64,
    pub rendered_prompt: String,
    pub source_ids: Vec<u64>,
    pub record_end: Option<u64>,
    pub native_turn_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub since_seq: u64,
    // The source WAL sequence survives compaction; old snapshots lack ordering evidence.
    #[serde(default)]
    pub received_seq: Option<u64>,
    pub state: RowState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<AttemptEvidence>,
    pub input: Value,
    // Tracked generations and witnesses outlive the input text as the row's tombstone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<AttemptMeta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub witnesses: Vec<Seen>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dispositions: Vec<Disposition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
}

impl Row {
    /// Two distinct model records carried this input.
    pub fn duplicate_delivery(&self) -> bool {
        (self.witnesses.iter())
            .filter(|seen| seen.witness.kind.confirms_input())
            .count()
            > 1
    }

    /// A model record arrived after a user clear had already cut the input.
    pub fn delivered_after_clear(&self) -> bool {
        self.state == RowState::Abandoned(AbandonReason::UserClear)
            && (self.witnesses.iter()).any(|seen| seen.late && seen.witness.kind.confirms_input())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StagedRow {
    seq: u64,
    key: u64,
    input: Value,
    state: RowState,
}

// Fold of one channel's ledger; the same snapshot and records always yield the same value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rows {
    folded_seq: u64,
    rows: BTreeMap<u64, Row>,
    // Staged records since the last commit or handback; only they can still be bound.
    staged: Vec<StagedRow>,
    boundary_seq: u64,
    // Keys a commit claimed without a Staged record in its window; the ledger holds them.
    unbound: BTreeSet<u64>,
    ignored: u64,
}

impl Rows {
    pub fn fold(snapshot: Option<&Snapshot>, records: &[Record]) -> io::Result<Self> {
        let mut rows = match snapshot {
            Some(snapshot) => {
                let rows: Self = serde_json::from_value(snapshot.state.clone())?;
                if rows.folded_seq != snapshot.seq {
                    return Err(invalid("input snapshot state does not match its sequence"));
                }
                rows
            }
            None => Self::default(),
        };
        for record in records {
            rows.apply(record)?;
        }
        Ok(rows)
    }

    // Undecodable or out-of-order records fail the fold so a caller holds the channel.
    pub fn apply(&mut self, record: &Record) -> io::Result<()> {
        if self.folded_seq.checked_add(1) != Some(record.seq) {
            return Err(invalid("input ledger records are not contiguous"));
        }
        let seq = record.seq;
        match Entry::decode(record)? {
            Entry::Received { key, input } => {
                if self.can_activate(key) {
                    self.activate(key, seq, seq, input, RowState::Received);
                } else {
                    self.ignored += 1;
                }
            }
            Entry::Staged { key, input, state } => self.staged.push(StagedRow {
                seq,
                key,
                input,
                state,
            }),
            Entry::MoveCommitted {
                first_staged_seq,
                ids,
            } => self.commit(seq, first_staged_seq, &ids),
            Entry::Transition {
                key,
                state,
                attempt,
            } => match record.payload.get("tracking") {
                None => self.legacy_transition(seq, key, state, attempt),
                Some(tracking) => {
                    let tracking: Tracking = serde_json::from_value(tracking.clone())
                        .map_err(|_| invalid("unrecognized input ledger tracking"))?;
                    self.admit(key, state, attempt.as_ref(), &tracking)
                        .map_err(invalid)?;
                    self.track(seq, key, state, attempt, tracking);
                }
            },
        }
        self.folded_seq = seq;
        Ok(())
    }

    fn legacy_transition(
        &mut self,
        seq: u64,
        key: u64,
        state: RowState,
        attempt: Option<AttemptEvidence>,
    ) {
        let writable = self
            .rows
            .get(&key)
            .is_some_and(|row| !row.state.is_terminal());
        self.transition(seq, key, state);
        if writable
            && let Some(attempt) = attempt
            && let Some(row) = self.rows.get_mut(&key)
            && row.state == state
        {
            row.attempt = Some(attempt);
        }
    }

    // The writer and the fold apply the same check, so a violating record never lands silently.
    fn admit(
        &self,
        key: u64,
        state: RowState,
        evidence: Option<&AttemptEvidence>,
        tracking: &Tracking,
    ) -> Result<(), &'static str> {
        let row = self.rows.get(&key).ok_or("tracking names an unknown row")?;
        match (&tracking.attempt, &tracking.witness, tracking.disposition) {
            (Some(meta), None, None) => {
                let tokens = self.rows.values().flat_map(|row| &row.attempts);
                if tokens.into_iter().any(|other| other.token == meta.token) {
                    return Err("attempt token is not unique");
                }
                attempt::admits_attempt(row, state, evidence, meta)
            }
            (None, Some(witness), None) if evidence.is_none() => {
                attempt::admits_witness(row, state, witness)
            }
            // A close reason never stands in for a model record: it may only settle or hold a row.
            (None, None, Some(_)) if row.state.is_terminal() && state != row.state => {
                Err("a settled row keeps its state")
            }
            (None, None, Some(_)) if row.state.is_terminal() => Ok(()),
            (None, None, Some(_))
                if state.is_terminal()
                    || matches!(state, RowState::Unaccepted | RowState::Held(_)) =>
            {
                Ok(())
            }
            _ => Err("tracking must carry exactly one consistent fact"),
        }
    }

    fn track(
        &mut self,
        seq: u64,
        key: u64,
        state: RowState,
        evidence: Option<AttemptEvidence>,
        tracking: Tracking,
    ) {
        let Some(row) = self.rows.get_mut(&key) else {
            return;
        };
        if let Some(witness) = tracking.witness {
            let late = row.state.is_terminal();
            match (row.witnesses.iter_mut()).find(|seen| seen.witness.same_record(&witness)) {
                Some(seen) if seen.witness.turn_ref.is_none() => {
                    seen.witness.turn_ref = witness.turn_ref;
                }
                Some(_) => {}
                None => row.witnesses.push(Seen { witness, late }),
            }
            if !late {
                attempt::settle_effect(row, state);
                row.state = state;
            }
            return;
        }
        if let Some(meta) = tracking.attempt {
            row.attempts.push(meta);
        }
        if let Some(disposition) = tracking.disposition
            && !row.dispositions.contains(&disposition)
        {
            row.dispositions.push(disposition);
        }
        self.legacy_transition(seq, key, state, evidence);
    }

    fn commit(&mut self, seq: u64, first: u64, ids: &[u64]) {
        let staged = std::mem::take(&mut self.staged);
        let mut bound = BTreeMap::new();
        // A window reaching back past another commit or handback is malformed and binds nothing.
        if first > self.boundary_seq {
            for row in staged.into_iter().filter(|r| (first..seq).contains(&r.seq)) {
                bound.insert(row.key, row);
            }
        }
        for &key in ids {
            match bound.remove(&key) {
                Some(row) if self.can_activate(key) => {
                    self.activate(key, seq, row.seq, row.input, row.state);
                }
                Some(_) => self.ignored += 1,
                None if self.owner(key) == Owner::Legacy => {
                    self.unbound.insert(key);
                }
                None => {}
            }
        }
        self.boundary_seq = seq;
    }

    fn transition(&mut self, seq: u64, key: u64, state: RowState) {
        if state.is_handback() {
            self.staged.clear();
            self.boundary_seq = seq;
        }
        match self.rows.get_mut(&key) {
            Some(row) if !row.state.is_terminal() => {
                attempt::settle_effect(row, state);
                row.state = state;
            }
            _ => self.ignored += 1,
        }
    }

    // Settled keys stay as dedup history; only a key handed back to Legacy can return.
    fn can_activate(&self, key: u64) -> bool {
        !self.keeps_tracked_history(key)
            && (self.rows.get(&key)).is_none_or(|row| row.state.released_to_legacy())
    }

    /// A key handed back after a tracked attempt; activating it again would erase its tombstone.
    pub fn keeps_tracked_history(&self, key: u64) -> bool {
        (self.rows.get(&key))
            .is_some_and(|row| row.state.released_to_legacy() && !row.attempts.is_empty())
    }

    fn activate(&mut self, key: u64, seq: u64, received_seq: u64, input: Value, state: RowState) {
        self.unbound.remove(&key);
        self.rows.insert(
            key,
            Row {
                since_seq: seq,
                received_seq: {
                    #[cfg(test)]
                    if super::transition::mutant("drop_order") {
                        None
                    } else {
                        Some(received_seq)
                    }
                    #[cfg(not(test))]
                    {
                        Some(received_seq)
                    }
                },
                state,
                attempt: serde_json::from_value(input["move_attempt"].clone()).ok(),
                input,
                attempts: Vec::new(),
                witnesses: Vec::new(),
                dispositions: Vec::new(),
                retention: None,
            },
        );
    }

    pub fn owner(&self, key: u64) -> Owner {
        if self.unbound.contains(&key) {
            return Owner::Ledger;
        }
        match self.rows.get(&key) {
            None => Owner::Legacy,
            Some(row) if !row.state.is_terminal() => Owner::Ledger,
            Some(row) if row.state.released_to_legacy() => Owner::Legacy,
            Some(_) => Owner::Settled,
        }
    }

    pub fn row(&self, key: u64) -> Option<&Row> {
        self.rows.get(&key)
    }

    /// Every tracked attempt with its row key and whether that row is still open.
    pub fn attempts(&self) -> impl Iterator<Item = (u64, bool, &AttemptMeta)> {
        (self.rows.iter()).flat_map(|(key, row)| {
            let open = !row.state.is_terminal();
            row.attempts.iter().map(move |meta| (*key, open, meta))
        })
    }

    pub fn open_rows(&self) -> impl Iterator<Item = (u64, &Row)> {
        self.rows
            .iter()
            .filter(|(_, row)| !row.state.is_terminal())
            .map(|(key, row)| (*key, row))
    }

    // A terminal row may have been compacted, but still names its commit's accessories.
    pub fn boundary_keys(&self) -> impl Iterator<Item = u64> + '_ {
        self.rows
            .iter()
            .filter_map(|(key, row)| (row.since_seq == self.boundary_seq).then_some(*key))
    }

    pub fn unbound(&self) -> &BTreeSet<u64> {
        &self.unbound
    }

    // The channel stays in ledger mode while any committed input is unsettled.
    pub fn ledger_owned(&self) -> bool {
        !self.unbound.is_empty() || self.open_rows().next().is_some()
    }

    // Keys an unfinished attempt starting at `first_seq` has already staged.
    pub fn staged_since(&self, first_seq: u64) -> BTreeSet<u64> {
        self.staged
            .iter()
            .filter(|row| row.seq >= first_seq)
            .map(|row| row.key)
            .collect()
    }

    // A commit or handback closes this staging window; it is not an execution authority.
    pub fn boundary_since(&self, first_seq: u64) -> bool {
        self.boundary_seq >= first_seq
    }

    pub fn folded_seq(&self) -> u64 {
        self.folded_seq
    }

    pub fn ignored(&self) -> u64 {
        self.ignored
    }

    // Settled rows drop their input but keep attempt detail; open rows and Staged keep everything.
    pub fn compact(&self) -> io::Result<Value> {
        self.compact_with(&Keep, 0)
    }

    /// Drops a settled row's attempt detail only once two checkpoints found it collectable.
    pub fn compact_with(&self, facts: &impl Retire, generation: u64) -> io::Result<Value> {
        let mut compacted = self.clone();
        let shared: Vec<_> = (self.open_rows())
            .flat_map(|(_, row)| {
                row.witnesses
                    .iter()
                    .filter_map(|seen| seen.witness.range.as_ref())
            })
            .collect();
        for (key, row) in compacted.rows.iter_mut() {
            if !row.state.is_terminal() {
                continue;
            }
            row.input = Value::Null;
            let Some(now) = facts.now_ms().filter(|_| !row.attempts.is_empty()) else {
                continue;
            };
            let seen = row.retention.as_ref().map_or(now, |r| r.settled_seen_ms);
            let collectable = now
                .checked_sub(seen)
                .is_some_and(|age| age >= TOMBSTONE_HORIZON_MS)
                && !facts.obligated(*key)
                && row.attempts.iter().all(|attempt| facts.retired(attempt))
                && !(row.witnesses.iter())
                    .any(|seen| (seen.witness.range.as_ref()).is_some_and(|r| shared.contains(&r)));
            let retention = row.retention.get_or_insert(Retention {
                settled_seen_ms: now,
                collectable_since: None,
            });
            match retention.collectable_since {
                _ if !collectable => retention.collectable_since = None,
                None => retention.collectable_since = Some(generation),
                Some(marked) if marked < generation => {
                    row.attempts.clear();
                    row.witnesses.clear();
                    row.attempt = None;
                }
                Some(_) => {}
            }
        }
        Ok(serde_json::to_value(compacted)?)
    }
}

impl Ledger {
    pub fn append_entry(&mut self, entry: &Entry, pins: &[BlobPin]) -> io::Result<u64> {
        let (kind, payload) = entry.encode()?;
        Ok(self.append(&kind, payload, pins)?.seq)
    }

    pub fn rows(&self) -> io::Result<Rows> {
        Rows::fold(self.snapshot(), self.records())
    }

    pub fn checkpoint_rows(&mut self) -> io::Result<()> {
        self.checkpoint_rows_with(&Keep)
    }

    pub fn checkpoint_rows_with(&mut self, facts: &impl Retire) -> io::Result<()> {
        let generation = (self.snapshot().map_or(0, |s| s.generation))
            .checked_add(1)
            .ok_or_else(|| invalid("generation exhausted"))?;
        let state = self.rows()?.compact_with(facts, generation)?;
        self.checkpoint(state)
    }

    /// Appends a Transition carrying `tracking` only if the current fold admits it.
    pub fn append_tracked(
        &mut self,
        key: u64,
        state: RowState,
        attempt: Option<AttemptEvidence>,
        tracking: &Tracking,
    ) -> io::Result<u64> {
        (self.rows()?)
            .admit(key, state, attempt.as_ref(), tracking)
            .map_err(|reason| io::Error::new(io::ErrorKind::InvalidInput, reason))?;
        let (kind, mut payload) = Entry::Transition {
            key,
            state,
            attempt,
        }
        .encode()?;
        payload["tracking"] = serde_json::to_value(tracking)?;
        Ok(self.append(&kind, payload, &[])?.seq)
    }

    /// Records a witness once with the state it implies. A re-read appends nothing unless it
    /// names the turn the recorded copy lacked; that record keeps the row's state.
    pub fn append_witness(&mut self, key: u64, witness: Witness) -> io::Result<Option<u64>> {
        let rows = self.rows()?;
        let row = rows
            .row(key)
            .ok_or_else(|| invalid("witness names an unknown row"))?;
        let recorded = (row.witnesses.iter()).find(|seen| seen.witness.same_record(&witness));
        if recorded
            .is_some_and(|seen| seen.witness.turn_ref.is_some() || witness.turn_ref.is_none())
        {
            return Ok(None);
        }
        let state = match recorded {
            Some(_) => row.state,
            None => witness.kind.next_state(row.state),
        };
        let tracking = Tracking {
            witness: Some(witness),
            ..Tracking::default()
        };
        self.append_tracked(key, state, None, &tracking).map(Some)
    }
}
