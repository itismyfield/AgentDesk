//! Inflight sidecar filesystem + advisory-lock seam (#3479 extraction).
//!
//! The low-level path layout (`inflight_provider_dir` / `inflight_state_path`)
//! and the `flock(2)`-backed [`InflightStateFileLock`] guard
//! (`lock_inflight_state_path`) used by every read/modify/write helper in the
//! parent module. Behaviour-preserving move out of `inflight.rs`; the parent
//! re-exports the cross-module items so existing call sites resolve unchanged.

use super::*;

pub(super) fn inflight_provider_dir(root: &Path, provider: &ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}

pub(in crate::services::discord::inflight) fn inflight_state_path(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    inflight_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

pub(in crate::services::discord::inflight) struct InflightStateFileLock {
    _file: fs::File,
}

impl Drop for InflightStateFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Best effort unlock; closing the fd would release it anyway.
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn inflight_state_lock_path(path: &Path) -> PathBuf {
    path.with_extension("json.lock")
}

pub(in crate::services::discord::inflight) fn lock_inflight_state_path(
    path: &Path,
) -> Result<InflightStateFileLock, String> {
    let lock_path = inflight_state_lock_path(path);
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
    Ok(InflightStateFileLock { _file: file })
}
