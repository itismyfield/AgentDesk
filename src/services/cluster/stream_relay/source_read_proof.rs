use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

// Revision ownership survives the weak source lock and is never evicted.
static REVISIONS: LazyLock<Mutex<HashMap<String, Option<u64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn invalidate_source(key: &str) {
    let mut revisions = REVISIONS.lock().unwrap_or_else(|error| error.into_inner());
    let revision = revisions.entry(key.to_owned()).or_insert(Some(0));
    *revision = revision.and_then(|value| value.checked_add(1));
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourcePin {
    key: String,
    revision: u64,
}

impl SourcePin {
    #[cfg(test)]
    pub(crate) fn capture(key: String) -> Option<Self> {
        let revision = REVISIONS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .copied()
            .unwrap_or(Some(0))?;
        Some(Self { key, revision })
    }

    pub(crate) fn is_current(&self) -> bool {
        REVISIONS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&self.key)
            .copied()
            .unwrap_or(Some(0))
            == Some(self.revision)
    }
}

#[derive(Clone)]
pub(crate) struct SourceCertificate {
    pin: SourcePin,
    end: u64,
    generation: i64,
    reset: u64,
    #[cfg(all(test, unix))]
    seal: PrefixSeal,
}

impl SourceCertificate {
    #[cfg(all(test, unix))]
    pub(crate) fn prefix_is_current(&self) -> bool {
        self.seal.is_current()
    }

    pub(crate) fn matches(&self, end: u64, generation: i64, reset: u64) -> bool {
        self.end == end
            && self.generation == generation
            && self.reset == reset
            && self.pin.is_current()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn confirmed(
        seal: &PrefixSeal,
        end: u64,
        generation: i64,
        reset: u64,
    ) -> Option<Self> {
        (generation != 0
            && generation == seal.generation
            && reset == seal.reset
            && seal.end == end
            && seal.is_current())
        .then(|| Self {
            pin: seal.pin.clone(),
            end,
            generation,
            reset,
            seal: seal.clone(),
        })
    }
}

#[cfg(all(test, unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadVersion {
    identity: (u64, u64),
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(all(test, unix))]
impl ReadVersion {
    pub(crate) fn from_fd(file: &std::fs::File) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata().ok()?;
            metadata.is_file().then(|| Self {
                identity: (metadata.dev(), metadata.ino()),
                len: metadata.len(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            None
        }
    }
}

#[cfg(all(test, unix))]
#[derive(Clone)]
pub(crate) struct SourceReadProof {
    file: std::sync::Arc<std::fs::File>,
    path: std::path::PathBuf,
    pin: SourcePin,
    version: ReadVersion,
    range: std::ops::Range<u64>,
    generation: i64,
    reset: u64,
    digest: [u8; 32],
}

#[cfg(all(test, unix))]
impl SourceReadProof {
    pub(crate) fn capture(
        file: std::sync::Arc<std::fs::File>,
        path: std::path::PathBuf,
        pin: SourcePin,
        range: std::ops::Range<u64>,
        bytes: &[u8],
        before: ReadVersion,
        frontier: (i64, u64),
    ) -> Option<Self> {
        use sha2::{Digest, Sha256};
        let after = ReadVersion::from_fd(&file)?;
        if before != after
            || !pin.is_current()
            || range.start > range.end
            || range.end > after.len
            || range.end - range.start != bytes.len() as u64
        {
            return None;
        }
        Some(Self {
            file,
            path,
            pin,
            version: after,
            range,
            generation: frontier.0,
            reset: frontier.1,
            digest: Sha256::digest(bytes).into(),
        })
    }

    pub(crate) fn seal_prefix(&self) -> Option<PrefixSeal> {
        if self.range.start != 0 || self.range.end == 0 {
            return None;
        }
        let seal = PrefixSeal {
            file: self.file.clone(),
            path: self.path.clone(),
            pin: self.pin.clone(),
            version: self.version,
            end: self.range.end,
            generation: self.generation,
            reset: self.reset,
            digest: self.digest,
        };
        seal.is_current().then_some(seal)
    }
}

#[cfg(all(test, unix))]
#[derive(Clone)]
pub(crate) struct PrefixSeal {
    file: std::sync::Arc<std::fs::File>,
    path: std::path::PathBuf,
    pin: SourcePin,
    version: ReadVersion,
    end: u64,
    generation: i64,
    reset: u64,
    digest: [u8; 32],
}

#[cfg(all(test, unix))]
impl PrefixSeal {
    pub(crate) fn is_current(&self) -> bool {
        #[cfg(unix)]
        {
            use sha2::{Digest, Sha256};
            use std::os::unix::fs::{FileExt, MetadataExt};
            let Some(before) = ReadVersion::from_fd(&self.file) else {
                return false;
            };
            let path_matches = || {
                std::fs::metadata(&self.path)
                    .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == before.identity)
            };
            if !path_matches()
                || !self.pin.is_current()
                || before.identity != self.version.identity
                || before.len < self.version.len
                || self.end > before.len
                || (before.len == self.version.len && before != self.version)
            {
                return false;
            }
            let mut hasher = Sha256::new();
            let mut offset = 0;
            let mut buffer = [0u8; 8192];
            while offset < self.end {
                let count = (self.end - offset).min(buffer.len() as u64) as usize;
                let Ok(read) = self.file.read_at(&mut buffer[..count], offset) else {
                    return false;
                };
                if read == 0 {
                    return false;
                }
                hasher.update(&buffer[..read]);
                offset += read as u64;
            }
            let digest: [u8; 32] = hasher.finalize().into();
            ReadVersion::from_fd(&self.file) == Some(before)
                && path_matches()
                && self.pin.is_current()
                && digest == self.digest
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

#[cfg(test)]
pub(crate) fn assert_revision_unlocked_for_tests() {
    assert!(
        REVISIONS.try_lock().is_ok(),
        "revision mutex must not cover file IO"
    );
}

#[cfg(test)]
#[path = "source_read_proof_tests.rs"]
pub(crate) mod tests;
