//! Seal and Idle bookkeeping: a unit seals once and stays immutable, and Idle observations
//! close native turns for measurement only.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};

use super::UnitKey;
use super::identity::RecordFact;
use super::unit_plan::UnitPlan;

const SYNTHETIC_TOKEN_PREFIX: &str = "[o-shadow-synth:";

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

/// A turn's span within one source; `native_turn_id` is always set once closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnSpan {
    pub native_turn_id: Option<String>,
    pub start: u64,
    pub end: u64,
    pub opened_at: DateTime<Utc>,
    pub unit_keys: Vec<UnitKey>,
    pub autonomous: bool,
    pub synthetic_tokens: Vec<String>,
}

/// What one record did to the turn state of its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEvent {
    None,
    /// A turn opened under this native id.
    Opened(String),
    Closed(TurnSpan),
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
    Open(TurnSpan),
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
    pub fn observe(
        &mut self,
        fact: &RecordFact,
        row_key: Option<&String>,
        range: (u64, u64),
        at: DateTime<Utc>,
    ) -> TurnEvent {
        let open = |native_turn_id: Option<&String>, autonomous, synthetic_tokens| {
            TurnState::Open(TurnSpan {
                native_turn_id: native_turn_id.cloned(),
                start: range.0,
                end: range.1,
                opened_at: at,
                unit_keys: Vec::new(),
                autonomous,
                synthetic_tokens,
            })
        };
        let opened =
            |id: Option<&String>| id.map_or(TurnEvent::None, |id| TurnEvent::Opened(id.clone()));
        match (fact, &mut self.0) {
            (RecordFact::Prompt(_, text), TurnState::Open(turn)) => {
                let tokens = synthetic_tokens(text).into_iter();
                let tokens: Vec<String> = tokens
                    .filter(|token| !turn.synthetic_tokens.contains(token))
                    .collect();
                turn.synthetic_tokens.extend(tokens);
            }
            (RecordFact::Prompt(true, text), _) if row_key.is_some() => {
                self.0 = open(row_key, false, synthetic_tokens(text));
                return opened(row_key);
            }
            (RecordFact::Assistant, TurnState::Idle) => {
                self.0 = open(row_key, true, Vec::new());
                return opened(row_key);
            }
            (RecordFact::TurnStart(turn_id), _) => {
                self.0 = open(turn_id.as_ref(), false, Vec::new());
                return opened(turn_id.as_ref());
            }
            (RecordFact::Idle(closer_id), _) => {
                let mut turn = match std::mem::replace(&mut self.0, TurnState::Idle) {
                    TurnState::Open(turn) => turn,
                    TurnState::Unknown => return TurnEvent::EdgeTurn,
                    TurnState::Idle => return TurnEvent::StrayIdle,
                };
                // A completion naming another turn means this turn's own end was missed.
                match (&turn.native_turn_id, closer_id) {
                    (Some(open), Some(close)) if open != close => return TurnEvent::None,
                    (None, None) => return TurnEvent::None,
                    (None, close) => turn.native_turn_id = close.clone(),
                    (Some(_), _) => {}
                }
                turn.end = range.1;
                return TurnEvent::Closed(turn);
            }
            _ => {}
        }
        TurnEvent::None
    }

    pub fn add_unit(&mut self, key: &UnitKey) {
        if let TurnState::Open(turn) = &mut self.0
            && !turn.unit_keys.contains(key)
        {
            turn.unit_keys.push(key.clone());
        }
    }
}

/// Exact `[o-shadow-synth:<entry_id>]` tokens in native user input, in order, without repeats.
pub fn synthetic_tokens(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(SYNTHETIC_TOKEN_PREFIX) {
        rest = &rest[at + SYNTHETIC_TOKEN_PREFIX.len()..];
        let id = &rest[..rest.find(']').unwrap_or(0)];
        let token = format!("{SYNTHETIC_TOKEN_PREFIX}{id}]");
        if !id.is_empty() && !id.contains(char::is_whitespace) && !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    tokens
}
