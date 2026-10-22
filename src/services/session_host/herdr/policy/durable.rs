//! Replace this private publication adapter when the shared durable helper is available.

#[cfg(unix)]
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

#[cfg(unix)]
const LIMIT: usize = 1024 * 1024;
#[derive(Clone)]
pub(crate) struct Directory(pub(crate) Option<PathBuf>);

pub(crate) fn key(value: &impl Serialize) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(|e| e.to_string())?)
    ))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), String> {
    crate::services::discord::runtime_store::fsync_parent_dir(path).map_err(|e| e.to_string())
}

#[cfg(unix)]
fn ensure_dir(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => return Err(format!("{} is not a directory", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    ensure_dir(path.parent().ok_or("missing store ancestor")?)?;
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => sync_parent(path),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(unix)]
fn private_file(path: &Path, write: bool, fresh: bool) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .create(write && !fresh)
        .create_new(fresh)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.mode() & 0o777 != 0o600 {
        return Err("snapshot/lock must be a regular 0600 file".into());
    }
    Ok(file)
}

#[cfg(unix)]
fn clear_marker(pending: &Path, unlink: &'static str, synced: &'static str) -> Result<(), String> {
    checkpoint(unlink)?;
    tempfile::TempPath::try_from_path(pending)
        .and_then(|path| path.close())
        .map_err(|e| e.to_string())?;
    if let Err(error) = checkpoint(synced).and_then(|()| sync_parent(pending)) {
        // A failed cleanup barrier must remain visible to the next reader.
        if let Ok(marker) = private_file(pending, true, true) {
            let _ = marker.sync_all();
            let _ = sync_parent(pending);
        }
        return Err(error);
    }
    Ok(())
}

impl Directory {
    pub(crate) fn failure(&self, key: &str, reason: impl std::fmt::Display) -> String {
        let root = self
            .0
            .as_deref()
            .unwrap_or(Path::new("<unavailable policy store>"));
        let snapshot = root.join(format!("{key}.json"));
        let pending = root.join(format!("{key}.pending"));
        format!(
            "{}: {reason}; preserve files, inspect/restore the verified snapshot at {}, repair the interrupted marker {} only after verifying the snapshot, then restart this node",
            snapshot.display(),
            snapshot.display(),
            pending.display()
        )
    }
}

#[cfg(unix)]
impl Directory {
    fn root(&self) -> Result<&Path, String> {
        let root = self
            .0
            .as_deref()
            .ok_or("policy store root unavailable; configure this node's runtime root")?;
        if !root.is_absolute() {
            return Err(self.failure("", "policy store root must be absolute"));
        }
        match fs::symlink_metadata(root) {
            Ok(meta) if !meta.is_dir() || meta.mode() & 0o777 != 0o700 => {
                Err(self.failure("", "store root must be a real 0700 directory"))
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(self.failure("", e)),
            _ => Ok(root),
        }
    }

    fn path(&self, key: &str, suffix: &str) -> Result<PathBuf, String> {
        if key.len() != 64 || !key.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(self.failure("", "invalid policy snapshot key"));
        }
        Ok(self.root()?.join(format!("{key}.{suffix}")))
    }

    fn committed(&self, key: &str) -> Result<(), String> {
        let pending = self.path(key, "pending")?;
        match fs::symlink_metadata(&pending) {
            Ok(_) => Err(self.failure(key, "policy_publication_pending: interrupted publication")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.failure(key, e)),
        }
    }

    pub(crate) fn keys(&self) -> Result<Vec<String>, String> {
        let root = self.root()?;
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.failure("", e)),
        };
        let mut keys = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| self.failure("", e))?.path();
            if path.extension().is_some_and(|e| e == "pending") {
                return Err(self.failure(
                    path.file_stem().and_then(|s| s.to_str()).unwrap_or(""),
                    "policy_publication_pending: interrupted publication",
                ));
            }
            if path.extension().is_some_and(|e| e == "json") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| self.failure("", "invalid snapshot name"))?;
                self.path(name, "json")?;
                keys.push(name.into());
            }
        }
        keys.sort();
        Ok(keys)
    }

    pub(crate) fn read(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.committed(key)?;
        let path = self.path(key, "json")?;
        let result = (|| {
            let meta = match fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.to_string()),
                Ok(meta) => meta,
            };
            if !meta.is_file() {
                return Err("snapshot must be a regular file".into());
            }
            let file = private_file(&path, false, false)?;
            let mut bytes = Vec::new();
            file.take(LIMIT as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() > LIMIT {
                return Err("policy_snapshot_oversize: restore a bounded snapshot".into());
            }
            Ok(Some(bytes))
        })()
        .map_err(|e: String| self.failure(key, e))?;
        self.committed(key)?;
        Ok(result)
    }

    // A live writer's marker is transient; abandoned markers still fail after its lock is released.
    #[cfg(test)]
    pub(crate) fn read_stable(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let path = self.path(key, "lock")?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return self.read(key),
            Err(error) => return Err(self.failure(key, error)),
            Ok(_) => {}
        }
        let lock = private_file(&path, false, false).map_err(|error| self.failure(key, error))?;
        lock.lock_shared()
            .map_err(|error| self.failure(key, error))?;
        self.read(key)
    }

    pub(crate) fn change(
        &self,
        key: &str,
        update: impl FnOnce(Option<Vec<u8>>) -> Result<Vec<u8>, String>,
    ) -> Result<Vec<u8>, String> {
        let path = self.path(key, "json")?;
        let result = (|| {
            ensure_dir(path.parent().ok_or("missing store parent")?)?;
            self.root()?;
            let lock_path = self.path(key, "lock")?;
            let lock = private_file(&lock_path, true, false)?;
            lock.lock().map_err(|e| e.to_string())?;
            let previous = self.read(key)?;
            let next = update(previous.clone())?;
            if next.len() > LIMIT {
                return Err("policy snapshot exceeds size limit".into());
            }
            if previous.as_ref() == Some(&next) {
                return Ok(next);
            }
            let pending = self.path(key, "pending")?;
            let mut marker = private_file(&pending, true, true)?;
            let mut persist_started = false;
            let publication = (|| {
                marker
                    .write_all(b"publication not yet acknowledged")
                    .map_err(|e| e.to_string())?;
                checkpoint("marker_sync")?;
                marker.sync_all().map_err(|e| e.to_string())?;
                sync_parent(&pending)?;
                let mut temporary = tempfile::Builder::new()
                    .prefix(".policy-")
                    .tempfile_in(path.parent().ok_or("missing parent")?)
                    .map_err(|e| e.to_string())?;
                temporary.write_all(&next).map_err(|e| e.to_string())?;
                checkpoint("candidate_sync")?;
                temporary.as_file().sync_all().map_err(|e| e.to_string())?;
                checkpoint("before_rename")?;
                // Persist errors may have crossed rename, so keep their marker too.
                persist_started = true;
                temporary.persist(&path).map_err(|e| e.to_string())?;
                checkpoint("after_rename")?;
                checkpoint("parent_sync")?;
                sync_parent(&path)?;
                clear_marker(&pending, "before_marker_clear", "after_marker_clear")?;
                Ok(next)
            })();
            if publication.is_err() && !persist_started {
                clear_marker(&pending, "cleanup_unlink", "cleanup_parent_sync")?;
            }
            publication
        })();
        result.map_err(|e: String| self.failure(key, e))
    }
}

