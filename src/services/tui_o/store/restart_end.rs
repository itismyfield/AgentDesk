//! Immutable physical ends for a same-source restart; these never claim a native closer.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::{ChannelStore, STORE_DIR_NAME};
use crate::domain::file_sync::fsync_parent_dir;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};

pub(crate) const DIR_NAME: &str = "o_restart_ends";
const SCHEMA: u32 = 1;
const MAX_BYTES: u64 = 16 << 10;
static WRITE: Mutex<()> = Mutex::new(());
static VERSIONS: LazyLock<Mutex<HashMap<u64, watch::Sender<u64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestartEnd {
    pub end_id: String,
    pub channel: u64,
    pub provider: ShadowProvider,
    pub tmux_session: String,
    pub execution_nonce: String,
    pub binding_seq: u64,
    pub source: SourceId,
    pub cutoff: u64,
    pub prefix_hash: String,
    pub open_after: u64,
    pub native_turn_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    witness: RestartEnd,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Appended {
    Durable,
    PublishedUnsynced(String),
}

fn name(end_id: &str) -> String {
    format!("{}.json", hex::encode(Sha256::digest(end_id.as_bytes())))
}

fn dir(root: &Path, channel: u64) -> PathBuf {
    root.join(DIR_NAME).join(channel.to_string())
}

fn path(root: &Path, witness: &RestartEnd) -> PathBuf {
    dir(root, witness.channel).join(name(&witness.end_id))
}

fn version_sender(channel: u64) -> watch::Sender<u64> {
    VERSIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(channel)
        .or_insert_with(|| watch::channel(0).0)
        .clone()
}

pub(crate) fn subscribe(channel: u64) -> watch::Receiver<u64> {
    version_sender(channel).subscribe()
}

fn valid(witness: &RestartEnd, channel: u64) -> bool {
    channel != 0
        && witness.channel == channel
        && !witness.end_id.trim().is_empty()
        && !witness.tmux_session.trim().is_empty()
        && witness.execution_nonce.len() == 32
        && witness
            .execution_nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && witness.binding_seq != 0
        && !witness.source.session_id.trim().is_empty()
        && witness.source.path.is_absolute()
        && witness.open_after != 0
        && witness.open_after <= witness.cutoff
        && witness.prefix_hash.len() == 64
        && witness
            .prefix_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && witness
            .native_turn_id
            .as_ref()
            .is_none_or(|id| !id.trim().is_empty())
}

fn known_fields(raw: &serde_json::Value, encoded: &serde_json::Value) -> bool {
    match (raw, encoded) {
        (serde_json::Value::Object(raw), serde_json::Value::Object(encoded)) => {
            raw.iter().all(|(key, value)| {
                encoded
                    .get(key)
                    .is_some_and(|expected| known_fields(value, expected))
            })
        }
        _ => true,
    }
}

fn read_one(path: &Path, channel: u64) -> Result<RestartEnd, String> {
    if !fs::symlink_metadata(path)
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("restart end is not a regular file".into());
    }
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("restart end exceeds its size limit".into());
    }
    let record: Record = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let encoded = serde_json::to_value(&record).map_err(|error| error.to_string())?;
    if record.schema != SCHEMA
        || !known_fields(&raw, &encoded)
        || !valid(&record.witness, channel)
        || path
            .file_name()
            .is_none_or(|file| file != name(&record.witness.end_id).as_str())
    {
        return Err("restart end schema or identity mismatch".into());
    }
    Ok(record.witness)
}

/// Missing is empty. Records are never evicted, and source files are checked only by consumers.
pub(crate) fn read(root: &Path, channel: u64) -> Result<Vec<RestartEnd>, String> {
    let entries = match fs::read_dir(dir(root, channel)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut witnesses = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let file = entry.path();
        if file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".tmp"))
        {
            continue;
        }
        match read_one(&file, channel) {
            Ok(witness) => witnesses.push(witness),
            Err(reason) => {
                let end_id = file.file_name().unwrap_or_default().to_string_lossy();
                tracing::warn!(%end_id, %reason, "[tui_o] skipping invalid restart end");
            }
        }
    }
    witnesses.sort_by(|a, b| {
        (a.binding_seq, a.cutoff, &a.end_id).cmp(&(b.binding_seq, b.cutoff, &b.end_id))
    });
    Ok(witnesses)
}

