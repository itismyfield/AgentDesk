//! Seal and turn bookkeeping: a unit seals once and stays immutable, and Idle observations
//! close the open native turn of one source.

use std::collections::{BTreeSet, HashMap};

use super::UnitKey;
use super::identity::RecordFact;
use super::unit_plan::UnitPlan;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealOutcome {
    First,
    /// Same key and plan again, e.g. a forked source copying an ancestor block.
    Repeat,
    /// Same key with a different plan: the sealed unit would have changed.
    Conflict,
}

#[derive(Debug, Default)]
pub struct SealRegistry {
    sealed: HashMap<UnitKey, UnitPlan>,
    announced: BTreeSet<UnitKey>,
}

impl SealRegistry {
    pub fn announce(&mut self, key: UnitKey) {
        if !self.sealed.contains_key(&key) {
            self.announced.insert(key);
        }
    }

    pub fn seal(&mut self, key: &UnitKey, plan: &UnitPlan) -> SealOutcome {
        self.announced.remove(key);
        match self.sealed.get(key) {
            None => {
                self.sealed.insert(key.clone(), plan.clone());
                SealOutcome::First
            }
            Some(sealed) if sealed == plan => SealOutcome::Repeat,
            Some(_) => SealOutcome::Conflict,
        }
    }

    pub fn is_sealed(&self, key: &UnitKey) -> bool {
        self.sealed.contains_key(key)
    }

    pub fn knows(&self, key: &UnitKey) -> bool {
        self.sealed.contains_key(key) || self.announced.contains(key)
    }

    /// Announced units whose sealing record has not been captured yet.
    pub fn unsealed(&self) -> Vec<UnitKey> {
        self.announced.iter().cloned().collect()
    }
}

/// What one record did to the turn state of its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEvent {
    None,
    /// A turn opened under this native id.
    Opened(String),
    /// The open turn closed under this native id.
    Closed(String),
    /// Idle with no open turn, e.g. the second of consecutive E records.
    StrayIdle,
    /// Idle closing a turn whose opener lies before the capture start.
    EdgeTurn,
}

#[derive(Clone, Debug)]
enum TurnState {
    /// Capture began mid-file, so the running turn's opener was never seen.
    Unknown,
    Idle,
    /// An open turn and its native id when the opener carried one.
    Open(Option<String>),
}

/// Turn state of one source; turns never span sources.
#[derive(Clone, Debug)]
pub struct TurnTracker(TurnState);

impl TurnTracker {
    pub fn starting_at(first_offset: u64) -> Self {
        Self(match first_offset {
            0 => TurnState::Idle,
            _ => TurnState::Unknown,
        })
    }

    /// Applies one record fact; `row_key` is the record's own identity.
    pub fn observe(&mut self, fact: &RecordFact, row_key: Option<&String>) -> TurnEvent {
        let opened =
            |id: Option<&String>| id.map_or(TurnEvent::None, |id| TurnEvent::Opened(id.clone()));
        match (fact, &mut self.0) {
            // A prompt inside an open turn never reopens it.
            (RecordFact::Prompt(..), TurnState::Open(_)) => {}
            (RecordFact::Prompt(true, _), _) if row_key.is_some() => {
                self.0 = TurnState::Open(row_key.cloned());
                return opened(row_key);
            }
            (RecordFact::Assistant, TurnState::Idle) => {
                self.0 = TurnState::Open(row_key.cloned());
                return opened(row_key);
            }
            (RecordFact::TurnStart(turn_id), _) => {
                self.0 = TurnState::Open(turn_id.clone());
                return opened(turn_id.as_ref());
            }
            (RecordFact::Idle(closer_id), _) => {
                let open_id = match std::mem::replace(&mut self.0, TurnState::Idle) {
                    TurnState::Open(open_id) => open_id,
                    TurnState::Unknown => return TurnEvent::EdgeTurn,
                    TurnState::Idle => return TurnEvent::StrayIdle,
                };
                // A completion naming another turn means this turn's own end was missed.
                return match (open_id, closer_id) {
                    (Some(open), Some(close)) if open != *close => TurnEvent::None,
                    (Some(id), _) => TurnEvent::Closed(id),
                    (None, Some(close)) => TurnEvent::Closed(close.clone()),
                    (None, None) => TurnEvent::None,
                };
            }
            _ => {}
        }
        TurnEvent::None
    }
}
