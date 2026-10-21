//! One file descriptor and both path resolutions must describe the same policy bytes.

use std::fs::{File, Metadata};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::time::UNIX_EPOCH;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use super::Unverified;

const LIMIT: u64 = 256 * 1024;
const START_MARGIN: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    modified: SystemTime,
    changed: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SymlinkIdentity {
    path: PathBuf,
    target: PathBuf,
    identity: FileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileToken {
    logical_path: PathBuf,
    resolved_path: PathBuf,
    identity: FileIdentity,
    hash: [u8; 32],
    symlink_chain: Vec<SymlinkIdentity>,
    limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileSnapshot {
    pub token: FileToken,
    pub bytes: Vec<u8>,
}

impl FileToken {
    #[cfg(all(test, unix))]
    pub(crate) fn digest(&self) -> [u8; 32] {
        self.hash
    }

    #[cfg(all(test, unix))]
    pub(crate) fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    #[cfg(all(test, unix))]
    pub(crate) fn ownership(&self) -> (u32, u32, u32) {
        (self.identity.uid, self.identity.gid, self.identity.mode)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn same_path(&self, other: &Self) -> bool {
        self.logical_path == other.logical_path
            && self.resolved_path == other.resolved_path
            && self.symlink_chain == other.symlink_chain
    }

    pub(crate) fn logical_path(&self) -> &Path {
        &self.logical_path
    }

    #[cfg(test)]
    pub(crate) fn same_image(&self, other: &Self) -> bool {
        self.identity == other.identity && self.hash == other.hash
    }

    pub(crate) fn dev_ino(&self) -> (u64, u64) {
        (self.identity.dev, self.identity.ino)
    }

    pub(crate) fn parent_identity(&self) -> Result<(u64, u64), Unverified> {
        directory_identity(self.resolved_path.parent().ok_or(Unverified::File)?)
    }

    /// Metadata and every symlink fence cache reuse; only mutation fences hash bytes.
    pub(crate) fn fast_unchanged(&self) -> Result<(), Unverified> {
        let (resolved, links) = resolve(&self.logical_path)?;
        let now = identity(&std::fs::metadata(&resolved).map_err(|_| Unverified::File)?)?;
        if resolved == self.resolved_path && links == self.symlink_chain && now == self.identity {
            Ok(())
        } else {
            Err(Unverified::Changed)
        }
    }

    pub(crate) fn unchanged(&self) -> Result<(), Unverified> {
        let now = snapshot(&self.logical_path, self.limit, false, || {})?;
        if now.token == *self {
            Ok(())
        } else {
            Err(Unverified::Changed)
        }
    }

    pub(crate) fn predates(&self, started: SystemTime) -> bool {
        let older = |time: SystemTime| {
            time.checked_add(START_MARGIN)
                .is_some_and(|time| time < started)
        };
        std::iter::once(&self.identity)
            .chain(self.symlink_chain.iter().map(|link| &link.identity))
            .all(|identity| older(identity.modified) && older(identity.changed))
    }
}

pub(crate) fn directory_identity(path: &Path) -> Result<(u64, u64), Unverified> {
    let metadata = std::fs::metadata(path).map_err(|_| Unverified::File)?;
    if !metadata.is_dir() {
        return Err(Unverified::File);
    }
    let identity = identity(&metadata)?;
    Ok((identity.dev, identity.ino))
}

pub(crate) fn read(path: &Path) -> Result<FileSnapshot, Unverified> {
    snapshot(path, Some(LIMIT), true, || {})
}

/// Executable images are hashed incrementally; their bytes are never retained as config text.
pub(crate) fn image(path: &Path) -> Result<FileToken, Unverified> {
    snapshot(path, None, false, || {}).map(|snapshot| snapshot.token)
}

fn snapshot(
    path: &Path,
    limit: Option<u64>,
    keep: bool,
    between: impl FnOnce(),
) -> Result<FileSnapshot, Unverified> {
    let (resolved_path, symlink_chain) = resolve(path)?;
    let at_open = std::fs::metadata(&resolved_path).map_err(|_| Unverified::File)?;
    if !at_open.is_file() {
        return Err(Unverified::File);
    }
    let mut file = File::open(&resolved_path).map_err(|_| Unverified::File)?;
    let metadata = file.metadata().map_err(|_| Unverified::File)?;
    let before = identity(&metadata)?;
    if !metadata.is_file() || limit.is_some_and(|limit| before.size > limit) {
        return Err(Unverified::File);
    }
    let mut bytes = Vec::new();
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    let mut chunk = [0_u8; 16 * 1024];
    let ceiling = before.size.checked_add(1).ok_or(Unverified::File)?;
    loop {
        let budget = usize::try_from((ceiling - size).min(chunk.len() as u64))
            .map_err(|_| Unverified::File)?;
        let read = file
            .read(&mut chunk[..budget])
            .map_err(|_| Unverified::File)?;
        if read == 0 {
            break;
        }
        size = size.checked_add(read as u64).ok_or(Unverified::File)?;
        if limit.is_some_and(|limit| size > limit) {
            return Err(Unverified::File);
        }
        if size > before.size {
            return Err(Unverified::Changed);
        }
        hash.update(&chunk[..read]);
        if keep {
            bytes.extend_from_slice(&chunk[..read]);
        }
    }
    between();
    let after = identity(&file.metadata().map_err(|_| Unverified::File)?)?;
    let (resolved_after, links_after) = resolve(path)?;
    let at_path = identity(&std::fs::metadata(path).map_err(|_| Unverified::File)?)?;
    if before != after
        || before != at_path
        || before.size != size
        || resolved_path != resolved_after
        || symlink_chain != links_after
    {
        return Err(Unverified::Changed);
    }
    Ok(FileSnapshot {
        token: FileToken {
            logical_path: path.to_owned(),
            resolved_path,
            identity: before,
            hash: hash.finalize().into(),
            symlink_chain,
            limit,
        },
        bytes,
    })
}

fn resolve(path: &Path) -> Result<(PathBuf, Vec<SymlinkIdentity>), Unverified> {
    if !path.is_absolute() {
        return Err(Unverified::File);
    }
    let mut pending = path.to_owned();
    let mut resolved = PathBuf::new();
    let mut links = Vec::new();
    loop {
        let mut replacement = None;
        let mut components = pending.components();
        while let Some(component) = components.next() {
            match component {
                Component::RootDir => resolved.push(component.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(name) => {
                    resolved.push(name);
                    let metadata =
                        std::fs::symlink_metadata(&resolved).map_err(|_| Unverified::File)?;
                    if metadata.is_symlink() {
                        if links.len() == 40 {
                            return Err(Unverified::File);
                        }
                        let target = std::fs::read_link(&resolved).map_err(|_| Unverified::File)?;
                        links.push(SymlinkIdentity {
                            path: resolved.clone(),
                            target: target.clone(),
                            identity: identity(&metadata)?,
                        });
                        resolved.pop();
                        let mut next = target;
                        next.push(components.as_path());
                        replacement = Some(next);
                        break;
                    }
                }
                Component::Prefix(_) => return Err(Unverified::Unsupported),
            }
        }
        match replacement {
            Some(next) => pending = next,
            None => return Ok((resolved, links)),
        }
    }
}

#[cfg(unix)]
fn identity(metadata: &Metadata) -> Result<FileIdentity, Unverified> {
    use std::os::unix::fs::MetadataExt;
    let time = |seconds, nanos| {
        let seconds = u64::try_from(seconds).map_err(|_| Unverified::File)?;
        let nanos = u32::try_from(nanos).map_err(|_| Unverified::File)?;
        if nanos >= 1_000_000_000 {
            return Err(Unverified::File);
        }
        UNIX_EPOCH
            .checked_add(Duration::new(seconds, nanos))
            .ok_or(Unverified::File)
    };
    Ok(FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        size: metadata.len(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode() & 0o7777,
        modified: time(metadata.mtime(), metadata.mtime_nsec())?,
        changed: time(metadata.ctime(), metadata.ctime_nsec())?,
    })
}

#[cfg(not(unix))]
fn identity(_: &Metadata) -> Result<FileIdentity, Unverified> {
    Err(Unverified::Unsupported)
}

#[cfg(all(test, unix))]
#[path = "file_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissingToken {
    logical: PathBuf,
    pub(crate) target: PathBuf,
    parent: (u64, u64),
    chain: Vec<SymlinkIdentity>,
}

#[cfg(all(test, unix))]
impl MissingToken {
    pub(crate) fn read(logical: &Path) -> Result<Self, Unverified> {
        if !matches!(std::fs::symlink_metadata(logical), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(Unverified::File);
        }
        let parent = logical.parent().ok_or(Unverified::File)?;
        let (resolved, chain) = resolve(parent)?;
        let target = resolved.join(logical.file_name().ok_or(Unverified::File)?);
        Ok(Self {
            logical: logical.into(),
            target,
            parent: directory_identity(&resolved)?,
            chain,
        })
    }

    pub(crate) fn unchanged(&self) -> Result<(), Unverified> {
        if &Self::read(&self.logical)? == self {
            Ok(())
        } else {
            Err(Unverified::Changed)
        }
    }

    pub(crate) fn matches_path(&self, token: &FileToken) -> bool {
        self.logical == token.logical_path
            && self.target == token.resolved_path
            && self.chain == token.symlink_chain
    }
}
