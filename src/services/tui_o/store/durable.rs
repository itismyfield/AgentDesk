//! Crash-ordered file primitives: create-once, atomic replace, synced append and tail truncation.

use std::fs::{self, OpenOptions};
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

const TMP_SUFFIX: &str = ".tmp";

/// A fresh temp name per write, so a retry never reuses an alias a crash left behind.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}{TMP_SUFFIX}", uuid::Uuid::new_v4().simple()));
    PathBuf::from(name)
}

/// `create_new` never opens, and so never truncates, an inode that is already published.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Unlinks temp files a crash left in `dir`, including hard-link aliases of published files.
pub(super) fn sweep_tmp(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|name| name.to_str());
        if name.is_some_and(|name| name.ends_with(TMP_SUFFIX)) && path.is_file() {
            fs::remove_file(&path)?;
            fsync_parent_dir(&path)?;
        }
    }
    Ok(())
}

/// Publishes `bytes` at `path` once; the link fails if it exists, and a crash leaves it absent or whole.
pub(super) fn create_once(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path);
    write_synced(&tmp, bytes)?;
    fs::hard_link(&tmp, path)?;
    fsync_parent_dir(path)?;
    fs::remove_file(&tmp)
}

/// Replaces `path` atomically: temp write, fsync, rename, then the directory entry.
pub(super) fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path);
    write_synced(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    fsync_parent_dir(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_once_retry_leaves_the_published_file_and_its_crash_alias_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init");
        create_once(&path, b"original").unwrap();
        // The state a crash between the link and the temp unlink leaves behind.
        fs::hard_link(&path, dir.path().join("init.tmp")).unwrap();
        assert!(create_once(&path, b"retried").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        sweep_tmp(dir.path()).unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["init"]);
    }
}
