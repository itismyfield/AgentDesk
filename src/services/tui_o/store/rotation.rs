//! Source rotation state: the consumed binding seq and each bound source's start boundary.
//! Both files are replaced atomically; only resolution or explicit provisional refinement changes a decided boundary.

use std::collections::BTreeMap;
use std::fs::OpenOptions;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ledger::{self, LedgerEntry};
use super::spool::source_key;
use super::{ChannelStore, LEDGER_FILE, OStore, StoreError, damage, durable};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingRecord, BindingTarget,
};

pub const CHECKPOINT_FILE: &str = "binding_checkpoint";
pub const BOUNDARY_FILE: &str = "boundary";
pub(crate) const FRESH_SELECTION_FILE: &str = "fresh_selection";
pub(crate) const TAKEOVER_END_FILE: &str = "takeover_end";

/// The end of the execution a late takeover took; its source and nonce tie it to that init.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TakeoverEnd {
    pub(crate) source: SourceId,
    pub(crate) nonce: String,
    pub(crate) basis: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FreshSelection {
    schema: u32,
    channel: u64,
    pub(crate) nonce: String,
    pub(crate) keys: Vec<String>,
}

impl ChannelStore {
    pub(crate) fn fresh_selection(&self) -> Result<Option<FreshSelection>, StoreError> {
        let read: Option<FreshSelection> =
            durable::read_json(&self.dir.join(FRESH_SELECTION_FILE))?;
        if let Some(record) = &read {
            let valid = record.schema == 1
                && record.channel == self.init.channel
                && !record.nonce.trim().is_empty()
                && record.keys.iter().all(|key| {
                    key.len() == 32
                        && key
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
                && record.keys.windows(2).all(|keys| keys[0] < keys[1]);
            if !valid {
                return Err(damage("fresh selection identity or keys are invalid"));
            }
        }
        Ok(read)
    }

    pub(crate) fn write_fresh_selection(
        &mut self,
        nonce: String,
        mut keys: Vec<String>,
    ) -> Result<(), StoreError> {
        keys.sort();
        keys.dedup();
        let record = FreshSelection {
            schema: 1,
            channel: self.init.channel,
            nonce,
            keys,
        };
        let bytes = serde_json::to_vec(&record)?;
        self.mutate(|store| {
            Ok(durable::replace(
                &store.dir.join(FRESH_SELECTION_FILE),
                &bytes,
            )?)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    /// Still inside the inherited prefix; nothing from the source is owed yet.
    Undecided,
    /// Records starting at or after `from` are owed.
    Owed { from: u64 },
    /// The source spools but posts nothing until its delivery start is known.
    Pending { candidates: Vec<u64> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLink {
    pub source: SourceId,
    pub seq: u64,
    /// The verified parent; rows whose identities its consumed bytes hold are not owed again.
    pub parent: Option<SourceId>,
    pub committed_at: DateTime<Utc>,
    pub boundary: Boundary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_candidate: Option<HotCandidate>,
}

/// Fixed at first hot attachment; an old Pending never gains this eligibility on restore.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotCandidate {
    pub channel: u64,
    pub pane: String,
    pub nonce: String,
    pub parent: SourceId,
}

/// The binding and captured snapshots that justified an automatic start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryEvidence {
    pub candidate: HotCandidate,
    pub seq: u64,
    pub through: u64,
    pub prefix_hash: String,
    pub parent_through: u64,
    pub parent_hash: String,
}

/// The source an old one was rotated to and what that hop measured. The extra fields sit beside
/// the source's own, so a reader that knows only `SourceId` reads the same identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Successor {
    #[serde(flatten)]
    pub source: SourceId,
    /// Seq of the bind that made the hop; absent in records written before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session: Option<String>,
    /// The old source's length when the hop was applied; the successor waits until it is spooled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_to: Option<u64>,
    /// Seq of the provider record showing the old source's session was left, or of the hop's own
    /// bind when a confirmed clear end shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<u64>,
}

impl From<SourceId> for Successor {
    /// The shape of a record written before the hop's fields were kept.
    fn from(source: SourceId) -> Self {
        Self {
            source,
            seq: None,
            tmux_session: None,
            drain_to: None,
            proof: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rotation {
    /// Sources a bind attached, by spool key.
    pub links: BTreeMap<String, SourceLink>,
    /// The source each old one was rotated to; binding a source again clears its entry.
    pub successors: BTreeMap<String, Successor>,
}

impl Rotation {
    pub fn link(&self, source: &SourceId) -> Option<&SourceLink> {
        self.links.get(&source_key(source))
    }
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    channel: u64,
    seq: u64,
}

impl ChannelStore {
    pub fn binding_checkpoint(&self) -> Result<Option<u64>, StoreError> {
        let read: Option<Checkpoint> = durable::read_json(&self.dir.join(CHECKPOINT_FILE))?;
        match read {
            Some(checkpoint) if checkpoint.channel != self.init.channel => {
                Err(damage("binding checkpoint names another channel"))
            }
            read => Ok(read.map(|checkpoint| checkpoint.seq)),
        }
    }

    /// Callers move it only after the cursor and boundary of that bind are durable.
    pub fn set_binding_checkpoint(&mut self, seq: u64) -> Result<(), StoreError> {
        let channel = self.init.channel;
        let bytes = serde_json::to_vec(&Checkpoint { channel, seq })?;
        self.mutate(|store| Ok(durable::replace(&store.dir.join(CHECKPOINT_FILE), &bytes)?))
    }

    pub fn rotation(&self) -> Result<Rotation, StoreError> {
        let read = durable::read_json(&self.dir.join(BOUNDARY_FILE))?;
        Ok(read.unwrap_or_default())
    }

    /// Refuses to drop a link or change a boundary that is no longer `Undecided`.
    pub fn write_rotation(&mut self, next: &Rotation) -> Result<(), StoreError> {
        let current = self.rotation()?;
        for (key, link) in &current.links {
            let kept = next.links.get(key).is_some_and(|next| {
                let decided = link.boundary != Boundary::Undecided;
                next.source == link.source && !(decided && next.boundary != link.boundary)
            });
            if !kept {
                return Err(StoreError::Rejected(format!(
                    "boundary of {key} is decided"
                )));
            }
        }
        let bytes = serde_json::to_vec(next)?;
        self.mutate(|store| Ok(durable::replace(&store.dir.join(BOUNDARY_FILE), &bytes)?))
    }

    /// Refines only a later explicit re-pin of this exact provisional Unknown attachment.
    pub(crate) fn refine_provisional_hot_link(
        &mut self,
        expected: &SourceLink,
        event: &BindingEvent,
    ) -> Result<Option<Rotation>, StoreError> {
        let BindingRecord::Bound {
            new: BindingTarget::Source(source),
            cause,
            parent_hint,
            ..
        } = &event.record
        else {
            return Ok(None);
        };
        let Some(candidate) = &expected.hot_candidate else {
            return Ok(None);
        };
        if !matches!(expected.boundary, Boundary::Pending { .. })
            || *cause == BindingCause::Unknown
            || event.seq <= expected.seq
            || event.channel_id != self.init.channel
            || event.channel_id != candidate.channel
            || event.provider != ShadowProvider::Claude
            || event.tmux_session != candidate.pane
            || event.execution_nonce != candidate.nonce
            || *source != expected.source
            || candidate.parent == *source
            || candidate.pane.is_empty()
            || candidate.nonce.is_empty()
            || self.cursor(&candidate.parent).is_none()
        {
            return Ok(None);
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(self.dir.join(LEDGER_FILE))?;
        let mut file = durable::LockedFile::try_lock(file)?;
        if ledger::resolution(&mut file, source)?.is_some() {
            return Err(StoreError::Rejected(
                "boundary already resolved; reopen the writer".into(),
            ));
        }
        let mut rotation = self.rotation()?;
        let Some(link) = rotation.links.get_mut(&source_key(source)) else {
            return Ok(None);
        };
        if link != expected {
            return Ok(None);
        }
        let parent = match cause {
            BindingCause::Fork | BindingCause::Continuation | BindingCause::Compact => {
                Some(candidate.parent.clone())
            }
            BindingCause::Resume => parent_hint
                .clone()
                .filter(|p| p != source && self.cursor(p).is_some()),
            _ => None,
        };
        link.boundary = match cause {
            BindingCause::Clear | BindingCause::Startup => Boundary::Owed { from: 0 },
            _ if parent.is_some() => Boundary::Undecided,
            _ => expected.boundary.clone(),
        };
        link.parent = parent;
        link.hot_candidate = None;
        link.seq = event.seq;
        link.committed_at = event.committed_at;
        let bytes = serde_json::to_vec(&rotation)?;
        self.mutate(|store| Ok(durable::replace(&store.dir.join(BOUNDARY_FILE), &bytes)?))?;
        Ok(Some(rotation))
    }

    /// Only a source whose owed start is known may lose spool segments.
    pub(super) fn gc_allowed(&self, source: &SourceId) -> Result<bool, StoreError> {
        let rotation = self.rotation()?;
        let boundary = rotation.link(source).map(|link| &link.boundary);
        Ok(matches!(boundary, None | Some(Boundary::Owed { .. })))
    }

    /// Moves each pending boundary to the start its durable `BoundaryResolved` names.
    pub fn apply_resolved_boundaries(&mut self) -> Result<Vec<SourceId>, StoreError> {
        self.apply_resolved_boundaries_filtered(|_| true)
    }

    pub(crate) fn apply_resolved_boundaries_filtered(
        &mut self,
        mut selected: impl FnMut(&SourceId) -> bool,
    ) -> Result<Vec<SourceId>, StoreError> {
        let mut rotation = self.rotation()?;
        let mut applied = Vec::new();
        for link in rotation.links.values_mut() {
            if !selected(&link.source) {
                continue;
            }
            let from = self.ledger.boundary_resolved(&link.source);
            if let (Boundary::Pending { .. }, Some(from)) = (&link.boundary, from) {
                if let Some(evidence) = self.ledger.boundary_evidence(&link.source) {
                    if link.hot_candidate.as_ref() != Some(&evidence.candidate)
                        || link.seq != evidence.seq
                        || evidence.candidate.channel != self.init.channel
                        || evidence.candidate.parent == link.source
                        || from >= evidence.through
                    {
                        return Err(damage("automatic boundary evidence does not match link"));
                    }
                    if !selected(&evidence.candidate.parent) {
                        return Err(StoreError::Rejected("boundary parent is suspended".into()));
                    }
                    link.parent = Some(evidence.candidate.parent.clone());
                } else {
                    link.hot_candidate = None;
                }
                link.boundary = Boundary::Owed { from };
                applied.push(link.source.clone());
            }
        }
        if !applied.is_empty() {
            let bytes = serde_json::to_vec(&rotation)?;
            self.mutate(|store| Ok(durable::replace(&store.dir.join(BOUNDARY_FILE), &bytes)?))?;
        }
        Ok(applied)
    }

    /// Appends/fsyncs proof before applying parent and start together through the resolution path.
    pub(crate) fn resolve_automatic_boundary(
        &mut self,
        source: &SourceId,
        from: u64,
        evidence: BoundaryEvidence,
    ) -> Result<(), StoreError> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(self.dir.join(LEDGER_FILE))?;
        let mut file = durable::LockedFile::try_lock(file)?;
        if ledger::resolution(&mut file, source)?.is_some() {
            return Err(StoreError::Rejected(
                "boundary already resolved; reopen the writer".into(),
            ));
        }
        let rotation = self.rotation()?;
        let link = rotation
            .link(source)
            .ok_or_else(|| damage("missing pending link"))?;
        let snapshot = |source: &SourceId, through, hash: &str| {
            self.cursor(source)
                .is_some_and(|c| c.captured_through == through && c.prefix_hash == hash)
        };
        if !matches!(link.boundary, Boundary::Pending { .. })
            || link.seq != evidence.seq
            || link.hot_candidate.as_ref() != Some(&evidence.candidate)
            || evidence.candidate.channel != self.init.channel
            || evidence.candidate.parent == *source
            || from >= evidence.through
            || !snapshot(source, evidence.through, &evidence.prefix_hash)
            || !snapshot(
                &evidence.candidate.parent,
                evidence.parent_through,
                &evidence.parent_hash,
            )
            || self.ledger.boundary_resolved(source).is_some()
        {
            return Err(damage("stale automatic boundary proof"));
        }
        self.append_ledger(LedgerEntry::BoundaryResolved {
            source: source.clone(),
            from,
            excluded_range: 0..from,
            operator: "claude_content_continuity".into(),
            at: Utc::now(),
            evidence: Some(Box::new(evidence)),
        })?;
        self.apply_resolved_boundaries()?;
        Ok(())
    }

    /// Appends/fsyncs a start of 0 for a fresh Codex source's `Pending{[0]}` link bound at `seq`,
    /// then applies it through the resolution path.
    pub(crate) fn resolve_codex_fresh_boundary(
        &mut self,
        source: &SourceId,
        seq: u64,
    ) -> Result<(), StoreError> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(self.dir.join(LEDGER_FILE))?;
        let mut file = durable::LockedFile::try_lock(file)?;
        if ledger::resolution(&mut file, source)?.is_some() {
            return Err(StoreError::Rejected(
                "boundary already resolved; reopen the writer".into(),
            ));
        }
        let rotation = self.rotation()?;
        let link = rotation
            .link(source)
            .ok_or_else(|| damage("missing pending link"))?;
        if link.boundary
            != (Boundary::Pending {
                candidates: vec![0],
            })
            || link.seq != seq
            || link.parent.is_some()
            || link.hot_candidate.is_some()
            || self.ledger.boundary_resolved(source).is_some()
        {
            return Err(damage("stale Codex fresh boundary"));
        }
        self.append_ledger(LedgerEntry::BoundaryResolved {
            source: source.clone(),
            from: 0,
            excluded_range: 0..0,
            operator: "codex_fresh_launch".into(),
            at: Utc::now(),
            evidence: None,
        })?;
        self.apply_resolved_boundaries()?;
        Ok(())
    }
}

impl OStore {
    /// The channel's binding checkpoint read without opening the channel: nothing is swept or
    /// recovered, and a checkpoint naming another channel is damage.
    pub fn peek_binding_checkpoint(&self, channel: u64) -> Result<Option<u64>, StoreError> {
        let path = self.channel_dir(channel).join(CHECKPOINT_FILE);
        match durable::read_json::<Checkpoint>(&path)? {
            Some(checkpoint) if checkpoint.channel != channel => {
                Err(damage("binding checkpoint names another channel"))
            }
            read => Ok(read.map(|checkpoint| checkpoint.seq)),
        }
    }

    /// The channel's rotation record read without opening the channel.
    pub fn peek_rotation(&self, channel: u64) -> Result<Rotation, StoreError> {
        let read = durable::read_json(&self.channel_dir(channel).join(BOUNDARY_FILE))?;
        Ok(read.unwrap_or_default())
    }

    pub(crate) fn takeover_end(&self, channel: u64) -> Result<Option<TakeoverEnd>, StoreError> {
        durable::read_json(&self.channel_dir(channel).join(TAKEOVER_END_FILE))
    }

    /// Kept beside the init, so neither a later execution nor a restart loses it.
    pub(crate) fn keep_takeover_end(
        &self,
        channel: u64,
        end: &TakeoverEnd,
    ) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(end)?;
        let path = self.channel_dir(channel).join(TAKEOVER_END_FILE);
        Ok(durable::replace(&path, &bytes)?)
    }

    /// The channel's stored cursors, read without opening the channel; an unreadable cursor is an
    /// error, never an absent one.
    pub fn peek_cursors(&self, channel: u64) -> Result<Vec<super::spool::Cursor>, StoreError> {
        let dir = self.channel_dir(channel).join(super::CURSOR_DIR);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut cursors = Vec::new();
        for entry in entries {
            let entry = entry?.path();
            if entry.extension().is_some_and(|ext| ext == "json") {
                cursors.extend(durable::read_json::<super::spool::Cursor>(&entry)?);
            }
        }
        Ok(cursors)
    }
}

#[cfg(test)]
#[path = "rotation_tests.rs"]
mod tests;
