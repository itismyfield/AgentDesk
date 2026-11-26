//! Per-record flock on a `.json.lock` sidecar, so a record's read-modify-write is atomic
//! against a concurrent writer in the same process.

use std::fs;
use std::path::Path;

/// Shared by the delivery record, its reachability ledger and the engine's inject ring.
pub struct DeliveryRecordLock {
    _file: fs::File,
}

impl Drop for DeliveryRecordLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Best effort unlock; closing the fd would release it anyway.
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

pub fn lock_record_path(record_path: &Path) -> Result<DeliveryRecordLock, String> {
    let lock_path = record_path.with_extension("json.lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(DeliveryRecordLock { _file: file })
}
