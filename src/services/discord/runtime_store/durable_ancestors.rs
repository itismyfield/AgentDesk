//! Flushes the directories between a published file's parent and a trusted root, skipping only
//! entries `P/name -> D` this process proved by syncing `P` after reading both identities.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use super::PARENT_DIR_FSYNC_FLUSHES;
use super::durable_publish::{SyncStage, flush_dir};

/// Bound on remembered links: a few hundred store chains of two to four links each.
const WITNESS_CAP: usize = 2048;

/// Device, inode and birth time: a directory recreated at the same path differs in one of them.
type DirIdentity = (u64, u64, SystemTime);

/// What reading a directory proved: `None` when the link through it cannot be remembered.
struct Seen {
    identity: Option<DirIdentity>,
    symlink: bool,
}

/// Flushes every directory above `path`'s parent up to and including `root`, never above it;
/// a parent outside `root` (or reached through `..`) is refused.
pub(crate) fn fsync_created_ancestors(root: &Path, path: &Path) -> io::Result<()> {
    fsync_ancestors_with(&WITNESS, root, path)
}

fn fsync_ancestors_with(witness: &Mutex<Witness>, root: &Path, path: &Path) -> io::Result<()> {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return Ok(());
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let depth = chain_depth(root, parent)?;
    if depth == 0 {
        return Ok(());
    }
    // Link i is the entry of dirs[i] in dirs[i + 1]; identities are read before any flush, so a
    // directory replaced after this read never matches what is remembered for it.
    let dirs: Vec<&Path> = parent.ancestors().take(depth + 1).collect();
    let seen = dirs
        .iter()
        .enumerate()
        .map(|(i, dir)| read_identity(dir, i == depth))
        .collect::<io::Result<Vec<_>>>()?;
    let link = |i: usize| (seen[i + 1].identity, seen[i].identity);
    let proven: Vec<bool> = {
        let mut witness = lock(witness);
        (0..depth)
            .map(|i| witness.matches(dirs[i], link(i)))
            .collect()
    };
    let mut flushed = Vec::new();
    let mut failure = None;
    for i in (0..depth).filter(|&i| !proven[i]) {
        let dir = dirs[i + 1];
        if let Err((_, error)) = flush_dir(path, dir, SyncStage::Ancestors, SyncStage::Ancestors) {
            failure = Some(io::Error::new(
                error.kind(),
                format!("sync ancestor {}: {error}", dir.display()),
            ));
            break;
        }
        flushed.push(i);
    }
    {
        let mut witness = lock(witness);
        for i in flushed {
            witness.record(dirs[i], link(i), WITNESS_CAP);
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    // The entry naming a symlink's target lives outside the flushed chain.
    match dirs[..depth]
        .iter()
        .zip(&seen)
        .find(|(_, seen)| seen.symlink)
    {
        Some((dir, _)) => Err(io::Error::other(format!(
            "{} is a symlink; its target's own entry is not flushed",
            dir.display()
        ))),
        None => Ok(()),
    }
}

/// The root is followed, since only what it names matters; below it a symlink is reported
/// rather than followed into an identity, and a missing birth time leaves nothing to prove.
fn read_identity(dir: &Path, is_root: bool) -> io::Result<Seen> {
    let meta = if is_root {
        fs::metadata(dir)?
    } else {
        fs::symlink_metadata(dir)?
    };
    let symlink = meta.file_type().is_symlink();
    let born = meta.created().ok().filter(|_| !symlink);
    #[cfg(test)]
    let born = born.filter(|_| !no_birth_time::covers(dir));
    let identity = born.map(|born| {
        let (dev, ino) = super::dir_identity(&meta);
        (dev, ino, born)
    });
    Ok(Seen { identity, symlink })
}

/// Number of directories between `root` and `parent`, refusing anything outside `root`.
fn chain_depth(root: &Path, parent: &Path) -> io::Result<usize> {
    let outside = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is outside trusted root {}",
                parent.display(),
                root.display()
            ),
        )
    };
    if root.as_os_str().is_empty() {
        return Err(outside());
    }
    let relative = parent.strip_prefix(root).map_err(|_| outside())?;
    let mut depth = 0;
    for component in relative.components() {
        match component {
            Component::Normal(_) => depth += 1,
            _ => return Err(outside()),
        }
    }
    Ok(depth)
}

/// Identities of a link's containing directory and of the directory it names.
type Link = (Option<DirIdentity>, Option<DirIdentity>);

/// Links this process flushed, keyed by the directory they name, with the identities seen
/// before flushing and a use stamp for least-recently-used eviction.
#[derive(Default)]
pub(super) struct Witness {
    links: HashMap<PathBuf, (DirIdentity, DirIdentity, u64)>,
    next_seq: u64,
}

impl Witness {
    /// A stale entry is dropped, so identities returning to an older pair never match it.
    fn matches(&mut self, dir: &Path, (parent, child): Link) -> bool {
        let (Some(parent), Some(child)) = (parent, child) else {
            return false;
        };
        self.next_seq += 1;
        match self.links.get_mut(dir) {
            Some(entry) if entry.0 == parent && entry.1 == child => {
                entry.2 = self.next_seq;
                true
            }
            Some(_) => {
                self.links.remove(dir);
                false
            }
            None => false,
        }
    }

    fn record(&mut self, dir: &Path, (parent, child): Link, cap: usize) {
        let (Some(parent), Some(child)) = (parent, child) else {
            return;
        };
        if !self.links.contains_key(dir)
            && self.links.len() >= cap
            && let Some(oldest) = self
                .links
                .iter()
                .min_by_key(|(_, (.., seq))| *seq)
                .map(|(dir, _)| dir.clone())
        {
            self.links.remove(&oldest);
        }
        self.next_seq += 1;
        self.links
            .insert(dir.to_path_buf(), (parent, child, self.next_seq));
    }
}

static WITNESS: LazyLock<Mutex<Witness>> = LazyLock::new(Mutex::default);

/// Held only for a lookup or update; no filesystem call runs under it.
fn lock(witness: &Mutex<Witness>) -> std::sync::MutexGuard<'_, Witness> {
    witness.lock().unwrap_or_else(|e| e.into_inner())
}

/// Directories read as if their filesystem reported no birth time, until the guard drops.
#[cfg(test)]
pub(super) mod no_birth_time {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static SCOPES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(in super::super) struct Guard(PathBuf);

    impl Guard {
        pub(in super::super) fn new(scope: &Path) -> Self {
            SCOPES.lock().unwrap().push(scope.to_path_buf());
            Self(scope.to_path_buf())
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            SCOPES.lock().unwrap().retain(|scope| scope != &self.0);
        }
    }

    pub(super) fn covers(dir: &Path) -> bool {
        SCOPES
            .lock()
            .unwrap()
            .iter()
            .any(|scope| dir.starts_with(scope))
    }
}

#[cfg(test)]
#[path = "durable_ancestors_tests.rs"]
mod tests;
