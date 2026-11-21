//! Durable restart-notice intents: one file per restart-interrupted episode, kept until the writer
//! settles its single ledger piece. A repeated record of the same episode names the same file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ChannelStore, STORE_DIR_NAME};
use crate::domain::file_sync::fsync_parent_dir;
use crate::services::tui_o::shadow::ShadowProvider;

pub(crate) const DIR_NAME: &str = "o_restart_notices";
static WRITE: Mutex<()> = Mutex::new(());

/// The exact episode a restart cut: the row's channel, provider, turn nonce and born generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Intent {
    pub channel: u64,
    pub provider: ShadowProvider,
    pub turn_nonce: String,
    pub born_generation: u64,
}

impl Intent {
    /// `None` without an exact episode identity; no notice is ever synthesized from less.
    pub(crate) fn exact(
        channel: u64,
        provider: ShadowProvider,
        turn_nonce: Option<&str>,
        born_generation: u64,
    ) -> Option<Self> {
        let turn_nonce = turn_nonce.map(str::trim).filter(|nonce| !nonce.is_empty())?;
        (channel != 0 && born_generation != 0).then(|| Self {
            channel,
            provider,
            turn_nonce: turn_nonce.to_string(),
            born_generation,
        })
    }

    pub(crate) fn key(&self) -> String {
        let named = format!(
            "{:?}/{}/{}/{}",
            self.provider, self.channel, self.born_generation, self.turn_nonce
        );
        hex::encode(Sha256::digest(named.as_bytes()))
    }
}

fn dir(root: &Path, channel: u64) -> PathBuf {
    root.join(DIR_NAME).join(channel.to_string())
}

fn path(root: &Path, intent: &Intent) -> PathBuf {
    dir(root, intent.channel).join(format!("{}.json", intent.key()))
}

/// Durable before it returns; the same episode recorded again is a no-op.
pub(crate) fn record(root: &Path, intent: &Intent) -> Result<(), String> {
    let exact = Intent::exact(
        intent.channel,
        intent.provider,
        Some(&intent.turn_nonce),
        intent.born_generation,
    );
    if exact.as_ref() != Some(intent) {
        return Err("restart notice has no exact episode".into());
    }
    let bytes = serde_json::to_vec(intent).map_err(|error| error.to_string())?;
    let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let target = path(root, intent);
    let same = |target: &Path| fs::read(target).is_ok_and(|now| now == bytes);
    if fs::symlink_metadata(&target).is_ok() {
        return if same(&target) {
            fsync_parent_dir(&target).map_err(|error| error.to_string())
        } else {
            Err("restart notice key already names another episode".into())
        };
    }
    super::durable::ensure_dir(&root.join(DIR_NAME)).map_err(|error| error.to_string())?;
    super::durable::ensure_dir(&dir(root, intent.channel)).map_err(|error| error.to_string())?;
    match super::durable::create_once(&target, &bytes) {
        Ok(()) => Ok(()),
        Err(_) if same(&target) => fsync_parent_dir(&target).map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

/// This channel's recorded episodes; an unreadable file stays for a later pass.
pub(crate) fn pending(root: &Path, channel: u64) -> Vec<Intent> {
    let Ok(entries) = fs::read_dir(dir(root, channel)) else {
        return Vec::new();
    };
    let mut intents: Vec<Intent> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
        .filter_map(|entry| serde_json::from_slice(&fs::read(entry.path()).ok()?).ok())
        .filter(|intent: &Intent| intent.channel == channel)
        .collect();
    intents.sort_by(|a, b| (a.born_generation, a.key()).cmp(&(b.born_generation, b.key())));
    intents
}

/// Drops an episode whose piece reached a final outcome; the ledger keeps it from posting again.
pub(crate) fn settle(root: &Path, intent: &Intent) -> io::Result<()> {
    let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let target = path(root, intent);
    match fs::remove_file(&target) {
        Ok(()) => fsync_parent_dir(&target),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

impl ChannelStore {
    pub(crate) fn restart_notice_root(&self) -> Option<&Path> {
        let parent = self.dir.parent()?;
        (parent.file_name()? == STORE_DIR_NAME)
            .then(|| parent.parent())
            .flatten()
    }
}
