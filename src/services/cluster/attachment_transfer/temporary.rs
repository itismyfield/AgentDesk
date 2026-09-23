//! Private per-invocation directories, with OS locks for safe orphan cleanup.
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

// Active invocations hold locks regardless of age; only day-old orphans qualify.
const ORPHAN_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug)]
pub(super) struct Directory {
    // Drop the open lock file before TempDir removes it (required on Windows).
    _lease: File,
    directory: tempfile::TempDir,
}

fn root() -> std::io::Result<PathBuf> {
    #[cfg(test)]
    {
        static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        return Ok(ROOT
            .get_or_init(|| tempfile::tempdir().expect("test upload root"))
            .path()
            .to_path_buf());
    }
    #[cfg(not(test))]
    crate::config::runtime_root()
        .map(|root| root.join("runtime").join("portable_attachments"))
        .ok_or_else(|| std::io::Error::other("runtime root unavailable"))
}

impl Directory {
    pub(super) fn new() -> std::io::Result<Self> {
        Self::in_root(&root()?)
    }

    fn in_root(root: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(root)?;
        if fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err(std::io::Error::other(
                "attachment root must not be a symlink",
            ));
        }
        let directory = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(root)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join(".owner.lock"))?;
        lease.lock()?;
        Ok(Self {
            _lease: lease,
            directory,
        })
    }

    pub(super) fn path(&self) -> &Path {
        self.directory.path()
    }
}

fn cleanup_in(root: &Path, max_age: Duration) -> std::io::Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Ok(0);
    }
    let absolute_root = root.canonicalize()?;
    let mut deleted = 0;
    for entry in fs::read_dir(&absolute_root)?.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("bundle-")
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let path = entry.path().canonicalize()?;
        if path.parent() != Some(absolute_root.as_path()) {
            continue;
        }
        let marker = path.join(".owner.lock");
        if fs::symlink_metadata(&marker).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        let Ok(lease) = OpenOptions::new().read(true).write(true).open(&marker) else {
            continue;
        };
        let old = lease
            .metadata()?
            .modified()?
            .elapsed()
            .is_ok_and(|age| age >= max_age);
        if !old || lease.try_lock().is_err() {
            continue;
        }
        // The resolved directory is a direct child of the dedicated runtime root;
        // active invocations hold the lock regardless of their age.
        drop(lease);
        match fs::remove_dir_all(&path) {
            Ok(()) => deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(deleted)
}

pub(crate) fn spawn_cleanup() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        tokio::spawn(async {
            loop {
                let result =
                    tokio::task::spawn_blocking(|| cleanup_in(&root()?, ORPHAN_MAX_AGE)).await;
                if let Ok(Err(error)) = result {
                    tracing::warn!(%error, "attachment orphan cleanup failed");
                }
                tokio::time::sleep(CLEANUP_INTERVAL).await;
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_cleanup_keeps_locked_execution_and_removes_only_expired_orphans() {
        let root = tempfile::tempdir().unwrap();
        let active = Directory::in_root(root.path()).unwrap();
        let orphan = Directory::in_root(root.path()).unwrap();
        let Directory {
            _lease: orphan_lock,
            directory,
        } = orphan;
        let orphan_path = directory.keep();
        drop(orphan_lock);
        let unrelated = root.path().join("operator-files");
        fs::create_dir(&unrelated).unwrap();
        assert_eq!(
            cleanup_in(root.path(), Duration::from_secs(86_400)).unwrap(),
            0
        );
        assert_eq!(cleanup_in(root.path(), Duration::ZERO).unwrap(), 1);
        assert!(active.path().exists());
        assert!(!orphan_path.exists());
        assert!(unrelated.exists());
        let active_path = active.path().to_path_buf();
        drop(active);
        assert!(!active_path.exists());
    }
}
