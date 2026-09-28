//! Crash-ordered file primitives: create-once, synced append and tail truncation.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use super::{StoreError, damage};
use crate::services::discord::runtime_store::fsync_parent_dir;

/// Creates `dir` if absent (a symlink is refused) and flushes its entry in the parent.
pub(super) fn ensure_dir(dir: &Path) -> io::Result<()> {
    if let Err(error) = fs::create_dir(dir) {
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    fsync_parent_dir(dir)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Publishes `bytes` at `path` once; the link fails if it exists, and a crash leaves it absent or whole.
pub(super) fn create_once(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path);
    write_synced(&tmp, bytes)?;
    fs::hard_link(&tmp, path)?;
    fsync_parent_dir(path)?;
    fs::remove_file(&tmp)
}

pub(super) fn append_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_data()
}

/// Cuts an unfinished append off the end; only bytes no durable step relied on are removed.
pub(super) fn truncate_synced(path: &Path, len: u64) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    file.sync_all()
}

/// Reads a JSON file; absent is `None`, unparsable is store damage.
pub(super) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| damage(format!("{}: {error}", path.display()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
