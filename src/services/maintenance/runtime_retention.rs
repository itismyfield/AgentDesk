use std::time::{Duration, SystemTime};

pub(crate) fn cutoff(now: SystemTime) -> Option<SystemTime> {
    const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    now.checked_sub(RETENTION)
}

pub(crate) fn expired(timestamp: SystemTime, cutoff: SystemTime) -> bool {
    timestamp <= cutoff
}

pub(crate) fn regular_file_modified(path: &std::path::Path) -> Option<SystemTime> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    metadata.modified().ok()
}

pub(crate) fn regular_file_expired(path: &std::path::Path, cutoff: SystemTime) -> bool {
    regular_file_modified(path).is_some_and(|timestamp| expired(timestamp, cutoff))
}