#[cfg(unix)]
fn checkpoint(_stage: &'static str) -> Result<(), String> {
    #[cfg(test)]
    if CRASH.with(|f| f.get() == Some(_stage)) {
        std::process::exit(86);
    }
    #[cfg(test)]
    if FAIL.with(|f| f.get() == Some(_stage)) || CLEANUP_FAIL.with(|f| f.get() == Some(_stage)) {
        return Err(format!("injected {_stage}"));
    }
    Ok(())
}
#[cfg(all(test, unix))]
thread_local! {
    static FAIL: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static CLEANUP_FAIL: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    static CRASH: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}
#[cfg(all(test, unix))]
#[path = "durable_tests.rs"]
mod tests;

#[cfg(not(unix))]
impl Directory {
    pub(crate) fn keys(&self) -> Result<Vec<String>, String> {
        Err(self.failure(
            "",
            "policy_store_unsupported_platform: Unix permissions and directory durability required",
        ))
    }
    pub(crate) fn read(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Err(self.failure(
            key,
            "policy_store_unsupported_platform: Unix permissions and directory durability required",
        ))
    }
    pub(crate) fn change(
        &self,
        key: &str,
        _update: impl FnOnce(Option<Vec<u8>>) -> Result<Vec<u8>, String>,
    ) -> Result<Vec<u8>, String> {
        Err(self.failure(
            key,
            "policy_store_unsupported_platform: Unix permissions and directory durability required",
        ))
    }
}
