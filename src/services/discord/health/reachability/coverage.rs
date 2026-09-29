//! Coverage telemetry from the same sweep that supplies the reachability verdict.

use serde::Serialize;

use super::ledger::ReachabilityLedger;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(in crate::services::discord) struct CoverageReport {
    pub schema_version: u32,
    pub observation_state: &'static str,
    pub uncovered_ranges: Option<u32>,
    pub unproven_ranges: Option<u32>,
    pub pending_ranges: Option<u32>,
    pub oldest_uncovered_age_secs: Option<u64>,
    pub oldest_unproven_age_secs: Option<u64>,
    pub oldest_pending_age_secs: Option<u64>,
    pub age_basis: &'static str,
    pub cursor_offset: Option<u64>,
    pub observed_eof: Option<u64>,
    pub observation_committed_at_epoch_ms: Option<u64>,
    pub provenance: Option<CoverageProvenanceCounts>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(in crate::services::discord) struct CoverageProvenanceCounts {
    pub exact_receipt_ranges: u32,
    pub frontier_prefix_ranges: u32,
    pub mixed_ranges: u32,
}

impl CoverageReport {
    pub(super) fn new(ledger: Option<&ReachabilityLedger>, committed_at: Option<u64>) -> Self {
        Self {
            schema_version: 1,
            observation_state: "unresolved",
            uncovered_ranges: None,
            unproven_ranges: None,
            pending_ranges: None,
            oldest_uncovered_age_secs: None,
            oldest_unproven_age_secs: None,
            oldest_pending_age_secs: None,
            age_basis: "first_observed",
            cursor_offset: ledger.map(|ledger| ledger.cursor_offset),
            observed_eof: ledger.map(|ledger| ledger.last_observed_len),
            observation_committed_at_epoch_ms: committed_at,
            provenance: None,
        }
    }

    pub(super) fn record(
        &mut self,
        uncovered: &[u64],
        unproven: &[u64],
        provenance: CoverageProvenanceCounts,
    ) {
        self.uncovered_ranges = Some(uncovered.len() as u32);
        self.unproven_ranges = Some(unproven.len() as u32);
        self.pending_ranges = Some((uncovered.len() + unproven.len()) as u32);
        self.oldest_uncovered_age_secs = uncovered.iter().copied().max();
        self.oldest_unproven_age_secs = unproven.iter().copied().max();
        self.oldest_pending_age_secs = self
            .oldest_uncovered_age_secs
            .max(self.oldest_unproven_age_secs);
        self.provenance = Some(provenance);
    }
}
