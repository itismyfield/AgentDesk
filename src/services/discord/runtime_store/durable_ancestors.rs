//! Flush of the directories between a published file's parent and a trusted root, so a newly
//! created provider/token/store directory is linked durably, not just the file inside it.
//!
//! A chain flushed by this process is remembered with the identity of every directory on it.
//! Later publications under the same parent skip the chain only while those identities still
//! match; a new process, a replaced directory, an eviction or a failed flush re-flushes it all.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use super::PARENT_DIR_FSYNC_FLUSHES;
use super::durable_publish::{SyncStage, flush_dir};

/// Bound on remembered chains; one per (root, parent directory) pair.
const WITNESS_CAP: usize = 512;

type DirIdentity = (u64, u64);
type ChainKey = (PathBuf, PathBuf);

/// Flushes every directory strictly above `path`'s parent, up to and including `root`; the
/// parent itself is the caller's flush. Never opens anything above `root`. A `path` whose parent
/// is not `root` or below it, through plain names only, is refused as unbounded.
pub(crate) fn fsync_created_ancestors(root: &Path, path: &Path) -> io::Result<()> {
    fsync_ancestors_with(&WITNESS, root, path)
}

fn fsync_ancestors_with(witness: &Mutex<Witness>, root: &Path, path: &Path) -> io::Result<()> {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return Ok(());
    }
    #[cfg(test)]
    if super::durable_publish::probe::bypassed(path) {
        return Ok(());
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let depth = chain_depth(root, parent)?;
    if depth == 0 {
        return Ok(());
    }
    // Identities are read before flushing: a directory replaced after this read differs from
    // the remembered identity, so the next publication re-flushes rather than trusting it.
    let identities = parent
        .ancestors()
        .take(depth + 1)
        .map(identity)
        .collect::<io::Result<Vec<_>>>()?;
    let key = (root.to_path_buf(), parent.to_path_buf());
    if lock(witness).matches(&key, &identities) {
        return Ok(());
    }
    for dir in parent.ancestors().skip(1).take(depth) {
        if let Err((_, error)) = flush_dir(path, dir, SyncStage::Ancestors, SyncStage::Ancestors) {
            lock(witness).forget(&key);
            return Err(io::Error::new(
                error.kind(),
                format!("sync ancestor {}: {error}", dir.display()),
            ));
        }
    }
    lock(witness).record(key, identities, WITNESS_CAP);
    Ok(())
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

#[cfg(unix)]
fn identity(dir: &Path) -> io::Result<DirIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(dir)?;
    Ok((meta.dev(), meta.ino()))
}

/// Unreachable in practice: platforms without a directory flush return before any identity.
#[cfg(not(unix))]
fn identity(dir: &Path) -> io::Result<DirIdentity> {
    fs::metadata(dir).map(|_| (0, 0))
}

/// Chains this process flushed, keyed by (root, parent) with the identities seen before flushing.
#[derive(Default)]
pub(super) struct Witness {
    chains: HashMap<ChainKey, (Vec<DirIdentity>, u64)>,
    next_seq: u64,
}

impl Witness {
    fn matches(&self, key: &ChainKey, identities: &[DirIdentity]) -> bool {
        self.chains
            .get(key)
            .is_some_and(|(seen, _)| seen.as_slice() == identities)
    }

    fn record(&mut self, key: ChainKey, identities: Vec<DirIdentity>, cap: usize) {
        if !self.chains.contains_key(&key)
            && self.chains.len() >= cap
            && let Some(oldest) = self
                .chains
                .iter()
                .min_by_key(|(_, (_, seq))| *seq)
                .map(|(key, _)| key.clone())
        {
            self.chains.remove(&oldest);
        }
        self.next_seq += 1;
        self.chains.insert(key, (identities, self.next_seq));
    }

    fn forget(&mut self, key: &ChainKey) {
        self.chains.remove(key);
    }
}

static WITNESS: LazyLock<Mutex<Witness>> = LazyLock::new(Mutex::default);

/// Held only for a lookup or update; no filesystem call runs under it.
fn lock(witness: &Mutex<Witness>) -> std::sync::MutexGuard<'_, Witness> {
    witness.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
#[path = "durable_ancestors_tests.rs"]
mod tests;
