//! Attempt, witness and retention metadata a ledger Transition may carry; it adds no record kind.

use serde::{Deserialize, Serialize};

use super::rows::{AttemptEvidence, HeldReason, Row, RowState};
use crate::services::tui_o::shadow::{SourceId, SourceRange};

/// Settled attempt detail stays at least this long after a checkpoint first saw the row settled.
pub const TOMBSTONE_HORIZON_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// One generation's pane effect, derived from the state that follows Injecting.
/// `Intent` means the effect may or may not have started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    #[default]
    Intent,
    NotSent,
    Refused,
    Sent,
}

impl Effect {
    /// Positive evidence that nothing reached the pane.
    pub fn none(self) -> bool {
        matches!(self, Self::NotSent | Self::Refused)
    }
}

/// The pane and processes an attempt targeted, so a restart can prove the old ones are gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incarnation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// tmux server pid or socket identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_started_at: Option<String>,
}

/// Evidence that a queued generation's provider queue ended before the row is offered again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEnd {
    pub prior_generation: u32,
    pub old_nonce: String,
    pub new_nonce: String,
    /// Complete-prefix EOF when the old exit was confirmed; a same-file resume writes after it.
    pub old_source: SourceId,
    pub old_end: u64,
    pub stable_reads: u8,
    pub settle_profile: String,
}

/// One generation of a row's submission; its token names the frame and every witness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptMeta {
    pub generation: u32,
    pub token: String,
    pub frame_digest: String,
    /// Provider normalization (line endings, tabs) the digest was taken under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_profile: Option<String>,
    pub execution_nonce: String,
    pub source: SourceId,
    pub anchor: u64,
    #[serde(default)]
    pub effect: Effect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<Incarnation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_end: Option<QueueEnd>,
}

/// Evidence for a registered frame: Q and H are queue acceptance, U and A model input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WitnessKind {
    Queued,
    Hook,
    User,
    Attachment,
    Removed,
    Tool,
}

impl WitnessKind {
    pub fn confirms_input(self) -> bool {
        matches!(self, Self::User | Self::Attachment)
    }

    pub fn queue_only(self) -> bool {
        matches!(self, Self::Queued | Self::Hook)
    }

    /// A witness never revives a settled row or takes a confirmed one back to Queued.
    pub fn next_state(self, current: RowState) -> RowState {
        if current.is_terminal() {
            return current;
        }
        match self {
            Self::User | Self::Attachment => RowState::Running,
            Self::Queued | Self::Hook if current != RowState::Running => RowState::Queued,
            _ => current,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    pub generation: u32,
    pub token: String,
    pub kind: WitnessKind,
    /// The transcript record; a hook witness has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<SourceRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_ref: Option<String>,
}

impl Witness {
    /// Re-reading one record is one witness; its turn may be learned later.
    pub fn same_record(&self, other: &Self) -> bool {
        (&self.token, self.kind, &self.range, &self.record_key)
            == (&other.token, other.kind, &other.range, &other.record_key)
    }
}

/// A recorded witness; `late` marks one that arrived after the row settled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub witness: Witness,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub late: bool,
}

/// Close reasons a writer knows beyond the row state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Interrupted,
    Cancelled,
    Consumed,
    ReturnedToComposer,
}

/// Optional metadata on a Transition payload; legacy writers omit it and old readers ignore it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tracking {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<AttemptMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witness: Option<Witness>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<Disposition>,
}

/// Snapshot-only bookkeeping a checkpoint keeps for a settled row with attempt detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    /// Wall clock at the first checkpoint that saw the row settled.
    pub settled_seen_ms: u64,
    /// Checkpoint generation that first recorded the row as collectable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collectable_since: Option<u64>,
}

/// Facts a checkpoint needs before it drops settled attempt detail; the supervisor supplies them.
pub trait Retire {
    /// Wall-clock milliseconds since the epoch; None when the clock is not trusted.
    fn now_ms(&self) -> Option<u64>;
    /// The attempt's process positively exited and its source left the binding lineage.
    fn retired(&self, attempt: &AttemptMeta) -> bool;
    /// A clear, cancel, handback, re-offer or late-delivery watch still names the row.
    fn obligated(&self, key: u64) -> bool;
}

/// Keeps every tombstone; the policy until a supervisor supplies retirement facts.
pub struct Keep;

impl Retire for Keep {
    fn now_ms(&self) -> Option<u64> {
        None
    }
    fn retired(&self, _: &AttemptMeta) -> bool {
        false
    }
    fn obligated(&self, _: u64) -> bool {
        true
    }
}

