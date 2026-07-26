use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use uuid::Uuid;

use super::StoreError;

const CHANNEL_LOCK: &str = "channel.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::status_panel_transition_v2) enum WriteStage {
    CreateTemp,
    WriteAll,
    SyncFile,
    Rename,
    SyncParent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteTarget {
    Operation,
    Channel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Failpoint {
    pub target: WriteTarget,
    pub stage: WriteStage,
}

pub(super) struct ChannelLock {
    file: File,
}

impl Drop for ChannelLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

pub(super) fn ensure_directory(path: &Path) -> Result<(), StoreError> {
    if !path.exists() {
        fs::create_dir_all(path).map_err(|_| StoreError::WriteFailed(WriteStage::CreateTemp))?;
    }
    reject_directory(path)
}

pub(super) fn ensure_child_directory(root: &Path, path: &Path) -> Result<(), StoreError> {
    if !path.starts_with(root) {
        return Err(StoreError::InvalidPathComponent("root"));
    }
    if !path.exists() {
        fs::create_dir(path).map_err(|_| StoreError::WriteFailed(WriteStage::CreateTemp))?;
    }
    reject_directory(path)
}

pub(super) fn lock_channel(channel_dir: &Path) -> Result<ChannelLock, StoreError> {
    let path = channel_dir.join(CHANNEL_LOCK);
    reject_symlink(&path)?;
    let file = secure_open(&path, true, true).map_err(|_| StoreError::LockFailed)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(StoreError::LockFailed);
        }
    }
    Ok(ChannelLock { file })
}

pub(super) fn read_nofollow(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    reject_symlink(path)?;
    let mut file = match secure_open(path, false, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(StoreError::ReadFailed),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| StoreError::ReadFailed)?;
    Ok(Some(bytes))
}

pub(super) fn atomic_write(
    path: &Path,
    body: &[u8],
    target: WriteTarget,
    failpoint: Option<Failpoint>,
) -> Result<(), StoreError> {
    let parent = path.parent().ok_or(StoreError::UnexpectedFileType)?;
    reject_symlink(path)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("record"),
        Uuid::new_v4().simple()
    ));
    maybe_fail(failpoint, target, WriteStage::CreateTemp)?;
    let result = (|| {
        let mut file = secure_create_new(&temp)
            .map_err(|_| StoreError::WriteFailed(WriteStage::CreateTemp))?;
        maybe_fail(failpoint, target, WriteStage::WriteAll)?;
        file.write_all(body)
            .map_err(|_| StoreError::WriteFailed(WriteStage::WriteAll))?;
        maybe_fail(failpoint, target, WriteStage::SyncFile)?;
        file.sync_all()
            .map_err(|_| StoreError::WriteFailed(WriteStage::SyncFile))?;
        maybe_fail(failpoint, target, WriteStage::Rename)?;
        fs::rename(&temp, path).map_err(|_| StoreError::WriteFailed(WriteStage::Rename))?;
        maybe_fail(failpoint, target, WriteStage::SyncParent)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub(super) fn quarantine(channel_dir: &Path, path: &Path) -> Result<(), StoreError> {
    let quarantine = channel_dir.join("quarantine").join(format!(
        "{}-{}.json",
        Uuid::new_v4().simple(),
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("record")
    ));
    fs::rename(path, quarantine).map_err(|_| StoreError::ReadFailed)?;
    sync_directory(channel_dir)
}

pub(super) fn prune_directory(path: &Path, keep: usize) -> Result<(), StoreError> {
    let mut files: Vec<_> = fs::read_dir(path)
        .map_err(|_| StoreError::ReadFailed)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .collect();
    files.sort_by_key(|entry| entry.file_name());
    let remove_count = files.len().saturating_sub(keep);
    for entry in files.into_iter().take(remove_count) {
        fs::remove_file(entry.path())
            .map_err(|_| StoreError::WriteFailed(WriteStage::SyncParent))?;
    }
    if remove_count > 0 {
        sync_directory(path)?;
    }
    Ok(())
}

fn maybe_fail(
    failpoint: Option<Failpoint>,
    target: WriteTarget,
    stage: WriteStage,
) -> Result<(), StoreError> {
    if failpoint == Some(Failpoint { target, stage }) {
        Err(StoreError::WriteFailed(stage))
    } else {
        Ok(())
    }
}

fn reject_directory(path: &Path) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| StoreError::ReadFailed)?;
    if metadata.file_type().is_symlink() {
        Err(StoreError::SymlinkRejected)
    } else if !metadata.is_dir() {
        Err(StoreError::UnexpectedFileType)
    } else {
        Ok(())
    }
}

fn reject_symlink(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::SymlinkRejected),
        Ok(metadata) if !metadata.is_file() => Err(StoreError::UnexpectedFileType),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(StoreError::ReadFailed),
    }
}

fn secure_open(path: &Path, create: bool, write: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(write).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    options.open(path)
}

fn secure_create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    options.open(path)
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| StoreError::WriteFailed(WriteStage::SyncParent))
}

#[cfg(test)]
pub(super) fn try_lock_for_test(channel_dir: &Path) -> Result<ChannelLock, StoreError> {
    let path = channel_dir.join(CHANNEL_LOCK);
    reject_symlink(&path)?;
    let file = secure_open(&path, true, true).map_err(|_| StoreError::LockFailed)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(StoreError::LockFailed);
        }
    }
    Ok(ChannelLock { file })
}
