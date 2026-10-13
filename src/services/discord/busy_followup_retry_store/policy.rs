//! Retry-budget latch codec and the Ready/Parked/Unavailable classifier shared by every queue
//! consumer.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serenity::model::id::MessageId;

use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

use super::{MAX_BUSY_RETRY_COUNT, MAX_BUSY_RETRY_ELAPSED, STORE_WRITE_LOCK};

/// Fields the latch format adds; a stored record carries all of them or none.
const LATCH_FIELDS: [&str; 3] = ["active_retry_ms", "capped", "notice_bound"];

fn notice_bound_by_default() -> bool {
    true
}

/// One input's retry budget: the legacy fields plus the summed cost of real attempts and a cap
/// latch. Legacy files carry none of the new fields; partial or unknown fields are refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) struct RetryBudget {
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
    /// The pane that refused the last retry to protect a person's draft, watched again after a
    /// restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_session: Option<String>,
}

impl RetryBudget {
    /// Spent by count, by real attempt cost, or by an earlier latch. Reads no clock.
    pub fn exhausted(&self) -> bool {
        self.capped
            || self.busy_retry_count >= MAX_BUSY_RETRY_COUNT
            || self.active_retry_ms >= MAX_BUSY_RETRY_ELAPSED.as_millis() as u64
    }

    /// Any retry spent so far; a bare notice binding has none.
    pub fn has_budget(&self) -> bool {
        self.busy_retry_count > 0 || self.active_retry_ms > 0 || self.capped
    }

    /// Carries another id's spending of the same logical input: the larger count and cost, and
    /// either latch.
    pub fn absorb(&mut self, other: &Self) {
        self.busy_retry_count = self.busy_retry_count.max(other.busy_retry_count);
        self.active_retry_ms = self.active_retry_ms.max(other.active_retry_ms);
        self.capped |= other.capped;
    }

    /// Charges one real pre-submit failure that took `attempt_ms`; the latch never clears here.
    /// `draft_session` names the pane that refused it to protect a person's draft.
    pub fn charge(
        prior: Option<Self>,
        notice_message_id: u64,
        attempt_ms: u64,
        now_ms: u64,
        draft_session: Option<&str>,
    ) -> Self {
        let mut budget = prior.unwrap_or(Self {
            notice_message_id,
            busy_retry_count: 0,
            first_busy_retry_at_ms: now_ms,
            draft_hold: false,
            active_retry_ms: 0,
            capped: false,
            notice_bound: true,
            draft_session: None,
        });
        if budget.first_busy_retry_at_ms == 0 {
            budget.first_busy_retry_at_ms = now_ms;
        }
        budget.busy_retry_count = budget.busy_retry_count.saturating_add(1);
        budget.active_retry_ms = budget.active_retry_ms.saturating_add(attempt_ms);
        budget.draft_hold = draft_session.is_some();
        budget.draft_session = draft_session.map(str::to_string);
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
            draft_session: None,
            ..self
        }
    }
}

/// Why an input's retry state cannot be trusted. Consumers branch on this, never on the detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableReason {
    RootUnavailable,
    ReadFailed,
    MalformedRecord,
    /// Some latch fields present, or one written as null.
    IncompleteRecord,
    InvalidNotice,
    /// The reads do not cover exactly the input's identity.
    IncompleteView,
}

/// A refused read: the coded reason and a detail kept for diagnostics only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct ReadRefusal {
    pub reason: UnavailableReason,
    pub detail: String,
}

impl ReadRefusal {
    fn new(reason: UnavailableReason, detail: impl ToString) -> Self {
        Self {
            reason,
            detail: detail.to_string(),
        }
    }
}

/// Restores a budget from stored bytes without guessing: truncated, partial, unknown or
/// incomplete records are refused, never read as an empty budget.
pub(in crate::services::discord) fn decode_checked(
    bytes: &[u8],
) -> Result<RetryBudget, ReadRefusal> {
    use UnavailableReason::{IncompleteRecord, InvalidNotice, MalformedRecord};
    let malformed = |error: serde_json::Error| ReadRefusal::new(MalformedRecord, error);
    let fields: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(bytes).map_err(malformed)?;
    let named = LATCH_FIELDS
        .iter()
        .filter(|name| fields.contains_key(**name));
    let written = named
        .clone()
        .filter(|name| !fields[**name].is_null())
        .count();
    if named.count() != written || (written != 0 && written != LATCH_FIELDS.len()) {
        let detail = format!("{written} of {} latch fields written", LATCH_FIELDS.len());
        return Err(ReadRefusal::new(IncompleteRecord, detail));
    }
    let budget: RetryBudget = serde_json::from_slice(bytes).map_err(malformed)?;
    if budget.notice_message_id == 0 {
        return Err(ReadRefusal::new(InvalidNotice, "notice_message_id is 0"));
    }
    Ok(budget)
}

/// What one source id's retry file says, with a missing file kept apart from an unreadable one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum SourceRead {
    Absent,
    Budget(RetryBudget),
    Unreadable(ReadRefusal),
}

pub(super) fn read_source_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
) -> SourceRead {
    let path = super::input_file_path_in_root(root, provider, channel_id, user_msg_id);
    match fs::read(path) {
        Ok(bytes) => decode_checked(&bytes).map_or_else(SourceRead::Unreadable, SourceRead::Budget),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SourceRead::Absent,
        Err(error) => {
            SourceRead::Unreadable(ReadRefusal::new(UnavailableReason::ReadFailed, error))
        }
    }
}

