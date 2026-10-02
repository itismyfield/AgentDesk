//! Order filter for Herdr agent reports, held only in one observer's memory. Within one
//! (endpoint, pane, expected nonce) only a larger verified seq of the followed source is newer.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// The execution a report was read for; seqs of different scopes are never compared.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReportScope {
    pub endpoint: String,
    pub pane_id: String,
    pub execution_nonce: String,
}

/// `Verified` only when the schema is known to order this payload by the seq.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportSeq {
    Verified(u64),
    Unverified,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportOrder {
    Accepted,
    /// At or below the source's last accepted seq: a duplicate or a late report.
    Stale,
    /// The source's last accepted seq with another payload; the accepted one stays.
    Diverged,
    /// Not the source the scope followed; nothing is accepted until the pane is read again.
    SourceChanged,
    /// The scope lost its source and waits for [`ReportOrderFilter::refollow`] after a fresh read.
    AwaitingRecheck,
    /// No verified seq: a diagnostic, never ordered and never a reset.
    Unordered,
    /// Read on a connection older than the filter's.
    OldConnection,
}

struct Last {
    seq: u64,
    payload: u64,
}

/// Lives as long as its observer; a new connection generation clears it.
#[derive(Default)]
pub(crate) struct ReportOrderFilter {
    generation: u64,
    /// The source each scope follows; `None` after a switch until a fresh read names one.
    followed: HashMap<ReportScope, Option<String>>,
    /// Each source's last accepted report, kept across a switch so its late reports stay stale.
    last: HashMap<(ReportScope, String), Last>,
}

impl ReportOrderFilter {
    /// Whether `generation` is current; a newer one drops everything the older one ordered.
    fn on_connection(&mut self, generation: u64) -> bool {
        if generation > self.generation {
            self.generation = generation;
            self.followed.clear();
            self.last.clear();
        }
        generation == self.generation
    }

    /// One report read on connection `generation`; `payload` is what the seq orders.
    pub(crate) fn observe(
        &mut self,
        generation: u64,
        scope: &ReportScope,
        source: &str,
        seq: ReportSeq,
        payload: &impl Hash,
    ) -> ReportOrder {
        if !self.on_connection(generation) {
            return ReportOrder::OldConnection;
        }
        let ReportSeq::Verified(seq) = seq else {
            return ReportOrder::Unordered;
        };
        let mut hasher = DefaultHasher::new();
        payload.hash(&mut hasher);
        let payload = hasher.finish();
        let key = (scope.clone(), source.to_string());
        if let Some(last) = self.last.get(&key) {
            if seq < last.seq || (seq == last.seq && payload == last.payload) {
                return ReportOrder::Stale;
            }
            if seq == last.seq {
                return ReportOrder::Diverged;
            }
        }
        match self.followed.get(scope) {
            Some(None) => return ReportOrder::AwaitingRecheck,
            Some(Some(followed)) if followed != source => {
                self.followed.insert(scope.clone(), None);
                return ReportOrder::SourceChanged;
            }
            _ => {}
        }
        self.followed.insert(scope.clone(), Some(key.1.clone()));
        self.last.insert(key, Last { seq, payload });
        ReportOrder::Accepted
    }

    /// A fresh read of the pane named `source` as its reporter; an older connection's read
    /// changes nothing.
    pub(crate) fn refollow(&mut self, generation: u64, scope: &ReportScope, source: &str) -> bool {
        if !self.on_connection(generation) {
            return false;
        }
        self.followed
            .insert(scope.clone(), Some(source.to_string()));
        true
    }
}

#[cfg(test)]
#[path = "herdr_report_order_tests.rs"]
mod tests;
