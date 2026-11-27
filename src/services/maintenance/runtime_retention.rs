use serde::de::DeserializeOwned;
use std::time::{Duration, SystemTime};
use std::{fs, io, path::Path};

pub(crate) fn cutoff(now: SystemTime) -> Option<SystemTime> {
    const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    now.checked_sub(RETENTION)
}

pub(crate) fn expired(timestamp: SystemTime, cutoff: SystemTime) -> bool {
    timestamp <= cutoff
}

pub(crate) fn regular_file_modified(path: &Path) -> Option<SystemTime> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    metadata.modified().ok()
}

pub(crate) fn regular_file_expired(path: &Path, cutoff: SystemTime) -> bool {
    regular_file_modified(path).is_some_and(|timestamp| expired(timestamp, cutoff))
}

pub(crate) fn checked_owner_file<T: DeserializeOwned>(
    path: &Path,
    matches_owner: impl FnOnce(&T) -> bool,
) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(io::ErrorKind::InvalidData.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let record = serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !matches_owner(&record) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "owner record identity does not match its path",
        ));
    }
    Ok(true)
}
