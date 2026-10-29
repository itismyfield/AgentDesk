//! The notice ledger format becomes mandatory before its first new entry is appended.

use std::fs::{self, File};
use std::io;
use std::path::Path;

use super::durable;
use crate::services::discord::runtime_store::fsync_parent_dir;

/// This build reads and writes the notice ledger format, so it may advertise the capability.
pub(crate) const READER_SUPPORTED: bool = true;
const FILE_NAME: &str = "input_notice.floor";
const CONTENT: &[u8] = b"1\n";

pub(super) fn before_append(store_root: &Path, enabled: bool) -> io::Result<()> {
    if !enabled {
        return Ok(());
    }
    durable::ensure_dir(store_root)?;
    let path = store_root.join(FILE_NAME);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Err(error) = durable::create_once(&path, CONTENT)
                && error.kind() != io::ErrorKind::AlreadyExists
            {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    if !fs::symlink_metadata(&path)?.is_file() || fs::read(&path)? != CONTENT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input notice floor is not a regular schema-1 marker",
        ));
    }
    // A published floor left by a failed append stays mandatory on the next attempt.
    File::open(store_root.join(FILE_NAME))?.sync_all()?;
    fsync_parent_dir(&path)
}

#[cfg(test)]
#[path = "input_notice_floor/tests.rs"]
mod tests;
