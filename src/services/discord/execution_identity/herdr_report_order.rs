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
    /// At or below the last accepted seq: a duplicate or a late report.
    Stale,
    /// The last accepted seq with another payload; the accepted one stays.
    Diverged,
    /// Not the source the scope followed; the scope is dropped so the pane is read again.
    SourceChanged,
    /// No verified seq: a diagnostic, never ordered and never a reset.
    Unordered,
    /// Read on a connection older than the filter's.
    OldConnection,
}

struct Accepted {
    source: String,
    seq: u64,
    payload: u64,
}

/// Lives as long as its observer; a new connection generation clears it.
#[derive(Default)]
pub(crate) struct ReportOrderFilter {
    generation: u64,
    last: HashMap<ReportScope, Accepted>,
}

impl ReportOrderFilter {
    /// One report read on connection `generation`; `payload` is what the seq orders.
    pub(crate) fn observe(
        &mut self,
        generation: u64,
        scope: &ReportScope,
        source: &str,
        seq: ReportSeq,
        payload: &impl Hash,
    ) -> ReportOrder {
        if generation < self.generation {
            return ReportOrder::OldConnection;
        }
        if generation > self.generation {
            self.generation = generation;
            self.last.clear();
        }
        let ReportSeq::Verified(seq) = seq else {
            return ReportOrder::Unordered;
        };
        let mut hasher = DefaultHasher::new();
        payload.hash(&mut hasher);
        let payload = hasher.finish();
        let Some(last) = self.last.get_mut(scope) else {
            let source = source.to_string();
            let first = Accepted {
                source,
                seq,
                payload,
            };
            self.last.insert(scope.clone(), first);
            return ReportOrder::Accepted;
        };
        if last.source != source {
            self.last.remove(scope);
            return ReportOrder::SourceChanged;
        }
        if seq > last.seq {
            (last.seq, last.payload) = (seq, payload);
            return ReportOrder::Accepted;
        }
        if seq == last.seq && payload != last.payload {
            return ReportOrder::Diverged;
        }
        ReportOrder::Stale
    }
}

#[cfg(test)]
#[path = "herdr_report_order_tests.rs"]
mod tests;