/// A random 128-bit token in lowercase hex.
pub fn fresh_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `meta` may open the row's next generation; the ledger checks token uniqueness.
pub(super) fn admits_attempt(
    row: &Row,
    state: RowState,
    evidence: Option<&AttemptEvidence>,
    meta: &AttemptMeta,
) -> Result<(), &'static str> {
    if row.state.is_terminal() || state != RowState::Injecting {
        return Err("an attempt opens only an unsettled row's Injecting");
    }
    let evidence = evidence.ok_or("an attempt needs its submission evidence")?;
    if (
        &evidence.execution_nonce,
        evidence.eof,
        &evidence.binding.source,
    ) != (&meta.execution_nonce, meta.anchor, &meta.source)
    {
        return Err("attempt metadata disagrees with its evidence");
    }
    if !hex(&meta.token, 32) || !hex(&meta.frame_digest, 64) || meta.effect != Effect::Intent {
        return Err("malformed attempt token, digest or effect");
    }
    let prior = row.attempts.last();
    if meta.generation != prior.map_or(1, |prior| prior.generation.saturating_add(1)) {
        return Err("attempt generation is not the next one");
    }
    match (prior, &meta.queue_end) {
        (None, None) if matches!(row.state, RowState::Received | RowState::Ready) => Ok(()),
        (Some(prior), None) if prior.effect.none() && row.state == RowState::Ready => Ok(()),
        (Some(prior), Some(end)) if queue_ended(row, prior, meta, end) => Ok(()),
        _ => Err("this attempt may not offer the row again"),
    }
}

// Only a durable Q for the prior generation, a new incarnation and no recorded delivery admit it.
// The old exit's EOF is on the prior source, past its anchor and every prior-generation record.
fn queue_ended(row: &Row, prior: &AttemptMeta, meta: &AttemptMeta, end: &QueueEnd) -> bool {
    let before_end = |seen: &Seen| {
        (seen.witness.range.as_ref())
            .is_none_or(|range| range.source == end.old_source && range.end <= end.old_end)
    };
    row.state == RowState::Queued
        && end.prior_generation == prior.generation
        && end.old_nonce == prior.execution_nonce
        && end.new_nonce == meta.execution_nonce
        && end.old_nonce != end.new_nonce
        && end.old_source == prior.source
        && end.old_end >= prior.anchor
        && (row.witnesses.iter())
            .filter(|seen| seen.witness.generation == prior.generation)
            .all(before_end)
        && row.witnesses.iter().any(|seen| {
            seen.witness.generation == prior.generation && seen.witness.kind == WitnessKind::Queued
        })
        && !row.witnesses.iter().any(|seen| {
            seen.witness.kind.confirms_input() || seen.witness.kind == WitnessKind::Tool
        })
}

/// Whether a witness may be recorded with `state`; an unknown token never names the row.
pub(super) fn admits_witness(
    row: &Row,
    state: RowState,
    witness: &Witness,
) -> Result<(), &'static str> {
    let attempt = (row.attempts.iter())
        .find(|attempt| attempt.token == witness.token)
        .ok_or("witness token names no attempt of this row")?;
    if attempt.generation != witness.generation {
        return Err("witness generation disagrees with its token");
    }
    let recorded = (row.witnesses.iter()).any(|seen| seen.witness.same_record(witness));
    let expected = if recorded {
        row.state
    } else {
        witness.kind.next_state(row.state)
    };
    if state != expected {
        return Err("witness and state disagree");
    }
    Ok(())
}

/// Records the current generation's effect when the row first leaves Injecting.
pub(super) fn settle_effect(row: &mut Row, next: RowState) {
    if row.state != RowState::Injecting || next == RowState::Injecting {
        return;
    }
    // An untracked Injecting replaces the evidence; then no tracked generation takes the outcome.
    let Some(current) = row.attempt.as_ref() else {
        return;
    };
    if let Some(meta) = row.attempts.last_mut().filter(|meta| {
        meta.effect == Effect::Intent
            && (&meta.execution_nonce, meta.anchor) == (&current.execution_nonce, current.eof)
    }) {
        meta.effect = match next {
            RowState::Ready => Effect::NotSent,
            RowState::Held(HeldReason::NotReady) => Effect::Refused,
            RowState::AwaitTurn | RowState::Running | RowState::Queued => Effect::Sent,
            _ => Effect::Intent,
        };
    }
}