/// How the automatic paths may treat one queued input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryClass {
    /// Runnable; it also holds back inputs that arrived later.
    Ready,
    /// Budget spent: kept, never auto-submitted, and never holds back later inputs.
    Parked,
    /// State unknown: neither run nor skipped, and it holds back later inputs.
    Unavailable(UnavailableReason),
}

/// Unavailable over Parked over Ready, decided only after every read; the first refusal names
/// the reason.
fn classify_reads<'a>(reads: impl IntoIterator<Item = &'a SourceRead>) -> RetryClass {
    let mut class = RetryClass::Ready;
    for read in reads {
        match read {
            SourceRead::Unreadable(refusal) => return RetryClass::Unavailable(refusal.reason),
            SourceRead::Budget(budget) if budget.exhausted() => class = RetryClass::Parked,
            SourceRead::Budget(_) | SourceRead::Absent => {}
        }
    }
    class
}

/// The primary first, then each distinct source id.
fn identity_ids(primary: u64, sources: &[MessageId]) -> Vec<u64> {
    let mut ids = vec![primary];
    for id in sources.iter().map(|id| id.get()) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// One logical input's identity together with a checked read of every id it names; the only
/// way outside this module to classify an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct CheckedInputView {
    provider: ProviderKind,
    channel_id: u64,
    primary: u64,
    sources: Vec<MessageId>,
    reads: Vec<(u64, SourceRead)>,
}

impl CheckedInputView {
    /// Reads the primary and every distinct source id under one store lock.
    pub fn read(
        provider: &ProviderKind,
        channel_id: u64,
        primary: u64,
        sources: &[MessageId],
    ) -> Self {
        let root = runtime_store::discord_busy_followup_retries_root();
        Self::read_in(root.as_deref(), provider, channel_id, primary, sources)
    }

    fn read_in(
        root: Option<&Path>,
        provider: &ProviderKind,
        channel_id: u64,
        primary: u64,
        sources: &[MessageId],
    ) -> Self {
        let ids = identity_ids(primary, sources);
        let reads = match root {
            None => {
                let refusal = ReadRefusal::new(
                    UnavailableReason::RootUnavailable,
                    "runtime root unavailable",
                );
                ids.into_iter()
                    .map(|id| (id, SourceRead::Unreadable(refusal.clone())))
                    .collect()
            }
            Some(root) => {
                let _guard = STORE_WRITE_LOCK
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                ids.into_iter()
                    .map(|id| (id, read_source_in_root(root, provider, channel_id, id)))
                    .collect()
            }
        };
        Self::from_reads(provider, channel_id, primary, sources, reads)
    }

    /// Wraps reads a consumer already holds; `classify` refuses them unless they cover the
    /// input's identity exactly once each.
    pub fn from_reads(
        provider: &ProviderKind,
        channel_id: u64,
        primary: u64,
        sources: &[MessageId],
        reads: Vec<(u64, SourceRead)>,
    ) -> Self {
        Self {
            provider: provider.clone(),
            channel_id,
            primary,
            sources: sources.to_vec(),
            reads,
        }
    }

    /// A fresh read of the same identity, for re-checking at claim time.
    #[cfg(test)]
    pub fn reread(&self) -> Self {
        Self::read(&self.provider, self.channel_id, self.primary, &self.sources)
    }

    /// Each identity id with its read, naming which id holds a budget.
    pub fn reads(&self) -> &[(u64, SourceRead)] {
        &self.reads
    }

    fn covers_identity(&self) -> bool {
        let ids = identity_ids(self.primary, &self.sources);
        self.primary != 0
            && self.reads.len() == ids.len()
            && ids.iter().all(|id| {
                self.reads
                    .iter()
                    .filter(|(read_id, _)| read_id == id)
                    .count()
                    == 1
            })
    }

    pub fn classify(&self) -> RetryClass {
        if !self.covers_identity() {
            return RetryClass::Unavailable(UnavailableReason::IncompleteView);
        }
        classify_reads(self.reads.iter().map(|(_, read)| read))
    }
}

/// Automatic selection: parked inputs are passed over, the first ready one is taken, and an
/// unavailable one stops the walk so nothing behind it overtakes it.
pub(in crate::services::discord) fn auto_pick(classes: &[RetryClass]) -> Option<usize> {
    for (index, class) in classes.iter().enumerate() {
        match class {
            RetryClass::Parked => continue,
            RetryClass::Ready => return Some(index),
            RetryClass::Unavailable(_) => return None,
        }
    }
    None
}

/// Whether nothing but parked inputs sits in one queue layer.
pub(crate) fn parked_only(classes: &[RetryClass]) -> bool {
    classes.iter().all(|class| *class == RetryClass::Parked)
}

/// One queue layer as the injection check read it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LayerSnapshot<'a> {
    Classes(&'a [RetryClass]),
    Unreadable,
}

/// The backlog a later input would pass. Both layers are required; one left unchecked refuses.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BacklogLayers<'a> {
    pub memory: Option<LayerSnapshot<'a>>,
    pub disk: Option<LayerSnapshot<'a>>,
}

impl BacklogLayers<'_> {
    /// Only layers read as parked-only admit a later input past them.
    pub fn admits_later_input(&self) -> bool {
        [self.memory, self.disk].iter().all(
            |layer| matches!(layer, Some(LayerSnapshot::Classes(classes)) if parked_only(classes)),
        )
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
