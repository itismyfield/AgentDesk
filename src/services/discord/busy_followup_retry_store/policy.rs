//! Retry-budget latch codec and the Ready/Parked/Unavailable classifier shared by every queue
//! consumer. Nothing in production reads or writes this format yet.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serenity::model::id::MessageId;

use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

use super::{MAX_BUSY_RETRY_COUNT, MAX_BUSY_RETRY_ELAPSED, STORE_WRITE_LOCK};

fn notice_bound_by_default() -> bool {
    true
}

/// One input's retry budget: the legacy fields plus the summed cost of real attempts and a cap
/// latch. Legacy files carry none of the new fields; unknown fields are refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetryBudget {
    pub notice_message_id: u64,
    pub busy_retry_count: u32,
    /// Diagnostic only; never an input to the cap.
    pub first_busy_retry_at_ms: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft_hold: bool,
    /// Summed duration of failed submission attempts; time spent queued is never added.
    #[serde(default)]
    pub active_retry_ms: u64,
    /// Set once the budget is spent; only an observed draft release clears it.
    #[serde(default)]
    pub capped: bool,
    /// Whether the notice card still belongs to this input. Legacy files restore as bound.
    #[serde(default = "notice_bound_by_default")]
    pub notice_bound: bool,
}

impl RetryBudget {
    /// Spent by count, by real attempt cost, or by an earlier latch. Reads no clock.
    pub fn exhausted(&self) -> bool {
        self.capped
            || self.busy_retry_count >= MAX_BUSY_RETRY_COUNT
            || self.active_retry_ms >= MAX_BUSY_RETRY_ELAPSED.as_millis() as u64
    }

    /// Charges one real pre-submit failure that took `attempt_ms`; the latch never clears here.
    pub fn charge(
        prior: Option<Self>,
        notice_message_id: u64,
        attempt_ms: u64,
        now_ms: u64,
        draft_hold: bool,
    ) -> Self {
        let mut budget = prior.unwrap_or(Self {
            notice_message_id,
            busy_retry_count: 0,
            first_busy_retry_at_ms: now_ms,
            draft_hold,
            active_retry_ms: 0,
            capped: false,
            notice_bound: true,
        });
        if budget.first_busy_retry_at_ms == 0 {
            budget.first_busy_retry_at_ms = now_ms;
        }
        budget.busy_retry_count = budget.busy_retry_count.saturating_add(1);
        budget.active_retry_ms = budget.active_retry_ms.saturating_add(attempt_ms);
        budget.draft_hold = draft_hold;
        budget.capped = budget.exhausted();
        budget
    }

    /// The fresh budget an observed draft release grants; the notice binding is kept.
    pub fn released_from_draft(self) -> Self {
        Self {
            busy_retry_count: 0,
            first_busy_retry_at_ms: 0,
            draft_hold: false,
            active_retry_ms: 0,
            capped: false,
            ..self
        }
    }
}

/// Why stored bytes are not a budget this build can trust.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DecodeError {
    Malformed(String),
    ZeroNotice,
}

/// Restores a budget from stored bytes without guessing: truncated, unknown or incomplete
/// records are errors, never an empty budget.
pub(super) fn decode_checked(bytes: &[u8]) -> Result<RetryBudget, DecodeError> {
    let budget: RetryBudget =
        serde_json::from_slice(bytes).map_err(|error| DecodeError::Malformed(error.to_string()))?;
    if budget.notice_message_id == 0 {
        return Err(DecodeError::ZeroNotice);
    }
    Ok(budget)
}

/// What one source id's retry file says, with a missing file kept apart from an unreadable one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SourceRead {
    Absent,
    Budget(RetryBudget),
    Unreadable,
}

fn read_source_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> SourceRead {
    let path = super::input_file_path_in_root(root, provider, channel_id, user_msg_id);
    match fs::read(path) {
        Ok(bytes) => decode_checked(&bytes).map_or(SourceRead::Unreadable, SourceRead::Budget),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SourceRead::Absent,
        Err(_) => SourceRead::Unreadable,
    }
}

/// Reads the primary and every distinct source id of one logical input, naming which id holds
/// each budget. No runtime root means nothing can be confirmed.
pub(super) fn read_sources(
    provider: &ProviderKind,
    channel_id: u64,
    primary: u64,
    sources: &[MessageId],
) -> Vec<(u64, SourceRead)> {
    let mut ids = vec![primary];
    for id in sources.iter().map(|id| id.get()) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let Some(root) = runtime_store::discord_busy_followup_retries_root() else {
        return ids
            .into_iter()
            .map(|id| (id, SourceRead::Unreadable))
            .collect();
    };
    let _guard = STORE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    ids.into_iter()
        .map(|id| (id, read_source_in_root(&root, provider, channel_id, id)))
        .collect()
}

/// How the automatic paths may treat one queued input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetryClass {
    /// Runnable; it also holds back inputs that arrived later.
    Ready,
    /// Budget spent: kept, never auto-submitted, and never holds back later inputs.
    Parked,
    /// State unknown: neither run nor skipped, and it holds back later inputs.
    Unavailable,
}

/// One logical input: Unavailable over Parked over Ready, decided only after every source.
pub(super) fn classify_input<'a>(reads: impl IntoIterator<Item = &'a SourceRead>) -> RetryClass {
    let mut class = RetryClass::Ready;
    for read in reads {
        match read {
            SourceRead::Unreadable => return RetryClass::Unavailable,
            SourceRead::Budget(budget) if budget.exhausted() => class = RetryClass::Parked,
            SourceRead::Budget(_) | SourceRead::Absent => {}
        }
    }
    class
}

/// Automatic selection: parked inputs are passed over, the first ready one is taken, and an
/// unavailable one stops the walk so nothing behind it overtakes it.
pub(super) fn auto_pick(classes: &[RetryClass]) -> Option<usize> {
    for (index, class) in classes.iter().enumerate() {
        match class {
            RetryClass::Parked => continue,
            RetryClass::Ready => return Some(index),
            RetryClass::Unavailable => return None,
        }
    }
    None
}

/// Whether a later input may be injected past the queued backlog. Each layer (memory queue,
/// disk queue) is `None` when its snapshot could not be read; only parked-only layers admit.
pub(super) fn admits_later_input(layers: &[Option<&[RetryClass]>]) -> bool {
    layers.iter().all(|layer| {
        layer.is_some_and(|classes| classes.iter().all(|class| *class == RetryClass::Parked))
    })
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
