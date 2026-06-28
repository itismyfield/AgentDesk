/// Eligible/rejection buckets for catch-up scans. These are logged separately so
/// "no recovery" is distinguishable from filter, dedupe, and age-window skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CatchUpClassification {
    /// Eligible user/allowed-bot message that should be enqueued.
    Recover,
    /// System message kind (thread-created / slash-command etc.) - silently dropped.
    SystemKind,
    /// Authored by this bot (self) - must not re-enqueue our own output.
    SelfAuthored,
    /// Already present in the live mailbox / known set - duplicate.
    Duplicate,
    /// Older than the catch-up max-age window - too late to safely replay.
    TooOld,
    /// Empty content (whitespace only).
    Empty,
    /// Authored by a non-allowed bot or an allowed bot without DISPATCH prefix.
    NotAllowed,
}

/// Per-channel running tally of [`CatchUpClassification`] outcomes - fed into
/// the always-on breakdown log. Keeping this separate from the recovery loop
/// keeps the filter-stats accounting honest and unit-testable.
#[derive(Debug, Default, Clone, Copy)]
pub(in crate::services::discord) struct CatchUpScanStats {
    pub returned: usize,
    pub recovered: usize,
    pub system_kind: usize,
    pub self_authored: usize,
    pub duplicate: usize,
    pub too_old: usize,
    pub empty: usize,
    pub not_allowed: usize,
}

impl CatchUpScanStats {
    pub(in crate::services::discord) fn record(&mut self, outcome: CatchUpClassification) {
        match outcome {
            CatchUpClassification::Recover => self.recovered += 1,
            CatchUpClassification::SystemKind => self.system_kind += 1,
            CatchUpClassification::SelfAuthored => self.self_authored += 1,
            CatchUpClassification::Duplicate => self.duplicate += 1,
            CatchUpClassification::TooOld => self.too_old += 1,
            CatchUpClassification::Empty => self.empty += 1,
            CatchUpClassification::NotAllowed => self.not_allowed += 1,
        }
    }
}