fn confirm_durable(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    super::durable::fault::strike(path, super::durable::fault::Step::ReplaceDirSync)
        .map_err(|error| error.to_string())?;
    fsync_parent_dir(path).map_err(|error| error.to_string())
}

/// Re-establishes directory durability before a consumer projects any published end.
pub(crate) fn read_durable(root: &Path, channel: u64) -> Result<Vec<RestartEnd>, String> {
    let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let witnesses = read(root, channel)?;
    if let Some(first) = witnesses.first() {
        confirm_durable(&path(root, first))?;
    }
    Ok(witnesses)
}

fn verify_source_eof(witness: &RestartEnd) -> Result<(), String> {
    use crate::services::tui_o::shadow::capture::Fingerprint;
    use std::io::{Seek, SeekFrom};

    let mut file = fs::File::open(&witness.source.path).map_err(|error| error.to_string())?;
    let before = Fingerprint::of(&file.metadata().map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    if (before.dev, before.ino) != (witness.source.dev, witness.source.ino)
        || before.len != witness.cutoff
    {
        return Err("restart end source or EOF differs".into());
    }
    let mut hash = Sha256::new();
    io::copy(&mut (&mut file).take(witness.cutoff), &mut hash)
        .map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(witness.cutoff - 1))
        .map_err(|error| error.to_string())?;
    let mut last = [0];
    file.read_exact(&mut last)
        .map_err(|error| error.to_string())?;
    let after = Fingerprint::of(&file.metadata().map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    if last != [b'\n']
        || hex::encode(hash.finalize()) != witness.prefix_hash
        || before != after
        || Fingerprint::path(&witness.source).map_err(|(_, error)| error)? != before
    {
        return Err("restart end prefix or complete EOF differs".into());
    }
    Ok(())
}

/// Written by the effort restart once it has confirmed the old process ended.
pub(crate) fn append(root: &Path, witness: &RestartEnd) -> Result<Appended, String> {
    if !valid(witness, witness.channel) {
        return Err("restart end identity mismatch".into());
    }
    let bytes = serde_json::to_vec(&Record {
        schema: SCHEMA,
        witness: witness.clone(),
    })
    .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("restart end exceeds its size limit".into());
    }
    let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let target = path(root, witness);
    match fs::symlink_metadata(&target) {
        Ok(_) => {
            if read_one(&target, witness.channel)? != *witness {
                return Err("restart end id already names another record".into());
            }
            confirm_durable(&target)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            verify_source_eof(witness)?;
            super::durable::ensure_dir(&root.join(DIR_NAME)).map_err(|error| error.to_string())?;
            super::durable::ensure_dir(&dir(root, witness.channel))
                .map_err(|error| error.to_string())?;
            if let Err(error) = super::durable::create_once(&target, &bytes) {
                if fs::read(&target).is_ok_and(|now| now == bytes) {
                    version_sender(witness.channel).send_modify(|version| *version += 1);
                    return Ok(Appended::PublishedUnsynced(error.to_string()));
                }
                return Err(error.to_string());
            }
        }
        Err(error) => return Err(error.to_string()),
    }
    version_sender(witness.channel).send_modify(|version| *version += 1);
    Ok(Appended::Durable)
}

impl ChannelStore {
    fn restart_end_root(&self) -> Result<&Path, String> {
        self.dir
            .parent()
            .filter(|parent| {
                parent
                    .file_name()
                    .is_some_and(|name| name == STORE_DIR_NAME)
            })
            .and_then(Path::parent)
            .ok_or_else(|| "the store has no runtime root".into())
    }

    pub(crate) fn durable_restart_ends(&self) -> Result<Vec<RestartEnd>, String> {
        read_durable(self.restart_end_root()?, self.init.channel)
    }
}

#[cfg(test)]
#[path = "restart_end_tests.rs"]
mod tests;
