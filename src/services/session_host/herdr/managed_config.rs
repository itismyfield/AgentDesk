//! Dedicated-home config preparation; activation belongs to the runtime owner.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::super::observe::CANONICAL_CONFIG;
use crate::services::discord::runtime_store::fsync_parent_dir;

#[path = "managed_config/path.rs"]
pub(crate) mod path;
#[path = "managed_config/publication.rs"]
pub(crate) mod publication;

pub(crate) use path::ManagedHome;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnsureResult {
    Unchanged,
    Prepared { backup: Option<PathBuf> },
    Recovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    BeforePreparing,
    Preparing,
    Backup,
    BeforeRename,
    Renamed,
    ConfigSynced,
    BeforeCommitted,
    Committed,
}

fn invalid(detail: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

fn open_regular(path: &Path) -> io::Result<Option<File>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(invalid(
            "managed config entry must be a single-link regular file",
        ));
    }
    Ok(Some(file))
}

fn same(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    (
        a.dev(),
        a.ino(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}

fn unchanged(file: &File, path: &Path, before: &fs::Metadata) -> io::Result<()> {
    if !same(before, &file.metadata()?) || !same(before, &fs::symlink_metadata(path)?) {
        return Err(invalid("managed config entry changed during observation"));
    }
    Ok(())
}

fn source(config: &Path) -> io::Result<Option<(File, fs::Metadata, bool)>> {
    let Some(mut file) = open_regular(config)? else {
        return Ok(None);
    };
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(CANONICAL_CONFIG.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    unchanged(&file, config, &before)?;
    Ok(Some((file, before, bytes == CANONICAL_CONFIG)))
}

fn synced_temp(path: &Path, bytes: &[u8]) -> io::Result<tempfile::NamedTempFile> {
    let mut tmp = tempfile::Builder::new()
        .prefix(".agentdesk-config-")
        .tempfile_in(
            path.parent()
                .ok_or_else(|| invalid("config entry has no parent"))?,
        )?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    Ok(tmp)
}

fn publish(tmp: tempfile::NamedTempFile, path: &Path) -> io::Result<()> {
    tmp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn backup(config: &Path, file: &mut File, before: &fs::Metadata) -> io::Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let name = format!(
        "config.toml.bak.{}-{:09}.{}",
        stamp.as_secs(),
        stamp.subsec_nanos(),
        uuid::Uuid::new_v4()
    );
    let path = config.with_file_name(name);
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.seek(SeekFrom::Start(0))?;
    io::copy(&mut *file, &mut out)?;
    unchanged(file, config, before)?;
    out.sync_all()?;
    fsync_parent_dir(&path)?;
    Ok(path)
}

struct HomeLock(File);
impl Drop for HomeLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl ManagedHome {
    pub(crate) fn ensure(&self) -> io::Result<EnsureResult> {
        self.ensure_with(|_| Ok(()))
    }

    fn lock(&self) -> io::Result<HomeLock> {
        let path = self.path.join(".agentdesk-config.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.nlink() != 1 || meta.uid() != self.uid {
            return Err(invalid("unsafe managed config lock"));
        }
        file.try_lock().map_err(io::Error::from)?;
        unchanged(&file, &path, &meta)?;
        Ok(HomeLock(file))
    }

    fn ensure_with(
        &self,
        mut checkpoint: impl FnMut(Step) -> io::Result<()>,
    ) -> io::Result<EnsureResult> {
        self.prepare_dir()?;
        let config = self.path.join("config.toml");
        let current = source(&config)?;
        let state = publication::state(&config)?;
        let canonical = current.as_ref().is_some_and(|(_, _, canonical)| *canonical);
        if canonical && state != Some(publication::State::Preparing) {
            return Ok(EnsureResult::Unchanged);
        }
        let _lock = self.lock()?;
        let current = source(&config)?;
        let state = publication::state(&config)?;
        let canonical = current.as_ref().is_some_and(|(_, _, canonical)| *canonical);
        if canonical && state != Some(publication::State::Preparing) {
            return Ok(EnsureResult::Unchanged);
        }
        checkpoint(Step::BeforePreparing)?;
        // Persist the veto before publishing canonical bytes with a possibly older tmp mtime.
        publication::store(&config, publication::State::Preparing)?;
        checkpoint(Step::Preparing)?;
        let mut saved = None;
        if !canonical {
            if let Some((mut file, before, _)) = current {
                saved = Some(backup(&config, &mut file, &before)?);
                checkpoint(Step::Backup)?;
            }
            let tmp = synced_temp(&config, CANONICAL_CONFIG)?;
            checkpoint(Step::BeforeRename)?;
            self.verify_dir()?;
            publish(tmp, &config)?;
            checkpoint(Step::Renamed)?;
            fsync_parent_dir(&config)?;
            checkpoint(Step::ConfigSynced)?;
        }
        checkpoint(Step::BeforeCommitted)?;
        publication::store(&config, publication::State::Committed(SystemTime::now()))?;
        checkpoint(Step::Committed)?;
        Ok(if canonical {
            EnsureResult::Recovered
        } else {
            EnsureResult::Prepared { backup: saved }
        })
    }
}

#[cfg(test)]
#[path = "managed_config_tests.rs"]
mod tests;
