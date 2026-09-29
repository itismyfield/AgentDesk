//! Source rotation state: the consumed binding seq and each bound source's start boundary.
//! Both files are replaced atomically; a decided boundary is never rewritten.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::spool::source_key;
use super::{ChannelStore, StoreError, damage, durable};
use crate::services::tui_o::shadow::SourceId;

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
    /// The verified parent whose keys mask the inherited prefix.
    pub parent: Option<SourceId>,
    pub committed_at: DateTime<Utc>,
    pub boundary: Boundary,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rotation {
    /// Sources a bind attached, by spool key.
    pub links: BTreeMap<String, SourceLink>,
    /// The source each old one was rotated to; binding a source again clears its entry.
    pub successors: BTreeMap<String, SourceId>,
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
}

#[cfg(test)]
#[path = "rotation_tests.rs"]
mod tests;
