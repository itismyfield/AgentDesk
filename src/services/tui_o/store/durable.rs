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
    #[cfg(test)]
    fault::strike(path, fault::Step::Write)?;
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
    #[cfg(test)]
    if let Some((kept, error)) = fault::append(path, bytes) {
        file.write_all(kept)?;
        return Err(error);
    }
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

/// Test builds: store writes under a directory fail as a full disk would, and recovery attempts
/// and ledger withdrawals under it are recorded.
#[cfg(test)]
pub(crate) mod fault {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use tokio::time::Instant;

    /// The store step a planted failure strikes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Step {
        /// A synced append that first writes none, half or all of its bytes.
        Append(Keep),
        /// A temp file written for a create or a replace.
        Write,
        /// Cutting a withdrawn ledger line, before the cut or before its sync.
        Cut,
        CutSync,
        /// Recovering the channel's spool and cursors.
        SpoolRecovery,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Keep {
        Nothing,
        Half,
        All,
    }

    struct Plant {
        id: u64,
        under: PathBuf,
        step: Step,
        kind: io::ErrorKind,
        /// Strikes left; `None` strikes until the plant is dropped.
        left: Option<usize>,
    }

    #[derive(Default)]
    struct Registry {
        next: u64,
        plants: Vec<Plant>,
        watched: Vec<(u64, PathBuf, Vec<Instant>, usize)>,
    }

    static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
        next: 0,
        plants: Vec::new(),
        watched: Vec::new(),
    });

    fn registry() -> MutexGuard<'static, Registry> {
        REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Removes its plant when dropped: the space came back.
    pub(crate) struct Planted(u64);

    impl Drop for Planted {
        fn drop(&mut self) {
            registry().plants.retain(|plant| plant.id != self.0);
        }
    }

    /// Fails `step` under `under` with `kind` until dropped, or only `times` times.
    pub(crate) fn plant(
        under: &Path,
        step: Step,
        kind: io::ErrorKind,
        times: Option<usize>,
    ) -> Planted {
        let mut registry = registry();
        registry.next += 1;
        let id = registry.next;
        let (under, left) = (under.to_path_buf(), times);
        registry.plants.push(Plant {
            id,
            under,
            step,
            kind,
            left,
        });
        Planted(id)
    }

    fn take(path: &Path, wanted: impl Fn(Step) -> bool) -> Option<(Step, io::Error)> {
        let mut registry = registry();
        let plant = registry.plants.iter_mut().find(|plant| {
            path.starts_with(&plant.under) && wanted(plant.step) && plant.left != Some(0)
        })?;
        if let Some(left) = plant.left.as_mut() {
            *left -= 1;
        }
        Some((plant.step, io::Error::from(plant.kind)))
    }

    pub(crate) fn strike(path: &Path, step: Step) -> io::Result<()> {
        take(path, |planted| planted == step).map_or(Ok(()), |(_, error)| Err(error))
    }

    /// The bytes a planted append writes before it fails, and the failure.
    pub(crate) fn append<'a>(path: &Path, bytes: &'a [u8]) -> Option<(&'a [u8], io::Error)> {
        let (step, error) = take(path, |step| matches!(step, Step::Append(_)))?;
        let kept = match step {
            Step::Append(Keep::Nothing) => &bytes[..0],
            Step::Append(Keep::Half) => &bytes[..bytes.len() / 2],
            _ => bytes,
        };
        Some((kept, error))
    }

    /// Records recovery attempts and withdrawals under `under` until dropped.
    pub(crate) struct Watch(u64);

    impl Drop for Watch {
        fn drop(&mut self) {
            registry().watched.retain(|(id, ..)| *id != self.0);
        }
    }

    impl Watch {
        pub(crate) fn opens(&self) -> Vec<Instant> {
            let registry = registry();
            let watched = registry.watched.iter().find(|(id, ..)| *id == self.0);
            watched
                .map(|(_, _, opens, _)| opens.clone())
                .unwrap_or_default()
        }

        pub(crate) fn withdrawals(&self) -> usize {
            let registry = registry();
            let watched = registry.watched.iter().find(|(id, ..)| *id == self.0);
            watched.map_or(0, |(.., withdrawn)| *withdrawn)
        }
    }

    pub(crate) fn watch(under: &Path) -> Watch {
        let mut registry = registry();
        registry.next += 1;
        let id = registry.next;
        registry
            .watched
            .push((id, under.to_path_buf(), Vec::new(), 0));
        Watch(id)
    }

    pub(crate) fn note_open(dir: &Path) {
        let now = Instant::now();
        let mut registry = registry();
        let watched = registry
            .watched
            .iter_mut()
            .filter(|(_, under, ..)| dir.starts_with(under));
        watched.for_each(|(_, _, opens, _)| opens.push(now));
    }

    pub(crate) fn note_withdrawn(path: &Path) {
        let mut registry = registry();
        let watched = registry
            .watched
            .iter_mut()
            .filter(|(_, under, ..)| path.starts_with(under));
        watched.for_each(|(.., withdrawn)| *withdrawn += 1);
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
