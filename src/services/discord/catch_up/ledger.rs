//! Dormant history material and common intake classification; no scan or receipt effects.

mod classify;
mod fetch;

use poise::serenity_prelude::Message;

pub(super) use classify::{ClassifiedSource, classify_prepared, prepare};
pub(super) use fetch::fetch;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Incomplete {
    FetchFailed(String),
    CursorNotAdvancing,
    BudgetExhausted,
    UnknownLowerBound,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Coverage {
    Complete,
    Incomplete(Incomplete),
}

pub(super) struct LedgerFetchResult {
    pub messages: Vec<Message>,
    pub lower_exclusive: Option<u64>,
    pub horizon: Option<u64>,
    pub coverage: Coverage,
}

impl LedgerFetchResult {
    pub fn complete_fetch(&self) -> bool {
        self.coverage == Coverage::Complete
    }
}
