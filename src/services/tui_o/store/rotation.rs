//! Source rotation state: the consumed binding seq and each bound source's start boundary.
//! Both files are replaced atomically; a decided boundary changes only by `BoundaryResolved`.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ledger::{self, LedgerEntry};
use super::spool::source_key;
use super::{ChannelStore, LEDGER_FILE, OStore, StoreError, damage, durable};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::file_identity;

pub const CHECKPOINT_FILE: &str = "binding_checkpoint";
pub const BOUNDARY_FILE: &str = "boundary";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    /// Still inside the inherited prefix; nothing from the source is owed yet.
    Undecided,
    /// Records starting at or after `from` are owed.
    Owed { from: u64 },
    /// The source spools but posts nothing until an operator picks the start.
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
    /// Seq of the provider record showing the old source's session was left.
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

    /// Only a source whose owed start is known may lose spool segments.
    pub(super) fn gc_allowed(&self, source: &SourceId) -> Result<bool, StoreError> {
        let rotation = self.rotation()?;
        let boundary = rotation.link(source).map(|link| &link.boundary);
        Ok(matches!(boundary, None | Some(Boundary::Owed { .. })))
    }

    /// Moves each pending boundary to the start its durable `BoundaryResolved` names.
    pub fn apply_resolved_boundaries(&mut self) -> Result<Vec<SourceId>, StoreError> {
        let mut rotation = self.rotation()?;
        let mut applied = Vec::new();
        for link in rotation.links.values_mut() {
            let from = self.ledger.boundary_resolved(&link.source);
            if let (Boundary::Pending { .. }, Some(from)) = (&link.boundary, from) {
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
}

/// Where an operator's resolved boundary starts: a byte offset, or the record with this uuid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveFrom {
    Offset(u64),
    Uuid(String),
}

fn rejected(detail: String) -> StoreError {
    StoreError::Rejected(detail)
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

    /// Appends an operator's start for a pending source to the ledger and fsyncs it, without
    /// opening the channel: nothing is swept or rewritten, and the writer applies it on start.
    pub fn record_boundary_resolved(
        &self,
        channel: u64,
        source: &str,
        from: &ResolveFrom,
        operator: &str,
    ) -> Result<(SourceId, u64), StoreError> {
        self.record_boundary_resolved_with_append(
            channel,
            source,
            from,
            operator,
            ledger::append_to,
        )
    }

    pub(super) fn record_boundary_resolved_with_append(
        &self,
        channel: u64,
        source: &str,
        from: &ResolveFrom,
        operator: &str,
        append: impl FnOnce(&mut File, DateTime<Utc>, &LedgerEntry) -> Result<(), StoreError>,
    ) -> Result<(SourceId, u64), StoreError> {
        let path = self.channel_dir(channel).join(LEDGER_FILE);
        let mut file = OpenOptions::new().read(true).append(true).open(&path)?;
        // Hold the existing ledger lock across validation and the durable operator append.
        file.try_lock().map_err(io::Error::from)?;
        if self.read_init(channel)?.is_none() {
            return Err(rejected(format!("channel {channel} has no O store")));
        }
        let dir = self.channel_dir(channel);
        let rotation: Rotation = durable::read_json(&dir.join(BOUNDARY_FILE))?.unwrap_or_default();
        let mut named = rotation
            .links
            .iter()
            .filter(|(key, link)| key.as_str() == source || link.source.path == Path::new(source));
        let link = match (named.next(), named.next()) {
            (Some((_, link)), None) => link,
            (None, _) => {
                return Err(rejected(format!(
                    "channel {channel} has no source {source}"
                )));
            }
            _ => return Err(rejected(format!("{source} names more than one source"))),
        };
        if !matches!(link.boundary, Boundary::Pending { .. }) {
            return Err(rejected(format!("the boundary of {source} is not pending")));
        }
        if let Some(recorded) = ledger::resolution(&mut file, &link.source)? {
            let detail = format!("{source} is already resolved at {recorded}; restart the writer");
            return Err(rejected(detail));
        }
        let from = record_start(&link.source, from)?;
        let (operator, at) = (operator.to_string(), Utc::now());
        let entry = LedgerEntry::BoundaryResolved {
            source: link.source.clone(),
            from,
            excluded_range: 0..from,
            operator,
            at,
        };
        append(&mut file, at, &entry)?;
        Ok((link.source.clone(), from))
    }
}

/// The byte offset `from` names in the source file, which must be the start of a record.
fn record_start(source: &SourceId, from: &ResolveFrom) -> Result<u64, StoreError> {
    let file = File::open(&source.path)?;
    if file_identity(&file.metadata()?) != (source.dev, source.ino) {
        return Err(rejected("the source file was replaced".into()));
    }
    let (mut reader, mut line) = (BufReader::new(file), Vec::new());
    let (mut start, mut found) = (0u64, Vec::new());
    loop {
        if *from == ResolveFrom::Offset(start) {
            return Ok(start);
        }
        line.clear();
        let read = reader.read_until(b'\n', &mut line)? as u64;
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        if let ResolveFrom::Uuid(uuid) = from {
            let value: Option<serde_json::Value> = serde_json::from_slice(&line).ok();
            if value.is_some_and(|value| value["uuid"].as_str() == Some(uuid)) {
                found.push(start);
            }
        }
        start += read;
    }
    match (from, found.as_slice()) {
        (ResolveFrom::Uuid(_), [start]) => Ok(*start),
        (ResolveFrom::Uuid(uuid), []) => Err(rejected(format!("no record has uuid {uuid}"))),
        (ResolveFrom::Uuid(uuid), _) => Err(rejected(format!("uuid {uuid} is not unique"))),
        (ResolveFrom::Offset(offset), _) => {
            Err(rejected(format!("byte {offset} is not a record start")))
        }
    }
}

#[cfg(test)]
#[path = "rotation_tests.rs"]
mod tests;
