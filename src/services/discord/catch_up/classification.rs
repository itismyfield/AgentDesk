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

/// Plain inputs to the catch-up filter, decoupled from `serenity::Message` so
/// the classification order can be tested without a Discord runtime.
#[derive(Debug, Clone)]
pub(in crate::services::discord) struct CatchUpMessageView {
    pub message_id: u64,
    pub author_id: u64,
    pub author_is_bot: bool,
    pub is_processable_kind: bool,
    pub age_secs: i64,
    pub trimmed_text: String,
}

/// Pure phase-1 filter. Sender eligibility, including the output-only notify
/// identity, is decided before the age gate so non-input automation cannot
/// become recovery work or TooOld/DLQ evidence.
pub(in crate::services::discord) fn classify_catch_up_message(
    msg: &CatchUpMessageView,
    bot_user_id: Option<u64>,
    existing_ids: &std::collections::HashSet<u64>,
    max_age_secs: i64,
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    notify_bot_id: Option<u64>,
) -> CatchUpClassification {
    if !msg.is_processable_kind {
        return CatchUpClassification::SystemKind;
    }
    if Some(msg.author_id) == bot_user_id {
        return CatchUpClassification::SelfAuthored;
    }
    if existing_ids.contains(&msg.message_id) {
        return CatchUpClassification::Duplicate;
    }
    if super::is_restart_gap_notice(msg.author_is_bot, &msg.trimmed_text) {
        return CatchUpClassification::SelfAuthored;
    }
    if msg.trimmed_text.is_empty() {
        return CatchUpClassification::Empty;
    }
    if Some(msg.author_id) == notify_bot_id {
        return CatchUpClassification::NotAllowed;
    }
    if !super::is_allowed_turn_sender(
        allowed_bot_ids,
        announce_bot_id,
        msg.author_id,
        msg.author_is_bot,
        &msg.trimmed_text,
    ) {
        return CatchUpClassification::NotAllowed;
    }
    if msg.age_secs > max_age_secs {
        return CatchUpClassification::TooOld;
    }
    CatchUpClassification::Recover
}
