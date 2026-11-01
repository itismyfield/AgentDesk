//! When AgentDesk spawned each socket's server, kept on disk so the start limit holds across
//! AgentDesk restarts; read and written only under the socket dir's lock.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// At most this many spawns of one socket's server within `WINDOW` seconds.
pub(crate) const LIMIT: usize = 3;
pub(crate) const WINDOW: u64 = 600;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub socket: PathBuf,
    pub spawns: Vec<Spawn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Spawn {
    /// Unix seconds when the spawn was recorded, before the process existed.
    pub at: u64,
    pub pid: Option<u32>,
    pub outcome: String,
}

pub(crate) fn path(dir: &Path, socket: &Path) -> PathBuf {
    let digest = Sha256::digest(socket.as_os_str().as_encoded_bytes());
    dir.join(format!("{}.json", hex::encode(&digest[..8])))
}

/// The record of `socket`, empty when none was written; an unreadable one refuses the start.
pub(crate) fn load(dir: &Path, socket: &Path) -> Result<Record, String> {
    match std::fs::read(path(dir, socket)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Record {
            socket: socket.into(),
            spawns: Vec::new(),
        }),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn save(dir: &Path, record: &Record) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| e.to_string())?;
    let target = path(dir, &record.socket);
    let staged = target.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
    std::fs::write(&staged, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&staged, &target).map_err(|e| e.to_string())
}

impl Record {
    /// Whether `LIMIT` spawns fall within `WINDOW` of `now`; a clock that moved back counts too.
    pub(crate) fn exhausted(&self, now: u64) -> bool {
        let recent = self
            .spawns
            .iter()
            .filter(|spawn| spawn.at.abs_diff(now) < WINDOW)
            .count();
        recent >= LIMIT
    }

    /// Records a spawn about to happen, keeping only the entries the limit can still count.
    pub(crate) fn begin(&mut self, now: u64) {
        self.spawns.retain(|spawn| spawn.at.abs_diff(now) < WINDOW);
        self.spawns.push(Spawn {
            at: now,
            pid: None,
            outcome: "spawning".into(),
        });
    }

    pub(crate) fn finish(&mut self, pid: Option<u32>, outcome: &str) {
        if let Some(last) = self.spawns.last_mut() {
            last.pid = pid;
            last.outcome = outcome.into();
        }
    }
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
