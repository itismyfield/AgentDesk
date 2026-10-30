//! Durable input evidence and the finite legacy cohort; none of these entries prepare a POST.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::services::tui_o::shadow::{ShadowProvider, SourceId, SourceRange, UnitKey, UnitKind};
use crate::services::tui_o::writer::input_notices::NoticeOrigin;

type RecordId = (SourceId, u64, u64);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeEvidence {
    pub record: SourceRange,
    pub raw_record: Vec<u8>,
    pub prompt: Option<String>,
    pub prompt_sha256: Option<String>,
    pub unit_key: Option<UnitKey>,
    pub payload: Option<String>,
    pub origin: Option<Box<NoticeOrigin>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationSource {
    pub source: SourceId,
    pub lower: u64,
    pub upper: u64,
    pub prefix_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MigrationHistory {
    /// The lower id is exclusive; the upper id is inclusive, including an empty range.
    Fixed {
        lower: u64,
        upper: u64,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationSnapshot {
    pub sources: Vec<MigrationSource>,
    pub binding_checkpoint: u64,
    pub history: MigrationHistory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoticeEntry {
    Materialized {
        evidence: Box<NoticeEvidence>,
    },
    MigrationStarted {
        snapshot: MigrationSnapshot,
    },
    LegacyImported {
        input_key: UnitKey,
        msg_id: u64,
    },
    MigrationUnresolved {
        record: SourceRange,
        reason: String,
        candidates: Vec<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationDebt {
    pub reason: String,
    pub candidates: Vec<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoticeState {
    pub migration: Option<MigrationSnapshot>,
    pub frozen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub evidence: HashMap<RecordId, NoticeEvidence>,
    pub unresolved: HashMap<RecordId, MigrationDebt>,
    pub imported: BTreeMap<UnitKey, u64>,
    pub reserved_ids: BTreeSet<u64>,
}

fn record_id(record: &SourceRange) -> RecordId {
    (record.source.clone(), record.start, record.end)
}

fn merge<T: PartialEq>(kept: &mut Option<T>, next: Option<T>) -> Result<(), String> {
    if kept
        .as_ref()
        .zip(next.as_ref())
        .is_some_and(|(a, b)| a != b)
    {
        return Err("input notice materialized evidence changed".into());
    }
    if kept.is_none() {
        *kept = next;
    }
    Ok(())
}

impl NoticeEvidence {
    fn validate(&self) -> Result<(), String> {
        if self.record.start >= self.record.end
            || self.record.end - self.record.start != self.raw_record.len() as u64 + 1
            || self.raw_record.is_empty()
        {
            return Err("input notice has no complete record evidence".into());
        }
        if let Some(digest) = &self.prompt_sha256 {
            let expected = self
                .prompt
                .as_ref()
                .map(|prompt| hex::encode(Sha256::digest(prompt.as_bytes())));
            if expected.as_ref() != Some(digest) {
                return Err("input notice prompt digest mismatch".into());
            }
        }
        if let Some(key) = &self.unit_key
            && (key.kind != UnitKind::InputNotice
                || key.provider != ShadowProvider::Claude
                || key.native_key.trim().is_empty())
        {
            return Err("input notice has an invalid native key".into());
        }
        if self.prompt.is_some() || self.unit_key.is_some() {
            let raw = serde_json::from_slice(&self.raw_record)
                .map_err(|_| "input notice classification record is invalid")?;
            let (prompt, uuid) = crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(&raw)
                .ok_or("input notice classification evidence is not a native input")?;
            if self.prompt.as_ref().is_some_and(|kept| kept != &prompt)
                || self
                    .unit_key
                    .as_ref()
                    .is_some_and(|key| Some(&key.native_key) != uuid.as_ref())
            {
                return Err("input notice prompt or native key mismatches its raw record".into());
            }
        }
        if let Some(origin) = &self.origin
            && (origin.record != self.record
                || origin.logical_session.trim().is_empty()
                || origin.binding_seq == 0
                || origin.execution_nonce.trim().is_empty())
        {
            return Err("input notice origin does not match its record".into());
        }
        Ok(())
    }

    fn canonical_digest(&self) -> Result<Option<&str>, String> {
        self.validate()?;
        if self.unit_key.is_none() {
            return Ok(None);
        }
        match (&self.prompt, self.prompt_sha256.as_deref()) {
            (Some(_), Some(digest)) => Ok(Some(digest)),
            _ => Err("input notice native key has no canonical prompt digest".into()),
        }
    }

    fn complete(&self) -> bool {
        self.prompt.is_some()
            && self.prompt_sha256.is_some()
            && self.unit_key.is_some()
            && self.payload.is_some()
            && self.origin.is_some()
    }
}

impl MigrationSnapshot {
    fn contains(&self, record: &SourceRange) -> bool {
        self.sources.iter().any(|source| {
            source.source == record.source
                && source.lower <= record.start
                && record.start < source.upper
        })
    }

    fn contains_message(&self, msg_id: u64) -> bool {
        matches!(&self.history, MigrationHistory::Fixed { lower, upper } if *lower < msg_id && msg_id <= *upper)
    }
}

impl NoticeState {
    pub(super) fn apply(&mut self, entry: NoticeEntry) -> Result<Option<UnitKey>, String> {
        match entry {
            NoticeEntry::MigrationStarted { snapshot } => {
                let mut sources = HashSet::new();
                if snapshot.sources.iter().any(|source| {
                    source.lower > source.upper
                        || !sources.insert(source.source.clone())
                        || source.prefix_hash.len() != 64
                        || !source
                            .prefix_hash
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                }) || match &snapshot.history {
                    MigrationHistory::Fixed { lower, upper } => lower > upper,
                    MigrationHistory::Unavailable { reason } => reason.trim().is_empty(),
                } {
                    return Err("invalid input notice migration snapshot".into());
                }
                if self
                    .migration
                    .as_ref()
                    .is_some_and(|kept| kept != &snapshot)
                {
                    return Err("input notice migration snapshot changed".into());
                }
                self.migration.get_or_insert(snapshot);
            }
            NoticeEntry::Materialized { evidence } => {
                evidence.validate()?;
                let id = record_id(&evidence.record);
                let mut next = *evidence;
                if let Some(kept) = self.evidence.get(&id) {
                    if kept.raw_record != next.raw_record {
                        return Err("input notice source record changed".into());
                    }
                    let mut merged = kept.clone();
                    merge(&mut merged.prompt, next.prompt)?;
                    merge(&mut merged.prompt_sha256, next.prompt_sha256)?;
                    merge(&mut merged.unit_key, next.unit_key)?;
                    merge(&mut merged.payload, next.payload)?;
                    merge(&mut merged.origin, next.origin)?;
                    next = merged;
                }
                let digest = next.canonical_digest()?;
                for kept in self
                    .evidence
                    .values()
                    .filter(|kept| kept.unit_key.is_some() && kept.unit_key == next.unit_key)
                {
                    if kept.canonical_digest()? != digest {
                        return Err("input notice UUID changed its content".into());
                    }
                }
                self.evidence.insert(id, next);
            }
            NoticeEntry::LegacyImported { input_key, msg_id } => {
                let mut digest = None;
                for evidence in self
                    .evidence
                    .values()
                    .filter(|evidence| evidence.unit_key.as_ref() == Some(&input_key))
                {
                    let canonical = evidence.canonical_digest()?;
                    if digest.is_some() && digest != canonical {
                        return Err("input notice UUID changed its content".into());
                    }
                    digest = canonical;
                }
                if self.imported.get(&input_key) == Some(&msg_id) {
                    return Ok(Some(input_key));
                }
                let known = self.evidence.iter().any(|(id, evidence)| {
                    evidence.unit_key.as_ref() == Some(&input_key)
                        && evidence.complete()
                        && !self.unresolved.contains_key(id)
                        && self.migration.as_ref().is_some_and(|snapshot| {
                            snapshot.contains(&evidence.record) && snapshot.contains_message(msg_id)
                        })
                });
                if !known
                    || self.imported.contains_key(&input_key)
                    || self.reserved_ids.contains(&msg_id)
                    || self
                        .unresolved
                        .keys()
                        .any(|id| self.evidence[id].unit_key.as_ref() == Some(&input_key))
                {
                    return Err("input notice legacy receipt is not uniquely attributable".into());
                }
                self.reserved_ids.insert(msg_id);
                self.imported.insert(input_key.clone(), msg_id);
                return Ok(Some(input_key));
            }
            NoticeEntry::MigrationUnresolved {
                record,
                reason,
                candidates,
            } => {
                let id = record_id(&record);
                if reason.trim().is_empty()
                    || candidates.contains(&0)
                    || !self.evidence.contains_key(&id)
                    || !self
                        .migration
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.contains(&record))
                    || self.evidence[&id]
                        .unit_key
                        .as_ref()
                        .is_some_and(|key| self.imported.contains_key(key))
                {
                    return Err("invalid unresolved input notice migration".into());
                }
                let debt = MigrationDebt { reason, candidates };
                if self.unresolved.get(&id).is_some_and(|kept| kept != &debt) {
                    return Err("input notice migration result changed".into());
                }
                self.reserved_ids.extend(&debt.candidates);
                self.unresolved.entry(id).or_insert(debt);
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
