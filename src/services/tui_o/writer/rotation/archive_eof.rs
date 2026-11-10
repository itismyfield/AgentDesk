//! Proves an archived transcript ends exactly at its durable capture cursor.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_o::shadow::{ShadowProvider, SourceAnomalyKind, SourceId};
use crate::services::tui_o::store::spool::Cursor;

pub(super) struct VerifiedArchiveEof {
    cursor: Cursor,
    archive: PathBuf,
    file: File,
    fingerprint: Fingerprint,
}

fn invalid(detail: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

fn archive_path(provider: ShadowProvider, source: &SourceId) -> Option<PathBuf> {
    if provider != ShadowProvider::Codex || source.path.extension().is_none_or(|e| e != "jsonl") {
        return None;
    }
    let mut path = source.path.as_os_str().to_os_string();
    path.push(".zst");
    Some(path.into())
}

/// Candidate discovery grants no permission to retire the source.
pub(super) fn candidate(provider: ShadowProvider, source: &SourceId) -> bool {
    archive_path(provider, source)
        .is_some_and(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()))
}

fn require_missing(source: &SourceId) -> io::Result<()> {
    match Fingerprint::path(source) {
        Err((SourceAnomalyKind::Missing, _)) => Ok(()),
        Err((_, detail)) => Err(io::Error::other(format!("source is not missing: {detail}"))),
        Ok(_) => Err(invalid("source reappeared during archive validation")),
    }
}

fn regular_fingerprint(path: &Path) -> io::Result<Fingerprint> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(invalid("archive path is not a regular file"));
    }
    Fingerprint::of(&metadata)
}

impl VerifiedArchiveEof {
    /// The caller also checks its reader obligations immediately before the cursor write.
    pub(super) fn recheck(&self, current: &Cursor, wall_now: SystemTime) -> io::Result<()> {
        if current != &self.cursor {
            return Err(invalid("archive cursor changed after validation"));
        }
        if self.fingerprint.modified > wall_now {
            return Err(invalid("archive mtime is in the future"));
        }
        if Fingerprint::of(&self.file.metadata()?)? != self.fingerprint
            || regular_fingerprint(&self.archive)? != self.fingerprint
        {
            return Err(invalid("archive changed during validation"));
        }
        require_missing(&current.source)
    }
}

pub(super) fn verify(
    provider: ShadowProvider,
    cursor: &Cursor,
    wall_now: SystemTime,
) -> io::Result<VerifiedArchiveEof> {
    let archive = archive_path(provider, &cursor.source)
        .ok_or_else(|| invalid("source has no supported Codex archive path"))?;
    if cursor.retired {
        return Err(invalid("archive cursor is already retired"));
    }
    let limit = cursor
        .captured_through
        .checked_add(1)
        .ok_or_else(|| invalid("archive cursor offset overflow"))?;
    require_missing(&cursor.source)?;
    let fingerprint = regular_fingerprint(&archive)?;
    if fingerprint.modified > wall_now {
        return Err(invalid("archive mtime is in the future"));
    }
    let file = File::open(&archive)?;
    if Fingerprint::of(&file.metadata()?)? != fingerprint {
        return Err(invalid("archive changed while opening"));
    }
    #[cfg(test)]
    test_point(TestPoint::Opened);
    let mut decoder = zstd::stream::read::Decoder::new(&file)?;
    decoder.window_log_max(27)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut hasher = Sha256::new();
    let mut decoded = 0u64;
    loop {
        let capacity = (limit - decoded).min(buffer.len() as u64) as usize;
        let count = match decoder.read(&mut buffer[..capacity]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        decoded += count as u64;
        if decoded > cursor.captured_through {
            return Err(invalid("archive contains bytes beyond the capture cursor"));
        }
        hasher.update(&buffer[..count]);
    }
    if decoded != cursor.captured_through {
        return Err(invalid("archive ends before the capture cursor"));
    }
    if hex::encode(hasher.finalize()) != cursor.prefix_hash {
        return Err(invalid("archive bytes before the cursor changed"));
    }
    drop(decoder);
    #[cfg(test)]
    test_point(TestPoint::Decoded);
    let proof = VerifiedArchiveEof {
        cursor: cursor.clone(),
        archive,
        file,
        fingerprint,
    };
    proof.recheck(cursor, wall_now)?;
    Ok(proof)
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestPoint {
    Opened,
    Decoded,
}

#[cfg(test)]
type TestHook = Box<dyn FnMut(TestPoint)>;

#[cfg(test)]
thread_local! {
    static TEST_HOOK: std::cell::RefCell<Option<TestHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn test_point(point: TestPoint) {
    TEST_HOOK.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(point);
        }
    });
}

#[cfg(test)]
#[path = "archive_eof_tests.rs"]
mod tests;
