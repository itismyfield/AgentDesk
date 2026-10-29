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

/// Whether `channel`'s ledger under `runtime_root` already holds its frozen notice cohort, read
/// without recovering it; a missing or unreadable ledger proves nothing.
pub(crate) fn migration_recorded(runtime_root: &Path, channel: u64) -> bool {
    use super::ledger::{LedgerEntry, LedgerLine, input_notices::NoticeEntry};
    let channel_dir = runtime_root
        .join(super::STORE_DIR_NAME)
        .join(channel.to_string());
    let Ok(bytes) = fs::read(channel_dir.join(super::LEDGER_FILE)) else {
        return false;
    };
    let started = |line: &&[u8]| {
        line.windows(17)
            .any(|window| window == b"migration_started")
    };
    bytes
        .split(|byte| *byte == b'\n')
        .filter(started)
        .any(|line| {
            matches!(
                serde_json::from_slice::<LedgerLine>(line),
                Ok(LedgerLine { entry: LedgerEntry::InputNotice { entry }, .. })
                    if matches!(*entry, NoticeEntry::MigrationStarted { .. })
            )
        })
}

#[cfg(test)]
#[path = "input_notice_floor/tests.rs"]
mod tests;
