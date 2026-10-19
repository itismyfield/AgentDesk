//! Clear ends: a `!clear` that confirmed the end of a Claude TUI pane's execution, written by
//! the clear and only read by O, which may prove that pane's next hop with it.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::durable;
use super::{ChannelStore, STORE_DIR_NAME};
use crate::services::tui_o::shadow::SourceId;

pub(crate) const DIR_NAME: &str = "o_clear_ends";
/// Records kept per channel; an older one only proves a hop O has not applied yet.
pub(crate) const MAX_RECORDS: usize = 16;
const MAX_BYTES: u64 = 16 << 10;
const SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClearEnd {
    pub channel: u64,
    pub provider: String,
    pub tmux_session: String,
    /// The execution the clear ended.
    pub execution_nonce: String,
    /// The pane's source that execution was writing.
    pub source: SourceId,
    /// The channel's last binding seq when the clear read the pane.
    pub binding_seq: u64,
    pub host: Option<String>,
    pub ended_at: DateTime<Utc>,
    pub basis: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Records {
    schema: u32,
    records: Vec<ClearEnd>,
}

/// How an append left the record: durable, or published by its rename but with its directory
/// entry unsynced, so a host crash may still lose it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Appended {
    Durable,
    PublishedUnsynced(String),
}

/// Test-only switch arming one named mutant of the clear-end contract.
#[cfg(test)]
pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("ADK_CLEAR_END_MUTANT").is_ok_and(|armed| armed == name)
}

#[cfg(not(test))]
pub(crate) fn mutant(_name: &str) -> bool {
    false
}

static WRITE: Mutex<()> = Mutex::new(());
static VERSIONS: LazyLock<Mutex<HashMap<u64, watch::Sender<u64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn path(runtime_root: &Path, channel: u64) -> PathBuf {
    runtime_root.join(DIR_NAME).join(format!("{channel}.json"))
}

fn version_sender(channel: u64) -> watch::Sender<u64> {
    let mut versions = VERSIONS.lock().unwrap_or_else(PoisonError::into_inner);
    versions
        .entry(channel)
        .or_insert_with(|| watch::channel(0).0)
        .clone()
}

/// Bumped after each append that published a record of `channel`.
pub(crate) fn subscribe(channel: u64) -> watch::Receiver<u64> {
    version_sender(channel).subscribe()
}

/// The channel's records, oldest first; a missing file has none.
pub(crate) fn read(runtime_root: &Path, channel: u64) -> Result<Vec<ClearEnd>, String> {
    let file = match fs::File::open(path(runtime_root, channel)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("clear ends exceed their size limit".into());
    }
    let records: Records = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if records.schema != SCHEMA {
        return Err(format!("clear ends schema {}", records.schema));
    }
    Ok(records.records)
}

/// Adds `record`, dropping the oldest past `MAX_RECORDS`; an error means nothing was published.
pub(crate) fn append(runtime_root: &Path, record: ClearEnd) -> Result<Appended, String> {
    let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let channel = record.channel;
    let target = path(runtime_root, channel);
    let dir = runtime_root.join(DIR_NAME);
    durable::ensure_dir(&dir).map_err(|error| error.to_string())?;
    durable::sweep_tmp(&dir).map_err(|error| error.to_string())?;
    let mut records = read(runtime_root, channel).unwrap_or_default();
    records.push(record);
    let excess = records.len().saturating_sub(MAX_RECORDS);
    records.drain(..excess);
    let schema = SCHEMA;
    let bytes = serde_json::to_vec(&Records { schema, records }).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("clear ends exceed their size limit".into());
    }
    let appended = match durable::replace(&target, &bytes) {
        Ok(()) => Appended::Durable,
        // The rename may have published the file before the failure; what it holds decides.
        Err(error)
            if fs::read(&target).is_ok_and(|now| now == bytes) || mutant("unpublished_bumps") =>
        {
            Appended::PublishedUnsynced(error.to_string())
        }
        Err(error) => return Err(error.to_string()),
    };
    version_sender(channel).send_modify(|version| *version += 1);
    Ok(appended)
}

#[cfg(test)]
#[path = "clear_end_tests.rs"]
mod tests;

impl ChannelStore {
    /// The clear ends of this channel, read beside the store under the same runtime root.
    pub(crate) fn clear_ends(&self) -> Result<Vec<ClearEnd>, String> {
        let root = (self.dir.parent())
            .filter(|parent| {
                parent
                    .file_name()
                    .is_some_and(|name| name == STORE_DIR_NAME)
            })
            .and_then(Path::parent)
            .ok_or("the store has no runtime root")?;
        read(root, self.init.channel)
    }
}
