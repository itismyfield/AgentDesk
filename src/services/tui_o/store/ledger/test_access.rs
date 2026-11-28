//! Test-only entry points to the ledger's private replay, for tests outside this crate layer.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};

use super::{LedgerEntry, LedgerState, StoreError};
use crate::services::tui_o::shadow::UnitKey;

impl LedgerState {
    pub fn starting_at_for_test(initial_anchor: u64) -> Self {
        Self::starting_at(initial_anchor)
    }

    pub fn apply_for_test(&mut self, at: DateTime<Utc>, entry: LedgerEntry) {
        self.apply(at, entry);
    }

    pub fn excluded_for_test(&self) -> &BTreeMap<UnitKey, String> {
        &self.excluded
    }
}

pub fn append_for_test(
    path: &Path,
    at: DateTime<Utc>,
    entry: &LedgerEntry,
) -> Result<(), StoreError> {
    super::append(path, at, entry)
}

pub fn recover_for_test(path: &Path, initial_anchor: u64) -> Result<LedgerState, StoreError> {
    super::recover(path, initial_anchor)
}
