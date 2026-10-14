//! Read-only probe of one old source for the transition cleanup judgment. It opens its own FD,
//! so a dormant reader that holds none is checked the same way, and it writes nothing.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::services::tui_o::shadow::capture::{FileMatch, Fingerprint, file_match};
use crate::services::tui_o::shadow::{SourceAnomalyKind, SourceId};
use crate::services::tui_o::store::spool::Cursor;

/// One probe of a source against the cursor it was taken for, stamped with the caller's wall time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Observation {
    pub source: SourceId,
    pub through: u64,
    pub prefix_hash: String,
    pub at: SystemTime,
    pub file: FileSeen,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileSeen {
    Missing,
    /// The path names another inode than the cursor's source.
    Replaced,
    Unreadable(String),
    Present(Present),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Present {
    /// Of the opened FD, taken after the prefix was read.
    pub fingerprint: Fingerprint,
    /// The path and the opened FD name the same inode, and it is the cursor's source.
    pub same_file: bool,
    /// The file holds the cursor's prefix and it hashes to the cursor's hash.
    pub prefix_matches: bool,
    /// The file is empty or ends with a newline.
    pub whole_tail: bool,
    /// The FD's metadata held while the file was read.
    pub steady: bool,
}

impl Observation {
    pub(crate) fn is_of(&self, cursor: &Cursor) -> bool {
        self.source == cursor.source
            && self.through == cursor.captured_through
            && self.prefix_hash == cursor.prefix_hash
    }
}

pub(crate) fn observe(cursor: &Cursor, at: SystemTime) -> Observation {
    let file = match Fingerprint::path(&cursor.source) {
        Err((SourceAnomalyKind::Missing, _)) => FileSeen::Missing,
        Err((SourceAnomalyKind::Replaced, _)) => FileSeen::Replaced,
        Err((_, detail)) => FileSeen::Unreadable(detail),
        Ok(path) => match read(cursor, &path) {
            Ok(present) => FileSeen::Present(present),
            Err(error) if error.kind() == io::ErrorKind::NotFound => FileSeen::Missing,
            Err(error) => FileSeen::Unreadable(error.to_string()),
        },
    };
    Observation {
        source: cursor.source.clone(),
        through: cursor.captured_through,
        prefix_hash: cursor.prefix_hash.clone(),
        at,
        file,
    }
}

fn read(cursor: &Cursor, path: &Fingerprint) -> io::Result<Present> {
    let mut file = File::open(&cursor.source.path)?;
    let before = Fingerprint::of(&file.metadata()?)?;
    let opened = (before.dev, before.ino);
    let same_file =
        opened == (path.dev, path.ino) && file_match(&cursor.source, opened) != FileMatch::Other;
    let through = cursor.captured_through;
    let mut hasher = Sha256::new();
    let hashed = io::copy(&mut (&mut file).take(through), &mut hasher)?;
    let prefix_matches = hashed == through && hex::encode(hasher.finalize()) == cursor.prefix_hash;
    let whole_tail = before.len == 0 || {
        let mut last = [0u8];
        file.seek(SeekFrom::Start(before.len - 1))?;
        file.read_exact(&mut last)?;
        last == [b'\n']
    };
    let fingerprint = Fingerprint::of(&file.metadata()?)?;
    let steady = fingerprint == before;
    Ok(Present {
        fingerprint,
        same_file,
        prefix_matches,
        whole_tail,
        steady,
    })
}

#[cfg(all(test, unix))]
#[path = "transition_observe_tests.rs"]
mod tests;
