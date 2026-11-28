//! Per-channel journal of reply anchors fixed for native runs, made durable before any
//! `Prepared` names one, so a restart replies to the same message.

use std::fs::OpenOptions;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use super::ChannelStore;
use crate::domain::file_sync::fsync_parent_dir;
use crate::services::tui_o::shadow::SourceId;

const FILE: &str = "anchor_bindings.jsonl";

/// A run's fixed anchor and the joined inputs already marked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AnchorBinding {
    pub(crate) source: SourceId,
    pub(crate) run_start: u64,
    pub(crate) anchor: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) joined: Vec<u64>,
}

impl ChannelStore {
    /// Every readable complete line; a line torn by an interrupted append is no binding.
    pub(crate) fn anchor_bindings(&self) -> io::Result<Vec<AnchorBinding>> {
        let bytes = match std::fs::read(self.dir.join(FILE)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            other => other?,
        };
        let complete = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |end| end + 1);
        bytes[..complete]
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| Ok(serde_json::from_slice(line).ok()))
            .filter_map(Result::transpose)
            .collect()
    }

    pub(crate) fn record_anchor(&self, binding: &AnchorBinding) -> io::Result<()> {
        let path = self.dir.join(FILE);
        let created = !path.exists();
        let mut line = serde_json::to_vec(binding).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(&line)?;
        file.sync_data()?;
        if created {
            fsync_parent_dir(&path)?;
        }
        Ok(())
    }
}
