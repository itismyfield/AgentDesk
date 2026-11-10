use std::path::Path;

#[cfg(all(not(test), not(windows)))]
use std::fs;

/// Opens the parent read-only and syncs its directory entry, returning I/O errors.
/// This is separate from file sync and rename; a bare filename uses `.`.
#[cfg(not(windows))]
pub(crate) fn fsync_parent_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    use parent_sync_probe as fs;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    fs::File::open(parent.unwrap_or_else(|| Path::new(".")))?.sync_all()
}

/// Records successful directory syncs while a test arms the thread-local probe.
#[cfg(all(test, not(windows)))]
pub(crate) mod parent_sync_probe {
    use std::path::{Path, PathBuf};
    thread_local! {
        pub(crate) static SYNCED: std::cell::RefCell<Option<Vec<PathBuf>>> = const { std::cell::RefCell::new(None) };
    }
    pub(crate) struct File(std::fs::File, PathBuf);
    impl File {
        pub(crate) fn open(dir: &Path) -> std::io::Result<Self> {
            Ok(Self(std::fs::File::open(dir)?, dir.to_path_buf()))
        }
        pub(crate) fn sync_all(&self) -> std::io::Result<()> {
            self.0.sync_all()?;
            SYNCED.with(|synced| synced.borrow_mut().as_mut().map(|s| s.push(self.1.clone())));
            Ok(())
        }
    }
}

/// Windows reports success without flushing the directory entry.
#[cfg(windows)]
pub(crate) fn fsync_parent_dir(_: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Distinguishes a directory flush from the unflushed success on Windows.
pub(crate) const PARENT_DIR_FSYNC_FLUSHES: bool = cfg!(not(windows));
